#!/usr/bin/env python3
"""Unit tests for mac-messages-export.py (standard library only).

    python3 -I tools/test_mac_messages_export.py [path/to/mac-messages-export.py]

Also run by `cargo test -p imsg` (skipped if python3 is missing).
"""

import importlib.util
import os
import sys
import unittest

sys.dont_write_bytecode = True
SCRIPT = sys.argv.pop(1) if len(sys.argv) > 1 and sys.argv[1].endswith(".py") else os.path.join(os.path.dirname(os.path.abspath(__file__)), "mac-messages-export.py")
spec = importlib.util.spec_from_file_location("mac_messages_export", SCRIPT)
mme = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mme)

HEADER = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84"


def length(n):
    if n < 0x80:
        return bytes([n])
    if n < 0x8000:
        return b"\x81" + n.to_bytes(2, "little")
    return b"\x82" + n.to_bytes(4, "little")


def plain(text):
    """NSString: the type tag right after the class version."""
    b = text.encode("utf-8")
    return HEADER + b"\x08NSString\x01\x94\x84\x01+" + length(len(b)) + b + b"\x86\x84\x02iI\x01\x05\x92"


def mutable(text):
    """NSMutableString -> NSString -> NSObject chain before the tag."""
    b = text.encode("utf-8")
    return (
        HEADER
        + b"\x0fNSMutableString\x01\x84\x84\x08NSString\x01\x84\x84\x08NSObject\x00\x85\x92\x84\x01+"
        + length(len(b))
        + b
        + b"\x86"
    )


class DecodeAttributedBody(unittest.TestCase):
    def test_plain_nsstring(self):
        self.assertEqual(mme.decode_attributed_body(plain("hello")), "hello")
        self.assertEqual(mme.decode_attributed_body(plain("héllo \U0001f525")), "héllo \U0001f525")

    def test_mutable_string_chain(self):
        self.assertEqual(mme.decode_attributed_body(mutable("mutable text")), "mutable text")

    def test_long_lengths(self):
        long = "x" * 300
        self.assertEqual(mme.decode_attributed_body(plain(long)), long)
        self.assertEqual(mme.decode_attributed_body(mutable(long)), long)
        huge = "y" * 70000
        self.assertEqual(mme.decode_attributed_body(plain(huge)), huge)

    def test_bare_plus_legacy(self):
        b = b"...NSString\x01\x94+\x03abc"
        self.assertEqual(mme.decode_attributed_body(b), "abc")

    def test_garbage(self):
        self.assertIsNone(mme.decode_attributed_body(None))
        self.assertIsNone(mme.decode_attributed_body(b""))
        self.assertIsNone(mme.decode_attributed_body(b"no class name here"))
        self.assertIsNone(mme.decode_attributed_body(plain("hello")[:-12]))
        self.assertIsNone(mme.decode_attributed_body(b"NSString" + b"\x00" * 60 + b"\x84\x01+\x01a"))


class ExportLinkPreviews(unittest.TestCase):
    """A URL balloon carries payload_data (base64) and its hidden preview files."""

    def setUp(self):
        import tempfile

        self.tmp = tempfile.TemporaryDirectory()
        d = self.tmp.name
        self.db = os.path.join(d, "chat.db")
        self.img = os.path.join(d, "hero.pluginPayloadAttachment")
        with open(self.img, "wb") as f:
            f.write(b"\xff\xd8\xffjpeg")
        import sqlite3

        db = sqlite3.connect(self.db)
        db.executescript(
            """
            CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT, attributedBody BLOB, is_from_me INT, date INT,
              date_delivered INT, date_read INT, subject TEXT, service TEXT, item_type INT, group_action_type INT,
              associated_message_guid TEXT, associated_message_type INT, balloon_bundle_id TEXT, expressive_send_style_id TEXT,
              is_sent INT, is_delivered INT, error INT, destination_caller_id TEXT, cache_has_attachments INT, handle_id INT,
              payload_data BLOB);
            CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
            CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT, style INT, service_name TEXT,
              display_name TEXT, group_id TEXT);
            CREATE TABLE chat_message_join (chat_id INT, message_id INT);
            CREATE TABLE chat_handle_join (chat_id INT, handle_id INT);
            CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, guid TEXT, filename TEXT, mime_type TEXT, uti TEXT,
              transfer_name TEXT, total_bytes INT, is_sticker INT, hide_attachment INT);
            CREATE TABLE message_attachment_join (message_id INT, attachment_id INT);
            INSERT INTO handle VALUES (1, 'bob@example.com');
            INSERT INTO chat VALUES (1, 'iMessage;-;bob@example.com', 'bob@example.com', 45, 'iMessage', NULL, NULL);
            INSERT INTO chat_handle_join VALUES (1, 1);
            """
        )
        url = "com.apple.messages.URLBalloonProvider"
        for rowid, balloon, payload in [(1, url, b"bplist00archive"), (2, "com.example.app", b"other")]:
            db.execute(
                "INSERT INTO message (ROWID, guid, text, is_from_me, date, handle_id, balloon_bundle_id, payload_data, item_type)"
                " VALUES (?, ?, 'https://example.com', 0, ?, 1, ?, ?, 0)",
                (rowid, "G-%d" % rowid, 800_000_000 * 1_000_000_000 + rowid, balloon, payload),
            )
            db.execute("INSERT INTO chat_message_join VALUES (1, ?)", (rowid,))
            db.execute("INSERT INTO attachment VALUES (?, ?, ?, NULL, 'dyn.x', 'x.pluginPayloadAttachment', 0, 0, 1)", (rowid, "A-%d" % rowid, self.img))
            db.execute("INSERT INTO message_attachment_join VALUES (?, ?)", (rowid, rowid))
        db.commit()
        db.close()

    def tearDown(self):
        self.tmp.cleanup()

    def export(self):
        import io
        import json
        import tarfile
        import types

        args = types.SimpleNamespace(db=self.db, since=0, days=None, max_attachment_mb=50.0, no_files=False)
        out = types.SimpleNamespace(buffer=io.BytesIO())
        real_out, real_err = sys.stdout, sys.stderr
        sys.stdout, sys.stderr = out, io.StringIO()
        try:
            mme.export(args)
        finally:
            sys.stdout, sys.stderr = real_out, real_err
        tar = tarfile.open(fileobj=io.BytesIO(out.buffer.getvalue()))
        lines = [json.loads(l) for l in tar.extractfile("export.jsonl").read().decode().splitlines()]
        return lines, {m.name: tar.extractfile(m).read() for m in tar.getmembers() if m.name != "export.jsonl"}

    def test_url_balloon_payload_and_files(self):
        import base64

        lines, files = self.export()
        msgs = {m["guid"]: m for m in lines if m["type"] == "message"}
        link = msgs["G-1"]
        self.assertEqual(base64.b64decode(link["payload_data"]), b"bplist00archive")
        self.assertEqual(link["attachments"][0]["link_file"], True)
        self.assertEqual(files[link["attachments"][0]["path"]], b"\xff\xd8\xffjpeg")
        self.assertEqual(lines[0]["link_previews"], 1)
        # other balloons: no payload, hidden files stay out
        other = msgs["G-2"]
        self.assertIsNone(other["payload_data"])
        self.assertEqual(other["attachments"][0]["missing"], "hidden")
        self.assertNotIn("link_file", other["attachments"][0])


if __name__ == "__main__":
    unittest.main()
