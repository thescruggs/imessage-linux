#!/bin/sh
# Screenshots of the portable client (imsg-app) against the fake bridge
# (crates/imsg-remote/examples/fake_bridge.rs), headless under Xvfb, with
# the debug-only IMSG_APP_DEV_* variables (docs/PORTABLE-WINDOWS.md).
#
#   tools/app-screenshots.sh [out-dir]     # default: target/app-screenshots
#
# Writes a light and a dark set, <out-dir>/light/*.png and
# <out-dir>/dark/*.png (IMSG_APP_DEV_THEME), each with chats.png (chat
# list, nothing open) and one transcript per chat:
#   bob.png    1:1 with a picture, my tapback, Bob typing
#   group.png  group with reactions, an edited message, a PDF, a mention
#   sms.png    text-message (SMS) chat
#   links.png  link previews and a failed send
#   dave.png   unverified sender and an unsent message
#   group-picker.png  the group with the tapback picker open on a message
#   settings.png      the settings dialog
# THEMES="light" (or "dark") limits the sets. Needs xvfb-run.
# SLINT_SCALE_FACTOR defaults to 1 (1120×780 pixels; Xvfb's made-up DPI
# would otherwise pick a scale).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/.." && pwd)
out=${1:-$root/target/app-screenshots}
case "$out" in /*) ;; *) out=$(pwd)/$out ;; esac
command -v xvfb-run >/dev/null 2>&1 || { echo "app-screenshots.sh: xvfb-run not found" >&2; exit 1; }

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/app}"
export SLINT_SCALE_FACTOR="${SLINT_SCALE_FACTOR:-1}"
# Headless: winit would prefer a Wayland session over Xvfb's DISPLAY.
unset WAYLAND_DISPLAY
cd "$root"
cargo build -q -p imsg-app
cargo build -q -p imsg-remote --example fake_bridge
app="$CARGO_TARGET_DIR/debug/imsg-app"

work=$(mktemp -d)
bridge_pid=
trap '[ -z "$bridge_pid" ] || kill "$bridge_pid" 2>/dev/null; rm -rf "$work"' EXIT INT TERM
FAKE_BRIDGE_TYPING="mailto:bob@example.com" "$CARGO_TARGET_DIR/debug/examples/fake_bridge" </dev/null >"$work/bridge.out" 2>"$work/bridge.err" &
bridge_pid=$!
i=0
until pairing=$(head -n1 "$work/bridge.out" 2>/dev/null) && [ -n "$pairing" ]; do
    i=$((i + 1))
    if ! kill -0 "$bridge_pid" 2>/dev/null || [ $i -ge 100 ]; then
        echo "app-screenshots.sh: fake bridge did not start" >&2
        cat "$work/bridge.err" >&2
        exit 1
    fi
    sleep 0.1
done

# shot <theme> <name> [chat id] [message id: open its tapback picker]
shot() {
    dir="$work/app-$1-$2"
    png="$out/$1/$2.png"
    mkdir -p "$dir" "$out/$1"
    IMSG_APP_DIR="$dir" IMSG_APP_DEV_PASSPHRASE=devdevdev IMSG_APP_DEV_PAIRING="$pairing" \
        IMSG_APP_DEV_SCREENSHOT="$png" IMSG_APP_DEV_OPEN_CHAT="${3:-}" \
        IMSG_APP_DEV_PICKER="${4:-}" IMSG_APP_DEV_THEME="$1" IMSG_APP_DEV_OVERLAY="${OVERLAY:-}" \
        timeout 90 xvfb-run -a -s "-screen 0 1280x820x24" "$app" 2>"$work/$1-$2.log" || {
        echo "app-screenshots.sh: $1/$2 failed:" >&2
        cat "$work/$1-$2.log" >&2
        exit 1
    }
    [ -s "$png" ] || { echo "app-screenshots.sh: $png missing" >&2; cat "$work/$1-$2.log" >&2; exit 1; }
    echo "$png"
}

for theme in ${THEMES:-light dark}; do
    shot "$theme" chats
    shot "$theme" bob "mailto:bob@example.com"
    shot "$theme" group "group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"
    shot "$theme" sms "tel:+15559876543"
    shot "$theme" links "mailto:eve@example.org"
    shot "$theme" dave "tel:+15551234567"
    # The fake bridge's ids are fixed: 00000023-… is "Perfect, I'll book the site".
    shot "$theme" group-picker "group:5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40" "00000023-FAKE-4000-8000-000000000023"
    OVERLAY=settings shot "$theme" settings
done
