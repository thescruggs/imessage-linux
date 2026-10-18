# imessage-linux — plan

Goal: a native Linux iMessage client (CLI + KDE app styled after macOS
Messages) that talks to Apple directly. No Mac/iPhone relay, no third-party
server. A Mac is touched **once** to extract its hardware identity
(see docs/RESEARCH.md for why that is unavoidable).

## Architecture

```
┌──────────────┐   D-Bus (session)    ┌──────────────────────────┐
│ kmessages    │◄────────────────────►│ imessaged (Rust daemon)  │
│ (Qt6/Kirigami)│                     │  • rustpush engine       │
└──────────────┘                      │  • open-absinthe (NAC)   │
┌──────────────┐                      │  • anisette (remote v3)  │
│ imsg (CLI)   │◄────────────────────►│  • SQLite store          │
└──────────────┘                      │  • state/keys on disk    │
                                      └───────────┬──────────────┘
                                                  │ APNs 5223 / HTTPS
                                                  ▼
                                            Apple (courier, IDS, GSA, MMCS)
```

Crate layout (Cargo workspace):

| path | crate | role |
|---|---|---|
| `third_party/rustpush` | git submodule, pinned | protocol engine (SSPL) |
| `crates/open-absinthe` | `open-absinthe` drop-in | NAC emulator: runs `IMDAppleServices` under `unicorn-engine`, exposes `HardwareConfig` + `ValidationCtx` with the exact API rustpush expects |
| `crates/imessaged` | daemon | wraps rustpush: login/2FA, register, APNs loop, renewal, message store, D-Bus `org.imessage.Daemon1` |
| `crates/imsg` | CLI | `imsg login`, `status`, `chats`, `messages`, `send`, `send-file`, `react`, `watch`, `doctor`, `hwconfig import` |
| `kmessages/` | KDE app | C++/QML Kirigami 6, QtDBus client, Messages.app-like 3-pane UI |
| `crates/imessage-common` | library | hwconfig format + validation, shared paths |
| `patches/` + `tools/bootstrap.sh` | setup | submodule init, patch application, `IMDAppleServices` download + sha1 check |
| `tools/mac-hwinfo.sh` | script | run once on the Mac; writes `hwconfig.plist` |

## Phases (each ends with an independent review; loop until clean)

**Phase 0 — scaffold.** Workspace, submodule pin, SSPL license, CI-free
`cargo build`, docs. Patch rustpush minimally (legacy FairPlay cert path,
strip `tel:` handles, anisette client-info) via `patches/*.patch` applied by
`tools/bootstrap.sh`.

**Phase 1 — NAC emulator (`crates/open-absinthe`).** Port pypush `nac.py`/`jelly.py`
to Rust on `unicorn-engine` (reference: `docs/reference-relay-03-open-absinthe.patch`).
Mach-O x86-64 loader, lazy-bind resolution, ~37 CF/IOKit hooks, heap/stack,
`nac_init` / `key_establishment` / `sign`. Binary downloaded and sha1-verified
by `tools/bootstrap.sh` (the library itself does no network I/O; it re-checks
the sha1 on load); never committed. Tests: an offline check against the pypush
reference identity and the bootstrap-placed cert (338-byte request, no hook
anomalies; skips when the binary is absent), plus an `--ignored` live test
against `id-initialize-validation` (338 → 698 → 517 bytes).

**Phase 2 — daemon.** rustpush integration: Apple ID login with 2FA prompt
(via D-Bus signal/CLI), IDS registration + automatic renewal/re-register,
APNs connection with reconnect, inbound message decode → SQLite
(chats, messages, participants, attachments, reactions, read/typing state),
outbound send/edit/unsend/react/typing/read receipts, attachment up/download
via MMCS, D-Bus API + signals, systemd user unit, state encrypted at rest
(key in kwallet/Secret Service, fallback file 0600).
Split into **2a** (engine wrapper behind an `Engine` trait, encrypted state,
shared model; see `docs/DAEMON.md`) and **2b** (SQLite store, D-Bus API,
systemd unit, attachment download).

**Phase 3 — CLI (`imsg`).** Thin D-Bus client. Scriptable JSON output.
`imsg doctor` validates hwconfig, anisette reachability, binary hash,
registration status.

