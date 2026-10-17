# imessaged internals

The daemon wraps OpenBubbles **rustpush** behind a small `Engine` trait,
keeps rustpush's state encrypted at rest, and exposes everything as plain
model types (`imessage_common::model`) (Phase 2a). On top of that trait
(Phase 2b) sit the SQLite message store, the D-Bus API
([docs/DBUS.md](DBUS.md)), attachment download and the systemd unit.

```
rustpush ──EngineEvent──▶ event pump ──▶ SQLite store ──▶ signals (broadcast + D-Bus)
   ▲                                          ▲
   └──────────── Hub (dyn Engine + store) ◀───┴── D-Bus methods (org.imessage.Daemon1)
```

Code map: `engine/` (2a), `store/` (SQLite), `hub.rs` (operations +
signal fan-out + typing), `dbus.rs` (zbus interface), `daemon.rs` (wiring,
event pump, shutdown).

## Files in the state directory

`$XDG_STATE_HOME/imessaged` (default `~/.local/state/imessaged`), created
mode 0700.

| File | Written by | Encrypted | Contents |
|---|---|---|---|
| `hwconfig.plist` | `imsg hwconfig import` | no (0600) | Mac hardware identity; deserializes directly into `rustpush::macos::MacOSConfig` |
| `config.toml` | you | no | daemon settings (below) |
| `state.enc` | daemon | **yes** | `SavedState` (below) |
| `state.key` | daemon | — (0600) | only with the `file` key provider / `auto` fallback |
| `id_cache.plist` | rustpush (`KeyCache`) | **no** | IDS lookup cache: contacts' *public* keys and push tokens, pseudonym expiry. Not secret, but it does reveal who you talk to. |
| `anisette/state.plist` | omnisette | **no** | remote-anisette-v3 provisioning (`adi_pb` blob + keychain id). Device attestation state for the anisette server; omnisette reads/writes it itself, so it cannot be folded into `state.enc` without patching omnisette. Directory is 0700. |
| `messages.db` (+ `-wal`, `-shm`) | daemon (SQLite) | **no**, except attachment locators | chats, participants, messages, reactions, attachment metadata. Mode 0600. Attachment download locators (they contain the per-file decryption key) are sealed with the state key in the `IMSE` format below. Message text is plaintext: protect the state dir (0700) / use disk encryption. |
| `attachments/<id>/<filename>` | daemon | no (0600, dirs 0700) | downloaded attachments (`DownloadAttachment`) and copies of attachments sent with `SendFile` |
| `backgrounds/<chat id>/<background id>[.heic\|.png\|.jpg]` | daemon | no (0600, dirs 0700) | chat transcript backgrounds: the payload as Apple sent it (or as imported) and the picture extracted from it (`src/background.rs`). The MMCS locator (with the file key) is sealed in `messages.db` like attachment locators. |
| `profiles/<handle>/<record key>.<ext>` | daemon | no (0600, dirs 0700) | photos contacts shared with us ("Share Name and Photo"; `/`, `+`, `=` of the base64 record key become `_`, `-`, `~`; extension sniffed: `jpg`, `png`, `heic`, `gif`, `webp`, else `img`). See [Shared names and photos](#shared-names-and-photos-cloudkit). |
| `chat_icons/<chat id>/<id>.<ext>` | daemon | no (0600, dirs 0700) | group photos (`id` = the message that changed it; extension sniffed). |

### `SavedState` (inside `state.enc`)

| Field | rustpush type | Why |
|---|---|---|
| `push` | `APSState` | APNs push token + push certificate (private key lives in `keystore`) |
| `users` | `Vec<IDSUser>` | IDS auth cert + per-service registration certs/handles |
| `identity` | `Option<IDSNGMIdentity>` | this device's message-encryption keys (inline) |
| `account` | `PersistAccountData` as plist | Apple ID username, SRP password hash, GSA tokens, dsid. Enables silent re-login. **The hash is password-equivalent for Apple GSA**: it is an unsalted `sha256(password)`, and GSA's SRP takes exactly that as its input, so whoever holds it can log in as you (2FA still applies on new sessions). It lives only inside the sealed `state.enc`; with the `file` key provider anyone who can read the state directory (and thus `state.key`) has it. Omitted entirely when `store_credentials = false`. |
| `keystore` | `keystore::software::SoftwareKeystoreState` as plist | private keys referenced by alias from `push` and `users` (rustpush keeps them in a process-global keystore that we initialise and persist) |
| `cloudkit_tokens` | `TokenProviderState` as plist | the MobileMe (iCloud) delegate: `cloudKitToken`, `mmeAuthToken`, … and when they were fetched. CloudKit needs them for contacts' shared names and photos. Auth tokens, not password-equivalent, but sealed like the rest. Omitted with `store_credentials = false` (and removed at logout). |
| `registration` | `model::RegistrationState` | summary for clients |

Saved after login, after registration, whenever rustpush reports new state
(the `IMClient` keys-updated callback, keystore writes, every APNs
(re)connect), and on shutdown. Saves are coalesced (250 ms), run on the
blocking thread pool (AES-GCM + `fsync`), and are ordered by a sequence
number so a slow older write can never replace a newer one.

Engine shutdown order: stop supervisors and tasks → `close()` the IDS identity and
drop the `IMClient` → `close()` APNs (its final push state is copied into
`SavedState`) → wait up to 2 s for the IDS `ResourceManager` loop to end →
stop the save loop → one final save. So the last keystore / keys-updated
write before exit is always on disk.

A `keystore` entry that is present but does not parse fails startup
(`EngineError::State`) instead of silently becoming an empty keystore.

## Message store (`messages.db`)

SQLite (bundled, via `rusqlite`), WAL journal, `foreign_keys = ON`,
`synchronous = NORMAL`. `crates/imessaged/src/store/`.

**Concurrency.** `Store` is synchronous: one connection behind a mutex.
Async code calls `StoreHandle::call(|store| …)`, which runs the closure on
tokio's blocking pool (`spawn_blocking`); every operation is one
transaction.

**Migrations.** `store/migrations/NNNN_*.sql`, embedded with
`include_str!`, applied in order (one transaction each) and recorded in
`schema_migrations(version, applied_at)`. Re-opening is a no-op; a
database with a version newer than the binary knows is refused.

| Table | Contents |
|---|---|
| `handles` | `uri` (PK), `display` |
| `chats` | `id` (PK, model `ChatId`), `display_name`, `is_group`, `last_message_at`, `unread_count` (denormalized, recomputed on every change), `name_at` / `participants_at` (timestamp of the rename / membership change that set the current value) |
| `chat_participants` | `(chat_id, handle)` PK, `position`; everyone except us |
| `messages` | `seq` (autoincrement, cursor tie-break), `id` (unique GUID), `chat_id`, `sender`, `sent_at`, `text`, `parts` (JSON array of `MessagePart`), `subject`, `reply_to`, `is_from_me`, `delivery`, `error`, `edited`, `edited_at`, `unsent`, `verified`, `effect`, `is_read` |
| `attachments` | `id` (PK `<msg>:<part>`), `message_id` (FK, cascade), `mime`, `filename`, `size`, `local_path`, `locator` (sealed BLOB) |
| `reactions` | `(message_id, sender)` PK, `kind` (JSON `ReactionKind`), `added`, `at`; one per sender, removals kept as tombstones; no FK (may precede the message) |
| `handle_profiles` (schema 4) | `handle` (PK), `record_key` (CloudKit record; `NULL` = not shared / stopped), `decryption_key` (sealed BLOB), `poster` (sealed contact-poster tags, kept but not fetched), `name` / `first` / `last`, `photo_path`, `shared_at` (`at` of the newest share or stop), `fetched_at` / `fetched_key` (last successful fetch and of which record), `last_seen_at` |
| `chats.icon_*` (schema 4) | group photo: `icon_id`, `icon_locator` (sealed MMCS locator), `icon_size`, `icon_path` (downloaded file), `icon_at` (`at` of the newest change, removals included) |

How each `EngineEvent` lands:

| Event | Store effect | Signal |
|---|---|---|
| `MessageReceived` | upsert chat (older snapshots don't override newer renames / membership), insert message + attachments (a redelivery merges: never resurrects unsent content, never undoes an edit, never moves delivery back, `verified` only downgrades) | `MessageReceived` (new row) / `MessageUpdated`, `ChatUpdated` |
| `MessageSent` / `Delivered` | delivery forward-only; a report that beats its row is held (bounded, 256) and applied on insert | `MessageUpdated` |
| `Read` | from a peer (`by_me: false`) on our message → delivery `read` (held like delivery reports if it beats the row); from one of our devices (`by_me: true`) on someone else's message → local read marker; anything else (a peer "reading" their own message, us reading ours) ignored | `MessageUpdated` / `ChatUpdated` |
| `Reaction` | one per sender, ordered by `at` (late older ones ignored, removal tombstones win) | `MessageUpdated` |
| `Edited` | replaces the text parts, `edited_at` ordering. Our own messages only from our own devices (`from_me: true`, any of our handles); someone else's only by its original sender with `from_me: false`. An edit that beats its message is held in memory (bounded, 256; not persisted, lost on restart) and applied when the message arrives | `MessageUpdated` |
| `Unsent` | whole message emptied (`parts: []`, attachments + reactions deleted, downloaded files under `attachments/` removed); same sender rule and early hold as `Edited` (a late original is stored already unsent, without attachment rows); a redelivered unsent message never recreates attachment rows | `MessageUpdated`, `ChatUpdated` |
| `ChatRenamed` / `ParticipantsChanged` | applied if `at` ≥ the stored one | `ChatUpdated` |
| `ChatBackground` | `chat_backgrounds` row (schema 2-3): background id, payload version, sealed MMCS locator, `set_by`/`set_at`; a set or removal older than the stored change (or the last removal, `chats.background_removed_at`) is ignored, a redelivery of the same id is a no-op; a set announcing more than 32 MiB (`MAX_BACKGROUND_BYTES`) is ignored with a warning, and one with an unusable reference (key / signature not base64, key under 2 bytes, no object id, not an https URL) is dropped in `convert`; replaced or removed backgrounds' files are deleted. A new background is then downloaded eagerly by the hub (`Hub::spawn_background_fetch`; one replaced while the previous download runs is fetched right after, up to 3 in a row) to `backgrounds/<chat>/<id>`, and the picture inside it extracted next to it; a failed download is logged and retried on `GetChat` | `ChatUpdated` (stored), `ChatUpdated` again (downloaded, `path` set) |
| `ProfileShared` | handled by the hub (`hub/profiles.rs`), not `apply_event`: `handle_profiles` row; a share older than `shared_at` is ignored; a new or different record is fetched in the background, the same record only if it was never fetched (and no fetch failed in the last hour); a stop (`share: None`) clears name / photo / key (the row stays as a tombstone so a late older share cannot bring it back) and deletes the photo | `HandleProfileUpdated` + `ChatUpdated` per chat with that participant (after the fetch, or at once for a stop) |
| `ChatIcon` | `chats.icon_*`; older than `icon_at` ignored, a redelivery of the same id a no-op, more than 32 MiB (`MAX_ICON_BYTES`) ignored, the replaced / removed file deleted. A new one is downloaded at once by the hub (`Hub::spawn_icon_fetch`, the capped attachment download); a failed download is retried on `GetChat` | `ChatUpdated` (stored), `ChatUpdated` (downloaded, `icon_path` set) |
| `Typing` | in memory; cleared after 60 s or by a message from that sender | `Typing` |
| `ConnectionChanged` / `RegistrationChanged` / `TwoFactorRequired` / `Error` | not stored | `ConnectionStateChanged` + `PropertiesChanged` / `Registered` / `TwoFactorRequired` / `BackgroundError` |

Unread = incoming, not unsent, not marked read. `ListMessages` pages are
keyed on `(sent_at, seq)` (opaque cursor `"<sent_at>-<seq>"`), stable
while messages arrive. `Search` is a `LIKE` substring match (no FTS yet).
Maintenance helpers: `purge_chat`, `purge_before`, `vacuum`,
`checkpoint`.

Messages we send through the daemon are stored by the D-Bus handler
(`Hub::send_text` etc.) from the engine's `SentMessage`; reactions, edits
and unsends we send are applied to the store the same way. After a
successful `SendFile` the file is copied to `attachments/<id>/<filename>`
(same sanitizing and modes as downloads; temp file + rename) and the
attachment's `local_path` points at that copy, because an outgoing
attachment has no download locator and the user may delete the original.
If the copy fails (disk full, file gone already) the send still counts, a
warning is logged and `local_path` keeps the original path. Unsending the
message deletes the copy like any downloaded file; files outside
`attachments/` are never deleted.

**History import** (`ImportMessages`, `Hub::import_messages`,
`store/import.rs`; fed by `imsg import-mac`). A one-time backfill from
another device's database, not an engine event: insert-only and
idempotent by message id. Message ids that exist are skipped and never
touched (live rows win, re-runs are free); new rows are inserted through
the same path as received messages, then `is_read = 1`, so the import
never raises the unread count. Chats are upserted like a message's chat
snapshot dated `last_message_at` (so a newer live rename / membership
change wins; `imsg` dates each chat by its newest imported message). The
snapshot's participants and name are ignored when the stored chat already
has newer messages, and an undated snapshot never replaces a non-empty
participant list. Attachment files are copied into `attachments/<id>/` before
the transaction (files of messages that turn out to exist after all, e.g.
a live message that raced the import, are deleted again); attachment rows
have no download locator. Tapbacks apply via the normal reaction rules if
their target exists, else they are dropped and counted (re-applying an
identical reaction is a no-op). `MessageUpdated` for each message that
existed before the call and whose reactions changed (e.g. a live message
tapbacked on the Mac), then one `ChatUpdated` per touched chat; no
`MessageReceived`, and every imported group
is `upsert_chat`ed into the engine. Transcript backgrounds are not part of
the import (they arrive with the next background change). The batch is
validated first: every chat id must pass `ChatId::is_valid` and every
handle `Handle::is_valid_uri` (normalized `mailto:` / `tel:` URIs,
`group:` / `group-p:` ids), else nothing is stored (`InvalidInput`).

**Housekeeping: empty groups.** At startup (before the engine is seeded
with stored groups) and inside every `ImportMessages` transaction the
daemon deletes group chats with no participants, no messages, no
background and no name (`Store::delete_empty_groups`, count logged at
info). The import does it before reading back the chats it touched, so
no `ChatUpdated` (and no engine seed) is ever sent for a row that is
then gone, and such a row does not count in `chats_created`. Older daemons created these from 1:1 traffic that carried a group
id (see "Chat ids" below).

**Recovering from a bad import.** An `imsg import-mac` before the id
validation could store chats keyed by raw Mac handle ids (`46339(smsft)`,
`+12125550199(smsfp)`, …). Remove them with

```sh
imsg store purge-invalid-chats        # dry run: lists what would go
imsg store purge-invalid-chats --yes  # delete
imsg import-mac <mac> --days N        # re-import; now keyed tel:46339 etc.
```

`PurgeInvalidChats` (`Store::purge_invalid_chats`, `store/housekeeping.rs`)
deletes, in one transaction, every chat whose id fails `ChatId::is_valid`
with its messages, reactions and attachment rows, plus invalid handle rows
no remaining chat lists; then the hub removes the chats' copied files under
`attachments/` (and `backgrounds/`; nothing outside those is touched). It
emits no signals (clients reload their chat list) and logs a summary. The
re-import is safe: messages it already has under a valid chat id are
skipped, the purged ones come back under the normalized id.

**Restart.** At startup and after every successful registration the daemon
calls `Engine::upsert_chat` for every stored group, and a `ChatNotFound`
from the engine for a stored chat triggers one re-seed + retry, so
`ChatTarget::Chat(group)` works after a restart.

## Shutdown (daemon)

SIGTERM/SIGINT → `Engine::shutdown()` (order below; persists `state.enc`)
→ the event pump drains whatever the engine emitted meanwhile into the
store/signals and stops → typing timers stop, the D-Bus name is released
and the connection closed → the store runs `PRAGMA wal_checkpoint(TRUNCATE)`
and closes. If D-Bus cannot be served at startup the engine and store are
shut down the same way before the error is returned.

## Encryption format

```
offset len
0      4   magic "IMSE"
4      1   format version = 1
5      12  random 96-bit nonce (new for every write)
17     n   AES-256-GCM(key, nonce, plaintext = XML plist of SavedState,
                       aad = bytes 0..5)  -- includes the 16-byte tag
```

Writes are atomic: `.state.enc.tmp-<pid>-<rand>` created with `O_EXCL`
mode 0600, written, `fsync`ed, renamed over `state.enc`, then the directory
is `fsync`ed. A file that fails authentication (wrong key, tampering,
truncation) is never overwritten: the daemon refuses to start and says so.

## Key providers (`key_provider`)

* `secret-service`: an item in the default Secret Service collection
  (KWallet via ksecretd, GNOME Keyring, KeePassXC) with attributes
  `app=imessaged`, `purpose=state-key`, value = 64 hex chars. Created on
  first run. Unlock prompts time out after 60 s.
* `file`: `state.key`, 32 random bytes, mode 0600 (re-tightened if loosened).
  Protects backups/sync of `state.enc` alone; not against someone who can
  read your home directory.
* `auto` (default): an existing `state.key` wins (so a boot without a keyring
  does not orphan the state); otherwise Secret Service; otherwise — **only if
  there is no `state.enc` yet** — create `state.key` with a one-time warning.
  If `state.enc` exists, there is no `state.key` and the Secret Service is
  unavailable, startup fails with "refusing to create a new key": the real
  key is in the keyring, and a fresh file key would shadow it forever.

The daemon distinguishes the two startup failures: *key provider
unavailable* (nothing wrong with `state.enc`; fix/unlock the keyring and
restart) vs. *wrong key* (`state.enc` does not authenticate with the key it
got; restore the original key, and only if it is truly lost move the file
aside and log in again).

Switching providers does not migrate the key; move `state.enc` aside and log
in again (or copy the key value across by hand).

**Recovery and `messages.db`.** Attachment locators in `messages.db` are
sealed with the same state key. Starting over with a new key (moving
`state.enc` aside, switching key provider without copying the key, a
replaced keyring item) keeps the message history readable, but every
attachment that was not downloaded yet becomes undownloadable:
`DownloadAttachment` fails with `NotFound` "attachment locator unreadable
(state key changed)" (files already under `attachments/` are unaffected).
Either accept that, or move `messages.db` (+ `-wal`, `-shm`) and
`attachments/` aside together with `state.enc` to start completely fresh.

## Configuration

`state_dir/config.toml` (all keys optional); CLI flags and env vars override:

| key | flag / env | default |
|---|---|---|
| `anisette_url` | `--anisette-url` / `IMESSAGED_ANISETTE_URL` | `https://ani.sidestore.io` |
| `key_provider` | `--key-provider` / `IMESSAGED_KEY_PROVIDER` | `auto` (`auto`, `secret-service`, `file`) |
| `log_level` | `--log-level` / `IMESSAGED_LOG` (`RUST_LOG` wins) | `info,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn,zbus=warn,tracing=warn` |
| `store_credentials` | `--store-credentials` / `IMESSAGED_STORE_CREDENTIALS` | `true` |
| — | `IMESSAGED_PROFILES_FORCE_REFRESH=1` (env only; debugging) | unset: re-run the silent iCloud sign-in at the next start, see [Liveness watchdog](#liveness-watchdog-enginerustpush_enginelivenessrs) |
| `anisette_command` | `--anisette-command` / `IMESSAGED_ANISETTE_COMMAND` | unset (use `anisette_url`); see [Anisette source](#anisette-source) |
| `sender_handle` | `--sender-handle` / `IMESSAGED_SENDER_HANDLE` | unset = the first registered email handle |
| `other_handles` | `--other-handle` (repeatable) / `IMESSAGED_OTHER_HANDLES` (comma-separated) | `[]` |
| — | `--state-dir` / `IMESSAGED_STATE_DIR` | `$XDG_STATE_HOME/imessaged` |
| — | `--no-dbus` | serve D-Bus (off = headless: receive + store only) |

Unknown keys are an error (typo protection).

`sender_handle` (a bare email address / phone number or an IDS URI;
stored normalized, e.g. `mailto:me@example.com`; anything else fails
startup) picks the handle new conversations start from (Messages.app's
"Start new conversations from"). It is used only if it is one of the
registered email handles; otherwise the daemon warns at startup and after
every (re-)registration and uses the first registered handle. The
effective choice is `registration.sender` in `GetStatus`. Like
Messages.app, a conversation the daemon already knows keeps going out
from the handle of ours it names: a 1:1 replies from the address the peer
wrote to (sent as `[peer, that handle]`, so it stays 1:1), and a group
from the handle we are a member under, so its membership does not change.
Only a remembered 1:1 that names none of our handles falls back to
`sender_handle`. Delivery receipts go out from the handle the peer
addressed.

`other_handles` (array; bare email addresses / phone numbers or IDS URIs,
stored normalized, anything else fails startup; a command line / env list
replaces the file's) are more addresses of the user that this device is
not registered for — typically the iPhone's phone number, which a
Mac-class registration cannot carry. They join the registered handles
wherever the daemon decides what is *ours*: `is_from_me` (a message sent
from the iPhone is the user's own, not unread), the "others" in a
conversation and so the chat id (the iPhone writing to Bob is Bob's 1:1,
not a group of Bob and the phone), typing echoes, delivery receipts, and
`MarkRead` (no read receipt ever goes to a message from any of our
handles). They are never registered and never sent from (sender choice
uses registered handles only). `GetStatus` reports them as
`registration.other_handles`. Messages and chats already stored are not
rewritten: older rows from the phone keep `is_from_me = false` and their
old chat ids.

`store_credentials = false` drops `PersistAccountData` (username, SRP
hash, GSA tokens) from `SavedState` — never written, and removed from an
existing `state.enc` at the next start. Silent re-login is then disabled:
when IDS demands a fresh login (6005) the state becomes `NeedsLogin` and the
user must enter the password again.

## Logging

Our own log lines never contain message bodies, passwords, 2FA codes, keys
or tokens (events are logged as ids/counts only). **The protocol crates are
different**: at `info`/`debug`

* rustpush logs decrypted message bodies (`MessageInst` `Display` in
  `aps_client.rs`), the APNs connect response including the push token
  (`aps.rs`), IDS send payloads (`identity_manager.rs`) and alias
  responses (`user.rs`);
* icloud_auth logs the decrypted SPD (GSA tokens) at `debug`;
* omnisette / keystore log provisioning and key-store details.

So the default filter is
`info,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn,zbus=warn,tracing=warn`
(`zbus`/`tracing` are only noisy: zbus logs D-Bus headers and body
*signatures*, never argument values). A
`log_level` from config / `--log-level` / `IMESSAGED_LOG` (or `RUST_LOG`)
**replaces it entirely**: `log_level = "debug"` raises those crates too and
puts message bodies, push tokens and GSA tokens into the journal. To debug
the daemon alone use
`debug,rustpush=warn,icloud_auth=warn,omnisette=warn,keystore=warn`; the
daemon warns at startup when the *effective* filter (`RUST_LOG` if set,
else the configured one) leaves any of the four crates above `warn`. It
evaluates the filter like env_logger does (longest matching directive
wins, a bare level is global), so `warn`, `error` or `off` alone count as
quiet, and `warn,rustpush::aps=debug` does not.

## Login, 2FA, registration

`Engine::login(apple_id, password)`:

1. `APSConnectionResource::new(config, saved push state)` — connect APNs
   first; the GSA client info carries the push token.
2. `AppleAccount::new_with_anisette(config.get_gsa_config(..), anisette)`
   where anisette = `RemoteAnisetteProviderV3::new(anisette_url, ..,
   state_dir/anisette)`.
3. `login_email_pass(apple_id, sha256(password))` and the GSA state machine:
   * `NeedsDevice2FA` → `send_2fa_to_devices()` (code pushed to trusted devices)
   * `NeedsSMS2FA` → `get_auth_extras()` + `send_sms_2fa_to_devices(first trusted number)`
   * `Needs2FAVerification` / `NeedsSMS2FAVerification` → session parked,
     `LoginOutcome::TwoFactorRequired`, event `TwoFactorRequired`, state
     `NeedsTwoFactor`
   * `NeedsExtraStep` with a PET token is accepted (same as rustpush)
4. `Engine::submit_two_factor(code)` → `verify_2fa` / `verify_sms_2fa`; a
   wrong code (or a transient network error) keeps the session for another
   try (`BadTwoFactorCode` / `Transient`); `NeedsLogin` after 2FA triggers
   one more `login_email_pass` (as rustpush's own `login_with_anisette`
   does). If Apple accepts the code but asks for another one, it returns
   `Ok(LoginOutcome::TwoFactorRequired)` (not an error).

APNs must be connected before the GSA login (30 s; otherwise
`Transient("APNs not connected after 30s")`), because the GSA client info
carries the push token. The anisette client is rebuilt whenever the push
token changes.
5. `login_apple_delegates(&account, None, config, [LoginDelegate::IDS,
   LoginDelegate::MobileMe])` → `authenticate_apple(ids delegate)` →
   `IDSUser`; the MobileMe delegate's tokens become `cloudkit_tokens` (shared
   names and photos, below). If Apple refuses MobileMe the call is repeated
   with IDS alone (logged; profiles are then unavailable). The silent
   re-login takes the same path.
6. `register(config, aps_state, [MADRID_SERVICE], users, identity)`
   (identity = saved `IDSNGMIdentity` or a new one). Phone (`tel:`) handles
   are dropped by our rustpush patch: Mac-class validation data registers
   email handles only.
7. Save, then `IMClient::new(aps, users, identity, [MADRID], id_cache.plist,
   config, keys_updated)` and the receive loop.

Passwords and codes are never logged by imessaged (see [Logging](#logging) for the
protocol crates); the parked session's password hash is
zeroized on drop and has a redacting `Debug`.

On restart with saved users the daemon skips 1–6 and goes straight to
`IMClient::new`.

## Receive path

`aps.messages_cont` → `IMClient::handle` → `MessageInst` → per message:

* **Delivery acknowledgement**: if the sender asked for certified delivery
  (`certified_context`), `identity.certify_delivery("com.apple.madrid", ..)`
  (what current rustpush does); otherwise, if `send_delivered`, the legacy
  `Message::Delivered` reply with the same id and target.
* De-duplication of payload messages (last 2048 ids, shared by all receive
  loops of the engine, so a client replacement cannot re-emit). A redelivered
  duplicate is still acknowledged (above) and then dropped.
* **Sender verification**: rustpush still returns a message whose sender
  signing key could not be verified (`MessageInst.verification_failed`).
  We pass it on with `model::Message.verified = false` and log a warning
  (id and claimed sender only). 2b must store the flag; the UI must mark
  such messages "unverified sender" and not present the sender as
  authenticated.
* `engine::convert::convert_incoming` (pure, unit-tested) → `EngineEvent`s:
  text/attachments/mentions/replies/subject/effect, reactions (tapbacks incl.
  emoji; stickers ignored), edits, unsends, typing, delivered/read receipts,
  group rename, participant changes, send errors (`MessageSent{Failed}`),
  transcript backgrounds (`SetTranscriptBackground`, c=138 →
  `ChatBackground`; non-secret fields logged at debug, never key or
  signature), contacts' shared names and photos (a message's or
  reaction's `embedded_profile`, `ShareProfile` c=131, `UpdateProfile`
  c=180 → `ProfileShared`; only from others, never logged) and group
  photos (`IconChange` c=190 → `ChatIcon`, groups only, a reference
  without object / https URL / key dropped). Everything else
  (`UpdateProfileSharing` and `RecoverProfile`, which our own devices
  exchange about *our* sharing settings, SMS relay, recycle bin, …) is
  dropped, with its `c` value logged at debug.

Chat ids: 1:1 = other party's URI (`mailto:bob@x.com`); group =
`group:<GUID>` (rustpush `sender_guid`, Apple's `gid`); group without GUID =
`group-p:<sorted participants>`. Send and receive both treat *every*
registered handle (plus `other_handles`) as "us", so accounts with several
addresses get the same chat ids in both directions. A conversation is a
group only when it has **more than one party besides us**; rustpush's own
`ConversationData::is_group()` (more than two participants) is not used,
because the peer writing to two of our addresses (the Mac sending from one
with the other listed) has three participants but one other party, and
Apple reuses one `gid` across several unrelated 1:1 chats, so a `gid` is
not a group identity for two-party traffic. Such a conversation is the 1:1
chat with the peer for messages, receipts, typing, renames and
backgrounds alike, and replies go out as a two-participant (1:1)
conversation. The same rule applies to a group that shrinks: once a
group is down to one other party, its later messages (and receipts,
typing, renames, backgrounds) file in that party's 1:1 chat, not under
`group:<GUID>`; the group row keeps its history and the shrunken member
list. A membership change (`ChangeParticipants`) is always about a group:
its chat id comes from the old *and* new members together, so Bob adding
Carol to what was a two-party conversation updates `group:<GUID>`, never
the 1:1 chat with Bob, and a change that leaves no group at all (still
one other party) is ignored.

**Note to self.** A send target naming only one of our own handles
(`imsg send me@example.com …`, or the chat id `mailto:me@example.com`) is
the 1:1 chat with that handle: the conversation's only participant is that
handle, it is also the sender, and rustpush delivers it to our other
devices. Incoming messages whose conversation lists only our own handles
(or nobody) land in the same chat (own handle in the participants, else
the sender). The stored chat lists that handle as its participant. A
target with several of our handles and nobody else is rejected
(`InvalidInput`); groups need at least one other participant.

### Shared names and photos (CloudKit)

Messages' "Share Name and Photo": a contact's share names a record in
Apple's public CloudKit container `com.apple.messages.profiles` plus a key
that decrypts it. It rides along on messages and tapbacks
(`embedded_profile`) or arrives on its own (`ShareProfile`, or
`UpdateProfile`; `UpdateProfile` without a profile, or an empty share,
means the contact stopped sharing). rustpush's
`ProfilesClient::get_record` fetches and opens the record (name `dn` /
`fn` / `ln` and the photo; the contact poster is not fetched: the poster
tags are left out of the request on purpose).

**CloudKit access** (`engine/rustpush_engine/profiles.rs`). CloudKit wants
the MobileMe delegate tokens and the account's dsid (`PersistAccountData`,
= `DsPrsId`). The engine builds a `CloudKitClient` from `cloudkit_tokens`
and an `AppleAccount` made from the saved session; rustpush's
`TokenProvider` renews the tokens itself (weekly, or after a 401) with the
account's PET token, which `AppleAccount` renews silently with the saved
SRP hash. The status is `GetStatus.profiles`:

* `ready`: works;
* `pending`: not tried yet;
* `needs_login`: Apple wants a fresh sign-in (no PET token obtainable
  without a 2FA code, or the delegate was refused). Logged once as a
  warning ("shared names and photos need a fresh login … Run `imsg login`
  once"); nothing retries until a login. Messaging is unaffected;
* `unavailable` (`detail` says why): network / anisette failures are
  retried after 15 min, other failures (a CloudKit panic, odd answers)
  after an hour, `store_credentials = false` never (CloudKit needs the
  saved session).

**Sessions from before this.** A `state.enc` written by an older daemon
has no `cloudkit_tokens`. Once per daemon start, after the connection is
`connected` (so the anisette source answered APNs/IDS bring-up; if it
never connects within 10 min the attempt is skipped), the engine asks
Apple for them with the saved credentials, exactly like the silent
re-login (no user interaction; if Apple wants 2FA the state becomes
`needs_login`). The first fetch does the same if the warm-up did not run.
After that `imsg login` is the only way to get tokens back once Apple
refuses them.

rustpush's CloudKit code `expect`s and `panic!`s on unexpected answers (a
deleted record panics with "No record found?"); every CloudKit call runs
in its own task, so a panic is just a failed fetch.

**Anisette.** Every CloudKit request carries anisette headers (omnisette
caches them for 60 s), so with `anisette_command` a fetch runs the ssh
command when the cache is cold: with the Mac asleep a fetch fails
(`Transient`) and is tried again after the hour below, or when the chat is
opened after that.

**Fetch policy** (`hub/profiles.rs`): a new or changed record is fetched
right away in the background, at most two at a time
(`PROFILE_FETCH_CONCURRENCY`), deduplicated per handle; a failed fetch is
not repeated for an hour unless the record changes. `GetChat` (the app
opening a conversation) refetches every participant whose profile is
older than 24 h (`PROFILE_MAX_AGE`), was never fetched, or whose photo
file is gone. Photos over 8 MiB (`MAX_PROFILE_PHOTO_BYTES`) are dropped
(the name is kept). The photo is written 0600 to
`profiles/<handle>/<record key>.<ext>`; a replaced one is deleted. Our log
lines name the handle and say whether a name / photo came, never the name,
the record key or the decryption key.

**Our own name and photo** are not published (`ProfilesClient::set_record`
exists in rustpush; sending it to contacts is not implemented).

## Supervision

The receive loop and the APS/IDS state monitors run under
`engine::supervise::supervise`. rustpush's handle path has
`unwrap`/`expect`/`panic!` sites; a panic (or an `Err`) is logged, emitted
as `EngineEvent::Error`, the connection state shows `Connecting` while the
task is dead, and the task is respawned with backoff 1 s → 2 → 4 … capped
at 30 s (reset after 60 s of healthy running). The receive loop
re-subscribes to `messages_cont` on restart. Supervisors stop on shutdown
or when the client is replaced (`client_gen`), and aborting a supervisor
aborts its child.

### Liveness watchdog (`engine/rustpush_engine/liveness.rs`)

Why it exists: rustpush acknowledges **every** APNs notification by
itself (the ack task in `APSConnectionResource::new`), and keeps the
socket alive by itself (a ping every 60 s; a missed pong reconnects).
Neither depends on our receive loop. A notification that reaches the
process but is never handled by the receive loop is therefore gone for
good: Apple has it as delivered and does not resend it after a restart.
The receive loop handles messages one at a time, inline through
`IMClient::handle`, and rustpush's handler has unbounded waits in it
(IDS push commands fetch the account's handles over HTTPS through a
client with no timeout; some paths return silently). And the connection
state (`Connected`) is computed from the APNs and IDS resource states
alone, so a stuck or missing receive loop is invisible there. This
happened on 2026-10-17 14:55–15:10: `Connected`, pongs flowing, nothing
received for 15 minutes, the messages lost.

What the daemon does about it:

* the receive loop logs `receive loop running (client generation N)` at
  info when it starts, and at debug every APNs message it takes from the
  broadcast (`APNs pong`, `APNs notification on <topic>`), so the journal
  shows whether traffic reaches it;
* handling one message (rustpush's `handle`, and each delivery
  acknowledgement) is bounded by `HANDLE_TIMEOUT` (120 s; every wait
  inside rustpush is itself bounded at 15–60 s, so only a genuine stall
  gets there). On timeout that message is given up (warn, `Error` event)
  and the loop goes on with the next one;
* a closed APNs broadcast ends the loop with an error (logged, respawned
  with a fresh subscription) instead of a silent `Ok`;
* a watchdog task (`APNs watchdog`, per client generation) checks every
  30 s (`WATCHDOG_TICK`): if the connection is `Connected` and the
  receive loop has seen no APNs message at all for `SILENCE_LIMIT`
  (3 × the 60 s keep-alive = 180 s; a healthy socket shows a pong at
  least every 60 s) it logs a warning, emits an `Error` event and forces
  an APNs reconnect (`request_update`), which re-filters the topics and
  re-runs `IMClient`'s connection setup; at most once per
  `SILENCE_LIMIT`. A loop busy inside the handler counts as alive. The
  rule is the pure `liveness::verdict`, unit tested.

Debugging aid: `IMESSAGED_PROFILES_FORCE_REFRESH=1` in the daemon's
environment makes the next start ignore the saved `cloudkit_tokens` and
run the silent "no iCloud tokens yet" sign-in again (the tokens are
replaced, never deleted). With it, that sign-in alone did **not**
reproduce the 14:55 deafness (inbound worked 60 ms after the test
message), so the trigger was something one-off at that start (the first
MobileMe sign-in of this device, which may well have made Apple push an
IDS command to the account's devices, is the prime suspect; it could not
be captured because the build in use logged nothing of the kind).

## Anisette source

`engine::anisette::Provider` wraps either omnisette's remote v3 client
(`anisette_url`) or `CommandAnisetteProvider` (`anisette_command`: a shell
command printing a JSON object of headers, normally `ssh -F ~/.ssh/config
-o BatchMode=yes -o LogLevel=ERROR <mac> .local/bin/imessage-anisette`). The command supplies `X-Apple-I-MD`,
`X-Apple-I-MD-M`, `X-Apple-I-MD-LU`, `X-Mme-Device-Id`,
`X-Apple-I-MD-RINFO`, `X-Mme-Client-Info`, `X-Apple-Locale`; the provider
adds `X-Apple-I-Client-Time`/`X-Apple-I-TimeZone`, times out after 30 s,
logs the command's stderr (lines naming an anisette header are dropped and
long base64/hex tokens masked, `imessage_common::redact`, also for the
last line quoted in errors) and never logs its stdout. omnisette caches
headers for 60 s. Failures surface as `EngineError::Anisette`
(`anisette command failed: <reason>`, redacted, the command itself only
logged): an interactive `Login` fails with it at once (D-Bus `Transient`),
a silent re-login keeps retrying (see below). The sandboxed unit implies `PrivateUsers=`, which is why
the ssh command must skip the system config (`-F ~/.ssh/config`);
`-o LogLevel=ERROR` keeps MOTD/banner/known_hosts chatter out of the
journal. The command must reach the same Mac that `hwconfig.plist` was
extracted from: the headers attest that machine, and `imsg doctor` warns
when the model in `X-Mme-Client-Info` differs from the hwconfig's.

## Re-registration and reconnect (what rustpush actually does)

rustpush wraps both the APNs connection and the IDS identity in a
`ResourceManager` that regenerates itself:

* **APNs**: on socket error the resource regenerates with exponential backoff
  (max 30 s); a 60 s ping/pong keep-alive detects dead sockets. We watch
  `aps.resource_state` (`Generating`→`Connecting`, `Generated`→`Connected`,
  failures with a retry → still `Connecting`) and save after every
  `generated_signal` (a reconnect may re-activate and change the token).
* **IDS registration** (`IdentityResource`): re-registers automatically
  * on schedule: `IDSRegistration::calculate_rereg_time_s` = heartbeat
    interval from Apple, or 5 minutes before the registration cert expires;
  * immediately if the service client-data hash changed (rustpush upgrade);
  * on IDS push command 32 ("re-register") and 66 (handles changed);
  * on lookup error 6005;
  * backoff on failure: 5 min doubling to 1 day, forever.
  Each success fires `IMClient`'s keys-updated callback → we persist the
  users and emit `RegistrationChanged` (with `expires_at` = next scheduled
  re-registration). During a re-registration the state is `Registering`;
  while failing with retries it is `Error("registration failing, …")`.
* **6005 on register** means the IDS auth is dead: rustpush marks the
  resource `DoNotRetry` and stops (`Failed` then `Closed`; a `Closed`
  identity while the client is current is treated the same). We then try a
  **silent re-login** with the saved `PersistAccountData` (username + SRP
  hash) and rebuild the client from step 5 above. Retry policy
  (`relogin_retry_delay`):
  * network failures and APNs not ready (`EngineError::Transient`) and an
    unreachable or failing anisette source (`EngineError::Anisette`, e.g.
    the Mac behind `anisette_command` is asleep and ssh times out) are
    retried **indefinitely**: after 10 s, 60 s, 5 min, then every 15 min.
    The saved SRP hash is still valid, so the user is not asked to log in
    again. Meanwhile the state is
    `Error("anisette source unreachable: <reason>")` for anisette failures
    and `Error("registration failing, retrying: re-login: …")` otherwise;
    the first successful attempt clears it (`Connected`);
  * other rustpush / IDS errors (`Protocol`) are retried after 10 s, 60 s
    and 5 min, then `NeedsLogin`;
  * genuine auth failures (Apple rejects the credentials, extra step, no
    saved credentials) short-circuit to `NeedsLogin`. If Apple wants 2FA,
    `TwoFactorRequired` is emitted and `submit_two_factor` finishes it.

  Every wait ends early on shutdown (the loop exits), and an interactive
  login or logout supersedes the loop (below).
  An interactive login (or logout) that completes while a silent re-login
  is pending or sleeping between retries cancels it (`login_epoch`), so it
  cannot replace the user's fresh client. Silent re-logins that succeed
  but are rejected again right away are counted: the 2nd..5th in a row wait
  30 s, 60 s, 120 s, 240 s first; after 5 the daemon stops, emits `Error`
  (D-Bus `BackgroundError`)
  and goes to `NeedsLogin` (the count resets after an hour or an
  interactive login).
* Once shutdown has started, `ensure_aps`, `finish_login`,
  `silent_relogin_once` and client creation refuse to start (and an APNs
  connection / `IMClient` that finished connecting during shutdown is
  closed immediately), so nothing outlives `shutdown()`.
* `Engine::register()` *requests* `identity.refresh_now()`. rustpush's
  `refresh_option` silently does nothing (returns `Ok`) if the identity was
  regenerated less than 15 s ago (`MAX_RESOURCE_REGEN`).

## Engine trait (for Phase 2b)

`crates/imessaged/src/engine/mod.rs`:

```rust
#[async_trait]
pub trait Engine: Send + Sync {
    async fn login(&self, apple_id: &str, password: &str) -> EngineResult<LoginOutcome>;
    async fn submit_two_factor(&self, code: &str) -> EngineResult<LoginOutcome>;
    async fn logout(&self) -> EngineResult<()>;
    async fn register(&self) -> EngineResult<RegistrationState>;
    async fn registration_state(&self) -> RegistrationState;
    async fn connection_state(&self) -> ConnectionState;
    async fn handles(&self) -> Vec<Handle>;
    async fn send_text(&self, chat: &ChatTarget, text: &str, reply_to: Option<&MessageId>) -> EngineResult<SentMessage>;
    async fn send_attachment(&self, chat: &ChatTarget, path: &Path, mime: &str) -> EngineResult<SentMessage>;
    async fn react(&self, chat: &ChatTarget, message_id: &MessageId, kind: ReactionKind, remove: bool) -> EngineResult<SentMessage>;
    async fn edit(&self, chat: &ChatTarget, message_id: &MessageId, new_text: &str) -> EngineResult<SentMessage>;
    async fn unsend(&self, chat: &ChatTarget, message_id: &MessageId) -> EngineResult<SentMessage>;
    async fn set_typing(&self, chat: &ChatTarget, typing: bool) -> EngineResult<()>;
    async fn mark_read(&self, chat: &ChatTarget, up_to: &MessageId) -> EngineResult<()>;
    async fn lookup_handles(&self, handles: Vec<String>) -> EngineResult<Vec<(String, bool)>>;
    async fn download_attachment(&self, locator: &str, dest: &Path, max_bytes: u64) -> EngineResult<()>;
    async fn fetch_profile(&self, share: &ShareRef) -> EngineResult<FetchedProfile>;
    async fn profiles_status(&self) -> ProfilesStatus;
    async fn upsert_chat(&self, chat: &Chat);
    fn events(&self) -> broadcast::Receiver<EngineEvent>;
    async fn shutdown(&self);
}
```

* `ChatTarget::Participants(handles)` or `ChatTarget::Chat(chat_id)`. The
  engine remembers conversations it has seen this session; after a restart
  2b must call `upsert_chat` for stored chats (at least groups) so
  `ChatTarget::Chat` resolves. Unknown 1:1 ids resolve by themselves.
  Handles that are neither an email nor a phone number (`""`, `"bob"`)
  fail with `EngineError::InvalidHandle` (`model::Handle::try_normalize`).
* Send-type calls return `SentMessage { id, chat_id, sent_at, sender }`
  (`sender` = the handle the engine actually sent from). For
  `Participants` naming a new group the engine mints the `group:<GUID>`
  id; this is the only place the caller learns it synchronously.
* `MessageSent`, `Reaction`, `Edited`, `Unsent`, `ChatRenamed` and
  `ParticipantsChanged` carry `at` (ms since epoch, from the message's
  `sent_timestamp`) so the store can order late / out-of-order updates.
* `Edited` / `Unsent` carry `from_me` and `Read` (`ReadReceipt`) carries
  `by_me`: whether the sender is one of our own handles. The store uses
  them so a peer cannot edit / unsend our messages or mark messages read
  on our side.
* `EngineEvent` is `Serialize` (`{"event": "message_received", …}`); the
  channel holds 1024 events, a lagging consumer gets `Lagged` and should
  resync from the store.
* `MessageReceived` carries `attachments: Vec<model::Attachment>` whose
  `mmcs` field is the download locator (`{"v":1,"plist":"<rustpush
  Attachment as XML plist>"}`, via `convert::attachment_locator` /
  `attachment_from_locator`). It contains the per-file decryption key:
  store it like a secret. 2b downloads with
  `Engine::download_attachment(locator, dest, max_bytes)`: inline attachments are
  written directly, MMCS ones via `rustpush::Attachment::get_attachment`
  (needs APNs); the file is written atomically (temp file in `dest`'s
  directory, mode 0600, rename). The size in a message is the sender's
  claim: a download announcing or receiving more than `max_bytes`
  (`MAX_ATTACHMENT_BYTES` = 1 GiB for attachments, `MAX_BACKGROUND_BYTES`
  = 32 MiB for transcript backgrounds and group photos) fails with `InvalidInput`, and at
  most 4 MiB is pre-allocated. `MockEngine` writes
  `MockEngine::ATTACHMENT_PAYLOAD`.
* `fetch_profile(share)` fetches a shared profile (`ShareRef` = record key
  + decryption key, redacted from `Debug`); `Unsupported` while
  `profiles_status()` is not `ready` and cannot become so without the
  user.
* `MockEngine` (`engine::mock`, always compiled): records `MockCall`s
  (without passwords/codes/locators/profile keys), `inject(event)`,
  `set_profile(record, FetchedProfile)`, `set_profiles_status`,
  `set_require_two_factor`, `set_reachable`, `fail_next`,
  `set_connection_state`. Same chat-id scheme as the real engine: new
  participant sets get a deterministic `group:00000000-0000-4000-9000-<n>`
  reused for the same set. See `crates/imessaged/tests/mock_engine.rs`.
* `logout()` forgets the account locally (users, identity, saved
  credentials, pending 2FA, conversation cache) and cancels any silent
  re-login; it does not deregister from Apple.
* `Daemon::with_engine(cfg, Arc<dyn Engine>)` (+ `.with_store(..)`,
  `.with_dbus(BusChoice::Address(..))`, `.with_typing_timeout(..)`) and
  `start()` / `run_until(fut)` are the test seam: see
  `crates/imessaged/tests/{daemon_store,dbus_api,binary}.rs`. Event log
  lines carry ids and counts, never message contents.

## Limits / stubs

* **Attachment send** is implemented (`MMCSFile::prepare_put` +
  `Attachment::new_mmcs`, UTI guessed from the MIME type; the file is read
  into memory once) and **download** (`download_attachment`, buffered in
  memory then written atomically), both untested against Apple.
* Inline replies set `reply_part = "0:0:0"` (the quoted range is cosmetic).
* Reactions/edits/unsends target part 0; the store treats every unsend as
  whole-message and maps an edit's part index onto its own `parts` only
  approximately (single-part messages, the common case, are exact).
* One tapback per sender per message (Apple's classic model).
* `Search` is `LIKE`-based (ASCII case folding only); no FTS index yet.
* `Logout` is local only (no IDS deregistration).
* Stickers, app balloons, FaceTime, SMS relay: ignored. Shared names and
  photos are received only; our own are not published.
* Only one `RustpushEngine` per process (rustpush's keystore is global).
* Nothing here has talked to Apple yet; first live run is Phase 5.
