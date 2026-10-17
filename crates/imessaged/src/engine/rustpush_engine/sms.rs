//! Text Message Forwarding: SMS / MMS / RCS relayed by the user's iPhone.
//!
//! Nothing extra is registered: the `com.apple.private.alloy.sms` topic is a
//! sub-service of `com.apple.madrid` ([`rustpush::MADRID_SERVICE`]), so our
//! iMessage registration already lists this device for it, and
//! `IMClient` already listens on (and routes `is_sms()` sends to) that
//! topic. What this module adds:
//!
//! * **Activation.** [`Inner::set_sms_forwarding`] sends
//!   `Message::EnableSmsActivation(true)` (`c = 145`, `{wc: false, ar:
//!   true}`) from our handle to our iPhone (the device of our account whose
//!   IDS identities include the relay number; `get_sms_targets`), and
//!   records `requested`. The user then switches this device on in the
//!   iPhone's Settings > Messages > Text Message Forwarding. The iPhone
//!   tells the device it enables with its own `145` (rustpush answers it
//!   with `MessageReadOnDevice`, `c = 147`, by itself) and from then on
//!   relays texts; either makes the state `active` ([`Inner::on_sms_inbound`]).
//!   A `145` with `{ue: true}` (`EnableSmsActivation(false)`: the user
//!   turned it off on the phone) makes it `off`. Disabling here sends that
//!   same `{ue: true}` to the iPhone.
//! * **Sending** ([`Inner::send_sms`]): a `NormalMessage` with
//!   `MessageType::SMS { is_phone: false, using_number: <relay number>,
//!   from_handle: None }` to the recipients, sent from our handle and
//!   targeted at the iPhone only (rustpush then uses the sms topic and
//!   delivers to our own handle). The iPhone sends the text and answers
//!   `SmsConfirmSent(true / false)` under the same message id (`convert`
//!   turns that into a `MessageSent`).
//!
//! The state is kept in `state.enc` ([`SavedState::sms`](crate::state::SavedState)).

use imessage_common::model::{ChatId, Handle, MessageId, Service, SmsForwarding, SmsState};
use rustpush::{ConversationData, IMClient, Message as RpMessage, MessageInst, MessageParts, MessageTarget, MessageType, NormalMessage, PrivateDeviceInfo};

use super::{Inner, lock, normalize_handles, now_ms, perr};
use crate::engine::{ChatTarget, EngineError, EngineEvent, EngineResult, SentMessage, convert};

/// The IDS topic text messages travel on.
pub(super) const SMS_TOPIC: &str = "com.apple.private.alloy.sms";

/// Whether `device` (one of our account's registrations) carries `number`.
fn carries(device: &PrivateDeviceInfo, number: &str) -> bool {
    device.identites.iter().any(|i| Handle::normalize(i) == number)
}

/// The devices that can relay texts for `number`. Every device of the
/// account lists the iPhone's number among its identities (iPad, Watch,
/// Macs share it through iCloud), but only the iPhone has the SIM: prefer
/// devices named like an iPhone, else all that carry the number.
fn relay_devices<'a>(devices: &'a [PrivateDeviceInfo], number: &str) -> Vec<&'a PrivateDeviceInfo> {
    let carrying: Vec<&PrivateDeviceInfo> = devices.iter().filter(|d| carries(d, number)).collect();
    let phones: Vec<&PrivateDeviceInfo> =
        carrying.iter().copied().filter(|d| d.device_name.as_deref().is_some_and(|n| n.to_lowercase().contains("iphone"))).collect();
    if phones.is_empty() { carrying } else { phones }
}

/// Recipients of a text message: phone numbers only, not ours, at least one.
pub(super) fn sms_recipients(raw: &[String], ours: &[String]) -> EngineResult<Vec<String>> {
    let all = normalize_handles(raw)?;
    let others = convert::others(&all, ours);
    if others.is_empty() {
        return Err(EngineError::InvalidInput("a text message needs a recipient other than you".into()));
    }
    if let Some(h) = others.iter().find(|h| !h.starts_with("tel:")) {
        return Err(EngineError::InvalidInput(format!("{h} is not a phone number; text messages (SMS) go to phone numbers only")));
    }
    Ok(others)
}

