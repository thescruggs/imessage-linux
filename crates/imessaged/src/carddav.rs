//! iCloud contacts over CardDAV (RFC 6352), signed in with an Apple ID and
//! an app-specific password (appleid.apple.com > Sign-In and Security >
//! App-Specific Passwords; the account password itself is refused once
//! two-factor authentication is on).
//!
//! Discovery: `current-user-principal` at `https://contacts.icloud.com/`,
//! the principal's `addressbook-home-set` (an account-specific
//! `pNN-contacts.icloud.com` URL), the home's address books (iCloud has
//! one, `card/`). A sync lists every card's ETag (`PROPFIND`, depth 1) and
//! fetches only new or changed cards (`addressbook-multiget`).
//!
//! The password only ever goes to iCloud: every request (redirects, the
//! discovered home, photo URLs inside cards) must be `https` on
//! `icloud.com` or a subdomain, or on the configured base URL's own origin
//! (tests point that at a local server). Nothing here logs it or the cards.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use reqwest::{Method, StatusCode, Url};
use zeroize::Zeroizing;

/// Where discovery starts.
pub const ICLOUD_CONTACTS: &str = "https://contacts.icloud.com/";
const DAV: &str = "DAV:";
const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: usize = 5;
/// Response size caps: a listing / discovery reply, a multiget batch, one
/// photo fetched from a URL in a card.
const MAX_LISTING: usize = 32 << 20;
const MAX_BATCH: usize = 128 << 20;
const MAX_PHOTO: usize = 4 << 20;
/// Cards per `addressbook-multiget`.
pub const BATCH: usize = 100;

/// Apple ID + app-specific password. `Debug` never prints the password.
#[derive(Clone)]
pub struct Credentials {
    pub apple_id: String,
    pub password: Zeroizing<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("apple_id", &self.apple_id).field("password", &"<redacted>").finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CardDavError {
    /// 401 / 403: wrong Apple ID or password, or not an app-specific one.
    #[error(
        "iCloud refused the Apple ID / app-specific password (create one at appleid.apple.com > Sign-In and Security > \
         App-Specific Passwords; your Apple ID password itself does not work)"
    )]
    Auth,
    /// Network trouble, timeouts, 5xx: worth trying again later.
    #[error("cannot reach iCloud contacts: {0}")]
    Transient(String),
    /// Anything else unexpected from the server.
    #[error("iCloud contacts: {0}")]
    Protocol(String),
}

type Result<T> = std::result::Result<T, CardDavError>;

/// One card in an address book listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The card's path, percent-decoded: the key it is known by.
    pub key: String,
    /// The `href` as the server wrote it (sent back in a multiget).
    pub href: String,
    pub etag: String,
}

/// A fetched card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub key: String,
    pub etag: String,
    pub vcard: String,
}

pub struct Client {
    http: reqwest::Client,
    creds: Credentials,
    base: Url,
}