**Phase 4 — KDE app (`kmessages`).** Kirigami 6 + kirigami-addons. Sidebar
conversation list (avatar, name, snippet, time, unread dot), bubble transcript
(blue sent / gray received, grouped, tapbacks, delivered/read, typing
indicator, attachments/images inline, link previews), composer (attach,
emoji, enter-to-send), search, new-message with handle lookup, KNotifications
with inline reply, system tray, Breeze-consistent, offscreen screenshot tests.

**Phase 5 — live bring-up (needs you).** You run `tools/mac-hwinfo.sh` on the
Mac, copy `hwconfig.plist` here, run `imsg login` with your Apple ID + 2FA.
Then end-to-end tests with a real iPhone/Mac on the other side. Fix loop.

**Phase 6 — polish/hardening.** Reconnect storms, renewal edge cases, group
chat admin ops, stickers, PQ3 (if rustpush exposes it), local anisette (ADI)
to drop the remote anisette dependency, packaging (AUR/Flatpak).

## Execution model (requested)

Fable (this session) directs **Opus** implementation agents per phase, then an
independent **Fable** review agent audits the diff for correctness, security
(key handling, IPC), and spec fidelity; findings are fed back and the phase
repeats until the reviewer has no blocking findings and `cargo test` /
`cmake --build` pass.

## Next (planned 2026-10-17, in progress): kmessages pinned chats, keyboard, emoji suggestions

Three features requested for kmessages, in the order they should be built.
Each is its own PR-sized step with tests, docs and a changelog line; the
usual loop (Opus implements, Fable reviews) applies.

### A. Pinned conversations at the top (as Messages.app)

Messages.app shows *pinned* conversations above the list. The Mac keeps the
pins in `~/Library/Preferences/com.apple.messages.pinning.plist`
(`pD.pP`: an ordered array of handles such as `+1205…` or `bob@x.com`, group
chat GUIDs for groups; `pR`/`pV` are format versions). Contacts carry no
"favorite" flag in vCards, so pins are the source of truth, like on the Mac.

1. **[done 2026-10-18]** **Daemon / store** (`crates/imessaged`): `chats.pinned_at INTEGER` and
   `chats.pin_order INTEGER` (migration 0006). `Chat` JSON gains
   `pinned: bool`, `pin_order: int|null`. D-Bus `SetChatPinned(chat_id,
   pinned, order)` and `ReorderPins(chat_ids_json)`; `ChatUpdated` is
   emitted as for other chat changes. Local only (not synced to Apple;
   Messages in iCloud pin sync is CloudKit and out of scope).
2. **[done 2026-10-18]** **CLI**: `imsg chat CHAT --pin / --unpin` (and `--position N`), `imsg chats` marks pinned rows
   (📌 / `pinned` column in `--json`). Pins come from the Mac through the
   sync in section D (the pinning plist, each entry mapped to a chat id:
   handle → 1:1 `mailto:`/`tel:` id, group GUID → `group:` id; unknown
   ones reported; order kept). Pinning in kmessages is local; the next
   sync from the Mac wins for entries the Mac has, and keeps local-only pins
   (so pinning here is not undone by a sync). As built: `imsg` remembers
   which pins the last sync took from the Mac
   (`~/.local/state/imsg/mac-pins.json`), so a chat the Mac unpinned is
   unpinned here too, while pins made here stay; group pins that are not a
   GUID (`chat123…`) are looked up in the Mac's `chat.db` and mapped like
   the history import.
3. **[done 2026-10-18]** **kmessages**: `ChatListModel` sorts pinned chats first by `pin_order`,
   then the rest by time as now; a "Pinned" section header (Kirigami
   `ListSectionHeader`) when any exist; `ChatListDelegate` gets a context
   menu (right-click / long-press) with Pin / Unpin (and later Mute);
   the chat details page gets the same toggle. Drag-to-reorder of pins is
   a stretch goal (use `ReorderPins`). Tests: model ordering with mixed
   pins (tst_models), a `pinned-chats` screenshot (tst_screens), fake
   daemon `SetChatPinned`.
   As built: roles `pinned`, `pinOrder`, `section` ("Pinned" /
   "Conversations", empty while nothing is pinned, so the plain list has
   no header); a pin change re-sorts the whole list (the daemon renumbers
   several pins, one `ChatUpdated` each), new activity only repositions
   within its block. Reorders are `layoutChanged(VerticalSortHint)`, and
   the list binds `ChatListModel` directly unless searching: the
   `QSortFilterProxyModel` drops the hint, and QML resets the view (all
   delegates, scroll to top) on an unhinted layout change (review fix). Context menu = right-click or press
   and hold → Pin / Unpin (`SetChatPinned`, order -1); a Pin / Unpin button
   under the heading of the details page; a small `pin` icon next to the
   time. Not built: drag to reorder the pins (`ReorderPins` is wired in
   `DaemonClient::reorderPins` but has no UI yet; the Mac's order arrives
   through the sync, `imsg chat CHAT --pin --position N` reorders by hand),
   and Mute (the daemon has no mute).

