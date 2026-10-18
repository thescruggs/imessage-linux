# `imsg` command-line client

`imsg` is a thin client for the `imessaged` D-Bus API
([docs/DBUS.md](DBUS.md)). It does not link the protocol engine: every
messaging command is one or two calls on `org.imessage.Daemon1`, so the
daemon must be running (or D-Bus-activatable, see the README). Only
`hwconfig import` works without it.

```console
$ imsg login --apple-id me@icloud.com
Password for me@icloud.com:
A verification code was sent to your trusted devices.
Two-factor code: 123456
logged in; connection connected; iMessage handles: me@icloud.com

$ imsg send bob@example.com "hello from Linux"
sent 6C1D2E3F-… to mailto:bob@example.com

$ imsg chats
ID                                          NAME             LAST       UNREAD
mailto:bob@example.com                      bob@example.com  14:03      1
group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40  Team             Tue 09:12

$ imsg messages bob@example.com -n 3
14:01 me: hello from Linux  [delivered]
      id 6C1D2E3F-…
14:03 bob@example.com: look at this  ❤️1
      📎 IMG_0001.jpeg (179.1 KB, image/jpeg)  id 0F1E2D3C-…:1
      id 0F1E2D3C-…
older: imsg messages mailto:bob@example.com --before 1700000000000-42
```

## Global options

| Option | Env | Meaning |
|---|---|---|
| `--json` | | Print the daemon's JSON unchanged (see [JSON mode](#json-mode)). |
| `-q`, `--quiet` | | No confirmations, hints or progress lines; only requested data and errors. |
| `--bus-address ADDR` | `IMSG_BUS_ADDRESS` | Talk to this D-Bus address instead of the session bus (tests, containers). |
| `--state-dir DIR` | `IMESSAGED_STATE_DIR` | Daemon state directory, for `hwconfig import` and `doctor`. Default `$XDG_STATE_HOME/imessaged`. |

Global options may appear anywhere (`imsg status --json` = `imsg --json status`).
Colour is decided per stream: stdout output is coloured only when stdout is
a terminal, stderr hints only when stderr is a terminal, and never when
`NO_COLOR` is set.
`RUST_LOG=debug` shows client-side logging (zbus); passwords and codes are
never logged or printed.

## Arguments: chats, handles, ids

* **Handle**: an email address or phone number in any form
  (`Bob@Example.com`, `mailto:bob@example.com`, `+1 (555) 123-4567`); it is
  normalized to an IDS URI. For commands that need an existing chat
  (`messages`, `react`, `edit`, `unsend`, `read`) a handle selects the 1:1
  chat with that person (its id is the URI).
* **Chat**: a chat id as printed by `imsg chats` (`mailto:…`, `tel:…`,
  `group:<GUID>`), a handle, or a **name**: an exact (case-insensitive) chat
  name, else a unique prefix of a chat id, group GUID or name
  (`imsg messages team`, `imsg messages 5B0E`). Ambiguous names fail with
  exit code 2 and list the candidates.
* **Message id**: the upper-case GUID shown as `id …` under each message.
  **Attachment id**: `<message id>:<part>`, shown after the 📎 line.

## Commands

### `login [--apple-id ID] [--password-stdin] [--code-stdin] [--code-only]`

Signs in and registers with iMessage. Prompts for the Apple ID (if not
given), the password (hidden, read from the terminal) and, when Apple asks,
the two-factor code. A wrong code may be retried: three wrong codes end the
command with exit code 4. If Apple asks for another code after a correct
one, `imsg` prompts again. `imsg` waits up to 120 s for the daemon's
answer to the login, each code and `send --file` (the daemon may first run
the anisette command over ssh); after that it exits with code 7 and the
daemon carries on (`imsg status` shows the outcome). An unreachable
anisette source fails the login at once with
`anisette command failed: ...` (exit code 7).

Non-interactive use: `--password-stdin` reads the password from the first
line of stdin (requires `--apple-id`), `--code-stdin` reads codes from the
following lines, one per attempt. `--password-stdin` refuses to run when
stdin is a terminal (exit code 2): pipe the password or drop the flag.

