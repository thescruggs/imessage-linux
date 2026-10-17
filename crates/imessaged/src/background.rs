//! Transcript background payloads -> something a client can paint.
//!
//! What Apple delivers (and what macOS 26 caches in
//! `~/Library/Messages/TranscriptBackgroundCache/<background id>`) is a
//! PosterKit configuration: an uncompressed Apple Archive (`AA01`) or a
//! stored zip, holding `configuration/versions/<n>/contents/<uuid>/
//! output.layerStack/portrait-layer_background.HEIC` (Photos posters) next
//! to plists and a `Contents.json` layer description. The sibling
//! `-watchBackground` file is a binary plist whose `backgroundImageData` is a
//! pre-rendered PNG (read as a bounded event stream; XML plists are not
//! accepted). See docs/RESEARCH.md ("transcript backgrounds").
//!
//! [`extract_image`] finds the background picture in any of these (or
//! passes a plain image through). Pure functions, no I/O.

/// An image found in a payload: file extension (lowercase) + bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub ext: &'static str,
    pub data: Vec<u8>,
}

/// One regular file in an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub data: Vec<u8>,
}

/// Image format by magic bytes.
pub fn image_ext(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if b.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" {
        match &b[8..12] {
            b"heic" | b"heix" | b"mif1" | b"msf1" | b"heim" | b"heis" => Some("heic"),
            b"avif" | b"avis" => Some("avif"),
            _ => None,
        }
    } else {
        None
    }
}

/// Regular files of an uncompressed Apple Archive (`AA01` headers; the
/// LZFSE-compressed `pbze` container is not handled). `None` if `b` is not
/// one or is malformed.
pub fn apple_archive_entries(b: &[u8]) -> Option<Vec<Entry>> {
    let mut out = vec![];
    let mut off = 0usize;
    while off < b.len() {
        if b.get(off..off + 4)? != b"AA01" {
            return None;
        }
        let hs = u16::from_le_bytes(b.get(off + 4..off + 6)?.try_into().ok()?) as usize;
        let end = off.checked_add(hs)?;
        if hs < 6 || end > b.len() {
            return None;
        }
        let mut p = off + 6;
        let (mut typ, mut path, mut blobs) = (0u8, String::new(), Vec::<([u8; 3], usize)>::new());
        while p < end {
            let key: [u8; 3] = b.get(p..p + 3)?.try_into().ok()?;
            let t = *b.get(p + 3)?;
            p += 4;
            let uint = |p: usize, n: usize| -> Option<u64> {
                let mut v = 0u64;
                for (i, x) in b.get(p..p + n)?.iter().enumerate() {
                    v |= (*x as u64) << (8 * i);
                }
                Some(v)
            };
            match t {
                b'*' => {}
                b'1' | b'2' | b'4' | b'8' => {
                    let n = (t - b'0') as usize;
                    if &key == b"TYP" {
                        typ = uint(p, n)? as u8;
                    }
                    p += n;
                }
                b'P' => {
                    let n = uint(p, 2)? as usize;
                    if &key == b"PAT" {
                        path = String::from_utf8_lossy(b.get(p + 2..p + 2 + n)?).into_owned();
                    }
                    p += 2 + n;
                }
                b'A' | b'B' | b'C' => {
                    let n = match t {
                        b'A' => 2,
                        b'B' => 4,
                        _ => 8,
                    };
                    blobs.push((key, usize::try_from(uint(p, n)?).ok()?));
                    p += n;
                }
                b'S' => p += 8,
                b'T' => p += 12,
                b'F' => p += 4,
                b'G' => p += 20,
                b'H' => p += 32,
                b'I' => p += 48,
                b'J' => p += 64,
                _ => return None,
            }
        }
        if p != end {
            return None;
        }
        off = end;
        for (key, n) in blobs {
            let data = b.get(off..off.checked_add(n)?)?;
            if &key == b"DAT" && typ == b'F' {
                out.push(Entry { path: path.clone(), data: data.to_vec() });
            }
            off += n;
        }
    }
    Some(out)
}

/// Regular files of a zip whose entries are stored (method 0), read via
/// the local headers. Deflated entries are skipped. `None` if `b` is not a
/// zip.
pub fn stored_zip_entries(b: &[u8]) -> Option<Vec<Entry>> {
    if !b.starts_with(b"PK\x03\x04") {
        return None;
    }
    let u16_at = |p: usize| b.get(p..p + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize);
    let u32_at = |p: usize| b.get(p..p + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize);
    let mut out = vec![];
    let mut off = 0usize;
    while b.get(off..off + 4) == Some(b"PK\x03\x04") {
        let flags = u16_at(off + 6)?;
        let method = u16_at(off + 8)?;
        let size = u32_at(off + 18)?;
        let name_len = u16_at(off + 26)?;
        let extra_len = u16_at(off + 28)?;
        let name = String::from_utf8_lossy(b.get(off + 30..off + 30 + name_len)?).into_owned();
        let data_at = off + 30 + name_len + extra_len;
        if flags & 0x08 != 0 {
            // Sizes only in a trailing data descriptor: stop here.
            break;
        }
        let data = b.get(data_at..data_at.checked_add(size)?)?;
        if method == 0 && !name.ends_with('/') {
            out.push(Entry { path: name, data: data.to_vec() });
        }
        off = data_at + size;
    }
    Some(out)
}

