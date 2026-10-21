imsg for Windows -- first-run steps
====================================

imsg for Windows is the Windows build of imessage-linux: the imessaged
background engine plus two clients, the `imsg` command line and the `imsg-tui`
terminal UI. It is an independent project, not made by or affiliated with
Apple. It needs a Mac you own (once for its hardware identity, and on every
sign-in for "anisette" headers over ssh).

The installer put imessaged.exe, imsg.exe and imsg-tui.exe in this folder,
added it to your user PATH (open a NEW terminal to pick that up), created the
logon task "@@TASK_NAME@@" that starts imessaged when you sign in to Windows,
and added Start-menu entries ("imsg TUI", "imsg doctor").

Unsigned build: Windows SmartScreen says "Windows protected your PC" when you
run the installer. Click "More info", then "Run anyway". Microsoft Defender
may also take a moment to scan imessaged.exe on its first start.

Your data lives in %LOCALAPPDATA%\imessaged (state, keys, message store,
config.toml, logs) and %LOCALAPPDATA%\imessage-linux (Apple files). The
uninstaller removes the programs, the PATH entry, the shortcuts and the logon
task, but leaves those folders; delete them yourself for a clean slate.

Run every command below in a new Windows Terminal / PowerShell / cmd window.
`imsg doctor` checks each step and tells you what is still missing.


1. Mac hardware identity (hwconfig.plist)
-----------------------------------------
Copy hwconfig.plist from the Linux machine that already runs imessage-linux
(it is in ~/.local/state/imessaged/hwconfig.plist there), or produce one on
the Mac with tools/mac-hwinfo.sh. Then:

    imsg hwconfig import C:\path\to\hwconfig.plist

Treat the file like a password. Running the same hwconfig on two computers at
the same time may make Apple drop one registration.


2. Apple's files (IMDAppleServices, cert-1.0.plist)
---------------------------------------------------
The NAC emulator needs Apple's IMDAppleServices binary. It is not
redistributable, so it is downloaded (and hash-checked) on your machine:

    imsg @@APPLE_FILES_CMD@@

The files land in %LOCALAPPDATA%\imessage-linux. Without internet access
from this PC, copy the folder ~/.local/share/imessage-linux from the Linux
machine and run `imsg @@APPLE_FILES_CMD@@ --from C:\path\to\that\folder`.


3. ssh to the Mac (anisette)
----------------------------
Apple sign-in needs anisette headers from the Mac whose hwconfig you
imported. imessaged gets them by running a command, normally ssh to the Mac
(see tools/mac-anisette-install.sh in the source tree to install the
`imessage-anisette` helper on the Mac).

Windows 10/11 include the OpenSSH client (C:\Windows\System32\OpenSSH\ssh.exe).
Make sure this works without a password prompt:

    ssh -o BatchMode=yes <user>@<mac-host> .local/bin/imessage-anisette

- Key in the Windows ssh-agent: in an admin PowerShell run
  `Get-Service ssh-agent | Set-Service -StartupType Automatic; Start-Service ssh-agent`,
  then (as yourself) `ssh-add $env:USERPROFILE\.ssh\id_ed25519`.
- Or the 1Password SSH agent: Settings > Developer > "Use the SSH agent"
  (it serves the same \\.\pipe\openssh-ssh-agent pipe; stop and disable the
  Windows ssh-agent service so they do not fight over it).
- Accept the Mac's host key once interactively (`ssh <user>@<mac-host>`).
- `<mac-host>.local` (mDNS) usually resolves on Windows 10/11; if not, use
  the Mac's IP address or a Host alias in %USERPROFILE%\.ssh\config.

Then create %LOCALAPPDATA%\imessaged\config.toml (Notepad is fine; single
quotes mean backslashes are taken literally):

    anisette_command = 'ssh -o BatchMode=yes -o LogLevel=ERROR <user>@<mac-host> .local/bin/imessage-anisette'
    mac_host = '<user>@<mac-host>'

On Windows the command runs through cmd.exe, so `~` is not expanded; ssh
reads %USERPROFILE%\.ssh\config by itself, no `-F` needed.

Restart the engine after editing config.toml:

    imsg service stop
    imsg service start


4. Sign in
----------

    imsg login --apple-id you@example.com

It asks for the password and the two-factor code. `imsg status` should then
show the connection and your registered addresses.


5. Use it
---------

    imsg-tui

or the "imsg TUI" Start-menu entry. `imsg --help` lists the command-line
commands (chats, messages, send, ...).


Troubleshooting
---------------
- `imsg doctor` (or the "imsg doctor" Start-menu entry) checks everything.
- Logs: %LOCALAPPDATA%\imessaged\logs
- "cannot reach imessaged": the logon task is not running. `imsg service
  status` shows it, `imsg service start` starts it (`imsg service install`
  recreates the task), or run imessaged.exe by hand in a terminal to see why
  it stops.