impl Inner {
    /// The current state, with the configured relay number.
    pub(super) fn sms_view(&self) -> SmsForwarding {
        let mut s = lock(&self.saved).sms.clone();
        if s.number.is_none() || s.state == SmsState::Off {
            s.number = self.cfg.sms_number();
        }
        s
    }

    /// Record a state change, persist it and announce it. No-op (no event)
    /// when nothing changes.
    pub(super) fn set_sms_state(&self, state: SmsState, detail: Option<String>) -> SmsForwarding {
        let number = self.cfg.sms_number();
        let new = {
            let mut g = lock(&self.saved);
            if g.sms.state == state && g.sms.detail == detail && g.sms.number == number {
                return g.sms.clone();
            }
            g.sms = SmsForwarding { state, number, since: Some(now_ms()), detail };
            g.sms.clone()
        };
        self.request_save();
        self.emit(EngineEvent::SmsForwardingChanged(new.clone()));
        new
    }

    /// Push tokens of our iPhone (the device whose identities include
    /// `number`), refreshing the device list once if it is not there.
    async fn sms_relay_targets(&self, client: &IMClient, me: &str, number: &str) -> EngineResult<Vec<MessageTarget>> {
        let mut devices = client.identity.get_sms_targets(me, false).await.map_err(perr)?;
        if !devices.iter().any(|d| carries(d, number)) {
            devices = client.identity.get_sms_targets(me, true).await.map_err(perr)?;
        }
        let relays = relay_devices(&devices, number);
        log::debug!(
            "sms relay: {} of {} device(s) chosen; account devices:{}",
            relays.len(),
            devices.len(),
            devices
                .iter()
                .map(|d| format!(
                    " [{}; sms sub-service: {}; carries number: {}; identities: {}; hsa trusted: {}]",
                    d.device_name.as_deref().unwrap_or("?"),
                    d.sub_services.iter().any(|s| s == SMS_TOPIC),
                    carries(d, number),
                    d.identites.len(),
                    d.is_hsa_trusted
                ))
                .collect::<String>()
        );
        if relays.is_empty() {
            return Err(EngineError::Unsupported(format!(
                "no device on this Apple ID is registered with {number}; is the iPhone signed in to iMessage with that number?"
            )));
        }
        Ok(relays.into_iter().map(|d| MessageTarget::Token(d.token.clone())).collect())
    }

    /// Ask our iPhone to start (or stop) relaying text messages to us.
    pub(super) async fn set_sms_forwarding(&self, enable: bool) -> EngineResult<SmsForwarding> {
        let client = self.client()?;
        let number = self.cfg.sms_number().ok_or_else(|| {
            EngineError::InvalidInput("no phone number to relay through: set sms_relay_number (or add your iPhone's number to other_handles)".into())
        })?;
        let (_, me) = self.my_handles(&client).await?;
        let targets = match self.sms_relay_targets(&client, &me, &number).await {
            Ok(t) => t,
            Err(e) => {
                if enable {
                    self.set_sms_state(SmsState::Error, Some(e.to_string()));
                    return Err(e);
                }
                // Turning it off locally works without the phone.
                log::warn!("sms forwarding: cannot reach the iPhone to turn it off there: {e}");
                return Ok(self.set_sms_state(SmsState::Off, None));
            }
        };
        let conv = ConversationData { participants: vec![], cv_name: None, sender_guid: Some(uuid::Uuid::new_v4().to_string()), after_guid: None };
        let mut inst = MessageInst::new(conv, &me, RpMessage::EnableSmsActivation(enable));
        inst.target = Some(targets);
        if let Err(e) = client.send(&mut inst).await {
            let e = perr(e);
            if enable {
                self.set_sms_state(SmsState::Error, Some(format!("the request did not go out: {e}")));
            }
            return Err(e);
        }
        log::info!("sms forwarding: {} request sent to the iPhone ({number})", if enable { "enable" } else { "disable" });
        let current = lock(&self.saved).sms.state;
        Ok(match (enable, current) {
            // Asking again while it works keeps it working.
            (true, SmsState::Active) => self.sms_view(),
            (true, _) => self.set_sms_state(SmsState::Requested, None),
            (false, _) => self.set_sms_state(SmsState::Off, None),
        })
    }