```sh
printf '%s\n' "$PW" | imsg login --apple-id me@icloud.com --password-stdin
printf '%s\n%s\n' "$PW" "$CODE" | imsg login --apple-id me@icloud.com --password-stdin --code-stdin
```

`--code-only` submits a code for a login that is already waiting for one,
e.g. after the daemon's automatic re-login emitted `TwoFactorRequired`
(`imsg watch` prints it). With `--json`, prints the `GetStatus` JSON after
logging in.

Note: the password and code travel over the session bus to the daemon (see
[docs/DBUS.md](DBUS.md), "What crosses the bus").

### `logout`

Forgets the account locally (state becomes `needs_login`); message history
is kept. Does not deregister from Apple.

### `status`

```console
$ imsg status
daemon         0.1.0
connection     connected
registered     yes
handles        +15551234567, me@icloud.com, me@example.com
sends as       me@example.com
registered at  2026-10-11 09:00 +02:00
renews at      2026-11-10 09:00 +01:00
unread         3
profiles       ready
sms            active (via +15551234567)
```

`sms` (newer daemons only): Text Message Forwarding, see [`sms`](#sms-enabledisablestatus).

`profiles` (newer daemons only): whether contacts' shared names and
photos can be fetched: `ready`, `not tried yet`, `unavailable (…)`, or
"needs `imsg login`" when Apple wants a fresh sign-in for iCloud (run it
once; messaging keeps working meanwhile).

`handles` are the addresses this computer is registered for: your Apple
ID's email addresses and, when an iPhone signed in to the account has
linked it, your phone number. `sends as` is the handle new conversations
start from (a reply in a known chat goes out from the handle of yours that
chat names, e.g. the address the other person wrote to): the daemon's
`sender_handle` setting (`config.toml`, `--sender-handle`,
`IMESSAGED_SENDER_HANDLE`) if it is registered, else the first email
handle. `also mine` (only when set) is the daemon's `other_handles`:
addresses of yours this computer is not registered for; messages from
them count as yours.

`--json`: the `GetStatus` JSON.

### `register`

Registers with iMessage again right now and prints the status. Normally
not needed (the daemon renews its registration by itself); use it to pick
up an address the account gained, such as your phone number after an
iPhone linked it, without waiting for the renewal. Takes a few seconds.
`--json`: the new `registration` object of `GetStatus`.

### `chats [-n/--limit N] [--offset N]`

Conversations, most recent activity first: id, name (or participants),
last activity, unread count. Default 50, maximum 500 per call.

### `messages CHAT [-n/--limit N] [--before CURSOR] [-r/--reverse]`

A page of messages, oldest first (`--reverse`: newest first). Each message
shows time, sender (`me` for your own; `(unverified)` when the sender's key
could not be verified), text, reactions (`❤️2 👍1`), `[edited]`, `(unsent)`,
delivery state of your own messages (`[sent]`, `[delivered]`, `[read]`,
`[FAILED]`), `[SMS]` for a text message relayed by your iPhone, attachments with their ids, the message id and the
reply-to id. When older messages exist, the command for the next page is
printed on stderr (`--before CURSOR`). Times: today `14:03`, last week
`Tue 14:03`, this year `Mar 05 14:03`, else `2025-03-05 14:03` (local time).

`--json`: the `MessagePage` JSON unchanged (not reversed).

### `send TO TEXT... [--reply-to MSG_ID] [--sms | --imessage]`

`TO` is a chat (id or name), a handle, or several handles separated by
commas: `alice@example.com,+15551234567` starts (or reuses) a group with
those people. One of your own handles alone is a note to self (shows up
on your other devices, like "Note to Self" in Messages). Words of `TEXT` are joined with spaces; `-` reads the text
from stdin (one trailing newline removed). Text starting with `-` needs
`--` first: `imsg send bob@example.com -- -5 degrees`.

```sh
imsg send bob@example.com "on my way"
imsg send alice@example.com,bob@example.com dinner at 8?
fortune | imsg send bob@example.com -
imsg send bob@example.com "agreed" --reply-to 0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0
```

Prints `sent <id> to <chat id>` (plus `as a text message (via your
iPhone)` when it went out as SMS); `--json`: the `SentMessage` JSON (this
is where a new group's `group:<GUID>` id is learned; `service` says
`imessage` or `sms`).

With Text Message Forwarding active (`imsg sms enable`), a message to a
phone number that is not on iMessage goes out as a text message through
your iPhone by itself. `--sms` forces a text message (phone numbers only;
`--reply-to` is ignored), `--imessage` forces iMessage:

```sh
imsg send --sms +15555550123 "running late"
```

### `send-file TO PATH [--mime TYPE] [--sms | --imessage]`

Sends a file. The path is made absolute; the daemon reads the file
itself, so it must be readable by the daemon (the systemd unit has
`PrivateTmp=yes`: files in `/tmp` are **not** visible to it, use your home
directory). The MIME type is guessed from common extensions (jpeg, png,
heic, gif, mov, mp4, m4a, pdf, …) unless `--mime` is given; otherwise the
daemon uses `application/octet-stream`. `--sms` sends it as a picture /
MMS message through your iPhone, `--imessage` forces iMessage (default:
as `send`).

### `sms enable|disable|status`

Text Message Forwarding: SMS, MMS and RCS relayed by your iPhone, like on
a Mac. The daemon needs your iPhone's number (`sms_relay_number` in
`config.toml`, else the first phone number in `other_handles`).

