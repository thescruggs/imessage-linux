#!/usr/bin/env python3
"""
Diagnostic: verify iCloud CardDAV access with an Apple ID + app-specific
password, and learn exactly how iCloud answers each step of the sync flow
that imessaged (`crates/imessaged/src/carddav.rs`) implements.

Standard library only (no pip installs needed).

Usage:
  export ICLOUD_ID="you@icloud.com"
  export ICLOUD_APP_PASSWORD="abcd-efgh-ijkl-mnop"   # from account.apple.com → App-Specific Passwords
  python3 carddav_probe.py                 # discover + list + fetch a sample batch
  python3 carddav_probe.py --all           # also fetch every card (prints counts only)
  python3 carddav_probe.py --dump N        # print N raw vCards (default 2)
  python3 carddav_probe.py --save-fixtures DIR   # save scrubbed XML responses for unit tests

Everything here is read-only against iCloud. The real sync is the daemon's
(`imsg contacts login`); this only shows what iCloud answers.
"""

import argparse
import base64
import os
import re
import sys
import xml.etree.ElementTree as ET
from urllib import request, error, parse

APPLE_ID = os.environ.get("ICLOUD_ID", "")
APP_PASSWORD = os.environ.get("ICLOUD_APP_PASSWORD", "")
ROOT = "https://contacts.icloud.com/"

NS = {
    "D": "DAV:",
    "C": "urn:ietf:params:xml:ns:carddav",
}
D = "{DAV:}"
C = "{urn:ietf:params:xml:ns:carddav}"


# ---------- tiny WebDAV helper (stdlib only) ----------

