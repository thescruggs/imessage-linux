#!/bin/bash
# mac-hwinfo.sh: run ONCE on your own Mac (Intel or Apple Silicon) to extract
# the hardware identity imessage-linux needs for iMessage registration.
#
# Writes hwconfig.plist in the layout documented in
# crates/imessage-common/src/hwconfig.rs (outer MacOSConfig keys + `inner`
# HardwareConfig dict). Copy that file to the Linux machine and run
#     imsg hwconfig import hwconfig.plist
#
# THE OUTPUT IS A DEVICE CREDENTIAL. Do not share it, post it, or reuse it for
# more than your own account; Apple bans Mac identities that are shared.
#
# Usage: tools/mac-hwinfo.sh [-o OUTPUT] [--dry-run]
#   -o OUTPUT   output path (default: ./hwconfig.plist)
#   --dry-run   print the commands that would be run; read nothing, write nothing
#
# Needs only stock macOS tools: ioreg, sysctl, nvram, plutil, base64, od,
# shasum, osascript, and xxd or perl (all ship with macOS). Compatible with
# bash 3.2.

set -eu
umask 077

OUT="./hwconfig.plist"
DRY_RUN=0
while [ $# -gt 0 ]; do
    case "$1" in
        -o) [ $# -ge 2 ] || { echo "mac-hwinfo: -o needs a path" >&2; exit 64; }
            OUT="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "mac-hwinfo: unknown argument: $1" >&2; exit 64 ;;
    esac
done

if [ "$DRY_RUN" -eq 0 ] && [ "$(uname -s)" != "Darwin" ]; then
    echo "mac-hwinfo: this script must run on macOS (use --dry-run to preview)" >&2
    exit 1
fi

NVRAM_GUID="4D1EDE05-38C7-4A6A-9CC6-4BCCA8B38C14"

# --- command wrapper (honours --dry-run) -----------------------------------
run() {
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "+ $*" >&2
        return 0
    fi
    "$@" 2>/dev/null || true
}

# --- byte helpers (hex is the canonical intermediate form) -----------------
have() { command -v "$1" >/dev/null 2>&1; }

# stdin bytes -> lowercase hex, no whitespace
bin2hex() { od -An -v -tx1 | tr -d ' \n\t'; }

# stdin hex -> bytes
hex2bin() {
    if have xxd; then
        xxd -r -p
    else
        perl -e 'binmode STDOUT; local $/; my $h = <STDIN>; $h =~ s/[^0-9a-fA-F]//g; print pack("H*", $h);'
    fi
}

# hex -> base64 (single line)
hex2b64() { printf '%s' "$1" | hex2bin | base64 | tr -d '\n'; }

# hex -> printable string: drop NULs, trim surrounding whitespace
hex2str() {
    printf '%s' "$1" | hex2bin | tr -d '\000' | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

str2hex() { printf '%s' "$1" | bin2hex; }

b64_decode() { base64 -D 2>/dev/null || base64 -d; }

# Trim NULs / whitespace from a plain string.
clean() { printf '%s' "$1" | tr -d '\000\r\n' | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'; }

# --- ioreg dumps (cached) ---------------------------------------------------
TMP=""
if [ "$DRY_RUN" -eq 0 ]; then
    TMP="$(mktemp -d "${TMPDIR:-/tmp}/mac-hwinfo.XXXXXX")"
    trap 'rm -rf "$TMP"' EXIT
fi

dump() { # dump NAME cmd...   (writes $TMP/NAME once)
    name="$1"; shift
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "+ $*" >&2
        return 0
    fi
    [ -f "$TMP/$name" ] || { "$@" > "$TMP/$name" 2>/dev/null || true; }
}

dump_all() {
    dump pe      ioreg -w0 -rd1 -c IOPlatformExpertDevice
    dump chosen  ioreg -w0 -p IODeviceTree -rd1 -n chosen
    dump options ioreg -w0 -p IODeviceTree -rd1 -n options
    dump iopower ioreg -w0 -p IOPower -l -d2
    dump eth     ioreg -w0 -l -r -c IOEthernetController -d2
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "+ ioreg -w0 -l    (only if a key is missing from the dumps above)" >&2
    fi
}

# rhs2hex RHS : an ioreg value as hex. Handles <"string">, <hexdata>,
# "string" and bare values.
rhs2hex() {
    case "$1" in
        '<"'*'">') v="${1#<\"}"; v="${v%\">}"; str2hex "$v" ;;
        '<'*'>')   v="${1#<}"; v="${v%>}"; printf '%s' "$v" | tr -d ' ' | tr 'A-F' 'a-f' ;;
        '"'*'"')   v="${1#\"}"; v="${v%\"}"; str2hex "$v" ;;
        *)         str2hex "$1" ;;
    esac
}

