# imessage-linux

A native Linux iMessage client (daemon, CLI and a KDE app) that talks to
Apple's servers directly. No Mac or iPhone relay stays running, and no
third-party server holds your login. The protocol engine is
[rustpush](https://github.com/OpenBubbles/rustpush), included as a pinned
submodule. Everything else is in this repo: the NAC (validation data)
emulator, the daemon, the CLI, and the setup tools.

See [PLAN.md](PLAN.md) for the architecture and phases, and
[docs/RESEARCH.md](docs/RESEARCH.md) for how iMessage registration works and
why a Mac is needed once.

## Status: 0.1.12, working on a live account

Version 0.1.12 is the current release (changes: [CHANGELOG.md](CHANGELOG.md)).
It logs in, registers and sends/receives iMessages on a real Apple ID:

* `crates/open-absinthe` runs Apple's `IMDAppleServices` under unicorn to
  produce registration validation data (Apple accepts both Intel and Apple
  Silicon identities).
* `imessaged` daemon: login/2FA, IDS registration, APNs loop, encrypted
  state, SQLite store, D-Bus API ([docs/DAEMON.md](docs/DAEMON.md),
  [docs/DBUS.md](docs/DBUS.md)); anisette from your own Mac over ssh.
* `imsg` CLI ([docs/CLI.md](docs/CLI.md)), including a one-time history
  import from the Mac.
* `kmessages` KDE app ([docs/KMESSAGES.md](docs/KMESSAGES.md)) with
  contact names and photos, notifications and transcript backgrounds.
* Contacts' **shared names and photos** (Messages "Share Name and
  Photo", fetched from iCloud) and **group photos**, without a Mac: shown
  in the app wherever your address book has no name or photo, and by
  `imsg chat` / `imsg profile`.
* **Text messages (SMS / MMS / RCS) through your iPhone** ("Text Message
  Forwarding", like a Mac): green bubbles, sent by your iPhone; see
  [Text messages](#text-messages-sms-through-your-iphone).

Still missing: sending transcript backgrounds (they are received only),
publishing your own shared name and photo, and
group admin operations.
On Arch / CachyOS, see "Install on Arch" below for a package.

## Read this first: risks and limits

* **Apple ToS.** You would be using iMessage from an unofficial client.
  The realistic worst case is suspension of your Apple ID. Don't send bulk
  messages.
* **Use your own Mac's identity, for your own account only.** Apple blocks
  Mac identities that are shared across many users or used for spam. The
  block can be temporary (1–2 days) or permanent.
* **Your addresses.** The daemon registers your Apple ID email addresses
  and, if an iPhone signed in to the account has linked it, your phone
  number (like a Mac; `imsg status` lists the handles). Messages still go
  out from an email address unless you set `sender_handle` to the number.
  SMS can be relayed by the iPhone (Text Message Forwarding, below) while
  it is on and online.
* Turn off **Contact Key Verification** on the account.

## One-time step on a Mac

Registration needs a cryptographic proof minted from a real Mac's hardware
identity. On your Mac (Intel or Apple Silicon, normal user account, nothing
to install), run:

```sh
bash tools/mac-hwinfo.sh            # writes ./hwconfig.plist (mode 600)
bash tools/mac-hwinfo.sh --dry-run  # preview the commands; reads nothing
```

Copy `hwconfig.plist` to the Linux machine over a private channel. It is a
device credential, so don't commit or share it. After this the Mac is never
contacted again.

## Install on Arch (CachyOS, Manjaro, …)

`packaging/arch/PKGBUILD` builds one package with `imessaged`, `imsg`,
`kmessages`, the systemd user unit, the D-Bus activation file and the
setup tools:

```sh
git clone https://github.com/thescruggs/imessage-linux.git && cd imessage-linux/packaging/arch
makepkg -si                      # or: sudo pacman -U imessage-linux-0.1.12-1-x86_64.pkg.tar.zst
```

(`makepkg --nocheck` skips the test suite. From a local checkout, before
a tag exists: `packaging/arch/build-local.sh OUTDIR -- -f`.) Then, as
your user:

```sh
/usr/share/imessage-linux/tools/bootstrap.sh   # downloads Apple's IMDAppleServices + cert
# on the Mac: bash mac-hwinfo.sh (from /usr/share/imessage-linux/tools), copy hwconfig.plist back
imsg hwconfig import /path/to/hwconfig.plist
/usr/share/imessage-linux/tools/mac-anisette-install.sh you@mac.local   # see "Anisette"
systemctl --user daemon-reload
systemctl --user enable --now imessaged
imsg doctor
imsg login --apple-id you@icloud.com
imsg sync-mac you@mac.local    # contacts + pinned chats from the Mac; see "Contacts"
```

The packaged unit runs `/usr/bin/imessaged`. A unit you installed by
hand in `~/.config/systemd/user/imessaged.service` takes precedence over
it; remove that file (and `~/.cargo/bin/imessaged`) to switch to the
package. The installed bootstrap script only downloads Apple's files
(there is no checkout to set up); the files land in
`~/.local/share/imessage-linux/` exactly as from a source checkout.

## Setup on Linux

Requirements: Rust 1.88+, git, curl, cmake, a C compiler, pkg-config.

```sh
git clone https://github.com/thescruggs/imessage-linux.git && cd imessage-linux
tools/bootstrap.sh                  # submodules + patches + Apple downloads
# tools/bootstrap.sh --no-download  # submodules + patches only
# tools/bootstrap.sh --download-only  # Apple downloads only
cargo run -p imsg -- hwconfig import /path/to/hwconfig.plist
cargo run -p imsg -- doctor
```

`tools/bootstrap.sh` can be run any number of times. It:

1. initialises `third_party/rustpush` and its nested submodules. It rewrites
   `git@github.com:` URLs to https and skips the closed `open-absinthe` stub.
2. applies `patches/` (see [patches/README.md](patches/README.md)).
3. downloads Apple's `IMDAppleServices` (sha1 `e1181cca…bd13`, ~6.2 MB) and
   the public `cert-1.0.plist` (sha256-pinned; a test/`doctor` copy only,
   since rustpush fetches the cert from the IDS bag at runtime) into
   `${XDG_DATA_HOME:-~/.local/share}/imessage-linux/`. These are Apple's
   files, so they are never committed to this repo. Set
   `OPEN_ABSINTHE_BINARY=/path/to/IMDAppleServices` to use a copy elsewhere.

The imported identity goes to `${XDG_STATE_HOME:-~/.local/state}/imessaged/hwconfig.plist`
(mode 600). Override this with `--state-dir` or `IMESSAGED_STATE_DIR`.
Re-importing the identical file is a no-op. Importing a *different* file
refuses unless you pass `imsg hwconfig import --force <file>`, which replaces
it; a new identity means the account must register again.

## Anisette (login attestation)

Logging in to an Apple ID needs *anisette* headers, a device attestation
that macOS mints with its AOSKit/AuthKit frameworks. Until October 2026
the daemon took them from a public remote anisette v3 server
(`anisette_url`, default ani.sidestore.io). **Since 2026-10-13 Apple
rejects those** when the client presents itself as a Mac: login fails with
`This action cannot be completed at this time. (-80045)`. Headers minted by
a real Mac still work, so the daemon can ask your Mac for them over ssh:

```sh
tools/mac-anisette-install.sh you@mac.local   # builds tools/mac-anisette/imessage-anisette.m there
```

It installs `~/.local/bin/imessage-anisette` on the Mac (needs the Command
Line Tools: `xcode-select --install`), checks it from here, and prints the
line to put in `~/.local/state/imessaged/config.toml`:

```toml
anisette_command = "ssh -F ~/.ssh/config -o BatchMode=yes -o LogLevel=ERROR you@mac.local .local/bin/imessage-anisette"
```

Restart `imessaged` and `imsg doctor` should report `anisette: command ok`.
The Mac is contacted only when the daemon authenticates with Apple: at
`imsg login` and at the rare silent re-login after Apple drops the IDS
auth. Messages never go through it, so it can sleep in between (ssh must
be able to wake it, or run `imsg login` while it is awake). The `-F`
option is deliberate: the daemon runs sandboxed, and OpenSSH inside that
sandbox rejects the system `ssh_config.d` includes. `-o LogLevel=ERROR`
keeps login banners and known_hosts notices out of the daemon's journal.
The command must reach the same Mac that `hwconfig.plist` came from.
If that Mac is unreachable when the daemon needs a silent re-login, the
daemon shows `error: anisette source unreachable: ...` and keeps retrying
(every 15 minutes at most) until the Mac answers; no new password or 2FA
code is needed.

## Sending address

An account usually has several iMessage addresses (your iCloud address,
other Apple ID emails). By default the daemon sends new messages from the
first one Apple registered; `imsg status` shows it as `sends as`. Like
Messages.app's "Start new conversations from", it applies to *new*
conversations: replies go out from the address the other person wrote
to. To pick another, add it to `~/.local/state/imessaged/config.toml` and restart the
daemon:

```toml
sender_handle = "you@example.com"     # or IMESSAGED_SENDER_HANDLE / --sender-handle
```

It must be one of the registered handles (`imsg status`, `handles`);
otherwise the daemon logs a warning and keeps the default. Chats you are
already in keep the address they use (1:1: the one the other person wrote
to; groups: the one you are a member under).

An address of yours that this device is not registered for (your phone
number when no iPhone has linked it to the account, a second Apple ID's
email) makes messages you send from it look like someone else's (unread,
and a message from the phone to Bob would land in a "group" of Bob and your
phone). Tell the daemon about such addresses; `imsg status` shows them as
`also mine`:

```toml
other_handles = ["+15551234567"]     # or IMESSAGED_OTHER_HANDLES=a,b / --other-handle (repeatable)
```

They are never sent from. Messages already stored are not rewritten.

## Text messages (SMS through your iPhone)

Like a Mac, the daemon can send and receive SMS / MMS / RCS through your
iPhone ("Text Message Forwarding"). It needs your iPhone's number: the
first phone number in `other_handles` (above), or set it explicitly:

```toml
sms_relay_number = "+15551234567"   # or IMESSAGED_SMS_RELAY_NUMBER / --sms-relay-number
```

Then, once:

1. `imsg sms enable` (or **Settings → Text Message Forwarding → Enable**
   in kmessages).
2. On the iPhone: **Settings > Messages > Text Message Forwarding**, turn
   on this computer (listed under the Mac model name of your hardware
   identity).
3. `imsg sms status` says `active` once the iPhone confirms (or relays its
   first text).

Texts then arrive in the same chat as iMessages with that number (green
bubbles in kmessages, `[SMS]` in `imsg messages`). Sending to a number that
is not on iMessage goes out as a text automatically; `imsg send --sms
+1555… "text"` forces it. Texts have no tapbacks, edits, unsend or typing
indicators. The iPhone must be on and online; it sends each text from its
own number and confirms it (a failure shows as `failed`).

## Running the daemon

`imessaged` keeps the Apple connection, stores messages in
`~/.local/state/imessaged/messages.db` and serves the D-Bus API
`org.imessage.Daemon1` on your session bus (see [docs/DBUS.md](docs/DBUS.md);
internals in [docs/DAEMON.md](docs/DAEMON.md)).

Install it as a systemd user service (started on login, and on demand by
the first D-Bus call):

```sh
cargo install --path crates/imessaged            # -> ~/.cargo/bin/imessaged
install -Dm644 packaging/systemd/imessaged.service \
    ~/.config/systemd/user/imessaged.service
install -Dm644 packaging/dbus/org.imessage.Daemon1.service \
    ~/.local/share/dbus-1/services/org.imessage.Daemon1.service
systemctl --user daemon-reload
systemctl --user enable --now imessaged
journalctl --user -u imessaged -f                # logs
```

The unit runs `%h/.cargo/bin/imessaged`; edit `ExecStart` if you installed
it elsewhere (the Arch package ships a unit running `/usr/bin/imessaged`). It is hardened
(`ProtectSystem=strict`, home read-only except `~/.local/state/imessaged`,
`NoNewPrivileges`, `PrivateTmp`), so files you send must be readable and
received attachments land in `~/.local/state/imessaged/attachments/`.

Then use the `imsg` CLI (below), or talk to the API directly with
`busctl --user` / `gdbus` (examples in [docs/DBUS.md](docs/DBUS.md)).

Run it by hand with `imessaged` (`--help` for options); `--no-dbus` runs
headless (receive and store only). Only one instance can own the bus name.

On-demand D-Bus activation goes through systemd (the activation file has
`Exec=/bin/false`), so it only works on a systemd-managed session bus;
under `dbus-run-session` or in a container, start `imessaged` by hand
first. The very first activated call may also exceed the bus's 25 s
method timeout if the keyring shows an unlock prompt; just retry (or keep
the service enabled so it starts at login). Note that `Login` sends your
password over the session bus, where other processes running as you can
observe it (see [docs/DBUS.md](docs/DBUS.md)).

## Using the CLI

`imsg` talks to the running daemon over D-Bus (it does not contain the
protocol engine). Full reference, JSON shapes and exit codes:
[docs/CLI.md](docs/CLI.md).

```sh
cargo install --path crates/imsg                  # -> ~/.cargo/bin/imsg
imsg doctor                                       # setup + daemon checks
imsg login --apple-id you@icloud.com              # prompts for password and 2FA code
imsg status
imsg register                                     # re-register now (e.g. to pick up your phone number)
imsg chats                                        # conversations, newest first
imsg messages bob@example.com                     # a chat by handle, id or name
imsg chat bob@example.com                         # one chat's details (incl. background)
imsg chat bob@example.com --pin                   # pin it to the top (--unpin)
imsg sync-mac mac.local                           # contacts + pinned chats from your Mac
imsg send bob@example.com "hello from Linux"
imsg send alice@example.com,bob@example.com hi    # group
imsg send-file bob@example.com ~/Pictures/cat.jpg
imsg react bob@example.com <message-id> love
imsg watch                                        # live events until Ctrl-C
```

Every command takes `--json` (prints the daemon's JSON unchanged, for
scripts and `jq`) and `-q`. Exit codes: 0 ok, 1 error, 2 usage, 3 not
logged in, 4 two-factor code needed, 5 daemon not running, 6 not found,
7 temporary failure. For non-interactive login use `--password-stdin` /
`--code-stdin`; never put the password on the command line.

## KDE app (kmessages)

```sh
cmake -B kmessages/build -S kmessages -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build kmessages/build
./kmessages/build/bin/kmessages      # or cmake --install kmessages/build
```

Three-pane Kirigami UI styled after macOS Messages, with KNotifications
(inline reply) and a tray icon. Needs Qt >= 6.10 and KDE Frameworks 6.

Your phone number is not registered on Linux (see above), so what you send
from your iPhone's number shows up in kmessages as incoming unless the
daemon's `other_handles` lists it (see Sending address). Put that
number (and any other address of yours this computer does not send from)
into **Settings → My other addresses**: messages from them then raise no
notification and mark the chat read instead of leaving it unread. Text
messages relayed by your iPhone (see Text messages) show as green bubbles
and the composer says "Text Message"; **Settings → Text Message
Forwarding** turns the relay on or off.
**Keyboard.** Ctrl+N starts a new message, Ctrl+F searches the chat
list, Ctrl+Q quits. In the new message's To: field, Up / Down pick a
suggested contact or conversation, Enter adds it (or what you typed, when
nothing matches), Escape clears the field and, when it is empty, goes back.
In the composer Enter sends and Shift+Enter starts a new line.

**Emoji.** While you type, kmessages offers emoji for the word before the
cursor, like macOS: type `snake` (or `:snake:`) and 🐍 shows above the text
field; Tab, or Enter while the suggestions show, replaces the word with the
highlighted emoji, the arrow keys pick another one and Escape hides them.
Emoji you use come first next time. The emoji button's picker has a search
field. Turn the suggestions off in **Settings → Suggest emoji while
typing**. The keywords come from Unicode CLDR (Unicode License v3, see
`kmessages/data/emoji-keywords.LICENSE`).

Details, tests and screenshots: [docs/KMESSAGES.md](docs/KMESSAGES.md).

## Contacts

kmessages shows contact names and photos the way Messages.app does
("Alice Anderson" instead of `+1 (555) 010-1001`, "Alice, Bob & Carol" for
unnamed groups, photos in avatars and notifications). They come from
**KPeople**, KDE's contact aggregation (package `kpeople`; build option
`-DKMESSAGES_WITH_KPEOPLE=ON`, the default; without KPeople the app shows
handles). Handles match an address-book entry by email (case-insensitive)
or phone number (all digits, or the last ten, so `(205) 555-0100` matches
`tel:+12055550100`).

KPeople reads vCards (`*.vcf`, `*.vcard`) from
`~/.local/share/kpeoplevcard/` (`$XDG_DATA_HOME/kpeoplevcard`, subfolders
included; KDE Connect syncs your phone's contacts there too). Any vCard you
drop in shows up while kmessages runs. KAddressBook / Akonadi contacts work
automatically once Akonadi's KPeople plugin is installed (from
`akonadi-contacts`).

To copy your **Mac's** contacts and pinned conversations (re-run whenever
they change):

