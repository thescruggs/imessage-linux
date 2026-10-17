#!/usr/bin/env python3
"""Export recent Messages history from a Mac's chat.db as a tar stream.

Runs ON THE MAC (macOS' own /usr/bin/python3, 3.9+, standard library only)
and is normally fed over ssh by `imsg import-mac`:

    ssh <mac> python3 - --days 10 < tools/mac-messages-export.py > export.tar

The ssh session needs Full Disk Access on the Mac (System Settings > Privacy
& Security > Full Disk Access > sshd-keygen-wrapper / Terminal) to read
~/Library/Messages. chat.db is opened read-only; nothing on the Mac changes.

Output (tar on stdout, nothing else; diagnostics go to stderr):

    export.jsonl                      one JSON object per line:
        {"type": "meta", ...}         first line: run parameters and counters
        {"type": "chat", ...}         every chat that has a message in range
        {"type": "message", ...}      messages in range, oldest first
    attachments/<guid>/<file name>    attachment files referenced by messages

Records stay close to chat.db (raw Apple dates, raw handle ids, raw
associated_message_type); `imsg` maps them to the daemon's model. Message
text comes from `message.text`, or, when that is NULL (most rows on recent
macOS), from the NSString inside the `attributedBody` typedstream.
"""

import argparse
import io
import json
import os
import sqlite3
import sys
import tarfile
import time
import urllib.parse

APPLE_EPOCH = 978307200  # 2001-01-01T00:00:00Z in Unix seconds
DB = os.path.expanduser("~/Library/Messages/chat.db")


def parse_args():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = p.add_mutually_exclusive_group()
    g.add_argument("--days", type=float, default=10.0, help="export messages newer than this many days (default 10)")
    g.add_argument("--since", type=float, help="export messages at or after this Unix time (seconds)")
    p.add_argument("--max-attachment-mb", type=float, default=50.0, help="skip attachment files larger than this (default 50)")
    p.add_argument("--no-files", action="store_true", help="metadata only: do not include attachment files")
    p.add_argument("--db", default=DB, help=argparse.SUPPRESS)
    return p.parse_args()


def apple_ns(unix_seconds):
    """Unix seconds -> chat.db date (nanoseconds since 2001-01-01)."""
    return int((unix_seconds - APPLE_EPOCH) * 1_000_000_000)


# ---------------------------------------------------------------------------
# attributedBody (NSArchiver typedstream) -> plain text
# ---------------------------------------------------------------------------

# How far after the class name the string's type tag may sit.
ATTRIBUTED_BODY_WINDOW = 48


def decode_attributed_body(blob):
    """The NSString payload of an attributedBody typedstream, or None.

    Layout after the class name: ... b"NSString" <class version, possibly
    the superclass chain, e.g. b"\x01\x84\x84\x08NSObject\x00\x85" when the
    string is an NSMutableString> b"\x84\x01+" (a new type string "+",
    "C string follows") <length> <UTF-8 bytes>. The length is one byte if
    < 0x80, else 0x81 + int16 LE, 0x82 + int32 LE. The type tag is looked
    for within ATTRIBUTED_BODY_WINDOW bytes of the class name; a bare '+'
    right after it (within 12 bytes) is accepted too.
    """
    if not blob:
        return None
    data = bytes(blob)
    for marker in (b"NSString", b"NSMutableString"):
        i = data.find(marker)
        if i < 0:
            continue
        start = i + len(marker)
        j = data.find(b"\x84\x01+", start, start + ATTRIBUTED_BODY_WINDOW)
        if j >= 0:
            j += 2
        else:
            j = data.find(b"\x2b", start, start + 12)
        if j < 0:
            continue
        k = j + 1
        if k >= len(data):
            return None
        n = data[k]
        if n == 0x81:
            if k + 3 > len(data):
                return None
            length = int.from_bytes(data[k + 1:k + 3], "little")
            k += 3
        elif n == 0x82:
            if k + 5 > len(data):
                return None
            length = int.from_bytes(data[k + 1:k + 5], "little")
            k += 5
        elif n < 0x80:
            length = n
            k += 1
        else:
            return None
        raw = data[k:k + length]
        if len(raw) != length:
            return None
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError:
            return None
    return None


