# imsg-bridge: imessaged over the network (HTTP + WebSocket, API v1)

`imsg-bridge` is a small service that runs next to `imessaged` on the Linux
host. It is an ordinary D-Bus client of `org.imessage.Daemon1`
([DBUS.md](DBUS.md)) and re-exposes a messaging subset of it over HTTP and
one WebSocket event stream, for clients on other machines (the portable
Windows client `imsg-app`, see [PORTABLE-WINDOWS.md](PORTABLE-WINDOWS.md)).
The daemon is not changed and knows nothing about it.

It listens on the host's **Tailscale addresses only** (never `0.0.0.0`) and
requires both a known tailnet peer and a bearer token on every request.
Traffic is encrypted by WireGuard, so the bridge speaks plain HTTP.

## Running

```console
$ imsg-bridge token new work-stick        # prints a pairing string, once
imsg+bridge://100.64.0.10:7227#Tq3…
$ systemctl --user enable --now imsg-bridge
$ imsg-bridge token list
$ imsg-bridge token revoke work-stick
```

| Path | What |
|---|---|
| `~/.config/imsg-bridge/config.toml` | configuration (optional; all keys have defaults) |
| `~/.local/state/imsg-bridge/tokens.toml` | token hashes (0600; `tokens.lock` next to it serialises writers) |
| `~/.local/state/imsg-bridge/access.log` | one line per authenticated TCP connection / refused request: time, token name, peer, method and path (no query string, no bodies) |
| `~/.cache/imsg-bridge/uploads/` | uploads in flight (0700), deleted after the send |

`--config <file>` (or `IMSG_BRIDGE_CONFIG`) selects another config file;
the other paths follow `XDG_STATE_HOME` / `XDG_CACHE_HOME`. `serve` is the
default command; `serve --bus-address <addr>` (or `IMSG_BUS_ADDRESS`) uses
another D-Bus bus (tests). The unit is `packaging/systemd/imsg-bridge.service`.

### config.toml

```toml
# "tailscale" = this host's Tailscale IPv4 + IPv6 (looked up through
# tailscaled's LocalAPI; retried every 5 s until the interface is up).
# Explicit "ip:port" entries are also accepted (127.0.0.1:7227 for tests);
# 0.0.0.0 / [::] are refused. Once listening, the addresses are re-checked
# every 60 s; if they changed the bridge exits and systemd restarts it.
listen = ["tailscale"]
port = 7227
# A request is allowed only if the tailnet peer (LocalAPI whois of the source
# address) has one of these login names, or its node carries one of these tags.
# Defaults: allowed_logins = [this host's own tailnet login, from the
# LocalAPI], allowed_tags = ["tag:imsg-portable"].
allowed_logins = ["scruggs@thescruggs.org"]
allowed_tags = ["tag:imsg-portable"]
# false only for tests / loopback listeners: skip the whois check.
require_whois = true
max_upload_mib = 100
# Defaults: imessaged's state dir and the KPeople vCard folder (or IMSG_CONTACTS_DIR).
# daemon_state_dir = "~/.local/state/imessaged"
# contacts_dirs = ["~/.local/share/kpeoplevcard"]
```

## Authentication

Every HTTP request and the WebSocket upgrade must carry
`Authorization: Bearer <token>`. Tokens are 32 random bytes, base64url without
padding (43 characters); only their SHA-256 is stored. Comparison is
constant-time; a failed attempt is answered after a 1 s delay. The running
bridge re-reads `tokens.toml` when it changes, so `token revoke` applies
from the next request on; `last_used` / `last_peer` are written at most
once a minute per token (or when the peer changes).

With `require_whois`, the source address is also looked up with tailscaled's
LocalAPI (`GET /localapi/v0/whois?addr=<ip:port>` on
`/run/tailscale/tailscaled.sock`, cached 60 s); the request is refused with
403 unless `UserProfile.LoginName` is in `allowed_logins` or `Node.Tags`
intersects `allowed_tags`. Tagged nodes report the placeholder login
`tagged-devices`, which never matches; they are allowed by tag only. If
the LocalAPI cannot be reached the request gets 503 `Transient`.

**Pairing string:** `imsg+bridge://<host>:<port>#<token>`. The client stores
`http://<host>:<port>` as the bridge URL and the token as its secret.

**Not exposed** (on purpose; a lost client must not be able to wipe or log
out the account): `Login`, `Logout`, `Shutdown`, `Reregister`,
`EnableSmsForwarding`, `ImportMessages`, `ImportChatBackground`,
`PurgeInvalidChats`.