impl Client {
    /// `base`: where discovery starts ([`ICLOUD_CONTACTS`]).
    pub fn new(creds: Credentials, base: &str) -> Result<Client> {
        let base = Url::parse(base).map_err(|e| CardDavError::Protocol(format!("bad base URL: {e}")))?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("imessaged/", env!("CARGO_PKG_VERSION"), " (CardDAV)"))
            .build()
            .map_err(|e| CardDavError::Transient(format!("HTTP client: {e}")))?;
        Ok(Client { http, creds, base })
    }

    /// Whether the password may be sent to `url` (see the module docs).
    pub fn allowed(&self, url: &Url) -> bool {
        if url.origin() == self.base.origin() {
            return true;
        }
        url.scheme() == "https" && url.host_str().is_some_and(|h| h == "icloud.com" || h.ends_with(".icloud.com"))
    }

    /// One request with Basic auth, following redirects to allowed URLs.
    /// -> (the URL that answered, body). `max`: body size cap.
    async fn request(&self, method: Method, url: &Url, depth: Option<&str>, body: Option<String>, max: usize) -> Result<(Url, Vec<u8>, Option<String>)> {
        let mut url = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            if !self.allowed(&url) {
                return Err(CardDavError::Protocol(format!("refusing to send the password to {}", host_of(&url))));
            }
            let mut req = self.http.request(method.clone(), url.clone()).basic_auth(&self.creds.apple_id, Some(self.creds.password.as_str()));
            if let Some(d) = depth {
                req = req.header("Depth", d);
            }
            if let Some(b) = &body {
                req = req.header("Content-Type", "application/xml; charset=utf-8").body(b.clone());
            }
            let mut resp = req.send().await.map_err(|e| CardDavError::Transient(net_error(&e)))?;
            let status = resp.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let loc = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
                url = url.join(loc).map_err(|e| CardDavError::Protocol(format!("bad redirect: {e}")))?;
                continue;
            }
            match status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(CardDavError::Auth),
                s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => {
                    return Err(CardDavError::Transient(format!("{} answered {s}", host_of(&url))));
                }
                s if !s.is_success() => return Err(CardDavError::Protocol(format!("{} answered {s} to {method}", host_of(&url)))),
                _ => {}
            }
            let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
            if resp.content_length().is_some_and(|n| n as usize > max) {
                return Err(CardDavError::Protocol(format!("reply from {} is larger than {} MiB", host_of(&url), max >> 20)));
            }
            let mut out = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| CardDavError::Transient(net_error(&e)))? {
                if out.len() + chunk.len() > max {
                    return Err(CardDavError::Protocol(format!("reply from {} is larger than {} MiB", host_of(&url), max >> 20)));
                }
                out.extend_from_slice(&chunk);
            }
            return Ok((url, out, ctype));
        }
        Err(CardDavError::Protocol("too many redirects".into()))
    }

    async fn propfind(&self, url: &Url, depth: &str, props: &str, max: usize) -> Result<(Url, Vec<Response>)> {
        let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:card="{CARDDAV}"><d:prop>{props}</d:prop></d:propfind>"#);
        let (at, raw, _) = self.request(Method::from_bytes(b"PROPFIND").expect("valid method"), url, Some(depth), Some(body), max).await?;
        Ok((at, multistatus(&raw)?))
    }

    /// The address books of the account (checks the credentials).
    pub async fn discover(&self) -> Result<Vec<Url>> {
        let (at, r) = self.propfind(&self.base, "0", "<d:current-user-principal/>", MAX_LISTING).await?;
        let principal = r
            .iter()
            .find_map(|r| r.prop(DAV, "current-user-principal").and_then(|p| p.child(DAV, "href")).map(|h| h.text.trim().to_string()))
            .filter(|h| !h.is_empty())
            .ok_or_else(|| CardDavError::Protocol("no current-user-principal".into()))?;
        let principal = join(&at, &principal)?;
        let (at, r) = self.propfind(&principal, "0", "<card:addressbook-home-set/>", MAX_LISTING).await?;
        let home = r
            .iter()
            .find_map(|r| r.prop(CARDDAV, "addressbook-home-set").and_then(|p| p.child(DAV, "href")).map(|h| h.text.trim().to_string()))
            .filter(|h| !h.is_empty())
            .ok_or_else(|| CardDavError::Protocol("no addressbook-home-set".into()))?;
        let home = join(&at, &home)?;
        if !self.allowed(&home) {
            return Err(CardDavError::Protocol(format!("the address book home is not on iCloud ({})", host_of(&home))));
        }
        let (at, r) = self.propfind(&home, "1", "<d:resourcetype/>", MAX_LISTING).await?;
        let mut books = vec![];
        for resp in &r {
            if resp.prop(DAV, "resourcetype").is_some_and(|t| t.child(CARDDAV, "addressbook").is_some()) {
                let u = join(&at, &resp.href)?;
                if self.allowed(&u) && !books.contains(&u) {
                    books.push(u);
                }
            }
        }
        if books.is_empty() {
            return Err(CardDavError::Protocol("the account has no address book".into()));
        }
        Ok(books)
    }

    /// Every card (not sub-collection) in `book` with its ETag.
    pub async fn list(&self, book: &Url) -> Result<Vec<Entry>> {
        let (at, r) = self.propfind(book, "1", "<d:getetag/><d:resourcetype/>", MAX_LISTING).await?;
        let own = decode_path(at.path());
        let mut out = vec![];
        for resp in r {
            let Ok(u) = join(&at, &resp.href) else { continue };
            let key = decode_path(u.path());
            if key.trim_end_matches('/') == own.trim_end_matches('/') || resp.prop(DAV, "resourcetype").is_some_and(|t| t.child(DAV, "collection").is_some()) {
                continue;
            }
            let Some(etag) = resp.prop(DAV, "getetag").map(|e| e.text.trim().to_string()).filter(|e| !e.is_empty()) else { continue };
            out.push(Entry { key, href: resp.href, etag });
        }
        Ok(out)
    }

    /// The cards behind `entries` (one batch; at most [`BATCH`]). Cards
    /// the server no longer has are simply missing from the result.
    pub async fn multiget(&self, book: &Url, entries: &[&Entry]) -> Result<Vec<Card>> {
        let hrefs: String = entries.iter().map(|e| format!("<d:href>{}</d:href>", quick_xml::escape::escape(e.href.as_str()))).collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><card:addressbook-multiget xmlns:d="DAV:" xmlns:card="{CARDDAV}"><d:prop><d:getetag/><card:address-data/></d:prop>{hrefs}</card:addressbook-multiget>"#
        );
        let (at, raw, _) = self.request(Method::from_bytes(b"REPORT").expect("valid method"), book, Some("1"), Some(body), MAX_BATCH).await?;
        let mut out = vec![];
        for resp in multistatus(&raw)? {
            let Ok(u) = join(&at, &resp.href) else { continue };
            let (Some(etag), Some(data)) = (resp.prop(DAV, "getetag"), resp.prop(CARDDAV, "address-data")) else { continue };
            if data.text.trim().is_empty() {
                continue;
            }
            out.push(Card { key: decode_path(u.path()), etag: etag.text.trim().to_string(), vcard: data.text.clone() });
        }
        Ok(out)
    }

    /// A photo a card links to (`PHOTO;VALUE=uri:https://…`), with its
    /// content type.
    pub async fn photo(&self, url: &str) -> Result<(Vec<u8>, Option<String>)> {
        let u = Url::parse(url).map_err(|e| CardDavError::Protocol(format!("bad photo URL: {e}")))?;
        let (_, body, ctype) = self.request(Method::GET, &u, None, None, MAX_PHOTO).await?;
        Ok((body, ctype))
    }
}