class _Redirect(request.HTTPRedirectHandler):
    """urllib refuses to redirect non-GET verbs; iCloud 301s PROPFIND from
    contacts.icloud.com to pNN-contacts.icloud.com, so replay the same verb +
    body at the new location (this is what the Qt client must do too)."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = request.Request(newurl, data=req.data, headers=dict(req.header_items()), method=req.get_method())
        return new


_opener = request.build_opener(_Redirect)


def dav(method, url, depth=None, body=None, timeout=60):
    """Return (status, final_url, body_text)."""
    headers = {
        "Authorization": "Basic " + base64.b64encode(f"{APPLE_ID}:{APP_PASSWORD}".encode()).decode(),
        "User-Agent": "kimessage-carddav-probe/0.1",
    }
    if depth is not None:
        headers["Depth"] = str(depth)
    data = None
    if body is not None:
        data = body.encode()
        headers["Content-Type"] = "application/xml; charset=utf-8"
    req = request.Request(url, data=data, headers=headers, method=method)
    try:
        with _opener.open(req, timeout=timeout) as resp:
            return resp.status, resp.geturl(), resp.read().decode("utf-8", "replace")
    except error.HTTPError as e:
        return e.code, e.geturl(), e.read().decode("utf-8", "replace")


def resolve(base, href):
    """iCloud returns a mix of absolute URLs (home set, with :443) and
    server-relative paths (principal, card hrefs)."""
    return parse.urljoin(base, href)


def section(title):
    print(f"\n=== {title} ===")


def must(status, where, body):
    if status == 401:
        sys.exit(f"{where}: 401 Unauthorized — check ICLOUD_ID / app-specific password (NOT your Apple ID password).")
    if status not in (200, 207):
        sys.exit(f"{where}: unexpected HTTP {status}\n{body[:800]}")


# ---------- the five steps ----------

def discover_principal():
    section("1. current-user-principal")
    body = ('<?xml version="1.0" encoding="utf-8"?>'
            '<D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>')
    status, final, text = dav("PROPFIND", ROOT, depth=0, body=body)
    print(f"HTTP {status}  (final URL after redirects: {final})")
    must(status, "principal PROPFIND", text)
    root = ET.fromstring(text)
    href = root.find(f".//{D}current-user-principal/{D}href")
    if href is None:
        sys.exit(f"no current-user-principal in response:\n{text[:800]}")
    principal = resolve(final, href.text.strip())
    print("principal:", principal)
    return principal, text


def discover_home(principal):
    section("2. addressbook-home-set")
    body = ('<?xml version="1.0" encoding="utf-8"?>'
            '<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav">'
            '<D:prop><C:addressbook-home-set/></D:prop></D:propfind>')
    status, final, text = dav("PROPFIND", principal, depth=0, body=body)
    print(f"HTTP {status}  (final URL: {final})")
    must(status, "home-set PROPFIND", text)
    root = ET.fromstring(text)
    href = root.find(f".//{C}addressbook-home-set/{D}href")
    if href is None:
        sys.exit(f"no addressbook-home-set in response:\n{text[:800]}")
    home = resolve(final, href.text.strip())
    print("home set:", home)
    return home, text


def list_addressbooks(home):
    section("3. address books under home set")
    body = ('<?xml version="1.0" encoding="utf-8"?>'
            '<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/">'
            '<D:prop><D:resourcetype/><D:displayname/><CS:getctag/><D:sync-token/></D:prop></D:propfind>')
    status, final, text = dav("PROPFIND", home, depth=1, body=body)
    print(f"HTTP {status}  (final URL: {final})")
    must(status, "home PROPFIND Depth:1", text)
    root = ET.fromstring(text)
    books = []
    for resp in root.findall(f"{D}response"):
        href = resp.find(f"{D}href").text.strip()
        rtype = resp.find(f".//{D}resourcetype")
        is_book = rtype is not None and rtype.find(f"{C}addressbook") is not None
        name_el = resp.find(f".//{D}displayname")
        name = name_el.text if name_el is not None and name_el.text else ""
        ctag_el = resp.find(".//{http://calendarserver.org/ns/}getctag")
        ctag = ctag_el.text if ctag_el is not None else ""
        print(f"  {'[addressbook]' if is_book else '[collection] '} {href}  name={name!r}  ctag={ctag[:24]!r}")
        if is_book:
            books.append(resolve(final, href))
    if not books:
        sys.exit("no addressbook collections found")
    print("using:", books[0])
    return books[0], text


def list_cards(book):
    section("4. href -> etag listing (PROPFIND Depth:1)")
    body = ('<?xml version="1.0" encoding="utf-8"?>'
            '<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/><D:resourcetype/></D:prop></D:propfind>')
    status, final, text = dav("PROPFIND", book, depth=1, body=body)
    print(f"HTTP {status}  (final URL: {final})  bytes={len(text)}")
    must(status, "addressbook PROPFIND Depth:1", text)
    root = ET.fromstring(text)
    etags = {}
    for resp in root.findall(f"{D}response"):
        href = resp.find(f"{D}href").text.strip()
        rtype = resp.find(f".//{D}resourcetype")
        if rtype is not None and rtype.find(f"{D}collection") is not None:
            continue  # the address book itself
        etag_el = resp.find(f".//{D}getetag")
        etags[href] = etag_el.text if etag_el is not None else ""
    print(f"{len(etags)} cards")
    for href, etag in list(etags.items())[:3]:
        print(f"  {href}  etag={etag}")
    return etags, text


def multiget(book, hrefs):
    body = ('<?xml version="1.0" encoding="utf-8"?>'
            '<C:addressbook-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav">'
            '<D:prop><D:getetag/><C:address-data/></D:prop>'
            + "".join(f"<D:href>{parse.quote(h, safe='/%')}</D:href>" for h in hrefs)
            + "</C:addressbook-multiget>")
    status, final, text = dav("REPORT", book, depth=1, body=body)
    must(status, f"multiget of {len(hrefs)}", text)
    root = ET.fromstring(text)
    cards = []
    for resp in root.findall(f"{D}response"):
        href = resp.find(f"{D}href").text.strip()
        st = resp.find(f".//{D}status")
        data = resp.find(f".//{C}address-data")
        etag = resp.find(f".//{D}getetag")
        cards.append((href, etag.text if etag is not None else "", data.text if data is not None else "",
                      st.text if st is not None else ""))
    return cards, text


def summarize_vcard(vcf):
    fn = re.search(r"^FN:(.*)$", vcf, re.M)
    tels = re.findall(r"^TEL[^:]*:(.*)$", vcf, re.M)
    emails = re.findall(r"^EMAIL[^:]*:(.*)$", vcf, re.M)
    ver = re.search(r"^VERSION:(.*)$", vcf, re.M)
    return (fn.group(1).strip() if fn else "<no FN>", ver.group(1).strip() if ver else "?", tels, emails)


def scrub(text):
    """Remove account identifiers from saved fixtures."""
    text = re.sub(r"/\d{6,}/", "/DSID/", text)
    text = re.sub(r"p\d+-contacts\.icloud\.com", "pNN-contacts.icloud.com", text)
    return text


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--all", action="store_true", help="fetch every card in batches of 50")
    ap.add_argument("--dump", type=int, default=2, help="print N raw vCards from the sample batch")
    ap.add_argument("--batch", type=int, default=50, help="multiget batch size to test")
    ap.add_argument("--save-fixtures", metavar="DIR", help="save scrubbed XML responses here")
    args = ap.parse_args()

    if not APPLE_ID or not APP_PASSWORD:
        sys.exit("set ICLOUD_ID and ICLOUD_APP_PASSWORD first")

    fixtures = {}
    principal, fixtures["1_principal.xml"] = discover_principal()
    home, fixtures["2_home.xml"] = discover_home(principal)
    book, fixtures["3_addressbooks.xml"] = list_addressbooks(home)
    etags, fixtures["4_listing.xml"] = list_cards(book)

    section(f"5. addressbook-multiget (batch of {min(args.batch, len(etags))})")
    hrefs = list(etags)
    sample, fixtures["5_multiget.xml"] = multiget(book, hrefs[:args.batch])
    ok = sum(1 for c in sample if c[2])
    print(f"got {len(sample)} responses, {ok} with address-data")
    versions = {}
    for href, etag, vcf, st in sample:
        fn, ver, tels, emails = summarize_vcard(vcf)
        versions[ver] = versions.get(ver, 0) + 1
    print("vCard versions in batch:", versions)
    for href, etag, vcf, st in sample[:args.dump]:
        fn, ver, tels, emails = summarize_vcard(vcf)
        print(f"\n--- {href}  etag={etag}  status={st}")
        print(f"FN={fn!r}  TEL={tels}  EMAIL={emails}")
        print(vcf.strip())

    if args.all and len(hrefs) > args.batch:
        section("5b. fetching all remaining cards")
        total = len(sample)
        for i in range(args.batch, len(hrefs), args.batch):
            chunk = hrefs[i:i + args.batch]
            cards, _ = multiget(book, chunk)
            total += len(cards)
            print(f"  {i + len(chunk):>5}/{len(hrefs)}  (+{len(cards)})")
        print(f"fetched {total} cards total")

    if args.save_fixtures:
        os.makedirs(args.save_fixtures, exist_ok=True)
        for name, text in fixtures.items():
            path = os.path.join(args.save_fixtures, name)
            with open(path, "w") as f:
                f.write(scrub(text))
            print("saved", path)
        print("NOTE: 5_multiget.xml contains real contact data — review before committing.")

    section("summary")
    print("principal   :", principal)
    print("home set    :", home)
    print("address book:", book)
    print("cards       :", len(etags))


if __name__ == "__main__":
    main()
