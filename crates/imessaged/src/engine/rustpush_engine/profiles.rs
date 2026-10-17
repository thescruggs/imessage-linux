//! Contacts' shared names and photos ("Share Name and Photo") through
//! CloudKit.
//!
//! A share in a message names a record in Apple's public
//! `com.apple.messages.profiles` CloudKit container plus the key that
//! decrypts it; rustpush's `ProfilesClient::get_record` fetches and opens
//! it. CloudKit wants the account's MobileMe delegate tokens
//! (`cloudKitToken`, `mmeAuthToken`) and its dsid. We get the tokens at
//! login (`LoginDelegate::MobileMe` next to IDS) and keep them, as
//! rustpush's `TokenProviderState`, sealed in `state.enc`. rustpush's
//! `TokenProvider` refreshes them weekly (or on a 401) with the account's
//! PET token, which `AppleAccount` renews by itself with the saved SRP
//! hash; if Apple wants a 2FA code for that, the tokens are gone until the
//! user logs in again ([`ProfilesState::NeedsLogin`]).
//!
//! A session saved by an older daemon has no tokens. The first use (the
//! startup warm-up, or the first fetch) then asks for them silently the
//! same way. Attempts are bounded: `NeedsLogin` stays until a login, other
//! failures wait [`RETRY_TRANSIENT`] / [`RETRY_OTHER`] before the next try.
//!
//! rustpush's CloudKit code `expect`s / `panic!`s on odd server answers
//! (a deleted record, a missing delegate), so every call runs in its own
//! task and a panic becomes an error.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use imessage_common::model::{ProfilesState, ProfilesStatus};
use rustpush::cloudkit::{CloudKitClient, CloudKitState};
use rustpush::name_photo_sharing::ProfilesClient;
use rustpush::{DebugMutex, DebugRwLock, MobileMeDelegateResponse, OSConfig, PersistAccountData, PushError, TokenProvider, TokenProviderState};

use super::{Inner, Provider, lock};
use crate::engine::{EngineError, EngineEvent, EngineResult, FetchedProfile, MAX_PROFILE_PHOTO_BYTES, ShareRef};
use crate::engine::convert;
use imessage_common::model::ConnectionState;

/// Wait after a network / anisette failure before trying CloudKit again.
pub(super) const RETRY_TRANSIENT: Duration = Duration::from_secs(15 * 60);
/// Wait after any other failure.
pub(super) const RETRY_OTHER: Duration = Duration::from_secs(60 * 60);
/// The startup warm-up waits this long for the connection to come up.
const WARMUP_WAIT: Duration = Duration::from_secs(10 * 60);

pub(super) type Profiles = ProfilesClient<Provider>;

/// In-memory CloudKit state of the engine.
#[derive(Default)]
pub(super) struct ProfilesSlot {
    pub client: Option<Arc<Profiles>>,
    pub status: ProfilesStatus,
    /// No new attempt before this (after a failure).
    pub retry_at: Option<Instant>,
    /// "needs a fresh login" was logged already.
    pub warned: bool,
}

/// rustpush's `TokenProviderState` (private fields) holding `mme`, as a
/// plist value, dated now.
pub(super) fn token_state_with(mme: &MobileMeDelegateResponse) -> Option<plist::Value> {
    let mut v = plist::to_value(&TokenProviderState::default()).ok()?;
    let d = v.as_dictionary_mut()?;
    d.insert("mme".into(), plist::to_value(mme).ok()?);
    d.insert("refreshed".into(), plist::to_value(&SystemTime::now()).ok()?);
    plist::from_value::<TokenProviderState>(&v).ok()?;
    Some(v)
}

