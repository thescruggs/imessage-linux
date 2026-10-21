# Windows port — plan

Status: **W0, W1 and W2 done, W3 in code** (2026-10-20): the workspace
compiles for Windows, the platform layer is in, clients reach the daemon
peer-to-peer over `nonce-tcp`, and `imsg setup` / `imsg service` /
file logging cover a first real run; W3's exit test (a live Windows run)
is next. Drafted 2026-10-18 from the 0.1.13 tree.
Goal: the same three clients (`imsg`, `imsg-tui`, kmessages) talking to the same
`imessaged` engine, running natively on Windows 10/11 (x86_64 first, ARM64
later), installed by a normal installer, with no WSL.

## TL;DR

* The **protocol core ports cheaply.** rustpush, open-absinthe (unicorn runs
  the x86_64 `IMDAppleServices` image without depending on the host OS),
  rusqlite (bundled), reqwest/rustls and tokio all build on
  `x86_64-pc-windows-msvc`. Most of the work is the **Linux glue around it**:
  D-Bus, Secret Service, systemd, XDG paths, Unix file modes, `sh`/`ssh`/`xdg-open`.
* **Recommended order:** platform layer → IPC transport → daemon + CLI + TUI on
  Windows (usable product) → kmessages via KDE Craft → installer.
* **Biggest open question:** one Mac `hwconfig` registered from both Linux and
  Windows at the same time (see Risks). Settle it before Phase W3.

## What is Linux-specific today (audit of 0.1.13)