# ---------------------------------------------------------------------------
# queries
# ---------------------------------------------------------------------------

def columns(db, table):
    return {r[1] for r in db.execute("PRAGMA table_info(%s)" % table)}


def opt(cols, name, alias=None):
    """`m.<name>` if the column exists on this macOS, else NULL."""
    return ("m.%s" % name if name in cols else "NULL") + " AS " + (alias or name)


def export(args):
    db = sqlite3.connect("file:%s?mode=ro" % urllib.parse.quote(args.db), uri=True)
    db.row_factory = sqlite3.Row
    mcols = columns(db, "message")
    since_unix = args.since if args.since is not None else time.time() - args.days * 86400
    since = apple_ns(since_unix)
    # Very old rows store seconds instead of nanoseconds; a cutoff this
    # recent is always in nanoseconds, and such rows are far older anyway.
    rows = db.execute(
        """
        SELECT m.ROWID AS rowid, m.guid, m.text, m.attributedBody, m.is_from_me, m.date, m.date_delivered, m.date_read,
               m.subject, m.service, m.item_type, m.group_action_type, m.associated_message_guid, m.associated_message_type,
               m.balloon_bundle_id, m.expressive_send_style_id, m.is_sent, m.is_delivered, m.error,
               m.destination_caller_id, m.cache_has_attachments, h.id AS handle,
               %s, %s, %s, %s
          FROM message m LEFT JOIN handle h ON h.ROWID = m.handle_id
         WHERE m.date >= ?
         ORDER BY m.date, m.ROWID
        """
        % (
            opt(mcols, "thread_originator_guid"),
            opt(mcols, "date_edited"),
            opt(mcols, "date_retracted"),
            opt(mcols, "associated_message_emoji"),
        ),
        (since,),
    ).fetchall()

    chat_of = {}
    for r in db.execute(
        "SELECT j.message_id, j.chat_id FROM chat_message_join j JOIN message m ON m.ROWID = j.message_id WHERE m.date >= ?", (since,)
    ):
        chat_of.setdefault(r[0], []).append(r[1])

    atts_of = {}
    for r in db.execute(
        """SELECT j.message_id, a.ROWID AS rowid, a.guid, a.filename, a.mime_type, a.uti, a.transfer_name, a.total_bytes,
                  a.is_sticker, a.hide_attachment
             FROM message_attachment_join j JOIN attachment a ON a.ROWID = j.attachment_id JOIN message m ON m.ROWID = j.message_id
            WHERE m.date >= ? ORDER BY j.message_id, a.ROWID""",
        (since,),
    ):
        atts_of.setdefault(r["message_id"], []).append(r)

    chat_ids = sorted({c for v in chat_of.values() for c in v})
    chats = []
    for cid in chat_ids:
        c = db.execute(
            "SELECT ROWID AS rowid, guid, chat_identifier, style, service_name, display_name, group_id FROM chat WHERE ROWID = ?", (cid,)
        ).fetchone()
        if c is None:
            continue
        participants = [
            r[0]
            for r in db.execute(
                "SELECT h.id FROM chat_handle_join j JOIN handle h ON h.ROWID = j.handle_id WHERE j.chat_id = ? ORDER BY h.ROWID", (cid,)
            )
        ]
        chats.append(
            {
                "type": "chat",
                "rowid": c["rowid"],
                "guid": c["guid"],
                "chat_identifier": c["chat_identifier"],
                "style": c["style"],
                "service": c["service_name"],
                "display_name": c["display_name"] or None,
                "group_id": c["group_id"] or None,
                "participants": participants,
            }
        )

    max_bytes = int(args.max_attachment_mb * 1024 * 1024)
    counters = {"text_decode_failures": 0, "attachments_missing": 0, "attachments_too_large": 0, "attachments_included": 0}
    files = []  # (arcname, path)
    messages = []
    for r in rows:
        text, source = r["text"], "text"
        if text is None and r["attributedBody"] is not None:
            text, source = decode_attributed_body(r["attributedBody"]), "attributed_body"
            if text is None:
                counters["text_decode_failures"] += 1
                source = "decode_failed"
        elif text is None:
            source = "none"
        attachments = []
        for a in atts_of.get(r["rowid"], []):
            rec = {
                "guid": a["guid"],
                "mime": a["mime_type"],
                "uti": a["uti"],
                "transfer_name": a["transfer_name"],
                "total_bytes": a["total_bytes"] or 0,
                "is_sticker": bool(a["is_sticker"]),
                "hidden": bool(a["hide_attachment"]),
                "path": None,
                "missing": None,
            }
            path = os.path.expanduser(a["filename"]) if a["filename"] else None
            if a["hide_attachment"]:
                # Link-preview payloads and the like: not shown as attachments.
                rec["missing"] = "hidden"
            elif args.no_files:
                rec["missing"] = "not_requested"
            elif not path or not os.path.isfile(path):
                rec["missing"] = "not_downloaded"
                counters["attachments_missing"] += 1
            else:
                size = os.path.getsize(path)
                rec["total_bytes"] = size
                if size > max_bytes:
                    rec["missing"] = "too_large"
                    counters["attachments_too_large"] += 1
                else:
                    name = os.path.basename(path).replace("/", "_") or "attachment"
                    guid = a["guid"].replace("/", "_")
                    arc = "attachments/%s/%s" % (guid, name)
                    rec["path"] = arc
                    files.append((arc, path))
                    counters["attachments_included"] += 1
            attachments.append(rec)
        messages.append(
            {
                "type": "message",
                "rowid": r["rowid"],
                "guid": r["guid"],
                "chats": chat_of.get(r["rowid"], []),
                "handle": r["handle"],
                "is_from_me": bool(r["is_from_me"]),
                "date": r["date"],
                "date_delivered": r["date_delivered"] or 0,
                "date_read": r["date_read"] or 0,
                "date_edited": r["date_edited"] or 0,
                "date_retracted": r["date_retracted"] or 0,
                "text": text,
                "text_source": source,
                "subject": r["subject"],
                "service": r["service"],
                "item_type": r["item_type"] or 0,
                "group_action_type": r["group_action_type"] or 0,
                "associated_guid": r["associated_message_guid"],
                "associated_type": r["associated_message_type"] or 0,
                "associated_emoji": r["associated_message_emoji"],
                "thread_originator_guid": r["thread_originator_guid"],
                "balloon": r["balloon_bundle_id"],
                "effect": r["expressive_send_style_id"],
                "is_sent": bool(r["is_sent"]),
                "is_delivered": bool(r["is_delivered"]),
                "error": r["error"] or 0,
                "destination_caller_id": r["destination_caller_id"],
                "attachments": attachments,
            }
        )

    meta = {
        "type": "meta",
        "format": 1,
        "exported_at": int(time.time()),
        "since_unix": int(since_unix),
        "messages": len(messages),
        "chats": len(chats),
        "max_attachment_bytes": max_bytes,
    }
    meta.update(counters)
    lines = [meta] + chats + messages
    body = "".join(json.dumps(x, ensure_ascii=False, separators=(",", ":")) + "\n" for x in lines).encode("utf-8")

    out = tarfile.open(fileobj=sys.stdout.buffer, mode="w|", format=tarfile.PAX_FORMAT)
    info = tarfile.TarInfo("export.jsonl")
    info.size = len(body)
    info.mtime = int(time.time())
    info.mode = 0o600
    out.addfile(info, io.BytesIO(body))
    for arc, path in files:
        try:
            st = os.stat(path)
            info = tarfile.TarInfo(arc)
            info.size = st.st_size
            info.mtime = int(st.st_mtime)
            info.mode = 0o600
            with open(path, "rb") as f:
                out.addfile(info, f)
        except OSError as e:
            sys.stderr.write("mac-messages-export: skipping %s: %s\n" % (arc, e))
    out.close()
    sys.stdout.buffer.flush()
    sys.stderr.write(
        "mac-messages-export: %d chats, %d messages, %d attachment files, %d text decode failures\n"
        % (len(chats), len(messages), counters["attachments_included"], counters["text_decode_failures"])
    )


def main():
    args = parse_args()
    try:
        export(args)
    except sqlite3.OperationalError as e:
        sys.stderr.write(
            "mac-messages-export: cannot read %s: %s\n(give the ssh session Full Disk Access: System Settings > Privacy & Security)\n" % (args.db, e)
        )
        return 1
    except BrokenPipeError:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
