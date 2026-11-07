#!/usr/bin/env python3
"""Unit tests for mac-sync-export.py (standard library only).

    python3 -I tools/test_mac_sync_export.py [path/to/mac-sync-export.py]

mac-backgrounds-export.py must sit next to the script. `make_mac_home(dir)`
builds a synthetic Mac home (pinning plist, chat.db, background
cache); the Rust end-to-end test of `imsg sync-mac` uses it too
(`--make-home DIR`). All data here is synthetic.
"""

import importlib.util
import io
import json
import os
import plistlib
import sqlite3
import sys
import tarfile
import tempfile
import types
import unittest

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = sys.argv.pop(1) if len(sys.argv) > 1 and sys.argv[1].endswith(".py") else os.path.join(HERE, "mac-sync-export.py")
spec = importlib.util.spec_from_file_location("mac_sync_export", SCRIPT)
mse = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mse)

GROUP_GUID = "5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"


def make_mac_home(home):
    """A synthetic Mac home directory under `home`."""
    prefs = os.path.join(home, "Library", "Preferences")
    os.makedirs(prefs, exist_ok=True)
    with open(os.path.join(prefs, "com.apple.messages.pinning.plist"), "wb") as f:
        plistlib.dump({"pD": {"pP": ["+12055550100", "bob@example.com", GROUP_GUID.lower(), "chat987654", "garbage!"],
                              "pR": 2, "pV": 2, "pU": "contextMenu"}}, f, fmt=plistlib.FMT_BINARY)
    msgs = os.path.join(home, "Library", "Messages")
    os.makedirs(os.path.join(msgs, "TranscriptBackgroundCache"), exist_ok=True)
    db = sqlite3.connect(os.path.join(msgs, "chat.db"))
    db.executescript(
        """
        CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT, style INTEGER, service_name TEXT,
                           display_name TEXT, group_id TEXT, properties BLOB);
        CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
        CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
        """
    )
    bg = plistlib.dumps({"backgroundProperties": {"trabaid": "AAAA-BG-1", "trabapv": 1, "trabafs": 10}}, fmt=plistlib.FMT_BINARY)
    db.executemany("INSERT INTO chat VALUES (?,?,?,?,?,?,?,?)", [
        (1, "iMessage;-;+12055550100", "+12055550100", 45, "iMessage", None, "G-1", bg),
        (2, "iMessage;+;chat987654", "chat987654", 43, "iMessage", "Team", "ABCD-0000-1111", None),
    ])
    db.executemany("INSERT INTO handle VALUES (?,?)", [(1, "+12055550100"), (2, "bob@example.com"), (3, "carol@example.com")])
    db.executemany("INSERT INTO chat_handle_join VALUES (?,?)", [(1, 1), (2, 2), (2, 3)])
    db.commit()
    db.close()
    with open(os.path.join(msgs, "TranscriptBackgroundCache", "AAAA-BG-1"), "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR")
    return home


def run_export(home, **flags):
    args = mse.parse_args([])
    args.pinning = os.path.join(home, "Library", "Preferences", "com.apple.messages.pinning.plist")
    args.db = os.path.join(home, "Library", "Messages", "chat.db")
    args.cache = os.path.join(home, "Library", "Messages", "TranscriptBackgroundCache")
    for k, v in flags.items():
        setattr(args, k, v)
    buf = io.BytesIO()
    meta = mse.export(args, buf)
    tar = tarfile.open(fileobj=io.BytesIO(buf.getvalue()), mode="r:")
    members = {m.name: tar.extractfile(m).read() for m in tar.getmembers()}
    return meta, members