# prop_hex KEY DUMP... : value of "KEY" in the first dump containing it, as hex.
prop_hex() {
    key="$1"; shift
    [ "$DRY_RUN" -eq 1 ] && return 0
    for d in "$@"; do
        if [ "$d" = full ]; then dump full ioreg -w0 -l; fi
        [ -f "$TMP/$d" ] || continue
        rhs="$(grep -F "\"$key\" = " "$TMP/$d" | head -n 1 | sed -e 's/^.*"'"$key"'" = //' -e 's/[[:space:]]*$//')" || true
        [ -n "$rhs" ] || continue
        rhs2hex "$rhs"
        return 0
    done
    return 0
}

prop_str() { h="$(prop_hex "$@")"; [ -n "$h" ] && hex2str "$h"; return 0; }

# nvram_hex VAR : NVRAM variable bytes as hex (via `nvram -x`; the value is
# either XML <data> (base64) or <string>).
nvram_hex() {
    if [ "$DRY_RUN" -eq 1 ]; then echo "+ nvram -x $1" >&2; return 0; fi
    xml="$(nvram -x "$1" 2>/dev/null | tr -d '\n\t')" || true
    b64="$(printf '%s' "$xml" | sed -n 's/.*<data>\([^<]*\)<\/data>.*/\1/p' | tr -d ' ')"
    if [ -n "$b64" ]; then
        printf '%s' "$b64" | b64_decode | bin2hex
        return 0
    fi
    sv="$(printf '%s' "$xml" | sed -n 's/.*<string>\([^<]*\)<\/string>.*/\1/p' \
        | sed -e 's/&lt;/</g' -e 's/&gt;/>/g' -e 's/&amp;/\&/g')"
    [ -n "$sv" ] && str2hex "$sv"
    return 0
}

# --- collect ----------------------------------------------------------------
dump_all

ARM=0
if [ "$DRY_RUN" -eq 1 ]; then
    echo "+ sysctl -n hw.optional.arm64" >&2
else
    [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" = "1" ] && ARM=1
fi

PRODUCT_NAME="$(clean "$(run sysctl -n hw.model)")"
[ -n "$PRODUCT_NAME" ] || PRODUCT_NAME="$(prop_str model pe)"
[ -n "$PRODUCT_NAME" ] || PRODUCT_NAME="$(prop_str product-name pe)"

SERIAL="$(prop_str IOPlatformSerialNumber pe)"
PLATFORM_UUID="$(prop_str IOPlatformUUID pe)"
ROOT_DISK_UUID="$(prop_str boot-uuid chosen full)"

# MAC address of the primary interface's controller; fallback: en0.
MAC_HEX=""
if [ "$DRY_RUN" -eq 0 ] && [ -f "$TMP/eth" ]; then
    # Raw RHS (<hex> or <"printable">), normalised like prop_hex values.
    mac_rhs="$(awk '
        /"IOMACAddress" = </ { m = $0; sub(/.*"IOMACAddress" = /, "", m); sub(/[[:space:]]+$/, "", m) }
        /"IOPrimaryInterface" = Yes/ { if (m != "") { print m; exit } }
    ' "$TMP/eth")"
    if [ -n "$mac_rhs" ]; then MAC_HEX="$(rhs2hex "$mac_rhs")"; fi
fi
if [ -z "$MAC_HEX" ]; then
    MAC_HEX="$(run ifconfig en0 | awk '/ether /{print $2; exit}' | tr -d ':' | tr 'A-F' 'a-f')"
fi

# board-id: Intel exposes "Mac-XXXXXXXXXXXXXXXX"; Apple Silicon only has a
# numeric chosen/board-id, presented as "Mac-" + hex.
BOARD_ID="$(prop_str board-id pe)"
case "$BOARD_ID" in
    Mac-*) ;;
    *)  bid_hex="$(prop_hex board-id chosen)"
        BOARD_ID=""
        [ -n "$bid_hex" ] && BOARD_ID="Mac-$(printf '%s' "$bid_hex" | tr 'a-f' 'A-F')" ;;
esac

OS_BUILD="$(clean "$(run sysctl -n kern.osversion)")"
OS_VERSION="$(clean "$(run sysctl -n kern.osproductversion)")"
DARWIN="$(clean "$(run sysctl -n kern.osrelease)")"

