# imessaged D-Bus API (`org.imessage.Daemon1`)

| | |
|---|---|
| Bus | session |
| Well-known name | `org.imessage.Daemon1` (claimed with `DoNotQueue`: a second daemon fails to start) |
| Object | `/org/imessage/Daemon1` |
| Interface | `org.imessage.Daemon1` |
| Activation | `packaging/dbus/org.imessage.Daemon1.service` → `imessaged.service` (systemd only, see [Activation](#activation)) |

Arguments are plain D-Bus types; structured payloads are **JSON strings** of
the shared model types in `crates/imessage-common/src/model.rs` (serde
names, snake_case). Conventions:

* timestamps are **milliseconds since the Unix epoch** (registration times
  are seconds — see `RegistrationState`);
* handles are IDS URIs (`mailto:bob@example.com`, `tel:+15551234567`);
  inputs may be bare addresses in any case and are normalized;
* chat ids: 1:1 = the other party's URI, groups = `group:<GUID>` (or
  `group-p:<sorted participants>` for groups without a GUID); a note to
  self (sending to one of your own handles) is the 1:1 chat with that
  handle;
* message ids are Apple's upper-case GUIDs; attachment ids are
  `<message id>:<part>`;
* `""` means "none" for optional string arguments.

**What crosses the bus.** `Login`'s password and `SubmitTwoFactor`'s code
are ordinary method arguments on the session bus, unencrypted: any process
running as your user can watch them with `dbus-monitor` / `busctl monitor`
(the session bus does not isolate processes of the same uid). The same
goes for message text in method arguments and signals. Treat the session
bus as trusted only to the extent that everything running as you is.
What does *not* cross the bus: attachment download locators (which hold
the per-file key) never leave the daemon, and the daemon never logs
passwords, 2FA codes or message bodies.

## Activation

`packaging/dbus/org.imessage.Daemon1.service` has `Exec=/bin/false` and
`SystemdService=imessaged.service`: the bus asks systemd to start the
unit, so on-demand activation **requires systemd --user** (the bus
must be the one systemd manages). Under a bus without systemd — e.g.
`dbus-run-session`, or a CI container — activation fails; start
`imessaged` yourself first (`imessaged &`, then wait for the name).

The **first** activated call can exceed the bus's default 25 s method
timeout: the daemon has to obtain the state key first, and the Secret
Service may show an unlock prompt that waits for you (up to 60 s). The
call then fails with `NoReply`/`TimedOut` although the daemon keeps
starting; retry once it owns the name, or start the service ahead of time
(`systemctl --user start imessaged`; it is started at login when enabled).

## Methods

**Slow calls.** `Login`, `SubmitTwoFactor`, `SendFile` and `ImportMessages` can take well
over the 25 s default reply timeout of libdbus / QtDBus / busctl: the
anisette command alone may need up to 30 s, followed by GSA and IDS round
trips (or the upload). Call them with a timeout of 120 s (`imsg` and
kmessages do; `busctl --timeout=120`). A client that gives up earlier
leaves the daemon working: a later `TwoFactorRequired` signal or
`ConnectionState` change reports the outcome. If the anisette source
fails during an interactive `Login`, the call fails at once with
`Transient` and a message starting `anisette command failed:`.

| Method | In | Out | Notes |
|---|---|---|---|
| `Login` | `apple_id s`, `password s` | `s` | `"ok"` or `"two_factor_required"` (then a `TwoFactorRequired` signal has been sent; call `SubmitTwoFactor`). Continues into IDS registration. |
| `SubmitTwoFactor` | `code s` | `s` | `"ok"`, or `"two_factor_required"` if Apple accepted the code but wants another one. A wrong code fails with `BadTwoFactorCode` and the session is kept for a retry. |
| `Logout` | — | — | Forget the account locally (IDS users, identity, saved credentials); state becomes `needs_login`. Message history is kept. Does not deregister from Apple. |
| `GetStatus` | — | `s` | [Status](#status) JSON |
| `ListChats` | `limit u`, `offset u` | `s` | JSON array of [Chat](#chat), newest activity first. `limit` 0 = 50, max 500. |
| `GetChat` | `chat_id s` | `s` | [Chat](#chat) JSON; `NotFound` if unknown |
| `ListMessages` | `chat_id s`, `before_cursor s`, `limit u` | `s` | [MessagePage](#messagepage) JSON. `before_cursor` `""` = newest page; pass the previous page's `next_cursor` to page back. |
| `Search` | `query s`, `limit u` | `s` | JSON array of [Message](#message), newest first. Case-insensitive (ASCII) substring match on text and subject; `%`/`_` are literal; unsent messages excluded. |
| `SendText` | `chat_id_or_empty s`, `participants_json s`, `text s`, `reply_to s` | `s` | [SentMessage](#sentmessage) JSON. Either a chat id, or `""` plus a JSON array of handles (`["bob@example.com"]`; 2+ handles = a new or existing group; our own handles are not counted as recipients). A single one of our own handles is a note to self: sent from that handle, delivered to our other devices. `reply_to` = message id or `""`. The new row is stored and announced with `MessageReceived` (`is_from_me: true`) before the call returns. |
| `SendFile` | `chat_id_or_empty s`, `participants_json s`, `path s`, `mime s` | `s` | [SentMessage](#sentmessage). `path` must be an absolute path to a regular file readable by the daemon; `mime` `""` = `application/octet-stream`. |
| `React` | `chat_id s`, `message_id s`, `kind s`, `remove b` | — | `kind`: `love`, `like`, `dislike`, `laugh`, `emphasize`, `question`, `emoji:<emoji>` or a bare emoji. Stored and announced with `MessageUpdated`. |
| `Edit` | `chat_id s`, `message_id s`, `text s` | — | Own messages only (`InvalidInput` otherwise). `MessageUpdated` with `edited: true`. |
| `Unsend` | `chat_id s`, `message_id s` | — | Own messages only. Text, parts, attachments and reactions are dropped; `unsent: true`. |
| `SetTyping` | `chat_id s`, `typing b` | — | Sends our typing indicator. |
| `MarkRead` | `chat_id s`, `up_to_message_id s` | — | Marks incoming messages up to and including that one read locally (`""` = all), emits `ChatUpdated`, then sends a read receipt (best effort: a failed receipt is logged, not returned). |
| `LookupHandles` | `handles_json s` | `s` | JSON array of handles in → `[{"handle":"mailto:bob@example.com","reachable":true}, …]` in input order. Any invalid handle fails the call (`InvalidHandle`). |
| `DownloadAttachment` | `attachment_id s` | `s` | Local path. Downloads once to `$STATE_DIR/attachments/<id>/<filename>` (file 0600, dirs 0700; filename sanitized and cut to 200 bytes); later and concurrent calls return the same path without downloading again. `NotFound` if the attachment is unknown, has no locator, or its locator is unreadable because the state key changed (see docs/DAEMON.md, recovery). Outgoing attachments (`SendFile`) return the daemon's own copy, made at send time in the same place, so deleting or moving the original does not break them (if that copy could not be made, a warning is logged and the original path is returned). |
| `ImportChatBackground` | `chat_id s`, `path s` | `s` | Set the chat's transcript background **locally only** (nothing is sent) from a file the daemon can read: a PosterKit payload as Apple sends it or a plain image (e.g. recovered from a Mac's `~/Library/Messages/TranscriptBackgroundCache`). Copied into `$STATE_DIR/backgrounds/`; id `local-…`, `set_at` = the import time, so a later change from iMessage replaces it but a late-delivered older one (e.g. a removal) does not. Returns the updated [Chat](#chat) and emits `ChatUpdated`. `NotFound` for an unknown chat, `InvalidInput` for a relative path, an unreadable file or one over 32 MiB. |
| `ImportMessages` | `batch_json s` | `s` | One-time history import ([ImportBatch](#importbatch) in, [ImportResult](#importresult) out); used by `imsg import-mac`. Insert-only and idempotent by message id: a message the store already has is skipped and left unchanged. Every chat id and handle must be valid (see below), else `InvalidInput` naming the first bad value and nothing is stored. Slow (copies files): call with a 120 s timeout. |
| `PurgeInvalidChats` | `dry_run b` | `s` | Maintenance (`imsg store purge-invalid-chats`): deletes every chat whose id is not a valid chat id (a `mailto:` / `tel:` URI, `group:<GUID>` or `group-p:<URIs>`; e.g. raw Mac SMS handles like `46339(smsft)` left by an old import) with its messages, reactions, attachment rows and the copied files under `$STATE_DIR/attachments/` (and background files), plus handle rows with an invalid URI no remaining chat lists. `dry_run = true` deletes nothing and reports what would go. Returns a [PurgeSummary](#purgesummary). No signals (only logged): clients should reload their chat list afterwards. |
| `Version` | — | `s` | daemon version, e.g. `"0.1.0"` |

## Properties

| Property | Type | Change signal | Value |
|---|---|---|---|
| `ConnectionState` | `s` | `PropertiesChanged` | `disconnected`, `connecting`, `connected`, `needs_login`, `needs_two_factor`, `registering`, `error` (details in `GetStatus`) |
| `Registered` | `b` | `PropertiesChanged` | IDS registration has at least one handle |

## Signals

| Signal | Args | When |
|---|---|---|
| `MessageReceived` | `chat_id s`, `message_json s` | A **new message row**: incoming, from another of our devices, or sent through this daemon (`is_from_me` tells which). Fires after the row is stored. |
| `MessageUpdated` | `chat_id s`, `message_json s` | An existing message changed: delivery state (`sent` → `delivered` → `read`, or `failed`), reaction added/removed, edit, unsend. Carries the full current message. |
| `ChatUpdated` | `chat_json s` | Chat created, renamed, membership changed, new activity, unread count changed. |
| `Typing` | `chat_id s`, `handle s`, `typing b` | Someone started/stopped typing. A start without refresh is cleared after 60 s, and a message from that person clears it. In memory only. |
| `ConnectionStateChanged` | `state s` | Same value as the `ConnectionState` property. |
| `TwoFactorRequired` | — | A login (interactive or the automatic re-login after IDS error 6005) needs a code: call `SubmitTwoFactor`. |
| `BackgroundError` | `message s` | Background problems not tied to a method call (registration rejected, state save failed, task restarted, events dropped — resync). Method failures are D-Bus errors instead (below). |

Ordering: engine event → SQLite → signal, one event at a time, so a client
that reacts to a signal by calling `ListMessages`/`GetChat` always sees the
change. If the daemon's own event queue overflows it emits
`BackgroundError("… engine event(s) were dropped; clients should resync")`.

## Errors

`org.imessage.Daemon1.Error.<Name>`, message = human-readable text:

| Name | Meaning |
|---|---|
| `NotLoggedIn` | no registered account |
| `NoHardwareConfig` | no `hwconfig.plist` imported (`imsg hwconfig import`) |
| `NoPendingTwoFactor` | `SubmitTwoFactor` without a pending login |
| `BadTwoFactorCode` | wrong code; retry |
| `Login` | Apple rejected the login or wants something we cannot do (don't retry without the user) |
| `Transient` | network / anisette / Apple hiccup; retry later |
| `InvalidHandle` | not an email address or phone number |
| `ChatNotFound` | unknown chat id (the engine re-learns stored chats automatically; this means it is in neither) |
| `InvalidInput` | bad argument (JSON, path, reaction kind, editing someone else's message, …) |
| `Unsupported` | not implemented by the engine |
| `State` / `StateKeyMismatch` | engine state storage problems |
| `Protocol` | other engine / rustpush failure |
| `NotFound` | no such chat / message / attachment in the store |
| `Store` | SQLite / file system failure |

## JSON payloads

### Status

```json
{
  "version": "0.1.0",
  "connection": {"state": "connected"},
  "registered": true,
  "registration": {"handles": [{"uri": "mailto:me@example.com"}], "registered_at": 1700000000, "expires_at": 1702592000,
                   "sender": {"uri": "mailto:me@example.com"}, "other_handles": ["tel:+15551234567"]},
  "unread": 3
}
```

`connection` is `{"state": "<name>"}` or, for errors,
`{"state": "error", "detail": "registration failing, retrying: …"}`.
Our handles are `registration.handles`. `registration.sender` is the
handle new conversations start from (replies in a known chat use the
handle of ours that chat names): the daemon's `sender_handle` setting if
it is registered, else the first handle (`null` when not registered; see
docs/DAEMON.md, Configuration). `registration.other_handles` (array of
IDS URIs, `[]` if unset; older daemons omit it) is the daemon's
`other_handles` setting: addresses of the user that this device is not
registered for (e.g. the iPhone's number). The daemon already treats
messages from them as our own (`is_from_me`, chat ids, no read receipts);
they are never sent from. `registered_at` / `expires_at` are **seconds**. `unread` is `null` if the
store is unavailable.

### Chat

```json
{
  "id": "group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40",
  "participants": [{"uri": "mailto:bob@example.com"}, {"uri": "tel:+15551234567"}],
  "display_name": "Team",
  "is_group": true,
  "last_message_at": 1700000123456,
  "unread_count": 2,
  "background": {
    "id": "9C5E17D7-D261-4AFF-82C0-85AF2D1D362E",
    "path": "/home/me/.local/state/imessaged/backgrounds/group_5B0E…/9C5E17D7-….heic",
    "payload": "/home/me/.local/state/imessaged/backgrounds/group_5B0E…/9C5E17D7-…",
    "version": 1,
    "set_by": "mailto:bob@example.com",
    "set_at": 1700000100000
  }
}
```

`background` is the chat's transcript background (Messages "Backgrounds",
iOS/macOS 26, shared by everyone in the chat), or `null`/absent for none.
`path` is the picture to paint behind the transcript (HEIC, PNG or JPEG
extracted from the payload; `null` until downloaded, or when the payload
holds no picture, e.g. a colour poster); `payload` is the file as Apple
sent it (a PosterKit archive, see docs/RESEARCH.md); `version` is Apple's
payload version; `set_by`/`set_at` say who changed it and when (`null`/0
for a background imported with `ImportChatBackground`). A new background
arrives as `ChatUpdated` twice: first without `path` (stored), then with
it (downloaded). `GetChat` retries a download that failed.

`participants` excludes us, except in a note-to-self chat (id = one of
our own handles), where it is that handle. Handles may carry `"display"` (not filled yet).

### Message

```json
{
  "id": "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0",
  "chat_id": "mailto:bob@example.com",
  "sender": {"uri": "mailto:bob@example.com"},
  "sent_at": 1700000123456,
  "text": "look at this",
  "parts": [
    {"type": "text", "text": "look at this"},
    {"type": "attachment", "attachment_id": "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0:1",
     "mime": "image/jpeg", "filename": "IMG_0001.jpeg", "size": 183422},
    {"type": "mention", "handle": "mailto:carol@example.com", "text": "Carol"},
    {"type": "unsupported", "description": "app message"}
  ],
  "subject": null,
  "reply_to": null,
  "is_from_me": false,
  "delivery": "delivered",
  "edited": false,
  "unsent": false,
  "reactions": [
    {"kind": "love", "sender": {"uri": "mailto:me@example.com"}, "added": true},
    {"kind": {"emoji": "🔥"}, "sender": {"uri": "mailto:carol@example.com"}, "added": true}
  ],
  "effect": null,
  "verified": true
}
```

* `delivery`: `pending`, `sent`, `delivered`, `read`, `failed`. It only
  moves forward; `failed` only replaces `pending`/`sent`.
* `verified: false` = the message decrypted but the sender's signing key
  could not be verified: show the sender as **unverified**.
* `reactions` lists current reactions only (one per sender; a new one
  replaces the old). `kind` is a string for the classic tapbacks and
  `{"emoji": "…"}` for emoji reactions.
* `text` concatenates the text and mention parts; `null` if none.
* An unsent message keeps its id/sender/time with `text: null`,
  `parts: []`, `unsent: true`.

### MessagePage

```json
{"messages": [ /* Message, oldest first */ ], "next_cursor": "1700000000000-42"}
```

`next_cursor` is opaque (`null` on the oldest page). Pages are keyed on
`(sent_at, insertion order)`, so they stay stable while new messages
arrive.

### SentMessage

```json
{"id": "6C1D…", "chat_id": "group:5B0E2C1A-…", "sent_at": 1700000123456, "sender": {"uri": "mailto:me@example.com"}}
```

`sender` is the handle the message was actually sent from (also the
stored row's `sender`).

For a new group (`participants_json` with several handles) this is where
the minted `group:<GUID>` chat id is learned.

### ImportBatch

```json
{
  "chats": [ /* Chat (unread_count is ignored) */ ],
  "messages": [
    {"message": { /* Message */ },
     "attachments": [{"id": "<message id>:1", "mime": "image/heic", "filename": "IMG_1.HEIC", "size": 1234,
                      "source_path": "/home/me/.cache/imsg/import/…/IMG_1.HEIC"}]}
  ],
  "reactions": [
    {"message_id": "<target message id>", "reaction": {"kind": "love", "sender": {"uri": "mailto:me@example.com"}, "added": true},
     "at": 1700000123456}
  ]
}
```

All three arrays are optional; at most 2000 messages and 10000 reactions
per call. Every chat id (in `chats` and each message's `chat_id`) must be
a valid chat id — a normalized `mailto:` / `tel:` URI (`tel:` + optional
`+` and digits only, so SMS short codes are `tel:46339`), `group:<GUID>`
(upper case) or `group-p:<URIs>` — and every handle (chat participants,
message senders, reaction senders) a normalized `mailto:` / `tel:` URI;
otherwise the whole batch is refused with `InvalidInput` naming the first
bad value. Processing, in one store transaction:

* **chats** are created, or get the batch's participants / name unless a
  live rename or membership change is newer;
* **messages** whose id the store already has are skipped (counted in
  `skipped_existing`, never modified); new ones are inserted with their
  attachments and **marked read**. Each attachment must be an
  `Attachment` part of its message (`InvalidInput` otherwise, and nothing
  is stored); `source_path` (absolute, readable by the daemon) is copied
  to `$STATE_DIR/attachments/<id>/<filename>` like a `SendFile` copy; a
  missing or unreadable file is logged and the row stored without one;
* **reactions** are applied in order when their target message exists
  (same rules as received tapbacks: one per sender, newer `at` wins),
  else counted as orphaned and dropped.

Signals: one `ChatUpdated` per touched chat at the end; **no**
`MessageReceived` / `MessageUpdated` (an import must not notify). Imported
group chats are taught to the engine, so sending to them works.

### ImportResult

```json
{"inserted": 744, "skipped_existing": 0, "chats_created": 37, "attachments": 33,
 "attachments_without_file": 0, "reactions": 79, "reactions_orphaned": 1}
```

`attachments` counts stored files, `attachments_without_file` attachment
rows without one.

Each import also drops empty group chats (no participants, messages,
name or background; see docs/DAEMON.md) in the same transaction: they
get no `ChatUpdated` and do not count in `chats_created`.

### PurgeSummary

```json
{"dry_run": true,
 "chats": [{"id": "46339(smsft)", "messages": 3, "reactions": 0, "attachments": 1}],
 "messages": 3, "reactions": 0, "attachments": 1, "files": 1, "handles": 1}
```

`files`: local attachment / background files of those chats (deleted
unless `dry_run`); `handles`: handle rows with an invalid URI that no
remaining chat refers to (deleted unless `dry_run`).

## Example session

```console
$ busctl --user call org.imessage.Daemon1 /org/imessage/Daemon1 org.imessage.Daemon1 GetStatus
s "{\"version\":\"0.1.0\",\"connection\":{\"state\":\"needs_login\"},\"registered\":false,\"registration\":{\"handles\":[],\"registered_at\":null,\"expires_at\":null,\"sender\":null},\"unread\":0}"

$ busctl --user get-property org.imessage.Daemon1 /org/imessage/Daemon1 org.imessage.Daemon1 ConnectionState Registered
s "needs_login"
b false

$ busctl --user --timeout=120 call org.imessage.Daemon1 /org/imessage/Daemon1 org.imessage.Daemon1 Login ss me@example.com "$PASSWORD"
s "two_factor_required"
$ busctl --user --timeout=120 call org.imessage.Daemon1 /org/imessage/Daemon1 org.imessage.Daemon1 SubmitTwoFactor s 123456
s "ok"

$ gdbus call --session --dest org.imessage.Daemon1 --object-path /org/imessage/Daemon1 \
    --method org.imessage.Daemon1.SendText "" '["bob@example.com"]' "hello from Linux" ""
('{"id":"6C1D…","chat_id":"mailto:bob@example.com","sent_at":1700000123456,"sender":{"uri":"mailto:me@example.com"}}',)

$ gdbus call --session --dest org.imessage.Daemon1 --object-path /org/imessage/Daemon1 \
    --method org.imessage.Daemon1.ListMessages mailto:bob@example.com "" 20

$ gdbus call --session --dest org.imessage.Daemon1 --object-path /org/imessage/Daemon1 \
    --method org.imessage.Daemon1.GetChat nobody
Error: GDBus.Error:org.imessage.Daemon1.Error.NotFound: not found: chat nobody

# watch everything the daemon announces
$ gdbus monitor --session --dest org.imessage.Daemon1
```

(Passing a password on the command line leaves it in your shell history
and process list; `imsg login` prompts for it instead, see docs/CLI.md.)

`busctl --user introspect org.imessage.Daemon1 /org/imessage/Daemon1`
prints the full interface.
