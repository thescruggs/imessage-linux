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

for tool in "$triple-gcc" "$triple-g++" "$triple-ar" "$triple-windres" cmake perl make; do
    command -v "$tool" >/dev/null 2>&1 || { echo "build-release.sh: $tool not found" >&2; exit 1; }
done
rustup target list --installed 2>/dev/null | grep -qx "$target" || {
    echo "build-release.sh: run 'rustup target add $target'" >&2; exit 1; }

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/windows-gnu}"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="$triple-gcc"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER="${CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER:-wine}"
# -static: libgcc, libstdc++ and winpthread come from their .a archives, so
# the .exe needs only system DLLs (plus the UCRT api-ms-win-crt-* sets, part
# of Windows 10+). unicorn-engine-sys asks for `rustc-link-lib=pthread`
# (dylib), which rustc wraps in -Bdynamic and so finds libpthread.dll.a ->
# libwinpthread-1.dll despite -static; the shim dir, searched first, maps
# that name to the static libwinpthread.a. (No libatomic: unicorn built by
# mingw has no __atomic_*_16 references.)
shim="$CARGO_TARGET_DIR/mingw-shim"
mkdir -p "$shim"
ln -sf "$("$triple-gcc" -print-file-name=libwinpthread.a)" "$shim/libpthread.dll.a"
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-L native=$shim -C link-arg=-static -C link-arg=-static-libgcc"
# cc / cmake crates (openssl-src, rusqlite, unicorn, zstd, ring ...)
export CC_x86_64_pc_windows_gnu="$triple-gcc"
export CXX_x86_64_pc_windows_gnu="$triple-g++"
export AR_x86_64_pc_windows_gnu="$triple-ar"
export RANLIB_x86_64_pc_windows_gnu="$triple-ranlib"
export WINDRES_x86_64_pc_windows_gnu="$triple-windres"
# The cmake crate picks CMAKE_SYSTEM_NAME etc. from this toolchain file.
mkdir -p "$CARGO_TARGET_DIR"
cat >"$CARGO_TARGET_DIR/mingw-toolchain.cmake" <<EOF
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR AMD64)
set(CMAKE_C_COMPILER $triple-gcc)
set(CMAKE_CXX_COMPILER $triple-g++)
set(CMAKE_RC_COMPILER $triple-windres)
set(CMAKE_AR $(command -v "$triple-ar"))
set(CMAKE_RANLIB $(command -v "$triple-ranlib"))
set(CMAKE_FIND_ROOT_PATH /usr/$triple)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
EOF
export CMAKE_TOOLCHAIN_FILE_x86_64_pc_windows_gnu="$CARGO_TARGET_DIR/mingw-toolchain.cmake"
# Keep wine quiet and away from ~/.wine when tests run.
export WINEDEBUG="${WINEDEBUG:--all}"

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
