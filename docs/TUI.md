# `imsg-tui` terminal client

`imsg-tui` shows your conversations as iMessage-style bubbles in a terminal
and lets you read, send, react and reply. It is the terminal client of
[imessage-cli](https://github.com/thescruggs/imessage-cli) (which runs on a
Mac and reads Messages' database) ported to Linux: the UI, keys and bubble
rendering are the same, but every piece of data comes from and goes to the
`imessaged` daemon over D-Bus ([docs/DBUS.md](DBUS.md)), through the same
`imsg-client` library the `imsg` CLI uses. It does not link the protocol
engine, so `imessaged` must be running (or D-Bus-activatable).

```sh
cargo install --path crates/imsg-tui      # -> ~/.cargo/bin/imsg-tui
imsg-tui
imsg-tui --contacts-dir ~/contacts        # vCards elsewhere (also IMSG_CONTACTS_DIR, `:`-separated)
imsg-tui --bus-address unix:path=/tmp/x   # a private bus (tests, containers; also IMSG_BUS_ADDRESS)
```

Exit codes on start-up failure are the CLI's (5 = no session bus). A
missing daemon is not fatal: the status bar says `○ no daemon` and the
app keeps retrying (D-Bus activation starts it when the unit is enabled).

## Screen

```
╭ Messages ─────────────╮ Vicki Brenner
│● Vicki Brenner  10:34 │ iMessage · +12053128755
│  You: Before dawn, …  │ ─────────────────────────────────────────────
│  Gabe Denner    10:22 │   Beans 🤣  10:07 AM                 (gray: them)
│  Haha, nice work      │                     Before dawn …  (blue: you; green: SMS)
│  …                    │                                            Delivered
╰───────────────────────╯ ╭────────────────────────────────────────────╮
                          │ iMessage  (Enter to send, …)                │
                          ╰────────────────────────────────────────────╯
● connected ✉ 1 sent   j/k move · Enter compose · / filter · Ctrl+N new …
```

* **Chat list** (left): pinned chats first (📌, in the daemon's pin order),
  then by latest activity; 👥 marks groups, `●` and bold mark unread, the
  second line is the newest message (`You:` for yours; "… is typing" while
  someone types).
* **Transcript**: one bubble per message, blue for yours, green for a text
  message (SMS / MMS / RCS through your iPhone), gray for theirs; sender
  names in groups; reply quotes (`↩ Name: …`); attachments as
  `🖼 IMG_1.jpg (183 KB)` lines; link previews as a card (title, site,
  URL; a message that is only a link shows just the card); `Edited`,
  `(message unsent)`, `✨ sent with confetti`, `⚠ unverified sender`;
  reactions under the bubble (`❤️ You  🔥 Bob`); under your latest message
  its state (`Sending…`, `Sent`, `Delivered`, `Read`; `Not delivered` in
  red under any failed one).
* **Status bar**: the daemon's connection state (`● connected`, `○ needs
  login`, `○ no daemon` while the bus is unreachable), the unread total,
  the last status message, and the keys for the focused pane.

## Keys

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Move between the chat list, the messages, and the compose box |
| `j` / `k`, `↑` / `↓` | Move through chats, or select a bubble |
| `g` / `G`, `PgUp` / `PgDn` | Top / bottom, scroll (scrolling up past the top loads older messages) |
| `Enter` | Open the compose box (from the chat list, or the messages pane with no bubble selected), or send |
| `Alt+Enter`, `Shift+Enter`, `Ctrl+J` | New line while composing |
| `:smile:` | Emoji shortcode, expanded as you type; `Ctrl+E` opens the emoji picker |
| `Ctrl+F` | Attach a file by path (Tab completes); Backspace on an empty composer removes the last one |
| `Ctrl+N` | New message: pick a contact (name, phone or email) or type an address |
| `Ctrl+S` | Search all messages; Enter on a result opens that conversation |
| `/` | Filter the chat list by name, address or preview; Esc clears |
| `o` / `Enter` | Open the selected message's attachments (or its link) with `xdg-open` |
| `s` | Save the selected message's attachments to `~/Downloads` (never overwrites) |
| `y` | Copy the selected message's text (`wl-copy`, `xclip`, `xsel`, else the terminal's OSC 52) |
| `r` | React to the selected message: `1`-`6` for ❤️ 👍 👎 😂 ‼️ ❓, or type an emoji / `:shortcode:`; the same one again removes it |
| `R` | Reply to the selected message (a quote shows above the composer; Esc cancels) |
| `Ctrl+R` | Reload the chat list, the open chat and the contacts |
| `Ctrl+L` | Redraw |
| `?` | Help; `q` (from the lists) or `Ctrl+Q` / `Ctrl+C` quits |

Opening a chat (or a new message arriving in the open chat) marks it read
in the daemon, which sends a read receipt as Messages.app does. Typing in
the composer sends a typing indicator once per draft.

## How it talks to the daemon

| Action | D-Bus |
| --- | --- |
| Start-up | `GetStatus` (version, connection, our handles), `ListChats` in pages of 500, then `ListMessages(chat, "", 1)` per chat for the previews |
| Open a chat | `ListMessages(chat, "", 60)`, older pages with the returned cursor; `MarkRead(chat, "")` |
| Send | `SendText(chat or participants, text, reply_to)`, then `SendFile` per attached file; a new conversation is selected from the returned `chat_id` |
| Live updates | signals `MessageReceived`, `MessageUpdated`, `ChatUpdated`, `Typing`, `ConnectionStateChanged`, `BackgroundError`; if the bus connection drops the app reconnects with backoff and reloads everything |
| Others | `Search`, `React`, `SetTyping`, `DownloadAttachment` (the daemon's copy is opened / copied) |

Contact names: the vCards in `~/.local/share/kpeoplevcard` (subfolders
included; `imsg sync-mac` writes your Mac's there, KDE Connect your
phone's), matched by email (case-insensitive) or phone number (all its
digits, or the last ten for numbers without a country code or with `+1`,
so `(205) 555-0100` matches `tel:+12055550100` but `+44 7911 123456` never
matches `+1 791-112-3456`; the same rule as kmessages); then the name a
person shared with you (the chat's `profile`); then the bare address. The contact picker lists every phone
and email of the matching people, and accepts a raw address.

Not carried over from the Mac version: the `--list` / `--to` / `--file`
one-shot flags (use `imsg chats`, `imsg send`, `imsg send-file`), the
server/token configuration (there is no server), and `Attachment not on
this Mac` (the daemon downloads attachments on demand). Not done yet:
editing / unsending your own messages, pinning from the TUI (`imsg chat
CHAT --pin`), and showing images inline.