/// How a CloudKit failure counts (pure; unit tested): the state to show,
/// a log-safe reason, and when to try again (`None` = not before a login).
pub(super) fn classify(e: &PushError) -> (ProfilesState, String, Option<Duration>) {
    use PushError as P;
    match e {
        // No PET token: Apple wants the password / a 2FA code again.
        P::TokenMissing => (ProfilesState::NeedsLogin, "Apple wants a fresh sign-in for iCloud".into(), None),
        P::UnauthorizedAccountError => (ProfilesState::NeedsLogin, "Apple refused the iCloud sign-in (unauthorized)".into(), None),
        P::MobileMeError(m, _) => (ProfilesState::NeedsLogin, format!("Apple refused the iCloud sign-in ({m})"), None),
        P::DelegateLoginFailed(d, status, _) => (ProfilesState::NeedsLogin, format!("Apple refused the {d} delegate (status {status})"), None),
        // The parsed response can be anything; do not print it.
        P::AuthError(_) => (ProfilesState::NeedsLogin, "Apple refused the iCloud sign-in".into(), None),
        P::AnisetteError(a) => (ProfilesState::Unavailable, format!("anisette source failed: {a}"), Some(RETRY_TRANSIENT)),
        P::RequestError(_) | P::IoError(_) | P::ResourceTimeout | P::NotConnected | P::TooManyRequests => {
            (ProfilesState::Unavailable, format!("network: {e}"), Some(RETRY_TRANSIENT))
        }
        other => (ProfilesState::Unavailable, other.to_string(), Some(RETRY_OTHER)),
    }
}

/// Text of a task panic.
fn panic_text(e: tokio::task::JoinError) -> String {
    match e.try_into_panic() {
        Ok(p) => p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into()),
        Err(e) => e.to_string(),
    }
}

impl Inner {
    pub(super) fn profiles_status_now(&self) -> ProfilesStatus {
        lock(&self.profiles).status.clone()
    }

    fn set_profiles_status(&self, state: ProfilesState, detail: Option<String>, retry: Option<Duration>) {
        let mut g = lock(&self.profiles);
        if state == ProfilesState::NeedsLogin {
            g.client = None;
            if !g.warned {
                g.warned = true;
                log::warn!(
                    "shared names and photos need a fresh login: {}. Run `imsg login` once (the password, and a 2FA code \
                     if Apple asks); messaging keeps working meanwhile",
                    detail.as_deref().unwrap_or("no iCloud access")
                );
            }
        }
        g.retry_at = retry.map(|d| Instant::now() + d);
        if g.status.state != state || g.status.detail != detail {
            log::info!("shared profiles: {state:?}{}", detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default());
        }
        g.status = ProfilesStatus { state, detail };
    }

    /// A fresh login brought new MobileMe tokens (or none): start over.
    pub(super) fn reset_profiles(&self, tokens: Option<plist::Value>) {
        let have = tokens.is_some();
        if self.cfg.store_credentials {
            lock(&self.saved).cloudkit_tokens = tokens;
        }
        let mut g = lock(&self.profiles);
        *g = ProfilesSlot::default();
        if !have {
            g.status = ProfilesStatus { state: ProfilesState::Unavailable, detail: Some("Apple returned no iCloud tokens at login".into()) };
            g.retry_at = Some(Instant::now() + RETRY_OTHER);
        }
    }

