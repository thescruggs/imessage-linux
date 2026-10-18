use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use super::*;

// ---------------------------------------------------------------------------
// a canned HTTP server (std TcpListener, one thread per connection)
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Canned {
    Ok { ctype: &'static str, body: Vec<u8> },
    Redirect(&'static str),
    Slow(Duration),
    Status(u16),
    /// A complete response as written (headers and body).
    Raw(Vec<u8>),
}

/// Requests seen: (path, User-Agent).
type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

/// Serves `routes` (path -> response) on 127.0.0.1; returns the base URL
/// (`http://127.0.0.1:PORT`) and the request log.
fn serve(routes: Vec<(&'static str, Canned)>) -> (String, RequestLog) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let l2 = log.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            let routes = routes.clone();
            let log = l2.clone();
            std::thread::spawn(move || {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let req = String::from_utf8_lossy(&req).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let ua = req.lines().find_map(|l| l.strip_prefix("user-agent: ").or_else(|| l.strip_prefix("User-Agent: "))).unwrap_or("").to_string();
                log.lock().unwrap().push((path.clone(), ua));
                let resp = routes.iter().find(|(p, _)| *p == path).map(|(_, r)| r.clone()).unwrap_or(Canned::Status(404));
                let (head, body) = match resp {
                    Canned::Ok { ctype, body } => (format!("HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()), body),
                    Canned::Redirect(to) => (format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"), vec![]),
                    Canned::Slow(d) => {
                        std::thread::sleep(d);
                        ("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(), vec![])
                    }
                    Canned::Status(s) => (format!("HTTP/1.1 {s} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"), vec![]),
                    Canned::Raw(r) => (String::new(), r),
                };
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(&body);
            });
        }
    });
    (base, log)
}

/// A PNG header claiming `w`x`h` (the daemon never decodes images).
fn png(w: u32, h: u32, pad: usize) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    v.resize(v.len() + pad, 0);
    v
}

/// A JPEG with an APP0 segment and a baseline SOF0 of `w`x`h`.
fn jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    v.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&[0x03, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn html(s: &str) -> Canned {
    Canned::Ok { ctype: "text/html; charset=utf-8", body: s.as_bytes().to_vec() }
}

fn opts() -> FetchOptions {
    FetchOptions { allow_private_hosts: true, deadline: Duration::from_secs(5), ..Default::default() }
}

const OG_PAGE: &str = r#"<!doctype html><html><head>
<meta charset="utf-8">
<title>Fallback title</title>
<meta property="og:title" content="The &amp; Article &#8211; Part 1">
<meta property="og:description" content="  A  summary
   over lines ">
<meta property="og:site_name" content="Example News">
<meta property="og:image" content="/images/hero.png?x=1">
<link rel="icon" href="/favicon.ico">
<link rel="apple-touch-icon" sizes="180x180" href="touch.png">
</head><body><p>hi</p></body></html>"#;

#[tokio::test]
async fn open_graph_with_relative_image_and_icon() {
    let (base, log) = serve(vec![
        ("/a/page", html(OG_PAGE)),
        ("/images/hero.png?x=1", Canned::Ok { ctype: "image/png", body: png(1200, 630, 100) }),
        ("/a/touch.png", Canned::Ok { ctype: "image/png", body: png(180, 180, 10) }),
    ]);
    let url = format!("{base}/a/page");
    let p = fetch(&url, &opts()).await.unwrap();
    assert_eq!(p.url, url);
    assert_eq!(p.original_url, url);
    assert_eq!(p.title.as_deref(), Some("The & Article – Part 1"));
    assert_eq!(p.summary.as_deref(), Some("A summary over lines"));
    assert_eq!(p.site_name.as_deref(), Some("Example News"));
    let img = p.image.as_ref().unwrap();
    assert_eq!((img.mime.as_str(), img.width, img.height), ("image/png", 1200, 630));
    assert_eq!(img.source_url, format!("{base}/images/hero.png?x=1"));
    let icon = p.icon.as_ref().unwrap();
    assert_eq!((icon.width, icon.height), (180, 180));
    assert_eq!(icon.source_url, format!("{base}/a/touch.png"));
    // our User-Agent on every request; the .ico is never fetched
    let log = log.lock().unwrap();
    assert!(log.iter().all(|(_, ua)| ua.starts_with("imessage-linux/")), "{log:?}");
    assert!(!log.iter().any(|(p, _)| p.contains("favicon")), "{log:?}");
}

#[tokio::test]
async fn redirects_are_followed_up_to_three() {
    let (base, _) = serve(vec![
        ("/r1", Canned::Redirect("/r2")),
        ("/r2", Canned::Redirect("/r3")),
        ("/r3", Canned::Redirect("/final")),
        ("/final", html("<title>Landed</title>")),
        ("/r0", Canned::Redirect("/r1")),
    ]);
    let p = fetch(&format!("{base}/r1"), &opts()).await.unwrap();
    assert_eq!(p.title.as_deref(), Some("Landed"));
    assert_eq!(p.url, format!("{base}/final"));
    assert_eq!(p.original_url, format!("{base}/r1"));
    assert!(p.image.is_none() && p.icon.is_none());
    // four redirects: refused
    assert!(matches!(fetch(&format!("{base}/r0"), &opts()).await, Err(PreviewError::Http(_))));
}

#[tokio::test]
async fn slow_pages_time_out() {
    let (base, _) = serve(vec![("/slow", Canned::Slow(Duration::from_secs(3)))]);
    let o = FetchOptions { deadline: Duration::from_millis(400), ..opts() };
    let t = std::time::Instant::now();
    assert_eq!(fetch(&format!("{base}/slow"), &o).await, Err(PreviewError::Timeout));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[tokio::test]
async fn slow_image_is_dropped_but_the_page_kept() {
    let (base, _) = serve(vec![
        ("/", html(r#"<meta property="og:image" content="/slow.png"><title>T</title>"#)),
        ("/slow.png", Canned::Slow(Duration::from_secs(3))),
    ]);
    let o = FetchOptions { deadline: Duration::from_millis(600), ..opts() };
    let p = fetch(&format!("{base}/"), &o).await.unwrap();
    assert_eq!(p.title.as_deref(), Some("T"));
    assert!(p.image.is_none());
}

#[tokio::test]
async fn non_html_and_errors() {
    let (base, _) = serve(vec![
        ("/json", Canned::Ok { ctype: "application/json", body: b"{\"title\":1}".to_vec() }),
        ("/pic", Canned::Ok { ctype: "image/png", body: png(10, 10, 0) }),
        ("/gone", Canned::Status(410)),
        ("/empty", html("<html><head></head><body><h1>No title</h1></body></html>")),
    ]);
    assert_eq!(fetch(&format!("{base}/json"), &opts()).await, Err(PreviewError::NotHtml("application/json".into())));
    assert!(matches!(fetch(&format!("{base}/pic"), &opts()).await, Err(PreviewError::NotHtml(_))));
    assert_eq!(fetch(&format!("{base}/gone"), &opts()).await, Err(PreviewError::Status(410)));
    assert_eq!(fetch(&format!("{base}/empty"), &opts()).await, Err(PreviewError::NoMetadata));
    assert_eq!(fetch("ftp://example.com/x", &opts()).await, Err(PreviewError::BadUrl));
    // private addresses only when allowed
    let strict = FetchOptions { allow_private_hosts: false, ..opts() };
    assert_eq!(fetch(&format!("{base}/empty"), &strict).await, Err(PreviewError::PrivateHost));
}

#[tokio::test]
async fn oversize_page_and_images() {
    // The head lies beyond what we read: no metadata.
    let mut big = "<!-- ".to_string();
    big.push_str(&"x".repeat(10_000));
    big.push_str(" --><title>Too late</title>");
    // An image over the download cap, an icon too big to send, a non-image.
    let page = r#"<title>Big</title><meta property="og:image" content="/huge.png">
        <link rel="apple-touch-icon" href="/wide.png"><link rel="icon" type="image/png" href="/notimage.png">"#;
    let (base, _) = serve(vec![
        ("/big", html(&big)),
        ("/page", html(page)),
        ("/huge.png", Canned::Ok { ctype: "image/png", body: png(100, 100, 3 << 20) }),
        ("/wide.png", Canned::Ok { ctype: "image/png", body: png(512, 512, 0) }),
        ("/notimage.png", Canned::Ok { ctype: "image/png", body: b"<html>".to_vec() }),
        ("/fat.jpg", Canned::Ok { ctype: "image/jpeg", body: { let mut j = jpeg(800, 600); j.resize(1_500_000, 0); j } }),
        ("/fatpage", html(r#"<title>F</title><meta property="og:image" content="/fat.jpg">"#)),
    ]);
    let o = FetchOptions { max_html_bytes: 4096, ..opts() };
    assert_eq!(fetch(&format!("{base}/big"), &o).await, Err(PreviewError::NoMetadata));
    let o = FetchOptions { max_image_download: 1 << 20, ..opts() };
    let p = fetch(&format!("{base}/page"), &o).await.unwrap();
    assert_eq!(p.title.as_deref(), Some("Big"));
    assert!(p.image.is_none(), "over the download cap");
    assert!(p.icon.is_none(), "512 px icon / not an image");
    // downloaded, but over the 1 MB we send as is
    let p = fetch(&format!("{base}/fatpage"), &opts()).await.unwrap();
    assert!(p.image.is_none());
}

/// A name that resolves to a private address is refused although the
/// literal-host check passes: every connection (page, redirect hop, image,
/// icon) goes through [`GuardedResolver`]. With private hosts allowed the
/// same name works.
#[tokio::test]
async fn names_resolving_to_private_addresses_are_refused() {
    let (base, log) = serve(vec![("/page", html("<title>Internal wiki</title>")), ("/hop", Canned::Redirect("/page"))]);
    let port = base.rsplit(':').next().unwrap().to_string();
    let wiki = |ip: &str| ("wiki.corp.example.com".to_string(), ip.parse::<IpAddr>().unwrap());
    let strict = FetchOptions { allow_private_hosts: false, hosts: vec![wiki("127.0.0.1")], ..opts() };
    let url = format!("http://wiki.corp.example.com:{port}/page");
    assert!(!is_private_host(&Url::parse(&url).unwrap()), "the literal check alone lets it through");
    assert_eq!(fetch(&url, &strict).await, Err(PreviewError::PrivateHost));
    assert!(log.lock().unwrap().is_empty(), "never connected");
    // a mixed answer keeps only the public address (unreachable here)
    let mixed = FetchOptions { hosts: vec![wiki("127.0.0.1"), wiki("192.0.2.1")], deadline: Duration::from_millis(300), ..strict.clone() };
    assert!(fetch(&url, &mixed).await.is_err());
    assert!(log.lock().unwrap().is_empty(), "127.0.0.1 dropped from the answer");
    // allowed: the same name works, through a redirect
    let open = FetchOptions { allow_private_hosts: true, hosts: vec![wiki("127.0.0.1")], ..opts() };
    let p = fetch(&format!("http://wiki.corp.example.com:{port}/hop"), &open).await.unwrap();
    assert_eq!(p.title.as_deref(), Some("Internal wiki"));
    assert_eq!(p.url, url);
}

#[tokio::test]
async fn charsets_gzip_and_streamed_bodies() {
    // ISO-8859-1 declared in the header, and in a <meta> only
    let latin1 = |s: &str| s.chars().map(|c| c as u32 as u8).collect::<Vec<u8>>();
    let crc = |d: &[u8]| {
        let mut c = !0u32;
        for b in d {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    };
    // gzip with one stored deflate block
    let gz = |d: &[u8]| {
        let mut v = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff, 1];
        v.extend_from_slice(&(d.len() as u16).to_le_bytes());
        v.extend_from_slice(&(!(d.len() as u16)).to_le_bytes());
        v.extend_from_slice(d);
        v.extend_from_slice(&crc(d).to_le_bytes());
        v.extend_from_slice(&(d.len() as u32).to_le_bytes());
        v
    };
    let zipped = gz(b"<title>Zipped page</title>");
    let mut gzip_resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", zipped.len()).into_bytes();
    gzip_resp.extend_from_slice(&zipped);
    // chunked, no Content-Length: an image streamed past the cap
    let mut chunked = b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    let big = png(100, 100, 300_000);
    for c in big.chunks(50_000) {
        chunked.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
        chunked.extend_from_slice(c);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let (base, _) = serve(vec![
        ("/l1", Canned::Ok { ctype: "text/html; charset=ISO-8859-1", body: latin1("<title>Caf\u{e9} cr\u{e8}me</title>") }),
        ("/l2", Canned::Ok { ctype: "text/html", body: latin1("<meta http-equiv=Content-Type content='text/html; charset=iso-8859-1'><title>Gr\u{fc}\u{df}e</title>") }),
        ("/l3", Canned::Ok { ctype: "text/html", body: [&b"<meta charset=\"windows-1252\"><title>"[..], &[0x93, b'q', 0x94], b"</title>"].concat() }),
        ("/gz", Canned::Raw(gzip_resp)),
        ("/stream", html(r#"<title>S</title><meta property="og:image" content="/big.png">"#)),
        ("/big.png", Canned::Raw(chunked)),
        ("/huge-px", html(r#"<title>H</title><meta property="og:image" content="/wide.png">"#)),
        ("/wide.png", Canned::Ok { ctype: "image/png", body: png(8000, 100, 0) }),
    ]);
    assert_eq!(fetch(&format!("{base}/l1"), &opts()).await.unwrap().title.as_deref(), Some("Café crème"));
    assert_eq!(fetch(&format!("{base}/l2"), &opts()).await.unwrap().title.as_deref(), Some("Grüße"));
    assert_eq!(fetch(&format!("{base}/l3"), &opts()).await.unwrap().title.as_deref(), Some("“q”"));
    assert_eq!(fetch(&format!("{base}/gz"), &opts()).await.unwrap().title.as_deref(), Some("Zipped page"));
    let p = fetch(&format!("{base}/stream"), &FetchOptions { max_image_download: 100_000, ..opts() }).await.unwrap();
    assert!(p.image.is_none(), "streamed past the cap without a Content-Length");
    let p = fetch(&format!("{base}/stream"), &FetchOptions { max_image_download: 1 << 20, ..opts() }).await.unwrap();
    assert_eq!(p.image.as_ref().map(|i| i.data.len()), Some(big.len()), "under the cap: read whole");
    assert!(fetch(&format!("{base}/huge-px"), &opts()).await.unwrap().image.is_none(), "8000 px wide");
    // UTF-8 by default; invalid bytes replaced
    assert_eq!(decode_html(b"<title>\xff ok</title>", "text/html"), "<title>\u{fffd} ok</title>");
}

// ---------------------------------------------------------------------------
// parser and helpers
// ---------------------------------------------------------------------------

#[test]
fn tricky_html() {
    let h = r#"<!DOCTYPE html>
<HTML><HEAD>
<!-- <title>commented out</title> <meta property=og:title content=nope> -->
<META name=description content=unquoted-value>
<meta name='twitter:title' content='Single &quot;quoted&quot; &amp; fine'>
<meta property="og:image" content=/rel/img.jpg >
<script>var s = "<title>in script</title>";</script>
<style>title { color: red }</style>
<meta name="og:site_name" content="Named not property">
<Title>
   Real &lt;Title&gt; &#x2014; &#169; &bogus; &amp
</TITLE>
<link rel="shortcut icon" href="/f.ico"><link rel=icon type=image/png sizes="16x16 32x32" href=/p32.png>
<link rel="icon" href="/big.png" sizes="512x512">
<link REL="apple-touch-icon-precomposed" href="/t.png"/>
<base href="https://cdn.example.com/base/">
</head><body><meta property="og:title" content="in body"></body></html>"#;
    let m = parse_head(h);
    assert_eq!(m.title.as_deref(), Some("Real <Title> — © &bogus; &"));
    assert_eq!(m.meta(&["description"]).as_deref(), Some("unquoted-value"));
    assert_eq!(m.best_title().as_deref(), Some(r#"Single "quoted" & fine"#), "twitter:title over <title>");
    assert_eq!(m.best_image().as_deref(), Some("/rel/img.jpg"));
    assert_eq!(m.site_name().as_deref(), Some("Named not property"), "name= works like property=");
    assert_eq!(m.base.as_deref(), Some("https://cdn.example.com/base/"));
    assert!(!m.metas.iter().any(|(_, v)| v == "nope" || v == "in body"));
    assert_eq!(m.icon_candidates(256), vec!["/t.png".to_string(), "/p32.png".to_string()]);
    // meta before the title, og:title wins over <title>
    let m = parse_head("<meta property='og:title' content='OG'><title>T</title>");
    assert_eq!((m.best_title().as_deref(), m.title.as_deref()), (Some("OG"), Some("T")));
    // no head at all, unterminated tags
    let m = parse_head("<title>Only</title><meta name=description content=\"x");
    assert_eq!(m.best_title().as_deref(), Some("Only"));
    assert_eq!(m.best_summary().as_deref(), Some("x"));
    assert_eq!(parse_head("< notatag <3 <title></title>"), PageMeta::default());
}

#[test]
fn entities() {
    assert_eq!(decode_entities("a &amp;&amp; b"), "a && b");
    assert_eq!(decode_entities("&#39;&#x27;&apos;"), "'''");
    assert_eq!(decode_entities("&#0; &#xZZ; & ; &unknown;"), "\u{FFFD} &#xZZ; & ; &unknown;");
    assert_eq!(decode_entities("caf&eacute; &mdash; ok"), "café — ok");
}

#[test]
fn urls_in_text() {
    assert_eq!(first_url("look https://example.com/a?b=c#d, then http://x.org").as_deref(), Some("https://example.com/a?b=c#d"));
    assert_eq!(first_url("(see https://en.wikipedia.org/wiki/Foo_(bar)).").as_deref(), Some("https://en.wikipedia.org/wiki/Foo_(bar)"));
    assert_eq!(first_url("<https://example.com/x>").as_deref(), Some("https://example.com/x"));
    assert_eq!(first_url("HTTPS://Example.com/").as_deref(), Some("HTTPS://Example.com/"));
    assert_eq!(first_url("nohttps://example.com and http:// and https://"), None);
    assert_eq!(first_url("nohttp://x.com then http://y.com!").as_deref(), Some("http://y.com"));
    assert_eq!(first_url("just text example.com"), None);
}

#[test]
fn private_hosts() {
    for p in ["http://localhost:8080/", "http://127.0.0.1/", "http://10.1.2.3/", "http://192.168.1.1/", "http://[::1]/", "http://printer.local/", "http://intranet/", "http://169.254.1.1", "http://[fd00::1]/", "http://100.64.0.1/"] {
        assert!(is_private_host(&Url::parse(p).unwrap()), "{p}");
    }
    for p in ["https://example.com/", "http://8.8.8.8/", "https://[2001:4860::8888]/"] {
        assert!(!is_private_host(&Url::parse(p).unwrap()), "{p}");
    }
    for ip in ["0.1.2.3", "224.0.0.1", "239.1.1.1", "::127.0.0.1", "::ffff:10.0.0.1", "fec0::1", "ff02::1", "::", "::1"] {
        assert!(private_ip(ip.parse().unwrap()), "{ip}");
    }
    for ip in ["1.1.1.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
        assert!(!private_ip(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn image_headers() {
    assert_eq!(image_info(&png(3, 4, 0)), Some(("image/png", 3, 4)));
    assert_eq!(image_info(&jpeg(640, 480)), Some(("image/jpeg", 640, 480)));
    assert_eq!(image_info(b"GIF89a...."), None);
    assert_eq!(image_info(&jpeg(640, 480)[..12]), None);
    assert_eq!(image_info(&png(0, 4, 0)), None);
}
