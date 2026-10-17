#!/bin/bash
# mac-contacts-export.sh: copy your Mac's Contacts to this Linux machine as
# vCards, so kmessages shows names and photos instead of phone numbers.
#
# Runs FROM LINUX. One ssh connection reads the Contacts database on the Mac
# (read-only, no Contacts.app, no privacy prompt) and streams back one
# vCard 3.0 per person, with the photo embedded (JPEG, at most 256 px),
# as a tar archive. The files land in the folder KPeople reads vCards from;
# kmessages (and any other KPeople user) picks changes up while running.
#
# Usage: tools/mac-contacts-export.sh [--dry-run] HOST [OUTDIR]
#   HOST     ssh destination of the Mac (e.g. mac.local or me@mac.local)
#   OUTDIR   default: ${XDG_DATA_HOME:-~/.local/share}/kpeoplevcard
#   --dry-run  list what would be written; change nothing
#
# Re-running replaces the previous export: files named mac-*.vcf in OUTDIR
# are removed first; other vCards there are left alone. Run it again
# whenever your contacts change.
#
# Needs on the Mac: Remote Login (ssh) and python3 (/usr/bin/python3 from the
# Command Line Tools); the ssh session needs Full Disk Access (System
# Settings > Privacy & Security), as for reading chat.db. sips (stock macOS)
# shrinks photos. The output is your address book: keep it private.

set -eu
set -o pipefail
umask 077

DRY_RUN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*) echo "mac-contacts-export: unknown option: $1" >&2; exit 64 ;;
        *) break ;;
    esac
