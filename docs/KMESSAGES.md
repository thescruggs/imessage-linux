# kmessages — the KDE app (Phase 4)

A Kirigami 6 / kirigami-addons client for `imessaged`, styled after macOS
Messages. Its **only** interface to the backend is the daemon's D-Bus API
([DBUS.md](DBUS.md)); it never touches the store or rustpush.

## Build, install, run

Needs Qt ≥ 6.10 (for `QSortFilterProxyModel::beginFilterChange()`; Core,
Gui, DBus, Qml, Quick, QuickControls2, Widgets, Test), KF6 ≥ 6.8 (Kirigami,
I18n, CoreAddons, Config, Notifications, StatusNotifierItem, IconThemes,
DBusAddons), kirigami-addons ≥ 1.0 and
extra-cmake-modules; qqc2-desktop-style for the Breeze look at runtime.
Tests also need `dbus-run-session` and a `python3` with dbus-python and
PyGObject (picked automatically; override with `-DKMESSAGES_TEST_PYTHON=`).

```console
$ cmake -B kmessages/build -S kmessages -G Ninja -DCMAKE_BUILD_TYPE=Debug
$ cmake --build kmessages/build
$ ctest --test-dir kmessages/build --output-on-failure
$ cmake --install kmessages/build --prefix ~/.local     # optional
$ kmessages                       # or kmessages/build/bin/kmessages
```

Options: `--minimized` (start in the tray), `--no-tray`, `--chat <chat id>`.
A second launch raises the running instance (`KDBusService::Unique`).
Developer options `--screenshot <file>` / `--screenshot-delay <ms>` grab the
window and exit; such a run skips the unique service (so it works next to
the desktop instance instead of being forwarded to it) and sends no
notifications.
Installed files: `bin/kmessages`, `org.imessage.kmessages.desktop`,
`org.imessage.kmessages.metainfo.xml`, `knotifications6/kmessages.notifyrc`
(events `newMessage`, `connectionProblem`, `sendFailed`),
`dbus-1/interfaces/org.imessage.Daemon1.xml`.
Icon: Breeze `dialog-messages`. Settings live in `kmessagesrc`.
`appstreamcli validate --pedantic --no-net` passes on the metainfo (with
network access the placeholder homepage URL is reported unreachable until
the repository is public).

On start the app checks for `org.imessage.Daemon1` and, if absent, asks the
bus to activate it (`StartServiceByName`, i.e. the systemd user unit). If
that fails, the sidebar says "imessaged is not running" with a retry button;
the app reconnects by itself whenever the daemon (re)appears.

## Architecture

```
QML (src/qml) ──▶ App singleton = AppController ──▶ models ──▶ DaemonClient ──▶ D-Bus (org.imessage.Daemon1)
```

`kmessages_core` (static library, shared by the app and the tests):

