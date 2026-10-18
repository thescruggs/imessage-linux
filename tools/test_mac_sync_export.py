#!/usr/bin/env python3
"""Unit tests for mac-sync-export.py (standard library only).

    python3 -I tools/test_mac_sync_export.py [path/to/mac-sync-export.py]

mac-backgrounds-export.py must sit next to the script. `make_mac_home(dir)`
builds a synthetic Mac home (Contacts database, pinning plist, chat.db,
background cache); the Rust end-to-end test of `imsg sync-mac` uses it too
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

JPEG = b"\xff\xd8\xff\xe0" + bytes(range(256)) * 2
GROUP_GUID = "5B0E2C1A-3F4D-4E21-9A55-0C7E1B2D3F40"
LONG_NICK = "Dr. Bartholomew-Maximilian Édouard von Wittelsbach-Hohenzollern \U0001F600"


def make_store(path, records, emails, phones, support_files=()):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    db = sqlite3.connect(path)
    db.executescript(
        """
        CREATE TABLE Z_PRIMARYKEY (Z_ENT INTEGER PRIMARY KEY, Z_NAME TEXT);
        CREATE TABLE ZABCDRECORD (Z_PK INTEGER PRIMARY KEY, Z_ENT INTEGER, ZUNIQUEID TEXT, ZFIRSTNAME TEXT,
            ZMIDDLENAME TEXT, ZLASTNAME TEXT, ZTITLE TEXT, ZSUFFIX TEXT, ZORGANIZATION TEXT, ZNICKNAME TEXT,
            ZDISPLAYFLAGS INTEGER, ZTHUMBNAILIMAGEDATA BLOB, ZIMAGEDATA BLOB);
        CREATE TABLE ZABCDEMAILADDRESS (ZOWNER INTEGER, ZADDRESS TEXT, ZLABEL TEXT, ZISPRIMARY INTEGER, ZORDERINGINDEX INTEGER);
        CREATE TABLE ZABCDPHONENUMBER (ZOWNER INTEGER, ZFULLNUMBER TEXT, ZLABEL TEXT, ZISPRIMARY INTEGER, ZORDERINGINDEX INTEGER);
        INSERT INTO Z_PRIMARYKEY VALUES (19, 'ABCDContact'), (20, 'ABCDGroup');
        """
    )
    db.executemany("INSERT INTO ZABCDRECORD VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)", records)
    db.executemany("INSERT INTO ZABCDEMAILADDRESS VALUES (?,?,?,?,?)", emails)
    db.executemany("INSERT INTO ZABCDPHONENUMBER VALUES (?,?,?,?,?)", phones)
    db.commit()
    db.close()
    for name, data in support_files:
        d = os.path.join(os.path.dirname(path), ".AddressBook-v22_SUPPORT", "_EXTERNAL_DATA")
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, name), "wb") as f:
            f.write(data)


def make_mac_home(home):
    """A synthetic Mac home directory under `home`."""
    ab = os.path.join(home, "Library", "Application Support", "AddressBook")
    # iCloud source: Alice (inline photo), Acme (show as company), a long
    # nickname (folding), an external-storage photo, a nameless record
    # (skipped) and a group (not a contact).
    make_store(
        os.path.join(ab, "Sources", "SRC-1", "AddressBook-v22.abcddb"),
        [
            (1, 19, "AAAA-1111:ABPerson", "Alice", None, "Anderson", None, None, None, None, 0, b"\x01" + JPEG, None),
            (2, 19, "BBBB-2222:ABPerson", "Bob", None, "Builder", None, None, "Acme, Inc.", None, 1, None, None),
            (3, 19, "CCCC-3333:ABPerson", None, None, None, None, None, None, LONG_NICK, 0, None, None),
            (4, 19, "DDDD-4444:ABPerson", "Dana", "Q", "Diaz", "Dr.", "Jr.", None, None, 0, None, b"\x02EXT-UUID-1\x00"),
            (5, 19, "EEEE-5555:ABPerson", None, None, None, None, None, None, None, 0, None, None),
            (6, 20, "GGGG-6666:ABGroup", "Friends", None, None, None, None, None, None, 0, None, None),
            (7, 19, None, "Pat", None, None, None, None, None, None, 0, None, None),
        ],
        [(1, "alice@example.com", "_$!<Work>!$_", 1, 0), (1, "alice@home.example", "_$!<Home>!$_", 0, 1),
         (2, "bob@acme.example", None, 1, 0), (3, "nick@example.com", "_$!<Other>!$_", 0, 0)],
        [(1, "+1 (205) 555-0100", "iPhone", 1, 0), (2, "+12055550111", "_$!<Main>!$_", 1, 0),
         (4, "205.555.0144", "_$!<Mobile>!$_", 1, 0), (4, "+12055550145", "_$!<HomeFAX>!$_", 0, 1)],
        [("EXT-UUID-1", JPEG)],
    )
    # local store: Alice again (same uid: exported once) and Erin.
    make_store(
        os.path.join(ab, "AddressBook-v22.abcddb"),
        [(1, 19, "AAAA-1111:ABPerson", "Alice", None, "Duplicate", None, None, None, None, 0, None, None),
         (2, 19, "FFFF-7777:ABPerson", "Erin", None, "Evans; Esq", None, None, None, None, 0, None, None)],
        [(2, "erin@example.org", None, 1, 0)],
        [],
    )
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
    args.addressbook = os.path.join(home, "Library", "Application Support", "AddressBook")
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


class Contacts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        make_mac_home(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_vcards(self):
        meta, files = run_export(self.tmp.name, contacts=True)
        self.assertIsNone(meta["pins"])
        self.assertEqual(meta["contacts"], {"count": 6, "photos": 2})
        self.assertEqual(json.loads(files["sync.json"])["contacts"]["count"], 6)
        names = sorted(n for n in files if n.startswith("contacts/"))
        self.assertEqual(names, ["contacts/mac-AAAA-1111_ABPerson.vcf", "contacts/mac-BBBB-2222_ABPerson.vcf",
                                 "contacts/mac-CCCC-3333_ABPerson.vcf", "contacts/mac-DDDD-4444_ABPerson.vcf",
                                 "contacts/mac-FFFF-7777_ABPerson.vcf", "contacts/mac-pk7.vcf"])
        alice = files["contacts/mac-AAAA-1111_ABPerson.vcf"].decode()
        self.assertTrue(alice.startswith("BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//imessage-linux//mac-contacts-export//EN\r\n"
                                         "UID:AAAA-1111:ABPerson\r\nFN:Alice Anderson\r\nN:Anderson;Alice;;;\r\n"), alice)
        self.assertIn("EMAIL;TYPE=INTERNET,WORK:alice@example.com\r\nEMAIL;TYPE=INTERNET,HOME:alice@home.example\r\n", alice)
        self.assertIn("TEL;TYPE=CELL:+1 (205) 555-0100\r\n", alice)
        self.assertIn("PHOTO;ENCODING=b;TYPE=JPEG:", alice)
        self.assertTrue(alice.endswith("END:VCARD\r\n"))
        self.assertNotIn("Duplicate", alice, "the first store wins for a uid")
        acme = files["contacts/mac-BBBB-2222_ABPerson.vcf"].decode()
        self.assertIn("FN:Acme\\, Inc.\r\n", acme)
        self.assertIn("X-ABShowAs:COMPANY\r\n", acme)
        self.assertIn("TEL;TYPE=MAIN:+12055550111\r\n", acme)
        long = files["contacts/mac-CCCC-3333_ABPerson.vcf"]
        self.assertTrue(all(len(l) <= 75 for l in long.split(b"\r\n")), "folded to 75 octets")
        self.assertIn(b"\r\n ", long)
        dana = files["contacts/mac-DDDD-4444_ABPerson.vcf"].decode()
        self.assertIn("FN:Dr. Dana Q Diaz Jr.\r\nN:Diaz;Dana;Q;Dr.;Jr.\r\n", dana)
        self.assertIn("PHOTO;ENCODING=b;TYPE=JPEG:", dana.replace("\r\n ", ""), "external-storage photo")
        self.assertIn("TEL;TYPE=CELL:205.555.0144\r\nTEL;TYPE=HOME:+12055550145\r\n", dana)
        self.assertIn("N:Evans\\; Esq;Erin;;;", files["contacts/mac-FFFF-7777_ABPerson.vcf"].decode())

    def test_missing_database_is_reported(self):
        meta, files = run_export(self.tmp.name, contacts=True, pins=True, addressbook=os.path.join(self.tmp.name, "nope"))
        self.assertIn("error", meta["contacts"])
        self.assertEqual(meta["pins"]["count"], 5, "the other sections go on")
        self.assertFalse([n for n in files if n.startswith("contacts/")])


class Pins(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        make_mac_home(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_pins(self):
        meta, files = run_export(self.tmp.name, pins=True)
        self.assertIsNone(meta["contacts"])
        self.assertEqual(meta["pins"], {"count": 5, "file": "pins.plist"})
        xml = files["pins.plist"]
        self.assertTrue(xml.startswith(b"<?xml"), "XML, not binary")
        self.assertEqual(mse.pin_list(xml), ["+12055550100", "bob@example.com", GROUP_GUID.lower(), "chat987654", "garbage!"])
        # non-addresses looked up in chat.db: the chat987654 group (the GUID and garbage are not there)
        self.assertEqual(meta["pin_chats"], [{"pin": "chat987654", "rowid": 2, "chat_identifier": "chat987654", "style": 43,
                                              "display_name": "Team", "group_id": "ABCD-0000-1111",
                                              "participants": ["bob@example.com", "carol@example.com"]}])

    def test_no_pins_file(self):
        meta, files = run_export(self.tmp.name, pins=True, pinning=os.path.join(self.tmp.name, "nope.plist"))
        if mse.shutil.which("defaults"):
            self.skipTest("on a Mac `defaults export` answers")
        self.assertEqual(meta["pins"], {"count": 0, "file": None})
        self.assertNotIn("pins.plist", files)

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
            meta, files = run_export(home, contacts=True, pins=True, backgrounds=True)
            self.assertEqual(meta["backgrounds"]["chats_with_background"], 1)
            lines = [json.loads(l) for l in files["backgrounds.jsonl"].decode().splitlines()]
            self.assertEqual(lines[1]["poster"], "backgrounds/AAAA-BG-1")
            self.assertEqual(files["backgrounds/AAAA-BG-1"][:4], b"\x89PNG")
            self.assertIn("pins.plist", files)
            self.assertIn("contacts/mac-AAAA-1111_ABPerson.vcf", files)
            self.assertEqual(sorted(json.loads(files["sync.json"])), ["backgrounds", "contacts", "exported_at", "format", "pin_chats", "pins"])

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
