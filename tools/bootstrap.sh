#!/usr/bin/env bash
# Bootstrap a checkout of imessage-linux:
#   1. initialise third_party/rustpush and its nested submodules (except the
#      closed `open-absinthe` stub, which crates/open-absinthe replaces);
#   2. apply patches/ to the submodules (idempotent);
#   3. download + sha1-verify Apple's IMDAppleServices (NAC emulator input)
#      and download + sha256-verify Apple's public identity cert-1.0.plist
#      into the data dir. The local cert copy is for tests / `imsg doctor`
#      only; rustpush fetches the cert from the IDS bag at runtime.
#
# Usage: tools/bootstrap.sh [--no-download | --download-only]
#   --no-download    steps 1-2 only (what a package build runs)
#   --download-only  step 3 only (what an installed package needs:
#                    /usr/share/imessage-linux/tools/bootstrap.sh)
# Outside a source checkout, --download-only is implied.
# Safe to re-run. Runnable from any working directory.
set -euo pipefail

NO_DOWNLOAD=0
DOWNLOAD_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --no-download) NO_DOWNLOAD=1 ;;
        --download-only) DOWNLOAD_ONLY=1 ;;
        -h|--help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "bootstrap: unknown argument: $arg" >&2; exit 64 ;;
    esac
done
if [[ $NO_DOWNLOAD -eq 1 && $DOWNLOAD_ONLY -eq 1 ]]; then
    echo "bootstrap: --no-download and --download-only exclude each other" >&2
    exit 64
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUSTPUSH="$ROOT/third_party/rustpush"
APA="$RUSTPUSH/third_party/apple-private-apis"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/imessage-linux"
# Never leave a partial download behind (fetch writes <dest>.part.$$).
trap 'rm -f "$DATA_DIR"/*.part.$$' EXIT

NAC_URL="https://github.com/JJTech0130/nacserver/raw/main/IMDAppleServices"
NAC_SHA1="e1181ccad82e6629d52c6a006645ad87ee59bd13"
CERT_URL="http://static.ess.apple.com/identity/validation/cert-1.0.plist"
# Served over plain http, so pin it. Keep in sync with VALIDATION_CERT_SHA256
# in crates/imessage-common/src/paths.rs (checked by `imsg doctor`).
CERT_SHA256="6ef3521ed899be820e132a6d8c53cac955267c2c3f862684477524be91dc3a06"

# Nested submodules use git@github.com: URLs; rewrite to https for this run only.
GIT=(git -c "url.https://github.com/.insteadOf=git@github.com:")

# rustpush submodules we need (everything except `open-absinthe`).
NESTED=(
    third_party/apple-private-apis
    third_party/rtc
    third_party/rustls
    third_party/rustls-platform-verifier
    third_party/quinn
)

summary=()
note() { echo "bootstrap: $*"; }
die() { echo "bootstrap: ERROR: $*" >&2; exit 1; }

# An installed copy (/usr/share/imessage-linux/tools) has no checkout to set up.
if [[ $DOWNLOAD_ONLY -eq 0 && ! -e "$ROOT/.git" ]]; then
    [[ $NO_DOWNLOAD -eq 1 ]] && die "$ROOT is not a source checkout; nothing to do with --no-download"
    note "$ROOT is not a source checkout: downloading Apple's files only"
    DOWNLOAD_ONLY=1
fi

if [[ $DOWNLOAD_ONLY -eq 0 ]]; then
# --- 1. submodules ---------------------------------------------------------
note "initialising submodules"
"${GIT[@]}" -C "$ROOT" submodule update --init third_party/rustpush
"${GIT[@]}" -C "$RUSTPUSH" submodule update --init --recursive -- "${NESTED[@]}"
summary+=("rustpush        $(git -C "$RUSTPUSH" rev-parse --short HEAD)")
summary+=("apple-priv-apis $(git -C "$APA" rev-parse --short HEAD)")