```console
$ imsg sms enable
state   requested (via +15551234567)
number  +15551234567
since   2026-10-17 16:28 -05:00
now, on your iPhone: Settings > Messages > Text Message Forwarding, and turn this device on.
```

1. `imsg sms enable` asks the iPhone to relay texts here (state
   `requested`).
2. On the iPhone open **Settings > Messages > Text Message Forwarding**
   and turn on this computer (it is listed under the Mac model name of
   the imported hardware identity).
3. `imsg sms status` shows `active` once the iPhone confirms or relays its
   first text. Incoming texts then appear in `imsg messages` / `watch`
   marked `[SMS]`, in the same chat as iMessages with that number.

`imsg sms disable` stops it (and tells the iPhone). `--json` prints the
`SmsForwarding` JSON. `imsg watch` shows state changes as `sms forwarding
…` lines.

### `react CHAT MSG_ID KIND [--remove]`

`KIND`: `love`, `like`, `dislike`, `laugh`, `emphasize`, `question`
(aliases `heart`, `thumbsup`/`+1`, `thumbsdown`/`-1`, `haha`/`lol`, `!!`,
`?`) or any emoji (`🔥`, or `emoji:🔥`). `-1` needs no `--`
(`imsg react bob@example.com <MSG_ID> -1`). A new reaction from you replaces
your previous one; `--remove` takes it back.

### `edit CHAT MSG_ID TEXT...`

Edits one of your own messages (`-` reads stdin).

### `unsend CHAT MSG_ID`

Unsends one of your own messages.

### `typing CHAT on|off`

Sends your typing indicator. `CHAT` may be a handle without history.

### `read CHAT [UP_TO_MSG_ID]`

Marks incoming messages read (all, or up to and including that message)
and sends a read receipt.

### `lookup HANDLE...`

Whether each handle is reachable on iMessage. `--json`: the
`LookupHandles` array.

### `profile HANDLE`

The name and photo `HANDLE` shared with you (Messages "Share Name and
Photo"), as the daemon fetched it:

```console
$ imsg profile bob@example.com
handle   mailto:bob@example.com
name     Bobby
first    Bob
last     Smith
photo    /home/me/.local/state/imessaged/profiles/mailto_bob@example.com/Zm9v….jpg
updated  2026-10-17 15:02 -05:00
```

`HANDLE has no shared name or photo` when there is none (not shared, not
fetched yet, or they stopped). `--json`: the `GetHandleProfile` JSON
(`HandleProfile` or `null`). Without an address book, `imsg` also uses
shared names in chat names (`imsg chats`, message senders).

### `search QUERY [-n/--limit N]`

Case-insensitive substring search over message text and subject, newest
first, with the chat id shown on each message.

### `chat CHAT`

One chat's details: id, name, participants (with the name each shared,
`mailto:bob@example.com (Bobby)`), last activity, unread count, its
transcript background (id, version, who set it, the image file), the
group photo's file and the participants' shared photos.

