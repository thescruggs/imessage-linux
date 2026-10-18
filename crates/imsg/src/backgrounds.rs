//! `imsg import-mac HOST --backgrounds`: copy the chats' transcript
//! backgrounds from a Mac into the daemon (see docs/CLI.md).
//!
//! 1. `tools/mac-backgrounds-export.py` (embedded) runs on the Mac over one
//!    ssh connection: for every chat whose `chat.db` `properties` plist has
//!    `backgroundProperties` it reads only the background id / version /
//!    size (never the MMCS key, signature or URL next to them) and streams
//!    a tar: `backgrounds.jsonl` + the cached payloads from
//!    `~/Library/Messages/TranscriptBackgroundCache/`.
//! 2. The tar is unpacked into `~/.cache/imsg/import/<run>/` (the daemon
//!    can read it; files under `backgrounds/`).
//! 3. Each Mac chat is mapped to the daemon's chat id with the same rules
//!    as the history import ([`import::chat_for`], pure: [`plan`]), and
//!    each file is handed to `ImportChatBackground`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use imessage_common::import::ImportBatch;
use imessage_common::model::{Chat, ChatId, Handle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Ctx;
use crate::client::{CliError, Client, exit};
use crate::import::{self, ChatRec, Me};
use crate::resolve::{self, ChatArg};

/// The Mac-side exporter, shipped inside the binary.
pub const EXPORT_SCRIPT: &str = include_str!("../../../tools/mac-backgrounds-export.py");
/// The daemon refuses larger background files (`MAX_BACKGROUND_BYTES`).
pub const MAX_FILE_MB: u64 = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundArgs {
    pub host: String,
    /// Only these chats (ids, handles or names); empty = all.
    pub chats: Vec<String>,
    /// Create chats the daemon does not have yet.
    pub create: bool,
    pub max_total_mb: u64,
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// backgrounds.jsonl records (see tools/mac-backgrounds-export.py)
// ---------------------------------------------------------------------------

/// The non-secret `backgroundProperties` fields.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BgFields {
    /// `trabaid`; `None` when missing or not usable as a file name.
    #[serde(default)]
    pub id: Option<String>,
    /// `trabapv`
    #[serde(default)]
    pub version: Option<u32>,
    /// `trabafs`
    #[serde(default)]
    pub size: Option<u64>,
    /// `refreshDate`, Unix seconds.
    #[serde(default)]
    pub refreshed: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BgChatRec {
    #[serde(flatten)]
    pub chat: ChatRec,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub background: BgFields,
    /// Tar path of the PosterKit payload, if cached on the Mac.
    #[serde(default)]
    pub poster: Option<String>,
    /// Tar path of the `-watchBackground` plist, if cached.
    #[serde(default)]
    pub watch: Option<String>,
    /// Why the poster is not there: `not_cached`, `too_large`, `bad_id`.
    #[serde(default)]
    pub missing: Option<String>,
}

pub fn read_records(stage: &Path) -> anyhow::Result<Vec<BgChatRec>> {
    let path = stage.join("backgrounds.jsonl");
    let f = std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mut out = vec![];
    for (i, line) in std::io::BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| anyhow::anyhow!("backgrounds.jsonl line {}: {e}", i + 1))?;
        if v.get("type").and_then(|t| t.as_str()) == Some("chat") {
            out.push(serde_json::from_value(v).map_err(|e| anyhow::anyhow!("backgrounds.jsonl line {}: {e}", i + 1))?);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// planning (pure)
// ---------------------------------------------------------------------------

/// Why a Mac chat's background cannot be imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// The chat has no valid iMessage address (contact name, `urn:biz:`).
    NoAddress,
    /// The background id is missing or not a plain id.
    BadId,
    /// Neither the payload nor its watch rendering is in the Mac's cache.
    NotCached,
    /// Over the daemon's 32 MiB limit.
    TooLarge,
    /// Another Mac chat with the same daemon chat has a newer background.
    Superseded { by_rowid: i64 },
}

impl Skip {
    pub fn code(&self) -> &'static str {
        match self {
            Skip::NoAddress => "no-address",
            Skip::BadId => "bad-id",
            Skip::NotCached => "not-cached",
            Skip::TooLarge => "too-large",
            Skip::Superseded { .. } => "superseded",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Skip::NoAddress => "the Mac chat has no valid iMessage address".into(),
            Skip::BadId => "unusable background id on the Mac".into(),
            Skip::NotCached => "the background file is not in the Mac's TranscriptBackgroundCache".into(),
            Skip::TooLarge => format!("the background file is larger than {MAX_FILE_MB} MB"),
            Skip::Superseded { by_rowid } => format!("Mac chat #{by_rowid} maps to the same chat and has a newer background"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub rowid: i64,
    /// `#<rowid> <chat_identifier>` plus the service / name.
    pub mac: String,
    /// The daemon chat it maps to (also for skipped rows, when valid).
    pub chat: Chat,
    pub background_id: Option<String>,
    /// Payload version and announced (MMCS) size on the Mac.
    pub version: Option<u32>,
    pub size: Option<u64>,
    pub refreshed: Option<i64>,
    /// Staged payload / watch rendering (validated paths inside the stage).
    pub poster: Option<PathBuf>,
    pub watch: Option<PathBuf>,
    pub skip: Option<Skip>,
}

fn mac_label(r: &BgChatRec) -> String {
    let mut s = format!("#{} {}", r.chat.rowid, r.chat.chat_identifier);
    let mut extra = vec![];
    if let Some(sv) = r.service.as_deref().filter(|s| !s.is_empty()) {
        extra.push(sv.to_string());
    }
    if let Some(n) = r.chat.display_name.as_deref().filter(|n| !n.trim().is_empty()) {
        extra.push(format!("\"{n}\""));
    }
    if !extra.is_empty() {
        s.push_str(&format!(" ({})", extra.join(", ")));
    }
    s
}

/// Map every exported Mac chat to the daemon's chat (as [`import::convert`]
/// does: 1:1 = the other party, a group = `group:<GROUP_ID>`, a two-person
/// Mac "group" = the 1:1, our own handles = note to self) and decide what
/// to import. When several Mac chats map to one daemon chat (an SMS and an
/// iMessage row for the same number), the newest background wins (by
/// `refreshDate`, then row id). Sorted by daemon chat id, then Mac row.
pub fn plan(recs: Vec<BgChatRec>, stage: &Path, my_handles: &[String], our: Option<&str>) -> Vec<PlanRow> {
    let me = Me::new(my_handles, std::iter::empty());
    let our = our.map(Handle::normalize);
    let mut rows: Vec<PlanRow> = recs
        .iter()
        .map(|r| {
            let chat = import::chat_for(&r.chat, &me, None, our.as_deref());
            let poster = r.poster.as_deref().and_then(|p| import::staged_file_under(stage, "backgrounds", p));
            let watch = r.watch.as_deref().and_then(|p| import::staged_file_under(stage, "backgrounds", p));
            let valid = chat.id.is_valid() && chat.participants.iter().all(|h| Handle::is_valid_uri(&h.uri));
            let skip = if !valid {
                Some(Skip::NoAddress)
            } else if r.background.id.is_none() || r.missing.as_deref() == Some("bad_id") {
                Some(Skip::BadId)
            } else if poster.is_none() && watch.is_none() {
                Some(if r.missing.as_deref() == Some("too_large") { Skip::TooLarge } else { Skip::NotCached })
            } else {
                None
            };
            PlanRow {
                rowid: r.chat.rowid,
                mac: mac_label(r),
                chat,
                background_id: r.background.id.clone(),
                version: r.background.version,
                size: r.background.size,
                refreshed: r.background.refreshed,
                poster,
                watch,
                skip,
            }
        })
        .collect();
    // newest usable background per daemon chat
    let mut best: BTreeMap<ChatId, (Option<i64>, i64)> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.skip.is_none()) {
        let k = (r.refreshed, r.rowid);
        best.entry(r.chat.id.clone()).and_modify(|b| *b = (*b).max(k)).or_insert(k);
    }
    for r in rows.iter_mut().filter(|r| r.skip.is_none()) {
        let (_, by) = best[&r.chat.id];
        if by != r.rowid {
            r.skip = Some(Skip::Superseded { by_rowid: by });
        }
    }
    rows.sort_by(|a, b| a.chat.id.0.cmp(&b.chat.id.0).then(a.rowid.cmp(&b.rowid)));
    rows
}

/// A `--chat` argument as a chat id without asking the daemon: `group:` ids
/// (GUID upper-cased, as stored) and handles (`2055551234` = `tel:+12055551234`,
/// as the import maps Mac numbers); `None` for a name.
pub fn filter_id(arg: &str) -> Option<ChatId> {
    match resolve::classify(arg) {
        ChatArg::GroupId(g) => Some(match g.strip_prefix(ChatId::GROUP_PREFIX) {
            Some(guid) => ChatId::group(guid),
            None => ChatId(g),
        }),
        ChatArg::Handle(_) => Some(ChatId(import::peer_handle(arg.trim()).uri)),
        ChatArg::Name(_) => None,
    }
}

// ---------------------------------------------------------------------------
// idempotency
// ---------------------------------------------------------------------------

pub fn sha256_file(p: &Path) -> Option<[u8; 32]> {
    let data = std::fs::read(p).ok()?;
    Some(Sha256::digest(&data).into())
}

/// Whether the daemon's chat already shows this background: the same
/// Apple id (it arrived over iMessage), or a local import (`local-…`)
/// whose stored payload has the same bytes as one of the staged files —
/// unless that import found no picture and the watch rendering (which may
/// have one) is still untried.
pub fn already_there(chat: &Chat, background_id: &str, poster: Option<&Path>, watch: Option<&Path>) -> Option<&'static str> {
    let b = chat.background.as_ref()?;
    if b.id.eq_ignore_ascii_case(background_id) {
        return Some("already-set");
    }
    if !b.id.starts_with("local-") {
        return None;
    }
    let have = sha256_file(Path::new(b.payload.as_deref()?))?;
    let poster_same = poster.and_then(sha256_file) == Some(have);
    let watch_same = watch.and_then(sha256_file) == Some(have);
    if watch_same || (poster_same && (b.path.is_some() || watch.is_none())) { Some("already-imported") } else { None }
}

// ---------------------------------------------------------------------------
// running it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub mac_rowid: i64,
    pub mac_chat: String,
    pub chat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_id: Option<String>,
    /// The Mac's payload version and announced size (not the cached file's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `imported`, `would-import`, `already-set`, `already-imported`,
    /// `not-in-daemon`, `failed`, or a [`Skip::code`]; with `--create`
    /// also `created+imported` / `would-create+import`.
    pub result: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The decoded picture the daemon now shows, after an import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

#[derive(Debug, Serialize)]
struct Summary<'a> {
    host: &'a str,
    dry_run: bool,
    chats: &'a [Outcome],
}

fn path_str(p: &Path) -> anyhow::Result<&str> {
    p.to_str().ok_or_else(|| anyhow::anyhow!("{}: path is not UTF-8", p.display()))
}

/// Import the poster (or, without one, the watch rendering); if the poster
/// holds no picture (a colour poster, a layout the daemon cannot read)
/// and a watch rendering exists, import that instead.
async fn import_files(client: &Client, chat_id: &str, poster: Option<&Path>, watch: Option<&Path>) -> anyhow::Result<(Chat, &'static str)> {
    let first = poster.or(watch).expect("planned rows have a file");
    let c = client.import_chat_background(chat_id, path_str(first)?).await?.value;
    let has_picture = c.background.as_ref().is_some_and(|b| b.path.is_some());
    match (has_picture, poster.is_some(), watch) {
        (false, true, Some(w)) => Ok((client.import_chat_background(chat_id, path_str(w)?).await?.value, "watch rendering (the poster has no picture the daemon can read)")),
        (_, true, _) => Ok((c, "poster")),
        _ => Ok((c, "watch rendering (no poster cached)")),
    }
}

pub async fn run(args: BackgroundArgs, ctx: &Ctx) -> anyhow::Result<u8> {
    if args.host.trim().is_empty() || args.host.starts_with('-') {
        return Err(CliError::usage(format!("invalid host {:?}", args.host)).into());
    }
    // The daemon tells us our own handles (for the chat mapping) and what
    // each chat already has; a dry run goes on without it.
    let client = match Client::connect(&ctx.bus).await {
        Ok(c) => Some(c),
        Err(e) if args.dry_run => {
            if !ctx.quiet && !ctx.json {
                eprintln!("imsg: daemon not reachable ({e}); mapping without our own handles");
            }
            None
        }
        Err(e) => return Err(e.into()),
    };
    let (mut my_handles, mut our) = (vec![], None);
    if let Some(c) = &client {
        let st = c.status().await?.value;
        my_handles = st.registration.handles.iter().map(|h| h.uri.clone()).chain(st.registration.other_handles.iter().cloned()).collect();
        our = st.registration.sender.map(|h| h.uri).or_else(|| my_handles.first().cloned());
    }
    // --chat: ids / handles directly, names through the daemon's chat list.
    let mut wanted: Vec<(String, ChatId)> = vec![];
    for a in &args.chats {
        let id = match filter_id(a) {
            Some(id) => id,
            None => {
                let Some(c) = &client else { return Err(CliError::usage(format!("--chat {a:?}: a name needs the daemon; give a handle or chat id")).into()) };
                let all = c.all_chats().await?;
                match resolve::match_chats(a, &all).as_slice() {
                    [one] => one.id.clone(),
                    [] => return Err(CliError::new(exit::NOT_FOUND, format!("--chat {a:?}: no conversation matches (give a handle or chat id)")).into()),
                    many => return Err(CliError::usage(format!("--chat {a:?} matches {} conversations; use the id", many.len())).into()),
                }
            }
        };
        wanted.push((a.clone(), id));
    }

    let run_id = format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), std::process::id());
    let stage = import::cache_dir().join("imsg/import").join(run_id);
    import::private_dir(&stage).map_err(|e| anyhow::anyhow!("{}: {e}", stage.display()))?;
    if !ctx.quiet && !ctx.json {
        eprintln!("exporting transcript backgrounds from {} into {} ...", args.host, stage.join("backgrounds").display());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let argv = import::ssh_python_argv(&args.host, home.as_deref(), &["--max-file-mb".into(), MAX_FILE_MB.to_string()]);
    let fetched = tokio::task::block_in_place(|| import::fetch_tar(&argv, EXPORT_SCRIPT.to_string(), &args.host, args.max_total_mb, &stage).and_then(|_| read_records(&stage)));
    let recs = match fetched {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    let mut rows = plan(recs, &stage, &my_handles, our.as_deref());
    if !wanted.is_empty() {
        let ids: BTreeSet<&ChatId> = wanted.iter().map(|(_, id)| id).collect();
        rows.retain(|r| ids.contains(&r.chat.id));
        for (a, id) in &wanted {
            if !rows.iter().any(|r| &r.chat.id == id) && !ctx.json {
                eprintln!("imsg: --chat {a}: no Mac chat with a background maps to {id}");
            }
        }
    }

    let result = execute(&args, client.as_ref(), &rows).await;
    let _ = std::fs::remove_dir_all(&stage);
    let out = result?;
    if ctx.json {
        println!("{}", serde_json::to_string(&Summary { host: &args.host, dry_run: args.dry_run, chats: &out })?);
    } else {
        if args.dry_run {
            println!("dry run: nothing imported");
        }
        print!("{}", table(&out));
    }
    Ok(if out.iter().any(|o| o.result == "failed") { exit::ERROR } else { exit::OK })
}

pub(crate) async fn execute(args: &BackgroundArgs, client: Option<&Client>, rows: &[PlanRow]) -> anyhow::Result<Vec<Outcome>> {
    let mut out = vec![];
    for r in rows {
        let mut o = Outcome {
            mac_rowid: r.rowid,
            mac_chat: r.mac.clone(),
            chat_id: r.chat.id.0.clone(),
            background_id: r.background_id.clone(),
            version: r.version,
            size: r.size,
            result: String::new(),
            detail: String::new(),
            picture: None,
        };
        if let Some(s) = &r.skip {
            (o.result, o.detail) = (s.code().into(), s.detail());
            out.push(o);
            continue;
        }
        let bid = r.background_id.as_deref().unwrap_or_default();
        let Some(client) = client else {
            (o.result, o.detail) = ("would-import".into(), "daemon not reachable: not checked".into());
            out.push(o);
            continue;
        };
        let existing = client.get_chat(&r.chat.id.0).await?.map(|j| j.value);
        let create = match &existing {
            Some(c) => {
                if let Some(code) = already_there(c, bid, r.poster.as_deref(), r.watch.as_deref()) {
                    o.result = code.into();
                    o.picture = c.background.as_ref().and_then(|b| b.path.clone());
                    out.push(o);
                    continue;
                }
                false
            }
            None if args.create => true,
            None => {
                (o.result, o.detail) = ("not-in-daemon".into(), "the daemon has no such chat (--create adds it)".into());
                out.push(o);
                continue;
            }
        };
        if args.dry_run {
            o.result = if create { "would-create+import" } else { "would-import" }.into();
            o.detail = if r.poster.is_some() { "poster".into() } else { "watch rendering (no poster cached)".into() };
            out.push(o);
            continue;
        }
        if create {
            let mut chat = r.chat.clone();
            chat.last_message_at = None;
            let batch = ImportBatch { chats: vec![chat], messages: vec![], reactions: vec![] };
            if let Err(e) = client.import_messages(&serde_json::to_string(&batch)?).await {
                (o.result, o.detail) = ("failed".into(), format!("creating the chat: {e}"));
                out.push(o);
                continue;
            }
        }
        match import_files(client, &r.chat.id.0, r.poster.as_deref(), r.watch.as_deref()).await {
            Ok((c, used)) => {
                o.result = if create { "created+imported" } else { "imported" }.into();
                let b = c.background.as_ref();
                o.picture = b.and_then(|b| b.path.clone());
                o.detail = format!("{used} as {}", b.map(|b| b.id.as_str()).unwrap_or("?"));
                if o.picture.is_none() {
                    o.detail.push_str("; no picture found in it");
                }
            }
            Err(e) => (o.result, o.detail) = ("failed".into(), e.to_string()),
        }
        out.push(o);
    }
    Ok(out)
}

/// MAC CHAT / DAEMON CHAT / BACKGROUND / RESULT, then the details.
pub fn table(rows: &[Outcome]) -> String {
    use std::fmt::Write;
    let head = ["MAC CHAT", "DAEMON CHAT", "BACKGROUND", "RESULT"];
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|o| [o.mac_chat.clone(), o.chat_id.clone(), o.background_id.clone().unwrap_or_else(|| "-".into()), o.result.clone()])
        .collect();
    let mut w = head.map(|h| h.chars().count());
    for c in &cells {
        for (i, v) in c.iter().enumerate() {
            w[i] = w[i].max(v.chars().count());
        }
    }
    let line = |out: &mut String, c: [&str; 4]| {
        let _ = writeln!(out, "{:<a$}  {:<b$}  {:<d$}  {}", c[0], c[1], c[2], c[3], a = w[0], b = w[1], d = w[2]);
    };
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("no Mac chat with a transcript background\n");
        return out;
    }
    line(&mut out, head);
    for c in &cells {
        line(&mut out, [&c[0], &c[1], &c[2], &c[3]]);
    }
    for o in rows.iter().filter(|o| !o.detail.is_empty() || o.picture.is_some()) {
        let mut d = o.detail.clone();
        if let Some(p) = &o.picture {
            d = if d.is_empty() { format!("picture {p}") } else { format!("{d}; picture {p}") };
        }
        let _ = writeln!(out, "  {}: {d}", o.chat_id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(rowid: i64, style: i64, ident: &str, group_id: Option<&str>, participants: &[&str], bid: Option<&str>, refreshed: Option<i64>) -> BgChatRec {
        BgChatRec {
            chat: ChatRec {
                rowid,
                chat_identifier: ident.into(),
                style,
                display_name: None,
                group_id: group_id.map(String::from),
                participants: participants.iter().map(|s| s.to_string()).collect(),
            },
            service: Some("iMessage".into()),
            background: BgFields { id: bid.map(String::from), version: Some(1), size: Some(10), refreshed },
            poster: bid.map(|b| format!("backgrounds/{b}")),
            watch: None,
            missing: None,
        }
    }

    fn stage_with(files: &[&str]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("backgrounds")).unwrap();
        for f in files {
            std::fs::write(d.path().join("backgrounds").join(f), f.as_bytes()).unwrap();
        }
        d
    }

    #[test]
    fn records_parse() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("backgrounds.jsonl"),
            concat!(
                r#"{"type":"meta","format":1,"files":1}"#,
                "\n",
                r#"{"type":"chat","rowid":7,"chat_identifier":"chat1","style":43,"service":"iMessage","display_name":"Fam","group_id":"g-1","participants":["bob@example.com","carol@example.com"],"background":{"id":"AAAA-1","version":1,"size":9,"av":5,"refreshed":1790856000},"poster":"backgrounds/AAAA-1","watch":null,"missing":null}"#,
                "\n"
            ),
        )
        .unwrap();
        let r = read_records(d.path()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].chat.rowid, r[0].chat.style, r[0].chat.group_id.as_deref()), (7, 43, Some("g-1")));
        assert_eq!(r[0].background.id.as_deref(), Some("AAAA-1"));
        assert_eq!(r[0].background.refreshed, Some(1_790_856_000));
        assert_eq!(r[0].poster.as_deref(), Some("backgrounds/AAAA-1"));
    }

    /// The same mapping as the history import: 1:1 = the other party
    /// (`+1` added to national numbers, SMS tags stripped), a group =
    /// `group:<GROUP_ID upper-case>`, a two-person Mac group = the 1:1, our
    /// own handles = note to self.
    #[test]
    fn mapping_matches_the_history_import() {
        let st = stage_with(&["A", "B", "C", "D", "E", "F", "G"]);
        let mine = vec!["mailto:me@icloud.example".to_string(), "tel:+12055550100".to_string()];
        let recs = vec![
            rec(1, 45, "2055551234", None, &["2055551234"], Some("A"), None),
            rec(2, 43, "chat2", Some("abcd-0001"), &["bob@example.com", "carol@example.com", "+12055550100"], Some("B"), None),
            rec(3, 43, "chat3", Some("eeee-0003"), &["dave@example.com", "me@icloud.example"], Some("C"), None),
            rec(4, 45, "me@icloud.example", None, &["me@icloud.example"], Some("D"), None),
            rec(5, 45, "+12055550199(smsft)", None, &["+12055550199(smsft)"], Some("E"), None),
            rec(6, 45, "mom", None, &["mom"], Some("F"), None),
            rec(7, 43, "chat7", None, &["bob@example.com", "erin@example.com"], Some("G"), None),
        ];
        let rows = plan(recs, st.path(), &mine, Some("mailto:me@icloud.example"));
        let got: BTreeMap<i64, (&str, Option<&Skip>, bool)> = rows.iter().map(|r| (r.rowid, (r.chat.id.as_str(), r.skip.as_ref(), r.chat.is_group))).collect();
        assert_eq!(got[&1], ("tel:+12055551234", None, false));
        assert_eq!(got[&2], ("group:ABCD-0001", None, true));
        assert_eq!(got[&3], ("mailto:dave@example.com", None, false), "a two-person Mac group is the 1:1");
        assert_eq!(got[&4], ("mailto:me@icloud.example", None, false));
        assert_eq!(got[&5], ("tel:+12055550199", None, false));
        assert_eq!(got[&6].1, Some(&Skip::NoAddress));
        let p = ChatId::group_by_participants(&["mailto:bob@example.com".into(), "mailto:erin@example.com".into(), "mailto:me@icloud.example".into()]);
        assert_eq!(got[&7].0, p.0, "no group id: participants including ours");
        let g = rows.iter().find(|r| r.rowid == 2).unwrap();
        assert_eq!(g.chat.participants, vec![Handle::new("bob@example.com"), Handle::new("carol@example.com")]);
        assert_eq!(g.poster.as_deref(), Some(st.path().join("backgrounds/B").as_path()));
        // sorted by daemon chat id
        assert!(rows.windows(2).all(|w| w[0].chat.id.0 <= w[1].chat.id.0));
    }

    #[test]
    fn files_ids_and_duplicates() {
        let st = stage_with(&["A", "B", "C-watchBackground", "D"]);
        std::os::unix::fs::symlink("/etc/passwd", st.path().join("backgrounds/L")).unwrap();
        let mut watch_only = rec(3, 45, "+12055550003", None, &["+12055550003"], Some("C"), None);
        watch_only.poster = None;
        watch_only.watch = Some("backgrounds/C-watchBackground".into());
        let mut traversal = rec(4, 45, "+12055550004", None, &["+12055550004"], Some("X"), None);
        traversal.poster = Some("backgrounds/../backgrounds.jsonl".into());
        let mut too_large = rec(5, 45, "+12055550005", None, &["+12055550005"], Some("Y"), None);
        too_large.poster = None;
        too_large.missing = Some("too_large".into());
        let mut bad = rec(6, 45, "+12055550006", None, &["+12055550006"], None, None);
        bad.missing = Some("bad_id".into());
        let recs = vec![
            // SMS and iMessage rows of one number: the newer background wins
            rec(1, 45, "+12055550001", None, &["+12055550001"], Some("A"), Some(100)),
            rec(2, 45, "2055550001", None, &["2055550001"], Some("B"), Some(200)),
            watch_only,
            traversal,
            too_large,
            bad,
            rec(7, 45, "+12055550007", None, &["+12055550007"], Some("L"), None),
            rec(8, 45, "+12055550008", None, &["+12055550008"], Some("D"), None),
        ];
        let rows = plan(recs, st.path(), &[], None);
        let skip = |rowid: i64| rows.iter().find(|r| r.rowid == rowid).unwrap().skip.clone();
        assert_eq!(skip(1), Some(Skip::Superseded { by_rowid: 2 }));
        assert_eq!(skip(2), None);
        let w = rows.iter().find(|r| r.rowid == 3).unwrap();
        assert_eq!((w.skip.clone(), w.poster.is_none(), w.watch.is_some()), (None, true, true));
        assert_eq!(skip(4), Some(Skip::NotCached), "paths outside backgrounds/ are ignored");
        assert_eq!(skip(5), Some(Skip::TooLarge));
        assert_eq!(skip(6), Some(Skip::BadId));
        assert_eq!(skip(7), Some(Skip::NotCached), "symlinks are not regular files");
        assert_eq!(skip(8), None);
    }

    #[test]
    fn chat_filters() {
        assert_eq!(filter_id("group:abcd-1").unwrap().0, "group:ABCD-1");
        assert_eq!(filter_id("2055551234").unwrap().0, "tel:+12055551234");
        assert_eq!(filter_id("+1 (205) 555-1234").unwrap().0, "tel:+12055551234");
        assert_eq!(filter_id("tel:+12055551234").unwrap().0, "tel:+12055551234");
        assert_eq!(filter_id("Bob@Example.com").unwrap().0, "mailto:bob@example.com");
        assert_eq!(filter_id("Family"), None);
    }

    #[test]
    fn idempotency() {
        use imessage_common::model::ChatBackground;
        let st = stage_with(&["P", "P-watchBackground"]);
        let d = tempfile::tempdir().unwrap();
        let stored = d.path().join("local-1");
        std::fs::write(&stored, b"P").unwrap();
        let (poster, watch) = (st.path().join("backgrounds/P"), st.path().join("backgrounds/P-watchBackground"));
        let mut chat = Chat {
            id: ChatId::direct("bob@example.com"),
            participants: vec![],
            display_name: None,
            last_message_at: None,
            unread_count: 0,
            is_group: false,
            background: None,
            icon_path: None,
            pinned: false,
            pin_order: None,
        };
        assert_eq!(already_there(&chat, "P", Some(&poster), None), None);
        let bg = |id: &str, payload: Option<&Path>, path: Option<&str>| ChatBackground {
            id: id.into(),
            path: path.map(String::from),
            payload: payload.map(|p| p.to_string_lossy().into_owned()),
            version: 1,
            set_by: None,
            set_at: 0,
        };
        chat.background = Some(bg("p", None, None));
        assert_eq!(already_there(&chat, "P", Some(&poster), None), Some("already-set"), "same Apple id");
        chat.background = Some(bg("local-1", Some(&stored), Some("/x.heic")));
        assert_eq!(already_there(&chat, "P", Some(&poster), Some(&watch)), Some("already-imported"), "same payload bytes");
        chat.background = Some(bg("local-1", Some(&stored), None));
        assert_eq!(already_there(&chat, "P", Some(&poster), Some(&watch)), None, "no picture yet: try the watch rendering");
        assert_eq!(already_there(&chat, "P", Some(&poster), None), Some("already-imported"));
        std::fs::write(&stored, b"P-watchBackground").unwrap();
        assert_eq!(already_there(&chat, "P", Some(&poster), Some(&watch)), Some("already-imported"), "the watch rendering was imported");
        std::fs::write(&stored, b"other").unwrap();
        assert_eq!(already_there(&chat, "P", Some(&poster), Some(&watch)), None);
        chat.background = Some(bg("OTHER-ID", Some(&stored), Some("/x.heic")));
        assert_eq!(already_there(&chat, "P", Some(&poster), None), None, "a different live background is replaced");
    }

    #[test]
    fn table_lists_every_row() {
        let rows = vec![Outcome {
            mac_rowid: 1,
            mac_chat: "#1 +12055550001 (iMessage)".into(),
            chat_id: "tel:+12055550001".into(),
            background_id: Some("AAAA-1".into()),
            version: Some(1),
            size: None,
            result: "imported".into(),
            detail: "poster as local-1".into(),
            picture: Some("/s/local-1.heic".into()),
        }];
        let t = table(&rows);
        assert!(t.starts_with("MAC CHAT"));
        assert!(t.contains("#1 +12055550001 (iMessage)  tel:+12055550001  AAAA-1      imported"), "{t}");
        assert!(t.contains("tel:+12055550001: poster as local-1; picture /s/local-1.heic"));
        assert_eq!(table(&[]), "no Mac chat with a transcript background\n");
    }

    /// The exporter's Python tests (tools/test_mac_backgrounds_export.py),
    /// with a `properties` fixture written here as a binary plist; skipped
    /// when python3 is not installed.
    #[test]
    fn exporter_python_tests() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("mac-backgrounds-export.py");
        let test = dir.path().join("test_mac_backgrounds_export.py");
        let fixture = dir.path().join("properties.plist");
        std::fs::write(&script, EXPORT_SCRIPT).unwrap();
        std::fs::write(&test, include_str!("../../../tools/test_mac_backgrounds_export.py")).unwrap();
        let secret = plist::Value::String("SECRET-KEY-MATERIAL-0123456789".into());
        let mut bp = plist::Dictionary::new();
        bp.insert("trabaid".into(), plist::Value::String("11111111-2222-3333-4444-555555555555".into()));
        bp.insert("trabapv".into(), plist::Value::Integer(1.into()));
        bp.insert("trabafs".into(), plist::Value::Integer(123_456.into()));
        bp.insert("trabav".into(), plist::Value::Integer(7.into()));
        bp.insert("trabaCommSafety".into(), plist::Value::Integer(0.into()));
        for k in ["trabak", "trabas", "trabar", "traboid"] {
            bp.insert(k.into(), secret.clone());
        }
        let mut top = plist::Dictionary::new();
        top.insert("backgroundProperties".into(), plist::Value::Dictionary(bp));
        top.insert("backgroundChannelGUID".into(), secret);
        plist::Value::Dictionary(top).to_file_binary(&fixture).unwrap();
        let out = match std::process::Command::new("python3").args(["-I", "-B"]).arg(&test).arg(&script).arg(&fixture).output() {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return eprintln!("python3 not found; skipping the exporter tests"),
            Err(e) => panic!("cannot run python3: {e}"),
        };
        let log = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{log}");
        assert!(!log.contains("skipped=1"), "the plist fixture test ran: {log}");
    }
}
