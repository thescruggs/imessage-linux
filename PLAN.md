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

## Next (planned 2026-10-17, not started): kmessages pinned chats, keyboard, emoji suggestions

Three features requested for kmessages, in the order they should be built.
Each is its own PR-sized step with tests, docs and a changelog line; the
usual loop (Opus implements, Fable reviews) applies.

### A. Pinned conversations at the top (as Messages.app)

Messages.app shows *pinned* conversations above the list. The Mac keeps the
pins in `~/Library/Preferences/com.apple.messages.pinning.plist`
(`pD.pP`: an ordered array of handles such as `+1205…` or `bob@x.com`, group
chat GUIDs for groups; `pR`/`pV` are format versions). Contacts carry no
"favorite" flag in vCards, so pins are the source of truth, like on the Mac.

1. **Daemon / store** (`crates/imessaged`): `chats.pinned_at INTEGER` and
   `chats.pin_order INTEGER` (migration 0006). `Chat` JSON gains
   `pinned: bool`, `pin_order: int|null`. D-Bus `SetChatPinned(chat_id,
   pinned, order)` and `ReorderPins(chat_ids_json)`; `ChatUpdated` is
   emitted as for other chat changes. Local only (not synced to Apple;
   Messages in iCloud pin sync is CloudKit and out of scope).
2. **CLI**: `imsg chat CHAT --pin / --unpin`, `imsg chats` marks pinned rows
   (📌 / `pinned` column in `--json`). `imsg import-mac HOST --pins`
   reads the pinning plist over the existing ssh path (as
   `--backgrounds` does; `plutil -convert xml1 -o -` on the Mac), maps
   each entry to a chat id (handle → 1:1 `mailto:`/`tel:` id; group GUID
   → `group:` id), reports unknown ones, `--dry-run` supported; order is
   kept.
3. **kmessages**: `ChatListModel` sorts pinned chats first by `pin_order`,
   then the rest by time as now; a "Pinned" section header (Kirigami
   `ListSectionHeader`) when any exist; `ChatListDelegate` gets a context
   menu (right-click / long-press) with Pin / Unpin (and later Mute);
   the chat details page gets the same toggle. Drag-to-reorder of pins is
   a stretch goal (use `ReorderPins`). Tests: model ordering with mixed
   pins (tst_models), a `pinned-chats` screenshot (tst_screens), fake
   daemon `SetChatPinned`.

### B. Keyboard: Ctrl+N and picking recipients with ↑ ↓ Enter

`Ctrl+N` already exists (`Main.qml`, `StandardKey.New`, since 0.1.10) and
opens the new-message page. Verify it works from every page, including when
the composer or search field has focus, and add it to the Settings/About
shortcut list and docs. Then the new-message page:

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
   text area lists the emoji; `Tab`/`Enter` inserts the highlighted one
   replacing the word (`:snake:` form) or appending it after the word
   (plain form, so "snake" becomes "snake 🐍"); `Left`/`Right` or `Up`/`Down`
   move the highlight, `Escape` dismisses for that word. Off by default or
   on? On, with a Settings toggle ("Suggest emoji while typing"). The
   existing EmojiPicker grid gets a search field backed by the same model.
4. Tests: model ranking; a screenshot with the popup open; the composer
   test types "snake" and checks the suggestion and insertion.

Order: A (daemon+CLI first, then the app), B, C. Release as 0.1.13.

## Non-goals / known limits

* A phone number of your own without an iPhone on the account (iPhone-class
  validation data). The number an iPhone linked is registered (0.1.12).
* FaceTime, FindMy (rustpush supports some; not wired). SMS/RCS through the
  iPhone (Text Message Forwarding) is wired (after 0.1.11; docs/DAEMON.md).
* Apple Contact Key Verification must be disabled on the account.