## Conventions

* Base URL `http://<host>:7227/v1`. JSON bodies are UTF-8,
  `Content-Type: application/json`.
* Responses marked **verbatim** are the daemon's JSON reply, unchanged
  (the model types in `crates/imessage-common/src/model.rs` and
  `crates/imessage-common/src/api.rs`).
* Ids in paths (chat ids such as `mailto:bob@example.com`, `tel:+1555…`,
  `group:<GUID>`; attachment ids `<message id>:<part>`; handles) are one
  **percent-encoded path segment** (encode everything except unreserved
  characters `A-Z a-z 0-9 - . _ ~`).
* Errors: an HTTP status and `{"error": "<Name>", "message": "<text>"}`.
  `<Name>` is the daemon's error short name (DBUS.md, Errors) or one of the
  bridge's own. Clients should act on `error`, not on the status.

| Status | `error` |
|---|---|
| 400 | `InvalidInput`, `InvalidHandle`, `BadRequest` (bridge: malformed request) |
| 401 | `Unauthorized` (missing / unknown token) |
| 403 | `Forbidden` (not an allowed tailnet peer) |
| 404 | `NotFound`, `ChatNotFound` (also: unknown endpoint, no photo / icon / background, a file outside the state dir) |
| 409 | `NotLoggedIn`, `NoHardwareConfig`, `NoPendingTwoFactor`, `BadTwoFactorCode`, `Unsupported` |
| 413 | `TooLarge` (upload over `max_upload_mib`) |
| 416 | `RangeNotSatisfiable` (file endpoints; with `Content-Range: bytes */<size>`) |
| 502 | any other daemon error (`Login`, `Protocol`, `Store`, `State`, …), or `DaemonError` (bridge: malformed daemon reply, unknown D-Bus error) |
| 503 | `NoDaemon` (imessaged not reachable on the bus), `Transient` |
| 504 | `Timeout` (a daemon call exceeded its timeout; it may still finish) |

Daemon calls use a 25 s timeout, except sending a file,
`SubmitTwoFactor` and `DownloadAttachment` (120 s). `limit` values above
the maximum are capped; `0` or absent means the default.

## Endpoints

| Method + path | Request | Response |
|---|---|---|
| `GET /v1/version` | — | `{"api": 1, "bridge": "0.1.14", "daemon": "0.1.14"}` (`daemon` `null` if unreachable) |
| `GET /v1/status` | — | verbatim `GetStatus` ([Status]) |
| `GET /v1/chats?limit=&offset=` | limit default 50, max 500 | verbatim `ListChats` (`[Chat]`, newest activity first) |
| `GET /v1/chats/{chat_id}` | — | verbatim `GetChat` |
| `GET /v1/chats/{chat_id}/messages?before=&limit=` | `before` = previous page's `next_cursor` (absent = newest page); limit default 60, max 500 | verbatim `ListMessages` ([MessagePage]) |
| `GET /v1/search?q=&limit=` | limit default 100, max 500 | verbatim `Search` (`[Message]`) |
| `POST /v1/send` | `{"chat_id": "…"}` **or** `{"participants": ["bob@example.com", …]}`, plus `"text": "…"`, optional `"reply_to": "<message id>"`, optional `"service": "auto"\|"imessage"\|"sms"` | verbatim `SendTextAs` ([SentMessage]) |
| `POST /v1/send-file?chat_id=…` or `?to=…&to=…`, plus `&filename=…&mime=…&service=…` | raw file bytes as the body (≤ `max_upload_mib`; a larger `Content-Length` is refused before reading). `filename` is sanitised (last path component, at most 200 bytes; default `file`) and becomes the attachment's name; `mime` absent = guessed from the extension | verbatim `SendFileAs` ([SentMessage]) |
| `POST /v1/chats/{chat_id}/messages/{message_id}/react` | `{"kind": "love", "remove": false}` | `204` |
| `POST /v1/chats/{chat_id}/messages/{message_id}/edit` | `{"text": "…"}` | `204` |
| `POST /v1/chats/{chat_id}/messages/{message_id}/unsend` | — | `204` |
| `POST /v1/chats/{chat_id}/typing` | `{"typing": true}` | `204` |
| `POST /v1/chats/{chat_id}/read` | `{"up_to": "<message id>"}` (absent / `""` = all) | `204` |
| `POST /v1/chats/{chat_id}/pin` | `{"pinned": true, "order": 0}` (`order` absent / `null` = last for a new pin) | verbatim `SetChatPinned` (Chat) |
| `POST /v1/pins` | `{"chat_ids": ["…", …]}` | verbatim `ReorderPins` (`[Chat]`) |
| `POST /v1/lookup` | `{"handles": ["…", …]}` | verbatim `LookupHandles` (`[Reachability]`) |
| `GET /v1/handles/{handle}/profile` | — | verbatim `GetHandleProfile` (HandleProfile or `null`) |
| `GET /v1/sms` | — | verbatim `GetSmsForwarding` |
| `POST /v1/two-factor` | `{"code": "123456"}` | `{"result": "ok" \| "two_factor_required"}` |
| `GET /v1/contacts` | — | `[{"name": "Bob Smith", "phones": ["+1 555…"], "emails": ["bob@example.com"]}, …]`, from the host's vCards (`imessage_common::contacts`); `ETag` from the files' newest mtime + count, `If-None-Match` → 304 |

