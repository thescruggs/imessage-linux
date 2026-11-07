//! The iCloud contacts sync against a canned CardDAV server: the password
//! is checked before anything is saved, cards (not groups) become
//! `icloud-*.vcf` files with linked photos embedded, later syncs fetch only
//! what changed, the account survives a restart, and signing out can
//! remove the cards.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use imessaged::config::DaemonConfig;
use imessaged::daemon::Daemon;
use imessaged::engine::Engine;
use imessaged::engine::mock::MockEngine;
use imessaged::hub::OpError;
use imessaged::keyring::StateKey;
use imessaged::store::{Store, StoreHandle};

const USER: &str = "me@icloud.com";
const PASSWORD: &str = "abcd-efgh-ijkl-mnop";
const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];

fn store(dir: &std::path::Path) -> StoreHandle {
    StoreHandle::new(Store::open(dir, StateKey::from_bytes([7; 32])).unwrap())
}

/// The address book: card name -> (etag, vCard); and every multiget's hrefs.
#[derive(Default)]
struct Book {
    cards: BTreeMap<String, (String, String)>,
    multigets: Vec<Vec<String>>,
    photo_gets: usize,
    /// Answer photo requests with 503.
    photo_fails: bool,
    /// Card names a multiget leaves out of its reply.
    omit: Vec<String>,
}

struct Server {
    base: String,
    book: Arc<Mutex<Book>>,
}

fn serve() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let book = Arc::new(Mutex::new(Book::default()));
    let (b, origin) = (book.clone(), base.clone());
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let (b, origin) = (b.clone(), origin.clone());
            std::thread::spawn(move || handle(s, &b, &origin));
        }
    });
    Server { base, book }
}

/// (method, path, headers lower-cased, body)
type Request = (String, String, Vec<(String, String)>, String);

fn read_request(s: &mut TcpStream) -> Option<Request> {
    let mut buf = vec![];
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_string(), first.next()?.to_string());
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len: usize = headers.iter().find(|h| h.0 == "content-length").and_then(|h| h.1.parse().ok()).unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Some((method, path, headers, String::from_utf8_lossy(&buf[head_end..]).into_owned()))
}

fn reply(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(body);
}

fn ms(responses: &str) -> Vec<u8> {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><multistatus xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">{responses}</multistatus>"#).into_bytes()
}

fn ok(href: &str, props: &str) -> String {
    format!("<response><href>{href}</href><propstat><prop>{props}</prop><status>HTTP/1.1 200 OK</status></propstat></response>")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\r', "&#13;")
}

fn handle(mut s: TcpStream, book: &Mutex<Book>, origin: &str) {
    let Some((method, path, headers, body)) = read_request(&mut s) else { return };
    use base64::Engine as _;
    let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASSWORD}")));
    if !headers.iter().any(|h| h.0 == "authorization" && h.1 == want) {
        return reply(&mut s, "401 Unauthorized", "text/plain", b"no");
    }
    let xml = "application/xml; charset=utf-8";
    match (method.as_str(), path.as_str()) {
        ("PROPFIND", "/") => reply(&mut s, "207 Multi-Status", xml, &ms(&ok("/", "<current-user-principal><href>/1/principal/</href></current-user-principal>"))),
        ("PROPFIND", "/1/principal/") => reply(
            &mut s,
            "207 Multi-Status",
            xml,
            &ms(&ok("/1/principal/", &format!("<card:addressbook-home-set><href>{origin}/1/carddav/</href></card:addressbook-home-set>"))),
        ),
        ("PROPFIND", "/1/carddav/") => reply(
            &mut s,
            "207 Multi-Status",
            xml,
            &ms(&format!(
                "{}{}",
                ok("/1/carddav/", "<resourcetype><collection/></resourcetype>"),
                ok("/1/carddav/card/", "<resourcetype><collection/><card:addressbook/></resourcetype>")
            )),
        ),
        ("PROPFIND", "/1/carddav/card/") => {
            let b = book.lock().unwrap();
            let mut r = ok("/1/carddav/card/", "<resourcetype><collection/><card:addressbook/></resourcetype><getetag>\"col\"</getetag>");
            for (name, (etag, _)) in &b.cards {
                r.push_str(&ok(&format!("/1/carddav/card/{name}"), &format!("<getetag>{}</getetag><resourcetype/>", xml_escape(etag))));
            }
            reply(&mut s, "207 Multi-Status", xml, &ms(&r));
        }
        ("REPORT", "/1/carddav/card/") => {
            let hrefs: Vec<String> =
                body.split("<d:href>").skip(1).filter_map(|p| p.split_once("</d:href>")).map(|(h, _)| h.replace("&amp;", "&")).collect();
            let mut b = book.lock().unwrap();
            b.multigets.push(hrefs.clone());
            let mut r = String::new();
            for h in &hrefs {
                let name = h.trim_start_matches("/1/carddav/card/").replace("%20", " ");
                if b.omit.contains(&name) {
                    continue;
                }
                if let Some((etag, card)) = b.cards.get(&name) {
                    r.push_str(&ok(h, &format!("<getetag>{}</getetag><card:address-data>{}</card:address-data>", xml_escape(etag), xml_escape(card))));
                }
            }
            reply(&mut s, "207 Multi-Status", xml, &ms(&r));
        }
        ("GET", "/photos/pat.jpg") => {
            let mut b = book.lock().unwrap();
            b.photo_gets += 1;
            if b.photo_fails {
                drop(b);
                return reply(&mut s, "503 Service Unavailable", "text/plain", b"");
            }
            drop(b);
            reply(&mut s, "200 OK", "image/jpeg", JPEG)
        }
        _ => reply(&mut s, "404 Not Found", "text/plain", b""),
    }
}

