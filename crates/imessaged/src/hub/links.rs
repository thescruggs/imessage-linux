//! Contact links (`crate::contacts`): one conversation per person.
//!
//! Engine events and sends name the address actually used; the hub files
//! them under the person's canonical chat ([`Hub::link_event`],
//! [`Hub::link_sent`]) and sends to the address the person last wrote from
//! ([`Hub::route`]). Stored chats of a person's other addresses are merged
//! into the canonical one whenever the links are refreshed.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{ChatId, Handle, Message};

use super::{DaemonSignal, Hub};
use crate::contacts::{self, ContactLinks, Fingerprint};
use crate::engine::{ChatTarget, EngineEvent, SentMessage};

/// How often the contact folder is checked for changes.
pub const CONTACTS_POLL: Duration = Duration::from_secs(60);

/// What the current links were built from.
#[derive(Default)]
pub(super) struct LinkSource {
    pub dir: Option<PathBuf>,
    pub fingerprint: Option<Fingerprint>,
    pub cards: Vec<contacts::Card>,
    pub own: HashSet<String>,
}

impl Hub {
    /// Link the addresses on the contact cards in `dir` (`None` = off).
    pub fn set_contacts_dir(&self, dir: Option<PathBuf>) {
        let mut src = self.link_source.lock().unwrap_or_else(|p| p.into_inner());
        *src = LinkSource { dir, ..Default::default() };
    }

