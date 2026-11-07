#!/usr/bin/env python3
"""Export pinned conversations and transcript backgrounds from a Mac as one tar stream.

Runs ON THE MAC (macOS' own /usr/bin/python3, 3.9+, standard library only)
and is normally fed over ssh by `imsg sync-mac`:

    ssh <mac> python3 - --pins < tools/mac-sync-export.py > sync.tar

Needs Full Disk Access for the ssh session (System Settings > Privacy &
Security) to read chat.db. Everything is opened read-only; nothing on the
Mac changes. (Contacts are not exported here: the daemon syncs them from
iCloud itself, see `imsg contacts login`.)

Sections (each only when asked for; one failing does not stop the others;
with none asked for, --pins):

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

    sync.json                    {"format": 1, "exported_at": ...,
                                  "pins": {...}, "pin_chats": [...], "backgrounds": {...}}
                                 a section is null when not asked for, else
                                 its counters or {"error": "..."}
    pins.plist                   the pinning preferences, XML
    backgrounds.jsonl            see mac-backgrounds-export.py
    backgrounds/<id>[-watchBackground]
"""

import argparse
import io
import json
import os
import plistlib
import re
import sqlite3
import subprocess
import sys
import tarfile
import time
import types
import urllib.parse

PINNING = os.path.expanduser("~/Library/Preferences/com.apple.messages.pinning.plist")
PINNING_DOMAIN = "com.apple.messages.pinning"
CHAT_DB = os.path.expanduser("~/Library/Messages/chat.db")


def parse_args(argv=None):
    # (no __doc__ when imsg put _BACKGROUNDS_SRC in front of it)
    p = argparse.ArgumentParser(description="Export pins and backgrounds from a Mac as one tar stream.")
    p.add_argument("--pins", action="store_true", help="export the pinned conversations")
    p.add_argument("--backgrounds", action="store_true", help="export the chats' transcript backgrounds")
    p.add_argument("--max-file-mb", type=float, default=32.0, help="backgrounds: skip cached files larger than this (default 32)")
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
    meta = {"format": 1, "exported_at": int(time.time()), "pins": None, "pin_chats": [], "backgrounds": None}
    tar = tarfile.open(fileobj=out_stream, mode="w|", format=tarfile.PAX_FORMAT)
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
    if not (args.pins or args.backgrounds):
        args.pins = True
    try:
        meta = export(args, sys.stdout.buffer)
    except BrokenPipeError:
        return 1
    done = []
    if meta["pins"] and "count" in meta["pins"]:
        done.append("%d pins" % meta["pins"]["count"])
    if meta["backgrounds"] and "files" in meta["backgrounds"]:
        done.append("%d chats with a background" % meta["backgrounds"]["chats_with_background"])
    sys.stderr.write("mac-sync-export: %s\n" % (", ".join(done) or "nothing exported"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