# ROM / MLB: on both architectures prefer the NVRAM values (IODeviceTree
# options node, then `nvram -x`); only if absent fall back to derived values.
ROM_SRC=""
ROM_HEX="$(prop_hex "$NVRAM_GUID:ROM" options)"
if [ -n "$ROM_HEX" ]; then ROM_SRC="nvram (ioreg options)"; fi
if [ -z "$ROM_HEX" ]; then
    ROM_HEX="$(nvram_hex "$NVRAM_GUID:ROM")"
    if [ -n "$ROM_HEX" ]; then ROM_SRC="nvram (nvram -x)"; fi
fi
if [ -z "$ROM_HEX" ] && [ "$ARM" -eq 1 ]; then
    # Apple Silicon: last 6 bytes of sha256(chosen/unique-chip-id).
    chip_hex="$(prop_hex unique-chip-id chosen full)"
    if [ -n "$chip_hex" ]; then
        sha="$(printf '%s' "$chip_hex" | hex2bin | shasum -a 256 | awk '{print $1}')"
        ROM_HEX="$(printf '%s' "$sha" | tail -c 12)"
        if [ -n "$ROM_HEX" ]; then ROM_SRC="derived (sha256 of unique-chip-id)"; fi
    fi
fi

MLB_SRC=""
MLB="$(prop_str "$NVRAM_GUID:MLB" options)"
if [ -n "$MLB" ]; then MLB_SRC="nvram (ioreg options)"; fi
if [ -z "$MLB" ]; then
    mlb_hex="$(nvram_hex "$NVRAM_GUID:MLB")"
    if [ -n "$mlb_hex" ]; then MLB="$(hex2str "$mlb_hex")"; fi
    if [ -n "$MLB" ]; then MLB_SRC="nvram (nvram -x)"; fi
fi
if [ -z "$MLB" ]; then
    # Apple Silicon (and some Intel models): mlb-serial-number property.
    MLB="$(prop_str mlb-serial-number pe chosen full)"
    if [ -n "$MLB" ]; then MLB_SRC="derived (mlb-serial-number)"; fi
fi

# FairPlay-encrypted copies. They live on the IOPower plane root entry, but
# `ioreg` does not print them on recent macOS (verified: macOS 26 / M2 shows
# none of them in any plane dump), while IORegistryEntryCreateCFProperty
# returns them fine. So after the ioreg dumps, fall back to IOKit via
# osascript (JavaScript for Automation, stock on every Mac), then python3.
iokit_enc_dump() { # writes $TMP/iokit: "KEY base64" per line
    [ "$DRY_RUN" -eq 1 ] && return 0
    [ -f "$TMP/iokit" ] && return 0
    : > "$TMP/iokit"
    osascript -l JavaScript - > "$TMP/iokit" 2>/dev/null <<'JXA' || true
ObjC.import("IOKit"); ObjC.import("CoreFoundation"); ObjC.import("Foundation");
const ent = $.IORegistryEntryFromPath(0, "IOPower:/");
const out = [];
for (const key of ["Gq3489ugfi", "Fyp98tpgj", "kbjfrfpoJU", "oycqAZloTNDm", "abKPld1EcMni"]) {
    const v = $.IORegistryEntryCreateCFProperty(ent, $(key), null, 0);
    const d = $.CFBridgingRelease(v);
    if (d && !d.isNil()) out.push(key + " " + ObjC.unwrap(d.base64EncodedStringWithOptions(0)));
}
out.join("\n");
JXA
    if ! grep -q . "$TMP/iokit" && have python3; then
        python3 - > "$TMP/iokit" 2>/dev/null <<'PY' || true
import ctypes
iokit = ctypes.CDLL("/System/Library/Frameworks/IOKit.framework/IOKit")
cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
iokit.IORegistryEntryFromPath.restype = ctypes.c_uint32
iokit.IORegistryEntryFromPath.argtypes = [ctypes.c_uint32, ctypes.c_char_p]
iokit.IORegistryEntryCreateCFProperty.restype = ctypes.c_void_p
iokit.IORegistryEntryCreateCFProperty.argtypes = [ctypes.c_uint32, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32]
cf.CFStringCreateWithCString.restype = ctypes.c_void_p
cf.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint32]
cf.CFDataGetLength.restype = ctypes.c_long
cf.CFDataGetLength.argtypes = [ctypes.c_void_p]
cf.CFDataGetBytePtr.restype = ctypes.POINTER(ctypes.c_ubyte)
cf.CFDataGetBytePtr.argtypes = [ctypes.c_void_p]
cf.CFGetTypeID.restype = ctypes.c_ulong
cf.CFGetTypeID.argtypes = [ctypes.c_void_p]
cf.CFDataGetTypeID.restype = ctypes.c_ulong
import base64
ent = iokit.IORegistryEntryFromPath(0, b"IOPower:/")
for key in [b"Gq3489ugfi", b"Fyp98tpgj", b"kbjfrfpoJU", b"oycqAZloTNDm", b"abKPld1EcMni"]:
    k = cf.CFStringCreateWithCString(None, key, 0x08000100)
    v = iokit.IORegistryEntryCreateCFProperty(ent, k, None, 0)
    if v and cf.CFGetTypeID(v) == cf.CFDataGetTypeID():
        n = cf.CFDataGetLength(v); p = cf.CFDataGetBytePtr(v)
        print(key.decode(), base64.b64encode(bytes(p[:n])).decode())
