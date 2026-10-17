#!/usr/bin/env python3
"""Unit tests for mac-backgrounds-export.py (standard library only).

    python3 -I tools/test_mac_backgrounds_export.py [path/to/mac-backgrounds-export.py] [fixture.plist]

`fixture.plist` is an optional binary `properties` plist written by the
Rust test (`cargo test -p imsg`, with the `plist` crate); it must carry
`backgroundProperties.trabaid` = FIXTURE_ID and the secret fields set to
SECRET. All data here is synthetic.
"""

import datetime
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
SCRIPT = sys.argv.pop(1) if len(sys.argv) > 1 and sys.argv[1].endswith(".py") else os.path.join(os.path.dirname(os.path.abspath(__file__)), "mac-backgrounds-export.py")
FIXTURE = sys.argv.pop(1) if len(sys.argv) > 1 and sys.argv[1].endswith(".plist") else None
spec = importlib.util.spec_from_file_location("mac_backgrounds_export", SCRIPT)
mbe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mbe)

SECRET = "SECRET-KEY-MATERIAL-0123456789"
FIXTURE_ID = "11111111-2222-3333-4444-555555555555"


def props(bid, **extra):
    bp = {
        "trabaid": bid,
        "trabapv": 1,
        "trabafs": 4242,
        "trabav": 99,
        "trabaCommSafety": 0,
        "refreshDate": datetime.datetime(2026, 10, 1, 12, 0, 0),
        "trabak": SECRET,
        "trabas": SECRET,
        "trabar": "https://example.invalid/" + SECRET,
        "traboid": SECRET,
    }
    bp.update(extra)
    return plistlib.dumps({"backgroundProperties": bp, "backgroundChannelGUID": SECRET, "pv": 1}, fmt=plistlib.FMT_BINARY)


class Fields(unittest.TestCase):
    def test_fields_and_no_secrets(self):
        f = mbe.background_fields(props("AAAA-1"))
        self.assertEqual(f, {"id": "AAAA-1", "version": 1, "size": 4242, "av": 99, "refreshed": 1790856000})
        self.assertNotIn(SECRET, json.dumps(f))

    def test_no_background(self):
        self.assertIsNone(mbe.background_fields(None))
        self.assertIsNone(mbe.background_fields(b""))
        self.assertIsNone(mbe.background_fields(b"not a plist"))
        self.assertIsNone(mbe.background_fields(plistlib.dumps({"pv": 1}, fmt=plistlib.FMT_BINARY)))
        self.assertIsNone(mbe.background_fields(plistlib.dumps({"backgroundProperties": "x"}, fmt=plistlib.FMT_BINARY)))

    def test_unusable_ids(self):
        for bad in ["../../etc/passwd", "a/b", "", " ", "-rf", "x" * 65]:
            self.assertIsNone(mbe.background_fields(props(bad))["id"], bad)
        self.assertIsNone(mbe.background_fields(props(None))["id"])
        self.assertIsNone(mbe.background_fields(props(12))["id"])
        f = mbe.background_fields(props("AAAA-2", trabapv="1", refreshDate="yesterday"))
        self.assertEqual((f["version"], f["refreshed"]), (None, None))

    @unittest.skipUnless(FIXTURE, "no Rust-written fixture given")
    def test_rust_fixture(self):
        with open(FIXTURE, "rb") as fh:
            data = fh.read()
        self.assertTrue(data.startswith(b"bplist00"))
        f = mbe.background_fields(data)
        self.assertEqual(f["id"], FIXTURE_ID)
        self.assertEqual((f["version"], f["size"]), (1, 123456))
        self.assertNotIn(SECRET, json.dumps(f))


