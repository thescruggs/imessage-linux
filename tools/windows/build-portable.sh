#!/bin/sh
# Portable Windows client (imsg-app.exe) for a USB stick, cross-built for
# x86_64-pc-windows-gnu with the same mingw environment as build-release.sh
# (docs/PORTABLE-WINDOWS.md, "Building and running the portable app").
#
#   tools/windows/build-portable.sh            # build, package, zip, DLL check
#   tools/windows/build-portable.sh --smoke    # ... then start it under wine for 15 s
#
# Output:
#   dist/windows/portable/IMSG/                 the folder to copy to the stick
#   dist/windows/portable/imsg-portable-<version>.zip
#
# Size settings (opt-level s, fat LTO, strip) are passed as CARGO_PROFILE_*
# overrides for this build only, in its own target dir
# (target/windows-gnu/portable) so they do not invalidate build-release.sh's.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
target=x86_64-pc-windows-gnu
triple=x86_64-w64-mingw32

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/windows-gnu/portable}"
# shellcheck source=tools/windows/mingw-env.sh
. "$here/mingw-env.sh"
command -v zip >/dev/null 2>&1 || { echo "build-portable.sh: zip not found" >&2; exit 1; }

export CARGO_PROFILE_RELEASE_OPT_LEVEL=s
export CARGO_PROFILE_RELEASE_LTO=true
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_STRIP=true
export CARGO_PROFILE_RELEASE_PANIC=unwind

cd "$root"
cargo build --release --target "$target" -p imsg-app

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
base="$root/dist/windows/portable"
out="$base/IMSG"
rm -rf "$out"
mkdir -p "$out"
cp "$CARGO_TARGET_DIR/$target/release/imsg-app.exe" "$out/"
"$triple-strip" "$out/imsg-app.exe"
cp "$root/packaging/windows/imsg-app.toml.example" "$out/"
cp "$root/packaging/windows/README-portable.txt" "$out/"
cp "$root/crates/imsg-app/assets/fonts/OFL.txt" "$out/LICENSE-fonts.txt"
# The marker: keep config, secrets, cache and logs in this folder.
printf 'imsg-app keeps all its files in this folder while this file exists.\r\n' >"$out/portable"
# Text files with CRLF for Notepad.
for f in "$out/imsg-app.toml.example" "$out/README-portable.txt" "$out/LICENSE-fonts.txt"; do
    sed -i 's/\r*$/\r/' "$f"
done

echo "== DLL imports"
exe="$out/imsg-app.exe"
dlls=$("$triple-objdump" -p "$exe" | sed -n 's/^[[:space:]]*DLL Name: //p' | sort -u | tr '\n' ' ')
echo "imsg-app.exe ($(du -h "$exe" | cut -f1)): $dlls"
for d in $dlls; do
    case "$(echo "$d" | tr 'A-Z' 'a-z')" in
        api-ms-win-*|kernel32.dll|user32.dll|gdi32.dll|shell32.dll|ole32.dll|oleaut32.dll|advapi32.dll|ws2_32.dll|bcrypt.dll|\
        bcryptprimitives.dll|ntdll.dll|comctl32.dll|comdlg32.dll|dwmapi.dll|uxtheme.dll|imm32.dll|dwrite.dll|d2d1.dll|\
        uiautomationcore.dll|userenv.dll|crypt32.dll|secur32.dll|iphlpapi.dll|winmm.dll|propsys.dll|shlwapi.dll|\
        msvcrt.dll|ucrtbase.dll|opengl32.dll|dbghelp.dll|powrprof.dll|setupapi.dll|version.dll|hid.dll|dxgi.dll|\
        d3d11.dll|d3dcompiler_47.dll|windows.ui.dll|combase.dll|oleacc.dll|rpcrt4.dll|msimg32.dll|winspool.drv|\
        dcomp.dll|ntdll.dll|cfgmgr32.dll|mfplat.dll|wtsapi32.dll|netapi32.dll|wininet.dll|winhttp.dll|dnsapi.dll) ;;
        *) echo "build-portable.sh: imsg-app.exe imports a non-system DLL: $d" >&2; exit 1 ;;
    esac
done

zipfile="$base/imsg-portable-$version.zip"
rm -f "$zipfile"
(cd "$base" && zip -q -r -X "$zipfile" IMSG)
echo "== $zipfile ($(du -h "$zipfile" | cut -f1))"

if [ "${1:-}" = --smoke ]; then
    echo "== wine smoke test (15 s; 124 = still running = good)"
    tmp="$CARGO_TARGET_DIR/smoke-appdir"
    rm -rf "$tmp"
    mkdir -p "$tmp"
    set +e
    IMSG_APP_DIR="$tmp" WINEDEBUG=-all timeout 15 wine "$exe"
    echo "exit: $?"
fi