fn host_of(u: &Url) -> String {
    u.host_str().unwrap_or("?").to_string()
}

/// reqwest's error without the URL (it can carry the account id).
fn net_error(e: &reqwest::Error) -> String {
    let what = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "reply cut short"
    } else {
        "request failed"
    };
    let mut src = std::error::Error::source(e);
    let mut detail = String::new();
    while let Some(s) = src {
        detail = s.to_string();
        src = s.source();
    }
    if detail.is_empty() { what.to_string() } else { format!("{what} ({detail})") }
}

fn join(at: &Url, href: &str) -> Result<Url> {
    at.join(href).map_err(|e| CardDavError::Protocol(format!("bad href {href:?}: {e}")))
}

/// `%XX` decoded (invalid escapes kept as they are).
pub fn decode_path(p: &str) -> String {
    let b = p.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// WebDAV XML
// ---------------------------------------------------------------------------

/// A namespaced element with its children and text (enough for multistatus).
#[derive(Debug, Default, Clone)]
pub struct El {
    pub ns: String,
    pub name: String,
    pub children: Vec<El>,
    pub text: String,
}

impl El {
    pub fn child(&self, ns: &str, name: &str) -> Option<&El> {
        self.children.iter().find(|c| c.ns == ns && c.name == name)
    }
    fn all<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a El> + 'a {
        self.children.iter().filter(move |c| c.ns == ns && c.name == name)
    }
}

