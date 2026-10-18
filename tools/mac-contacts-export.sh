#!/bin/bash
# mac-contacts-export.sh: copy your Mac's Contacts to this Linux machine as
# vCards, so kmessages shows names and photos instead of phone numbers.
#
# Superseded by `imsg sync-mac --contacts` (which this script runs when imsg
# is installed); `imsg sync-mac` also copies your pinned conversations.
#
# Runs FROM LINUX. One ssh connection reads the Contacts database on the Mac
# (read-only, no Contacts.app, no privacy prompt) and streams back one
# vCard 3.0 per person, with the photo embedded (JPEG, at most 256 px).
# The files land in the folder KPeople reads vCards from; kmessages (and
# any other KPeople user) picks changes up while running.
#
# Usage: tools/mac-contacts-export.sh [--dry-run] HOST [OUTDIR]
#   HOST     ssh destination of the Mac (e.g. mac.local or me@mac.local)
#   OUTDIR   default: ${XDG_DATA_HOME:-~/.local/share}/kpeoplevcard
#   --dry-run  list what would be written; change nothing
#
# Re-running replaces the previous export: files named mac-*.vcf in OUTDIR
# are replaced; other vCards there are left alone. Run it again whenever
# your contacts change.
#
# Needs on the Mac: Remote Login (ssh) and python3 (/usr/bin/python3 from the
# Command Line Tools); the ssh session needs Full Disk Access (System
# Settings > Privacy & Security), as for reading chat.db. sips (stock macOS)
# shrinks photos. The output is your address book: keep it private.

set -eu
set -o pipefail
umask 077

DRY_RUN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) echo "mac-contacts-export: unknown option: $1" >&2; exit 64 ;;
        *) break ;;
    esac
done
[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: $0 [--dry-run] HOST [OUTDIR]" >&2; exit 64; }
HOST="$1"
OUTDIR="${2:-${XDG_DATA_HOME:-$HOME/.local/share}/kpeoplevcard}"
case "$HOST" in -*) echo "mac-contacts-export: invalid host: $HOST" >&2; exit 64 ;; esac

IMSG="${IMSG:-imsg}"
if command -v "$IMSG" >/dev/null 2>&1; then
    args=(sync-mac --contacts --contacts-dir "$OUTDIR")
    [ "$DRY_RUN" -eq 1 ] && args+=(--dry-run)
    exec "$IMSG" "${args[@]}" "$HOST"
fi

# No imsg: run the same exporter (tools/mac-sync-export.py, next to this
# script) over ssh and unpack its contacts here.
PY="$(cd "$(dirname "$0")" && pwd)/mac-sync-export.py"
[ -f "$PY" ] || { echo "mac-contacts-export: needs imsg on PATH or $PY" >&2; exit 1; }
WORK=$(mktemp -d "${TMPDIR:-/tmp}/mac-contacts.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

echo "mac-contacts-export: reading contacts on $HOST …" >&2
mkdir -p "$WORK/new"
status=0
ssh -F "$HOME/.ssh/config" -o BatchMode=yes "$HOST" '/usr/bin/python3 - --contacts' < "$PY" \
    | tar -x --no-same-owner -C "$WORK/new" -f - || status=$?

# Regular files only: a symlink member never reaches OUTDIR.
NEW="$WORK/new/contacts"
count=$(find "$NEW" -maxdepth 1 -type f -name 'mac-*.vcf' 2>/dev/null | wc -l)
if [ "$count" -eq 0 ]; then
    echo "mac-contacts-export: no contacts received (exit status $status); leaving $OUTDIR unchanged" >&2
    exit 1
fi
if [ "$status" -ne 0 ]; then
    echo "mac-contacts-export: the transfer failed (exit status $status); leaving $OUTDIR unchanged" >&2
    exit 1
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "mac-contacts-export: would replace $(find "$OUTDIR" -maxdepth 1 -name 'mac-*.vcf' 2>/dev/null | wc -l) old files in $OUTDIR with $count:" >&2
    find "$NEW" -maxdepth 1 -type f -name 'mac-*.vcf' -printf '%f\n' | sort
    exit 0
fi

mkdir -p "$OUTDIR"
find "$OUTDIR" -maxdepth 1 -name 'mac-*.vcf' -type f -delete
find "$NEW" -maxdepth 1 -type f -name 'mac-*.vcf' -exec mv -t "$OUTDIR"/ -- {} +
echo "mac-contacts-export: wrote $count vCards to $OUTDIR" >&2
