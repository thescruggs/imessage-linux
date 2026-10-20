#!/bin/sh
# Cross-build for x86_64-pc-windows-msvc from Linux with cargo-xwin.
#
#   tools/windows/xcargo.sh check --workspace --all-targets
#   tools/windows/xcargo.sh clippy --workspace --all-targets
#   tools/windows/xcargo.sh test -p imessage-common -p imsg-client -p open-absinthe   # under wine
#   tools/windows/xcargo.sh build -p imsg -p imsg-tui    # links (no rustpush/openssl)
#
# Under wine %LOCALAPPDATA% is the prefix's C:\users\<you>\AppData\Local, so
# open-absinthe's emulator tests SKIP until IMDAppleServices and
# cert-1.0.plist are copied to its imessage-linux\ folder (a throwaway
# WINEPREFIX keeps ~/.wine clean). The live tests also SKIP: wine has no
# curl.exe.
#
# Needs: cargo-xwin (`cargo install cargo-xwin`; it downloads the MSVC CRT and
# Windows SDK on first use and accepts Microsoft's license for them), clang-cl,
# lld-link, llvm-lib, llvm-ml, cmake, ninja, nasm; wine to run tests.
#
# Two workarounds for unicorn-engine-sys (open-absinthe):
# * cargo-xwin's CMake toolchain sets CMAKE_MSVC_RUNTIME_LIBRARY while the
#   cmake crate also passes -MD in CMAKE_C_FLAGS; unicorn's CMakeLists refuses
#   both. We hand cmake a copy of the toolchain file without that line (the
#   hyphenated env var wins over the one cargo-xwin sets).
# * unicorn assembles a MASM file with ml64, which is `llvm-ml -m64` here.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
target=x86_64-pc-windows-msvc
xwin_cmake="${XWIN_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/cargo-xwin}/cmake/clang-cl"
out="$root/target/xwin-cmake"

export XWIN_ACCEPT_LICENSE=1
if [ ! -f "$xwin_cmake/$target-toolchain.cmake" ]; then
    # First run: let cargo-xwin download the SDK and write its toolchain files.
    cargo xwin env --target "$target" >/dev/null
fi
mkdir -p "$out"
sed 's/^set(CMAKE_MSVC_RUNTIME_LIBRARY.*$/# &/' "$xwin_cmake/$target-toolchain.cmake" >"$out/$target-toolchain.cmake"
cp "$xwin_cmake/override.cmake" "$out/"

sub=$1
shift

# openssl-src (vendored via rustpush) refuses to configure VC-WIN64A with a
# Unix perl. For `check`/`clippy` nothing is linked, so point openssl-sys at
# the host headers and empty import-library stand-ins. Anything that links
# rustpush (imessaged; the imsg / imsg-tui tests) needs a real Windows
# OpenSSL: set OPENSSL_DIR to one.
if { [ "$sub" = check ] || [ "$sub" = clippy ]; } && [ -z "${OPENSSL_DIR:-}" ]; then
    mkdir -p "$out/openssl-lib"
    : >"$out/openssl-lib/libssl.lib"
    : >"$out/openssl-lib/libcrypto.lib"
    export OPENSSL_NO_VENDOR=1 OPENSSL_INCLUDE_DIR=/usr/include OPENSSL_LIB_DIR="$out/openssl-lib"
elif [ -n "${OPENSSL_DIR:-}" ]; then
    export OPENSSL_NO_VENDOR=1
fi

PATH="$here/xbin:$PATH" exec env "CMAKE_TOOLCHAIN_FILE_$target=$out/$target-toolchain.cmake" \
    cargo xwin "$sub" --target "$target" "$@"