### `chat-background CHAT --import FILE`

Set the chat's background **locally only** (nothing is sent) from a
PosterKit payload or plain image, e.g. one recovered from a Mac's
`~/Library/Messages/TranscriptBackgroundCache/<background id>` (the id is
`backgroundProperties.trabaid` in the chat's `chat.db` `properties`
plist). FILE must be readable by the daemon (not under `/tmp`: the
systemd unit has a private `/tmp`). A later background change from
iMessage replaces it. Setting or clearing backgrounds over iMessage is
not supported yet. To pull every chat's background from a Mac in one go,
see `import-mac HOST --backgrounds`.

### `download ATTACHMENT_ID`

Downloads an attachment (once) and prints its local path (under the
daemon's state directory). `--json`: the path as a JSON string.

### `watch [--chat CHAT]`

Streams daemon events until Ctrl-C: new messages, updates (delivery,
reactions, edits, unsends), chat changes, typing, connection state,
two-factor requests, background errors and shared-profile changes
(`HandleProfileUpdated`; not shown with `--chat`). `--chat` keeps only events for
one chat (account-wide events are always shown).

`--json` prints one JSON object per line, with the daemon's payload
embedded verbatim:

```json
{"signal":"MessageReceived","chat_id":"mailto:bob@example.com","message":{…Message…}}
{"signal":"MessageUpdated","chat_id":"mailto:bob@example.com","message":{…Message…}}
{"signal":"ChatUpdated","chat":{…Chat…}}
{"signal":"Typing","chat_id":"mailto:bob@example.com","handle":"mailto:bob@example.com","typing":true}
{"signal":"ConnectionStateChanged","state":"connected"}
{"signal":"TwoFactorRequired"}
{"signal":"BackgroundError","message":"…"}
```

### `import-mac HOST [--days N] [--since DATE] [--max-attachment-mb MB] [--max-total-mb MB] [--imessage-only] [--dry-run]`

One-time import of recent history from a Mac's Messages database
(`~/Library/Messages/chat.db`) into the daemon's store.

* `HOST`: ssh destination of the Mac (`mac.local`, `user@mac`, a `Host`
  alias). Run as `ssh [-F ~/.ssh/config] -o BatchMode=yes -o LogLevel=ERROR
  HOST python3 - …`, so key-based login must work without a prompt; the
  ssh session needs Full Disk Access on the Mac. Needs the Mac's
  `/usr/bin/python3` (Command Line Tools) and `tar` locally.
* `--days N` (default 10) or `--since DATE` (`YYYY-MM-DD` = local
  midnight, or RFC 3339): what to import, by message date.
* `--max-attachment-mb MB` (default 50): larger files are not copied (the
  message keeps an attachment part without a file).
* `--max-total-mb MB` (default 2048): the import aborts (nothing
  imported) if the export stream from the Mac grows beyond this.
* `--imessage-only`: leave out SMS / RCS rows relayed by the iPhone.
* `--dry-run`: fetch and convert, print the counts, import nothing (no
  daemon needed).

Steps: the embedded `tools/mac-messages-export.py` runs on the Mac
(read-only) and streams a tar (`export.jsonl` + `attachments/<guid>/<file>`)
into `$XDG_CACHE_HOME/imsg/import/<run>/` (mode 0700; `~/.cache` is
readable by the sandboxed daemon; unpacked with `--no-same-owner
--no-same-permissions`, and an attachment `path` is only used if it is a
relative `attachments/…` path of plain components naming a regular file,
no symlinks on the way — the daemon also refuses non-regular sources); `imsg` converts the rows to the
daemon's model and calls `ImportMessages` with batches of 200 messages
(chats included), then the tapbacks; the staging directory is removed on
success and kept (path printed) if a batch fails.

Mapping (the same ids live messages get): 1:1 chats are keyed by the
other party's handle (SMS, RCS and iMessage with the same number become
one chat; a bare 10-digit number with a North American area code, or 11
digits starting with `1`, as the Mac stores many SMS handles, gets `+1`
so it matches the live `tel:+1…` chat; the tags Messages appends to
filtered senders — `(smsft)`, `(smsfp)`, `(filtered)`, `(smsft_rm)`,
`(smsft_fi)`, `(smsft_or)`, also repeated — are stripped, so an SMS short
code `46339(smsft)` is the chat `tel:46339`; a chat listing nobody is the
chat with its `chat_identifier`), groups by
`group:<chat.group_id>` — the group GUID iMessage itself uses, so later
live messages in the group land in the same chat — or, without one, by
all participants including your own handle; a Mac group with only one
other person left is that person's 1:1 chat (as live iMessage treats a
two-person conversation), and a chat listing only your own handles (also
a Mac "group" row, e.g. tests between your own devices; the Mac reuses its
group id across such chats) is the note-to-self chat with one of them:
the address the Mac sent from in that chat if it is listed, else the
first listed address the daemon knows (registered or `other_handles`),
else the first listed, in the form the daemon knows it (`2055550100`
files under a configured `tel:+12055550100`); a Mac group listing nobody (everyone else
left) stays a group. Text comes from `message.text`
or, when that is NULL (most rows on recent macOS), from the NSString in
`attributedBody`; rows whose text cannot be decoded are imported with
attachments only and counted as "text decoding failures". Attachments
keep their place relative to the text; hidden link-preview payloads are
dropped. Tapbacks (`associated_message_type` 2000-2006 / 3000-3006) are
applied to their target message; a target outside the imported range or
missing from the store counts as orphaned. Group events (renames,
membership changes) and other system rows are skipped; the chats carry
the Mac's current name and members (when several Mac chats map to one id,
e.g. group rows that share a group GUID, the one with the newest message
wins). Your own messages keep the address
they were sent from (`destination_caller_id`). Rows in chats, or from
senders, without a valid iMessage address (contact names, `urn:biz:`
business chats, …) are skipped and counted as "without a valid address"
(the daemon would refuse the batch).

