#!/usr/bin/env bash
# Build the Arch package from a local checkout instead of the GitHub tag
# (e.g. before the tag is pushed). Copies PKGBUILD + install file into
# OUTDIR, points the top-level source at git+file://<checkout>#branch=<branch>
# and every submodule source at the checkout's own submodule clone (no
# network for the git sources), then runs makepkg there.
#
# Usage: packaging/arch/build-local.sh [OUTDIR] [-- makepkg args...]
#   OUTDIR   default: ./pkgbuild-local (relative to the current directory)
#   BRANCH   environment, default: the checkout's current branch
# Example: packaging/arch/build-local.sh /tmp/pkg -- -f --nocheck
# Commit first: makepkg builds the branch's last commit, not the work tree.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="./pkgbuild-local"
if [[ $# -gt 0 && "$1" != "--" ]]; then out="$1"; shift; fi
[[ "${1:-}" == "--" ]] && shift
branch="${BRANCH:-$(git -C "$root" rev-parse --abbrev-ref HEAD)}"

mkdir -p "$out"
out="$(cd "$out" && pwd)"
cp "$here/PKGBUILD" "$here/imessage-linux.install" "$out/"

rp="$root/third_party/rustpush"
sub() { # sub <source name> <checkout dir>
    [[ -e "$2/.git" ]] || { echo "build-local: $2 is not checked out (run tools/bootstrap.sh --no-download)" >&2; exit 1; }
    sed -i "s|'$1::git+https://github.com/OpenBubbles/[^']*'|'$1::git+file://$2'|" "$out/PKGBUILD"
}
sed -i "s|\"imessage-linux::git+https://github.com/thescruggs/imessage-linux.git#tag=v\${pkgver}\"|\"imessage-linux::git+file://$root#branch=$branch\"|" "$out/PKGBUILD"
sub rustpush "$rp"
sub apple-private-apis "$rp/third_party/apple-private-apis"
sub clearadi-stub "$rp/third_party/apple-private-apis/clearadi"
sub rtc "$rp/third_party/rtc"
sub rustls "$rp/third_party/rustls"
sub rustls-platform-verifier "$rp/third_party/rustls-platform-verifier"
sub quinn "$rp/third_party/quinn"
if grep -q 'git+https://' "$out/PKGBUILD"; then
    echo "build-local: some sources still point at GitHub:" >&2
    grep -n 'git+https://' "$out/PKGBUILD" >&2
    exit 1
fi

cd "$out"
exec makepkg "$@"
