//! Link previews for messages we send (Messages' URL balloon; see
//! docs/DAEMON.md "Link previews").
//!
//! Messages never lets the receiver fetch anything: the *sender* fetches the
//! page's title, summary, site icon and hero image and sends them with the
//! text. [`fetch`] does that for the first `http(s)` URL of an outgoing
//! iMessage, under a hard deadline; any failure means the text goes out
//! alone. The HTML is read with a small tolerant scanner over the `<head>`
//! (Open Graph, Twitter cards, `<title>`, `<meta name=description>`,
//! `<link rel=icon|apple-touch-icon>`), not a full HTML parser.
//!
//! Images are not re-encoded (the daemon has no image codec): a JPEG or PNG
//! of at most [`FetchOptions::max_image_bytes`] is sent as is, anything
//! else is left out. Icons must also be at most [`FetchOptions::max_icon_px`]
//! pixels on each side.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use url::Url;

use crate::engine::{LinkImage, LinkPreview};

/// Longest title / summary we send (characters).
const MAX_TITLE_CHARS: usize = 300;
const MAX_SUMMARY_CHARS: usize = 1000;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Everything (page, image, icon) must finish within this; the image
    /// and icon are dropped if the page took most of it.
    pub deadline: Duration,
    pub max_redirects: usize,
    /// HTML read at most (the `<head>` is normally in the first few KB).
    pub max_html_bytes: usize,
    /// An `og:image` larger than this is not downloaded.
    pub max_image_download: usize,
    /// Largest image / icon we send as is.
    pub max_image_bytes: usize,
    /// Largest icon side, in pixels.
    pub max_icon_px: u32,
    /// Largest image side, in pixels (we cannot scale it down).
    pub max_image_px: u32,
    pub user_agent: String,
    /// Fetch `localhost`, `*.local` and private / loopback addresses too
    /// (tests). Off by default: a preview of an internal page would send
    /// its title to the recipient. Applies to names too: every name is
    /// resolved by [`GuardedResolver`], which drops private addresses.
    pub allow_private_hosts: bool,
    /// Fixed answers for host names, consulted before DNS (tests); the
    /// private-address filter applies to them as to DNS answers.
    pub hosts: Vec<(String, IpAddr)>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        FetchOptions {
            deadline: Duration::from_secs(5),
            max_redirects: 3,
            max_html_bytes: 2 << 20,
            max_image_download: 5 << 20,
            max_image_bytes: 1 << 20,
            max_icon_px: 256,
            max_image_px: 4096,
            user_agent: format!("imessage-linux/{}", env!("CARGO_PKG_VERSION")),
            allow_private_hosts: false,
            hosts: vec![],
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PreviewError {
    #[error("not an http(s) URL")]
    BadUrl,
    #[error("private or local address")]
    PrivateHost,
    #[error("timed out")]
    Timeout,
    #[error("request failed: {0}")]
    Http(String),
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("not an HTML page ({0})")]
    NotHtml(String),
    #[error("the page has no title or image")]
    NoMetadata,
}

// ---------------------------------------------------------------------------
// URL in the message text
// ---------------------------------------------------------------------------