    /// Track activation from what arrives: the iPhone's own activation
    /// message (from one of our handles), or a text it relayed.
    /// `registered`: the handles this device is registered for; `ours`:
    /// those plus `other_handles` and the relay number.
    pub(super) async fn on_sms_inbound(&self, client: &IMClient, inst: &MessageInst, registered: &[String], ours: &[String]) {
        let is = |list: &[String], s: &str| list.iter().any(|h| Handle::normalize(h) == Handle::normalize(s));
        let sender = inst.sender.as_deref().unwrap_or("");
        match &inst.message {
            RpMessage::EnableSmsActivation(on) if is(ours, sender) => {
                log::info!("sms forwarding: the iPhone {} Text Message Forwarding for this device", if *on { "enabled" } else { "disabled" });
                self.set_sms_state(if *on { SmsState::Active } else { SmsState::Off }, None);
                // rustpush acknowledges an activation (MessageReadOnDevice)
                // from the handle it came *to*, which fails when that is a
                // number this device is not registered for: then do it from
                // our own handle.
                if *on && !is(registered, sender) {
                    let ack = async {
                        let (_, me) = self.my_handles(client).await?;
                        let number = self.cfg.sms_number().unwrap_or_else(|| Handle::normalize(sender));
                        let targets = self.sms_relay_targets(client, &me, &number).await?;
                        let conv = ConversationData { participants: vec![], cv_name: None, sender_guid: Some(uuid::Uuid::new_v4().to_string()), after_guid: None };
                        let mut m = MessageInst::new(conv, &me, RpMessage::MessageReadOnDevice);
                        m.target = Some(targets);
                        client.send(&mut m).await.map_err(perr)?;
                        Ok::<(), EngineError>(())
                    };
                    match tokio::time::timeout(super::HANDLE_TIMEOUT, ack).await {
                        Ok(Ok(())) => log::info!("sms forwarding: confirmed the activation to the iPhone"),
                        Ok(Err(e)) => log::warn!("sms forwarding: could not confirm the activation to the iPhone: {e}"),
                        Err(_) => log::warn!("sms forwarding: confirming the activation timed out"),
                    }
                }
            }
            RpMessage::EnableSmsActivation(_) => log::warn!("sms forwarding: ignoring an activation message from someone else ({})", inst.id),
            RpMessage::Message(NormalMessage { service: MessageType::SMS { from_handle, .. }, .. }) => {
                if lock(&self.saved).sms.state != SmsState::Active {
                    log::info!("sms forwarding: the iPhone relayed a text message ({}); forwarding is active", if from_handle.is_some() { "incoming" } else { "sent" });
                    self.set_sms_state(SmsState::Active, None);
                }
            }
            RpMessage::SmsConfirmSent(ok) => log::info!("sms {}: the iPhone reports {}", inst.id, if *ok { "sent" } else { "NOT sent" }),
            _ => {}
        }
    }

