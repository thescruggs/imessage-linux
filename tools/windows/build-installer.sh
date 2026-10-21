#!/bin/sh
# Build dist/windows/imessage-<version>-setup.exe with Inno Setup under wine.
# Run tools/windows/build-release.sh first (or build-release.sh --installer).
#
# Inno Setup is downloaded once into ~/.cache/imessage-linux/innosetup and
# installed into its own WINEPREFIX there; ~/.wine is never touched.
# The download is pinned: version and sha256 below (the sha256 is the digest
# GitHub records for the asset on jrsoftware/issrc's release is-6_7_3).
#
# The W3 `imsg` commands the installer and README use are the SERVICE_* and
# APPLE_FILES_CMD variables (overridable from the environment):
# `imsg service install` (creates + starts the logon task `imessaged`),
# `imsg service uninstall`, `imsg setup` (Apple files).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

INNO_VERSION=6.7.3
INNO_SHA256=9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732
INNO_URL="https://github.com/jrsoftware/issrc/releases/download/is-$(echo "$INNO_VERSION" | tr . _)/innosetup-$INNO_VERSION.exe"

# Logon task and imsg subcommands (see packaging/windows/imessage.iss).
TASK_NAME="${TASK_NAME:-imessaged}"
SERVICE_INSTALL_ARGS="${SERVICE_INSTALL_ARGS:-service install}"
SERVICE_UNINSTALL_ARGS="${SERVICE_UNINSTALL_ARGS:-service uninstall}"
APPLE_FILES_CMD="${APPLE_FILES_CMD:-setup}"

cache="${XDG_CACHE_HOME:-$HOME/.cache}/imessage-linux/innosetup"
export WINEPREFIX="$cache/wineprefix"
export WINEDEBUG="${WINEDEBUG:--all}"
# No Mono / Gecko prompts in the dedicated prefix.
export WINEDLLOVERRIDES="${WINEDLLOVERRIDES:-mscoree,mshtml=}"
iscc="$WINEPREFIX/drive_c/InnoSetup6/ISCC.exe"

command -v wine >/dev/null 2>&1 || { echo "build-installer.sh: wine not found" >&2; exit 1; }

if [ ! -f "$iscc" ]; then
    mkdir -p "$cache/dl"
    setup="$cache/dl/innosetup-$INNO_VERSION.exe"
    if [ ! -f "$setup" ]; then
        curl -fL --retry 3 -o "$setup.part" "$INNO_URL"
        mv "$setup.part" "$setup"
    fi
    got=$(sha256sum "$setup" | cut -d' ' -f1)
    if [ "$got" != "$INNO_SHA256" ]; then
        rm -f "$setup"
        echo "build-installer.sh: Inno Setup sha256 mismatch ($got), deleted" >&2
        exit 1
    fi
    wineboot -i >/dev/null 2>&1 || true
    wine "$setup" /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER '/DIR=C:\InnoSetup6' >/dev/null 2>&1 || true
    wineserver -w 2>/dev/null || true
    [ -f "$iscc" ] || { echo "build-installer.sh: Inno Setup install failed" >&2; exit 1; }
fi

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml")
[ -n "$version" ] || { echo "build-installer.sh: no version in Cargo.toml" >&2; exit 1; }

bin="$root/dist/windows/bin"
out="$root/dist/windows"
stage="$out/stage"
for exe in imessaged imsg imsg-tui; do
    [ -f "$bin/$exe.exe" ] || { echo "build-installer.sh: $bin/$exe.exe missing; run build-release.sh" >&2; exit 1; }
done

rm -rf "$stage"
mkdir -p "$stage"
cp "$bin"/imessaged.exe "$bin"/imsg.exe "$bin"/imsg-tui.exe "$stage/"
# CRLF for Notepad on old Windows 10 builds.
sed 's/$/\r/' "$root/LICENSE" >"$stage/LICENSE.txt"
sed -e "s|@@TASK_NAME@@|$TASK_NAME|g" -e "s|@@APPLE_FILES_CMD@@|$APPLE_FILES_CMD|g" -e 's/$/\r/' \
    "$root/packaging/windows/README-Windows.txt" >"$stage/README-Windows.txt"

winpath() { winepath -w "$1" 2>/dev/null; }

rm -f "$out/imessage-$version-setup.exe"
wine "$iscc" /Q \
    "/DAppVersion=$version" \
    "/DStageDir=$(winpath "$stage")" \
    "/DOutputDir=$(winpath "$out")" \
    "/DTaskName=$TASK_NAME" \
    "/DServiceInstallArgs=$SERVICE_INSTALL_ARGS" \
    "/DServiceUninstallArgs=$SERVICE_UNINSTALL_ARGS" \
    "$(winpath "$root/packaging/windows/imessage.iss")"

setup="imessage-$version-setup.exe"
[ -f "$out/$setup" ] || { echo "build-installer.sh: ISCC produced no $setup" >&2; exit 1; }
(cd "$out" && sha256sum "$setup" bin/imessaged.exe bin/imsg.exe bin/imsg-tui.exe >SHA256SUMS)
echo "$out/$setup ($(du -h "$out/$setup" | cut -f1))"
cat "$out/SHA256SUMS"