    /// The CloudKit profiles client, building it (and checking it works)
    /// on first use. Never prompts; see the module docs for the retry rules.
    pub(super) async fn profiles_client(self: &Arc<Self>) -> EngineResult<Arc<Profiles>> {
        let _guard = self.profiles_init.lock().await;
        {
            let g = lock(&self.profiles);
            if let Some(c) = &g.client {
                return Ok(c.clone());
            }
            let detail = g.status.detail.clone().unwrap_or_default();
            match g.status.state {
                ProfilesState::NeedsLogin => return Err(EngineError::Unsupported(format!("shared profiles need `imsg login`: {detail}"))),
                ProfilesState::Unavailable if g.retry_at.is_none_or(|t| Instant::now() < t) => {
                    return Err(EngineError::Unsupported(format!("shared profiles unavailable: {detail}")));
                }
                _ => {}
            }
        }
        self.check_not_shutting_down()?;
        if !self.cfg.store_credentials {
            self.set_profiles_status(ProfilesState::Unavailable, Some("store_credentials = false (CloudKit needs the saved Apple ID session)".into()), None);
            return Err(EngineError::Unsupported("shared profiles need store_credentials = true".into()));
        }
        let config = self.os_config()?;
        let (persisted, tokens, push) = {
            let s = lock(&self.saved);
            (s.account.as_ref().and_then(|v| plist::from_value::<PersistAccountData>(v).ok()), s.cloudkit_tokens.clone(), s.push.clone())
        };
        let Some(persisted) = persisted.filter(|p| p.dsid != 0 && !p.username.is_empty()) else {
            self.set_profiles_status(ProfilesState::NeedsLogin, Some("no saved Apple ID session".into()), None);
            return Err(EngineError::Unsupported("shared profiles need `imsg login`".into()));
        };
        // Debug override: run the "no tokens yet" path again (the saved
        // tokens are not deleted; the fresh ones replace them).
        let forced = tokens.is_some() && std::env::var_os("IMESSAGED_PROFILES_FORCE_REFRESH").is_some_and(|v| v == "1");
        let tokens = if forced {
            log::warn!("shared profiles: IMESSAGED_PROFILES_FORCE_REFRESH=1: ignoring the saved iCloud tokens (debug override)");
            None
        } else {
            tokens
        };
        let fresh = tokens.is_none();
        if fresh {
            log::info!("shared profiles: the saved session has no iCloud tokens yet; asking Apple for them with the saved credentials");
        }
        let tp_state: TokenProviderState = tokens.and_then(|v| plist::from_value(&v).ok()).unwrap_or_default();
        let dsid = persisted.dsid.to_string();
        let account = self.new_account(&config, &push, Some(persisted))?;
        let weak = Arc::downgrade(self);
        let tp = TokenProvider::new(
            Arc::new(DebugMutex::new(account)),
            config.clone() as Arc<dyn OSConfig>,
            tp_state,
            Box::new(move |s: &TokenProviderState| {
                if let Some(i) = weak.upgrade()
                    && i.cfg.store_credentials
                {
                    lock(&i.saved).cloudkit_tokens = plist::to_value(s).ok();
                    i.request_save();
                }
            }),
        );
        let state = CloudKitState::new(dsid).ok_or_else(|| EngineError::Protocol("CloudKit state".into()))?;
        let ck = Arc::new(CloudKitClient {
            anisette: self.anisette(&config, &push),
            state: DebugRwLock::new(state),
            config: config as Arc<dyn OSConfig>,
            token_provider: tp,
        });
        let client = Arc::new(ProfilesClient::new(ck));
        // Open the container now: this is where missing / dead tokens show.
        let c = client.clone();
        let opened = tokio::spawn(async move { c.get_container().await.map(|_| ()) }).await;
        match opened {
            Ok(Ok(())) => {
                let mut g = lock(&self.profiles);
                g.client = Some(client.clone());
                g.retry_at = None;
                drop(g);
                self.set_profiles_status(ProfilesState::Ready, None, None);
                if fresh {
                    log::info!("shared profiles: iCloud access obtained silently");
                }
                Ok(client)
            }
            Ok(Err(e)) => {
                let (state, detail, retry) = classify(&e);
                self.set_profiles_status(state, Some(detail.clone()), retry);
                Err(match state {
                    ProfilesState::Unavailable if retry == Some(RETRY_TRANSIENT) => EngineError::Transient(detail),
                    _ => EngineError::Unsupported(detail),
                })
            }
            Err(join) => {
                let why = format!("CloudKit failed: {}", panic_text(join));
                self.set_profiles_status(ProfilesState::Unavailable, Some(why.clone()), Some(RETRY_OTHER));
                Err(EngineError::Protocol(why))
            }
        }
    }