Re-running is safe: the daemon skips every message id it already has
(including live ones) and changes nothing about them. Imported messages
are marked read and announced with one `ChatUpdated` per chat, not as new
messages. Transcript backgrounds are not imported this way; use
`import-mac HOST --backgrounds` (below).

Output: the export counts (chats, messages, reactions, attachments with /
without file, skipped rows, text decoding failures) and, unless
`--dry-run`, what the daemon did (new messages, already present, new
chats, attachment files, reactions, orphaned reactions); `--dry-run` also
lists the chat ids it would write with their message counts. `--json`:
one object `{"host","since","dry_run","export":{…},"imported":ImportResult}`
(`"chats": {"<chat id>": <messages>}` instead of `imported` in a dry run).

### `import-mac HOST --backgrounds [--chat CHAT]... [--create] [--max-total-mb MB] [--dry-run]`

Copies the chats' transcript backgrounds ("Backgrounds" in Messages on
macOS 26) from a Mac into the daemon, **locally only** (nothing is sent;
a later background change from iMessage replaces them). Same ssh and Full
Disk Access requirements as the message import; one ssh connection.

`tools/mac-backgrounds-export.py` (embedded in `imsg`) runs on the Mac
with `python3 -`: for every chat whose `chat.db` `properties` plist has
`backgroundProperties` it reads only the background id (`trabaid`), the
payload version (`trabapv`), the announced size (`trabafs`), `trabav`
and `refreshDate`. The MMCS download reference beside them (the file key
`trabak`, `trabas`, `trabar`, `traboid`) is never read into the output,
printed or stored. It streams back a tar with `backgrounds.jsonl` (one
record per chat: Mac row id, identifier, style, group id, participants,
those fields) and the cached files from
`~/Library/Messages/TranscriptBackgroundCache/`: `<id>` (the PosterKit
payload, Apple Archive `AA01` or stored zip) and, if present,
`<id>-watchBackground` (a pre-rendered binary plist). Files over 32 MB
(the daemon's limit) and anything that is not a regular file are left
out. The tar is unpacked with `--no-same-owner --no-same-permissions`
into `~/.cache/imsg/import/<run>/backgrounds/` (only plain relative paths
under `backgrounds/` that are regular files are used; the whole stream is
capped by `--max-total-mb`, default 2048) and removed afterwards.

Each Mac chat maps to the daemon's chat id exactly as in the message
import: a 1:1 is the other party's `tel:` / `mailto:` (bare national
numbers get `+1`, SMS filter tags are stripped), a group is
`group:<GROUP_ID>` (upper case), a style-43 "group" with one other person
is that person's 1:1 chat, and our own handles only = note to self (our
handles come from the daemon's status). When several Mac chats map to the
same daemon chat (an SMS and an iMessage row for one number), the one
with the newest `refreshDate` wins and the others are reported as
`superseded`. Then, per chat, `ImportChatBackground` gets the poster; if
the daemon finds no picture in it and a `-watchBackground` file exists,
that one is imported instead (also when no poster is cached).