| File | Role |
|---|---|
| `types.{h,cpp}` | `Handle`, `Chat`, `Message`, `MessagePart`, `Attachment`, `Reaction`, `Status` (incl. `registration.sender`), `SentMessage`, `HandleReachability`; JSON parsers exactly per DBUS.md; port of `Handle::try_normalize`; time/initials/colour helpers |
| `daemonclient.{h,cpp}` | Hand-written QtDBus client (introspection in `src/dbus/org.imessage.Daemon1.xml`). Every call async (`QDBusPendingCallWatcher`); typed callbacks for C++, result signals for QML. `QDBusServiceWatcher` on the name → `available`, re-reads `GetStatus` and emits `resyncRequested` on (re)appearance and on "resync" background errors. All seven signals + `PropertiesChanged` (`ConnectionState` / `Registered` values and invalidations). `statusKnown` until the first `GetStatus` reply: tapback toggles wait for it (our handles decide "already mine"). `ownSender` = `registration.sender`, else the first handle. `otherOwnHandles` (from the "My other addresses" setting) and `daemonOtherHandles` (`registration.other_handles`, read-only; messages from those already arrive with `is_from_me`) both count for `isOtherOwnHandle()` / `isFromMyself()`; phone numbers match on all digits when both carry a country code (`tel:+…`), on their last ten only against a bare national number. Long timeouts (120 s) for Login / 2FA / SendFile / DownloadAttachment / LookupHandles, 60 s for React / Edit / Unsend / MarkRead. `sendFile` sends files the sandboxed daemon cannot read from a staged copy (below) and maps `InvalidInput` "os error 2" to "The messaging service cannot read that folder (…)" |
| `outgoingfiles.{h,cpp}` | The daemon's unit has `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome=read-only`: files outside `$HOME` or under `/tmp`, `/var/tmp`, `/run` (portal documents in `/run/user/…/doc`), `/dev/shm` are copied to `~/.cache/kmessages/outgoing/<uuid>/<name>` (dirs 0700) and the copy is deleted when `SendFile` answers (the daemon keeps its own copy of what it sent); after a timeout it is left for the daemon and purged at the next start (copies older than a day; only real subdirectories, a symlink planted there is skipped). Clipboard images are saved there as PNG |
| `chatlistmodel.{h,cpp}` | `ChatListModel` (name, snippet, time, unread, initials, avatar colour, isGroup, typing, participants, `pinned`, `pinOrder`, `section`), pinned chats first in `pin_order` (a pin change re-sorts the whole list, since the daemon renumbers several pins with one `ChatUpdated` each), then the rest newest first (new activity never moves a chat into or out of the pinned block). Every reorder is one `layoutChanged` with `VerticalSortHint` (persistent indexes follow their chats), never `rowsMoved`: QML's delegate model rebinds its items in place for that hint, while a reset, or the unhinted layout change a `QSortFilterProxyModel` makes of any reorder, recreates every delegate and scrolls the list to the top. So `ChatListPage` binds this model directly and the filter proxy only while searching. `section` is "Pinned" / "Conversations" while any chat is pinned (`hasPinned`), else empty so the plain list has no header (empty in `ChatFilterModel` while a search text is set); `isPinned(chatId)`; live `ChatUpdated` / `MessageReceived` / `MessageUpdated` / `Typing`. Snippet = newest message via `ListMessages(limit 1)`, re-read only for chats whose `last_message_at` changed (or that have none yet); refreshes triggered by the daemon (re)appearing (`resyncRequested` + the state turning `connected`) are coalesced into one `ListChats`. `revision` bumps on any row change for QML bindings over `get()`. `ChatFilterModel` for the search field |
| `messagelistmodel.{h,cpp}` | One chat's transcript, **row 0 = newest** (bottom-to-top ListView). Paging with `next_cursor` via `loadOlder()`, which `ConversationPage` calls whenever the oldest end of the transcript is in view — scrolled to the top, or the loaded pages do not fill the viewport yet (Qt Quick's `ListView` never calls `canFetchMore`/`fetchMore`); one page at a time until the view overflows or the cursor runs out; grouping by sender within 5 min, time separators (new day or > 1 h gap), tail on the last bubble of a run, sender names/avatars in groups, aggregated tapbacks + "my" tapback, "Delivered/Read/Sent/Sending…" under the newest own message and "Not Delivered" on failures, edited/unsent/unverified, attachments with on-demand download, reply quotes, linkified text with mentions (trailing `.`, `,`, `;`, `:`, `!`, `?` and an unbalanced `)` stay outside the link). Live inserts/updates; typing clears on a message from the typist. A failed `SendText` / `Edit` is reported as `sendTextFailed` / `editFailed` so the composer gets its text back. A change of `verified` starts a new group, so every unverified run shows its badge. Own sent files show at once from the picked file: the attachment of our own `MessageReceived` (matched by chat + file name + size while `SendFile` is pending, each pending file used once, so two same-name files keep their own paths) and `<SentMessage.id>:0` map to it, no `DownloadAttachment`; if that file is gone the bubble falls back to the daemon's copy (`reloadAttachment`). A `DownloadAttachment` that times out (`NoReply`) stays "downloading" and is retried once after 30 s before it counts as failed |
| `handlelookupmodel.{h,cpp}` | Recipients for a new message: local normalization, then `LookupHandles` per entry (reachable / unreachable / invalid / error) |
| `messagesearchmodel.{h,cpp}` | Debounced daemon `Search`, shown under the chat list while searching |
| `contactresolver.{h,cpp}` | `ContactResolver`: second source, for handles the address book does not know: the name and photo the person shared ("Share Name and Photo"), fed by `DaemonClient` from every Chat payload's participants (`profile`; a participant without one shares nothing) and `HandleProfileUpdated`. The address book wins for the name, its photo for the avatar; the shared photo is used where it has none (`image://attachment/<path>?u=<updated_at>`). `sharedNameFor()` / `sharedPhotoSource()` for the details page; `nameFor()` stays address-book only. Indexes `KPeople::PersonsModel` (emails lowercased; phones by all digits, and by the last ten only for `+1` or numbers without a country code, so +44 7911 123456 is not +1 791-112-3456) → name, first name, photo (≤ 256 px). Re-indexes 500 ms after any model change, in chunks of 40 people per event-loop turn, and swaps the index atomically; until KPeople has loaded, lookups miss and handles show. `Handle::displayName()` / `shortName()` consult `ContactResolver::instance()`, so every name in the app (chat titles, "Alice, Bob & Carol" groups, senders, tapback tooltips, typing, search, notifications) follows the contacts; models re-emit `dataChanged` on `contactsChanged`. `ContactImageProvider` serves `image://contact/<uri>?<revision>` (thread-safe; Avatars load asynchronously) and the `avatarSource` roles hold those URLs ("" without a photo → initials/colour) |
| `contactsuggestionmodel.{h,cpp}` | New-message "To:" suggestions from the address book (name / address search, one row per email and phone number as IDS URIs), the shown rows checked with debounced `LookupHandles` to mark iMessage addresses: results cached per handle for the app session and re-asked after an hour at the earliest, at most 5 handles per call, one call at a time, the rest queued |
| `recipientpickermodel.{h,cpp}` | `RecipientPickerModel` (`App.recipientPicker`): the new-message suggestion list as one model, `ContactSuggestionModel` rows first, then `recipientSuggestions` (a `ChatFilterModel`), with `kind` ("contact" / "conversation"), the shared roles of both and `section` ("Contacts" / "Conversations", "" without contact rows); inserts / removals / moves are forwarded with the offset, resets and layout changes as resets; `rowData(row)` says what activating a row does. One model gives the page one `currentIndex` for the keyboard highlight |
| `appsettings.{h,cpp}` | notifications on/off, start minimised, close to tray, "My other addresses" (`OtherHandles`, comma-separated), the Mac to sync from (`MacHost`, "" = the daemon's `mac_host`) and the `imsg` program (`ImsgPath`, "" = PATH); KConfig `[General]` |
| `macsync.{h,cpp}` | `MacSync` ("Sync from Mac", `App.macSync`): runs `imsg sync-mac --json [-- HOST]` with `QProcess` (program: `$KMESSAGES_IMSG`, else the `ImsgPath` setting, else `imsg` on PATH; HOST only when `MacHost` is set, otherwise imsg uses the daemon's `mac_host`); `running`, `host` (setting, else `GetStatus.mac_host`), `lastSync` / `lastSyncText` (`[MacSync] LastSync`, after a fully successful run); `summarize()` turns the JSON summary into one line ("Synced from mac.local: 312 contacts (4 updated), 3 pins (1 not found)"; a failed part → "Sync from Mac incomplete: …"; `{"error"}` → "Sync from Mac failed: …"; exit code 5 → "imessaged is not running"; not JSON → imsg's last stderr line); no host known while the daemon is up → a hint to set one, without running imsg; `stop()` (clicking the busy toolbar button) and the 10-minute timeout send SIGTERM, then SIGKILL after 3 s ("Sync from Mac stopped." / "no answer from the Mac in time"); a program that cannot start → "cannot run imsg (…)". Pin details in the summary: not found, duplicates, unpinned (the Mac no longer pins them), local pins kept, kept (unmapped), a `lookup_error`; contacts "(3 updated, 1 removed)". `finished(ok, message)` → passive notification (long for failures) |
| `appcontroller.{h,cpp}` | Owns all of the above; `KNotification` `newMessage` with default action (open chat) and inline reply (`KNotificationReplyAction` → `SendText`, then `MarkRead` like iOS; a failure posts `sendFailed` "Reply not sent: …" with Open), one notification per chat; messages from "My other addresses" (the user's iPhone) raise none and mark their chat read without a read receipt for the user's own message: `MarkRead` up to the newest unread message from someone else (none: no `MarkRead`), and the own messages the daemon still counts as unread are subtracted in `ChatListModel` (`setOwnUnread`; also when the open chat is marked read). `connectionProblem` notifications when the daemon needs the user and the window is not in front: `needs_login` / `needs_two_factor` (Open shows the window and the login dialog at the right step), `error` (with its detail) and `BackgroundError`s (not the "resync" ones); once per problem until the state is `connected` again (a retrying error is not repeated), states settle 1.5 s first. `KStatusNotifierItem` with tooltip (unread or the problem) / NeedsAttention while there is unread mail or a problem, and "New Message"; `MarkRead` when the current chat is on screen and the window active; `clipboardFiles()` for pasting (copied files, or an image without text saved as PNG) |
| `qmlsetup.{h,cpp}` | registers `import org.imessage.kmessages` (singleton `App`) and KI18n for QML |

QML (`src/qml`, compiled into `qrc:/kmessages/qml`): `Main.qml` (PageRow:
sidebar | transcript | details on wide windows, a page stack on narrow ones;
`openChat` of the chat already open only brings its page to the front, and
the chat is closed when exactly that page leaves the row),
`ChatListPage`/`ChatListDelegate`/`StatusFooter` (search, unread dot,
avatar, typing dots, connection state; "Pinned" / "Conversations" section
headers while any chat is pinned and a small pin icon on pinned rows; a
context menu on right-click or press and hold with Pin / Unpin →
`DaemonClient::setChatPinned` (`SetChatPinned`, order -1); toolbar: New
Message, **Sync from Mac** (`view-refresh`, `displayHint: KeepVisible`;
while `imsg sync-mac` runs a busy indicator replaces the icon, the action
is disabled and a click on the button stops the sync; the tooltip names the
Mac and the last sync), Settings; the context menu opens at the click or
touch point and its Pin / Unpin follows the chat's live pin state; "Sync from Mac" is also in the
tray menu), `ConversationPage`/`MessageDelegate`/
`AttachmentView`/`ReactionChips`/`TypingBubble` (bubbles: `#1982fc` sent,
theme-tinted received so dark mode works; images inline with placeholder
while downloading, other files as chips; a failed download shows its error
as a tooltip and retries on click; files dropped on the conversation go to
the composer; the chat's transcript background, `App.currentChatBackground`
from Chat JSON `background.path`, is painted behind the transcript with
`PreserveAspectCrop`, dimmed in dark mode, with stronger received-bubble
tint, no carved tails and pill-backed date/receipt labels over it;
receive-only, there is no UI to change it yet), `Composer` (attachments wait as removable chips with
thumbnails and go out with the next Send — from the file dialog, a drop, or
Ctrl+V of an image or copied files; emoji grid, Enter sends / Shift+Enter
newline, typing on (repeated every 30 s while typing goes on, the
recipient's indicator expires after ~60 s) with a 5 s debounce off,
reply/edit modes), `MessageMenu` + `TapbackPicker` (tapback,
reply, copy, edit own, undo send own, open attachment; double-click a bubble
for tapbacks), `NewMessagePage` (recipient chips + lookup + suggestions
from `App.recipientPicker` in one `ListView` with section headers; the
page's `highlightedIndex` is the list's `currentIndex`: the first row
whenever the text changes, -1 without rows; in the To: field Down / Up
move it (wrapping, the row kept in view), Enter adds the highlighted
contact's address, opens a highlighted group or adds the other person of
a 1:1 conversation, and adds the typed text when nothing is highlighted;
Escape clears the text, and on an empty field goes back to the list; Tab
keeps its focus behaviour; hovering a row highlights it; the highlighted
row uses the Kirigami highlight colour and is `Accessible.selected`),
`LoginDialog` (Apple ID/password, then 2FA code; error mapping for
`NoHardwareConfig`, `BadTwoFactorCode`, `Transient`, timeouts),
`SettingsPage` (FormCard: status, a "Shared names and photos" line when the daemon's `profiles` state needs `imsg login` or is unavailable, "Sending from", addresses, "My other
addresses" + the daemon's read-only "Also yours (from the daemon)",
notifications, tray; **Sync from Mac**: Mac host with the daemon's
`mac_host` as placeholder, Sync Now with the last sync time, and the imsg
program path for development builds), `ChatDetailsPage` (a Pin / Unpin
button under the heading; participants; "Shared name: …"
under a participant who shared one, and their shared photo next to the
address-book photo when that is shown instead). Groups with a photo
(Chat JSON `icon_path`) show it as their avatar in the chat list and the
details page (`ChatListModel` `avatarSource` / `iconSource`, through
`image://attachment/`).
`StatusFooter` says "Connected as <sending handle>".

**Link previews.** A message with a `link` part (docs/DBUS.md) shows a
`LinkPreview` card under its text bubble, as Messages does: the page's
image on top when there is one (downloaded on demand through
`DownloadAttachment` of `image_attachment_id`, which the daemon has inline),
otherwise the site icon (`icon_attachment_id`, else a globe) beside the
title; the title in bold, the site name (`site_name`, else the URL's host
without `www.`) under it. A click opens the URL (http / https only), hover
shows the full URL, right-click or press and hold opens the message menu,
which has **Copy Link** for such messages. When the text is nothing but a
link (`linkOnly`: one `http(s)` token, or the part's `url` /
`original_url`; the card's URL may be the page after redirects), the card
replaces the text bubble; other text keeps its
clickable links. The chat list and notifications show the title for a
URL-only message. `MessageListModel` exposes `link` (url, title, summary,
siteName, host, openable, image / icon download state) and `linkOnly`;
the preview's image and icon are never listed as attachments. The previews
themselves are made by the sender's device; for messages sent from here the
daemon fetches the page (docs/DAEMON.md, "Link previews"). The composer
shows no preview before sending.

Only Kirigami 6 / addons 1.15 types are used.

## Tests and the fake daemon

`kmessages/tests/fake_daemon.py` (dbus-python + GLib) serves
`org.imessage.Daemon1` per DBUS.md with canned data: Bob (31 messages over
three days, an image with a tapback), a group "Weekend Trip" (mentions,
tapbacks, a PDF, an edited message, unread), a phone-number chat with an
unsent and unverified messages, Eve (two link previews: a message that is
only a URL with image + icon, one with text and an icon only; then a
failed, unsupported message). It
implements every method, both properties and all signals, plus a test-only
`org.imessage.FakeControl` interface (`InjectMessage`, `InjectLink(chat,
sender, text, url, title, site_name, image, icon)` (an incoming message
with a `link` part; empty text = just the URL; `DownloadAttachment` of its
`:link-image` / `:link-icon` returns generated PNGs), `InjectTyping`,
`InjectReaction`, `SetDelivery`, `SetConnectionState`,
`SetStateViaProperties`, `SetStatusDelay`, `EmitBackgroundError`,
`InjectMessageVerified`, `SetProfile` (a handle shares a name and a
generated photo, or stops: `HandleProfileUpdated` + `ChatUpdated`, and
`GetHandleProfile` answers it), `SetGroupIcon`, `SetUnreadablePrefixes` (SendFile fails like the
sandboxed daemon, "os error 2"), `SetSendFileDelay` (MessageReceived now,
the reply later), `SetOtherHandles` (`registration.other_handles`),
`FailDownloadsWithNoReply`, `SetMacHost` (`GetStatus.mac_host`; clients re-read the status), `PutChat` (merge fields into a chat, or create it, with one `ChatUpdated`: unread + activity, pin + activity, a new pinned chat), `SetSms` (Text Message Forwarding state,
`SmsForwardingChanged`), `AddSmsChat` (a 1:1 text-message chat with a
number that is not on iMessage), `Calls`,
`ClearCalls`). Login: password `wrong`
fails, an Apple ID containing `2fa` asks for a code, code `000000` is wrong.
Addresses containing `android` (and `AddSmsChat` numbers) are unreachable.
Text messages: `EnableSmsForwarding` → `requested`; with `active`,
`SendText` to an unreachable `tel:` chat stores `service: "sms"`;
`SendTextAs` takes the service explicitly.

CTest runs every test under `dbus-run-session --config-file=tests/session.conf`
(private bus, no activation directories) with `QT_QPA_PLATFORM=offscreen`,
`QT_QUICK_BACKEND=software`, `TZ=UTC`, `LC_ALL=C.UTF-8`. Each test points
`XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` / `XDG_STATE_HOME` at a
`QTemporaryDir` before its application object exists, so nothing is written
to the home directory. Test-only fake behaviour: `SendText` of `__fail__`
fails; `FakeControl.SetStatusDelay` delays `GetStatus` replies;
`FakeControl.SetStateViaProperties` announces a state only via
`PropertiesChanged` (value or invalidation).

| Test | Covers |
|---|---|
| `tst_json` (40 cases incl. data rows) | every payload in DBUS.md (fixtures in `tests/data/`), participants' `profile`, `icon_path`, `pinned` / `pin_order`, `mac_host`, `profiles` status, `sms` status / `SmsForwarding` / message and sent `service`, `link` parts (full, null / missing fields, no URL → unsupported; `isOnlyLink`, the title as snippet, link images left out of `attachments()`), `HandleProfile` / `null`, malformed input, handle normalization (mirrors the Rust tests), helpers |
| `tst_models` (39 incl. init) | `RecipientPickerModel` over two stand-in sources (contacts first, sections only with contact rows, `rowData`, shifted `dataChanged`, removals and resets, `QAbstractItemModelTester`); pinned chats (mixed pins, pinning / moving / unpinning moves the row, new messages keep the pinned block, `ReorderPins`, a fresh `ListChats`, `section` / `hasPinned`, a failed `SetChatPinned` reported); reorders (a pin change, a new message in a lower chat) arrive as `VerticalSortHint` layout changes with no reset or `rowsMoved`, persistent indexes follow, the filter proxy reorders while searching and has no sections then; one `ChatUpdated` changing unread + activity (the unread signal carries the moved chat's count), pin + activity, a new chat arriving pinned; `MacSync::summarize` (success, every pin outcome, part failure, early error, exit code 5, non-JSON output); `MacSync` with a fake imsg (`KMESSAGES_IMSG` override, not found, no host, cannot start, success, timeout, stop); Text Message Forwarding (EnableSmsForwarding, `SmsForwardingChanged`, `isSms` rows, `smsMode` only when active and the number is not on iMessage, no tapback / typing calls in a text chat, unreachable numbers sendable on the new-message page, a send stored as SMS); shared names and photos (`HandleProfileUpdated` → chat list, group participants, transcript senders and avatars; stop sharing) and group photos as avatars; status/state changes (also via `PropertiesChanged` only), chat list order/names/unread/snippets/filter, paging with cursors, grouping/separators/delivery labels, live `MessageReceived`, reactions (injected + own toggle; toggle waits for the first `GetStatus`), typing set/clear, send/reply/edit/unsend + delivery updates, failed send/edit reported, sending to new recipients and groups, attachment download, handle lookup, MarkRead, search, daemon restart → resync, resync without snippet refetch + coalesced refresh, `revision`, no row refresh when own handles are unchanged, link punctuation, login + 2FA flow, `registration.sender`, "My other addresses" (normalizing, phone matching with/without country codes, "from me" in the chat list, the daemon's `other_handles` merged in), unverified runs as groups, link previews (`link` / `linkOnly` roles, the card's image download updating `link`, link images not in `attachments`, the chat list snippet = the title for a URL-only message), staging of a `/tmp` file (copy sent and removed, original shown without a download, "cannot read that folder", purge, a planted symlink left alone), two same-name files in flight, download timeout → one retry / error |
| `tst_contacts` (9 incl. init) | shared profiles vs. the address book (name and photo precedence, no signal without a change, a participant without `profile` clears it, photo-only profiles); handle keys / IDS URIs for address-book numbers; lookups by email, `+1` and ten-digit phone, no last-ten match across country codes; contact names beat the daemon's `display`; group titles "Alice, Bob & Carol" / "… & 2 more"; avatar URLs and `ContactImageProvider`; KPeople against fixture vCards (`tests/data/contacts`, fake people) in a temporary `XDG_DATA_HOME`, incl. an edit on disk being picked up; chat list / transcript / tapback tooltips / contact suggestions (+ `LookupHandles` in bursts of five, cached, re-asked when stale) against the fake daemon |
| `tst_screens` (28) | the real QML window (incl. `new-message-keyboard`: keys typed into the To: field, Up wraps to the last row, Down / Down / Down / Enter on the 1:1 conversation adds the number, typing again highlights the first row and Down / Enter adds the second address, Enter with no match adds the typed address, Escape clears and then goes back; `pinned-chats`: right-click → Pin calls `SetChatPinned` and moves the row under a "Pinned" header, the details page's Pin button, a new message keeps the pins on top, Unpin restores the list, a left click on a row still opens the chat; the scrolled chat list keeps its position when a message or a pin reorders it; `sync-from-mac`: the toolbar button with a fake `imsg` on PATH, busy indicator and disabled action while it runs, the summary and the error as passive notifications, `-- HOST` from Settings, last sync time in the tooltip, a click on the busy button stops it; `shared-profiles`: shared names and photos in the list, transcript and details, a group photo; `sms-chat`: green bubbles and the "Text Message" composer, back to "iMessage" when forwarding goes off; `link-preview`: Eve's two cards, the bare URL hidden behind its card): grabs each screen to `kmessages/build/screenshots/*.png` and checks it is not blank; re-opening the open chat (twice, via `AppController::openChat`, with details open) keeps the transcript; older pages load when the first page fits (page size 5); scrolled up (position set directly, as the wheel handler does), the view stays on the oldest row of the first page when the older page lands and when the typing bubble appears, and sticks to the newest message again once scrolled back; the composer gets a failed send/edit back; Back closes the chat; notification decision (sender's contact name) + mark-read while active; contact photos (stand-in images) and the "To:" contact suggestions; composer attachments (a `/tmp` file + Ctrl+V of a clipboard image → two `SendFile`s, own images without `DownloadAttachment`); typing refresh; "My other addresses" suppress the notification and mark read only up to someone else's message (none: no MarkRead; unread hidden); inline reply (MarkRead on success, `replyFailed`); connection-problem notifications once per problem, NeedsAttention until connected; fails on any QML warning from our files |

Screenshots: `chat-list`, `transcript` (image, tapback, typing bubble, link,
"Read"), `transcript-group`, `transcript-group-details` (three panes),
`transcript-unverified` (unverified badge, unsent), `new-message`
(reachable + unreachable recipient), `new-message-contacts`,
`new-message-keyboard` (third row highlighted by the keyboard), `login`, `login-2fa`, `settings`,
`narrow-transcript`, `narrow-list`, `daemon-unavailable`, `needs-login`,
`composer-attachments`, `transcript-sent-images`, `shared-profiles`,
`sms-chat` (green text-message bubbles, "Text Message" composer),
`settings-sms` (Text Message Forwarding section, active), `pinned-chats`
(two pinned chats under "Pinned", details page with Unpin),
`sync-from-mac` (busy indicator on the toolbar button).

**Text messages.** Rows with `service: "sms"` are green (`#34c759`)
instead of blue and offer no tapback, edit or undo send. A conversation is
in "text message" mode (`MessageListModel.smsMode`: placeholder "Text
Message", green send button, no typing indicator) when the daemon reports
Text Message Forwarding `active`, the chat is with phone numbers (1:1
`tel:` or `group-p:`) and one of them is not on iMessage (`LookupHandles`;
until that answer arrives: the newest message is a text). The send itself
is a plain `SendText`: the daemon makes the same choice. On the
new-message page an unreachable phone number is accepted while forwarding
is active ("Text Message" in green). Settings has a **Text Message
Forwarding** section: state, Enable / Ask Again / Disable
(`EnableSmsForwarding`) and the iPhone step.

To try the UI by hand against canned data:

```console
$ dbus-run-session --config-file=kmessages/tests/session.conf -- sh -c \
    'python3 kmessages/tests/fake_daemon.py & sleep 1; kmessages/build/bin/kmessages --no-tray'
```

## Known gaps

* **Link previews** (PLAN.md) are not implemented: URLs are linkified only.
* **Pins** cannot be reordered by dragging yet (`DaemonClient::reorderPins`
  exists for it); new pins go last, the Mac's order comes with Sync from
  Mac, and `imsg chat CHAT --pin --position N` moves one by hand. Sync from
  Mac is `KeepVisible` so it stays out of the overflow menu (Kirigami 6.30
  loads `displayComponent` there as is, which would leave a bare spinner);
  in a toolbar too narrow even for that, Kirigami shows its own icon button
  for it (disabled while running, no busy indicator, no stop).
* While searching, the list shows `ChatFilterModel`, and a reorder there
  (a pin change, a new message) resets the filtered list.
* **Contacts** come from KPeople (README "Contacts"); a contact's first name
  for group titles is the first word of its formatted name (KPeople has no
  given-name property), so organizations show their first word there.
  Avatars are round only with a GPU scene graph (Kirigami clips with a
  shader; the software backend used by tests shows square photos).
* **Search** filters the chat list locally and lists daemon `Search` hits;
  clicking a hit opens the chat but does not scroll to the message.
* Bubble effects (`effect`), subjects and per-message read times are not
  rendered; group renames / participant edits are not offered (the daemon
  has no API for them).
* Sending from the new-message page is text only (attachments once the chat
  exists).
* Messages from "My other addresses" set only in the app are still drawn as
  incoming bubbles (the daemon reports them as `is_from_me: false`); only
  notifications, unread state and the chat-list "from me" flag treat them as
  the user's. Handles the daemon itself knows (`other_handles`, shown
  read-only in Settings as "Also yours (from the daemon)") arrive as
  `is_from_me: true` and are drawn as the user's own.
* Kirigami's `PageRow` logs "Created graphical object was not placed in the
  graphics scene" for every pushed page (its own `createObject(pagesLogic…)`);
  harmless, filtered in `tst_screens`.
* Inline notification replies depend on the notification server (Plasma
  supports them; elsewhere the action falls back to opening the chat).
* Tray, notifications and D-Bus activation are not exercised by the
  automated tests (no notification server / systemd on the private bus).
* `login-2fa.png` shows a ~96 px pencil over the dialog at ≈(781–877,
  370–466). It is the `document-edit` icon of the New Message page's
  `Kirigami.PlaceholderMessage` *behind* the modal dialog, i.e. our item but
  not our drawing bug: an artefact of Qt Quick's **software** renderer
  (`QT_QUICK_BACKEND=software`, which the offscreen tests use). Evidence from
  instrumenting the test: the only visible item covering that point under the
  dialog is that `Icon` (scene rect 781,370 96×96, opacity 0.75); it shows
  only when the dialog *shrinks* from step 0 to step 1 (opening step 1
  directly, or the larger step-0 dialog, draws correctly: 138 dark pixels in
  the box = dialog text only, vs. 760 with the pencil); setting the icon's
  opacity to 1, or hiding it, removes it; grabbing again 1.5 s later still
  shows it (nothing re-dirties that region). So the translucent icon's stale
  paint is kept by the software renderer's partial update when the popup
  above it shrinks. The test keeps the step 0 → 1 sequence because that is
  the real flow.