    pub(super) async fn fetch_profile(self: &Arc<Self>, share: &ShareRef) -> EngineResult<FetchedProfile> {
        let client = self.profiles_client().await?;
        let msg = convert::share_message(share);
        let r = tokio::spawn(async move { client.get_record(&msg).await }).await;
        let record = match r {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let (state, detail, _) = classify(&e);
                if state == ProfilesState::NeedsLogin {
                    // The tokens died and could not be renewed silently.
                    self.set_profiles_status(state, Some(detail.clone()), None);
                    return Err(EngineError::Unsupported(detail));
                }
                return Err(match &e {
                    PushError::AnisetteError(_) | PushError::RequestError(_) | PushError::IoError(_) | PushError::TooManyRequests => {
                        EngineError::Transient(detail)
                    }
                    _ => EngineError::Protocol(detail),
                });
            }
            // e.g. "No record found?": the contact deleted the record.
            Err(join) => return Err(EngineError::Protocol(format!("profile fetch failed: {}", panic_text(join)))),
        };
        let image = record.image.filter(|i| {
            let ok = (i.len() as u64) <= MAX_PROFILE_PHOTO_BYTES;
            if !ok {
                log::warn!("shared photo of {} bytes is over the limit ({MAX_PROFILE_PHOTO_BYTES}); keeping the name only", i.len());
            }
            ok && !i.is_empty()
        });
        Ok(FetchedProfile { name: record.name.name, first: record.name.first, last: record.name.last, image })
    }

    /// Startup: once the connection is up, make sure CloudKit works (and
    /// obtain the tokens silently if the saved session has none). One try
    /// per daemon start; skipped when the connection (and so the anisette
    /// source) never comes up.
    pub(super) fn spawn_profiles_warmup(self: &Arc<Self>) {
        if lock(&self.saved).account.is_none() {
            return;
        }
        let weak = Arc::downgrade(self);
        let mut events = self.events.subscribe();
        let t = tokio::spawn(async move {
            let connected = |w: &std::sync::Weak<Inner>| w.upgrade().is_some_and(|i| i.connection() == ConnectionState::Connected);
            let deadline = tokio::time::Instant::now() + WARMUP_WAIT;
            while !connected(&weak) {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Ok(EngineEvent::ConnectionChanged(_))) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => return,
                    Err(_) => {
                        log::info!("shared profiles: not connected after {} min; not trying iCloud this start", WARMUP_WAIT.as_secs() / 60);
                        return;
                    }
                }
            }
            let Some(inner) = weak.upgrade() else { return };
            if inner.is_shutting_down() || inner.relogin_running.load(Ordering::SeqCst) {
                return;
            }
            if let Err(e) = inner.profiles_client().await {
                log::info!("shared profiles not available: {e}");
            }
        });
        lock(&self.tasks).push(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_state_round_trips() {
        let mme = MobileMeDelegateResponse {
            tokens: [("cloudKitToken".to_string(), "t".to_string())].into_iter().collect(),
            config: plist::Dictionary::new(),
        };
        let v = token_state_with(&mme).expect("builds");
        let _: TokenProviderState = plist::from_value(&v).unwrap();
        assert!(v.as_dictionary().unwrap().contains_key("mme"));
    }

    #[test]
    fn failure_classes() {
        assert_eq!(classify(&PushError::TokenMissing).0, ProfilesState::NeedsLogin);
        assert_eq!(classify(&PushError::TokenMissing).2, None);
        let (s, _, retry) = classify(&PushError::ResourceTimeout);
        assert_eq!((s, retry), (ProfilesState::Unavailable, Some(RETRY_TRANSIENT)));
        assert_eq!(classify(&PushError::BadMsg).2, Some(RETRY_OTHER));
        // never prints the raw response
        let d = classify(&PushError::AuthError(plist::Value::String("SECRET".into()))).1;
        assert!(!d.contains("SECRET"));
    }
}
