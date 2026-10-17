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


if __name__ == "__main__":
    unittest.main()