/// Parse an XML document into its root element.
pub fn parse_xml(raw: &[u8]) -> Result<El> {
    let bad = |e: &dyn fmt::Display| CardDavError::Protocol(format!("bad XML reply: {e}"));
    let mut r = quick_xml::reader::NsReader::from_reader(raw);
    r.config_mut().expand_empty_elements = true;
    let mut stack: Vec<El> = vec![El::default()];
    loop {
        let (ns, ev) = r.read_resolved_event().map_err(|e| bad(&e))?;
        match ev {
            Event::Start(s) => {
                let ns = match ns {
                    ResolveResult::Bound(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
                    _ => String::new(),
                };
                let name = String::from_utf8_lossy(s.local_name().as_ref()).into_owned();
                if stack.len() > 64 {
                    return Err(bad(&"nested too deeply"));
                }
                stack.push(El { ns, name, ..Default::default() });
            }
            Event::End(_) => {
                let el = stack.pop().ok_or_else(|| bad(&"unbalanced"))?;
                stack.last_mut().ok_or_else(|| bad(&"unbalanced"))?.children.push(el);
            }
            Event::Text(t) => {
                let s = t.decode().map_err(|e| bad(&e))?;
                stack.last_mut().expect("root").text.push_str(&s);
            }
            Event::CData(t) => {
                let s = t.decode().map_err(|e| bad(&e))?;
                stack.last_mut().expect("root").text.push_str(&s);
            }
            Event::GeneralRef(g) => {
                let top = stack.last_mut().expect("root");
                if let Some(c) = g.resolve_char_ref().map_err(|e| bad(&e))? {
                    top.text.push(c);
                } else {
                    let name = g.decode().map_err(|e| bad(&e))?;
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(v) => top.text.push_str(v),
                        None => return Err(bad(&format!("unknown entity &{name};"))),
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(bad(&"unexpected end"));
    }
    let mut doc = stack.pop().expect("root");
    doc.children.pop().ok_or_else(|| bad(&"empty document"))
}

/// One `<d:response>`: its href and the properties of its 2xx propstats.
#[derive(Debug, Clone)]
pub struct Response {
    pub href: String,
    pub props: Vec<El>,
}

impl Response {
    pub fn prop(&self, ns: &str, name: &str) -> Option<&El> {
        self.props.iter().find(|p| p.ns == ns && p.name == name)
    }
}

/// The responses of a `207 Multi-Status` body.
pub fn multistatus(raw: &[u8]) -> Result<Vec<Response>> {
    let root = parse_xml(raw)?;
    if root.ns != DAV || root.name != "multistatus" {
        return Err(CardDavError::Protocol(format!("expected a multistatus reply, got <{}>", root.name)));
    }
    let ok = |s: Option<&El>| s.is_none_or(|s| s.text.split_whitespace().nth(1).is_some_and(|c| c.starts_with('2')));
    let mut out = vec![];
    for r in root.all(DAV, "response") {
        let Some(href) = r.child(DAV, "href").map(|h| h.text.trim().to_string()) else { continue };
        let props = r
            .all(DAV, "propstat")
            .filter(|ps| ok(ps.child(DAV, "status")))
            .filter_map(|ps| ps.child(DAV, "prop"))
            .flat_map(|p| p.children.iter().cloned())
            .collect();
        out.push(Response { href, props });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// vCards
// ---------------------------------------------------------------------------

/// Unfolded content lines (RFC 6350 3.2), line breaks of any kind.
fn unfold(v: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for line in v.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t'))
            && let Some(last) = out.last_mut()
        {
            last.push_str(rest);
            continue;
        }
        out.push(line.to_string());
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// The property name of a content line (group prefix dropped), upper case,
/// and its parameters.
fn prop_of(line: &str) -> (String, &str) {
    let key = line.split_once(':').map(|(k, _)| k).unwrap_or(line);
    let (name, params) = key.split_once(';').unwrap_or((key, ""));
    (name.rsplit('.').next().unwrap_or("").to_ascii_uppercase(), params)
}

/// An Apple contact group card (`X-ADDRESSBOOKSERVER-KIND:group`), not a person.
pub fn is_group(vcard: &str) -> bool {
    unfold(vcard).iter().any(|l| {
        let (p, _) = prop_of(l);
        (p == "X-ADDRESSBOOKSERVER-KIND" || p == "KIND") && l.split_once(':').is_some_and(|(_, v)| v.trim().eq_ignore_ascii_case("group"))
    })
}

/// Whether the card has a `PHOTO`.
pub fn has_photo(vcard: &str) -> bool {
    unfold(vcard).iter().any(|l| prop_of(l).0 == "PHOTO")
}

/// The URL of a linked (not embedded) photo, if the card has one.
pub fn photo_url(vcard: &str) -> Option<String> {
    unfold(vcard).iter().find_map(|l| {
        let (p, params) = prop_of(l);
        if p != "PHOTO" {
            return None;
        }
        let v = l.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
        let uri = params.split(';').any(|x| x.eq_ignore_ascii_case("VALUE=uri") || x.eq_ignore_ascii_case("VALUE=url"));
        // A `data:` URI is embedded already (vCard 4), nothing to fetch.
        ((uri && !v.starts_with("data:")) || v.starts_with("https://") || v.starts_with("http://")).then(|| v.to_string())
    })
}

/// `line` without its vCard property group (`item1.TEL;…` -> `TEL;…`).
/// A group is letters, digits and `-` up to the first `.` of the name.
fn ungroup_line(line: &str) -> &str {
    let end = line.find([':', ';']).unwrap_or(line.len());
    match line[..end].find('.') {
        Some(dot) if dot > 0 && line[..dot].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => &line[dot + 1..],
        _ => line,
    }
}

/// The card with every property group removed (`item1.TEL` -> `TEL`,
/// `item1.X-ABLabel` -> `X-ABLabel`), refolded. Apple writes custom
/// labels as groups, and KContacts, which KPeople reads the vCard folder
/// with, ignores grouped properties altogether: without this, kmessages
/// would not know a number or email labelled "Work cell" or "School".
/// Values are left alone; the Rust parsers here read both forms.
pub fn ungroup(vcard: &str) -> String {
    let mut out = String::with_capacity(vcard.len());
    for l in unfold(vcard) {
        fold_into(&mut out, ungroup_line(&l));
    }
    out
}

/// The card with its linked photo replaced by `photo` embedded (base64,
/// vCard 3.0 style, which KPeople reads), or dropped when `None`.
pub fn embed_photo(vcard: &str, photo: Option<(&[u8], &str)>) -> String {
    let mut out = String::with_capacity(vcard.len() + photo.map_or(0, |p| p.0.len() * 4 / 3 + 64));
    for l in unfold(vcard) {
        let (p, _) = prop_of(&l);
        if p == "PHOTO" {
            if let Some((bytes, kind)) = photo {
                let line = format!("PHOTO;ENCODING=b;TYPE={kind}:{}", base64::engine::general_purpose::STANDARD.encode(bytes));
                fold_into(&mut out, &line);
            }
            continue;
        }
        fold_into(&mut out, &l);
    }
    out
}

/// `line` folded at 75 octets (on char boundaries), CRLF-terminated.
fn fold_into(out: &mut String, line: &str) {
    let mut width = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if width + n > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += n;
    }
    out.push_str("\r\n");
}

/// `JPEG` / `PNG` / `GIF` / `HEIC` from the bytes, else from the content type.
pub fn image_kind(bytes: &[u8], ctype: Option<&str>) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("JPEG");
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("PNG");
    }
    if bytes.starts_with(b"GIF8") {
        return Some("GIF");
    }
    if bytes.len() > 12 && &bytes[4..8] == b"ftyp" && matches!(&bytes[8..12], b"heic" | b"heix" | b"mif1") {
        return Some("HEIC");
    }
    match ctype.map(|c| c.split(';').next().unwrap_or("").trim().to_ascii_lowercase()) {
        Some(c) if c == "image/jpeg" => Some("JPEG"),
        Some(c) if c == "image/png" => Some("PNG"),
        _ => None,
    }
}

/// The local file name for the card known as `key`: `icloud-<hash>.vcf`
/// (stable, safe on every file system).
pub fn file_name(key: &str) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(key.as_bytes());
    let hex: String = h[..10].iter().map(|b| format!("{b:02x}")).collect();
    format!("icloud-{hex}.vcf")
}

/// `icloud-<20 hex>.vcf`, as [`file_name`] makes them.
pub fn is_synced_name(n: &str) -> bool {
    n.strip_prefix("icloud-").and_then(|r| r.strip_suffix(".vcf")).is_some_and(|h| h.len() == 20 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multistatus_parses() {
        let xml = br#"<?xml version="1.0"?>
<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav">
  <response><href>/1/carddav/card/</href><propstat><prop><resourcetype><collection/><C:addressbook/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat></response>
  <response><href>/1/carddav/card/a%20b.vcf</href>
    <propstat><prop><getetag>"e1"</getetag><C:address-data>BEGIN:VCARD&#13;
FN:A &amp; B&#13;
END:VCARD</C:address-data></prop><status>HTTP/1.1 200 OK</status></propstat>
    <propstat><prop><displayname/></prop><status>HTTP/1.1 404 Not Found</status></propstat>
  </response>
</multistatus>"#;
        let r = multistatus(xml).unwrap();
        assert_eq!(r.len(), 2);
        assert!(r[0].prop(DAV, "resourcetype").unwrap().child(CARDDAV, "addressbook").is_some());
        assert_eq!(r[1].prop(DAV, "getetag").unwrap().text, "\"e1\"");
        assert_eq!(r[1].prop(CARDDAV, "address-data").unwrap().text, "BEGIN:VCARD\r\nFN:A & B\r\nEND:VCARD");
        assert!(r[1].prop(DAV, "displayname").is_none(), "404 propstats are dropped");
        assert!(multistatus(b"<html/>").is_err());
        assert!(multistatus(b"<d:multistatus xmlns:d=\"DAV:\"><d:response>").is_err());
        assert_eq!(decode_path("/1/card/a%20b%2x.vcf"), "/1/card/a b%2x.vcf");
        assert_eq!(decode_path("/x%4"), "/x%4");
    }

    #[test]
    fn vcard_helpers() {
        let linked = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Pat\r\nitem1.PHOTO;VALUE=uri:https://p1-contacts.icloud.com/\r\n x/photo\r\nEND:VCARD\r\n";
        assert_eq!(photo_url(linked).as_deref(), Some("https://p1-contacts.icloud.com/x/photo"));
        assert!(has_photo(linked));
        let embedded = embed_photo(linked, Some((&[0xff, 0xd8, 0xff, 0xe0], "JPEG")));
        assert!(embedded.contains("PHOTO;ENCODING=b;TYPE=JPEG:/9j/4A==\r\n"), "{embedded}");
        assert!(photo_url(&embedded).is_none());
        let dropped = embed_photo(linked, None);
        assert!(!has_photo(&dropped) && dropped.contains("FN:Pat\r\n"), "{dropped}");
        assert!(photo_url("BEGIN:VCARD\r\nPHOTO;ENCODING=b;TYPE=JPEG:/9j/\r\nEND:VCARD").is_none());
        assert!(is_group("BEGIN:VCARD\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nEND:VCARD"));
        assert!(!is_group(linked));
        // Apple's custom labels are property groups, which KContacts drops; the files lose them.
        let grouped = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Pat\r\nitem1.TEL;type=pref:+1 555-000-1212\r\nitem1.X-ABLabel:Work Cell\r\n\
                       item2.EMAIL;type=INTERNET:pat@example.com\r\nNOTE:see item1.TEL; a.b:c\r\nitem3.PHOTO;VALUE=uri:https://p1-contacts.icloud.com/x/\r\n photo\r\nEND:VCARD\r\n";
        let u = ungroup(grouped);
        assert_eq!(
            u,
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Pat\r\nTEL;type=pref:+1 555-000-1212\r\nX-ABLabel:Work Cell\r\nEMAIL;type=INTERNET:pat@example.com\r\n\
             NOTE:see item1.TEL; a.b:c\r\nPHOTO;VALUE=uri:https://p1-contacts.icloud.com/x/photo\r\nEND:VCARD\r\n"
        );
        assert_eq!(photo_url(&u).as_deref(), Some("https://p1-contacts.icloud.com/x/photo"));
        assert_eq!(ungroup(&u), u, "idempotent");
        assert_eq!(ungroup_line("a.b.C:d"), "b.C:d", "only the first dot is the group");
        assert_eq!(ungroup_line("bad group.TEL:1"), "bad group.TEL:1", "a space is not a group");
        let long = format!("NOTE:{}", "é".repeat(60));
        let mut f = String::new();
        fold_into(&mut f, &long);
        assert!(f.split("\r\n").all(|l| l.len() <= 75), "{f}");
        assert_eq!(unfold(&f).concat(), long);
        assert_eq!(image_kind(b"\x89PNG\r\n\x1a\nxxxx", None), Some("PNG"));
        assert_eq!(image_kind(b"????", Some("image/jpeg; q=1")), Some("JPEG"));
        assert_eq!(image_kind(b"<html>", Some("text/html")), None);
        let n = file_name("/1/carddav/card/x.vcf");
        assert!(is_synced_name(&n), "{n}");
        assert_ne!(n, file_name("/1/carddav/card/y.vcf"));
        assert!(!is_synced_name("icloud-x.vcf") && !is_synced_name("mac-1.vcf"));
    }

    #[test]
    fn password_only_goes_to_icloud() {
        let c = Client::new(Credentials { apple_id: "a@b.c".into(), password: Zeroizing::new("p".into()) }, "http://127.0.0.1:9/").unwrap();
        let ok = |u: &str| c.allowed(&Url::parse(u).unwrap());
        assert!(ok("https://p42-contacts.icloud.com:443/1/carddav/"));
        assert!(ok("https://icloud.com/"));
        assert!(ok("http://127.0.0.1:9/x"), "the base's own origin");
        assert!(!ok("http://p42-contacts.icloud.com/"), "never in clear text");
        assert!(!ok("https://evil-icloud.com/"));
        assert!(!ok("https://icloud.com.evil.test/"));
        assert!(!ok("http://127.0.0.1:10/"));
        assert!(!format!("{:?}", c.creds).contains("\"p\""));
    }
}
