# Portable Windows client for `imessaged` — implementation plan

Status: planned (2026-10-22). Phase 0 in progress.

A different target from `docs/WINDOWS-PLAN.md` (which runs the *daemon* on Windows): this is a remote client that talks to `imessaged` on a Linux host over Tailscale and runs from a USB stick.

**Decisions (2026-10-22):** unsigned exes run from removable media on the work PC; the work PC's Tailscale install is sanctioned, so direct mode is the daily driver and embedded tsnet (Phase 3) is a resilience feature; the Slint royalty-free desktop licence is acceptable.

## Recommendation (summary)

1. **Transport: new sidecar crate `imsg-bridge` on vash** (Rust, axum + WebSocket). It is an ordinary D-Bus client of `org.imessage.Daemon1` and re-exposes the same JSON payloads over HTTP plus one WebSocket event stream, **bound only to vash's Tailscale IP (100.64.0.10:7227)**. Auth = Tailscale identity (LocalAPI whois, verified working unprivileged on vash) **and** a per-device bearer token. The daemon is not touched. Attachments/photos are streamed by the bridge (only paths the daemon returned and that lie under `~/.local/state/imessaged`); uploads go to a private temp dir, then `SendFile`.
2. **Client: new Rust crate `imsg-app` using Slint** (winit + software renderer, Fluent style) plus a shared crate `imsg-remote` (typed bridge client reusing `imessage_common::model`). Single static `.exe`, cross-built on vash with the existing mingw pipeline (`tools/windows/build-release.sh`, `x86_64-pc-windows-gnu`). No WebView2, no GPU needed (works over RDP/VMs). Also runs on Linux for development.
3. **No Tailscale needed on the host: embed tsnet** via a small Go c-archive (`crates/imsg-tsnet`), with an ephemeral, pre-authorized, tagged (`tag:imsg-portable`) reusable auth key and in-memory state, so nothing Tailscale-related persists on the stick. Cargo feature; "direct" mode (host Tailscale) is the Phase-2 default. The work PC `ipi-111jjs3` is already on the tailnet, so a usable client exists before tsnet lands.
4. **Portability:** everything next to the exe (`imsg-app.toml`, `secrets.bin`, `cache\`, `logs\`). Secrets encrypted with argon2id + AES-256-GCM under a passphrase. No message cache on disk by default. Tray-balloon notifications (no AUMID/registry). "Open with" temp files on the stick, wiped on exit.
5. **Lost stick** = two independent cuts: revoke the Tailscale auth key / ephemeral node, and `imsg-bridge token revoke <name>` on vash. The bridge never exposes `Login`, `Logout`, `Shutdown`, `ImportMessages`, `PurgeInvalidChats`, `ImportChatBackground`.

## Codebase findings

- Workspace: `imessaged`, `imsg` (CLI), `imsg-client` (zbus proxy), `imsg-tui`, `imessage-common`, `open-absinthe`, `kmessages/` (Qt/Kirigami).
- **Daemon is D-Bus-only.** No HTTP/WS API exists. Existing `docs/WINDOWS-PLAN.md` covers running the *daemon* on Windows (nonce-tcp p2p D-Bus on 127.0.0.1), not a remote client.
- API contract: `docs/DBUS.md`. Payloads are JSON strings of `imessage_common::model` types; 9 signals (`MessageReceived`, `MessageUpdated`, `ChatUpdated`, `Typing`, `ConnectionStateChanged`, `TwoFactorRequired`, `BackgroundError`, `HandleProfileUpdated`, `SmsForwardingChanged`) → cheap passthrough bridge.
- **Everything file-related crosses the bus as a local path** (attachments, profile photos, chat icons, backgrounds, SendFile input) — the main reason a bridge with a file layer is required rather than tunnelling D-Bus.
- Signal pattern to mirror: `crates/imsg-tui/src/backend.rs`. Contacts: vCards in `~/.local/share/kpeoplevcard` (830 files), parser in `crates/imsg-tui/src/contacts.rs`.
- Test harness: `crates/imsg-tui/tests/daemon.rs` (MockEngine on a private dbus-daemon).
- Build infra on vash: mingw gcc, rustup windows targets, wine. **No Go** (needs `pacman -S go`).
- `kde_imessage` is a BlueBubbles client — design reference only, no code reuse.

## 1. Transport

| Option | Verdict |
|---|---|
| **A. Sidecar `imsg-bridge` (D-Bus → HTTP/WS) on tailscale IP** | **Recommended.** Zero daemon changes, testable with MockEngine, restartable, can serve files. |
| B. Network API inside `imessaged` | Puts a listener in the process holding Apple keys; dep/sandbox bloat. No. |
| C. Expose nonce-tcp D-Bus over tailnet | Weak auth, can't move files. No. |
| D. SSH tunnel only | Needs ssh + key on stick, still no file layer. Manual fallback only. |
| E. gRPC | Duplicates the model schema for no gain. No. |

**Bridge design:**
- systemd user unit `imsg-bridge.service` (After=imessaged), config `~/.config/imsg-bridge/config.toml`, state `~/.local/state/imsg-bridge/`.
- Binds tailscale v4/v6 only, retries every 5 s if interface absent. Never 0.0.0.0.
- Auth (both required): LocalAPI whois (login ∈ `allowed_logins` or tag ∈ `allowed_tags`) + bearer token (SHA-256 stored; `token new|list|revoke`; pairing string `imsg+bridge://100.64.0.10:7227/#<token>`; constant-time compare; 1 s failure delay).
- HTTP v1 routes: status, chats, messages (paged), search, send text / new chat, file upload (≤100 MiB), react, edit, unsend, typing, read, handle lookup, pins, profiles, SMS, 2FA code; attachment/photo/icon/background download with Range/ETag; contacts + contact photos.
- D-Bus errors → HTTP status + `{error, message}` via `imsg_client::map_error_name`.
- `GET /v1/events` WebSocket: all 9 signals + `daemon lost/connected` + `hello` snapshot; 30 s ping; slow readers dropped; reconnect = resync.

## 2. Client stack

| Candidate | Assessment |
|---|---|
| **Rust + Slint (software renderer)** | **Recommended.** Works over RDP/VMs, native-ish Fluent look, cross-builds, runs on Linux. |
| Rust + egui | glow needs OpenGL; wgpu is heavy. Second choice. |
| Tauri | WebView2 dependency + writes to %LOCALAPPDATA%. No. |
| Qt portable (kmessages) | 60–120 MB DLLs, KDE deps missing on Windows. No. |
| `imsg-tui --remote` | Cheap bonus in Phase 4. |

- Split zbus-free types out of `imsg-client` (or new `imsg-types`) so the app doesn't pull zbus.
- `imsg-remote`: reqwest + tokio-tungstenite, `RemoteBackend` with the same `DaemonEvent` enum as the TUI.
- `imsg-app` v1 scope: chat list, transcript (bubbles, receipts, tapbacks, replies, inline images), composer (attach, paste image, typing), new message w/ contact suggestions, search, 2FA prompt, settings, connection status.
- Build: `build-release.sh --portable` → `dist/windows/portable/imsg-portable-<ver>.zip`. ~12–18 MB (≈40 MB with tsnet).

## 3. USB-stick portability

```
IMSG\
  imsg-app.exe
  imsg-app.toml        bridge URL, UI prefs, notifications.show_text = false
  secrets.bin          argon2id + AES-256-GCM: bridge_token, ts_auth_key, ts_hostname
  cache\thumbs\        optional, off by default, bounded
  cache\open\          temp "open with" copies, wiped on exit/start
  logs\                only if enabled, redacted
  README-portable.txt
```
- Passphrase every start; optional DPAPI "remember on this PC" (off by default — leaves a trace).
- No message cache by default; SQLCipher cache as a Phase-4 option.
- tsnet state in memory; ephemeral node per launch.
- Notifications: tray balloons (`Shell_NotifyIcon`), taskbar flash, sender-only by default.
- App writes nothing to the host; unavoidable OS traces (Prefetch, UserAssist, Defender logs, tray-icon prefs) documented — they show the app ran, not content.
- SmartScreen: no Mark-of-the-Web on FAT/exFAT copies; VirusTotal each release; signing optional. **Real blocker would be AppLocker/WDAC.**

## 4. Security

- Tailnet-only bind + whois + token. WireGuard-encrypted; plain HTTP inside tailnet OK; `tailscale serve --https` later.
- ACL:
  ```json
  "tagOwners": { "tag:imsg-portable": ["autogroup:admin"] },
  "acls": [
    { "action": "accept", "src": ["tag:imsg-portable"], "dst": ["vash:7227"] },
    { "action": "accept", "src": ["autogroup:member"],  "dst": ["*:*"] }
  ]
  ```
  Tagged node reaches only the bridge port. Auth key: reusable, ephemeral, pre-authorized, tagged, 90-day expiry.
- Lost-stick runbook: revoke key/node → `imsg-bridge token revoke` → check access log.
- Bridge least privilege: reduced method scope, upload limit, path containment, throttling, access log without bodies, systemd hardening.
- Corporate egress: tsnet uses 443 (control + DERP), honours system proxy; NTLM-auth proxies are the risk.

## 5. Phases

**Phase 0 — spikes (1–2 days)**
1. Install Go; tsnet c-archive linked into a Rust hello exe; run from stick on work PC; join as ephemeral node and reach vash.
2. Slint hello (software renderer) from the stick and over RDP.
3. Check work-PC policy: unsigned exe from removable media, `Get-AppLockerPolicy -Effective`.
*Done:* all pass, or switch to Plan B.

**Phase 0 log**
- 2026-10-22, spike 2 (Slint): Slint 1.18.1 with `backend-winit` + `renderer-software`, Fluent style, cross-built for `x86_64-pc-windows-gnu` → 9.7 MB release exe (opt-level s, LTO, strip); imports only system DLLs (UCRT `api-ms-win-crt-*`, user32, gdi32, dwrite, uiautomationcore, …). The first build **panicked at start-up under Wine** in `i-slint-renderer-software/fonts/systemfonts.rs` (no system default font found). Fix: **embed the UI font** (`import "fonts/NotoSans-Regular.ttf"` + `default-font-family`), which also removes the dependency on the host's fonts. `imsg-app` must ship an embedded font (Noto Sans, OFL; add emoji/CJK fallback later). With the font embedded, the exe runs under Wine without errors.
- 2026-10-22, spike 2 on the work PC: ran from a USB stick at the console, downloaded as a GitHub pre-release; no SmartScreen, AppLocker or other policy prompt; window rendered with clean text, input and Send worked. **Passed.** Not yet tried over RDP (low risk with the software renderer; retest when convenient).
- Spike 3 (work-PC policy): effectively answered by the above: an unsigned exe from removable media runs.

**Phase 1 — `imsg-bridge` (~1 week)**
Crate, token CLI, whois, routes, file serving + containment, uploads, contacts (parser moved to `imessage-common` + PHOTO), WS events, systemd unit, PKGBUILD, docs, MockEngine integration tests.
*Done:* from `ipi-111jjs3` curl lists chats, attachments download, websocat sees live messages, unauthenticated = 401/403, tests green, daemon unchanged.

**Phase 2 — `imsg-remote` + `imsg-app`, direct mode (~2–3 weeks)**
Full v1 UI, portable mode, secrets, pairing, tray notifications, portable zip, Linux screenshot tests.
*Done:* a week of daily use from the work PC; Process Monitor shows nothing written outside the stick; only system DLL imports; VirusTotal clean.

**Phase 3 — embedded tsnet (~1 week)**
`crates/imsg-tsnet` (Go module, build.rs, FFI start/status/stop, nonce-guarded loopback forwarder), `tsnet` feature + UI toggle.
*Done:* Windows VM with no Tailscale + proxy-only egress connects and works; node disappears after close; revoked key blocks start.

**Phase 4 — hardening/polish (ongoing)**
Runbook test, key/token rotation UI, **HEIC→JPEG on the bridge**, sleep/resume reconnects, SQLCipher cache, WinRT toasts opt-in, `imsg-tui --remote`, signing, CI cross-build.

**Plan B (if cgo cross-linking is fragile):** ship official signed `tailscaled.exe`/`tailscale.exe` on the stick, run userspace-networking with in-memory state and a SOCKS5 proxy; client just uses the proxy.

## Building and running the portable app (Phase 2)

Crates: `imsg-remote` (typed client for docs/BRIDGE.md: HTTP calls returning the `imessage_common` types, streamed up/downloads, the event stream with 1 s → 30 s reconnect backoff and `Connected` after every reconnect, pairing-string parser) and `imsg-app` (Slint UI; `src/state.rs` holds the UI-free logic and its tests).

```console
# Linux, for development (pairing screen on first start)
$ CARGO_TARGET_DIR=target/app cargo run -p imsg-app
$ IMSG_APP_DIR=/tmp/imsg-app-test CARGO_TARGET_DIR=target/app cargo run -p imsg-app   # throwaway data folder

# Windows exe + zip (mingw cross build, same environment as build-release.sh)
$ tools/windows/build-portable.sh            # → dist/windows/portable/IMSG/ and imsg-portable-<version>.zip
$ tools/windows/build-portable.sh --smoke    # … then 15 s under wine (exit 124 = still running)
```

Copy `dist/windows/portable/IMSG` (or unzip the zip) to the stick and start `imsg-app.exe`. The `portable` marker file in that folder keeps everything next to the exe; `README-portable.txt` there covers first start, the lost-stick runbook and the OS traces Windows keeps anyway.

Data folder: `IMSG_APP_DIR` if set; else the exe's folder if it holds `imsg-app.toml` or `portable`; else `%LOCALAPPDATA%\imsg-app` (Windows) / `~/.config/imsg-app`. `secrets.bin` = magic `IMSGSEC\0`, version, argon2id parameters (256 MiB, t = 3, p = 1 in release builds; 8 MiB in debug builds, stored in the header so either reads either), salt, nonce, then AES-256-GCM of `{bridge_url, token}` with the header as AAD; written atomically. `cache\open\` (attachments handed to other apps) is wiped at start and exit; nothing goes to `%TEMP%`. Logs only with `log = true`.

Debug builds only: `IMSG_APP_DEV_PASSPHRASE` (+ `IMSG_APP_DEV_PAIRING` for a first pairing) unlock / pair at start, for driving the UI headless (Xvfb) during development. `IMSG_APP_DEV_SCREENSHOT=<dir or file.png>` (+ `IMSG_APP_DEV_OPEN_CHAT=<chat id>`) waits until the chats (and that chat with its pictures) have loaded, writes a PNG of the 1120×780 window (`Window::take_snapshot`) and quits; `IMSG_APP_DEV_PICKER=<message id>` then opens that message's tapback picker before the shot; `IMSG_APP_DEV_THEME=light|dark|system` forces the colours (beats the setting); `IMSG_APP_DEV_OVERLAY=settings|new` opens that dialog.

Screenshots without a daemon: `cargo run -p imsg-remote --example fake_bridge` is a fake bridge with canned chats (the kmessages fixture: a 1:1 with a picture and a tapback, a group with reactions / edit / PDF / mention, an unverified sender with an unsent message, link previews and a failed send, an SMS chat, one pinned chat; stdin `recv <chat> <text>` / `typing <chat> on|off` inject live events; `FAKE_BRIDGE_TYPING=<chat id>` starts every event stream with that chat's contact typing). It prints its pairing string. `tools/app-screenshots.sh [out-dir]` builds both, starts the fake bridge (Bob typing) and writes a light and a dark set, `<out-dir>/light/*.png` and `<out-dir>/dark/*.png` (`THEMES=light` for one): `chats.png`, `bob.png`, `group.png`, `sms.png`, `links.png`, `dave.png`, `group-picker.png`, `settings.png` (default `target/app-screenshots/`, `SLINT_SCALE_FACTOR=1`) under `xvfb-run` (with `WAYLAND_DISPLAY` unset, or winit would open the windows on a Wayland desktop instead).

Tapbacks (kmessages ReactionChips / TapbackPicker): reactions are chips centred on the bubble's top outer corner (top-left of ours, top-right of theirs), one per kind with a count, blue when one of them is ours; hovering a chip shows who reacted beside it, clicking our own chip takes it back. The picker is a pill with the six tapbacks (ours checked; picking it again removes it, another replaces it: one per person) and an "Other emoji…" field (any non-ASCII text, Enter); it opens on right-click or double-click on a bubble or from the React button that appears beside a hovered bubble, and closes on Escape or a click outside. Not offered on SMS or unsent messages. The UI sends `POST …/react {kind, remove}` and waits for the bridge's `message_updated` (no optimistic chip). The software renderer draws no colour emoji, so the six classic tapbacks are white PNG icons tinted with `colorize` (`crates/imsg-app/assets/tapbacks/`, 48 px, regenerated by `tools/gen-tapback-icons.py`, Pillow); emoji reactions are text in Segoe UI Emoji on Windows (its glyphs have monochrome outlines) and the default font elsewhere. Layout notes: the chips and the senders label stay inside the message's own rectangle (a spacer above the bubble), since partial rendering does not repaint children drawn outside their parent; the senders label is not a `PopupWindow` because an open popup takes the next click (the chip's). A `changed` handler on a repeated item's property never ran in testing, so the screenshot hook opens the picker through a `Timer` whose `running` is bound to the request, after forcing one render.

Look (Phase 3, kmessages / Breeze; reference: `kmessages/src/qml/*.qml` and `kmessages/build/screenshots/`):
- **Theme**: light and dark palettes in `ui/theme.slint` (`Theme.dark` follows the std-widgets `Palette.color-scheme`; the Slint style is `fluent`, which picks light or dark Fluent widgets from the same setting). Setting "Colours" / `imsg-app.toml` `theme = "system" | "light" | "dark"` (default `system`). `AppWindow.set-theme-mode` sets `Palette.color-scheme`: `light` / `dark` pin it; `system` sets it to `unknown`, on which the Fluent palette reads Slint's own system colour scheme (winit reads the Windows app mode, `AppsUseLightTheme`, when the window is created and updates it on `ThemeChanged`; on Linux the desktop portal does), and `Theme.dark` reads the result back from `Palette.background`. So switching between the modes in Settings works live and in both directions, without winit's `Window::theme()` (which X11 does not implement). Contrast: body text ≥ 7:1 and secondary text ≥ 4.5:1 in both palettes (table in `theme.slint`); the selected chat row is white on #1D5FB8 (6.2:1) / #1F4E99 (7.7:1).
- **Bubbles** exactly as kmessages (user decision; overrides the contrast rule for bubble text only): sent iMessage #1982FC, SMS #34C759, white text; incoming = background tinted 11 % toward the text colour (#E7E7E7 light, #30302E dark) with the normal text colour. Radius min(18 px, h/2); a tail on the last bubble of a run (kmessages' carve: a bubble-coloured block whose outer edge is cut by a background-coloured rounded block), none when the message has a link card; 2 px between bubbles of a run, 10 px between runs; no per-bubble time (it appears beside a hovered bubble, with the React button); "Edited" in the accent colour under the bubble; Delivered / Read / Sent under the newest sent message, a bold red "Not delivered" under failed ones; "X unsent a message" centred; the reply quote is an outlined pill above the bubble.
- **Attachments and links** sit outside the text bubble (kmessages AttachmentView / LinkPreview): pictures with rounded corners baked into their pixels (`images::decode_picture`, 16 px; click opens, a hover button saves), other files as a bubble-coloured pill (file icon, name, size; click opens, the download button saves), link cards below the text in the bubble colour with the preview picture on top (top corners baked), bold title and site, or a globe icon without a picture.
- **Chat list**: toolbar "Messages" + new-message and settings icon buttons, search field, rows with a blue unread dot at the far left, a 40 px round avatar, the name, a pin icon by the time, a two-line snippet (or typing dots); "Pinned" / "Conversations" section headers only when something is pinned; the selected row is solid accent with white text. Avatars: shared photos clipped to a circle in their pixels (`images::circle_mask`; the software renderer cannot clip images), else a tinted circle with initials (or a person icon for bare numbers, a group icon for groups); the tint is a stable FNV-1a hash of the chat id / handle (`state::tint_index`, 8 colours per theme), so a person has the same colour in the list and in groups.
- **Groups**: sender name above the first bubble of a run and the sender's 24 px avatar (shared photo or initials) by its last bubble.
- **Transcript header**: chat title bold + participants; a typing bubble (three pulsing dots) at the end of the transcript.
- **Composer**: paperclip button, a pill text field (1 px border, 2 px accent when focused; placeholder "iMessage" / "Text Message"; grows to six lines, then scrolls; Enter sends, Shift+Enter breaks the line), paper-plane send button (blue, green in SMS chats).
- **Status footer**: a green / amber / red dot and "Connected as me@example.com" (the daemon's sender handle), "Bridge reconnecting…", or what is wrong with imessaged; notices on the right.
- **Icons**: white 48 px PNGs tinted with `colorize` (`crates/imsg-app/assets/icons/`, regenerated by `tools/gen-ui-icons.py`, Pillow). No `Path`/SVG (the `software-renderer-path` feature stays off).
- Not done: kmessages' chat details page ("i" button), its emoji picker in the composer, drop shadows (none in the software renderer), chat backgrounds.

Minimize / restore (Windows): with Slint 1.18.1's winit backend and software renderer the window could come back blank or stale after a minimize. winit sends no `Occluded` on Windows; a minimize is `Resized(0, 0)`, on which Slint marks the renderer occluded so that the next frame is a full repaint (the buffer age softbuffer reports is otherwise 1 = reuse). Any redraw while minimized (cursor blink, the 5 s tick, an incoming message) spends that full frame while the window is iconic: it lands in softbuffer's own bitmap (kept, same size) but not on screen, later frames are partial, and on restore nothing is dirty, so nothing is presented. Upstream is not fixed as of 1.18.1. `ui.rs` works around it in its winit event hook: on the restore `Resized` it bumps `repaint-nonce` (an invisible full-window rectangle whose colour flips between two fully transparent values; the software renderer's partial-rendering tracker sees the `background` change and marks the whole window dirty) and requests a redraw, so the next frame is rendered and presented in full. The same bump happens on every `Focused(true)` (lock screen, RDP reconnect, Win+D). `RedrawRequested` is deliberately not dropped while minimized: Slint's pending-redraw flag would stay set and its frame-throttle timer would re-request a redraw every refresh interval until the restore. Manual check on Windows: focus the composer, minimize, wait > 2 s, restore; also Win+D, lock / unlock, RDP disconnect / reconnect.

Known limits (follow-ups): no colour-emoji font is embedded (Noto Color Emoji is 10 MB; Windows renders emoji glyphs from system fonts only where Slint falls back); HEIC pictures and videos show as file rows (bridge-side conversion is Phase 4); no paste-image, reply / edit / unsend actions in the UI yet (the client library has them); emoji tapbacks in fonts without outline glyphs show as an empty chip (on Linux: anything only Noto Color Emoji has, e.g. 🔥); message text typed into Slint widgets cannot be zeroized; the tray icon is the only notification channel (no toasts). Live theme switching from the Windows app-mode setting (`ThemeChanged`) is implemented but only verified on Linux (the desktop portal's setting, and Settings > Colours switched live under Xvfb); without a desktop portal Slint knows no system theme, so `system` means light there. Whether the Windows title bar follows a forced theme is unverified. Slint ≥ 1.15 divides by the monitor refresh rate in its Windows/X11 frame throttle, so a display reporting 0 Hz (seen with Wine on a bare Xvfb) panics; real displays report a rate.

Phase 2 log
- 2026-10-22: `imsg-remote` + `imsg-app` implemented; release exe 14 MB (opt-level s, fat LTO, stripped), imports only system DLLs; runs under Wine; tested end to end on Linux against `imsg-bridge` on 127.0.0.1 with the real daemon (read-only).
- 2026-10-22: fake bridge example + `tools/app-screenshots.sh`; minimize / restore redraw workaround (above).
- 2026-10-22: tapbacks: chips, picker, React button, icons (above); `ReactionKind::arg` shared with imsg-tui.
- 2026-10-22: Phase 3 look (above): light + dark theme following the system, kmessages bubbles / tails / list / composer / status footer, round avatars, link cards, typing bubble; screenshots in light and dark.

## Files

New: `crates/imsg-bridge/`, `crates/imsg-remote/`, `crates/imsg-app/`, `crates/imsg-tsnet/`, `packaging/systemd/imsg-bridge.service`, `packaging/windows/README-portable.txt`, `tools/windows/build-portable.sh`, `docs/BRIDGE.md`, `docs/PORTABLE-WINDOWS.md`.

Changed: workspace `Cargo.toml`/lock, `imessage-common` (+contacts), `imsg-tui/src/contacts.rs` (shim), `imsg-client/src/lib.rs` (split types), `tools/windows/build-release.sh`, CI, PKGBUILD, README, CHANGELOG, `docs/WINDOWS-PLAN.md`. **Daemon: no changes.**

## Risks / open questions

Items 1, 2 and 9 were answered yes on 2026-10-22 (see Decisions above).

1. Can the work PC run an unsigned exe from USB (AppLocker/WDAC)? Decides everything.
2. Is the Tailscale install on `ipi-111jjs3` sanctioned and reliable? If yes, tsnet is resilience, not a prerequisite.
3. cgo + mingw cross-link fragility → Phase-0 spike / Plan B.
4. Unprivileged LocalAPI whois could tighten in future → `tailscale set --operator=qcv123`.
5. Tailscale IP / tailscaled restarts → bind-retry + Restart=always; verify after reboot.
6. Message content visible on a work screen → conservative notification defaults.
7. Auth key expires ≤90 days → renewal routine; app shows days left.
8. NTLM-auth corporate proxies may block control plane.
9. Slint royalty-free desktop licence alongside SSPL — OK?
10. HEIC photos need bridge-side conversion (Windows can't decode natively).

## Critical files
- `docs/DBUS.md`
- `crates/imsg-client/src/lib.rs`
- `crates/imsg-tui/src/backend.rs`
- `crates/imessaged/src/dbus/peer.rs`
- `tools/windows/build-release.sh`
- `crates/imsg-tui/tests/daemon.rs`