### Files

| Method + path | Response |
|---|---|
| `GET /v1/attachments/{attachment_id}` | the file (`DownloadAttachment`, 120 s timeout: may download from Apple first) |
| `GET /v1/handles/{handle}/photo` | the contact's shared photo (`GetHandleProfile.photo`), 404 if none |
| `GET /v1/chats/{chat_id}/icon` | the group photo (`Chat.icon_path`), 404 if none |
| `GET /v1/chats/{chat_id}/background` | the transcript background picture (`Chat.background.path`), 404 if none |

`Content-Type` is guessed from the file extension (`application/octet-stream`
otherwise), `Content-Disposition: inline; filename*=UTF-8''<name>`,
`Content-Length`, a weak `ETag` from size + mtime (`If-None-Match` → 304),
`Cache-Control: private, max-age=86400`. Single `Range: bytes=a-b` requests
are honoured (206).

**Containment rule:** the bridge only ever serves a path the daemon returned,
and only if its canonical form (symlinks resolved) lies under the daemon's
state dir (otherwise 404 `NotFound`, logged as a warning). Clients never
send paths.

Uploads are written to `uploads/<random>/<filename>` (0700 / 0600) and the
directory is removed when `SendFileAs` returns. If the call timed out (the
daemon may still be reading the file) it is removed 10 minutes later
instead; whatever a crash left there is removed at start.

## Events: `GET /v1/events` (WebSocket)

Upgrade with the same `Authorization` header. The server sends text frames,
each one JSON object with a `type`; it ignores anything the client sends
except pong / close. It pings every 30 s; a client whose socket is not
readable for 10 s (a frame cannot be sent within 10 s) is dropped, and so
is one that sent nothing, not even a pong, for 75 s.

**Slow clients:** all sockets share one D-Bus subscription through a
1024-frame buffer. A socket that falls further behind is **closed** with
close code 1013 (try again later), reason `lagged: reconnect and resync`;
no frame tells it what it missed. Clients treat any close like a lost
connection: reconnect (with backoff) and resync.

| `type` | Fields | From |
|---|---|---|
| `hello` | `bridge` (version), `daemon_connected` (bool) | first frame after the upgrade |
| `message_received` | `chat_id`, `message` (Message) | `MessageReceived` |
| `message_updated` | `chat_id`, `message` (Message) | `MessageUpdated` |
| `chat_updated` | `chat` (Chat) | `ChatUpdated` |
| `typing` | `chat_id`, `handle`, `typing` | `Typing` |
| `connection_state` | `state` | `ConnectionStateChanged` |
| `two_factor_required` | — | `TwoFactorRequired` |
| `background_error` | `message` | `BackgroundError` |
| `handle_profile_updated` | `handle`, `profile` (HandleProfile or `null`) | `HandleProfileUpdated` |
| `sms_forwarding_changed` | `state` (SmsForwarding) | `SmsForwardingChanged` |
| `daemon` | `state`: `"lost"` \| `"connected"`, optional `detail` | the bridge's own D-Bus connection dropped / came back, or (session bus) `org.imessage.Daemon1` lost / regained its owner, e.g. a daemon restart. Sent on changes only; `hello.daemon_connected` has the state at connect time |

Payload objects are the daemon's signal JSON, embedded as JSON (not as a
string). Frames are sent in the order the daemon emitted the signals.

**Resync rule (clients):** after `hello`, after `daemon` `connected`, and
after reconnecting the WebSocket, anything may have been missed: reload the
status, the chat list and the open chat's newest page.

[Status]: DBUS.md#status
[MessagePage]: DBUS.md#messagepage
[SentMessage]: DBUS.md#sentmessage
