#!/bin/bash
# mac-anisette-install.sh: build tools/mac-anisette/imessage-anisette.m on a
# Mac over ssh and install it as ~/.local/bin/imessage-anisette there.
#
# Usage: tools/mac-anisette-install.sh [user@]host
#
# Needs the Command Line Tools (clang) on the Mac: `xcode-select --install`.
# Afterwards put the printed line in ~/.local/state/imessaged/config.toml
# on Linux, restart imessaged and check with `imsg doctor`.
#
# The command uses `ssh -F ~/.ssh/config` (or -F /dev/null): the daemon's
# sandboxed systemd unit runs in a user namespace where OpenSSH rejects the
# root-owned system ssh_config.d includes ("Bad owner or permissions").
# -o LogLevel=ERROR keeps banners and known_hosts notices (which the daemon
# logs as the command's stderr) out of the journal.
set -eu
host="${1:?usage: $0 [user@]host}"
here="$(cd "$(dirname "$0")" && pwd)"
src="$here/mac-anisette/imessage-anisette.m"
ssh -o BatchMode=yes "$host" 'mkdir -p ~/.local/bin && cat > ~/.local/bin/imessage-anisette.m' < "$src"
ssh -o BatchMode=yes "$host" 'cd ~/.local/bin && clang -fobjc-arc -framework Foundation -o imessage-anisette imessage-anisette.m && rm -f imessage-anisette.m && ./imessage-anisette >/dev/null && echo "installed ~/.local/bin/imessage-anisette on $(hostname)"'
# No login shell on the Mac side, so ~/.local/bin is not on PATH there: use
# the path relative to the home directory.
if [ -f "$HOME/.ssh/config" ]; then conf='~/.ssh/config'; else conf='/dev/null'; fi
cmd="ssh -F $conf -o BatchMode=yes -o LogLevel=ERROR $host .local/bin/imessage-anisette"
if sh -c "$cmd" >/dev/null; then
    echo "add to ~/.local/state/imessaged/config.toml:"
    echo "anisette_command = \"$cmd\""
else
    echo "mac-anisette-install: installed, but '$cmd' failed from here; fix ssh access first" >&2
    exit 1
fi