class Pins(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        make_mac_home(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_pins(self):
        meta, files = run_export(self.tmp.name, pins=True)
        self.assertIsNone(meta["backgrounds"])
        self.assertNotIn("contacts", meta, "contacts come from iCloud, not the Mac")
        self.assertEqual(meta["pins"], {"count": 5, "file": "pins.plist"})
        xml = files["pins.plist"]
        self.assertTrue(xml.startswith(b"<?xml"), "XML, not binary")
        self.assertEqual(mse.pin_list(xml), ["+12055550100", "bob@example.com", GROUP_GUID.lower(), "chat987654", "garbage!"])
        # non-addresses looked up in chat.db: the chat987654 group (the GUID and garbage are not there)
        self.assertEqual(meta["pin_chats"], [{"pin": "chat987654", "rowid": 2, "chat_identifier": "chat987654", "style": 43,
                                              "display_name": "Team", "group_id": "ABCD-0000-1111",
                                              "participants": ["bob@example.com", "carol@example.com"]}])

    def test_unreadable_chat_db_is_reported(self):
        os.remove(os.path.join(self.tmp.name, "Library", "Messages", "chat.db"))
        meta, files = run_export(self.tmp.name, pins=True)
        self.assertEqual(meta["pin_chats"], [])
        self.assertIn("lookup_error", meta["pins"])
        self.assertEqual(meta["pins"]["count"], 5)
        self.assertIn("pins.plist", files)

    def test_no_pins_file(self):
        meta, files = run_export(self.tmp.name, pins=True, pinning=os.path.join(self.tmp.name, "nope.plist"))
        self.assertEqual(meta["pins"], {"count": 0, "file": None})
        self.assertNotIn("pins.plist", files)

    def test_contacts_flag_is_gone(self):
        with open(os.devnull, "w") as null:
            stderr, sys.stderr = sys.stderr, null
            try:
                with self.assertRaises(SystemExit):
                    mse.parse_args(["--contacts"])
            finally:
                sys.stderr = stderr

    def test_pin_list_shapes(self):
        dump = lambda v: plistlib.dumps(v, fmt=plistlib.FMT_XML)
        self.assertEqual(mse.pin_list(dump({})), [])
        self.assertEqual(mse.pin_list(dump({"pD": {"pP": "x"}})), [])
        self.assertEqual(mse.pin_list(dump({"pD": {"pP": ["a@b.c", 3, "+1"]}})), ["a@b.c", "+1"])
        self.assertTrue(mse.looks_like_address("+1 (205) 555-0100"))
        self.assertTrue(mse.looks_like_address("bob@example.com"))
        self.assertFalse(mse.looks_like_address(GROUP_GUID))
        self.assertFalse(mse.looks_like_address("chat987654"))


class Backgrounds(unittest.TestCase):
    def test_backgrounds_in_the_same_tar(self):
        with tempfile.TemporaryDirectory() as home:
            make_mac_home(home)
            meta, files = run_export(home, pins=True, backgrounds=True)
            self.assertEqual(meta["backgrounds"]["chats_with_background"], 1)
            lines = [json.loads(l) for l in files["backgrounds.jsonl"].decode().splitlines()]
            self.assertEqual(lines[1]["poster"], "backgrounds/AAAA-BG-1")
            self.assertEqual(files["backgrounds/AAAA-BG-1"][:4], b"\x89PNG")
            self.assertIn("pins.plist", files)
            self.assertFalse([n for n in files if n.startswith("contacts/")])
            self.assertEqual(sorted(json.loads(files["sync.json"])), ["backgrounds", "exported_at", "format", "pin_chats", "pins"])

    def test_embedded_source(self):
        # imsg prepends the backgrounds exporter as _BACKGROUNDS_SRC
        with open(os.path.join(os.path.dirname(SCRIPT), "mac-backgrounds-export.py"), encoding="utf-8") as f:
            src = f.read()
        mse._BACKGROUNDS_SRC = src
        try:
            self.assertTrue(hasattr(mse.backgrounds_module(), "background_fields"))
        finally:
            del mse._BACKGROUNDS_SRC


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--make-home":
        make_mac_home(sys.argv[2])
        sys.exit(0)
    unittest.main()