`ImportChatBackground` refuses chats the daemon does not have, so those
are skipped (`not-in-daemon`) unless `--create` is given: then the chat
is first created with an `ImportMessages` batch holding just the chat
(participants and name from the Mac, undated, so live state wins).
`--chat` (repeatable) limits the run to those chats: a chat id, a handle
(`2055550100` = `tel:+12055550100`) or a name the daemon knows.

Re-running is safe: a chat is skipped when its background already has the
Mac's background id (`already-set`, it arrived over iMessage) or is a
local import (`local-…`) whose stored payload (`background.payload`) has
the same SHA-256 as the Mac's poster or watch file (`already-imported`).

`--dry-run` fetches and maps everything and shows what would happen
without importing (it works without the daemon, mapping without our own
handles). Output: a table `MAC CHAT  DAEMON CHAT  BACKGROUND  RESULT`, then
a detail line per chat (which file was used, the new `local-…` id, the
decoded picture). Results: `imported`, `created+imported`,
`would-import`, `would-create+import`, `already-set`, `already-imported`,
`not-in-daemon`, `superseded`, `not-cached` (the Mac has the background
id but not the file), `too-large`, `bad-id`, `no-address` (no valid
iMessage address), `failed` (exit code 1). `--json`: one object
`{"host","dry_run","chats":[{"mac_rowid","mac_chat","chat_id",
"background_id","version","size","result","detail","picture"}]}`.

### `store purge-invalid-chats [--yes]`

Maintenance after a bad import: lists the chats whose id is not a valid
chat id (`mailto:` / `tel:` / `group:` / `group-p:`; e.g. raw Mac SMS
handles like `46339(smsft)` stored by an older `import-mac`) with their
message, reaction and attachment counts. Nothing is deleted unless
`--yes` is given; then the daemon (`PurgeInvalidChats`) deletes those
chats with their messages, reactions, attachment rows and copied files.
Run `import-mac` again afterwards to bring the messages back under the
normalized ids (see docs/DAEMON.md, "Recovering from a bad import").
`--json`: the daemon's [PurgeSummary](DBUS.md#purgesummary).

### `doctor`

Checks, each `ok` / `WARN` / `FAIL`:

