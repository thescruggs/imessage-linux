# imessaged D-Bus API (`org.imessage.Daemon1`)

| | |
|---|---|
| Bus | session (Windows: none, peer-to-peer, see [Peer-to-peer transport](#peer-to-peer-transport-windows)) |
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

## Peer-to-peer transport (Windows)

Windows has no session bus, so there `imessaged` serves the same object
peer-to-peer (`--transport p2p` / `IMESSAGED_TRANSPORT=p2p` selects this on
Linux too; `session` selects the bus on Windows):

* it listens on an ephemeral port on **127.0.0.1 only** and writes
  `nonce-tcp:host=127.0.0.1,port=N,noncefile=<state dir>\bus-nonce,guid=...`
  to `<state dir>\bus-address` (`%LOCALAPPDATA%\imessaged\bus-address`),
  replacing it atomically at every start and removing it at shutdown;
* `bus-nonce` holds 16 random bytes, owner-only (the per-user
  `%LOCALAPPDATA%` ACL on Windows, mode 0600 on Unix). A client must send
  them first; a connection that does not is closed before any D-Bus
  traffic (constant-time compare). The SASL step after that is `ANONYMOUS`:
  the nonce is the authentication (reading it requires being the user).
  libdbus / QtDBus and zbus clients do this for any `nonce-tcp:` address;
* every client has its own connection with the object at
  `/org/imessage/Daemon1`; every signal is sent on every live connection
  (a client that does not read for 5 s is disconnected). There is no bus
  name, `NameOwnerChanged` or activation: a client learns that the daemon
  went away when its connection closes, and reconnects by reading
  `bus-address` again (it changes at every start). Method calls may carry
  the destination `org.imessage.Daemon1`; it is ignored;
* a second daemon on the same state directory fails at start
  (`imessaged.lock` there is held, with `LockFileEx` / `flock`, from before
  the engine starts until the process has shut down: `imsg service stop`
  calls `Shutdown` and waits for the lock to be released); a
  `bus-address` left by a crashed daemon is removed before the new nonce
  is written.

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
| `GetChat` | `chat_id s` | `s` | [Chat](#chat) JSON; `NotFound` if unknown. Meant for "the user opened this chat": it also starts background refreshes (a background or group photo whose download failed, participants' shared profiles older than 24 h / never fetched / whose photo file is gone); their results arrive as `ChatUpdated` / `HandleProfileUpdated`. |
| `GetHandleProfile` | `handle s` | `s` | The name and photo `handle` shared with us: [HandleProfile](#handleprofile) JSON, or `null` (nothing shared, not fetched yet, or stopped). `InvalidHandle` for a bad handle. |
| `ListMessages` | `chat_id s`, `before_cursor s`, `limit u` | `s` | [MessagePage](#messagepage) JSON. `before_cursor` `""` = newest page; pass the previous page's `next_cursor` to page back. |
| `Search` | `query s`, `limit u` | `s` | JSON array of [Message](#message), newest first. Case-insensitive (ASCII) substring match on text and subject; `%`/`_` are literal; unsent messages excluded. |
| `SendText` | `chat_id_or_empty s`, `participants_json s`, `text s`, `reply_to s` | `s` | [SentMessage](#sentmessage) JSON. Either a chat id, or `""` plus a JSON array of handles (`["bob@example.com"]`; 2+ handles = a new or existing group; our own handles are not counted as recipients). A single one of our own handles is a note to self: sent from that handle, delivered to our other devices. `reply_to` = message id or `""`. The new row is stored and announced with `MessageReceived` (`is_from_me: true`) before the call returns. With the daemon's `link_previews` on, an iMessage whose text contains an `http(s)` link waits up to 5 s while the daemon fetches the page, then goes out with the preview (and the stored row gets a `link` part); see docs/DAEMON.md, "Link previews". |
| `SendFile` | `chat_id_or_empty s`, `participants_json s`, `path s`, `mime s` | `s` | [SentMessage](#sentmessage). `path` must be an absolute path to a regular file readable by the daemon; `mime` `""` = `application/octet-stream`. |
| `SendTextAs` | `chat_id_or_empty s`, `participants_json s`, `text s`, `reply_to s`, `service s` | `s` | `SendText` over a chosen service: `""` / `"auto"` = as `SendText` (below), `"imessage"`, or `"sms"` = a text message through the user's iPhone ([Text Message Forwarding](#text-message-forwarding)); `InvalidInput` for anything else. SMS goes to phone numbers only, needs forwarding `active` or `requested` (`Unsupported` otherwise) and carries no `reply_to`. |
| `SendFileAs` | `chat_id_or_empty s`, `participants_json s`, `path s`, `mime s`, `service s` | `s` | `SendFile` over a chosen service (`"sms"` = MMS). |
| `EnableSmsForwarding` | `enable b` | `s` | Ask the iPhone to relay text messages to this device (`true`), or stop (`false`). Returns [SmsForwarding](#smsforwarding) (`requested` / `off`); the user then turns this device on in the iPhone's Settings > Messages > Text Message Forwarding, and the state becomes `active` (`SmsForwardingChanged`). `InvalidInput` if no relay number is configured; `Unsupported` if no device of the account carries it. Slow (IDS round trips): 120 s timeout. |
| `GetSmsForwarding` | — | `s` | [SmsForwarding](#smsforwarding) JSON (also `GetStatus.sms`). |
| `Reregister` | — | `s` | Register with IDS again now and return the new [`registration`](#getstatus) JSON (as `GetStatus.registration`); picks up a handle the account gained (e.g. the phone number an iPhone linked). rustpush skips it when the identity was (re)generated less than 15 s ago (then the current registration comes back). `NotLoggedIn` without an account. Slow: 120 s timeout. |
| `React` | `chat_id s`, `message_id s`, `kind s`, `remove b` | — | `kind`: `love`, `like`, `dislike`, `laugh`, `emphasize`, `question`, `emoji:<emoji>` or a bare emoji. Stored and announced with `MessageUpdated`. `Unsupported` on a text message (SMS), like `Edit` and `Unsend`. |
| `Edit` | `chat_id s`, `message_id s`, `text s` | — | Own messages only (`InvalidInput` otherwise). `MessageUpdated` with `edited: true`. |
| `Unsend` | `chat_id s`, `message_id s` | — | Own messages only. Text, parts, attachments and reactions are dropped; `unsent: true`. |
| `SetTyping` | `chat_id s`, `typing b` | — | Sends our typing indicator. `Unsupported` in a chat whose newest message is a text message (SMS). |
| `MarkRead` | `chat_id s`, `up_to_message_id s` | — | Marks incoming messages up to and including that one read locally (`""` = all), emits `ChatUpdated`, then sends a read receipt (best effort: a failed receipt is logged, not returned; none for a text message). |
| `LookupHandles` | `handles_json s` | `s` | JSON array of handles in → `[{"handle":"mailto:bob@example.com","reachable":true}, …]` in input order. Any invalid handle fails the call (`InvalidHandle`). |
| `DownloadAttachment` | `attachment_id s` | `s` | Local path. Downloads once to `$STATE_DIR/attachments/<id>/<filename>` (file 0600, dirs 0700; filename sanitized and cut to 200 bytes); later and concurrent calls return the same path without downloading again. `NotFound` if the attachment is unknown, has no locator, or its locator is unreadable because the state key changed (see docs/DAEMON.md, recovery). Outgoing attachments (`SendFile`) return the daemon's own copy, made at send time in the same place, so deleting or moving the original does not break them (if that copy could not be made, a warning is logged and the original path is returned). |
| `ImportChatBackground` | `chat_id s`, `path s` | `s` | Set the chat's transcript background **locally only** (nothing is sent) from a file the daemon can read: a PosterKit payload as Apple sends it or a plain image (e.g. recovered from a Mac's `~/Library/Messages/TranscriptBackgroundCache`). Copied into `$STATE_DIR/backgrounds/`; id `local-…`, `set_at` = the import time, so a later change from iMessage replaces it but a late-delivered older one (e.g. a removal) does not. Returns the updated [Chat](#chat) and emits `ChatUpdated`. `NotFound` for an unknown chat, `InvalidInput` for a relative path, an unreadable file or one over 32 MiB. |
| `SetChatPinned` | `chat_id s`, `pinned b`, `order i` | `s` | Pin a chat to the top of the chat list (`pinned = true`) or unpin it, **locally only** (Messages' own pins sync through CloudKit, which we do not; `imsg sync-mac` copies the Mac's). `order` is the position among the pins, 0 = first, past the end = last; negative = last for a chat that is not pinned yet, while an already pinned chat stays where it is (only a non-negative `order` moves it). The pins are renumbered 0, 1, 2, … Returns the updated [Chat](#chat); `ChatUpdated` is emitted for every chat whose `pinned` / `pin_order` changed (none if nothing did). `NotFound` for an unknown chat. |
| `ReorderPins` | `chat_ids_json s` | `s` | Put these chats first among the pins, in this order (pinning any that are not pinned); the other pins keep their relative order after them. Duplicates count once; `[]` changes nothing. Returns a JSON array of all pinned [Chat](#chat)s in pin order; `ChatUpdated` as for `SetChatPinned`. Any unknown chat id fails the call with `NotFound` and nothing changes; `InvalidInput` for JSON that is not an array of strings. |
| `ImportMessages` | `batch_json s` | `s` | One-time history import ([ImportBatch](#importbatch) in, [ImportResult](#importresult) out); used by `imsg import-mac`. Insert-only and idempotent by message id: a message the store already has is skipped and left unchanged. Every chat id and handle must be valid (see below), else `InvalidInput` naming the first bad value and nothing is stored. Slow (copies files): call with a 120 s timeout. |
| `PurgeInvalidChats` | `dry_run b` | `s` | Maintenance (`imsg store purge-invalid-chats`): deletes every chat whose id is not a valid chat id (a `mailto:` / `tel:` URI, `group:<GUID>` or `group-p:<URIs>`; e.g. raw Mac SMS handles like `46339(smsft)` left by an old import) with its messages, reactions, attachment rows and the copied files under `$STATE_DIR/attachments/` (and background files), plus handle rows with an invalid URI no remaining chat lists. `dry_run = true` deletes nothing and reports what would go. Returns a [PurgeSummary](#purgesummary). No signals (only logged): clients should reload their chat list afterwards. |
| `Version` | — | `s` | daemon version, e.g. `"0.1.0"` |
| `Shutdown` | — | — | stop the daemon as SIGTERM does (engine closes APNs / IDS and saves its state, the store is closed); replies first, the process exits a few seconds later. For Windows, where nothing else stops it gracefully (`imsg service stop` / `uninstall`). On Linux harmless: the systemd unit has `Restart=on-failure`, so a clean exit stays stopped until `systemctl --user start` or D-Bus activation |

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
| `ChatUpdated` | `chat_json s` | Chat created, renamed, membership changed, new activity, unread count changed, pinned / unpinned / moved among the pins. |
| `Typing` | `chat_id s`, `handle s`, `typing b` | Someone started/stopped typing. A start without refresh is cleared after 60 s, and a message from that person clears it. In memory only. |
| `ConnectionStateChanged` | `state s` | Same value as the `ConnectionState` property. |
| `TwoFactorRequired` | — | A login (interactive or the automatic re-login after IDS error 6005) needs a code: call `SubmitTwoFactor`. |
| `BackgroundError` | `message s` | Background problems not tied to a method call (registration rejected, state save failed, task restarted, events dropped — resync). Method failures are D-Bus errors instead (below). |
| `SmsForwardingChanged` | `state_json s` | Text Message Forwarding changed state ([SmsForwarding](#smsforwarding)): requested, confirmed by the iPhone / first relayed text (`active`), turned off on the iPhone, or an error. |
| `HandleProfileUpdated` | `handle s`, `profile_json s` | A contact's shared name / photo was fetched or changed ([HandleProfile](#handleprofile)), or `null`: they stopped sharing. Followed by `ChatUpdated` for every chat with that participant (their `profile` in it changed). |

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
  "unread": 3,
  "profiles": {"state": "ready"},
  "sms": {"state": "active", "number": "tel:+15551234567", "since": 1700000100000},
  "mac_host": "mac.local"
}
```

`sms` is [SmsForwarding](#smsforwarding) (older daemons omit it).

`mac_host` is the daemon's `mac_host` setting (docs/DAEMON.md,
Configuration): the ssh destination of the user's Mac that `imsg
sync-mac` (and the kmessages "Sync from Mac" button) copies contacts and
pins from when no host is given; `null` when unset (older daemons omit
it). The daemon itself never connects to it.

`profiles` says whether contacts' shared names and photos can be fetched
(iCloud / CloudKit access): `{"state": "ready"}`, `"pending"` (not tried
yet), `"needs_login"` (Apple wants a fresh sign-in: `imsg login` once;
messaging is unaffected) or `"unavailable"`, the last two with a
`"detail"`; older daemons omit it. See docs/DAEMON.md, "Shared names and
photos".

`connection` is `{"state": "<name>"}` or, for errors,
`{"state": "error", "detail": "registration failing, retrying: …"}`.
Our handles are `registration.handles`: the account's email addresses
and, when an iPhone on the account has linked it, its phone number.
`registration.sender` is the handle new conversations start from (replies
in a known chat use the handle of ours that chat names): the daemon's
`sender_handle` setting if it is registered (an email or the number), else
the first email handle (`null` when not registered; see docs/DAEMON.md,
Configuration). `registration.other_handles` (array of
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
  "participants": [{"uri": "mailto:bob@example.com",
                    "profile": {"name": "Bobby", "first": "Bob", "last": "Smith",
                                "photo": "/home/me/.local/state/imessaged/profiles/mailto_bob@example.com/Zm9v….jpg",
                                "updated_at": 1700000050000}},
                   {"uri": "tel:+15551234567"}],
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
  },
  "icon_path": "/home/me/.local/state/imessaged/chat_icons/group_5B0E…/6F1A….jpg",
  "pinned": true,
  "pin_order": 0
}
```

`pinned` says whether the chat is pinned to the top of the chat list and
`pin_order` is its position among the pins (0 = first; `null` when not
pinned). Pins are local to this computer (`SetChatPinned`,
`ReorderPins`); clients show pinned chats first in `pin_order`, then the
rest by `last_message_at`. `ListChats` keeps its newest-activity order.
Older daemons omit both (read them as `false` / `null`).

`icon_path` is the group's photo (Messages "Change Name and Photo" on a
group), absent / `null` for none or until it is downloaded. A change
arrives as `ChatUpdated` without it (stored), then with it (downloaded);
removal clears it.

Each participant may carry `profile`: the name and photo they shared with
us ([HandleProfile](#handleprofile)); absent when they share nothing (or
it was not fetched yet). Clients should prefer the user's own address
book and fall back to it.

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
    {"type": "unsupported", "description": "app message"},
    {"type": "link", "url": "https://example.com/story", "original_url": "https://exam.pl/s1", "title": "A story", "summary": "What happened",
     "site_name": null, "image_attachment_id": "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0:link-image",
     "icon_attachment_id": null}
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
  "verified": true,
  "service": "imessage"
}
```

* `service`: `"imessage"`, or `"sms"` for a text message (SMS / MMS /
  RCS) relayed by the user's iPhone; shown as a green bubble. Missing in
  older payloads = `"imessage"`. A text message and an iMessage with the
  same number share one 1:1 chat (`tel:…`); a group text is
  `group-p:<sorted numbers>`. Texts have no tapbacks, edits or unsend.

* `delivery`: `pending`, `sent`, `delivered`, `read`, `failed`. It only
  moves forward; `failed` only replaces `pending`/`sent`.
* `verified: false` = the message decrypted but the sender's signing key
  could not be verified: show the sender as **unverified**.
* `reactions` lists current reactions only (one per sender; a new one
  replaces the old). `kind` is a string for the classic tapbacks and
  `{"emoji": "…"}` for emoji reactions.
* `text` concatenates the text and mention parts; `null` if none.
* `link`: a link preview (Messages' URL balloon), after the text parts.
  Only `url` (the page, after redirects) is always there;
  `original_url` (the URL as written in the message, when the sender's
  metadata has one), `title`, `summary`, `site_name` (from the
  page's `og:site_name`; only on previews this daemon fetched, else show
  the URL's host) and the attachment ids may be `null` or missing. The
  image and icon are ordinary inline attachments of the message, ids
  `<message id>:link-image` / `<message id>:link-icon`, with **no**
  `attachment` part of their own (clients that list attachments skip
  them); `DownloadAttachment` returns their files at once (the daemon
  writes them when the message arrives). The message's `text` keeps the
  URL: a balloon sent without text gets the URL as `text` (and a text
  part), so search, snippets and the CLI keep working. The sender built
  the preview; the daemon never fetches anything for a received link
  (see docs/DAEMON.md, "Link previews"). Show the card instead of the
  text when the text is a single `http(s)` token (the card's `url` may
  differ from it after redirects). An edit that removes the URL from the
  text (neither `url` nor `original_url` left in it) drops the `link`
  part. A preview's icon is always PNG or JPEG.
* An unsent message keeps its id/sender/time with `text: null`,
  `parts: []`, `unsent: true`.

### SmsForwarding

```json
{"state": "requested", "number": "tel:+15551234567", "since": 1700000100000}
```

`state`: `off`, `requested` (asked the iPhone; waiting for the user to
turn this device on under Settings > Messages > Text Message Forwarding),
`active` (the iPhone confirmed, or relayed a text), `error` (with
`"detail"`: the request could not be sent, e.g. no device of the account
carries the number). `number` is the iPhone number texts go through
(daemon setting `sms_relay_number`, else the first phone number in
`other_handles`; `null` if none). `since` = ms of the last change.

#### Text Message Forwarding

The daemon relays SMS / MMS / RCS through the user's iPhone like a Mac
does. Once `active`: incoming texts arrive as ordinary `MessageReceived`
rows with `service: "sms"`, texts sent from the iPhone itself as
`is_from_me` rows, and `SendText` / `SendFile` to a phone number that is
not on iMessage go out as texts (`SendTextAs` / `SendFileAs` choose
explicitly). The iPhone confirms each text it sends; a failure turns the
row's delivery to `failed`. Details: docs/DAEMON.md, "Text Message
Forwarding".

### HandleProfile

```json
{"name": "Bobby", "first": "Bob", "last": "Smith",
 "photo": "/home/me/.local/state/imessaged/profiles/mailto_bob@example.com/Zm9v….jpg",
 "updated_at": 1700000050000}
```

What a contact shared (Messages "Share Name and Photo"). Every field may be
`null`: `name` is the display name they chose (else show `first last`),
`photo` an absolute path to a JPEG / PNG / HEIC (file 0600), `updated_at`
the fetch time in ms. A new record replaces the photo file (the old path
stops existing).

### MessagePage

```json
{"messages": [ /* Message, oldest first */ ], "next_cursor": "1700000000000-42"}
```

`next_cursor` is opaque (`null` on the oldest page). Pages are keyed on
`(sent_at, insertion order)`, so they stay stable while new messages
arrive.

### SentMessage

```json
{"id": "6C1D…", "chat_id": "group:5B0E2C1A-…", "sent_at": 1700000123456, "sender": {"uri": "mailto:me@example.com"},
 "service": "imessage"}
```

`sender` is the handle the message was actually sent from (also the
stored row's `sender`; for a text message, the iPhone's number).
`service` says how it went out (`SendText` picks SMS by itself when Text
Message Forwarding is active and a phone-number recipient is not on
iMessage).

For a new group (`participants_json` with several handles) this is where
the minted `group:<GUID>` chat id is learned.

### ImportBatch

```json
{
  "chats": [ /* Chat (unread_count is ignored) */ ],
  "messages": [
    {"message": { /* Message */ },
     "attachments": [{"id": "<message id>:1", "mime": "image/heic", "filename": "IMG_1.HEIC", "size": 1234,
                      "source_path": "/home/me/.cache/imsg/import/…/IMG_1.HEIC"}],
     "link": {"payload": "<base64 RichLink keyed archive>",
              "images": ["/home/me/.cache/imsg/import/…/x.pluginPayloadAttachment", null]}}
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
  an optional `link` (a Mac link preview: `chat.db`'s `payload_data`
  base64, at most 1 MiB, and the message's hidden preview files in
  `chat.db` order, absolute paths or `null`) is decoded into a `link`
  part after the text, its image / icon (the archive's indexes into
  `images`; regular files up to 8 MiB) stored as inline attachments with
  their files copied; the URL becomes the text of a message without
  one; an unreadable archive is logged and an otherwise empty message
  becomes `unsupported` ("app message");
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
