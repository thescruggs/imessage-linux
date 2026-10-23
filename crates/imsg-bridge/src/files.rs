//! Serving files the daemon pointed at (docs/BRIDGE.md, Files), and naming
//! uploaded ones.
//!
//! Containment: a path is served only if its canonical form (symlinks
//! resolved) lies under the canonical daemon state dir. Clients never send
//! paths; this guards against a daemon-returned path (or a symlink planted
//! in the state dir) that leads elsewhere.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use imessage_common::platform::fs::open_regular_file;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::error::ApiError;

/// MIME type for a file name's extension.
pub fn mime_for(name: &str) -> Option<&'static str> {
    let ext = Path::new(name).extension().and_then(|e| e.to_str())?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "mov" => "video/quicktime",
        "mp4" | "m4v" => "video/mp4",
        "m4a" => "audio/mp4",
        "caf" => "audio/x-caf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "amr" => "audio/amr",
        "pdf" => "application/pdf",
        "txt" => "text/plain; charset=utf-8",
        "vcf" => "text/vcard",
        "zip" => "application/zip",
        _ => return None,
    })
}

/// An uploaded file's name made safe to create: last path component only,
/// no control or reserved characters, no leading dots, at most 200 bytes.
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let mut out = cleaned.trim().trim_start_matches('.').to_string();
    if out.len() > 200 {
        let mut cut = 200;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    if out.trim().is_empty() { "file".into() } else { out }
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` against `len`: `Ok(None)` = serve
/// everything (absent, malformed or several ranges), `Err` = 416.
fn parse_range(h: Option<&HeaderValue>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = h.and_then(|v| v.to_str().ok()).and_then(|v| v.trim().strip_prefix("bytes=")) else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((a, b)) = spec.split_once('-') else { return Ok(None) };
    let (a, b) = (a.trim(), b.trim());
    let range = if a.is_empty() {
        let Ok(n) = b.parse::<u64>() else { return Ok(None) };
        if n == 0 || len == 0 {
            return Err(());
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let Ok(start) = a.parse::<u64>() else { return Ok(None) };
        let end = if b.is_empty() {
            len.saturating_sub(1)
        } else {
            match b.parse::<u64>() {
                Ok(e) => e.min(len.saturating_sub(1)),
                Err(_) => return Ok(None),
            }
        };
        if start >= len || start > end {
            return Err(());
        }
        (start, end)
    };
    Ok(Some(range))
}

pub(crate) fn etag_matches(if_none_match: Option<&HeaderValue>, etag: &str) -> bool {
    let Some(v) = if_none_match.and_then(|v| v.to_str().ok()) else { return false };
    let weak = |s: &str| s.trim().trim_start_matches("W/").to_string();
    v.split(',').any(|t| t.trim() == "*" || weak(t) == weak(etag))
}

/// `inline; filename*=UTF-8''<percent-encoded>`.
fn disposition(name: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
    format!("inline; filename*=UTF-8''{}", percent_encoding::utf8_percent_encode(name, SET))
}

/// Is `path` (canonical) under `root` (canonical)? Errors if not or if
/// either cannot be resolved.
pub fn contained(root: &Path, path: &Path) -> Result<PathBuf, ApiError> {
    let root = std::fs::canonicalize(root).map_err(|e| ApiError::new("Store", format!("daemon state directory: {e}")))?;
    let canon = std::fs::canonicalize(path).map_err(|_| ApiError::not_found("the file does not exist"))?;
    if !canon.starts_with(&root) {
        log::warn!("refusing to serve {} (resolves outside {})", path.display(), root.display());
        return Err(ApiError::not_found("the file is outside the daemon's state directory"));
    }
    Ok(canon)
}

/// Serve the daemon-returned `path` (see the module doc for containment).
pub async fn serve(state_dir: &Path, path: &str, headers: &HeaderMap) -> Result<Response, ApiError> {
    let original = PathBuf::from(path);
    if !original.is_absolute() {
        return Err(ApiError::not_found("the daemon returned a relative path"));
    }
    let (root, orig) = (state_dir.to_path_buf(), original.clone());
    let (file, meta) = tokio::task::spawn_blocking(move || {
        let canon = contained(&root, &orig)?;
        let f = open_regular_file(&canon).map_err(|_| ApiError::not_found("the file cannot be opened"))?;
        let meta = f.metadata().map_err(|e| ApiError::new("Store", e.to_string()))?;
        Ok::<_, ApiError>((f, meta))
    })
    .await
    .map_err(|e| ApiError::new("Store", e.to_string()))??;

    let len = meta.len();
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let etag = format!("W/\"{len:x}-{mtime:x}\"");
    let name = original.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let mut builder = Response::builder()
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, "private, max-age=86400")
        .header(header::ACCEPT_RANGES, "bytes");
    if etag_matches(headers.get(header::IF_NONE_MATCH), &etag) {
        return Ok(builder.status(StatusCode::NOT_MODIFIED).body(Body::empty()).expect("valid response"));
    }
    builder = builder
        .header(header::CONTENT_TYPE, mime_for(&name).unwrap_or("application/octet-stream"))
        .header(header::CONTENT_DISPOSITION, disposition(&name));
    let mut file = tokio::fs::File::from_std(file);
    let (status, start, count) = match parse_range(headers.get(header::RANGE), len) {
        Ok(None) => (StatusCode::OK, 0, len),
        Ok(Some((a, b))) => {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}"));
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        Err(()) => {
            let body = serde_json::json!({"error": "RangeNotSatisfiable", "message": "the range is outside the file"}).to_string();
            return Ok(builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("valid response"));
        }
    };
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await.map_err(|e| ApiError::new("Store", e.to_string()))?;
    }
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file.take(count)));
    Ok(builder.status(status).header(header::CONTENT_LENGTH, count).body(body).expect("valid response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 10), Ok(None));
        assert_eq!(parse_range(Some(&hv("bytes=0-3")), 10), Ok(Some((0, 3))));
        assert_eq!(parse_range(Some(&hv("bytes=5-")), 10), Ok(Some((5, 9))));
        assert_eq!(parse_range(Some(&hv("bytes=-4")), 10), Ok(Some((6, 9))));
        assert_eq!(parse_range(Some(&hv("bytes=8-100")), 10), Ok(Some((8, 9))));
        assert_eq!(parse_range(Some(&hv("bytes=10-")), 10), Err(()));
        assert_eq!(parse_range(Some(&hv("bytes=0-1,4-5")), 10), Ok(None));
        assert_eq!(parse_range(Some(&hv("items=0-1")), 10), Ok(None));
    }

    #[test]
    fn names_and_etags() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\x\\a?b.jpg"), "a_b.jpg");
        assert_eq!(sanitize_filename(".hidden"), "hidden");
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename(".."), "file");
        assert!(sanitize_filename(&"é".repeat(300)).len() <= 200);
        assert_eq!(disposition("a b.jpg"), "inline; filename*=UTF-8''a%20b.jpg");
        assert!(etag_matches(Some(&hv("W/\"a-b\"")), "W/\"a-b\""));
        assert!(etag_matches(Some(&hv("\"x\", \"a-b\"")), "W/\"a-b\""));
        assert!(!etag_matches(Some(&hv("\"x\"")), "W/\"a-b\""));
        assert_eq!(mime_for("IMG.JPEG"), Some("image/jpeg"));
        assert_eq!(mime_for("x.bin"), None);
    }
}
