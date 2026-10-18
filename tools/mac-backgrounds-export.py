#!/usr/bin/env python3
"""Export the chats' transcript backgrounds from a Mac as a tar stream.

Runs ON THE MAC (macOS' own /usr/bin/python3, 3.9+, standard library only)
and is normally fed over ssh by `imsg import-mac --backgrounds`:

    ssh <mac> python3 - < tools/mac-backgrounds-export.py > backgrounds.tar

Needs Full Disk Access for the ssh session, like mac-messages-export.py.
chat.db is opened read-only; nothing on the Mac changes.

For every chat whose `properties` plist has `backgroundProperties`, only the
non-secret fields are read: the background id (`trabaid`), payload version
(`trabapv`), announced size (`trabafs`), `trabav` and `refreshDate`. The
MMCS download reference next to them (`trabak` key, `trabas` signature,
`trabar` URL, `traboid`) is never read into the output, logged or written:
the payload already sits decrypted in the Mac's cache.

Output (tar on stdout, nothing else; diagnostics go to stderr):

    backgrounds.jsonl                    one JSON object per line:
        {"type": "meta", ...}            first line: counters
        {"type": "chat", ...}            every chat with a background
    backgrounds/<id>                     ~/Library/Messages/TranscriptBackgroundCache/<id>
                                         (PosterKit payload: Apple Archive or zip)
    backgrounds/<id>-watchBackground     its pre-rendered sibling (binary plist), if any
"""

import argparse
import datetime
import io
import json
import os
import plistlib
import re
import sqlite3
import stat
import sys
import tarfile
import time
import urllib.parse

DB = os.path.expanduser("~/Library/Messages/chat.db")
CACHE = os.path.expanduser("~/Library/Messages/TranscriptBackgroundCache")
# A background id is a UUID; anything else is not used as a file name.
ID_RE = re.compile(r"^[0-9A-Za-z][0-9A-Za-z-]{0,63}$")
WATCH_SUFFIX = "-watchBackground"


