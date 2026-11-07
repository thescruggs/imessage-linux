//! The iCloud contacts sync: the address book comes from iCloud over
//! CardDAV (`crate::carddav`) with an app-specific password, no Mac needed.
//!
//! Each card is one `icloud-<hash>.vcf` in the vCard folder
//! (`contacts_dir`, the KPeople folder kmessages and the contact links
//! read), written without vCard property groups (Apple's custom labels)
//! because KPeople's reader (KContacts) ignores grouped properties; other
//! vCards there are left alone, except the `mac-*.vcf` cards an earlier
//! `imsg sync-mac` copied from the Mac, which iCloud replaces.
//! The account (Apple ID + app-specific password) is sealed with the state
//! key in `$STATE/contacts-account.enc`; what was synced (ETags, files) is
//! in `$STATE/contacts-index.json`, so later syncs fetch only what changed.
//! Syncs run every `contacts_sync_minutes` and on request
//! (`SyncContacts`), one at a time.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use imessage_common::model::{ContactsSyncReport, ContactsSyncStatus};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use zeroize::Zeroizing;

use super::{DaemonSignal, Hub, OpError, OpResult};
use crate::carddav::{self, CardDavError, Client, Credentials};
use crate::engine::EngineError;

pub const ACCOUNT_FILE: &str = "contacts-account.enc";
pub const INDEX_FILE: &str = "contacts-index.json";
/// The first automatic sync waits this long after startup.
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// After a failed automatic sync (not a refused password), try again
/// after this long (or the interval, if shorter).
const RETRY: Duration = Duration::from_secs(15 * 60);

/// The sync's settings and state (part of [`Hub`]).
pub(super) struct ContactsSync {
    /// `None`: not configured (a bare hub); every operation says so.
    dir: std::sync::Mutex<Option<PathBuf>>,
    interval: std::sync::Mutex<Duration>,
    base: std::sync::Mutex<String>,
    status: std::sync::Mutex<ContactsSyncStatus>,
    creds: std::sync::Mutex<Option<Credentials>>,
    /// When the next automatic sync is due; `None` = none planned.
    next: std::sync::Mutex<Option<Instant>>,
    /// Held while a sync (or an account change) runs.
    run: tokio::sync::Mutex<()>,
    wake: tokio::sync::Notify,
}