PY
    fi
}

enc_hex() { # enc_hex KEY : ioreg dumps first, then the IOKit fallback
    h="$(prop_hex "$1" pe iopower full)"
    if [ -z "$h" ] && [ "$DRY_RUN" -eq 0 ]; then
        iokit_enc_dump
        b64="$(awk -v k="$1" '$1 == k { print $2; exit }' "$TMP/iokit")"
        [ -n "$b64" ] && h="$(printf '%s' "$b64" | b64_decode | bin2hex)"
    fi
    printf '%s' "$h"
}
SERIAL_ENC="$(enc_hex Gq3489ugfi)"
UUID_ENC="$(enc_hex Fyp98tpgj)"
DISK_ENC="$(enc_hex kbjfrfpoJU)"
ROM_ENC="$(enc_hex oycqAZloTNDm)"
MLB_ENC="$(enc_hex abKPld1EcMni)"

# CFNetwork version for the iCloud user agent (fallback: macOS 13.4 value).
CFNET=""
if [ "$DRY_RUN" -eq 0 ]; then
    CFNET="$(plutil -extract CFBundleVersion raw -o - /System/Library/Frameworks/CFNetwork.framework/Resources/Info.plist 2>/dev/null || true)"
    CFNET="$(clean "$CFNET")"
fi
[ -n "$CFNET" ] || CFNET="1408.0.4"
[ -n "$DARWIN" ] || DARWIN="22.5.0"
ICLOUD_UA="com.apple.iCloudHelper/282 CFNetwork/$CFNET Darwin/$DARWIN"
AOSKIT="com.apple.AOSKit/282 (com.apple.accountsd/113)"
PROTOCOL_VERSION=1640

if [ "$DRY_RUN" -eq 1 ]; then
    echo "+ plutil -extract CFBundleVersion raw -o - /System/Library/Frameworks/CFNetwork.framework/Resources/Info.plist" >&2
    echo "+ (Apple Silicon, only if NVRAM ROM is absent) shasum -a 256 <chosen/unique-chip-id bytes>" >&2
    echo "+ (only if ioreg omits the *_enc keys) osascript -l JavaScript: IORegistryEntryCreateCFProperty(IOPower:/, <key>)" >&2
    echo "+ plutil -lint -s $OUT.tmp.PID" >&2
    echo "mac-hwinfo: dry run; nothing read, nothing written (would write $OUT)" >&2
    exit 0
fi

# --- validate ---------------------------------------------------------------
missing=""
need() { [ -n "$2" ] || missing="$missing $1"; }
need product_name "$PRODUCT_NAME"
need io_mac_address "$MAC_HEX"
need platform_serial_number "$SERIAL"
need platform_uuid "$PLATFORM_UUID"
need root_disk_uuid "$ROOT_DISK_UUID"
need board_id "$BOARD_ID"
need os_build_num "$OS_BUILD"
need platform_serial_number_enc "$SERIAL_ENC"
need platform_uuid_enc "$UUID_ENC"
need root_disk_uuid_enc "$DISK_ENC"
need rom "$ROM_HEX"
need rom_enc "$ROM_ENC"
need mlb "$MLB"
need mlb_enc "$MLB_ENC"
need version "$OS_VERSION"
if [ -n "$missing" ]; then
    echo "mac-hwinfo: could not read:$missing" >&2
    echo "mac-hwinfo: refusing to write an incomplete identity (Apple rejects it with error 6001)." >&2
    exit 1