/// The picture to paint for a background payload: a plain image as is; in a
/// poster archive the `*layer_background*` image (else the largest image);
/// in a `-watchBackground` plist its `backgroundImageData`.
/// Payloads over [`crate::engine::MAX_BACKGROUND_BYTES`] yield `None`.
pub fn extract_image(payload: &[u8]) -> Option<Image> {
    if payload.len() as u64 > crate::engine::MAX_BACKGROUND_BYTES {
        return None;
    }
    if let Some(ext) = image_ext(payload) {
        return Some(Image { ext, data: payload.to_vec() });
    }
    if payload.starts_with(b"bplist00") {
        let data = watch_background_image_data(payload)?;
        return image_ext(&data).map(|ext| Image { ext, data });
    }
    let entries = apple_archive_entries(payload).or_else(|| stored_zip_entries(payload))?;
    let images = entries.into_iter().filter_map(|e| image_ext(&e.data).map(|ext| (e.path.to_ascii_lowercase().contains("layer_background"), ext, e.data)));
    images.max_by_key(|(bg, _, d)| (*bg, d.len())).map(|(_, ext, data)| Image { ext, data })
}

/// Most plist events [`watch_background_image_data`] reads before giving
/// up (a `-watchBackground` plist has a handful of keys). Binary plists
/// can reference one object many times, so even a small file can expand
/// into a huge event stream; this bounds the work.
const MAX_PLIST_EVENTS: usize = 10_000;