    pub fn contact_links(&self) -> Arc<ContactLinks> {
        self.links.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The chat `id` is kept under.
    pub fn link_chat(&self, id: &ChatId) -> ChatId {
        self.contact_links().chat(id)
    }

    /// `ev` with every chat id replaced by the one it is kept under.
    pub fn link_event(&self, mut ev: EngineEvent) -> EngineEvent {
        let l = self.contact_links();
        if l.is_empty() {
            return ev;
        }
        let fix = |id: &mut ChatId| *id = l.chat(id);
        match &mut ev {
            EngineEvent::MessageReceived { message, chat, .. } => {
                fix(&mut message.chat_id);
                let to = l.chat(&chat.id);
                if to != chat.id {
                    chat.participants = vec![Handle::new(&to.0)];
                    chat.id = to;
                }
            }
            EngineEvent::MessageSent { chat_id, .. } => chat_id.iter_mut().for_each(fix),
            EngineEvent::Delivered(r) => r.chat_id.iter_mut().for_each(fix),
            EngineEvent::Read(r) => r.chat_id.iter_mut().for_each(fix),
            EngineEvent::Typing(t) => fix(&mut t.chat_id),
            EngineEvent::Reaction { chat_id, .. }
            | EngineEvent::Edited { chat_id, .. }
            | EngineEvent::Unsent { chat_id, .. }
            | EngineEvent::ChatRenamed { chat_id, .. }
            | EngineEvent::ParticipantsChanged { chat_id, .. }
            | EngineEvent::ChatBackground { chat_id, .. }
            | EngineEvent::ChatIcon { chat_id, .. } => fix(chat_id),
            EngineEvent::ProfileShared { .. }
            | EngineEvent::ConnectionChanged(_)
            | EngineEvent::RegistrationChanged(_)
            | EngineEvent::TwoFactorRequired
            | EngineEvent::SmsForwardingChanged(_)
            | EngineEvent::Error(_) => {}
        }
        ev
    }

    /// `sent` filed under the chat it is kept in.
    pub(super) fn link_sent(&self, mut sent: SentMessage) -> SentMessage {
        sent.chat_id = self.link_chat(&sent.chat_id);
        sent
    }

    /// Where a send to a linked person's chat goes: the address `about`
    /// (a message being answered or changed) came from — for one of ours,
    /// the address they last wrote from before it — else the one they last
    /// wrote from, else their canonical address. `sms`: a text message,
    /// so only their phone numbers count. Anything else is sent as asked.
    pub(super) async fn route(&self, target: ChatTarget, about: Option<&Message>, sms: bool) -> ChatTarget {
        let ChatTarget::Chat(id) = &target else { return target };
        let l = self.contact_links();
        let members: Vec<String> = l.members(id).into_iter().filter(|h| !sms || h.starts_with("tel:")).collect();
        if members.is_empty() {
            return target;
        }
        let canon = l.chat(id);
        let mut from = about.filter(|m| !m.is_from_me).map(|m| Handle::normalize(&m.sender.uri));
        if from.is_none()
            && let Ok(store) = self.store()
        {
            let (c, before) = (canon.clone(), about.map(|m| m.sent_at));
            from = store.call(move |s| s.latest_incoming_sender(&c, before)).await.ok().flatten().map(|h| Handle::normalize(&h));
        }
        // Canonical = the first phone number, so it also suits a text message.
        let to = from.filter(|h| members.contains(h)).unwrap_or(canon.0);
        ChatTarget::Chat(ChatId(to))
    }

    /// Re-read the contact cards if they (or our own addresses) changed,
    /// then merge stored chats of linked addresses into their person's
    /// chat. Clients are told to resync when chats went away. Nothing is
    /// linked before we know our own addresses (registration), so our own
    /// card never links our own chats.
    pub async fn refresh_contact_links(self: &Arc<Self>) {
        let Some(dir) = self.link_source.lock().unwrap_or_else(|p| p.into_inner()).dir.clone() else { return };
        let own = self.own_handles().await;
        if self.engine.registration_state().await.handles.is_empty() {
            return;
        }
        let d = dir.clone();
        let Ok(fp) = tokio::task::spawn_blocking(move || contacts::fingerprint(&d)).await else { return };
        let (stale, same_own) = {
            let src = self.link_source.lock().unwrap_or_else(|p| p.into_inner());
            (src.fingerprint.as_ref() != Some(&fp), src.own == own)
        };
        if stale || !same_own {
            let cards = if stale {
                let d = dir.clone();
                tokio::task::spawn_blocking(move || contacts::load_cards(&d)).await.unwrap_or_default()
            } else {
                self.link_source.lock().unwrap_or_else(|p| p.into_inner()).cards.clone()
            };
            let links = ContactLinks::build(&cards, &own);
            {
                let mut src = self.link_source.lock().unwrap_or_else(|p| p.into_inner());
                if src.dir.as_ref() != Some(&dir) {
                    return; // switched off meanwhile
                }
                src.fingerprint = Some(fp);
                src.cards = cards;
                src.own = own;
            }
            if *self.contact_links() != links {
                log::info!("contact links: {} people with more than one address", links.people());
                *self.links.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(links);
            }
        }
        self.merge_linked_chats().await;
    }

    /// Fold stored chats of linked addresses into their person's chat.
    pub(super) async fn merge_linked_chats(self: &Arc<Self>) {
        let merges = self.contact_links().merges();
        if merges.is_empty() {
            return;
        }
        let Ok(store) = self.store() else { return };
        match store.call(move |s| s.merge_chats(&merges)).await {
            Ok((_, removed)) if removed.is_empty() => {}
            Ok((updates, removed)) => {
                log::info!("merged {} conversation(s) into their contact's conversation", removed.len());
                self.publish(updates).await;
                self.emit(DaemonSignal::BackgroundError("conversations of one contact were merged; clients should resync".into())).await;
            }
            Err(e) => log::warn!("cannot merge linked conversations: {e}"),
        }
    }

    /// [`Hub::refresh_contact_links`] every [`CONTACTS_POLL`] until
    /// the hub stops.
    pub fn spawn_contact_links(self: &Arc<Self>) {
        let hub = Arc::downgrade(self);
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(CONTACTS_POLL) => {}
                    _ = stop.wait_for(|s| *s) => return,
                }
                let Some(hub) = hub.upgrade() else { return };
                hub.refresh_contact_links().await;
            }
        });
    }
}