# --- 2. patches ------------------------------------------------------------
# apply_patch <repo dir> <patch file>
apply_patch() {
    local dir="$1" patch="$2" name
    name="$(basename "$(dirname "$patch")")/$(basename "$patch")"
    if git -C "$dir" apply --check --reverse "$patch" 2>/dev/null; then
        note "patch $name: already applied"
        summary+=("patch $name: already applied")
    elif git -C "$dir" apply --check "$patch" 2>/dev/null; then
        git -C "$dir" apply --whitespace=nowarn "$patch"
        note "patch $name: applied"
        summary+=("patch $name: applied")
    else
        git -C "$dir" apply --check "$patch" || true
        die "patch $name does not apply to $dir (submodule at an unexpected commit, or locally modified?)"
    fi
}

shopt -s nullglob
# apple-private-apis first: rustpush's 01 patch references its local-anisette feature.
for p in "$ROOT"/patches/apple-private-apis/*.patch; do apply_patch "$APA" "$p"; done
for p in "$ROOT"/patches/rustpush/*.patch; do apply_patch "$RUSTPUSH" "$p"; done
shopt -u nullglob
fi

# --- 3. downloads ----------------------------------------------------------
sha1_of() { sha1sum "$1" | awk '{print $1}'; }
sha256_of() { sha256sum "$1" | awk '{print $1}'; }

fetch() { # fetch <url> <dest>
    local tmp="$2.part.$$"
    if ! curl -fsSL --retry 3 --connect-timeout 20 -o "$tmp" "$1"; then
        rm -f "$tmp"
        die "download failed: $1"
    fi
    mv -f "$tmp" "$2"
}

if [[ $NO_DOWNLOAD -eq 1 ]]; then
    summary+=("downloads: skipped (--no-download)")
else
    command -v curl >/dev/null || die "curl is required"
    command -v sha1sum >/dev/null || die "sha1sum is required"
    command -v sha256sum >/dev/null || die "sha256sum is required"
    mkdir -p "$DATA_DIR"
    chmod 700 "$DATA_DIR"

    nac="$DATA_DIR/IMDAppleServices"
    if [[ -f "$nac" && "$(sha1_of "$nac")" == "$NAC_SHA1" ]]; then
        note "IMDAppleServices present and verified"
    else
        [[ -f "$nac" ]] && note "IMDAppleServices present but hash is wrong; re-downloading"
        note "downloading IMDAppleServices"
        fetch "$NAC_URL" "$nac"
        got="$(sha1_of "$nac")"
        if [[ "$got" != "$NAC_SHA1" ]]; then
            rm -f "$nac"
            die "IMDAppleServices sha1 mismatch: expected $NAC_SHA1, got $got (file deleted)"
        fi
    fi
    chmod 600 "$nac"
    summary+=("IMDAppleServices $nac ($(stat -c %s "$nac") bytes, sha1 ok)")

    cert="$DATA_DIR/cert-1.0.plist"
    if [[ -f "$cert" && "$(sha256_of "$cert")" == "$CERT_SHA256" ]]; then
        note "cert-1.0.plist present and verified"
    else
        [[ -f "$cert" ]] && note "cert-1.0.plist present but hash is wrong; re-downloading"
        note "downloading cert-1.0.plist"
        fetch "$CERT_URL" "$cert"
        got="$(sha256_of "$cert")"
        if [[ "$got" != "$CERT_SHA256" ]]; then
            rm -f "$cert"
            die "cert-1.0.plist sha256 mismatch: expected $CERT_SHA256, got $got (file deleted)"
        fi
    fi
    summary+=("cert-1.0.plist   $cert ($(stat -c %s "$cert") bytes, sha256 ok)")
fi

# --- 4. summary ------------------------------------------------------------
echo
echo "bootstrap complete:"
for line in "${summary[@]}"; do echo "  $line"; done
echo
if [[ $DOWNLOAD_ONLY -eq 1 ]]; then
    echo "next: imsg hwconfig import /path/to/hwconfig.plist && imsg doctor"
else
    echo "next: cargo build --workspace && cargo test --workspace"
    echo "      cargo build --manifest-path third_party/rustpush/Cargo.toml --lib --features remote-anisette-v3"
fi
