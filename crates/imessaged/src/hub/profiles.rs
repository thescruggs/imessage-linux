//! Contacts' shared names and photos and group photos: recording shares,
//! background fetches, refresh when a chat is opened.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use imessage_common::model::{Chat, ChatId, Handle, HandleProfile};

use super::{DaemonSignal, Hub, InFlight, OpError, OpResult, safe_component};
use crate::engine::{EngineError, MAX_ICON_BYTES, ShareRef};
use crate::store::{IconDownload, ShareOutcome, StoreError};

/// Contacts' photos: `profiles/<handle>/<record key>.<ext>`.
pub const PROFILES_DIR: &str = "profiles";
/// Group photos: `chat_icons/<chat id>/<id>.<ext>`.
pub const CHAT_ICONS_DIR: &str = "chat_icons";
/// A profile fetched longer ago than this is fetched again when a chat
/// with that person is opened.
pub const PROFILE_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// After a failed fetch, the same handle is not tried again for this long
/// (unless its share changes).
pub const PROFILE_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// Profile fetches running at once.
pub const PROFILE_FETCH_CONCURRENCY: usize = 2;

/// Profile fetch bookkeeping of the hub.
pub(crate) struct ProfileFetches {
    pub in_flight: Arc<Mutex<std::collections::HashSet<String>>>,
    pub failed: Mutex<HashMap<String, Instant>>,
    pub permits: Arc<tokio::sync::Semaphore>,
    pub icons: Arc<Mutex<std::collections::HashSet<ChatId>>>,
}

impl Default for ProfileFetches {
    fn default() -> Self {
        ProfileFetches {
            in_flight: Arc::default(),
            failed: Mutex::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(PROFILE_FETCH_CONCURRENCY)),
            icons: Arc::default(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// File extension for an image by its first bytes (pure; unit tested).
pub fn image_ext(b: &[u8]) -> &'static str {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if b.starts_with(b"GIF8") {
        "gif"
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "webp"
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && matches!(&b[8..12], b"heic" | b"heix" | b"mif1" | b"msf1" | b"heim" | b"heis" | b"hevc") {
        "heic"
    } else {
        "img"
    }
}

/// A CloudKit record key (base64, may hold `/` and `+`) as a file name.
fn record_file_stem(key: &str) -> String {
    let k: String = key.chars().map(|c| match c {
        '/' => '_',
        '+' => '-',
        '=' => '~',
        c => c,
    }).collect();
    safe_component(&k, "profile")
}

fn task_err(e: impl std::fmt::Display) -> OpError {
    OpError::Store(StoreError::Task(e.to_string()))
}

impl Hub {
    pub fn profiles_dir(&self) -> PathBuf {
        self.state_dir.join(PROFILES_DIR)
    }

    pub fn chat_icons_dir(&self) -> PathBuf {
        self.state_dir.join(CHAT_ICONS_DIR)
    }

    /// `GetHandleProfile`: what `handle` shared (`None`: nothing).
    pub async fn get_handle_profile(&self, handle: &str) -> OpResult<Option<HandleProfile>> {
        let h = Handle::try_normalize(handle).ok_or_else(|| OpError::Engine(EngineError::InvalidHandle(handle.to_string())))?;
        Ok(self.store()?.call(move |s| s.handle_profile(&h)).await?)
    }

    /// An engine `ProfileShared`: record it, fetch what is new, clear what
    /// was withdrawn.
    pub async fn on_profile_shared(self: &Arc<Self>, handle: Handle, share: Option<ShareRef>, at: u64) {
        let Ok(store) = self.store() else { return };
        let h = handle.clone();
        let outcome = match store.call(move |s| s.record_profile_share(&h, share.as_ref(), at)).await {
            Ok(o) => o,
            Err(e) => return log::warn!("{}: recording a shared profile failed: {e}", handle.uri),
        };
        match outcome {
            ShareOutcome::Nothing => {}
            ShareOutcome::Changed => {
                log::info!("{} shared a (new) name and photo", handle.uri);
                self.profile_fetches.failed.lock().unwrap_or_else(|p| p.into_inner()).remove(&handle.uri);
                self.spawn_profile_fetch(handle.uri);
            }
            ShareOutcome::Unfetched => {
                if !self.failed_recently(&handle.uri) {
                    self.spawn_profile_fetch(handle.uri);
                }
            }
            ShareOutcome::Cleared { files, had_profile } => {
                self.remove_files(files).await;
                if had_profile {
                    log::info!("{} stopped sharing their name and photo", handle.uri);
                    self.announce_profile(&handle.uri, None).await;
                }
            }
        }
    }

    fn failed_recently(&self, handle: &str) -> bool {
        self.profile_fetches.failed.lock().unwrap_or_else(|p| p.into_inner()).get(handle).is_some_and(|t| t.elapsed() < PROFILE_RETRY_AFTER)
    }

    /// `HandleProfileUpdated`, then `ChatUpdated` for every chat with that
    /// participant (their Chat JSON carries the profile).
    async fn announce_profile(&self, handle: &str, profile: Option<HandleProfile>) {
        self.emit(DaemonSignal::HandleProfileUpdated { handle: handle.to_string(), profile }).await;
        let Ok(store) = self.store() else { return };
        let h = handle.to_string();
        match store.call(move |s| s.chats_with_participant(&h)).await {
            Ok(chats) => {
                for c in chats {
                    self.emit(DaemonSignal::ChatUpdated(c)).await;
                }
            }
            Err(e) => log::warn!("{handle}: cannot load its chats: {e}"),
        }
    }

    /// Fetch `handle`'s shared profile in the background (deduplicated, at
    /// most [`PROFILE_FETCH_CONCURRENCY`] at once).
    pub fn spawn_profile_fetch(self: &Arc<Self>, handle: String) {
        let Some(in_flight) = InFlight::claim(&self.profile_fetches.in_flight, handle.clone()) else { return };
        let hub = self.clone();
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let Ok(_permit) = hub.profile_fetches.permits.clone().acquire_owned().await else { return };
            match hub.fetch_profile(&handle).await {
                Ok(()) => {
                    hub.profile_fetches.failed.lock().unwrap_or_else(|p| p.into_inner()).remove(&handle);
                }
                Err(e) => {
                    hub.profile_fetches.failed.lock().unwrap_or_else(|p| p.into_inner()).insert(handle.clone(), Instant::now());
                    match e {
                        // Says why in the engine's log already (once).
                        OpError::Engine(EngineError::Unsupported(m)) => log::debug!("{handle}: shared profile not fetched: {m}"),
                        e => log::warn!("{handle}: fetching the shared profile failed: {e}"),
                    }
                }
            }
        });
    }