| Check | What |
|---|---|
| `hwconfig` | `hwconfig.plist` present, valid, mode 0600 |
| `IMDAppleServices` | the Apple binary for the NAC emulator, sha1 pinned |
| `cert-1.0.plist` | local copy of Apple's validation cert (WARN only) |
| `anisette` | with `anisette_command` (`IMESSAGED_ANISETTE_COMMAND`, else `config.toml`): the command prints a JSON object with `X-Apple-I-MD` and `X-Apple-I-MD-M` (ok) or not (FAIL); WARN when the Mac model in its `X-Mme-Client-Info` differs from `hwconfig.plist`'s `product_name` (the command must reach the Mac the hwconfig came from). Values are never shown and the command's stderr is redacted. Otherwise the remote server (`IMESSAGED_ANISETTE_URL`, else `anisette_url`, else `https://ani.sidestore.io`) accepts a TCP connection, reported as WARN because Apple rejects remote-server anisette since 2026-10-13 |
| `anisette ssh` | only for an ssh `anisette_command`: WARN when it lacks `-F` (the sandboxed unit cannot read the system ssh config; use `ssh -F ~/.ssh/config -o BatchMode=yes -o LogLevel=ERROR ...`), WARN when `systemctl --user show-environment` has no `SSH_AUTH_SOCK` and `~/.ssh/config` names neither an `IdentityAgent` nor an `IdentityFile` |
| `D-Bus activation` | `org.imessage.Daemon1.service` installed in `$XDG_DATA_HOME` / `$XDG_DATA_DIRS` `dbus-1/services` |
| `systemd unit` | `systemctl --user is-active imessaged` (skipped without `systemctl`) |
| `daemon` | `imessaged` answers on the bus (this may D-Bus-activate it) |
| `connection` | connection state (`connected` = ok, `error` = FAIL) |
| `registration` | registered iMessage handles and the next renewal |
| `profiles` | shared names and photos (`GetStatus.profiles`): `ready` = ok, anything else WARN; `needs_login` says to run `imsg login` once |

Exit code 1 if any check FAILs. `--json`: `{"checks":[{"check","level","detail"}…],"ok":bool}`.

### `hwconfig import FILE [--force]`

Validates a `hwconfig.plist` from `tools/mac-hwinfo.sh` and installs it
into the state directory (mode 0600). Works without the daemon. `--force`
replaces a different existing identity (forces re-registration).
It never talks to the daemon, so the global `--json`, `-q`/`--quiet` and
`--bus-address` options are ignored: it always prints a human-readable
inspection report.

### `version`

`imsg` and `imessaged` versions (`--json`: `{"imsg":"0.1.0","imessaged":"0.1.0"}`,
`null` if the daemon is not reachable; exit code 0 either way).

## JSON mode

With `--json`, commands that return data print the daemon's JSON
**unchanged**, one document on one line, so it can be piped into `jq`:

| Command | Output |
|---|---|
| `status`, `login` | `Status` |
| `chats` | `[Chat]` |
| `messages` | `MessagePage` |
| `search` | `[Message]` |
| `send`, `send-file` | `SentMessage` |
| `lookup` | `[{"handle","reachable"}]` |
| `profile` | `HandleProfile` or `null` |
| `download` | path as a JSON string |
| `watch` | one event object per line (above) |
| `doctor`, `version`, `import-mac` | `imsg`'s own objects (above) |
| `store purge-invalid-chats` | `PurgeSummary` |
| `logout`, `react`, `edit`, `unsend`, `typing`, `read` | nothing (exit code only) |

Shapes are documented in [docs/DBUS.md](DBUS.md#json-payloads). Errors
always go to stderr as `imsg: error: …`.

```sh
imsg chats --json | jq -r '.[] | select(.unread_count > 0) | .id'
imsg watch --json | jq -c 'select(.signal == "MessageReceived") | .message.text'
```

## Exit codes

| Code | Meaning | Daemon errors |
|---|---|---|
| 0 | success | |
| 1 | other error; `doctor` found a FAIL | `Login`, `Protocol`, `Store`, `State`, `StateKeyMismatch`, `Unsupported`, unknown |
| 2 | usage: bad command line or argument | `InvalidInput`, `InvalidHandle` |
| 3 | not logged in / no hardware config | `NotLoggedIn`, `NoHardwareConfig` |
| 4 | two-factor code required or wrong | `BadTwoFactorCode` (after 3 tries), `NoPendingTwoFactor` |
| 5 | daemon not running / bus unreachable / no reply | `org.freedesktop.DBus.Error.ServiceUnknown`, `NameHasNoOwner`, `NoReply`, activation failures |
| 6 | no such chat / message / attachment | `NotFound`, `ChatNotFound` |
| 7 | temporary failure, retry later | `Transient` |

A closed stdout pipe (`imsg watch | head -1`) ends `imsg` silently.