/// The first `http://` / `https://` URL in `text` (Messages previews only
/// the first), with trailing punctuation that is not part of it removed.
pub fn first_url(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while from < text.len() {
        let pos = [lower[from..].find("https://"), lower[from..].find("http://")].into_iter().flatten().min()? + from;
        from = pos + 7;
        // Must start a token ("xhttp://" is not a link).
        let prev = text[..pos].chars().next_back();
        if prev.is_some_and(|c| c.is_alphanumeric() || c == '/' || c == '@') {
            continue;
        }
        let end = text[pos..].find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`' | '\u{FFFC}')).map_or(text.len(), |e| pos + e);
        let mut cand = &text[pos..end];
        loop {
            let trimmed = cand.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '*']);
            let trimmed = match trimmed.chars().next_back() {
                Some(c @ (')' | ']' | '}')) => {
                    let open = match c {
                        ')' => '(',
                        ']' => '[',
                        _ => '{',
                    };
                    if trimmed.matches(c).count() > trimmed.matches(open).count() { &trimmed[..trimmed.len() - 1] } else { trimmed }
                }
                _ => trimmed,
            };
            if trimmed.len() == cand.len() {
                break;
            }
            cand = trimmed;
        }
        if let Ok(u) = Url::parse(cand)
            && matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some_and(|h| !h.is_empty())
        {
            return Some(cand.to_string());
        }
    }
    None
}

/// `localhost`, `*.local`, `*.localhost`, `*.internal` and IP literals that
/// are loopback, private, link-local, unspecified or unique-local.
pub fn is_private_host(u: &Url) -> bool {
    match u.host() {
        None => true,
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local") || d.ends_with(".internal") || !d.contains('.')
        }
        Some(url::Host::Ipv4(ip)) => private_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => private_ip(IpAddr::V6(ip)),
    }
}

/// Loopback, private, link-local, unspecified, "this network" (0/8),
/// CGNAT, broadcast, multicast, unique-local, site-local, and IPv4
/// addresses in IPv4-mapped / -compatible IPv6 form.
pub fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback() || v.is_private() || v.is_link_local() || v.is_unspecified() || v.is_broadcast() || v.is_multicast() || o[0] == 0 || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4() {
                return private_ip(IpAddr::V4(v4));
            }
            let s = v.segments()[0];
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80 || (s & 0xffc0) == 0xfec0
        }
    }
}

/// The resolver refused every address of a name (all private).
#[derive(Debug, thiserror::Error)]
#[error("{0} resolves only to private addresses")]
pub struct BlockedAddress(String);

/// DNS for link previews: `tokio::net::lookup_host` (or
/// [`FetchOptions::hosts`]), minus private addresses unless allowed. reqwest
/// connects only to what this returns, so a name pointing at an internal
/// address (split horizon, `metadata.internal`, DNS rebinding) is never
/// reached: not for the page, a redirect hop, the image or the icon.
pub struct GuardedResolver {
    allow_private: bool,
    hosts: Vec<(String, IpAddr)>,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: hyper::client::connect::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().trim_end_matches('.').to_ascii_lowercase();
        let fixed: Vec<IpAddr> = self.hosts.iter().filter(|(h, _)| h.eq_ignore_ascii_case(&name)).map(|(_, ip)| *ip).collect();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = if fixed.is_empty() {
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect()
            } else {
                fixed.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect()
            };
            let ok: Vec<SocketAddr> = addrs.into_iter().filter(|a| allow_private || !private_ip(a.ip())).collect();
            if ok.is_empty() {
                return Err(Box::new(BlockedAddress(name)) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

// ---------------------------------------------------------------------------
// HTML head scanner
// ---------------------------------------------------------------------------

/// What the `<head>` says about the page.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PageMeta {
    /// `<title>` text (entities decoded, whitespace collapsed).
    pub title: Option<String>,
    /// `<meta>` key (`property`, else `name`, else `itemprop`; lower-cased)
    /// and `content`, in document order.
    pub metas: Vec<(String, String)>,
    pub icons: Vec<IconLink>,
    /// `<base href>`.
    pub base: Option<String>,
    /// `<meta charset>` or the charset of `<meta http-equiv=content-type>`.
    pub charset: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IconLink {
    /// `rel` tokens, lower-cased.
    pub rel: Vec<String>,
    pub href: String,
    pub mime: Option<String>,
    /// Largest side from `sizes` (`180x180`), if given.
    pub size: Option<u32>,
}

impl PageMeta {
    /// The first non-empty `<meta>` value for any of `keys` (in key order).
    pub fn meta(&self, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|k| self.metas.iter().find(|(mk, v)| mk == k && !v.trim().is_empty()).map(|(_, v)| collapse(v)))
    }

    pub fn best_title(&self) -> Option<String> {
        self.meta(&["og:title", "twitter:title"]).or_else(|| self.title.clone()).filter(|t| !t.is_empty())
    }

    pub fn best_summary(&self) -> Option<String> {
        self.meta(&["og:description", "twitter:description", "description"])
    }

    pub fn best_image(&self) -> Option<String> {
        self.meta(&["og:image:secure_url", "og:image", "og:image:url", "twitter:image", "twitter:image:src"])
    }

    pub fn site_name(&self) -> Option<String> {
        self.meta(&["og:site_name", "application-name"])
    }

    /// Icon candidates, best first: `apple-touch-icon(-precomposed)`, then
    /// PNG / JPEG `icon`s (largest declared size up to `max_px` first).
    /// Never `.ico` / SVG (we cannot send those).
    pub fn icon_candidates(&self, max_px: u32) -> Vec<String> {
        let ok_type = |i: &IconLink| {
            let path = i.href.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
            match i.mime.as_deref() {
                Some(m) => matches!(m, "image/png" | "image/jpeg" | "image/jpg"),
                None => path.ends_with(".png") || path.ends_with(".jpg") || path.ends_with(".jpeg"),
            }
        };
        let fits = |i: &IconLink| i.size.is_none_or(|s| s <= max_px);
        let mut apple: Vec<&IconLink> =
            self.icons.iter().filter(|i| i.rel.iter().any(|r| r == "apple-touch-icon" || r == "apple-touch-icon-precomposed") && fits(i)).collect();
        apple.sort_by_key(|i| std::cmp::Reverse(i.size.unwrap_or(0)));
        let mut plain: Vec<&IconLink> = self.icons.iter().filter(|i| i.rel.iter().any(|r| r == "icon") && ok_type(i) && fits(i)).collect();
        plain.sort_by_key(|i| std::cmp::Reverse(i.size.unwrap_or(0)));
        let mut out: Vec<String> = vec![];
        for i in apple.into_iter().chain(plain) {
            if !out.contains(&i.href) {
                out.push(i.href.clone());
            }
        }
        out
    }
}

/// Collapse runs of whitespace to one space and trim.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap_chars(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

/// Decode HTML character references: numeric ones and the named ones that
/// show up in titles. Unknown names are left as written.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|e| e + 1).unwrap_or(rest.len());
        let name = &rest[1..end];
        let has_semi = rest[end..].starts_with(';');
        let decoded = if let Some(num) = name.strip_prefix('#') {
            let n = match num.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => num.parse::<u32>().ok(),
            };
            n.map(|n| char::from_u32(n).filter(|c| *c != '\0').unwrap_or('\u{FFFD}').to_string())
        } else {
            named_entity(name).map(str::to_string)
        };
        match decoded {
            Some(d) if !name.is_empty() => {
                out.push_str(&d);
                rest = &rest[end + usize::from(has_semi)..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "ndash" => "–",
        "mdash" => "—",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "middot" => "·",
        "bull" => "•",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "eacute" => "é",
        "egrave" => "è",
        "aacute" => "á",
        "agrave" => "à",
        "uuml" => "ü",
        "ouml" => "ö",
        "auml" => "ä",
        "szlig" => "ß",
        "ccedil" => "ç",
        "ntilde" => "ñ",
        "euro" => "€",
        "pound" => "£",
        "times" => "×",
        "verbar" | "vert" => "|",
        _ => return None,
    })
}

struct Tag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
}

impl Tag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Parse the tag starting at `s[start] == '<'`; returns it and the index
/// just past its `>` (or the end of input for an unterminated tag).
fn parse_tag(s: &str, start: usize) -> Option<(Tag, usize)> {
    let b = s.as_bytes();
    let mut j = start + 1;
    let closing = b.get(j) == Some(&b'/');
    if closing {
        j += 1;
    }
    let name_start = j;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-' || b[j] == b':') {
        j += 1;
    }
    if j == name_start || !b[name_start].is_ascii_alphabetic() {
        return None;
    }
    let name = s[name_start..j].to_ascii_lowercase();
    let mut attrs = vec![];
    loop {
        while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'/') {
            j += 1;
        }
        if j >= b.len() {
            return Some((Tag { name, closing, attrs }, b.len()));
        }
        if b[j] == b'>' {
            return Some((Tag { name, closing, attrs }, j + 1));
        }
        let an = j;
        while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'=' && b[j] != b'>' && !(b[j] == b'/' && b.get(j + 1) == Some(&b'>')) {
            j += 1;
        }
        let key = s[an..j].to_ascii_lowercase();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut value = String::new();
        if b.get(j) == Some(&b'=') {
            j += 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            match b.get(j) {
                Some(&q @ (b'"' | b'\'')) => {
                    let vs = j + 1;
                    let ve = s[vs..].find(q as char).map_or(b.len(), |e| vs + e);
                    value = s[vs..ve].to_string();
                    j = (ve + 1).min(b.len());
                }
                _ => {
                    let vs = j;
                    while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                        j += 1;
                    }
                    value = s[vs..j].to_string();
                }
            }
        }
        if !key.is_empty() {
            attrs.push((key, decode_entities(&value)));
        }
    }
}

/// Largest side in a `sizes` attribute (`"32x32 180x180"`; `any` = none).
fn icon_size(sizes: &str) -> Option<u32> {
    sizes
        .split_whitespace()
        .filter_map(|s| {
            let (w, h) = s.to_ascii_lowercase().split_once('x').map(|(w, h)| (w.to_string(), h.to_string()))?;
            Some(w.parse::<u32>().ok()?.max(h.parse::<u32>().ok()?))
        })
        .max()
}

/// Scan the document's head: `<title>`, `<meta>`, `<link rel=…icon…>`,
/// `<base>`. Stops at `</head>` or `<body>`; skips comments, scripts and
/// styles. Tolerates unquoted or single-quoted attributes, tags in any
/// case, missing `</head>`, and metadata before or after the title.
pub fn parse_head(html: &str) -> PageMeta {
    let lower = html.to_ascii_lowercase();
    let mut meta = PageMeta::default();
    let mut i = 0;
    while let Some(off) = html[i..].find('<') {
        let start = i + off;
        let rest = &lower[start..];
        if rest.starts_with("<!--") {
            i = lower[start + 4..].find("-->").map_or(html.len(), |e| start + 4 + e + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = lower[start..].find('>').map_or(html.len(), |e| start + e + 1);
            continue;
        }
        let Some((tag, end)) = parse_tag(html, start) else {
            i = start + 1;
            continue;
        };
        i = end;
        if tag.closing {
            if tag.name == "head" {
                break;
            }
            continue;
        }
        match tag.name.as_str() {
            "body" => break,
            "title" => {
                let close = lower[end..].find("</title").map_or(html.len(), |e| end + e);
                if meta.title.is_none() {
                    let t = collapse(&decode_entities(&html[end..close]));
                    meta.title = (!t.is_empty()).then_some(t);
                }
                i = close;
            }
            "script" | "style" | "template" => {
                let close = lower[end..].find(&format!("</{}", tag.name)).map_or(html.len(), |e| end + e);
                i = close;
            }
            "meta" => {
                if meta.charset.is_none() {
                    meta.charset = tag.attr("charset").map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).or_else(|| {
                        tag.attr("http-equiv").filter(|h| h.trim().eq_ignore_ascii_case("content-type")).and_then(|_| tag.attr("content")).and_then(charset_param).map(str::to_string)
                    });
                }
                let key = tag.attr("property").or_else(|| tag.attr("name")).or_else(|| tag.attr("itemprop")).map(|k| k.trim().to_ascii_lowercase());
                if let (Some(k), Some(v)) = (key, tag.attr("content")) {
                    meta.metas.push((k, v.to_string()));
                }
            }
            "link" => {
                let rel: Vec<String> = tag.attr("rel").unwrap_or("").split_whitespace().map(str::to_ascii_lowercase).collect();
                if let Some(href) = tag.attr("href").map(str::trim).filter(|h| !h.is_empty())
                    && rel.iter().any(|r| r.contains("icon"))
                {
                    meta.icons.push(IconLink {
                        rel,
                        href: href.to_string(),
                        mime: tag.attr("type").map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()),
                        size: tag.attr("sizes").and_then(icon_size),
                    });
                }
            }
            "base" if meta.base.is_none() => {
                meta.base = tag.attr("href").map(|h| h.trim().to_string()).filter(|h| !h.is_empty());
            }
            _ => {}
        }
    }
    meta
}

// ---------------------------------------------------------------------------
// images
// ---------------------------------------------------------------------------

/// `(mime, width, height)` of a JPEG or PNG, from its header.
pub fn image_info(data: &[u8]) -> Option<(&'static str, u32, u32)> {
    if data.len() >= 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") && &data[12..16] == b"IHDR" {
        let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some(("image/png", w, h));
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let mut i = 2;
        while i + 4 <= data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            if marker == 0xFF {
                i += 1;
                continue;
            }
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                i += 2;
                continue;
            }
            if marker == 0xD9 || marker == 0xDA {
                return None;
            }
            let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                if i + 9 > data.len() {
                    return None;
                }
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return (w > 0 && h > 0).then_some(("image/jpeg", w, h));
            }
            if len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// fetching
// ---------------------------------------------------------------------------

fn client(opts: &FetchOptions) -> Result<reqwest::Client, PreviewError> {
    let resolver = Arc::new(GuardedResolver { allow_private: opts.allow_private_hosts, hosts: opts.hosts.clone() });
    let max = opts.max_redirects;
    let allow_private = opts.allow_private_hosts;
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > max {
            attempt.error("too many redirects")
        } else if !matches!(attempt.url().scheme(), "http" | "https") {
            attempt.error("redirect to a non-http URL")
        } else if !allow_private && is_private_host(attempt.url()) {
            attempt.error("redirect to a private address")
        } else {
            attempt.follow()
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .referer(false)
        .dns_resolver(resolver)
        .user_agent(opts.user_agent.clone())
        .timeout(opts.deadline)
        .build()
        .map_err(|e| PreviewError::Http(e.to_string()))
}

fn http_err(e: reqwest::Error) -> PreviewError {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(s) = src {
        if s.downcast_ref::<BlockedAddress>().is_some() {
            return PreviewError::PrivateHost;
        }
        src = s.source();
    }
    if e.is_timeout() {
        PreviewError::Timeout
    } else if e.is_redirect() {
        PreviewError::Http("redirect refused".into())
    } else {
        PreviewError::Http(e.without_url().to_string())
    }
}

/// Read the body, at most `max` bytes. `Ok(None)` = more than `max`
/// (announced or received). `stop_at` ends the read early once seen
/// (case-insensitive ASCII; for `</head>`).
async fn read_capped(resp: &mut reqwest::Response, max: usize, stop_at: Option<&[u8]>) -> Result<Option<Vec<u8>>, PreviewError> {
    if resp.content_length().is_some_and(|l| l > max as u64) && stop_at.is_none() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(http_err)? {
        let scan_from = buf.len().saturating_sub(16);
        let room = max - buf.len();
        if chunk.len() > room {
            if stop_at.is_some() {
                buf.extend_from_slice(&chunk[..room]);
                return Ok(Some(buf));
            }
            return Ok(None);
        }
        buf.extend_from_slice(&chunk);
        if let Some(pat) = stop_at
            && buf[scan_from..].windows(pat.len()).any(|w| w.eq_ignore_ascii_case(pat))
        {
            break;
        }
    }
    Ok(Some(buf))
}

/// Fetches link previews with one HTTP client (connection pool, guarded
/// resolver) for all sends.
pub struct Fetcher {
    opts: FetchOptions,
    client: reqwest::Client,
}

impl Fetcher {
    pub fn new(opts: FetchOptions) -> Result<Fetcher, PreviewError> {
        let client = client(&opts)?;
        Ok(Fetcher { opts, client })
    }

    /// See [`fetch`].
    pub async fn fetch(&self, original: &str) -> Result<LinkPreview, PreviewError> {
        fetch_with(&self.client, original, &self.opts).await
    }
}

/// Fetch the preview for `original` (a URL from the message text) within
/// `opts.deadline` (a one-off [`Fetcher`]).
pub async fn fetch(original: &str, opts: &FetchOptions) -> Result<LinkPreview, PreviewError> {
    Fetcher::new(opts.clone())?.fetch(original).await
}

async fn fetch_with(client: &reqwest::Client, original: &str, opts: &FetchOptions) -> Result<LinkPreview, PreviewError> {
    let deadline = Instant::now() + opts.deadline;
    let url = Url::parse(original).map_err(|_| PreviewError::BadUrl)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(PreviewError::BadUrl);
    }
    if !opts.allow_private_hosts && is_private_host(&url) {
        return Err(PreviewError::PrivateHost);
    }
    let (final_url, meta) = tokio::time::timeout_at(deadline, fetch_page(client, url, opts)).await.map_err(|_| PreviewError::Timeout)??;
    let base = meta.base.as_deref().and_then(|b| final_url.join(b).ok()).unwrap_or_else(|| final_url.clone());
    let resolve = |href: &str| base.join(href.trim()).ok().filter(|u| matches!(u.scheme(), "http" | "https") && (opts.allow_private_hosts || !is_private_host(u)));

    let title = meta.best_title().map(|t| cap_chars(t, MAX_TITLE_CHARS));
    let summary = meta.best_summary().map(|t| cap_chars(t, MAX_SUMMARY_CHARS));
    let image = match meta.best_image().as_deref().and_then(resolve) {
        Some(u) => fetch_image(client, u, opts, Some(opts.max_image_px), deadline).await,
        None => None,
    };
    let mut icon = None;
    for href in meta.icon_candidates(opts.max_icon_px).iter().take(2) {
        if let Some(u) = resolve(href) {
            icon = fetch_image(client, u, opts, Some(opts.max_icon_px), deadline).await;
            if icon.is_some() {
                break;
            }
        }
    }
    if title.is_none() && image.is_none() {
        return Err(PreviewError::NoMetadata);
    }
    Ok(LinkPreview {
        url: final_url.to_string(),
        original_url: original.to_string(),
        title,
        summary,
        site_name: meta.site_name().map(|s| cap_chars(s, MAX_TITLE_CHARS)),
        image,
        icon,
    })
}

async fn fetch_page(client: &reqwest::Client, url: Url, opts: &FetchOptions) -> Result<(Url, PageMeta), PreviewError> {
    let mut resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1")
        .send()
        .await
        .map_err(http_err)?;
    if !resp.status().is_success() {
        return Err(PreviewError::Status(resp.status().as_u16()));
    }
    let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let mime = ctype.split(';').next().unwrap_or("").trim().to_string();
    if mime != "text/html" && mime != "application/xhtml+xml" {
        return Err(PreviewError::NotHtml(if mime.is_empty() { "no content type".into() } else { mime }));
    }
    let final_url = resp.url().clone();
    let body = read_capped(&mut resp, opts.max_html_bytes, Some(b"</head>")).await?.unwrap_or_default();
    Ok((final_url, parse_head(&decode_html(&body, &ctype))))
}

/// The `charset=` of a Content-Type value or a `<meta>` `content`.
fn charset_param(s: &str) -> Option<&str> {
    let i = s.to_ascii_lowercase().find("charset=")?;
    let v = s[i + 8..].trim_start_matches(['"', '\'', ' ']);
    let end = v.find(['"', '\'', ';', ',', ' ', '>', '/']).unwrap_or(v.len());
    Some(&v[..end]).filter(|v| !v.is_empty())
}

/// Decode the page: a BOM, else the Content-Type's charset, else
/// `<meta charset>` / `<meta http-equiv=content-type>` in the first 4 KB,
/// else UTF-8 (invalid bytes replaced).
pub fn decode_html(body: &[u8], content_type: &str) -> String {
    let declared = charset_param(content_type).and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())).or_else(|| {
        let head = String::from_utf8_lossy(&body[..body.len().min(4096)]).into_owned();
        let m = parse_head(&head);
        m.charset.as_deref().and_then(|l| encoding_rs::Encoding::for_label(l.trim().as_bytes()))
    });
    let enc = declared.unwrap_or(encoding_rs::UTF_8);
    // A page cannot be UTF-16 once we read its <meta> as ASCII.
    let enc = if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE { encoding_rs::UTF_8 } else { enc };
    enc.decode(body).0.into_owned()
}

/// A JPEG / PNG we can send as is, or `None` (any failure, too big, not
/// JPEG / PNG, larger than `max_px` on a side, or out of time).
async fn fetch_image(client: &reqwest::Client, url: Url, opts: &FetchOptions, max_px: Option<u32>, deadline: Instant) -> Option<LinkImage> {
    let source_url = url.to_string();
    let work = async {
        let mut resp = client.get(url).header(reqwest::header::ACCEPT, "image/jpeg,image/png;q=0.9,image/*;q=0.5").send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        read_capped(&mut resp, opts.max_image_download, None).await.ok().flatten()
    };
    let data = match tokio::time::timeout_at(deadline, work).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            log::debug!("link preview: image not usable (fetch failed or too large)");
            return None;
        }
        Err(_) => {
            log::debug!("link preview: no time left for the image");
            return None;
        }
    };
    let (mime, width, height) = image_info(&data)?;
    if data.len() > opts.max_image_bytes || max_px.is_some_and(|m| width > m || height > m) {
        log::debug!("link preview: image too large to send as is ({} bytes, {width}x{height})", data.len());
        return None;
    }
    Some(LinkImage { data, mime: mime.to_string(), width, height, source_url })
}

#[cfg(test)]
mod tests;
