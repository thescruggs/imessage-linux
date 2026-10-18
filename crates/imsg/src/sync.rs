//! `imsg sync-mac [HOST]`: copy contacts, pinned conversations and
//! (optionally) transcript backgrounds from a Mac (see docs/CLI.md).
//!
//! 1. `tools/mac-sync-export.py` (embedded; with `--backgrounds` also
//!    `tools/mac-backgrounds-export.py`) runs on the Mac over one ssh
//!    connection and streams one tar: `sync.json` (what was exported, or
//!    each section's error), `contacts/mac-<uid>.vcf`, `pins.plist`, and
//!    the backgrounds export.
//! 2. The tar is unpacked into `~/.cache/imsg/import/<run>/` (the daemon
//!    can read it, for the backgrounds) and removed afterwards.
//! 3. Contacts: the `mac-*.vcf` files in the KPeople vCard folder are
//!    replaced (only changed files are rewritten; other vCards there are
//!    left alone). Pins: each Mac pin (`pD.pP`) is mapped to the daemon's
//!    chat id ([`map_pin`]) and applied with `SetChatPinned` /
//!    `ReorderPins` ([`plan_pins`]). Backgrounds: as `imsg import-mac
//!    --backgrounds`.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use imessage_common::model::{ChatId, Handle};
use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::backgrounds::{self, BackgroundArgs, Outcome};
use crate::client::{CliError, Client, exit};
use crate::import::{self, ChatRec, Me};

/// The Mac-side exporter, shipped inside the binary.
pub const SYNC_SCRIPT: &str = include_str!("../../../tools/mac-sync-export.py");
/// Under [`imessage_common::paths::imsg_state_dir`]: the chat ids the last
/// sync pinned because the Mac had them.
pub const PINS_STATE_FILE: &str = "mac-pins.json";

#[derive(Debug, Clone, PartialEq)]
pub struct SyncArgs {
    /// `None`: the daemon's `mac_host`.
    pub host: Option<String>,
    pub contacts: bool,
    pub pins: bool,
    pub backgrounds: bool,
    pub dry_run: bool,
    /// Where the vCards go (default: the KPeople folder).
    pub contacts_dir: Option<PathBuf>,
    pub max_total_mb: u64,
}

impl SyncArgs {
    /// (contacts, pins, backgrounds); no section flag = contacts + pins.
    pub fn sections(&self) -> (bool, bool, bool) {
        if self.contacts || self.pins || self.backgrounds { (self.contacts, self.pins, self.backgrounds) } else { (true, true, false) }
    }
}

/// What is fed to `python3 -` on the Mac: the sync script, with the
/// backgrounds exporter's source in front (`_BACKGROUNDS_SRC`) when needed.
/// A JSON string literal is a valid Python string literal.
pub fn remote_script(with_backgrounds: bool) -> String {
    if with_backgrounds {
        format!("_BACKGROUNDS_SRC = {}\n{SYNC_SCRIPT}", serde_json::to_string(backgrounds::EXPORT_SCRIPT).expect("strings serialize"))
    } else {
        SYNC_SCRIPT.to_string()
    }
}

// ---------------------------------------------------------------------------
// sync.json (see tools/mac-sync-export.py)
// ---------------------------------------------------------------------------

/// One section's result on the Mac: counters, or why it failed.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SectionMeta {
    #[serde(default)]
    pub photos: Option<u64>,
    /// `pins.plist`, or `None` when the Mac has no pinning preferences.
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A pin that is not an address, as chat.db has it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PinChatRec {
    pub pin: String,
    #[serde(flatten)]
    pub chat: ChatRec,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SyncMeta {
    #[serde(default)]
    pub contacts: Option<SectionMeta>,
    #[serde(default)]
    pub pins: Option<SectionMeta>,
    #[serde(default)]
    pub pin_chats: Vec<PinChatRec>,
    #[serde(default)]
    pub backgrounds: Option<SectionMeta>,
}

pub fn read_meta(stage: &Path) -> anyhow::Result<SyncMeta> {
    let p = stage.join("sync.json");
    let raw = std::fs::read(&p).map_err(|_| anyhow::anyhow!("the Mac sent no sync.json; see the messages above"))?;
    serde_json::from_slice(&raw).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
}

/// The section's error, or `what` when the Mac did not report it at all.
fn section_error(m: Option<&SectionMeta>, what: &str) -> Option<String> {
    match m {
        None => Some(format!("the Mac did not export {what}")),
        Some(m) => m.error.clone(),
    }
}

// ---------------------------------------------------------------------------
// contacts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContactsOutcome {
    pub dir: String,
    /// vCards the Mac sent.
    pub received: usize,
    pub photos: u64,
    /// New or changed files (would be, in a dry run).
    pub written: usize,
    pub unchanged: usize,
    /// Old `mac-*.vcf` files the Mac no longer has.
    pub removed: usize,
}