    async fn fetch_profile(&self, handle: &str) -> OpResult<()> {
        let store = self.store()?;
        let h = handle.to_string();
        let Some(share) = store.call(move |s| s.profile_share(&h)).await? else { return Ok(()) };
        let fetched = self.engine.fetch_profile(&share).await?;
        let mut photo: Option<String> = None;
        if let Some(img) = &fetched.image {
            let root = self.profiles_dir();
            let dir = root.join(safe_component(handle, "handle"));
            let dest = dir.join(format!("{}.{}", record_file_stem(&share.record_key), image_ext(img)));
            let (d, p, bytes) = (dir.clone(), dest.clone(), img.clone());
            tokio::task::spawn_blocking(move || {
                crate::state::ensure_private_dir(&root)?;
                crate::state::ensure_private_dir(&d)?;
                crate::state::atomic_write_in_place(&p, &bytes)
            })
            .await
            .map_err(task_err)?
            .map_err(|source| OpError::Store(StoreError::Io { path: dest.clone(), source }))?;
            photo = Some(dest.to_string_lossy().into_owned());
        }
        let (h, key, p) = (handle.to_string(), share.record_key.clone(), photo.clone());
        let (n, f, l) = (fetched.name.clone(), fetched.first.clone(), fetched.last.clone());
        let at = now_ms();
        let stored = store.call(move |s| s.set_profile_fetched(&h, &key, (&n, &f, &l), p.as_deref(), at)).await?;
        let Some((profile, replaced)) = stored else {
            // The share changed while we fetched: this photo is nobody's.
            if let Some(p) = photo {
                let _ = tokio::fs::remove_file(p).await;
            }
            return Ok(());
        };
        self.remove_files(replaced).await;
        log::info!("{handle}: shared profile fetched (name: {}, photo: {})", if fetched.name.trim().is_empty() { "no" } else { "yes" }, fetched.image.as_ref().map_or("none".to_string(), |i| format!("{} bytes", i.len())));
        self.announce_profile(handle, profile).await;
        Ok(())
    }