### B. Keyboard: Ctrl+N and picking recipients with ↑ ↓ Enter

`Ctrl+N` already exists (`Main.qml`, `StandardKey.New`, since 0.1.10) and
opens the new-message page (verified by the user 2026-10-17; nothing to do
beyond mentioning it in the docs). The new-message page:

1. The suggestion area is two models (address-book `contactSuggestions`
   rendered in the ListView header, then `recipientSuggestions`). Give the
   page one `highlightedIndex` over the combined rows (contacts first), with
   the first row highlighted whenever the text changes.
2. In the `To:` field: `Down`/`Up` move the highlight (wrapping, scrolling
   the row into view), `Enter`/`Return` adds the highlighted row (contact →
   its URI; existing conversation → opens it) and falls back to the typed
   text when nothing is highlighted, `Escape` clears the text, `Tab` keeps
   its focus behaviour. Mouse hover moves the highlight.
3. Also: `Escape` on the new-message page goes back; `Ctrl+Enter` is left
   as is in the composer. Tests in tst_screens with `QTest::keyClick` on
   the recipient field (down, down, enter → recipient chip added; enter with
   no match → typed address).

### C. Emoji suggestions while typing

Typing `snake` offers 🐍, like macOS's inline emoji replacement.

1. **Data**: bundle a compact keyword table built from Unicode CLDR
   `annotations/en.xml` (+ `annotationsDerived`) and `emoji-test.txt`
   (fully-qualified, no skin-tone variants): `tools/gen-emoji-keywords.py`
   writes `kmessages/data/emoji-keywords.json` (`[["🐍", ["snake"]], …]`,
   ~150 KB, Unicode License, pinned CLDR version noted in the file). Shipped
   via `qt_add_resources`. If `/usr/share/plasma/emoji/en.dict` (ibus
   format, Plasma's emojier) turns out to be readable without GLib, it can
   be a later optional source for other languages.
2. **Model**: `EmojiSuggestionModel` (C++): `setWord(QString)` → ranked
   matches (prefix match on keyword first, then substring; stable emoji
   order; cap 8), plus the user's recently used emoji first (stored in the
   KConfig `Emoji/Recent`). Unit tests in tst_models.
