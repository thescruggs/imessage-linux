imsg portable client (imsg-app.exe)
===================================

A Messages client that runs from this folder (a USB stick is fine) and talks
to imessaged on your Linux host through imsg-bridge, over Tailscale. Nothing
is installed. See docs/PORTABLE-WINDOWS.md and docs/BRIDGE.md in the source
tree for details.

What you need
-------------
* Windows 10 or 11 (64-bit). No other software: the exe uses only DLLs that
  are part of Windows.
* This PC on your tailnet (the Tailscale app) so it can reach the bridge,
  e.g. http://100.64.0.10:7227.
* On the host: imessaged and imsg-bridge running
  (systemctl --user enable --now imsg-bridge).

First start
-----------
1. On the host, create a token for this stick:
       imsg-bridge token new work-stick
   It prints a pairing string once: imsg+bridge://100.64.0.10:7227#....
2. Start imsg-app.exe, paste the pairing string, and choose a passphrase
   (at least 8 characters). The passphrase encrypts the token in
   secrets.bin (argon2id + AES-256-GCM); you type it at every start.
   If imsg-app.toml sets "bridge", pasting just the token also works.

Keys
----
Enter sends, Shift+Enter starts a new line. Ctrl+N new message, Ctrl+F
search chats, Ctrl+, settings, Esc closes a dialog.

Colours follow the Windows light/dark app setting; Settings > Colours
(or theme = "light" / "dark" in imsg-app.toml) fixes one.

Files in this folder
--------------------
imsg-app.exe            the app
portable                marker: keep all files in this folder (delete it, and
                        without an imsg-app.toml here the app uses
                        %LOCALAPPDATA%\imsg-app instead)
imsg-app.toml.example   settings template; copy to imsg-app.toml to use it
imsg-app.toml           settings (written when you change settings)
secrets.bin             bridge URL + token, encrypted under your passphrase
cache\                  temporary data; cache\open\ holds attachments you
                        opened in another app and is wiped at start and exit
logs\                   only if "log = true" (handles masked, no message text)
LICENSE-fonts.txt       licence of the embedded Noto Sans font (SIL OFL 1.1)

Messages are never stored on the stick: they stay on the host.

If the stick is lost
--------------------
Two independent cuts; do both:
1. Revoke the token on the host:
       imsg-bridge token revoke work-stick
   The bridge refuses the stick from its next request on.
2. If the stick was ever used with its own Tailscale node / auth key
   (a later version), revoke that key and remove the node in the Tailscale
   admin console. With the work PC's own Tailscale (the current setup) there
   is nothing to revoke there, but consider whether the PC itself is safe.
Then check ~/.local/state/imsg-bridge/access.log on the host for use after
the loss. Without the passphrase, secrets.bin is useless to a finder; with a
weak passphrase it can be guessed offline (argon2id slows that down a lot),
so revoke anyway.

What Windows still records (unavoidable)
----------------------------------------
The app writes nothing outside this folder, but Windows itself notes that a
program ran. These traces show that imsg-app.exe was started, not what you
read or wrote:
* Prefetch (C:\Windows\Prefetch\IMSG-APP.EXE-*.pf) and the Amcache /
  ShimCache program inventories.
* UserAssist / RecentApps / jump-list style launch records in the user
  profile and registry (HKCU), if you start it from Explorer.
* The notification-area (tray) icon cache: Explorer remembers icons it has
  seen (HKCU\Software\Classes\Local Settings\...\TrayNotify).
* Microsoft Defender / SmartScreen scan and reputation logs; other endpoint
  security software may log or upload the exe.
* Windows Firewall / network logs of the connection to the bridge, and
  Tailscale's own logs of traffic to the host.
* Native Save / Open dialogs remember recently used folders (HKCU
  ComDlg32 MRU) when you save or attach a file.
* The page file / hibernation file may hold app memory, as for any program.
Notifications show only the sender's name unless you turn on
"Show the message text" in Settings.
