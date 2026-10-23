# Shared by build-release.sh and build-portable.sh (sourced, POSIX sh):
# the mingw-w64 cross environment for x86_64-pc-windows-gnu. Expects
# $root (repo root), $target and $triple to be set by the caller.
#
# Everything target-specific is passed through CARGO_TARGET_X86_64_PC_WINDOWS_GNU_*
# and target-suffixed cc/cmake variables, so the Linux build is not affected.
# The output goes to target/windows-gnu (its own target dir, so this does not
# wait on the build lock of a concurrent Linux / xwin build).

for tool in "$triple-gcc" "$triple-g++" "$triple-ar" "$triple-windres" cmake perl make; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$(basename "$0"): $tool not found" >&2; exit 1; }
done
rustup target list --installed 2>/dev/null | grep -qx "$target" || {
    echo "$(basename "$0"): run 'rustup target add $target'" >&2; exit 1; }

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

