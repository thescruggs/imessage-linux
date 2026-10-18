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
}

#[test]
fn image_headers() {
    assert_eq!(image_info(&png(3, 4, 0)), Some(("image/png", 3, 4)));
    assert_eq!(image_info(&jpeg(640, 480)), Some(("image/jpeg", 640, 480)));
    assert_eq!(image_info(b"GIF89a...."), None);
    assert_eq!(image_info(&jpeg(640, 480)[..12]), None);
    assert_eq!(image_info(&png(0, 4, 0)), None);
}
