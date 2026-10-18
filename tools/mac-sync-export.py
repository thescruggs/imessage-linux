#!/usr/bin/env python3
"""Export contacts, pinned conversations and transcript backgrounds from a Mac as one tar stream.

Runs ON THE MAC (macOS' own /usr/bin/python3, 3.9+, standard library only)
and is normally fed over ssh by `imsg sync-mac`:

    ssh <mac> python3 - --contacts --pins < tools/mac-sync-export.py > sync.tar

Needs Full Disk Access for the ssh session (System Settings > Privacy &
Security) to read the Contacts database and chat.db. Everything is opened
read-only; nothing on the Mac changes.

Sections (each only when asked for; one failing does not stop the others):

  --contacts     the Contacts database (~/Library/Application Support/
                 AddressBook) as one vCard 3.0 per person, photo embedded
                 (JPEG, at most 256 px, shrunk with sips), named
                 contacts/mac-<uid>.vcf. Same output as the former
                 tools/mac-contacts-export.sh.
  --pins         the pinning preferences as XML (`defaults export
                 com.apple.messages.pinning`, else ~/Library/Preferences/
                 com.apple.messages.pinning.plist via `plutil -convert
                 xml1`), the pinned conversations in order (pD.pP: phone
                 numbers, email addresses, group chat ids). Pins that are
                 not an address are looked up in chat.db so the receiving
                 side can map them; if chat.db cannot be read that is
                 reported as pins.lookup_error.
  --backgrounds  the chats' transcript backgrounds, exactly as
                 tools/mac-backgrounds-export.py writes them.

Output (tar on stdout, nothing else; diagnostics go to stderr):

    sync.json                    {"format": 1, "exported_at": ..., "contacts": {...},
                                  "pins": {...}, "pin_chats": [...], "backgrounds": {...}}
                                 a section is null when not asked for, else
                                 its counters or {"error": "..."}
    contacts/mac-<uid>.vcf       one per person
    pins.plist                   the pinning preferences, XML
    backgrounds.jsonl            see mac-backgrounds-export.py
    backgrounds/<id>[-watchBackground]

The output is your address book: keep it private.
"""

import argparse
import base64
import glob
import io
import json
import os
import plistlib
import re
import shutil
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import time
import types
import urllib.parse

AB = os.path.expanduser("~/Library/Application Support/AddressBook")
PINNING = os.path.expanduser("~/Library/Preferences/com.apple.messages.pinning.plist")
PINNING_DOMAIN = "com.apple.messages.pinning"
CHAT_DB = os.path.expanduser("~/Library/Messages/chat.db")
PHOTO_MAX = 256


def parse_args(argv=None):
    # (no __doc__ when imsg put _BACKGROUNDS_SRC in front of it)
    p = argparse.ArgumentParser(description="Export contacts, pins and backgrounds from a Mac as one tar stream.")
    p.add_argument("--contacts", action="store_true", help="export the Contacts database as vCards")
    p.add_argument("--pins", action="store_true", help="export the pinned conversations")
    p.add_argument("--backgrounds", action="store_true", help="export the chats' transcript backgrounds")
    p.add_argument("--max-file-mb", type=float, default=32.0, help="backgrounds: skip cached files larger than this (default 32)")
    p.add_argument("--addressbook", default=AB, help=argparse.SUPPRESS)
    p.add_argument("--pinning", default=PINNING, help=argparse.SUPPRESS)
    p.add_argument("--db", default=CHAT_DB, help=argparse.SUPPRESS)
    p.add_argument("--cache", default=None, help=argparse.SUPPRESS)
    return p.parse_args(argv)


def add_bytes(tar, name, data):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mtime = int(time.time())
    info.mode = 0o600
    tar.addfile(info, io.BytesIO(data))


# ---------------------------------------------------------------------------
# contacts (ported unchanged from tools/mac-contacts-export.sh)
# ---------------------------------------------------------------------------

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