fi
if [ "${#MAC_HEX}" -ne 12 ]; then
    echo "mac-hwinfo: io_mac_address is ${#MAC_HEX} hex digits, expected 12" >&2
    exit 1
fi

# --- write ------------------------------------------------------------------
xml_escape() { printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'; }
kstr()  { printf '\t\t<key>%s</key>\n\t\t<string>%s</string>\n' "$1" "$(xml_escape "$2")"; }
kdata() { printf '\t\t<key>%s</key>\n\t\t<data>%s</data>\n' "$1" "$(hex2b64 "$2")"; }

TMP_OUT="$OUT.tmp.$$"
{
    printf '<?xml version="1.0" encoding="UTF-8"?>\n'
    printf '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    printf '<plist version="1.0">\n<dict>\n\t<key>inner</key>\n\t<dict>\n'
    kstr  product_name               "$PRODUCT_NAME"
    kdata io_mac_address             "$MAC_HEX"
    kstr  platform_serial_number     "$SERIAL"
    kstr  platform_uuid              "$PLATFORM_UUID"
    kstr  root_disk_uuid             "$ROOT_DISK_UUID"
    kstr  board_id                   "$BOARD_ID"
    kstr  os_build_num               "$OS_BUILD"
    kdata platform_serial_number_enc "$SERIAL_ENC"
    kdata platform_uuid_enc          "$UUID_ENC"
    kdata root_disk_uuid_enc         "$DISK_ENC"
    kdata rom                        "$ROM_HEX"
    kdata rom_enc                    "$ROM_ENC"
    kstr  mlb                        "$MLB"
    kdata mlb_enc                    "$MLB_ENC"
    printf '\t</dict>\n'
    printf '\t<key>version</key>\n\t<string>%s</string>\n' "$(xml_escape "$OS_VERSION")"
    printf '\t<key>protocol_version</key>\n\t<integer>%s</integer>\n' "$PROTOCOL_VERSION"
    printf '\t<key>device_id</key>\n\t<string>%s</string>\n' "$(xml_escape "$PLATFORM_UUID")"
    printf '\t<key>icloud_ua</key>\n\t<string>%s</string>\n' "$(xml_escape "$ICLOUD_UA")"
    printf '\t<key>aoskit_version</key>\n\t<string>%s</string>\n' "$(xml_escape "$AOSKIT")"
    printf '</dict>\n</plist>\n'
} > "$TMP_OUT"

if ! plutil -lint -s "$TMP_OUT" >/dev/null 2>&1; then
    rm -f "$TMP_OUT"
    echo "mac-hwinfo: generated plist failed plutil -lint (bug); nothing written" >&2
    exit 1
fi
mv -f "$TMP_OUT" "$OUT"
chmod 600 "$OUT"

# --- summary (masked) -------------------------------------------------------
mask() { # mask VALUE [KEEP]: all but the last KEEP (default 3) chars as '*'
    n=${#1}; k=${2:-3}
    if [ "$n" -le "$k" ]; then stars=$n; tail=""
    else stars=$((n - k)); tail="$(printf '%s' "$1" | cut -c$((n - k + 1))-)"; fi
    printf '%*s' "$stars" '' | tr ' ' '*'; printf '%s' "$tail"
}
bytes() { echo $(( ${#1} / 2 )); }

echo "mac-hwinfo: identity extracted ($([ "$ARM" -eq 1 ] && echo 'Apple Silicon' || echo 'Intel'))"
echo "  product_name               $PRODUCT_NAME"
echo "  board_id                   $BOARD_ID"
echo "  macOS                      $OS_VERSION ($OS_BUILD)"
echo "  platform_serial_number     $(mask "$SERIAL")"
echo "  platform_uuid              $(mask "$PLATFORM_UUID" 4)"
echo "  root_disk_uuid             $(mask "$ROOT_DISK_UUID" 4)"
echo "  io_mac_address             $(printf '%s' "$MAC_HEX" | cut -c1-6 | sed 's/../&:/g')**:**:**"
echo "  mlb                        $(mask "$MLB") (source: $MLB_SRC)"
echo "  rom                        $(bytes "$ROM_HEX") bytes (source: $ROM_SRC)"
echo "  *_enc                      $(bytes "$SERIAL_ENC")/$(bytes "$UUID_ENC")/$(bytes "$DISK_ENC")/$(bytes "$ROM_ENC")/$(bytes "$MLB_ENC") bytes"
echo
echo "wrote $OUT (mode 600). This file is a device credential; keep it private."
echo "next: copy it to the Linux machine and run: imsg hwconfig import hwconfig.plist"