/// The top-level `backgroundImageData` of a binary plist, read as an event
/// stream (no full `Value` is built; nested values are skipped).
fn watch_background_image_data(payload: &[u8]) -> Option<Vec<u8>> {
    use plist::stream::{BinaryReader, Event};
    let mut events = BinaryReader::new(std::io::Cursor::new(payload)).take(MAX_PLIST_EVENTS);
    if !matches!(events.next()?.ok()?, Event::StartDictionary(_)) {
        return None;
    }
    // depth 1 = in the top-level dictionary; `want` = the next top-level
    // value is the one we are after; `key` = expecting a top-level key.
    let (mut depth, mut key, mut want) = (1usize, true, false);
    for ev in events {
        let ev = ev.ok()?;
        match ev {
            Event::StartArray(_) | Event::StartDictionary(_) => {
                if depth == 1 {
                    if want {
                        return None;
                    }
                    key = true;
                }
                depth += 1;
            }
            Event::EndCollection => {
                depth -= 1;
                if depth == 0 {
                    return None;
                }
            }
            Event::String(s) if depth == 1 && key => {
                want = s == "backgroundImageData";
                key = false;
            }
            Event::Data(d) if depth == 1 && want => return Some(d.into_owned()),
            _ if depth == 1 => {
                if want {
                    return None;
                }
                key = true;
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEIC: &[u8] = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn aa_entry(typ: u8, path: &str, data: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(b"TYP1");
        h.push(typ);
        h.extend_from_slice(b"PATP");
        h.extend_from_slice(&(path.len() as u16).to_le_bytes());
        h.extend_from_slice(path.as_bytes());
        h.extend_from_slice(b"MOD2\xff\x01");
        h.extend_from_slice(b"MTMT");
        h.extend_from_slice(&[0; 12]);
        if typ == b'F' {
            h.extend_from_slice(b"DATB");
            h.extend_from_slice(&(data.len() as u32).to_le_bytes());
        }
        let mut out = b"AA01".to_vec();
        out.extend_from_slice(&((h.len() + 6) as u16).to_le_bytes());
        out.extend(h);
        if typ == b'F' {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn apple_archive_poster() {
        let mut a = aa_entry(b'D', "", b"");
        a.extend(aa_entry(b'D', "configuration", b""));
        a.extend(aa_entry(b'F', "configuration/versions/0/x.plist", b"bplist00junk"));
        a.extend(aa_entry(b'F', "configuration/versions/0/contents/U/output.layerStack/big.png", &[PNG, &[0; 100]].concat()));
        a.extend(aa_entry(b'F', "configuration/versions/0/contents/U/output.layerStack/portrait-layer_background.HEIC", HEIC));
        let e = apple_archive_entries(&a).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].path, "configuration/versions/0/x.plist");
        // The background layer wins over a larger image.
        assert_eq!(extract_image(&a), Some(Image { ext: "heic", data: HEIC.to_vec() }));
        // Truncated / not an archive.
        assert!(apple_archive_entries(&a[..a.len() - 3]).is_none());
        assert!(extract_image(b"AA01\x02").is_none());
        assert!(extract_image(b"nothing").is_none());
    }

    #[test]
    fn stored_zip_and_watch_plist_and_plain() {
        let mut z = Vec::new();
        for (name, data) in [("manifest.plist", &b"<?xml"[..]), ("contents/portrait-layer_background.HEIC", HEIC)] {
            z.extend_from_slice(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0\0\0\0\0");
            z.extend_from_slice(&(data.len() as u32).to_le_bytes());
            z.extend_from_slice(&(data.len() as u32).to_le_bytes());
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&0u16.to_le_bytes());
            z.extend_from_slice(name.as_bytes());
            z.extend_from_slice(data);
        }
        z.extend_from_slice(b"PK\x01\x02rest-of-central-directory");
        assert_eq!(stored_zip_entries(&z).unwrap().len(), 2);
        assert_eq!(extract_image(&z).unwrap().ext, "heic");

        let mut d = plist::Dictionary::new();
        d.insert("backgroundImageData".into(), plist::Value::Data(PNG.to_vec()));
        d.insert("luminance".into(), plist::Value::Real(0.5));
        let mut bin = Vec::new();
        plist::to_writer_binary(&mut bin, &plist::Value::Dictionary(d)).unwrap();
        assert_eq!(extract_image(&bin), Some(Image { ext: "png", data: PNG.to_vec() }));

        // S3: only the top-level key counts, other values (nested ones
        // included) are skipped; XML plists are not parsed at all.
        let mut inner = plist::Dictionary::new();
        inner.insert("backgroundImageData".into(), plist::Value::Data(HEIC.to_vec()));
        let mut d = plist::Dictionary::new();
        d.insert("a".into(), plist::Value::Dictionary(inner.clone()));
        d.insert("b".into(), plist::Value::Array(vec![plist::Value::Dictionary(inner.clone()), plist::Value::Integer(3.into())]));
        d.insert("c".into(), plist::Value::String("backgroundImageData".into()));
        d.insert("backgroundImageData".into(), plist::Value::Data(PNG.to_vec()));
        d.insert("z".into(), plist::Value::Boolean(true));
        let mut bin = Vec::new();
        plist::to_writer_binary(&mut bin, &plist::Value::Dictionary(d.clone())).unwrap();
        assert_eq!(extract_image(&bin), Some(Image { ext: "png", data: PNG.to_vec() }));
        let mut nested_only = Vec::new();
        plist::to_writer_binary(&mut nested_only, &plist::Value::Dictionary([("a".to_string(), plist::Value::Dictionary(inner))].into_iter().collect()))
            .unwrap();
        assert_eq!(extract_image(&nested_only), None);
        d.insert("backgroundImageData".into(), plist::Value::String("not data".into()));
        let mut wrong_type = Vec::new();
        plist::to_writer_binary(&mut wrong_type, &plist::Value::Dictionary(d)).unwrap();
        assert_eq!(extract_image(&wrong_type), None);
        let mut xml = Vec::new();
        let mut xd = plist::Dictionary::new();
        xd.insert("backgroundImageData".into(), plist::Value::Data(PNG.to_vec()));
        plist::to_writer_xml(&mut xml, &plist::Value::Dictionary(xd)).unwrap();
        assert_eq!(extract_image(&xml), None);
        assert_eq!(extract_image(b"bplist00garbage"), None);
        // The event budget is bounded: the key after 10k others is not reached.
        let mut many: plist::Dictionary = (0..MAX_PLIST_EVENTS).map(|i| (format!("k{i:05}"), plist::Value::Boolean(true))).collect();
        many.insert("zz-last".into(), plist::Value::Boolean(true));
        many.insert("backgroundImageData".into(), plist::Value::Data(PNG.to_vec()));
        let mut long = Vec::new();
        plist::to_writer_binary(&mut long, &plist::Value::Dictionary(many)).unwrap();
        assert_eq!(extract_image(&long), None);
        // Over the size cap: not looked at.
        let mut big = PNG.to_vec();
        big.resize(crate::engine::MAX_BACKGROUND_BYTES as usize + 1, 0);
        assert_eq!(extract_image(&big), None);

        assert_eq!(extract_image(PNG).unwrap().ext, "png");
        assert_eq!(image_ext(b"\xff\xd8\xff\xe0"), Some("jpg"));
    }
}