3. **Composer**: the word before the cursor (≥ 3 letters, or `:` + 2
   letters for the `:snake:` form) feeds the model; a small popup above the
   text area lists the emoji; `Tab`/`Enter` inserts the highlighted one,
   **replacing the word** in both forms (decided 2026-10-17: "snake" becomes
   "🐍"); `Left`/`Right` or `Up`/`Down` move the highlight, `Escape`
   dismisses for that word. On by default, with a Settings toggle ("Suggest
   emoji while typing"). The existing EmojiPicker grid gets a search field
   backed by the same model.
4. Tests: model ranking; a screenshot with the popup open; the composer
   test types "snake" and checks the suggestion and insertion.

### D. "Sync from Mac" button: contacts, photos, pins (and backgrounds)

Today contacts arrive through `tools/mac-contacts-export.sh` (bash wrapper
around a Python script that runs on the Mac over one ssh connection and
streams a tar of vCards with photos back into `~/.local/share/kpeoplevcard/`),
and `imsg import-mac HOST --backgrounds` uses the same shape (embedded
Python over ssh, tar back). The sync unifies them behind one command and one
button.

1. **[done 2026-10-18]** **CLI** `imsg sync-mac [HOST] [--contacts] [--pins] [--backgrounds]
   [--dry-run]` (default: contacts + pins; also `--json`, `--contacts-dir`). One ssh round trip: the
   embedded script collects the Contacts database export (ported from
   `mac-contacts-export.sh`, same vCard output and `mac-<uid>.vcf` naming,
   photos embedded), `~/Library/Preferences/com.apple.messages.pinning.plist`
   (`plutil -convert xml1 -o -`), and optionally the backgrounds records,
   into one tar. On Linux: write/replace the `mac-*.vcf` files (KPeople picks
   them up live), apply pins through `SetChatPinned`/`ReorderPins`, run the
   existing backgrounds plan. Prints a summary (contacts written/removed,
   pins applied/unknown, backgrounds). `HOST` defaults to a new daemon
   setting `mac_host` (`imessaged` config / `GetStatus.mac_host`), prefilled
   at `imsg doctor` time from the ssh host in `anisette_command` when that is
   an ssh command. `mac-contacts-export.sh` becomes a thin wrapper that calls
   `imsg sync-mac --contacts` (kept for the README).
   As built: the Mac side is `tools/mac-sync-export.py` (contacts code
   ported unchanged, output byte-identical; `mac-backgrounds-export.py`
   embedded for `--backgrounds`), one `sync.json` reports each part or
   its error; only changed vCards are rewritten; `imsg doctor` suggests
   `mac_host` (it does not write config.toml). The wrapper falls back to
   running the exporter itself when `imsg` is not installed.
2. **[done 2026-10-18]** **kmessages**: a "Sync from Mac" toolbar button next to "New message"
   (icon `view-refresh`, tooltip with the host and the last sync time) runs
   `imsg sync-mac` through `QProcess` (imsg is installed with the app;
   `KMESSAGES_IMSG` env / Settings path override for development), with a
   busy indicator on the button, a passive notification with the summary
   or the error (ssh not reachable, Full Disk Access missing: the script's
   messages are passed through), and the Mac host editable in Settings
   (defaults to the daemon's `mac_host`). Also `Tools > Sync from Mac` in
   the menu. The last sync time lives in the app's KConfig. Group photos
   and shared names already come from iCloud and need no sync.
   As built: `MacSync` (`kmessages/src/macsync.{h,cpp}`, `App.macSync`)
   runs `imsg sync-mac --json [-- HOST]` (HOST only when set in Settings;
   otherwise imsg reads the daemon's `mac_host` itself) and turns the JSON
   into one line ("Synced from mac.local: 312 contacts (4 updated), 3
   pins (1 not found)"; part errors as "Sync from Mac incomplete: …";
   `{"error"}` and exit code 5 have their own messages; non-JSON output
   falls back to imsg's last stderr line); a 10-minute timeout kills a
   hung ssh (SIGTERM, SIGKILL after 3 s); clicking the busy button stops a
   sync the same way. The summary lists every pin outcome imsg reports. There is no menu bar, so the second entry point is the tray
   menu ("Sync from Mac"); Settings has a Sync from Mac card (Mac host,
   Sync Now with the last sync time, the imsg path). The toolbar button
   is the action's `displayComponent`, with a busy indicator in place of
   the icon while running; kmessagesrc keys `MacHost`, `ImsgPath`
   (`[General]`) and `[MacSync] LastSync`.
3. **Tests**: the Rust side like `import-mac` (tar fixture → vCards, pins
   mapping, dry run); kmessages tests with a fake `imsg` script on `PATH`
   that prints a summary (button busy state, notification, failure path).
   Docs: README "Contacts" and "Install", CLI.md `sync-mac`, KMESSAGES.md.

### E. Link previews (as Messages)

Messages does not let the receiver fetch anything: the **sender** fetches
the page's metadata (title, summary, site icon, hero image), packs it as a
`com.apple.messages.URLBalloonProvider` balloon (`RichLink` →
`LPLinkMetadata` + image attachments) and sends it with the text; every
device shows the same card. rustpush already does both directions
(`NormalMessage.link_meta: Option<LinkMeta>`; parsed on receive, packed on
send; `rawmessages.rs` has `LPLinkMetadata` with `title`, `summary`, `url`,
`original_url`, `image`/`icon` substitutes indexing the balloon's
attachments). The daemon ignores `link_meta` today, so a link arrives as
plain text with the card missing, and a message that is *only* a URL
balloon shows "app message".

1. **[done 2026-10-18]** **Model / store**: new part `{"type": "link", "url", "title", "summary",
   "site_name", "image_attachment_id", "icon_attachment_id"}`
   (`MessagePart::Link`); images and icons are stored as ordinary
   attachment rows and files under `attachments/` (`inline`, no download
   step), so `DownloadAttachment`, purge and import paths need nothing new.
   `parts` is JSON in the store (schema unchanged; schema version 6 only
   if a backfill is needed). `Message.text` keeps the URL so search,
   snippets and the CLI keep working.
2. **[done 2026-10-18]** **Receive** (`convert.rs`): `link_meta` → a `link` part after the text
   part(s), its image/icon bytes → attachments. A URL-balloon with no text
   gets the URL as text from `LPLinkMetadata.url`. Unparseable metadata →
   today's behaviour, logged once per message.
3. **[done 2026-10-18]** **Send** (`Engine::send_text`): when the text contains a URL (first
   `http(s)://` token, as Messages previews only the first), the daemon
   fetches it (reqwest, 5 s, ≤ 2 MB, HTML only, follow ≤ 3 redirects,
   our own User-Agent; a small Open Graph / Twitter-card / `<title>` parser,
   `lol_html` or a minimal hand parser over the `<head>`; `og:image`
   downloaded ≤ 5 MB, re-encoded to JPEG ≤ 1024 px with the `image` crate
   already used for photos; `favicon`/`apple-touch-icon` ≤ 256 px), builds
   `LinkMeta` and sends the message with it in one go, so the recipient's
   iPhone shows the same card as from a Mac. The fetch runs before the
   send with a hard deadline; on any failure the text goes out alone (as
   Messages does when offline). The sent row stores the same `link` part
   so our own bubble shows the card. Setting `link_previews = true`
   (default on, decided 2026-10-17; daemon `config.toml` /
   `--no-link-previews`), and `SendTextAs` keeps
   working for SMS (no preview: MMS does not carry it).
   Privacy note in docs: the daemon contacts the linked site when *you*
   send a link, never when you receive one.
4. **[done 2026-10-18]** **kmessages**: a `LinkPreview.qml` card below the text in the bubble
   (image on top when present, else icon + title, title bold, site name or
   host under it, as Messages), clickable (opens the URL), hover shows the
   full URL, context menu "Copy link"; for a bubble whose text is just the
   URL, the card replaces the text (Messages hides the bare URL; decided
   2026-10-17). Chat list
   snippet: the title when the text is only the URL. Composer: nothing to
   show while typing (Messages shows the preview after send; a pre-send
   preview is a later nicety).
5. **[done 2026-10-18]** **CLI**: `imsg messages` prints `[link: <title> — <host>]` after the text;
   `--json` carries the part.
6. **[done 2026-10-18]** **Mac import**: `chat.db` keeps the balloon in `message.payload_data`
   (the same `RichLink` archive); the export script adds it to the tar and
   the import turns it into the `link` part, so old conversations get their
   cards back. Fake daemon: `InjectLink` for tests; a `link-preview`
   screenshot; convert tests for receive (with and without text), send tests
   with a local HTTP server fixture (OG tags, redirect, timeout, non-HTML).

   As built (all six steps, 2026-10-18): part `MessagePart::Link { url,
   title, summary, site_name, image_attachment_id, icon_attachment_id }`;
   image / icon are inline attachment rows `<id>:link-image` /
   `<id>:link-icon` (no attachment part; the pump writes their files when
   the message is stored, `DownloadAttachment` works as for any inline
   attachment); schema unchanged. Receive: `convert::link_from_meta` (URL,
   else originalURL; bad indexes, empty or > 8 MiB images dropped, the card
   kept). Send: `linkpreview.rs` (reqwest 0.11 already in the tree;
   one 5 s deadline for page + image + icon, ≤ 3 redirects checked by a
   custom policy, `text/html` / XHTML only, ≤ 2 MiB, stops at `</head>`,
   own tolerant head scanner). **No re-encoding**: the `image` crate is
   not a dependency of imessaged (the plan assumed it was), so a JPEG/PNG
   ≤ 1 MiB is sent as is and anything else is skipped; icons must be
   ≤ 256 px (sizes read from the PNG / JPEG header). Extra: no previews for
   localhost / `*.local` / single-label / private-IP hosts (also as a
   redirect target), so an internal page's title never goes to the
   recipient. `Engine::send_text_with_link` (default: text alone); the
   mock records `MockCall::LinkPreview`. Config `link_previews`
   (`--link-previews BOOL`, `--no-link-previews`,
   `IMESSAGED_LINK_PREVIEWS`); a bare `Hub` has previews off, the daemon
   sets them from the config. Mac import: the exporter adds
   `payload_data` (base64) and the hidden preview files (`link_file`);
   `imsg` passes them on as `ImportMessage.link` and the **daemon**
   decodes the archive (rustpush's `KeyedArchive`, read leniently by
   `convert::rich_link_from_archive`), so `imsg` stays free of rustpush
   (`cargo tree -p imsg -e normal`). The substitute index is taken as the
   position among the message's hidden attachments in `chat.db` ROWID
   order (not verified against a real Mac export yet). kmessages:
   `LinkPreview.qml`, roles `link` / `linkOnly`, "Copy Link" lives in the
   message menu (so tapbacks / reply stay reachable from the card); fake
   daemon seeds two link messages in Eve's chat and has `InjectLink`.

Order: D (CLI first, then the button) together with A's daemon part, then
A's app part, E, B, C. Release as 0.1.13.

## Future: easy install and a Mac companion app (not scheduled)

Goal: someone installs imessage-linux without a terminal on either machine.

### Linux: one-click install

* **Flatpak** (`org.imessage.kmessages`) bundling kmessages, imsg and the
  daemon (daemon started as a user service from inside the sandbox via the
  Background portal, or installed as a user unit on first run), on Flathub
  once the SSPL question for Flathub is settled; AppImage/AUR as fallbacks.
  Apple's `IMDAppleServices` still cannot be redistributed: the first-run
  wizard downloads it (as `bootstrap.sh` does) and shows the license note.
* **First-run wizard in kmessages**: Apple download, Mac pairing (below),
  Apple ID login with 2FA, contacts sync, done. Replaces the README steps
  `bootstrap.sh` → `mac-hwinfo.sh` → `hwconfig import` → `imsg login`.

### macOS: "iMessage Linux Helper" companion app

A small signed-and-notarized menu-bar app on the Mac that replaces every ssh
script. Setup becomes: install the helper, click Pair, scan or type the code
shown by kmessages.

* **Pairing** over the local network: the helper advertises itself with
  Bonjour (`_imessage-linux._tcp`); kmessages finds it, shows a 6-digit code,
  the user confirms on the Mac; both sides keep a key pair, traffic is mutual
  TLS (the same `rtc`/TLS stack the daemon already has). Works off-LAN later
  through a relay only if ever needed; not planned.
* **Hardware identity on demand**: the helper reads the same fields
  `tools/mac-hwinfo.sh` reads (IOKit, no root) and sends `hwconfig.plist` to
  the Linux side during setup, and again when the daemon reports a
  registration refusal that a refreshed identity can fix. The identity never
  leaves the pair.
* **Anisette**: the helper serves anisette headers to the paired daemon
  (what `.local/bin/imessage-anisette` over ssh does today), so no Remote
  Login is needed on the Mac. The Mac still has to be on for logins and
  re-registrations; messaging itself does not need it.
* **Contacts, photos, pins, backgrounds, history**: the helper uses the
  Contacts framework (one normal permission prompt instead of Full Disk
  Access for the address book), reads the pinning plist, and, with Full
  Disk Access granted once to the helper, exports `chat.db` history and
  backgrounds. It pushes changes to the paired Linux side (Contacts and
  pin changes trigger a sync automatically; a manual Sync button remains).
  The Linux side keeps the `imsg sync-mac` logic; the transport changes from
  ssh+tar to the paired connection with the same payload.
* **Distribution**: Swift/SwiftUI, Developer ID signed, notarized, Sparkle
  or a plain download; Homebrew cask. Separate repository or `mac-helper/` in
  this one; the protocol (pairing, payloads) documented in `docs/HELPER.md`
  so the ssh tools stay a supported fallback.

## Non-goals / known limits

* A phone number of your own without an iPhone on the account (iPhone-class
  validation data). The number an iPhone linked is registered (0.1.12).
* FaceTime, FindMy (rustpush supports some; not wired). SMS/RCS through the
  iPhone (Text Message Forwarding) is wired (after 0.1.11; docs/DAEMON.md).
* Apple Contact Key Verification must be disabled on the account.