/// `mac-<uid>.vcf` as the exporter names them (uid reduced to
/// `[A-Za-z0-9_-]`).
fn exported_name(n: &str) -> bool {
    n.strip_prefix("mac-")
        .and_then(|r| r.strip_suffix(".vcf"))
        .is_some_and(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// Regular files in `dir` whose name passes `keep`, sorted.
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

/// Write `data` to `path` via a temp file in the same directory (0600).
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Replace the `mac-*.vcf` files in `dir` with the ones staged in
/// `staged` (the tar's `contacts/`): new and changed files are written,
/// identical ones left alone (KPeople reloads what changes), old
/// `mac-*.vcf` files the Mac no longer has removed; other vCards in `dir`
/// are never touched. Nothing at all changes when the Mac sent no
/// contacts. A dry run only counts.
pub fn apply_contacts(staged: &Path, dir: &Path, photos: u64, dry_run: bool) -> anyhow::Result<ContactsOutcome> {
    let new = files_named(staged, exported_name).map_err(|e| anyhow::anyhow!("{}: {e}", staged.display()))?;
    if new.is_empty() {
        anyhow::bail!("no contacts received from the Mac; {} left unchanged", dir.display());
    }
    let old = files_named(dir, |n| n.starts_with("mac-") && n.ends_with(".vcf")).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
    let mut out = ContactsOutcome { dir: dir.display().to_string(), received: new.len(), photos, ..Default::default() };
    let mut writes = vec![];
    for n in &new {
        let data = std::fs::read(staged.join(n)).map_err(|e| anyhow::anyhow!("{n}: {e}"))?;
        if old.contains(n) && std::fs::read(dir.join(n)).is_ok_and(|cur| cur == data) {
            out.unchanged += 1;
        } else {
            writes.push((n, data));
        }
    }
    out.written = writes.len();
    let gone: Vec<&String> = old.iter().filter(|n| !new.contains(n)).collect();
    out.removed = gone.len();
    if dry_run {
        return Ok(out);
    }
    if !dir.is_dir() {
        import::private_dir(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
    }
    for (n, data) in writes {
        write_atomic(&dir.join(n), &data).map_err(|e| anyhow::anyhow!("{}: {e}", dir.join(n).display()))?;
    }
    for n in gone {
        std::fs::remove_file(dir.join(n)).map_err(|e| anyhow::anyhow!("{}: {e}", dir.join(n).display()))?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// pins
// ---------------------------------------------------------------------------

/// `pD.pP` of the Mac's `com.apple.messages.pinning` plist (XML or
/// binary): the pinned conversations, in order.
pub fn parse_pins(data: &[u8]) -> anyhow::Result<Vec<String>> {
    let v = plist::Value::from_reader(std::io::Cursor::new(data)).map_err(|e| anyhow::anyhow!("pins.plist: {e}"))?;
    let pins = v.as_dictionary().and_then(|d| d.get("pD")).and_then(|p| p.as_dictionary()).and_then(|p| p.get("pP")).and_then(|p| p.as_array());
    Ok(pins.map(|a| a.iter().filter_map(|x| x.as_string()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default())
}

/// A Messages group id: a UUID (hex digits and dashes, 8-4-4-4-12).
fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5 && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The daemon chat a Mac pin stands for: a phone number or email address
/// is the 1:1 chat (`tel:` / `mailto:`, numbers as the history import
/// maps them: `2055550100` = `tel:+12055550100`); a pin chat.db knows
/// (a group's chat id) maps like the history import; a group GUID is
/// `group:<GUID>`. `Err` says why not.
pub(crate) fn map_pin(pin: &str, pin_chats: &[PinChatRec], me: &Me, our: Option<&str>) -> Result<ChatId, String> {
    let p = pin.trim();
    if Handle::try_normalize(import::strip_tags(p)).is_some() {
        let h = import::peer_handle(p);
        return if Handle::is_valid_uri(&h.uri) { Ok(ChatId(h.uri)) } else { Err("not a usable address".into()) };
    }
    if let Some(rec) = pin_chats.iter().find(|r| r.pin == pin) {
        let chat = import::chat_for(&rec.chat, me, None, our);
        if chat.id.is_valid() {
            return Ok(chat.id);
        }
    }
    if is_guid(p) {
        return Ok(ChatId::group(p));
    }
    Err("not an address or a group id the Mac's chat.db knows".into())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinRow {
    /// As the Mac has it.
    pub pin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    /// `pinned` / `would-pin`, `unknown` (cannot be mapped),
    /// `not-in-daemon` (no such chat here; not created), `duplicate`
    /// (an earlier pin is the same chat).
    pub result: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PinPlan {
    /// Every Mac pin, in the Mac's order.
    pub mac: Vec<PinRow>,
    /// The pins afterwards, in order (the `ReorderPins` argument): the
    /// Mac's, then the local-only ones.
    pub order: Vec<String>,
    /// Pinned here before, kept (not pinned on the Mac by the last sync).
    pub kept_local: Vec<String>,
    /// Pinned by an earlier sync, no longer pinned on the Mac.
    pub unpin: Vec<String>,
    /// The chats the Mac pins (for the state file).
    #[serde(skip)]
    pub mac_ids: Vec<ChatId>,
}

impl PinPlan {
    /// Whether applying it changes anything.
    pub fn changes(&self, current: &[ChatId]) -> bool {
        !self.unpin.is_empty() || self.order.iter().map(String::as_str).ne(current.iter().map(|c| c.as_str()))
    }
}

/// What a sync does to the pins. The Mac's pins come first, in its order;
/// chats pinned here that the Mac does not pin stay pinned after them,
/// unless they are pinned only because an earlier sync copied them from
/// the Mac (`last_mac`) and the Mac has unpinned them since. Pins that
/// map to no chat of the daemon are reported, not created.
pub(crate) fn plan_pins(
    pins: &[String],
    pin_chats: &[PinChatRec],
    me: &Me,
    our: Option<&str>,
    known: &BTreeSet<ChatId>,
    current: &[ChatId],
    last_mac: &[ChatId],
) -> PinPlan {
    let mut plan = PinPlan::default();
    for pin in pins {
        let mut row = PinRow { pin: pin.clone(), chat_id: None, result: String::new(), detail: String::new() };
        match map_pin(pin, pin_chats, me, our) {
            Err(why) => (row.result, row.detail) = ("unknown".into(), why),
            Ok(id) => {
                row.chat_id = Some(id.0.clone());
                if plan.mac_ids.contains(&id) {
                    row.result = "duplicate".into();
                } else if !known.contains(&id) {
                    (row.result, row.detail) = ("not-in-daemon".into(), "no such chat here; not created".into());
                } else {
                    row.result = "pinned".into();
                    plan.mac_ids.push(id);
                }
            }
        }
        plan.mac.push(row);
    }
    for c in current.iter().filter(|c| !plan.mac_ids.contains(c)) {
        if last_mac.contains(c) {
            plan.unpin.push(c.0.clone());
        } else {
            plan.kept_local.push(c.0.clone());
        }
    }
    plan.order = plan.mac_ids.iter().map(|c| c.0.clone()).chain(plan.kept_local.iter().cloned()).collect();
    plan
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PinsState {
    host: String,
    /// Chat ids pinned because the Mac pinned them.
    chats: Vec<String>,
}

fn pins_state_path() -> PathBuf {
    imessage_common::paths::imsg_state_dir().join(PINS_STATE_FILE)
}

fn load_pins_state() -> Vec<ChatId> {
    std::fs::read(pins_state_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<PinsState>(&b).ok())
        .map(|s| s.chats.into_iter().map(ChatId).collect())
        .unwrap_or_default()
}

fn save_pins_state(host: &str, ids: &[ChatId]) -> anyhow::Result<()> {
    let p = pins_state_path();
    if let Some(d) = p.parent() {
        import::private_dir(d).map_err(|e| anyhow::anyhow!("{}: {e}", d.display()))?;
    }
    let s = PinsState { host: host.to_string(), chats: ids.iter().map(|c| c.0.clone()).collect() };
    write_atomic(&p, &serde_json::to_vec_pretty(&s)?).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// running it
// ---------------------------------------------------------------------------

/// A section's result in the `--json` summary.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Section<T> {
    Ok(T),
    Err { error: String },
}

impl<T> Section<T> {
    fn from(r: anyhow::Result<T>) -> Self {
        match r {
            Ok(v) => Section::Ok(v),
            Err(e) => Section::Err { error: format!("{e:#}") },
        }
    }
    fn failed(&self) -> bool {
        matches!(self, Section::Err { .. })
    }
}

#[derive(Debug, Serialize)]
struct BackgroundsOutcome {
    chats: Vec<Outcome>,
}

#[derive(Debug, Serialize)]
struct Summary {
    host: String,
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    contacts: Option<Section<ContactsOutcome>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pins: Option<Section<PinPlan>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backgrounds: Option<Section<BackgroundsOutcome>>,
}

/// The daemon's view: our handles, the sending handle, `mac_host`.
struct DaemonView {
    client: Client,
    my_handles: Vec<String>,
    our: Option<String>,
    mac_host: Option<String>,
}

async fn daemon_view(ctx: &Ctx) -> crate::client::Result<DaemonView> {
    let client = Client::connect(&ctx.bus).await?;
    let st = client.status().await?.value;
    let my_handles: Vec<String> = st.registration.handles.iter().map(|h| h.uri.clone()).chain(st.registration.other_handles.iter().cloned()).collect();
    let our = st.registration.sender.map(|h| h.uri).or_else(|| my_handles.first().cloned());
    Ok(DaemonView { client, my_handles, our, mac_host: st.mac_host })
}

/// `HOST`, else the daemon's `mac_host`; a usage error (with the host the
/// anisette command reaches as a hint) when neither is there.
fn pick_host(arg: Option<String>, mac_host: Option<String>, anisette_host: Option<String>) -> Result<String, CliError> {
    let Some(h) = arg.or(mac_host).map(|h| h.trim().to_string()) else {
        let hint = anisette_host.map(|h| format!(" (your anisette command reaches {h}: `imsg sync-mac {h}`, or mac_host = \"{h}\")")).unwrap_or_default();
        return Err(CliError::usage(format!("no Mac to sync from: give its ssh HOST or set mac_host in the daemon's config.toml{hint}")));
    };
    if !imessage_common::ssh::valid_host(&h) {
        return Err(CliError::usage(format!("invalid host {h:?}")));
    }
    Ok(h)
}

pub async fn run(args: SyncArgs, state_dir: &Path, ctx: &Ctx) -> anyhow::Result<u8> {
    let (want_contacts, want_pins, want_backgrounds) = args.sections();
    // Pins and backgrounds go through the daemon; contacts only need it
    // for `mac_host`.
    let daemon = match daemon_view(ctx).await {
        Ok(d) => Some(d),
        Err(e) if !(want_pins || want_backgrounds) => {
            if args.host.is_none() {
                return Err(e.into());
            }
            None
        }
        Err(e) => return Err(e.into()),
    };
    let anisette_host = crate::local::anisette_command(state_dir).and_then(|c| imessage_common::ssh::host_of_command(&c));
    let host = pick_host(args.host.clone(), daemon.as_ref().and_then(|d| d.mac_host.clone()), anisette_host)?;
    let contacts_dir = args.contacts_dir.clone().unwrap_or_else(imessage_common::paths::kpeople_vcard_dir);

    let run_id = format!("sync-{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), std::process::id());
    let stage = import::cache_dir().join("imsg/import").join(run_id);
    import::private_dir(&stage).map_err(|e| anyhow::anyhow!("{}: {e}", stage.display()))?;
    let what: Vec<&str> = [(want_contacts, "contacts"), (want_pins, "pins"), (want_backgrounds, "backgrounds")].iter().filter(|w| w.0).map(|w| w.1).collect();
    if !ctx.quiet && !ctx.json {
        eprintln!("syncing {} from {host} ...", what.join(", "));
    }
    let mut script_args: Vec<String> = vec![];
    for (on, flag) in [(want_contacts, "--contacts"), (want_pins, "--pins"), (want_backgrounds, "--backgrounds")] {
        if on {
            script_args.push(flag.into());
        }
    }
    if want_backgrounds {
        script_args.extend(["--max-file-mb".into(), backgrounds::MAX_FILE_MB.to_string()]);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let argv = import::ssh_python_argv(&host, home.as_deref(), &script_args);
    let fetched = tokio::task::block_in_place(|| {
        import::fetch_tar(&argv, remote_script(want_backgrounds), &host, args.max_total_mb, &stage).and_then(|_| read_meta(&stage))
    });
    let meta = match fetched {
        Ok(m) => m,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    let result = apply(&args, &host, &stage, &meta, daemon.as_ref(), &contacts_dir, (want_contacts, want_pins, want_backgrounds)).await;
    let _ = std::fs::remove_dir_all(&stage);
    let summary = result?;
    let failed = summary.contacts.as_ref().is_some_and(Section::failed)
        || summary.pins.as_ref().is_some_and(Section::failed)
        || summary.backgrounds.as_ref().is_some_and(|b| b.failed() || matches!(b, Section::Ok(o) if o.chats.iter().any(|c| c.result == "failed")));
    if ctx.json {
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        print!("{}", summary_text(&summary, ctx.quiet));
    }
    Ok(if failed { exit::ERROR } else { exit::OK })
}

async fn apply(
    args: &SyncArgs,
    host: &str,
    stage: &Path,
    meta: &SyncMeta,
    daemon: Option<&DaemonView>,
    contacts_dir: &Path,
    (c, p, b): (bool, bool, bool),
) -> anyhow::Result<Summary> {
    let mut s = Summary { host: host.to_string(), dry_run: args.dry_run, contacts: None, pins: None, backgrounds: None };
    if c {
        s.contacts = Some(Section::from(match section_error(meta.contacts.as_ref(), "contacts") {
            Some(e) => Err(anyhow::anyhow!("{e}")),
            None => {
                let photos = meta.contacts.as_ref().and_then(|m| m.photos).unwrap_or(0);
                apply_contacts(&stage.join("contacts"), contacts_dir, photos, args.dry_run)
            }
        }));
    }
    if p {
        let d = daemon.expect("pins need the daemon");
        s.pins = Some(Section::from(match section_error(meta.pins.as_ref(), "pins") {
            Some(e) => Err(anyhow::anyhow!("{e}")),
            None => apply_pins(args, host, stage, meta, d).await,
        }));
    }
    if b {
        let d = daemon.expect("backgrounds need the daemon");
        s.backgrounds = Some(Section::from(match section_error(meta.backgrounds.as_ref(), "backgrounds") {
            Some(e) => Err(anyhow::anyhow!("{e}")),
            None => {
                async {
                    let recs = backgrounds::read_records(stage)?;
                    let rows = backgrounds::plan(recs, stage, &d.my_handles, d.our.as_deref());
                    let bargs = BackgroundArgs { host: host.to_string(), chats: vec![], create: false, max_total_mb: args.max_total_mb, dry_run: args.dry_run };
                    Ok(BackgroundsOutcome { chats: backgrounds::execute(&bargs, Some(&d.client), &rows).await? })
                }
                .await
            }
        }));
    }
    Ok(s)
}

async fn apply_pins(args: &SyncArgs, host: &str, stage: &Path, meta: &SyncMeta, d: &DaemonView) -> anyhow::Result<PinPlan> {
    let pins = match meta.pins.as_ref().and_then(|m| m.file.as_deref()) {
        Some(f) => {
            let path = stage.join("pins.plist");
            if f != "pins.plist" || !path.symlink_metadata().is_ok_and(|m| m.is_file()) {
                anyhow::bail!("{f}: not in the export");
            }
            parse_pins(&std::fs::read(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?)?
        }
        None => vec![], // no pinning preferences on the Mac: nothing pinned there
    };
    let chats = d.client.all_chats().await?;
    let known: BTreeSet<ChatId> = chats.iter().map(|c| c.id.clone()).collect();
    let mut pinned: Vec<&imessage_common::model::Chat> = chats.iter().filter(|c| c.pinned).collect();
    pinned.sort_by_key(|c| (c.pin_order.unwrap_or(u32::MAX), c.id.0.clone()));
    let current: Vec<ChatId> = pinned.iter().map(|c| c.id.clone()).collect();
    let me = Me::new(&d.my_handles, std::iter::empty());
    let mut plan = plan_pins(&pins, &meta.pin_chats, &me, d.our.as_deref(), &known, &current, &load_pins_state());
    if args.dry_run {
        plan.mac.iter_mut().filter(|r| r.result == "pinned").for_each(|r| r.result = "would-pin".into());
        return Ok(plan);
    }
    if plan.changes(&current) {
        for id in &plan.unpin {
            d.client.set_chat_pinned(id, false, None).await?;
        }
        d.client.reorder_pins(&plan.order).await?;
    }
    save_pins_state(host, &plan.mac_ids)?;
    Ok(plan)
}

/// The human summary.
fn summary_text(s: &Summary, quiet: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if s.dry_run {
        out.push_str("dry run: nothing changed\n");
    }
    let would = |w: &str, done: &str| if s.dry_run { format!("would be {w}") } else { done.to_string() };
    match &s.contacts {
        None => {}
        Some(Section::Err { error }) => {
            let _ = writeln!(out, "contacts     failed: {error}");
        }
        Some(Section::Ok(c)) => {
            let _ = writeln!(
                out,
                "contacts     {} from the Mac ({} with photos): {} {}, {} unchanged, {} {}; {}",
                c.received,
                c.photos,
                c.written,
                would("written", "written"),
                c.unchanged,
                c.removed,
                would("removed", "removed"),
                c.dir
            );
        }
    }
    match &s.pins {
        None => {}
        Some(Section::Err { error }) => {
            let _ = writeln!(out, "pins         failed: {error}");
        }
        Some(Section::Ok(p)) => {
            let n = |r: &str| p.mac.iter().filter(|m| m.result == r).count();
            let applied = n("pinned") + n("would-pin");
            let skipped = n("unknown") + n("not-in-daemon");
            let _ = writeln!(
                out,
                "pins         {} on the Mac: {applied} {}, {skipped} not found; {} local pin(s) kept, {} {}",
                p.mac.len(),
                would("pinned", "pinned"),
                p.kept_local.len(),
                p.unpin.len(),
                would("unpinned", "unpinned (no longer pinned on the Mac)")
            );
            if !quiet {
                for m in &p.mac {
                    let detail = if m.detail.is_empty() { String::new() } else { format!(" ({})", m.detail) };
                    let _ = writeln!(out, "  {}  {}  {}{detail}", m.pin, m.chat_id.as_deref().unwrap_or("-"), m.result);
                }
                for u in &p.unpin {
                    let _ = writeln!(out, "  {u}  {}", would("unpinned", "unpinned"));
                }
            }
        }
    }
    match &s.backgrounds {
        None => {}
        Some(Section::Err { error }) => {
            let _ = writeln!(out, "backgrounds  failed: {error}");
        }
        Some(Section::Ok(b)) => {
            let n = |r: &str| b.chats.iter().filter(|o| o.result == r).count();
            let _ = writeln!(
                out,
                "backgrounds  {} Mac chat(s) with a background: {} imported, {} already there",
                b.chats.len(),
                n("imported") + n("would-import"),
                n("already-set") + n("already-imported")
            );
            if !quiet {
                for line in backgrounds::table(&b.chats).lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pin: &str, rowid: i64, style: i64, ident: &str, group_id: Option<&str>, participants: &[&str]) -> PinChatRec {
        PinChatRec {
            pin: pin.into(),
            chat: ChatRec {
                rowid,
                chat_identifier: ident.into(),
                style,
                display_name: None,
                group_id: group_id.map(Into::into),
                participants: participants.iter().map(|p| p.to_string()).collect(),
            },
        }
    }

    fn ids(v: &[&str]) -> Vec<ChatId> {
        v.iter().map(|s| ChatId(s.to_string())).collect()
    }

    #[test]
    fn sections_default_to_contacts_and_pins() {
        let a = SyncArgs { host: None, contacts: false, pins: false, backgrounds: false, dry_run: false, contacts_dir: None, max_total_mb: 1 };
        assert_eq!(a.sections(), (true, true, false));
        assert_eq!(SyncArgs { backgrounds: true, ..a.clone() }.sections(), (false, false, true));
        assert_eq!(SyncArgs { contacts: true, ..a }.sections(), (true, false, false));
    }

    #[test]
    fn remote_script_embeds_the_backgrounds_exporter() {
        assert_eq!(remote_script(false), SYNC_SCRIPT);
        let s = remote_script(true);
        let first = s.lines().next().unwrap();
        assert!(first.starts_with("_BACKGROUNDS_SRC = \"#!/usr/bin/env python3\\n"), "{first:.60}");
        assert!(s.ends_with(SYNC_SCRIPT));
        let lit: String = serde_json::from_str(first.trim_start_matches("_BACKGROUNDS_SRC = ")).unwrap();
        assert_eq!(lit, backgrounds::EXPORT_SCRIPT);
    }

    #[test]
    fn pins_plist_parses() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>IMPinningShouldTryFetchAgainIfNullKey</key><true/>
  <key>pD</key><dict>
    <key>pP</key><array><string>+12055550100</string><string>bob@example.com</string><integer>3</integer><string> </string></array>
    <key>pR</key><integer>2</integer><key>pV</key><integer>2</integer>
  </dict>
</dict></plist>"#;
        assert_eq!(parse_pins(xml).unwrap(), vec!["+12055550100", "bob@example.com"]);
        assert_eq!(parse_pins(b"<plist version=\"1.0\"><dict/></plist>").unwrap(), Vec::<String>::new());
        assert!(parse_pins(b"not a plist").is_err());
        // binary works too
        let mut d = plist::Dictionary::new();
        let mut pd = plist::Dictionary::new();
        pd.insert("pP".into(), plist::Value::Array(vec![plist::Value::String("a@b.c".into())]));
        d.insert("pD".into(), plist::Value::Dictionary(pd));
        let mut bin = vec![];
        plist::Value::Dictionary(d).to_writer_binary(&mut bin).unwrap();
        assert_eq!(parse_pins(&bin).unwrap(), vec!["a@b.c"]);
    }

    #[test]
    fn pin_mapping() {
        let me = Me::new(&["mailto:me@example.com".into(), "tel:+12055550199".into()], std::iter::empty());
        let our = Some("mailto:me@example.com");
        let chats = [
            rec("chat987654", 2, 43, "chat987654", Some("abcd-0000-1111"), &["bob@example.com", "carol@example.com"]),
            rec("chat111", 3, 43, "chat111", Some("EEEE-1"), &["bob@example.com"]),
        ];
        let m = |p: &str| map_pin(p, &chats, &me, our);
        assert_eq!(m("+12055550100"), Ok(ChatId("tel:+12055550100".into())));
        assert_eq!(m("2055550100"), Ok(ChatId("tel:+12055550100".into())), "national number as the import maps it");
        assert_eq!(m("Bob@Example.com"), Ok(ChatId("mailto:bob@example.com".into())));
        assert_eq!(m("5b0e2c1a-3f4d-4e21-9a55-0c7e1b2d3f40"), Ok(ChatId("group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40".into())));
        assert_eq!(m("chat987654"), Ok(ChatId("group:ABCD-0000-1111".into())), "via chat.db");
        assert_eq!(m("chat111"), Ok(ChatId("mailto:bob@example.com".into())), "a two-person Mac group is the 1:1 chat");
        assert!(m("chat000").is_err());
        assert!(m("garbage!").is_err());
        assert!(m("").is_err());
    }

    #[test]
    fn pin_planning() {
        let me = Me::new(&["mailto:me@example.com".into()], std::iter::empty());
        let known: BTreeSet<ChatId> =
            ids(&["tel:+12055550100", "mailto:bob@example.com", "mailto:carol@example.com", "mailto:dave@example.com", "mailto:erin@example.com"])
                .into_iter()
                .collect();
        let pins: Vec<String> =
            ["mailto:bob@example.com", "+1 (205) 555-0100", "2055550100", "frank@example.com", "x!"].iter().map(|s| s.to_string()).collect();
        // carol: pinned locally; dave: pinned by the last sync, unpinned on the Mac since; bob: pinned here too
        let current = ids(&["mailto:carol@example.com", "mailto:bob@example.com", "mailto:dave@example.com"]);
        let last = ids(&["mailto:dave@example.com", "mailto:bob@example.com"]);
        let p = plan_pins(&pins, &[], &me, None, &known, &current, &last);
        let results: Vec<(&str, &str)> = p.mac.iter().map(|r| (r.pin.as_str(), r.result.as_str())).collect();
        assert_eq!(
            results,
            [
                ("mailto:bob@example.com", "pinned"),
                ("+1 (205) 555-0100", "pinned"),
                ("2055550100", "duplicate"),
                ("frank@example.com", "not-in-daemon"),
                ("x!", "unknown")
            ]
        );
        assert_eq!(p.order, ["mailto:bob@example.com", "tel:+12055550100", "mailto:carol@example.com"]);
        assert_eq!(p.kept_local, ["mailto:carol@example.com"]);
        assert_eq!(p.unpin, ["mailto:dave@example.com"]);
        assert_eq!(p.mac_ids, ids(&["mailto:bob@example.com", "tel:+12055550100"]));
        assert!(p.changes(&current));
        // applying it again changes nothing
        let after = ids(&["mailto:bob@example.com", "tel:+12055550100", "mailto:carol@example.com"]);
        let again = plan_pins(&pins, &[], &me, None, &known, &after, &p.mac_ids);
        assert!(!again.changes(&after));
        assert_eq!(again.order, p.order);
        // the Mac pins nothing: only the synced pins go
        let none = plan_pins(&[], &[], &me, None, &known, &after, &p.mac_ids);
        assert_eq!(
            (none.order.clone(), none.unpin.clone()),
            (vec!["mailto:carol@example.com".to_string()], vec!["mailto:bob@example.com".to_string(), "tel:+12055550100".to_string()])
        );
    }

    #[test]
    fn contacts_replace_only_mac_files() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("stage/contacts");
        let dir = tmp.path().join("kpeoplevcard");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(staged.join("mac-A.vcf"), "A2").unwrap();
        std::fs::write(staged.join("mac-B.vcf"), "B").unwrap();
        std::fs::write(staged.join("mac-C_x.vcf"), "C").unwrap();
        std::fs::write(staged.join("notes.txt"), "ignored").unwrap();
        std::fs::write(staged.join("mac-bad name.vcf"), "ignored").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", staged.join("mac-L.vcf")).unwrap();
        std::fs::write(dir.join("mac-A.vcf"), "A1").unwrap();
        std::fs::write(dir.join("mac-B.vcf"), "B").unwrap();
        std::fs::write(dir.join("mac-old.vcf"), "gone").unwrap();
        std::fs::write(dir.join("kdeconnect-1.vcf"), "phone").unwrap();

        let dry = apply_contacts(&staged, &dir, 1, true).unwrap();
        assert_eq!((dry.received, dry.written, dry.unchanged, dry.removed), (3, 2, 1, 1));
        assert_eq!(std::fs::read_to_string(dir.join("mac-A.vcf")).unwrap(), "A1", "dry run writes nothing");
        assert!(dir.join("mac-old.vcf").exists());

        let b_mtime = std::fs::metadata(dir.join("mac-B.vcf")).unwrap().modified().unwrap();
        let o = apply_contacts(&staged, &dir, 1, false).unwrap();
        assert_eq!(o, ContactsOutcome { dir: dir.display().to_string(), ..dry });
        assert_eq!(std::fs::read_to_string(dir.join("mac-A.vcf")).unwrap(), "A2");
        assert_eq!(std::fs::read_to_string(dir.join("mac-C_x.vcf")).unwrap(), "C");
        assert_eq!(std::fs::metadata(dir.join("mac-B.vcf")).unwrap().modified().unwrap(), b_mtime, "unchanged file left alone");
        assert!(!dir.join("mac-old.vcf").exists());
        assert!(!dir.join("mac-L.vcf").exists(), "symlinks are not followed");
        assert_eq!(std::fs::read_to_string(dir.join("kdeconnect-1.vcf")).unwrap(), "phone", "other vCards untouched");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("mac-A.vcf")).unwrap().permissions().mode() & 0o777, 0o600);

        // nothing received: nothing changes
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(apply_contacts(&empty, &dir, 0, false).unwrap_err().to_string().contains("left unchanged"));
        assert!(apply_contacts(&tmp.path().join("missing"), &dir, 0, false).is_err());
        assert!(dir.join("mac-A.vcf").exists());
        // a new folder is created
        let fresh = tmp.path().join("new/kpeoplevcard");
        assert_eq!(apply_contacts(&staged, &fresh, 0, false).unwrap().written, 3);
        assert_eq!(std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn host_choice() {
        assert_eq!(pick_host(Some("mac.local".into()), Some("other".into()), None).unwrap(), "mac.local");
        assert_eq!(pick_host(None, Some(" bebop.local ".into()), None).unwrap(), "bebop.local");
        let e = pick_host(None, None, Some("bebop.local".into())).unwrap_err();
        assert_eq!(e.code, exit::USAGE);
        assert!(e.message.contains("mac_host = \"bebop.local\""), "{}", e.message);
        assert!(pick_host(Some("-oProxyCommand=sh".into()), None, None).is_err());
    }

    #[test]
    fn summary_wording() {
        let s = Summary {
            host: "mac".into(),
            dry_run: false,
            contacts: Some(Section::Ok(ContactsOutcome { dir: "/d".into(), received: 3, photos: 1, written: 2, unchanged: 1, removed: 0 })),
            pins: Some(Section::Err { error: "no Full Disk Access".into() }),
            backgrounds: None,
        };
        let t = summary_text(&s, false);
        assert!(t.contains("contacts     3 from the Mac (1 with photos): 2 written, 1 unchanged, 0 removed; /d"), "{t}");
        assert!(t.contains("pins         failed: no Full Disk Access"), "{t}");
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["contacts"]["written"], 2);
        assert_eq!(v["pins"]["error"], "no Full Disk Access");
        assert!(v.get("backgrounds").is_none());
    }

    /// The exporter's Python tests (tools/test_mac_sync_export.py);
    /// skipped when python3 is not installed.
    #[test]
    fn exporter_python_tests() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mac-sync-export.py");
        std::fs::write(&script, SYNC_SCRIPT).unwrap();
        std::fs::write(dir.path().join("mac-backgrounds-export.py"), backgrounds::EXPORT_SCRIPT).unwrap();
        let test = dir.path().join("test_mac_sync_export.py");
        std::fs::write(&test, include_str!("../../../tools/test_mac_sync_export.py")).unwrap();
        let out = match std::process::Command::new("python3").args(["-I", "-B"]).arg(&test).arg(&script).output() {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return eprintln!("python3 not found; skipping the exporter tests"),
            Err(e) => panic!("cannot run python3: {e}"),
        };
        let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{log}");
    }
}