class Export(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        d = self.tmp.name
        self.db = os.path.join(d, "chat.db")
        self.cache = os.path.join(d, "cache")
        os.mkdir(self.cache)
        db = sqlite3.connect(self.db)
        db.executescript(
            """
            CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, style INTEGER, service_name TEXT,
                               display_name TEXT, group_id TEXT, properties BLOB);
            CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
            CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
            """
        )
        rows = [
            (1, "+15550000001", 45, "iMessage", None, "G-1", props("AAAA-1")),
            (2, "chat123", 43, "iMessage", "Family", "abcd-0001", props("BBBB-2")),
            (3, "+15550000003", 45, "SMS", None, "G-3", props("CCCC-3")),  # not cached
            (4, "+15550000004", 45, "SMS", None, "G-4", props("../evil")),
            (5, "+15550000005", 45, "SMS", None, "G-5", plistlib.dumps({"pv": 1}, fmt=plistlib.FMT_BINARY)),
            (6, "+15550000006", 45, "SMS", None, "G-6", None),
            (7, "+15550000007", 45, "SMS", None, "G-7", props("DDDD-4")),  # too large
            (8, "+15550000008", 45, "SMS", None, "G-8", props("EEEE-5")),  # symlink
            (9, "+15550000009", 45, "SMS", None, "G-9", props("FFFF-6")),  # watch only
        ]
        if FIXTURE:
            with open(FIXTURE, "rb") as fh:
                rows.append((10, "+15550000010", 45, "iMessage", None, "G-10", fh.read()))
        db.executemany("INSERT INTO chat VALUES (?,?,?,?,?,?,?)", rows)
        db.executemany("INSERT INTO handle VALUES (?,?)", [(1, "+15550000001"), (2, "bob@example.com"), (3, "carol@example.com")])
        db.executemany("INSERT INTO chat_handle_join VALUES (?,?)", [(1, 1), (2, 2), (2, 3)])
        db.commit()
        db.close()
        for name, data in [("AAAA-1", b"AA01poster"), ("AAAA-1-watchBackground", b"bplist00watch"), ("BBBB-2", b"PK\x03\x04zip"),
                           ("DDDD-4", b"x" * 2048), ("FFFF-6-watchBackground", b"bplist00w"), (FIXTURE_ID, b"AA01fixture")]:
            with open(os.path.join(self.cache, name), "wb") as fh:
                fh.write(data)
        outside = os.path.join(d, "outside")
        with open(outside, "wb") as fh:
            fh.write(b"not yours")
        os.symlink(outside, os.path.join(self.cache, "EEEE-5"))

    def tearDown(self):
        self.tmp.cleanup()

    def run_export(self):
        buf = io.BytesIO()
        args = types.SimpleNamespace(db=self.db, cache=self.cache, max_file_mb=1 / 1024)
        counters = mbe.export(args, buf)
        raw = buf.getvalue()
        tar = tarfile.open(fileobj=io.BytesIO(raw), mode="r:")
        members = {m.name: tar.extractfile(m).read() for m in tar.getmembers()}
        lines = [json.loads(l) for l in members.pop("backgrounds.jsonl").decode().splitlines()]
        return raw, counters, lines, members

    def test_export(self):
        raw, counters, lines, files = self.run_export()
        self.assertNotIn(SECRET.encode(), raw, "no key material in the tar")
        meta, chats = lines[0], {c["rowid"]: c for c in lines[1:]}
        self.assertEqual(meta["type"], "meta")
        self.assertEqual(sorted(chats), [1, 2, 3, 4, 7, 8, 9] + ([10] if FIXTURE else []))
        c1 = chats[1]
        self.assertEqual((c1["poster"], c1["watch"], c1["missing"]), ("backgrounds/AAAA-1", "backgrounds/AAAA-1-watchBackground", None))
        self.assertEqual(c1["participants"], ["+15550000001"])
        self.assertEqual(c1["background"]["id"], "AAAA-1")
        self.assertEqual(set(c1["background"]), {"id", "version", "size", "av", "refreshed"})
        c2 = chats[2]
        self.assertEqual((c2["style"], c2["group_id"], c2["display_name"]), (43, "abcd-0001", "Family"))
        self.assertEqual(c2["participants"], ["bob@example.com", "carol@example.com"])
        self.assertEqual((c2["poster"], c2["watch"]), ("backgrounds/BBBB-2", None))
        self.assertEqual((chats[3]["poster"], chats[3]["missing"]), (None, "not_cached"))
        self.assertEqual((chats[4]["background"]["id"], chats[4]["missing"]), (None, "bad_id"))
        self.assertEqual((chats[7]["poster"], chats[7]["missing"]), (None, "too_large"))
        self.assertEqual((chats[8]["poster"], chats[8]["missing"]), (None, "not_cached"), "symlinks are not followed")
        self.assertEqual((chats[9]["poster"], chats[9]["watch"], chats[9]["missing"]), (None, "backgrounds/FFFF-6-watchBackground", None))
        want = {"backgrounds/AAAA-1": b"AA01poster", "backgrounds/AAAA-1-watchBackground": b"bplist00watch", "backgrounds/BBBB-2": b"PK\x03\x04zip",
                "backgrounds/FFFF-6-watchBackground": b"bplist00w"}
        if FIXTURE:
            want["backgrounds/" + FIXTURE_ID] = b"AA01fixture"
        self.assertEqual(files, want)
        self.assertEqual(meta["files"], len(want))
        self.assertTrue(all(n.startswith("backgrounds/") and ".." not in n for n in files))


if __name__ == "__main__":
    unittest.main()