    /// Send a text message (SMS / MMS) through our iPhone. Needs forwarding
    /// `active` or `requested` (the iPhone's confirmation may not have
    /// reached us; the hub only picks SMS by itself when `active`).
    pub(super) async fn send_sms(&self, chat: &ChatTarget, parts: MessageParts) -> EngineResult<SentMessage> {
        let sms = self.sms_view();
        if !matches!(sms.state, SmsState::Active | SmsState::Requested) {
            return Err(EngineError::Unsupported("Text Message Forwarding is not on (run `imsg sms enable`)".into()));
        }
        let number = sms.number.clone().or_else(|| self.cfg.sms_number()).ok_or_else(|| EngineError::InvalidInput("no SMS relay number configured".into()))?;
        let client = self.client()?;
        let (mine, me) = self.my_handles(&client).await?;
        let mut ours = self.with_other_handles(&mine);
        ours.push(number.clone());
        let raw: Vec<String> = match chat {
            ChatTarget::Participants(p) => p.clone(),
            ChatTarget::Chat(id) => {
                if let Some(list) = id.0.strip_prefix(ChatId::GROUP_PARTICIPANTS_PREFIX) {
                    list.split(',').map(str::to_string).collect()
                } else if id.is_group() {
                    // An iMessage group (group id): only if we know who is in it.
                    let known = lock(&self.chats).get(id).map(|c| c.participants.clone());
                    known.ok_or_else(|| EngineError::ChatNotFound(id.clone()))?
                } else {
                    vec![id.0.clone()]
                }
            }
        };
        let recipients = sms_recipients(&raw, &ours)?;
        let chat_id = convert::sms_chat_id(&recipients);
        let mut participants = recipients.clone();
        participants.push(me.clone());
        let conv = ConversationData { participants, cv_name: None, sender_guid: None, after_guid: None };
        let service = MessageType::SMS { is_phone: false, using_number: number.clone(), from_handle: None };
        let mut n = NormalMessage::new(String::new(), service);
        n.parts = parts;
        let mut inst = MessageInst::new(conv, &me, RpMessage::Message(n));
        inst.target = Some(self.sms_relay_targets(&client, &me, &number).await?);
        log::info!("sms {}: sending through the iPhone to {} recipient(s)", MessageId::new(&inst.id), recipients.len());
        self.send_inst(&client, chat_id, inst, Handle::new(&number), Service::Sms, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipients_are_other_phone_numbers() {
        let ours = vec!["mailto:me@example.com".to_string(), "tel:+15555550100".to_string()];
        assert_eq!(sms_recipients(&["(555) 555-0123".into()], &ours).unwrap(), vec!["tel:5555550123"]);
        assert_eq!(sms_recipients(&["+15555550123".into(), "tel:+15555550100".into()], &ours).unwrap(), vec!["tel:+15555550123"]);
        assert!(matches!(sms_recipients(&["bob@example.com".into()], &ours), Err(EngineError::InvalidInput(_))));
        assert!(matches!(sms_recipients(&["tel:+15555550100".into()], &ours), Err(EngineError::InvalidInput(_))));
        assert!(matches!(sms_recipients(&["bob".into()], &ours), Err(EngineError::InvalidHandle(_))));
    }

    #[test]
    fn device_carries_number() {
        let d = PrivateDeviceInfo {
            uuid: None,
            device_name: Some("iPhone".into()),
            token: vec![1],
            is_hsa_trusted: true,
            identites: vec!["mailto:me@example.com".into(), "tel:+15555550100".into()],
            sub_services: vec![SMS_TOPIC.into()],
        };
        assert!(carries(&d, "tel:+15555550100"));
        assert!(!carries(&d, "tel:+15555550199"));
        let named = |n: &str, t: u8| PrivateDeviceInfo { device_name: Some(n.into()), token: vec![t], ..d.clone() };
        let all = vec![named("Sam's iPad", 2), named("Sam’s iPhone", 3), named("Work Mac", 4)];
        let r = relay_devices(&all, "tel:+15555550100");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].token, vec![3]);
        let no_phone = vec![named("Sam's iPad", 2), named("Work Mac", 4)];
        assert_eq!(relay_devices(&no_phone, "tel:+15555550100").len(), 2);
    }
}
