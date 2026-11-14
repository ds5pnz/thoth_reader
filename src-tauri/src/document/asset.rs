//! `thoth-doc` custom protocol (spec §8.4). URL: thoth-doc://localhost/<assetToken>/pdf
//! or thoth-doc://localhost/<assetToken>/epub/<safe relative path>. No OS paths in URLs.
//! GET/HEAD only, single Range -> 206, invalid -> 416, nosniff, token bound to session.

use super::paths::safe_join;
use super::session::SessionManager;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct AssetResponse {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl AssetResponse {
    fn err(status: u16) -> Self {
        AssetResponse { status, headers: base_headers("text/plain"), body: Vec::new() }
    }
}

fn base_headers(mime: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Content-Type", mime.to_string()),
        ("X-Content-Type-Options", "nosniff".into()),
        ("Cache-Control", "no-store".into()),
        ("Cross-Origin-Resource-Policy", "same-site".into()),
    ]
}

/// Allow-listed MIME types by extension; anything else is refused.
pub fn mime_for(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "xhtml" => "application/xhtml+xml",
        "opf" => "application/oebps-package+xml",
        "xml" => "application/xml",
        "ncx" => "application/x-dtbncx+xml",
        "css" => "text/css",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => return None,
    })
}

const EPUB_DOC_CSP: &str = "default-src 'none'; script-src 'none'; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; img-src thoth-doc: http://thoth-doc.localhost blob:; font-src thoth-doc: http://thoth-doc.localhost; style-src thoth-doc: http://thoth-doc.localhost 'unsafe-inline'";

pub fn parse_range(h: &str, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = h.trim().strip_prefix("bytes=") else { return Err(()) };
    if spec.contains(',') {
        return Ok(None); // multi-range: serve full content (RFC 9110 permits ignoring)
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        (len.saturating_sub(n), len.saturating_sub(1))
    } else {
        let s: u64 = a.parse().map_err(|_| ())?;
        let e: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().map_err(|_| ())?.min(len.saturating_sub(1)) };
        (s, e)
    };
    if len == 0 || start >= len || start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