done
[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: $0 [--dry-run] HOST [OUTDIR]" >&2; exit 64; }
HOST="$1"
OUTDIR="${2:-${XDG_DATA_HOME:-$HOME/.local/share}/kpeoplevcard}"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/mac-contacts.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# --- runs on the Mac: AddressBook Core Data stores -> tar of vCards on stdout
read -r -d '' REMOTE_PY <<'PYEOF' || true
import glob, io, os, re, shutil, sqlite3, subprocess, sys, tarfile, tempfile, base64

AB = os.path.expanduser("~/Library/Application Support/AddressBook")
stores = sorted(glob.glob(os.path.join(AB, "Sources", "*", "AddressBook-v22.abcddb")))
stores.append(os.path.join(AB, "AddressBook-v22.abcddb"))
PHOTO_MAX = 256

def esc(s):
    return (s.replace("\\", "\\\\").replace(";", "\\;").replace(",", "\\,")
             .replace("\r\n", "\\n").replace("\n", "\\n"))

def fold(line):
    # RFC 2425: lines of at most 75 octets, continued with a leading space.
    raw = line.encode("utf-8")
    if len(raw) <= 75:
        return line + "\r\n"
    out, cur = [], b""
    for ch in line:
        b = ch.encode("utf-8")
        if len(cur) + len(b) > (75 if not out else 74):
            out.append(cur)
            cur = b""
        cur += b
    out.append(cur)
    return "\r\n ".join(x.decode("utf-8") for x in out) + "\r\n"

def label_type(label):
    l = (label or "").lower()
    for key, t in (("iphone", "CELL"), ("mobile", "CELL"), ("cell", "CELL"), ("home", "HOME"),
                   ("work", "WORK"), ("main", "MAIN"), ("fax", "FAX"), ("pager", "PAGER")):
        if key in l:
            return t
    return "OTHER" if l else ""

def blob(data, support):
    # Core Data "allows external storage": 0x01 + bytes inline, 0x02 + UUID of a file.
    if not data:
        return None
    data = bytes(data)
    if data[0] == 1:
        return data[1:]
    if data[0] == 2:
        name = data[1:].split(b"\0")[0].decode("ascii", "replace")
        path = os.path.join(support, "_EXTERNAL_DATA", name)
        if os.path.isfile(path):
            with open(path, "rb") as f:
                return f.read()
        return None
    return data

def shrink(img, tmp):
    src, dst = os.path.join(tmp, "in"), os.path.join(tmp, "out.jpg")
    with open(src, "wb") as f:
        f.write(img)
    try:
        subprocess.run(["sips", "-Z", str(PHOTO_MAX), "-s", "format", "jpeg", "-s", "formatOptions", "85",
                        src, "--out", dst], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        with open(dst, "rb") as f:
            return f.read()
    except Exception:
        return img if img[:2] == b"\xff\xd8" else None

def connect(path, tmp):
    try:
        db = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
        db.execute("select count(*) from ZABCDRECORD").fetchone()
        return db
    except sqlite3.Error:
        # Fall back to a private copy (database + WAL) if read-only open fails.
        copy = os.path.join(tmp, "copy-%d.abcddb" % len(os.listdir(tmp)))
        for ext in ("", "-wal", "-shm"):
            if os.path.exists(path + ext):
                shutil.copyfile(path + ext, copy + ext)
        return sqlite3.connect(copy)

tar = tarfile.open(fileobj=sys.stdout.buffer, mode="w|")
seen, written, photos = set(), 0, 0
with tempfile.TemporaryDirectory() as tmp:
    for store in stores:
        if not os.path.isfile(store):
            continue
        support = os.path.join(os.path.dirname(store), ".AddressBook-v22_SUPPORT")
        db = connect(store, tmp)
        ent = db.execute("select Z_ENT from Z_PRIMARYKEY where Z_NAME = 'ABCDContact'").fetchone()
        if not ent:
            continue
        emails, phones = {}, {}
        for owner, addr, label in db.execute(
                "select ZOWNER, ZADDRESS, ZLABEL from ZABCDEMAILADDRESS where ZADDRESS is not null "
                "order by ZOWNER, ZISPRIMARY desc, ZORDERINGINDEX"):
            emails.setdefault(owner, []).append((addr.strip(), label))
        for owner, num, label in db.execute(
                "select ZOWNER, ZFULLNUMBER, ZLABEL from ZABCDPHONENUMBER where ZFULLNUMBER is not null "
                "order by ZOWNER, ZISPRIMARY desc, ZORDERINGINDEX"):
            phones.setdefault(owner, []).append((num.strip(), label))
        rows = db.execute(
            "select Z_PK, ZUNIQUEID, ZFIRSTNAME, ZMIDDLENAME, ZLASTNAME, ZTITLE, ZSUFFIX, ZORGANIZATION, "
            "ZNICKNAME, ZDISPLAYFLAGS, ZTHUMBNAILIMAGEDATA, ZIMAGEDATA from ZABCDRECORD where Z_ENT = ?", ent)
        for (pk, uid, first, middle, last, title, suffix, org, nick, flags, thumb, image) in rows:
            uid = uid or "pk%d" % pk
            if uid in seen:
                continue
            seen.add(uid)
            em, ph = emails.get(pk, []), phones.get(pk, [])
            person = " ".join(x.strip() for x in (title, first, middle, last, suffix) if x and x.strip())
            show_company = bool((flags or 0) & 1) and org
            fn = (org if show_company else person) or org or nick or (em[0][0] if em else "") or (ph[0][0] if ph else "")
            if not fn:
                continue
            lines = ["BEGIN:VCARD", "VERSION:3.0", "PRODID:-//imessage-linux//mac-contacts-export//EN",
                     "UID:" + esc(uid), "FN:" + esc(fn.strip()),
                     "N:" + ";".join(esc((x or "").strip()) for x in (last, first, middle, title, suffix))]
            if nick:
                lines.append("NICKNAME:" + esc(nick.strip()))
            if org:
                lines.append("ORG:" + esc(org.strip()))
            if show_company:
                lines.append("X-ABShowAs:COMPANY")
            for addr, label in em:
                t = label_type(label)
                lines.append("EMAIL;TYPE=INTERNET%s:%s" % ("," + t if t in ("HOME", "WORK") else "", esc(addr)))
            for num, label in ph:
                t = label_type(label)
                lines.append("TEL%s:%s" % (";TYPE=" + t if t else "", esc(num)))
            img = blob(thumb, support) or blob(image, support)
            if img:
                img = shrink(img, tmp)
            if img:
                lines.append("PHOTO;ENCODING=b;TYPE=JPEG:" + base64.b64encode(img).decode("ascii"))
                photos += 1
            lines.append("END:VCARD")
            data = "".join(fold(l) for l in lines).encode("utf-8")
            info = tarfile.TarInfo("mac-%s.vcf" % re.sub(r"[^A-Za-z0-9-]", "_", uid))
            info.size = len(data)
            info.mode = 0o600
            tar.addfile(info, io.BytesIO(data))
            written += 1
tar.close()
sys.stdout.buffer.flush()
sys.stderr.write("mac-contacts-export: %d contacts, %d with photos\n" % (written, photos))
PYEOF

echo "mac-contacts-export: reading contacts on $HOST …" >&2
mkdir -p "$WORK/new"
status=0
printf '%s\n' "$REMOTE_PY" | ssh -F "$HOME/.ssh/config" -o BatchMode=yes "$HOST" '/usr/bin/python3 -' \
    | tar -x --no-same-owner -C "$WORK/new" -f - || status=$?

# Regular files only: a symlink member never reaches OUTDIR.
count=$(find "$WORK/new" -maxdepth 1 -type f -name 'mac-*.vcf' | wc -l)
if [ "$count" -eq 0 ]; then
    echo "mac-contacts-export: no contacts received (exit status $status); leaving $OUTDIR unchanged" >&2
    exit 1
fi
if [ "$status" -ne 0 ]; then
    echo "mac-contacts-export: the transfer failed (exit status $status); leaving $OUTDIR unchanged" >&2
    exit 1
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "mac-contacts-export: would replace $(find "$OUTDIR" -maxdepth 1 -name 'mac-*.vcf' 2>/dev/null | wc -l) old files in $OUTDIR with $count:" >&2
    find "$WORK/new" -maxdepth 1 -type f -name 'mac-*.vcf' -printf '%f\n' | sort
    exit 0
fi

mkdir -p "$OUTDIR"
find "$OUTDIR" -maxdepth 1 -name 'mac-*.vcf' -type f -delete
find "$WORK/new" -maxdepth 1 -type f -name 'mac-*.vcf' -exec mv -t "$OUTDIR"/ -- {} +
echo "mac-contacts-export: wrote $count vCards to $OUTDIR" >&2
