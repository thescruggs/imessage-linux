# Changelog

## Unreleased

* **Text messages through your iPhone** (Text Message Forwarding, like a
  Mac): `imsg sms enable` / kmessages Settings → Text Message Forwarding
  asks the iPhone to relay SMS / MMS / RCS; turn this computer on under the
  iPhone's Settings > Messages > Text Message Forwarding. Relayed texts
  are stored with `service: "sms"` in the same chat as iMessages with that
  number (green bubbles in kmessages, `[SMS]` in `imsg messages`); texts
  sent from the iPhone itself are yours. Sending to a number that is not
  on iMessage goes out as a text automatically; `imsg send --sms` /
  `SendTextAs` force it. New config `sms_relay_number` (default: the
  first phone number in `other_handles`), D-Bus `EnableSmsForwarding`,
  `GetSmsForwarding`, `SendTextAs`, `SendFileAs`, signal
  `SmsForwardingChanged`, `GetStatus.sms`; store schema 5.

## 0.1.11 (2026-10-17)

* **APNs liveness watchdog.** Rustpush acknowledges every push
  notification on its own, so a message the receive loop never handles
  is lost for good. Each inbound message now has a 120 s handling
  budget (a stuck one is logged and skipped), and a watchdog forces an
  APNs reconnect when the daemon is connected but has seen no traffic
  for three keep-alive intervals. `receive loop running`, `APNs pong`
  and `APNs notification` lines at debug make the loop visible.
  `IMESSAGED_PROFILES_FORCE_REFRESH=1` re-fetches the iCloud tokens
  on the next start (debugging aid).
* `imsg import-mac HOST --backgrounds [--chat X]... [--create]` pulls
  chat transcript backgrounds from a Mac's Messages cache into the
  daemon (local only; the Mac's decryption keys are never read). Chats
  the daemon does not have are skipped unless `--create` is given.

* **Shared names and photos** (Messages "Share Name and Photo"): when a
  contact shares theirs, the daemon fetches the name and photo from
  iCloud (CloudKit) and keeps them in the message store; the photo lands
  under `profiles/` in the state directory. Opening a chat refreshes
  profiles older than a day. Login now also asks Apple for the iCloud
  (MobileMe) tokens CloudKit needs; a session saved by an older version
  gets them silently once at startup with the saved credentials. If
  Apple wants a fresh sign-in for that, `imsg status` / `imsg doctor`
  say so: run `imsg login` once. Receive only: your own name and photo
  are not published yet.
* **Group photos**: a group's photo is downloaded and shown as its
  avatar.
* D-Bus: participants in Chat JSON carry `profile`, groups `icon_path`;
  new `GetHandleProfile` method and `HandleProfileUpdated` signal;
  `GetStatus` reports `profiles`. Message store schema 4.
* `imsg profile <handle>`; shared names in `imsg chats` / `imsg chat`;
  `imsg doctor` checks shared-profile access.
* kmessages: shared names and photos for handles the address book does
  not know (chat list, transcript, notifications), "Shared name" on the
  details page, group photos as group avatars.

## 0.1.10 (2026-10-17)

First public release. iMessage on Linux with your own Apple ID: a
background daemon, a command-line client and a KDE app, talking to Apple
directly. A Mac you own is needed once for its hardware identity, and
again (over ssh) whenever the daemon has to log in to Apple.

### Registration and login

* **NAC emulator** (`crates/open-absinthe`): runs Apple's own
  `IMDAppleServices` under the unicorn CPU emulator to produce the
  validation data iMessage registration needs. Apple accepts it for both
  Intel and Apple Silicon Mac identities. The Apple binary is downloaded at
  setup time and never shipped with this project.
* **One-time Mac step**: `tools/mac-hwinfo.sh` reads your Mac's hardware
  identity (including the encrypted IOKit keys on Apple Silicon) into
  `hwconfig.plist`; `imsg hwconfig import` installs it on Linux.
* **Mac-native anisette**: since 2026-10-13 Apple rejects login headers
  from public anisette servers for Mac clients. `tools/mac-anisette-install.sh`
  builds a small helper on your Mac, and the daemon asks it for headers
  over ssh (`anisette_command`), only at login and the rare silent
  re-login. If the Mac is unreachable then, the daemon keeps retrying
  without asking for your password again.
* Apple ID login with two-factor codes, IDS registration that renews on
  its own, and an encrypted state directory with the key in your keyring.

### Daemon (`imessaged`)

* Send and receive text, attachments (photos, video, files), tapbacks
  (including emoji), replies, edits, unsend, typing indicators, delivery
  and read receipts; group chats with renames and membership changes.
* SQLite message store and a D-Bus API (`org.imessage.Daemon1`) on the
  session bus; systemd user unit with D-Bus activation, sandboxed
  (`ProtectSystem=strict`, home read-only).
* **Sending address**: `sender_handle` picks the address new
  conversations start from; replies go out from the address the other
  person wrote to.
* **Your other addresses**: `other_handles` (e.g. your iPhone's number)
  marks messages you send from them as yours instead of as someone
  else's.
* **Note to self**: send to one of your own addresses and it shows up on
  your other devices.
* **Transcript backgrounds** (receive-only): backgrounds set on a chat
  from an iPhone or Mac are downloaded and shown; you can also import a
  picture as a chat's background locally.
* Hardening: bounded downloads (1 GiB attachments, 32 MiB backgrounds),
  validation of peer-supplied data before the protocol engine sees it,
  supervised receive loop that restarts after a crash, credentials
  redacted from logs.

### Command line (`imsg`)

* `login`, `status`, `chats`, `messages`, `chat`, `send`, `send-file`,
  `react`, `watch`, and more; every command has `--json` for scripts and
  stable exit codes.
* `imsg doctor` checks the whole setup: Apple files, identity, anisette
  over ssh (including that the Mac model matches the identity), daemon.
* **History import**: `imsg import-mac <mac> --days N` copies recent
  history (messages, attachments, tapbacks, group chats, SMS relayed by
  the iPhone) from the Mac's Messages database over ssh. Safe to re-run;
  imported messages arrive read and without notifications.
* `imsg store purge-invalid-chats` cleans up chats an early import keyed
  wrongly.

### KDE app (`kmessages`)

* Three-pane Kirigami UI styled after macOS Messages: chat list,
  transcript with bubbles, tapbacks, replies, link and photo previews
  (EXIF orientation honoured), typing indicator and read receipts.
* Notifications with inline reply, tray icon, notifications when the
  daemon needs you (login, 2FA, unreachable Mac).
* Send files from anywhere: file picker, drag and drop, paste.
* **Contacts**: names and photos from KPeople (vCards, KDE Connect,
  Akonadi). `tools/mac-contacts-export.sh` copies your Mac's address book
  once over ssh.
* "My other addresses" setting so messages from your iPhone's number do
  not raise notifications.
* Chat backgrounds painted behind the transcript (HEIC needs
  `kimageformats`).

### Known limitations

* A Mac you own is required: once for the hardware identity, and
  reachable over ssh whenever the daemon logs in to Apple.
* Email addresses only; your phone number cannot be registered from
  Linux, and there is no SMS.
* Transcript backgrounds are received, not sent.
* Unofficial client: Apple may suspend accounts it considers abusive. Use
  your own Mac's identity for your own account only.
* License: SSPL-1.0, inherited from rustpush.