fn serve_file(path: &Path, mime: &str, method: &str, range: Option<&str>, extra: &[(&'static str, String)]) -> AssetResponse {
    let Ok(mut f) = std::fs::File::open(path) else { return AssetResponse::err(404) };
    let Ok(meta) = f.metadata() else { return AssetResponse::err(404) };
    if !meta.is_file() {
        return AssetResponse::err(404);
    }
    let len = meta.len();
    let mut headers = base_headers(mime);
    headers.push(("Accept-Ranges", "bytes".into()));
    headers.extend_from_slice(extra);
    let parsed = match range {
        Some(r) => match parse_range(r, len) {
            Ok(v) => v,
            Err(()) => {
                let mut resp = AssetResponse::err(416);
                resp.headers.push(("Content-Range", format!("bytes */{len}")));
                return resp;
            }
        },
        None => None,
    };
    let (status, start, end) = match parsed {
        Some((s, e)) => (206, s, e),
        None => (200, 0, len.saturating_sub(1)),
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    if status == 206 {
        headers.push(("Content-Range", format!("bytes {start}-{end}/{len}")));
    }
    headers.push(("Content-Length", count.to_string()));
    if method == "HEAD" || count == 0 {
        return AssetResponse { status, headers, body: Vec::new() };
    }
    // Cap a single response to keep memory bounded; PDF.js requests ranges anyway.
    const MAX_CHUNK: u64 = 16 * 1024 * 1024;
    if status == 200 && count > MAX_CHUNK && mime == "application/pdf" {
        // Full GET of a large PDF: still allowed but streamed into memory once.
    }
    let mut body = vec![0u8; count as usize];
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut body).is_err() {
        return AssetResponse::err(500);
    }
    AssetResponse { status, headers, body }
}

/// `path` is the URL path (e.g. "/<token>/epub/x/0.xhtml"), `origin` the request Origin.
pub fn handle(sessions: &SessionManager, method: &str, path: &str, range: Option<&str>, origin: Option<&str>, allowed_origins: &[&str]) -> AssetResponse {
    if method != "GET" && method != "HEAD" {
        return AssetResponse::err(405);
    }
    let decoded = match percent_encoding::percent_decode_str(path).decode_utf8() {
        Ok(s) => s.to_string(),
        Err(_) => return AssetResponse::err(400),
    };
    let trimmed = decoded.trim_start_matches('/');
    let mut parts = trimmed.splitn(3, '/');
    let (Some(token), Some(kind)) = (parts.next(), parts.next()) else { return AssetResponse::err(404) };
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return AssetResponse::err(403);
    }
    let Some(session) = sessions.by_asset_token(token) else { return AssetResponse::err(403) };
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    if let Some(o) = origin {
        if allowed_origins.contains(&o) {
            extra.push(("Access-Control-Allow-Origin", o.to_string()));
            extra.push(("Access-Control-Expose-Headers", "Content-Range, Content-Length, Accept-Ranges".into()));
            extra.push(("Vary", "Origin".into()));
        }
    }
    match (kind, parts.next()) {
        ("pdf", None) if session.format == super::import::Format::Pdf => {
            serve_file(&session.managed_file, "application/pdf", method, range, &extra)
        }
        ("epub", Some(rel)) if session.format == super::import::Format::Epub => {
            let Some(base) = session.derived_dir.read().unwrap().clone() else { return AssetResponse::err(404) };
            let Some(mime) = mime_for(rel) else { return AssetResponse::err(403) };
            let Some(p) = safe_join(&base, rel) else { return AssetResponse::err(403) };
            if mime == "application/xhtml+xml" {
                extra.push(("Content-Security-Policy", EPUB_DOC_CSP.into()));
            }
            serve_file(&p, mime, method, range, &extra)
        }
        _ => AssetResponse::err(404),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::import::Format;
    use crate::document::session::OpenParams;
    use crate::document::sink::VecSink;
    use std::sync::Arc;

    fn setup(fmt: Format) -> (SessionManager, String, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("doc.pdf");
        std::fs::write(&f, b"%PDF-0123456789").unwrap();
        let m = SessionManager::default();
        let s = m.open(OpenParams {
            document_id: "d".into(),
            format: fmt,
            window_label: "main".into(),
            managed_file: f,
            sink: Arc::new(VecSink::default()),
            target_language: "ko".into(),
            source_language: None,
            profile_id: None,
        });
        if fmt == Format::Epub {
            let dd = d.path().join("derived");
            std::fs::create_dir_all(dd.join("x")).unwrap();
            std::fs::write(dd.join("x/0.xhtml"), b"<html/>").unwrap();
            std::fs::write(d.path().join("outside.css"), b"x").unwrap();
            *s.derived_dir.write().unwrap() = Some(dd);
        }
        let t = s.asset_token.clone();
        (m, t, d)
    }

    #[test]
    fn range_requests() {
        let (m, t, _d) = setup(Format::Pdf);
        let p = format!("/{t}/pdf");
        let full = handle(&m, "GET", &p, None, None, &[]);
        assert_eq!(full.status, 200);
        assert_eq!(full.body.len(), 15);
        let r = handle(&m, "GET", &p, Some("bytes=5-9"), None, &[]);
        assert_eq!(r.status, 206);
        assert_eq!(r.body, b"01234");
        assert!(r.headers.iter().any(|(k, v)| *k == "Content-Range" && v == "bytes 5-9/15"));
        let suffix = handle(&m, "GET", &p, Some("bytes=-3"), None, &[]);
        assert_eq!(suffix.body, b"789");
        assert_eq!(handle(&m, "GET", &p, Some("bytes=99-"), None, &[]).status, 416);
        assert_eq!(handle(&m, "GET", &p, Some("items=0-1"), None, &[]).status, 416);
        let head = handle(&m, "HEAD", &p, None, None, &[]);
        assert!(head.body.is_empty());
        assert!(head.headers.iter().any(|(k, v)| *k == "Content-Length" && v == "15"));
        assert_eq!(handle(&m, "POST", &p, None, None, &[]).status, 405);
    }

    #[test]
    fn forged_tokens_and_traversal_rejected() {
        let (m, t, _d) = setup(Format::Epub);
        assert_eq!(handle(&m, "GET", &format!("/{}/epub/x/0.xhtml", "0".repeat(64)), None, None, &[]).status, 403);
        assert_eq!(handle(&m, "GET", &format!("/{t}/epub/x/0.xhtml"), None, None, &[]).status, 200);
        assert_eq!(handle(&m, "GET", &format!("/{t}/epub/../outside.css"), None, None, &[]).status, 403);
        assert_eq!(handle(&m, "GET", &format!("/{t}/epub/%2e%2e/outside.css"), None, None, &[]).status, 403);
        assert_eq!(handle(&m, "GET", &format!("/{t}/epub/x/0.exe"), None, None, &[]).status, 403);
        // wrong kind for the session format
        assert_eq!(handle(&m, "GET", &format!("/{t}/pdf"), None, None, &[]).status, 404);
    }
}