def vcards(ab=AB):
    """Yield (file name, vCard bytes, has photo) for every person in the
    Contacts database under `ab` (all sources, each person once)."""
    stores = sorted(glob.glob(os.path.join(ab, "Sources", "*", "AddressBook-v22.abcddb")))
    stores.append(os.path.join(ab, "AddressBook-v22.abcddb"))
    seen = set()
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
                lines.append("END:VCARD")
                data = "".join(fold(l) for l in lines).encode("utf-8")
                yield "mac-%s.vcf" % re.sub(r"[^A-Za-z0-9-]", "_", uid), data, bool(img)


def export_contacts(tar, ab):
    if not os.path.isdir(ab):
        raise OSError("no Contacts database at %s" % ab)
    written, photos = 0, 0
    for name, data, photo in vcards(ab):
        add_bytes(tar, "contacts/" + name, data)
        written += 1
        photos += photo
    return {"count": written, "photos": photos}


# ---------------------------------------------------------------------------
# pins
# ---------------------------------------------------------------------------

def pinning_xml(path):
    """The pinning preferences as an XML plist, or None if there are none:
    `defaults export` first (cfprefsd's current copy; the file on disk may
    not be flushed yet), only for the real preferences file; else
    `plutil -convert xml1` of the file; else plistlib (no plutil, e.g. in
    tests)."""
    if path == PINNING:
        try:
            out = subprocess.run(["defaults", "export", PINNING_DOMAIN, "-"], check=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout
            p = plistlib.loads(out) if out else None
            if isinstance(p, dict) and "pD" in p:
                return out
        except (OSError, subprocess.CalledProcessError, plistlib.InvalidFileException, ValueError):
            pass
    if not os.path.isfile(path):
        return None
    try:
        return subprocess.run(["plutil", "-convert", "xml1", "-o", "-", path], check=True,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout
    except (OSError, subprocess.CalledProcessError):
        pass
    with open(path, "rb") as f:
        return plistlib.dumps(plistlib.load(f), fmt=plistlib.FMT_XML)


def pin_list(xml):
    """pD.pP of the pinning plist: the pinned conversations in order."""
    p = plistlib.loads(xml)
    pd = p.get("pD") if isinstance(p, dict) else None
    pins = pd.get("pP") if isinstance(pd, dict) else None
    return [x for x in pins if isinstance(x, str)] if isinstance(pins, list) else []


def looks_like_address(s):
    s = s.strip()
    return "@" in s or bool(re.match(r"^\+?[0-9 ().-]+$", s))


def pin_chats(db_path, pins):
    """chat.db rows for pins that are not an address (group chat ids),
    with their participants, so they can be mapped like the history
    import does. Returns (rows, error): error says why chat.db could not
    be read (then rows is [] and the receiving side must not take an
    unmapped pin as "not pinned any more")."""
    wanted = [p for p in pins if not looks_like_address(p)]
    if not wanted:
        return [], None
    try:
        db = sqlite3.connect("file:%s?mode=ro" % urllib.parse.quote(db_path), uri=True)
        out = []
        for pin in wanted:
            row = db.execute(
                "SELECT ROWID, chat_identifier, style, display_name, group_id FROM chat "
                "WHERE group_id = ?1 COLLATE NOCASE OR chat_identifier = ?1 OR guid = ?1 "
                "ORDER BY (group_id = ?1 COLLATE NOCASE) DESC, ROWID DESC LIMIT 1", (pin,)).fetchone()
            if not row:
                continue
            rowid, ident, style, name, gid = row
            parts = [r[0] for r in db.execute(
                "SELECT h.id FROM chat_handle_join j JOIN handle h ON h.ROWID = j.handle_id WHERE j.chat_id = ? ORDER BY h.ROWID",
                (rowid,))]
            out.append({"pin": pin, "rowid": rowid, "chat_identifier": ident or "", "style": style or 0,
                        "display_name": name or None, "group_id": gid or None, "participants": parts})
        return out, None
    except sqlite3.Error as e:
        sys.stderr.write("mac-sync-export: cannot look pinned group chats up in %s: %s\n" % (db_path, e))
        return [], "%s: %s (give the ssh session Full Disk Access)" % (type(e).__name__, e)


def export_pins(tar, pinning, db_path):
    xml = pinning_xml(pinning)
    if xml is None:
        return {"count": 0, "file": None}, []
    pins = pin_list(xml)
    add_bytes(tar, "pins.plist", xml)
    rows, error = pin_chats(db_path, pins)
    meta = {"count": len(pins), "file": "pins.plist"}
    if error:
        meta["lookup_error"] = error
    return meta, rows


# ---------------------------------------------------------------------------
# backgrounds (mac-backgrounds-export.py, shipped alongside)
# ---------------------------------------------------------------------------

def backgrounds_module():
    """mac-backgrounds-export.py as a module: from `_BACKGROUNDS_SRC` (imsg
    prepends it when feeding this script over ssh), else from the file next
    to this one."""
    src = globals().get("_BACKGROUNDS_SRC")
    name = "mac-backgrounds-export.py"
    if src is None:
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), name)
        with open(path, encoding="utf-8") as f:
            src = f.read()
    mod = types.ModuleType("mac_backgrounds_export")
    exec(compile(src, name, "exec"), mod.__dict__)
    return mod


