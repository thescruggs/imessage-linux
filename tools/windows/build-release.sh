#!/bin/sh
# Release build of imessaged.exe, imsg.exe and imsg-tui.exe for
# x86_64-pc-windows-gnu (mingw-w64), cross-compiled from Linux.
#
#   tools/windows/build-release.sh            # build + copy to dist/windows/bin
#   tools/windows/build-release.sh --installer   # ... then build-installer.sh
#   tools/windows/build-release.sh cargo test -p open-absinthe   # any cargo
#                                     # subcommand with the same environment
#                                     # (tests run under wine)
#
# Why gnu and not msvc (xcargo.sh): openssl-src (vendored by rustpush)
# configures `mingw64` happily with a Unix perl, so everything links without
# a prebuilt Windows OpenSSL.
#
# Needs: rustup target x86_64-pc-windows-gnu, mingw-w64-gcc (x86_64-w64-mingw32-*),
# cmake, perl, make; wine for tests and the installer.
#
# Everything target-specific is passed through CARGO_TARGET_X86_64_PC_WINDOWS_GNU_*
# and target-suffixed cc/cmake variables, so the Linux build is not affected.
# The output goes to target/windows-gnu (its own target dir, so this does not
# wait on the build lock of a concurrent Linux / xwin build).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
target=x86_64-pc-windows-gnu
triple=x86_64-w64-mingw32

# shellcheck source=tools/windows/mingw-env.sh
. "$here/mingw-env.sh"

cd "$root"

if [ "${1:-}" = cargo ]; then
    shift
    sub=$1
    shift
    exec cargo "$sub" --target "$target" "$@"
fi

cargo build --release --target "$target" -p imessaged -p imsg -p imsg-tui

bin="$CARGO_TARGET_DIR/$target/release"
out="$root/dist/windows/bin"
mkdir -p "$out"
for exe in imessaged imsg imsg-tui; do
    cp "$bin/$exe.exe" "$out/"
    "$triple-strip" "$out/$exe.exe"
done

echo "== DLL imports"
for exe in "$out"/*.exe; do
    dlls=$("$triple-objdump" -p "$exe" | sed -n 's/^[[:space:]]*DLL Name: //p' | sort -u | tr '\n' ' ')
    echo "$(basename "$exe") ($(du -h "$exe" | cut -f1)): $dlls"
    # Anything from the mingw runtime would have to ship next to the exe.
    case "$dlls" in
        *libgcc*|*libstdc++*|*libwinpthread*|*libatomic*|*libssl*|*libcrypto*)
            echo "build-release.sh: $(basename "$exe") imports a mingw/openssl DLL" >&2; exit 1 ;;
    esac
done

if [ "${1:-}" = --installer ]; then
    exec "$here/build-installer.sh"
fi