fn vcf_files(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn icloud_contacts_sync() {
    let srv = serve();
    let pat = |photo: &str| {
        format!("BEGIN:VCARD\r\nVERSION:3.0\r\nN:Doe;Pat;;;\r\nFN:Pat Doe\r\nitem1.EMAIL;type=INTERNET:pat@example.com\r\nTEL;type=CELL:(555) 555-0142\r\n{photo}END:VCARD\r\n")
    };
    {
        let mut b = srv.book.lock().unwrap();
        b.cards.insert("pat one.vcf".into(), ("\"p1\"".into(), pat(&format!("PHOTO;VALUE=uri:{}/photos/pat.jpg\r\n", srv.base))));
        b.cards.insert("bob.vcf".into(), ("\"b1\"".into(), "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bob & Co\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n".into()));
        b.cards.insert(
            "friends.vcf".into(),
            ("\"g1\"".into(), "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Friends\r\nX-ADDRESSBOOKSERVER-KIND:group\r\nEND:VCARD\r\n".into()),
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let cards = dir.path().join("kpeoplevcard");
    std::fs::create_dir(&cards).unwrap();
    std::fs::write(cards.join("mac-OLD-1.vcf"), "BEGIN:VCARD\r\nFN:Old\r\nEMAIL:old@example.com\r\nEND:VCARD\r\n").unwrap();
    std::fs::write(cards.join("kdeconnect-1.vcf"), "BEGIN:VCARD\r\nFN:Phone\r\nTEL:+15555550100\r\nEND:VCARD\r\n").unwrap();

    let start = |mock: Arc<MockEngine>| {
        let mut cfg = DaemonConfig::new(dir.path().into());
        cfg.contacts_dir = cards.clone();
        cfg.contacts_sync_minutes = 0;
        let base = srv.base.clone() + "/";
        let st = store(dir.path());
        async move { Daemon::with_engine(cfg, mock).with_store(st).with_contacts_base(base).start().await.unwrap() }
    };
    let mock = Arc::new(MockEngine::new());
    mock.login("me@example.com", "pw").await.unwrap();
    let running = start(mock.clone()).await;
    let hub = running.hub();
    assert_eq!(hub.contacts_sync_status().account, None);
    assert!(matches!(hub.sync_contacts().await, Err(OpError::NotFound(_))), "no account yet");

    // a wrong password is refused and nothing is saved
    let e = hub.set_contacts_account(USER, "wrong-pass-word-xxxx").await.unwrap_err();
    assert!(e.to_string().contains("app-specific password"), "{e}");
    assert!(!dir.path().join("contacts-account.enc").exists());
    assert_eq!(hub.contacts_sync_status().account, None);

    // the right one: saved (sealed), synced
    let st = hub.set_contacts_account(USER, " abcd-efgh-ijkl-mnop\n").await.unwrap();
    assert_eq!(st.account.as_deref(), Some(USER), "{st:?}");
    assert_eq!(st.last_error, None, "{st:?}");
    let r = st.last.clone().unwrap();
    assert_eq!((r.cards, r.photos, r.written, r.unchanged, r.removed, r.removed_mac), (2, 1, 2, 0, 0, 1), "{r:?}");
    assert!(st.last_sync.is_some());
    let sealed = std::fs::read(dir.path().join("contacts-account.enc")).unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("efgh"), "the password is sealed");
    let files = vcf_files(&cards);
    assert_eq!(files.len(), 3, "{files:?}");
    assert!(files.contains(&"kdeconnect-1.vcf".to_string()), "other vCards stay");
    assert!(!files.iter().any(|f| f.starts_with("mac-")), "the Mac's copies are replaced");
    let texts: Vec<String> = files.iter().filter(|f| f.starts_with("icloud-")).map(|f| std::fs::read_to_string(cards.join(f)).unwrap()).collect();
    let pat_card = texts.iter().find(|t| t.contains("FN:Pat Doe")).expect("pat written");
    assert!(pat_card.contains("PHOTO;ENCODING=b;TYPE=JPEG:/9j/4AECAw==\r\n"), "{pat_card}");
    assert!(!pat_card.contains("VALUE=uri"));
    assert!(pat_card.contains("\r\nEMAIL;type=INTERNET:pat@example.com\r\n") && !pat_card.contains("item1."), "groups dropped for KPeople: {pat_card}");
    assert!(texts.iter().any(|t| t.contains("FN:Bob & Co")), "entities decoded");
    assert!(!texts.iter().any(|t| t.contains("Friends")), "groups are not people");
    assert_eq!(hub.contact_links().people(), 1, "pat's email and phone are one conversation now");

    // nothing changed: nothing fetched
    srv.book.lock().unwrap().multigets.clear();
    let r = hub.sync_contacts().await.unwrap().last.unwrap();
    assert_eq!((r.written, r.unchanged, r.removed), (0, 2, 0), "{r:?}");
    assert!(srv.book.lock().unwrap().multigets.is_empty());

    // pat's photo cannot be fetched: written without it, fetched again next time
    {
        let mut b = srv.book.lock().unwrap();
        b.photo_fails = true;
        let pat_photo = pat(&format!("PHOTO;VALUE=uri:{}/photos/pat.jpg\r\nNOTE:new\r\n", srv.base));
        b.cards.insert("pat one.vcf".into(), ("\"p2\"".into(), pat_photo));
    }
    let r = hub.sync_contacts().await.unwrap().last.unwrap();
    assert_eq!((r.written, r.photos), (1, 0), "{r:?}");
    srv.book.lock().unwrap().photo_fails = false;
    let r = hub.sync_contacts().await.unwrap().last.unwrap();
    assert_eq!((r.written, r.photos), (1, 1), "the photo is back: {r:?}");

    // a changed card left out of the multiget reply keeps its file
    {
        let mut b = srv.book.lock().unwrap();
        b.cards.insert("bob.vcf".into(), ("\"b9\"".into(), "BEGIN:VCARD\r\nFN:Bob Gone?\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n".into()));
        b.omit = vec!["bob.vcf".into()];
    }
    let r = hub.sync_contacts().await.unwrap().last.unwrap();
    assert_eq!((r.cards, r.written, r.removed), (2, 0, 0), "{r:?}");
    {
        let mut b = srv.book.lock().unwrap();
        b.omit.clear();
        b.cards.insert("bob.vcf".into(), ("\"b1\"".into(), "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bob & Co\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n".into()));
    }
    let r = hub.sync_contacts().await.unwrap().last.unwrap();
    assert_eq!((r.cards, r.written), (2, 0), "asked again, same content as the file: {r:?}");

    // a later sync never touches mac-*.vcf (only the first one replaces them)
    std::fs::write(cards.join("mac-NEW-2.vcf"), "BEGIN:VCARD\r\nFN:Kept\r\nEMAIL:k@example.com\r\nEND:VCARD\r\n").unwrap();
    assert_eq!(hub.sync_contacts().await.unwrap().last.unwrap().removed_mac, 0);
    std::fs::remove_file(cards.join("mac-NEW-2.vcf")).unwrap();

    // bob changes, pat goes: only bob is fetched
    srv.book.lock().unwrap().multigets.clear();
    {
        let mut b = srv.book.lock().unwrap();
        b.cards.insert("bob.vcf".into(), ("\"b2\"".into(), "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bob Brown\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n".into()));
        b.cards.remove("pat one.vcf");
    }
    let r = hub.sync_contacts().await.unwrap().last.unwrap();
    assert_eq!((r.cards, r.photos, r.written, r.unchanged, r.removed), (1, 0, 1, 0, 1), "{r:?}");
    assert_eq!(srv.book.lock().unwrap().multigets, vec![vec!["/1/carddav/card/bob.vcf".to_string()]]);
    assert_eq!(vcf_files(&cards).len(), 2);

    // an empty listing changes nothing (and is an error)
    let saved = std::mem::take(&mut srv.book.lock().unwrap().cards);
    assert!(hub.sync_contacts().await.is_err());
    assert_eq!(vcf_files(&cards).len(), 2);
    assert!(hub.contacts_sync_status().last_error.unwrap().contains("no contacts"));
    srv.book.lock().unwrap().cards = saved;

    // the account survives a restart
    running.shutdown().await.unwrap();
    let running = start(mock.clone()).await;
    let hub = running.hub();
    let st = hub.contacts_sync_status();
    assert_eq!(st.account.as_deref(), Some(USER));
    assert_eq!(st.last.as_ref().map(|r| r.cards), Some(1));
    assert_eq!(hub.status().await.contacts_sync.account.as_deref(), Some(USER), "in GetStatus");
    assert_eq!(hub.sync_contacts().await.unwrap().last_error, None);

    // signing out with the cards
    let st = hub.clear_contacts_account(true).await.unwrap();
    assert_eq!((st.account, st.last), (None, None));
    assert_eq!(vcf_files(&cards), vec!["kdeconnect-1.vcf"]);
    assert!(!dir.path().join("contacts-account.enc").exists());
    running.shutdown().await.unwrap();
}
