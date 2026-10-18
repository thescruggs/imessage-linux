#!/usr/bin/env python3
# SPDX-License-Identifier: SSPL-1.0
"""Build kmessages/data/emoji-keywords.json (emoji suggestions while typing).

Sources (pinned; the output is committed, so this only runs to update it):
  * Unicode CLDR annotations, English: common/annotations/en.xml and
    common/annotationsDerived/en.xml of CLDR_RELEASE
  * emoji-test.txt of EMOJI_VERSION: which sequences are fully qualified,
    and their order (the order of emoji keyboards)

Output: [["🐍", ["snake", "reptile", "serpent"]], ...] in emoji-test.txt
order; the keywords are lower case, the CLDR short name (tts) first, then
the other annotations. Only fully-qualified emoji, no skin-tone variants
(U+1F3FB..U+1F3FF anywhere in the sequence), ZWJ sequences kept; an emoji
without English annotations is left out. The data is © Unicode, Inc.,
under the Unicode License v3 (kmessages/data/emoji-keywords.LICENSE).

Usage:
  tools/gen-emoji-keywords.py                 # download into a temp dir
  tools/gen-emoji-keywords.py --data-dir DIR  # use en.xml, en-derived.xml,
                                              # emoji-test.txt from DIR
"""

import argparse
import json
import os
import sys
import tempfile
import urllib.request
import xml.etree.ElementTree as ET

CLDR_RELEASE = "release-48"
EMOJI_VERSION = "17.0.0"
CLDR_BASE = f"https://raw.githubusercontent.com/unicode-org/cldr/{CLDR_RELEASE}/common"
SOURCES = {
    "en.xml": f"{CLDR_BASE}/annotations/en.xml",
    "en-derived.xml": f"{CLDR_BASE}/annotationsDerived/en.xml",
    # Since Unicode 17 the emoji data sits with the version's other files:
    # Public/emoji/ ends at 16.0 (Public/emoji/17.0/ is a 404, checked
    # 2026-10-18; Public/emoji/latest/ is not pinned).
    "emoji-test.txt": f"https://unicode.org/Public/{EMOJI_VERSION}/emoji/emoji-test.txt",
}

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "kmessages", "data", "emoji-keywords.json")

SKIN_TONES = {chr(c) for c in range(0x1F3FB, 0x1F400)}
VS16 = "️"


def fetch(data_dir):
    for name, url in SOURCES.items():
        path = os.path.join(data_dir, name)
        if not os.path.exists(path):
            print(f"downloading {url}", file=sys.stderr)
            with urllib.request.urlopen(url, timeout=60) as r, open(path, "wb") as f:
                f.write(r.read())


def key(emoji):
    """CLDR spells most emoji without U+FE0F; compare without it."""
    return emoji.replace(VS16, "")


def read_annotations(path, table):
    for a in ET.parse(path).getroot().iter("annotation"):
        cp = a.get("cp")
        text = (a.text or "").strip()
        if not cp or not text or text == "↑↑↑":
            continue
        entry = table.setdefault(key(cp), {"tts": None, "words": []})
        if a.get("type") == "tts":
            entry["tts"] = text.lower()
        else:
            for w in text.split("|"):
                w = w.strip().lower()
                if w and w not in entry["words"]:
                    entry["words"].append(w)


def read_emoji_test(path):
    out = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)
            if len(line) < 2:
                continue
            fields = line[0].split(";")
            if len(fields) != 2 or fields[1].strip() != "fully-qualified":
                continue
            emoji = "".join(chr(int(cp, 16)) for cp in fields[0].split())
            if any(c in SKIN_TONES for c in emoji):
                continue
            out.append(emoji)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data-dir", help="directory with en.xml, en-derived.xml, emoji-test.txt (downloaded when missing)")
    ap.add_argument("--output", default=OUT)
    args = ap.parse_args()

    tmp = None
    data_dir = args.data_dir
    if not data_dir:
        tmp = tempfile.TemporaryDirectory()
        data_dir = tmp.name
    fetch(data_dir)

    table = {}
    read_annotations(os.path.join(data_dir, "en.xml"), table)
    read_annotations(os.path.join(data_dir, "en-derived.xml"), table)

    rows = []
    missing = 0
    for emoji in read_emoji_test(os.path.join(data_dir, "emoji-test.txt")):
        entry = table.get(key(emoji))
        if not entry:
            missing += 1
            continue
        words = ([entry["tts"]] if entry["tts"] else []) + [w for w in entry["words"] if w != entry["tts"]]
        rows.append([emoji, words])

    os.makedirs(os.path.dirname(args.output), exist_ok=True)
    with open(args.output, "w", encoding="utf-8") as f:
        # One emoji per line: readable diffs when the data is updated.
        f.write("[\n")
        f.write(",\n".join(json.dumps(r, ensure_ascii=False, separators=(",", ":")) for r in rows))
        f.write("\n]\n")
    size = os.path.getsize(args.output)
    print(f"{args.output}: {len(rows)} emoji ({missing} without English annotations skipped), {size} bytes "
          f"(CLDR {CLDR_RELEASE}, Emoji {EMOJI_VERSION})", file=sys.stderr)


if __name__ == "__main__":
    main()