| Area | Where | Linux mechanism | Windows replacement |
|---|---|---|---|
| IPC | `imessaged/src/dbus.rs`, `imsg-client`, kmessages `daemonclient.*` (QtDBus) | zbus 5 on the session bus, `org.imessage.Daemon1` | Same D-Bus interface, **peer-to-peer over `nonce-tcp:` on 127.0.0.1** (zbus and libdbus/QtDBus both speak it; no bus daemon). The address and nonce file live in `%LOCALAPPDATA%\imessaged\` (user-only ACL). Fallback: ship `dbus-daemon.exe` (KDE Craft has it) and keep the session bus. |
| Secrets | `imessaged/src/keyring.rs` (`secret-service` crate + 0600 key file fallback) | Secret Service / KWallet | Windows Credential Manager (`keyring` crate, `windows-native`) or DPAPI `CryptProtectData` for the state key; the file fallback becomes DPAPI-wrapped, not mode bits |
| File modes | `state.rs`, `keyring.rs`, `hub.rs` (`PermissionsExt`, `OpenOptionsExt`, `O_NONBLOCK`, 0600/0700) | Unix permissions | `#[cfg(windows)]` path: rely on the per-user `%LOCALAPPDATA%` ACL, optionally set an explicit owner-only DACL; the FIFO guard in `copy_private` becomes a `metadata().is_file()` check (there are no FIFOs on Windows) |
| Paths | `imessage-common/src/paths.rs`, `open-absinthe/src/nac.rs` (`XDG_*`, `~/.local/...`) | XDG | `directories` crate: state → `%LOCALAPPDATA%\imessaged`, data → `%LOCALAPPDATA%\imessage-linux`, config → `%APPDATA%`. Centralise; nac.rs must use `paths.rs` instead of its own XDG lookup |
| Service | `packaging/` systemd user unit; `imsg/src/local.rs` calls `systemctl --user` | systemd user service, restart-on-failure | Per-user **Scheduled Task "at log on"** (restart on failure) created by the installer; `imsg` gets a `ServiceManager` trait (systemd / Task Scheduler / launchd later). *Not* a Windows Service: secrets and the keystore are per-user |
| Child processes | `engine/anisette.rs` and `imsg/src/local.rs` run `sh -c` | POSIX shell | Spawn the program directly (argv, no shell); `ssh` is Windows' built-in OpenSSH client (`C:\Windows\System32\OpenSSH\ssh.exe`) |
| Open/clipboard | `imsg-tui/src/app.rs` (`xdg-open`, `wl-copy`/`xclip`) | desktop tools | `open` crate (ShellExecute) + `arboard` |
| Signals | `imsg/src/main.rs` `SIGPIPE`; daemon SIGTERM | POSIX signals | `cfg(unix)` only; on Windows handle `ctrl_c` / `CTRL_CLOSE` and a D-Bus `Shutdown` method |
| Contacts | `kpeoplevcard` dir, KPeople in kmessages (already optional in CMake) | KPeople | Same vCard folder under `%LOCALAPPDATA%`, read by a small built-in resolver (kmessages already builds without KF6People, but then shows handles only), filled by `sync-mac` or **iCloud CardDAV** (PLAN.md, "Contacts over iCloud CardDAV"), which is the better source on Windows since it needs no Mac |
| Notifications | `kmessages.notifyrc` (KNotifications) | freedesktop notifications | KNotifications' Windows backend (toast via SnoreToast) in Craft builds; needs an AppUserModelID + Start-menu shortcut from the installer |
| Mac access | `imessage-common/src/ssh.rs`, `sync-mac`, `import-mac`, anisette over ssh | OpenSSH + ssh-agent | Windows OpenSSH works unchanged (agent: Windows `ssh-agent` service or 1Password's agent pipe). Long-term: the Mac companion app (PLAN.md "Future") removes ssh entirely |

Builds that need attention on MSVC:

* `openssl` with `vendored` (rustpush): needs Strawberry Perl + NASM, or
  switch rustpush's openssl to `openssl-sys` from vcpkg. Worth checking whether
  the openssl use can move to rustls/`aws-lc-rs` in a patch instead.
* `unicorn-engine` 2.1 (`arch_x86` only): CMake + MSVC; it builds, but pin the
  toolchain in CI. Windows Defender may flag a process that maps and executes
  foreign machine code (see Risks).
* rustpush's vendored `quinn-udp` already has `cfg(windows)` paths; `omnisette`'s
  macOS-only code is `cfg(target_os = "macos")` and stays off.

## Clients on Windows

| Client | Effort | Notes |
|---|---|---|
| `imsg` CLI | small | Once paths, service and process spawning are abstracted. `rpassword` works in Windows consoles |
| `imsg-tui` | small | crossterm/ratatui support Windows Terminal; replace `xdg-open`/clipboard. **First usable Windows client** |
| kmessages | medium | Qt6 + Kirigami 6 build on Windows through **KDE Craft** (which also packages it: NSIS/MSIX). QtDBus works peer-to-peer over `nonce-tcp`. KPeople is unavailable: use the built-in vCard resolver. QQC2DesktopStyle → Fluent/Windows style or Breeze |
| Native WinUI 3 app | large | Only if the Kirigami build looks or feels wrong on Windows. Would talk to the same D-Bus API (via a small Rust FFI to `imsg-client`). Not planned |

## Phases (each ends with the usual independent review, loop until clean)

**W0: CI and compile gate.** *Done 2026-10-20, except the first CI run.*
`.github/workflows/ci.yml` builds the workspace on `windows-latest` and
tests imessage-common, imsg-client and open-absinthe (the imessaged / imsg /
imsg-tui tests run there too, `continue-on-error` until they are known to be
Windows-clean); a Linux job runs the usual build, clippy and tests. openssl
stays vendored (Strawberry Perl + NASM in CI). The workflow has not run on
GitHub yet. Locally, `tools/windows/xcargo.sh` cross-checks from Linux with
cargo-xwin (`check` / `clippy --workspace --all-targets` are clean; `build`
links imsg and imsg-tui; `test` runs imessage-common, imsg-client and
open-absinthe under wine, including the NAC emulator: 338-byte request).
Crates that link rustpush need a Windows OpenSSL (`OPENSSL_DIR`) to link
locally. *Remaining:* the live open-absinthe test on Windows (CI step, needs
network; wine has no curl.exe), and making the imessaged / imsg integration
tests pass on Windows (`/` in paths, the D-Bus harness: W2).

Original plan: add a GitHub Actions `windows-latest` job that runs
`cargo build`/`cargo test` for `imessage-common`, `open-absinthe` and
`imsg-client`, then the rest as they come up. Gate every Unix-only call
behind `cfg`. Get rustpush building on MSVC (openssl decision above). Run the
open-absinthe live test on Windows: the validation-data sizes must match
Linux (338/698/517).

**W1: platform layer.** *Done 2026-10-20.* `imessage_common::platform`:
`fs` (`private_options`, `create_private_dir_all`, `mode` / `is_private` /
`set_mode`, `open_regular_file`: 0600/0700 and the FIFO guard on Unix; on
Windows the `%LOCALAPPDATA%` ACL, `TODO(W6)` explicit owner-only DACL),
`process::shell_command` (`sh -c` | `cmd.exe /d /s /c`; the configured
command is still one string run by a shell, not argv), `desktop`
(`xdg-open` / `wl-copy`... | `ShellExecuteW`, which shows the folder instead
of running a received program, and the Win32 clipboard via `clipboard-win`),
`dpapi` (Windows). `paths` gained Windows branches (`%LOCALAPPDATA%\imessaged`,
`\imsg`, `\imsg\cache`, `\imessage-linux`, `\kpeoplevcard`; `home_dir()` is
`%USERPROFILE%`); open-absinthe repeats the data-dir rule since it cannot
depend on imessage-common. Secrets: Windows has no Secret Service (the
`secret-service` dependency is Unix-only); `key_provider = auto` is the file
provider there and the file is `state.key.dpapi`, DPAPI-wrapped.
Service: `imsg doctor` reports "autostart not set up yet" on Windows instead
of the systemd / D-Bus activation checks. Signals: `SIGPIPE` reset is Unix
only; the daemon stops on Ctrl+C / Ctrl+Break / console close / logoff /
shutdown. Unix-only tests (symlinks, FIFOs, mode bits) are `cfg(unix)`.
*Remaining (moved to later phases):* the `ServiceManager` trait and Task
Scheduler (W3); Credential Manager instead of a DPAPI file (not needed so
far); spawning ssh without a shell (W3, if `cmd.exe` quoting bites).

Original plan (`imessage-common::platform`):
`paths` (directories), `secrets` (Secret Service | Credential Manager/DPAPI),
`private_file` (0600 write/copy | ACL), `spawn` (no shell), `service`
(systemd | Task Scheduler), `open_url`/`clipboard`. Linux behaviour must not
change, and the existing tests have to stay green on Linux.

**W2: IPC transport.** *Done 2026-10-20.* `imessaged --transport p2p`
(`IMESSAGED_TRANSPORT`; the Windows default, `session` stays the Linux
default and the Linux path is unchanged) serves `org.imessage.Daemon1` to
each client on its own D-Bus connection over TCP on 127.0.0.1 (ephemeral
port): 16 random bytes in `<state dir>\bus-nonce` (owner-only) that the
client must send first (constant-time compare, else the socket is closed),
then SASL ANONYMOUS (zbus' EXTERNAL over TCP on Windows looks up the peer's
process token, which fails for an elevated client; the nonce is the
authentication), and the address
`nonce-tcp:host=127.0.0.1,port=N,noncefile=...,guid=...` in
`<state dir>\bus-address`, both replaced atomically at start and removed
at shutdown. Signals go out on every live connection (`dbus/peer.rs`
`Peers::emit`; a client that does not take one within 5 s is
disconnected). `imessaged.lock` in the state directory keeps a second
daemon off it, checked before the engine starts. `imsg-client`'s
`Bus::Peer` re-reads `bus-address` at every connect (clear "not running"
errors pointing at `imsg service start`); `imsg` / `imsg-tui` use it by
default on Windows; the TUI notices the connection closing and reconnects
after a daemon restart. Tests run the transport on Linux (no dbus-daemon
needed): `crates/imessaged/tests/p2p.rs` (two clients + fan-out, wrong and
missing nonce, single instance, restart), the TUI reconnect, and the `imsg`
CLI through `IMESSAGED_TRANSPORT=p2p`. Contract: docs/DBUS.md, "Peer-to-peer
transport". kmessages (QtDBus) can use the same address later (W4).

Original plan: Daemon listens peer-to-peer on `nonce-tcp:` (Windows),
keeping the session bus on Linux; `imsg-client` reads the address from
`%LOCALAPPDATA%\imessaged\bus-address`. Multiple clients at once (kmessages
+ imsg + TUI) means one p2p connection per client; check signal fan-out
(the daemon's hub already broadcasts). Security: listen on 127.0.0.1 only,
nonce file user-only, no anonymous auth.

**W3: daemon on Windows.** *In code 2026-10-20; the live run is open.*
What a first real run needs:
* Apple files: `imsg setup` downloads and verifies `IMDAppleServices` and
  `cert-1.0.plist` into `%LOCALAPPDATA%\imessage-linux` (same URLs and
  hashes as bootstrap.sh, a test keeps them in sync); `imsg setup --from
  <dir>` copies them from a folder (e.g. the Linux box's
  `~/.local/share/imessage-linux`). The doctor's hints name it on Windows.
* Autostart: `imsg service install [--exe <imessaged.exe>]` creates the
  per-user Task Scheduler task `imessaged` (XML via `schtasks /Create /XML`:
  logon trigger repeating every 5 minutes with `IgnoreNew`, which restarts
  a crashed daemon, since Task Scheduler's restart-on-failure ignores exit
  codes; no time limit; interactive token, least privilege) and starts it;
  `start` / `stop` (= disable + end, so the repetition does not bring it
  back) / `status` / `uninstall`. On Linux the same subcommands are
  `systemctl --user` on the packaged unit. `imsg doctor` on Windows reports
  the task, whether the daemon listens, and the log path.
* No console: release `imessaged.exe` is `windows_subsystem = "windows"`
  (a console program started by the task would keep a console window or
  Windows Terminal tab open), logs to `%LOCALAPPDATA%\imessaged\logs\
  imessaged.log` (rotated at 10 MB at start; panics included), and
  `shell_command` spawns cmd.exe with `CREATE_NO_WINDOW` so anisette runs
  do not flash a console. Without a console there is no Ctrl+C / logoff
  event: the task's End or the session ending kills it (SQLite WAL, atomic
  state saves). A debug build keeps its console.
* Anisette over Windows OpenSSH: cmd.exe does not expand `~`, so
  `shell_command` expands a word-initial `~/` to `%USERPROFILE%` (quoted
  if it has spaces): the Linux `ssh -F ~/.ssh/config ...` line works
  unchanged. The doctor drops the sandbox / SSH_AUTH_SOCK advice on
  Windows, checks for `ssh.exe` and `BatchMode=yes`, and explains failures
  (OpenSSH Authentication Agent + `ssh-add`, or 1Password's agent; host
  key; name resolution). Child output parsing tolerates CRLF (`lines()`,
  serde_json).
* Not done: a same-identity warning (the doctor cannot see the other
  install, so it would be noise); a D-Bus `Shutdown` method for a
  graceful `imsg service stop`.

Original plan: `imsg hwconfig import`, `imsg login` (2FA),
register, APNs receive loop, send/receive/attachments, Text Message
Forwarding, link previews. Task Scheduler autostart + restart. Logs to
`%LOCALAPPDATA%\imessaged\logs` with the existing redaction. **Exit:**
imsg + imsg-tui are a working Windows iMessage client.

**W4: kmessages via KDE Craft.** Craft blueprint for kmessages, built without
KPeople and with the vCard resolver; Windows toasts; drag-drop/paste of files;
dark mode follows Windows. `Sync from Mac` keeps working through Windows ssh.

**W5: installer.** Craft → MSIX (or Inno Setup) bundling imessaged, imsg,
imsg-tui and kmessages; it creates the logon task, Start-menu shortcut and
AppUserModelID. The `IMDAppleServices` download stays a first-run step with
the license note, as `bootstrap.sh` does today. Code-signing certificate;
winget manifest. Later: the first-run wizard from PLAN.md "Future".

**W6: hardening.** Sleep/resume and network changes (APNs reconnect on
`WM_POWERBROADCAST`-style events), Defender false positives, ARM64
(unicorn emulating x86_64 code is architecture-independent; test under
Prism), and long-path/Unicode filenames for attachments.

## Risks and open questions

1. **Same Mac identity on two PCs.** The Linux box and the Windows box would both
   register with the Mac's serial/ROM/MLB. Running both at once may make Apple
   drop or flip-flop one registration, or flag the identity. Options: one
   active install per hwconfig (the doctor checks and warns), a second Mac's
   hwconfig for Windows, or treat Windows as a dual-boot of the same install
   (copy state and keystore). Decide before W3; test with a throwaway Apple ID first.
   *2026-10-20:* the user accepts the risk and runs the same hwconfig and
   Apple ID on Linux and Windows; watch for registration flip-flops.
2. **Defender / SmartScreen.** Running a Mach-O image under unicorn and
   talking to Apple private endpoints can look like malware heuristics. Mitigate
   with signed binaries, a stable publisher and a submission to Microsoft.
3. **openssl on MSVC** could cost more than expected; the fallback is vcpkg.
4. **Licensing.** SSPL code plus Apple's `IMDAppleServices` (downloaded, never
   bundled) apply unchanged on Windows; the MSIX/winget/Store policies need
   a check.
5. **Name.** "imessage-linux" running on Windows: keep the repo name, call the
   product something neutral in the installer (decide at W5).

## Rough size

W0–W3 (usable CLI/TUI client): roughly the size of Phases 2–3 of the Linux
build. W4–W5: comparable to Phase 4 plus packaging work. The engine
(rustpush/open-absinthe) needs no redesign.