def export_backgrounds(tar, args):
    mbe = backgrounds_module()
    bargs = types.SimpleNamespace(max_file_mb=args.max_file_mb, db=args.db, cache=args.cache or mbe.CACHE)
    return mbe.export(bargs, None, tar=tar)


# ---------------------------------------------------------------------------

def section(meta, key, fn):
    try:
        return fn()
    except Exception as e:  # reported, the other sections go on
        msg = "%s: %s" % (type(e).__name__, e)
        if isinstance(e, (sqlite3.Error, PermissionError)):
            msg += " (give the ssh session Full Disk Access: System Settings > Privacy & Security)"
        sys.stderr.write("mac-sync-export: %s: %s\n" % (key, msg))
        meta[key] = {"error": msg}
        return None


def export(args, out_stream):
    meta = {"format": 1, "exported_at": int(time.time()), "contacts": None, "pins": None, "pin_chats": [], "backgrounds": None}
    tar = tarfile.open(fileobj=out_stream, mode="w|", format=tarfile.PAX_FORMAT)
    if args.contacts:
        r = section(meta, "contacts", lambda: export_contacts(tar, args.addressbook))
        if r is not None:
            meta["contacts"] = r
    if args.pins:
        r = section(meta, "pins", lambda: export_pins(tar, args.pinning, args.db))
        if r is not None:
            meta["pins"], meta["pin_chats"] = r
    if args.backgrounds:
        r = section(meta, "backgrounds", lambda: export_backgrounds(tar, args))
        if r is not None:
            meta["backgrounds"] = r
    add_bytes(tar, "sync.json", json.dumps(meta, ensure_ascii=False).encode("utf-8"))
    tar.close()
    out_stream.flush()
    return meta


def main():
    args = parse_args()
    if not (args.contacts or args.pins or args.backgrounds):
        args.contacts = args.pins = True
    try:
        meta = export(args, sys.stdout.buffer)
    except BrokenPipeError:
        return 1
    done = []
    if meta["contacts"] and "count" in meta["contacts"]:
        done.append("%d contacts (%d with photos)" % (meta["contacts"]["count"], meta["contacts"]["photos"]))
    if meta["pins"] and "count" in meta["pins"]:
        done.append("%d pins" % meta["pins"]["count"])
    if meta["backgrounds"] and "files" in meta["backgrounds"]:
        done.append("%d chats with a background" % meta["backgrounds"]["chats_with_background"])
    sys.stderr.write("mac-sync-export: %s\n" % (", ".join(done) or "nothing exported"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