impl Default for ContactsSync {
    fn default() -> Self {
        ContactsSync {
            dir: Default::default(),
            interval: Default::default(),
            base: std::sync::Mutex::new(carddav::ICLOUD_CONTACTS.to_string()),
            status: Default::default(),
            creds: Default::default(),
            next: Default::default(),
            run: Default::default(),
            wake: Default::default(),
        }
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The sealed account file's contents.
#[derive(Serialize, Deserialize)]
struct SavedAccount {
    apple_id: String,
    password: Zeroizing<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct IndexEntry {
    etag: String,
    /// The vCard file; `None` for a group card (not written).
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    photo: bool,
}

/// What the last sync left, by card key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Index {
    account: String,
    dir: String,
    #[serde(default)]
    cards: BTreeMap<String, IndexEntry>,
    #[serde(default)]
    last_sync: Option<u64>,
    #[serde(default)]
    last: Option<ContactsSyncReport>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn op_error(e: CardDavError) -> OpError {
    let msg = e.to_string();
    OpError::Engine(match e {
        CardDavError::Auth => EngineError::Login(msg),
        CardDavError::Transient(_) => EngineError::Transient(msg),
        CardDavError::Protocol(_) => EngineError::Protocol(msg),
    })
}

/// Regular files in `dir` whose name passes `keep`.
fn files_named(dir: &Path, keep: impl Fn(&str) -> bool) -> std::io::Result<Vec<String>> {
    let mut out = vec![];
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for e in rd {
        let e = e?;
        let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
        if keep(&name) && e.path().symlink_metadata().is_ok_and(|m| m.is_file()) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// `mac-<uid>.vcf` as `imsg sync-mac` named them.
fn is_mac_card(n: &str) -> bool {
    n.strip_prefix("mac-")
        .and_then(|r| r.strip_suffix(".vcf"))
        .is_some_and(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// One sync into `dir` against what `prev` recorded. Nothing is removed
/// unless the whole address book was listed: a card that is listed but
/// missing from the multiget reply keeps its old file (and is asked for
/// again next time), and an empty address book is treated as a failure (it
/// is far likelier a server hiccup than a wiped iCloud) and changes
/// nothing. A card whose linked photo could not be fetched is written
/// without it and fetched again next time. The `mac-*.vcf` copies of an
/// earlier `imsg sync-mac` are removed by the first sync only.
async fn sync_once(client: &Client, apple_id: &str, dir: &Path, prev: &Index) -> Result<(ContactsSyncReport, Index), CardDavError> {
    let io = |what: &str, p: &Path, e: std::io::Error| {
        // The systemd unit mounts the home directory read-only and must
        // list the vCard folder in ReadWritePaths= (packaging/systemd).
        let hint = if e.kind() == std::io::ErrorKind::ReadOnlyFilesystem || e.raw_os_error() == Some(30) {
            " (the daemon's systemd unit must allow writing this folder: ReadWritePaths=, see docs/DAEMON.md \"iCloud contacts\")"
        } else {
            ""
        };
        CardDavError::Protocol(format!("{what} {}: {e}{hint}", p.display()))
    };
    let dir_s = dir.display().to_string();
    // Entries recorded for another account or folder say nothing here.
    let known: &BTreeMap<String, IndexEntry> = if prev.account == apple_id && prev.dir == dir_s { &prev.cards } else { &BTreeMap::new() };
    let books = client.discover().await?;
    let mut listed: Vec<(usize, carddav::Entry)> = vec![];
    let mut seen = HashSet::new();
    for (i, b) in books.iter().enumerate() {
        for e in client.list(b).await? {
            if seen.insert(e.key.clone()) {
                listed.push((i, e));
            }
        }
    }
    if listed.is_empty() {
        return Err(CardDavError::Protocol(format!("iCloud listed no contacts at all; {dir_s} left unchanged")));
    }
    if !dir.is_dir() {
        imessage_common::platform::fs::create_private_dir_all(dir).map_err(|e| io("cannot create", dir, e))?;
    }
    let mut index = Index { account: apple_id.to_string(), dir: dir_s.clone(), ..Default::default() };
    let mut report = ContactsSyncReport { dir: dir_s.clone(), ..Default::default() };
    let mut fetch: Vec<Vec<&carddav::Entry>> = vec![vec![]; books.len()];
    for (i, e) in &listed {
        match known.get(&e.key) {
            Some(k) if k.etag == e.etag && k.file.as_ref().is_none_or(|f| dir.join(f).is_file()) => {
                index.cards.insert(e.key.clone(), k.clone());
                if k.file.is_some() {
                    report.unchanged += 1;
                }
            }
            _ => fetch[*i].push(e),
        }
    }
    for (i, want) in fetch.iter().enumerate() {
        for batch in want.chunks(carddav::BATCH) {
            let cards = client.multiget(&books[i], batch).await?;
            for e in batch.iter().filter(|e| !cards.iter().any(|c| c.key == e.key)) {
                if let Some(k) = known.get(&e.key) {
                    log::debug!("a changed contact card was not returned; keeping the old one");
                    index.cards.insert(e.key.clone(), k.clone());
                    if k.file.is_some() {
                        report.unchanged += 1;
                    }
                }
            }
            for card in cards {
                if carddav::is_group(&card.vcard) {
                    index.cards.insert(card.key, IndexEntry { etag: card.etag, file: None, photo: false });
                    continue;
                }
                // Written without property groups, which KPeople's reader drops.
                let mut vcard = carddav::ungroup(&card.vcard);
                // An empty ETag: fetched again next time.
                let mut etag = card.etag;
                if let Some(url) = carddav::photo_url(&vcard) {
                    let photo = match client.photo(&url).await {
                        Ok((bytes, ctype)) => carddav::image_kind(&bytes, ctype.as_deref()).map(|k| (bytes, k)),
                        Err(e) => {
                            log::debug!("contact photo not fetched: {e}");
                            None
                        }
                    };
                    if photo.is_none() {
                        etag.clear();
                    }
                    vcard = carddav::embed_photo(&vcard, photo.as_ref().map(|(b, k)| (b.as_slice(), *k)));
                }
                let name = carddav::file_name(&card.key);
                let path = dir.join(&name);
                if std::fs::read(&path).is_ok_and(|cur| cur == vcard.as_bytes()) {
                    report.unchanged += 1;
                } else {
                    crate::state::atomic_write_in_place(&path, vcard.as_bytes()).map_err(|e| io("cannot write", &path, e))?;
                    report.written += 1;
                }
                index.cards.insert(card.key, IndexEntry { etag, file: Some(name), photo: carddav::has_photo(&vcard) });
            }
        }
    }
    report.cards = index.cards.values().filter(|e| e.file.is_some()).count() as u32;
    report.photos = index.cards.values().filter(|e| e.file.is_some() && e.photo).count() as u32;
    let keep: HashSet<&str> = index.cards.values().filter_map(|e| e.file.as_deref()).collect();
    for n in files_named(dir, carddav::is_synced_name).map_err(|e| io("cannot read", dir, e))? {
        if !keep.contains(n.as_str()) {
            std::fs::remove_file(dir.join(&n)).map_err(|e| io("cannot remove", &dir.join(&n), e))?;
            report.removed += 1;
        }
    }
    if report.cards > 0 && known.is_empty() {
        for n in files_named(dir, is_mac_card).map_err(|e| io("cannot read", dir, e))? {
            std::fs::remove_file(dir.join(&n)).map_err(|e| io("cannot remove", &dir.join(&n), e))?;
            report.removed_mac += 1;
        }
    }
    index.last_sync = Some(now_ms());
    index.last = Some(report.clone());
    Ok((report, index))
}

impl Hub {
    /// Sync iCloud contacts into `dir` every `interval_minutes` (0 = only
    /// on request), starting discovery at `base` (`None`: iCloud).
    pub fn configure_contacts_sync(&self, dir: PathBuf, interval_minutes: u32, base: Option<String>) {
        let cs = &self.contacts_sync;
        *lock(&cs.dir) = Some(dir);
        *lock(&cs.interval) = Duration::from_secs(u64::from(interval_minutes) * 60);
        if let Some(b) = base {
            *lock(&cs.base) = b;
        }
        lock(&cs.status).interval_minutes = interval_minutes;
    }

    fn contacts_dir_configured(&self) -> OpResult<PathBuf> {
        lock(&self.contacts_sync.dir).clone().ok_or_else(|| OpError::InvalidInput("the iCloud contacts sync is not available in this daemon".into()))
    }

    fn account_path(&self) -> PathBuf {
        self.state_dir.join(ACCOUNT_FILE)
    }

    fn index_path(&self) -> PathBuf {
        self.state_dir.join(INDEX_FILE)
    }

    fn load_index(&self) -> Index {
        std::fs::read(self.index_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn contacts_sync_status(&self) -> ContactsSyncStatus {
        lock(&self.contacts_sync.status).clone()
    }

    async fn contacts_sync_changed(&self) {
        self.emit(DaemonSignal::ContactsSyncChanged(self.contacts_sync_status())).await;
    }

    fn plan_next(&self, after: Option<Duration>) {
        *lock(&self.contacts_sync.next) = after.map(|d| Instant::now() + d);
        self.contacts_sync.wake.notify_one();
    }

    fn interval(&self) -> Duration {
        *lock(&self.contacts_sync.interval)
    }

    /// Read the saved account and what the last sync did (startup).
    pub async fn load_contacts_sync(&self) {
        if lock(&self.contacts_sync.dir).is_none() {
            return;
        }
        let index = self.load_index();
        {
            let mut st = lock(&self.contacts_sync.status);
            st.last_sync = index.last_sync;
            st.last = index.last.clone();
        }
        let sealed = match std::fs::read(self.account_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                log::warn!("cannot read {}: {e}", self.account_path().display());
                return;
            }
        };
        let Ok(store) = self.store() else { return };
        let opened = store.call(move |s| Ok(s.open_secret(&sealed).map(Zeroizing::new))).await.ok().flatten();
        let Some(acct) = opened.and_then(|b| serde_json::from_slice::<SavedAccount>(&b).ok()) else {
            log::warn!("{} cannot be opened with the state key; set the iCloud contacts account again", self.account_path().display());
            lock(&self.contacts_sync.status).last_error = Some("the saved iCloud contacts account cannot be read (was the state key changed?); set it again".into());
            return;
        };
        let SavedAccount { apple_id, password } = acct;
        lock(&self.contacts_sync.status).account = Some(apple_id.clone());
        *lock(&self.contacts_sync.creds) = Some(Credentials { apple_id, password });
        let interval = self.interval();
        if !interval.is_zero() {
            let age = index.last_sync.map(|t| Duration::from_millis(now_ms().saturating_sub(t)));
            let wait = match age {
                Some(a) if a < interval => (interval - a).max(STARTUP_DELAY),
                _ => STARTUP_DELAY,
            };
            self.plan_next(Some(wait));
        }
    }

    /// Run one sync with `creds` (the caller holds `run`).
    async fn run_contacts_sync(self: &Arc<Self>, creds: &Credentials) -> Result<ContactsSyncReport, CardDavError> {
        let dir = self.contacts_dir_configured().map_err(|e| CardDavError::Protocol(e.to_string()))?;
        lock(&self.contacts_sync.status).syncing = true;
        self.contacts_sync_changed().await;
        let base = lock(&self.contacts_sync.base).clone();
        let prev = self.load_index();
        let result = match Client::new(creds.clone(), &base) {
            Ok(client) => sync_once(&client, &creds.apple_id, &dir, &prev).await,
            Err(e) => Err(e),
        };
        let changed = match &result {
            Ok((report, index)) => {
                if let Ok(json) = serde_json::to_vec_pretty(index)
                    && let Err(e) = crate::state::atomic_write(&self.index_path(), &json)
                {
                    log::warn!("cannot save {}: {e}", self.index_path().display());
                }
                log::info!(
                    "iCloud contacts: {} cards ({} with photos): {} written, {} unchanged, {} removed{}",
                    report.cards,
                    report.photos,
                    report.written,
                    report.unchanged,
                    report.removed,
                    if report.removed_mac > 0 { format!(", {} Mac cards replaced", report.removed_mac) } else { String::new() }
                );
                let mut st = lock(&self.contacts_sync.status);
                st.last_sync = index.last_sync;
                st.last = Some(report.clone());
                st.last_error = None;
                st.auth_failed = false;
                report.written + report.removed + report.removed_mac > 0
            }
            Err(e) => {
                log::warn!("iCloud contacts sync failed: {e}");
                let mut st = lock(&self.contacts_sync.status);
                st.last_error = Some(e.to_string());
                st.auth_failed = matches!(e, CardDavError::Auth);
                false
            }
        };
        lock(&self.contacts_sync.status).syncing = false;
        let interval = self.interval();
        self.plan_next(match &result {
            _ if interval.is_zero() => None,
            Ok(_) => Some(interval),
            Err(CardDavError::Auth) => None,
            Err(_) => Some(RETRY.min(interval)),
        });
        self.contacts_sync_changed().await;
        if changed {
            self.refresh_contact_links().await;
        }
        result.map(|(r, _)| r)
    }

    /// Sync now. Waits for a sync already running, then runs another.
    pub async fn sync_contacts(self: &Arc<Self>) -> OpResult<ContactsSyncStatus> {
        self.contacts_dir_configured()?;
        let _g = self.contacts_sync.run.lock().await;
        let creds = lock(&self.contacts_sync.creds).clone().ok_or_else(|| {
            OpError::NotFound("no iCloud contacts account is set up (imsg contacts login, or SetContactsAccount)".into())
        })?;
        self.run_contacts_sync(&creds).await.map_err(op_error)?;
        Ok(self.contacts_sync_status())
    }

    /// Check `apple_id` / `password` (an app-specific password) against
    /// iCloud, save them and sync. Nothing is saved when iCloud refuses
    /// them or cannot be reached. A sync failing after that is reported in
    /// the returned status (`last_error`), the account stays.
    pub async fn set_contacts_account(self: &Arc<Self>, apple_id: &str, password: &str) -> OpResult<ContactsSyncStatus> {
        self.contacts_dir_configured()?;
        let apple_id = apple_id.trim().to_string();
        let password = Zeroizing::new(password.trim().to_string());
        if apple_id.is_empty() || password.is_empty() {
            return Err(OpError::InvalidInput("an Apple ID and an app-specific password are needed".into()));
        }
        let store = self.store()?.clone();
        let creds = Credentials { apple_id: apple_id.clone(), password };
        let _g = self.contacts_sync.run.lock().await;
        let base = lock(&self.contacts_sync.base).clone();
        Client::new(creds.clone(), &base).map_err(op_error)?.discover().await.map_err(op_error)?;
        let plain = Zeroizing::new(serde_json::to_vec(&SavedAccount { apple_id: apple_id.clone(), password: creds.password.clone() }).expect("serializes"));
        let sealed = store.call(move |s| Ok(s.seal_secret(&plain))).await?;
        crate::state::atomic_write(&self.account_path(), &sealed).map_err(|e| OpError::InvalidInput(format!("cannot save {}: {e}", self.account_path().display())))?;
        {
            let mut st = lock(&self.contacts_sync.status);
            st.account = Some(apple_id.clone());
            st.auth_failed = false;
            st.last_error = None;
        }
        *lock(&self.contacts_sync.creds) = Some(creds.clone());
        log::info!("iCloud contacts account set");
        let _ = self.run_contacts_sync(&creds).await;
        Ok(self.contacts_sync_status())
    }

    /// Forget the account (and with `remove_cards` the synced vCards and
    /// the record of them).
    pub async fn clear_contacts_account(self: &Arc<Self>, remove_cards: bool) -> OpResult<ContactsSyncStatus> {
        let dir = self.contacts_dir_configured()?;
        let _g = self.contacts_sync.run.lock().await;
        match std::fs::remove_file(self.account_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(OpError::InvalidInput(format!("cannot remove {}: {e}", self.account_path().display())));
            }
            _ => {}
        }
        *lock(&self.contacts_sync.creds) = None;
        let mut removed = 0;
        if remove_cards {
            for n in files_named(&dir, carddav::is_synced_name).unwrap_or_default() {
                if std::fs::remove_file(dir.join(&n)).is_ok() {
                    removed += 1;
                }
            }
            let _ = std::fs::remove_file(self.index_path());
        }
        {
            let mut st = lock(&self.contacts_sync.status);
            st.account = None;
            st.auth_failed = false;
            st.last_error = None;
            if remove_cards {
                st.last_sync = None;
                st.last = None;
            }
        }
        self.plan_next(None);
        log::info!("iCloud contacts account removed{}", if remove_cards { format!(" with {removed} cards") } else { String::new() });
        self.contacts_sync_changed().await;
        if removed > 0 {
            self.refresh_contact_links().await;
        }
        Ok(self.contacts_sync_status())
    }

    /// Automatic syncs (see [`Hub::configure_contacts_sync`]) until the hub
    /// stops.
    pub fn spawn_contacts_sync(self: &Arc<Self>) {
        let hub = Arc::downgrade(self);
        let mut stop = self.stop.subscribe();
        tokio::spawn(async move {
            loop {
                let (next, wake) = {
                    let Some(h) = hub.upgrade() else { return };
                    let next = *lock(&h.contacts_sync.next);
                    // A wake between reading `next` and waiting is not lost:
                    // `plan_next` uses `notify_one`, which keeps a permit.
                    let h2 = h.clone();
                    (next, async move { h2.contacts_sync.wake.notified().await })
                };
                let due = async {
                    match next {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = due => {}
                    _ = wake => continue,
                    _ = stop.wait_for(|s| *s) => return,
                }
                let Some(h) = hub.upgrade() else { return };
                let _g = h.contacts_sync.run.lock().await;
                // Re-check under the lock: a manual sync may have just run.
                if lock(&h.contacts_sync.next).is_none_or(|at| at > Instant::now()) {
                    continue;
                }
                let creds = lock(&h.contacts_sync.creds).clone();
                match creds {
                    Some(c) => {
                        let _ = h.run_contacts_sync(&c).await;
                    }
                    None => *lock(&h.contacts_sync.next) = None,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_card_names() {
        assert!(is_mac_card("mac-AAAA-1111_ABPerson.vcf"));
        assert!(!is_mac_card("mac-.vcf") && !is_mac_card("mac-a b.vcf") && !is_mac_card("kdeconnect-1.vcf"));
    }
}