    /// `GetChat`: refresh participants' profiles that are old, never
    /// fetched or whose photo file went missing, and retry a group photo
    /// download; all in the background.
    pub(super) async fn refresh_chat_extras(self: &Arc<Self>, chat: &Chat) {
        let Ok(store) = self.store() else { return };
        let handles: Vec<String> = chat.participants.iter().map(|h| h.uri.clone()).collect();
        let older_than = now_ms().saturating_sub(PROFILE_MAX_AGE.as_millis() as u64);
        let mut due = store.call(move |s| s.stale_profiles(&handles, older_than)).await.unwrap_or_default();
        for p in &chat.participants {
            if let Some(photo) = p.profile.as_ref().and_then(|p| p.photo.as_deref())
                && !tokio::fs::metadata(photo).await.is_ok_and(|m| m.is_file())
                && !due.contains(&p.uri)
            {
                due.push(p.uri.clone());
            }
        }
        for h in due {
            if !self.failed_recently(&h) {
                self.spawn_profile_fetch(h);
            }
        }
        let icon_present = match &chat.icon_path {
            Some(p) => tokio::fs::metadata(p).await.is_ok_and(|m| m.is_file()),
            None => false,
        };
        if chat.is_group && !icon_present {
            self.spawn_icon_fetch(chat.id.clone());
        }
    }

    /// Download the group's current photo (if any and not in flight).
    pub fn spawn_icon_fetch(self: &Arc<Self>, chat_id: ChatId) {
        let Some(in_flight) = InFlight::claim(&self.profile_fetches.icons, chat_id.clone()) else { return };
        let hub = self.clone();
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let k = chat_id.clone();
            let dl = match hub.store() {
                Ok(s) => s.call(move |s| s.chat_icon_locator(&k)).await,
                Err(_) => return,
            };
            let dl = match dl {
                Ok(Some(dl)) => dl,
                Ok(None) => return,
                Err(e) => return log::warn!("chat {chat_id}: group photo: {e}"),
            };
            match hub.fetch_icon(&chat_id, dl).await {
                Ok(Some(chat)) => hub.emit(DaemonSignal::ChatUpdated(chat)).await,
                Ok(None) => {}
                Err(e) => log::warn!("chat {chat_id}: group photo download failed: {e}"),
            }
        });
    }

    async fn fetch_icon(&self, chat_id: &ChatId, dl: IconDownload) -> OpResult<Option<Chat>> {
        if dl.size > MAX_ICON_BYTES {
            log::warn!("chat {chat_id}: group photo announces {} bytes (limit {MAX_ICON_BYTES}); not downloading", dl.size);
            return Ok(None);
        }
        let root = self.chat_icons_dir();
        let dir = root.join(safe_component(&chat_id.0, "chat"));
        let tmp = dir.join(safe_component(&dl.icon_id, "icon"));
        let d = dir.clone();
        tokio::task::spawn_blocking(move || crate::state::ensure_private_dir(&root).and_then(|_| crate::state::ensure_private_dir(&d)))
            .await
            .map_err(task_err)?
            .map_err(|source| OpError::Store(StoreError::Io { path: dir.clone(), source }))?;
        self.engine.download_attachment(&dl.locator, &tmp, MAX_ICON_BYTES).await?;
        // Name it after what it is.
        let t = tmp.clone();
        let dest = tokio::task::spawn_blocking(move || -> std::io::Result<PathBuf> {
            let mut head = [0u8; 16];
            let n = std::io::Read::read(&mut std::fs::File::open(&t)?, &mut head)?;
            let dest = t.with_extension(image_ext(&head[..n]));
            std::fs::rename(&t, &dest)?;
            Ok(dest)
        })
        .await
        .map_err(task_err)?
        .map_err(|source| OpError::Store(StoreError::Io { path: tmp.clone(), source }))?;
        let path = dest.to_string_lossy().into_owned();
        log::info!("chat {chat_id}: group photo downloaded");
        let (k, id, p) = (chat_id.clone(), dl.icon_id.clone(), path.clone());
        let updated = self.store()?.call(move |s| s.set_icon_file(&k, &id, &p)).await?;
        if updated.is_none() {
            let _ = tokio::fs::remove_file(&dest).await;
        }
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_images() {
        assert_eq!(image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(image_ext(b"\x89PNG\r\n\x1a\n...."), "png");
        assert_eq!(image_ext(b"\0\0\0\x18ftypheic\0\0\0\0"), "heic");
        assert_eq!(image_ext(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
        assert_eq!(image_ext(b"hello"), "img");
    }

    #[test]
    fn record_keys_become_file_names() {
        assert_eq!(record_file_stem("ab/c+d=="), "ab_c-d~~");
        assert_eq!(record_file_stem("../x"), "_x");
        assert_eq!(record_file_stem(""), "profile");
    }
}
