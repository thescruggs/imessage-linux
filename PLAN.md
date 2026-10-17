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

## Non-goals / known limits

* Phone-number handle (needs an always-on iPhone relay). Email handle only.
* SMS/RCS forwarding, FaceTime, FindMy (rustpush supports some; not wired).
* Apple Contact Key Verification must be disabled on the account.