```sh
imsg sync-mac mac.local     # ssh host of the Mac; or set mac_host and run `imsg sync-mac`
imsg sync-mac --contacts    # contacts only (--pins: pins only; --dry-run: change nothing)
```

It reads the Contacts database over one ssh connection (read-only; no
Contacts.app, no privacy prompt; the ssh session needs Full Disk Access as
for `chat.db`) and writes one vCard 3.0 per person, photo embedded (JPEG,
256 px), as `~/.local/share/kpeoplevcard/mac-<uid>.vcf`. Re-running replaces
only the `mac-*.vcf` files (and rewrites only those that changed). The
same trip copies the Mac's pinned conversations: kmessages shows them at
the top; pins you set here (`imsg chat CHAT --pin`) are kept. Set the
daemon's `mac_host` (`mac_host = "mac.local"` in `config.toml`; `imsg
doctor` suggests it) to leave out the host. Details: [docs/CLI.md](docs/CLI.md),
`sync-mac`. `tools/mac-contacts-export.sh mac.local [OUTDIR]` still works
(it runs `imsg sync-mac --contacts`). The files are your address book:
keep them out of git and backups you share.

In kmessages, the **Sync from Mac** button (the circular arrows next to
New Message, also in the tray menu) runs the same `imsg sync-mac` and shows
what it copied ("Synced from mac.local: 312 contacts (4 updated), 3 pins")
or why it failed; its tooltip says when it last ran. The Mac defaults to the
daemon's `mac_host` and can be set in Settings → Sync from Mac. Pin or
unpin a conversation in kmessages with a right-click (press and hold on a
touch screen) on it in the list, or from its details page.

## Importing history from the Mac

The daemon only sees messages that arrive after it registered. To bring
the recent past over once (messages, attachments, tapbacks, group chats)
from the Mac whose Messages app has it:

```sh
imsg import-mac mac.local --dry-run     # what would be imported
imsg import-mac mac.local --days 10     # or --since 2026-10-01
```

One ssh connection runs `tools/mac-messages-export.py` (embedded in
`imsg`) with the Mac's own `python3`: it reads
`~/Library/Messages/chat.db` read-only (the ssh session needs Full Disk
Access, as for the contacts export) and streams the rows plus the
attachment files (up to `--max-attachment-mb`, default 50) to
`~/.cache/imsg/import/<run>/`, from where the daemon copies them into its
own attachments directory. Imported messages show up in `imsg messages`
and kmessages like any other history, already read, without
notifications. Running it again is safe: messages the daemon already has
(by Apple message id) are skipped and never changed, so overlapping
ranges cost nothing. SMS / RCS conversations relayed by the iPhone are
included (merged into the same chat as iMessages with that number);
`--imessage-only` leaves them out. Transcript backgrounds are not part of
the import. Details: [docs/CLI.md](docs/CLI.md#import-mac-host---days-n---since-date---max-attachment-mb-mb---imessage-only---dry-run).

## Build and test

```sh
cargo build --workspace
cargo test --workspace      # D-Bus tests need `dbus-daemon` (else they print SKIP)
cargo clippy --workspace --all-targets

# The protocol engine against our open-absinthe (after bootstrap).
# On Linux an anisette backend feature is required:
cargo build --manifest-path third_party/rustpush/Cargo.toml --lib --features remote-anisette-v3
```

Layout: `crates/imessage-common` (hwconfig format, validation, paths),
`crates/open-absinthe` (NAC emulator), `crates/imessaged` (daemon),
`crates/imsg` (CLI), `tools/`, `patches/`, `third_party/rustpush`
(submodule, excluded from the workspace).

## License

SSPL-1.0 (see [LICENSE](LICENSE)). The license follows rustpush's, because
this project is a derived work.