def parse_args(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--max-file-mb", type=float, default=32.0, help="skip cached files larger than this (default 32, the daemon's limit)")
    p.add_argument("--db", default=DB, help=argparse.SUPPRESS)
    p.add_argument("--cache", default=CACHE, help=argparse.SUPPRESS)
    return p.parse_args(argv)


def _int(v):
    return v if isinstance(v, int) and not isinstance(v, bool) else None


def background_fields(properties):
    """The non-secret background fields of a chat's `properties` blob, or
    None if it has no background (or is not a plist).

    Returns {"id", "version", "size", "av", "refreshed"} (refreshed: Unix
    seconds or None); "id" is None when the id is missing or unusable as a
    file name. Nothing else from the plist is returned.
    """
    if not properties:
        return None
    try:
        p = plistlib.loads(bytes(properties))
    except Exception:
        return None
    bp = p.get("backgroundProperties") if isinstance(p, dict) else None
    if not isinstance(bp, dict):
        return None
    bid = bp.get("trabaid")
    bid = bid.strip() if isinstance(bid, str) else None
    if not bid or not ID_RE.match(bid):
        bid = None
    refreshed = bp.get("refreshDate")
    if isinstance(refreshed, datetime.datetime):
        # plistlib returns naive UTC datetimes
        refreshed = int(refreshed.replace(tzinfo=datetime.timezone.utc).timestamp())
    else:
        refreshed = None
    return {
        "id": bid,
        "version": _int(bp.get("trabapv")),
        "size": _int(bp.get("trabafs")),
        "av": _int(bp.get("trabav")),
        "refreshed": refreshed,
    }


def cached_file(cache, name, max_bytes):
    """(path, None) for a regular file in `cache` no larger than max_bytes,
    else (None, reason)."""
    path = os.path.join(cache, name)
    try:
        st = os.lstat(path)
    except OSError:
        return None, "not_cached"
    if not stat.S_ISREG(st.st_mode):
        return None, "not_cached"
    if st.st_size > max_bytes:
        return None, "too_large"
    return path, None


def export(args, out_stream, tar=None):
    """Write the tar to `out_stream`, or add the members to the open `tar`
    (mac-sync-export.py: one tar for several exports) and leave it open."""
    db = sqlite3.connect("file:%s?mode=ro" % urllib.parse.quote(args.db), uri=True)
    max_bytes = int(args.max_file_mb * 1024 * 1024)
    chats = []
    files = {}  # arcname -> path
    counters = {"chats_with_background": 0, "files": 0, "not_cached": 0, "too_large": 0, "bad_id": 0}
    rows = db.execute(
        "SELECT ROWID, chat_identifier, style, service_name, display_name, group_id, properties FROM chat WHERE properties IS NOT NULL ORDER BY ROWID"
    ).fetchall()
    for rowid, ident, style, service, display_name, group_id, props in rows:
        bg = background_fields(props)
        if bg is None:
            continue
        counters["chats_with_background"] += 1
        participants = [
            r[0]
            for r in db.execute(
                "SELECT h.id FROM chat_handle_join j JOIN handle h ON h.ROWID = j.handle_id WHERE j.chat_id = ? ORDER BY h.ROWID", (rowid,)
            )
        ]
        rec = {
            "type": "chat",
            "rowid": rowid,
            "chat_identifier": ident or "",
            "style": style or 0,
            "service": service,
            "display_name": display_name or None,
            "group_id": group_id or None,
            "participants": participants,
            "background": bg,
            "poster": None,
            "watch": None,
            "missing": None,
        }
        if bg["id"] is None:
            rec["missing"] = "bad_id"
            counters["bad_id"] += 1
        else:
            for key, name in (("poster", bg["id"]), ("watch", bg["id"] + WATCH_SUFFIX)):
                path, why = cached_file(args.cache, name, max_bytes)
                if path is not None:
                    arc = "backgrounds/" + name
                    rec[key] = arc
                    files[arc] = path
                elif key == "poster":
                    rec["missing"] = why
            if rec["poster"] is None and rec["watch"] is None:
                counters[rec["missing"] or "not_cached"] += 1
            elif rec["poster"] is None:
                rec["missing"] = None
        chats.append(rec)

    counters["files"] = len(files)
    meta = {"type": "meta", "format": 1, "exported_at": int(time.time())}
    meta.update(counters)
    body = "".join(json.dumps(x, ensure_ascii=False, separators=(",", ":")) + "\n" for x in [meta] + chats).encode("utf-8")
    out = tar if tar is not None else tarfile.open(fileobj=out_stream, mode="w|", format=tarfile.PAX_FORMAT)
    info = tarfile.TarInfo("backgrounds.jsonl")
    info.size = len(body)
    info.mtime = int(time.time())
    info.mode = 0o600
    out.addfile(info, io.BytesIO(body))
    for arc, path in sorted(files.items()):
        try:
            with open(path, "rb") as f:
                st = os.fstat(f.fileno())
                info = tarfile.TarInfo(arc)
                info.size = st.st_size
                info.mtime = int(st.st_mtime)
                info.mode = 0o600
                out.addfile(info, f)
        except OSError as e:
            sys.stderr.write("mac-backgrounds-export: skipping %s: %s\n" % (arc, e))
    if tar is None:
        out.close()
        out_stream.flush()
    return counters


def main():
    args = parse_args()
    try:
        c = export(args, sys.stdout.buffer)
    except sqlite3.OperationalError as e:
        sys.stderr.write(
            "mac-backgrounds-export: cannot read %s: %s\n(give the ssh session Full Disk Access: System Settings > Privacy & Security)\n" % (args.db, e)
        )
        return 1
    except BrokenPipeError:
        return 1
    sys.stderr.write("mac-backgrounds-export: %d chats with a background, %d files\n" % (c["chats_with_background"], c["files"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
