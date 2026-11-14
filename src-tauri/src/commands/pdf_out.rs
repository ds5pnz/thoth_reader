//! Minimal streaming PDF writer for exports: each page is a set of JPEG images placed on a
//! page (the webview rasterises pages; Rust validates and writes). Pages are appended as they
//! arrive, so a whole book never has to be in memory; the file appears under its final name
//! only when the export finishes.

use crate::commands::core::{respond, AppCore, CommandMeta, Wire};
use crate::error::{AppError, AppResult};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{State, WebviewWindow};
use ts_rs::TS;

const MAX_PAGES: usize = 20_000;
const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
const MAX_PAGE_PT: f64 = 14_400.0;

pub struct PdfWriter {
    file: std::io::BufWriter<std::fs::File>,
    partial: PathBuf,
    target: PathBuf,
    pos: u64,
    /// Byte offset of every object, index = object number (0 unused).
    offsets: Vec<u64>,
    pages: Vec<u32>,
}

/// (width, height, components) from the JPEG's own frame header.
pub fn jpeg_size(data: &[u8]) -> Option<(u32, u32, u8)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 || i + 2 + len > data.len() {
            return None;
        }
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            if len < 8 {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            let comps = data[i + 9];
            return (w > 0 && h > 0 && matches!(comps, 1 | 3)).then_some((w, h, comps));
        }
        i += 2 + len;
    }
    None
}

pub struct Placed<'a> {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub jpeg: &'a [u8],
}

impl PdfWriter {
    pub fn create(target: PathBuf) -> AppResult<Self> {
        let partial = target.with_extension("pdf.partial");
        let file = std::fs::File::create(&partial)?;
        let mut w = Self { file: std::io::BufWriter::new(file), partial, target, pos: 0, offsets: vec![0, 0, 0], pages: Vec::new() };
        w.put(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")?;
        Ok(w)
    }

    fn put(&mut self, b: &[u8]) -> AppResult<()> {
        self.file.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    fn begin_obj(&mut self) -> AppResult<u32> {
        let n = self.offsets.len() as u32;
        self.offsets.push(self.pos);
        self.put(format!("{n} 0 obj\n").as_bytes())?;
        Ok(n)
    }

    /// Append one page. Coordinates are points from the top-left corner.
    pub fn add_page(&mut self, width: f64, height: f64, images: &[Placed<'_>]) -> AppResult<()> {
        let ok = |v: f64| v.is_finite() && v >= 0.0 && v <= MAX_PAGE_PT;
        if self.pages.len() >= MAX_PAGES || !(ok(width) && ok(height)) || width < 1.0 || height < 1.0 || images.is_empty() || images.len() > 8 {
            return Err(AppError::limit("PDF 쪽 정보가 올바르지 않습니다."));
        }
        let mut names = Vec::new();
        let mut content = String::new();
        for (k, im) in images.iter().enumerate() {
            if !(ok(im.x) && ok(im.y) && ok(im.w) && ok(im.h)) || im.w < 0.5 || im.h < 0.5 || im.jpeg.len() > MAX_IMAGE_BYTES {
                return Err(AppError::limit("PDF 이미지 정보가 올바르지 않습니다."));
            }
            let (pw, ph, comps) = jpeg_size(im.jpeg).ok_or_else(|| AppError::invalid("이미지 형식이 올바르지 않습니다."))?;
            let n = self.begin_obj()?;
            let cs = if comps == 1 { "/DeviceGray" } else { "/DeviceRGB" };
            self.put(format!("<< /Type /XObject /Subtype /Image /Width {pw} /Height {ph} /ColorSpace {cs} /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n", im.jpeg.len()).as_bytes())?;
            self.put(im.jpeg)?;
            self.put(b"\nendstream\nendobj\n")?;
            names.push((k, n));
            // PDF's origin is the bottom-left corner.
            content.push_str(&format!("q {:.3} 0 0 {:.3} {:.3} {:.3} cm /Im{k} Do Q\n", im.w, im.h, im.x, height - im.y - im.h));
        }
        let c = self.begin_obj()?;
        self.put(format!("<< /Length {} >>\nstream\n{}endstream\nendobj\n", content.len(), content).as_bytes())?;
        let p = self.begin_obj()?;
        let xobj: String = names.iter().map(|(k, n)| format!("/Im{k} {n} 0 R ")).collect();
        self.put(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width:.3} {height:.3}] /Resources << /XObject << {xobj}>> >> /Contents {c} 0 R >>\nendobj\n").as_bytes())?;
        self.pages.push(p);
        Ok(())
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Write the catalog, page tree and cross-reference table, then move the file into place.
    pub fn finish(mut self, title: &str) -> AppResult<usize> {
        if self.pages.is_empty() {
            return Err(AppError::invalid("내보낼 쪽이 없습니다."));
        }
        self.offsets[2] = self.pos;
        let kids: String = self.pages.iter().map(|p| format!("{p} 0 R ")).collect();
        self.put(format!("2 0 obj\n<< /Type /Pages /Count {} /Kids [{kids}] >>\nendobj\n", self.pages.len()).as_bytes())?;
        self.offsets[1] = self.pos;
        self.put(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n")?;
        // Title as UTF-16BE hex so any script is valid.
        let info = self.begin_obj()?;
        let hex: String = std::iter::once(0xFEFFu16).chain(title.chars().take(200).collect::<String>().encode_utf16()).map(|u| format!("{u:04X}")).collect();
        self.put(format!("<< /Title <{hex}> /Producer (Thoth Reader) >>\nendobj\n").as_bytes())?;
        let xref = self.pos;
        let n = self.offsets.len();
        let mut t = format!("xref\n0 {n}\n0000000000 65535 f \n");
        for off in &self.offsets[1..] {
            t.push_str(&format!("{off:010} 00000 n \n"));
        }
        t.push_str(&format!("trailer\n<< /Size {n} /Root 1 0 R /Info {info} 0 R >>\nstartxref\n{xref}\n%%EOF\n"));
        self.put(t.as_bytes())?;
        self.file.flush()?;
        let pages = self.pages.len();
        drop(self.file);
        std::fs::rename(&self.partial, &self.target)?;
        Ok(pages)
    }

    pub fn abort(self) {
        let p = self.partial.clone();
        drop(self.file);
        let _ = std::fs::remove_file(p);
    }
}

/// Open exports, by token. At most a couple at a time; an abandoned one is dropped when the
/// next begins.
#[derive(Default)]
pub struct PdfExports {
    open: std::sync::Mutex<std::collections::HashMap<String, (PdfWriter, String)>>,
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PdfBeginArgs {
    pub session_id: String,
    /// "side-by-side" | "translation" — only used for the suggested file name.
    pub variant: String,
}

#[tauri::command]
pub async fn export_pdf_begin(core: State<'_, Arc<AppCore>>, window: WebviewWindow, meta: CommandMeta, args: PdfBeginArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        let doc = crate::storage::repo::documents::get(&core.db.reader, &s.document_id).await?.ok_or_else(|| AppError::not_found("문서"))?;
        let title = doc.metadata.get("title").and_then(|v| v.as_str()).or_else(|| doc.metadata.get("pdfTitle").and_then(|v| v.as_str())).unwrap_or(&doc.title).to_string();
        let lang = s.state.lock().unwrap().target_language.clone();
        let suffix = if args.variant == "side-by-side" { format!("원문+{lang}") } else { lang };
        let Some(path) = super::export::pick_save_path(&window, format!("{} ({suffix}).pdf", super::export::safe_file_stem(&title)), "pdf").await else {
            return Ok(serde_json::json!({"token": null}));
        };
        let writer = tokio::task::spawn_blocking(move || PdfWriter::create(path)).await.map_err(|_| AppError::internal("내보내기 작업이 중단되었습니다."))??;
        let token = crate::domain::new_uuid();
        let mut open = core.pdf_exports.open.lock().unwrap();
        // Anything left over from an export that never finished is discarded.
        for (_, (w, _)) in open.drain() {
            w.abort();
        }
        open.insert(token.clone(), (writer, title));
        Ok(serde_json::json!({"token": token}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PdfImage {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub jpeg_base64: String,
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PdfPageArgs {
    pub token: String,
    pub width: f64,
    pub height: f64,
    pub images: Vec<PdfImage>,
}

#[tauri::command]
pub async fn export_pdf_page(core: State<'_, Arc<AppCore>>, meta: CommandMeta, args: PdfPageArgs) -> Result<Wire, ()> {
    let r = (|| {
        crate::commands::core::check_id(&args.token, "token")?;
        if args.images.len() > 8 || args.images.iter().any(|i| i.jpeg_base64.len() > MAX_IMAGE_BYTES * 4 / 3 + 8) {
            return Err(AppError::limit("PDF 이미지가 너무 큽니다."));
        }
        let decoded: Vec<Vec<u8>> = args
            .images
            .iter()
            .map(|i| base64::engine::general_purpose::STANDARD.decode(&i.jpeg_base64).map_err(|_| AppError::invalid("이미지 형식이 올바르지 않습니다.")))
            .collect::<AppResult<_>>()?;
        let placed: Vec<Placed<'_>> = args.images.iter().zip(decoded.iter()).map(|(i, d)| Placed { x: i.x, y: i.y, w: i.w, h: i.h, jpeg: d }).collect();
        let mut open = core.pdf_exports.open.lock().unwrap();
        let (w, _) = open.get_mut(&args.token).ok_or_else(|| AppError::not_found("내보내기"))?;
        w.add_page(args.width, args.height, &placed)?;
        Ok(serde_json::json!({"pages": w.page_count()}))
    })();
    Ok(respond(&meta, r))
}

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PdfEndArgs {
    pub token: String,
    /// true: discard the partial file instead of finishing it.
    pub abort: bool,
}

#[tauri::command]
pub async fn export_pdf_end(core: State<'_, Arc<AppCore>>, meta: CommandMeta, args: PdfEndArgs) -> Result<Wire, ()> {
    let r = async {
        crate::commands::core::check_id(&args.token, "token")?;
        let Some((writer, title)) = core.pdf_exports.open.lock().unwrap().remove(&args.token) else {
            return Err(AppError::not_found("내보내기"));
        };
        if args.abort || writer.page_count() == 0 {
            writer.abort();
            return Ok(serde_json::json!({"saved": false}));
        }
        let pages = tokio::task::spawn_blocking(move || writer.finish(&title)).await.map_err(|_| AppError::internal("내보내기 작업이 중단되었습니다."))??;
        Ok(serde_json::json!({"saved": true, "pages": pages}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest useful JPEG header: SOI, APP0, SOF0 (w x h, 3 components), EOI.
    fn fake_jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x4A, 0x46, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.push(3);
        v.extend_from_slice(&[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    #[test]
    fn jpeg_header_is_parsed_and_junk_rejected() {
        assert_eq!(jpeg_size(&fake_jpeg(1224, 1584)), Some((1224, 1584, 3)));
        assert_eq!(jpeg_size(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xC0, 0xFF, 0xFF, 0, 0]), None, "segment longer than the data");
    }

    #[test]
    fn writes_a_structurally_valid_pdf() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("out.pdf");
        let mut w = PdfWriter::create(target.clone()).unwrap();
        let img = fake_jpeg(100, 200);
        w.add_page(1240.0, 792.0, &[Placed { x: 0.0, y: 0.0, w: 612.0, h: 792.0, jpeg: &img }, Placed { x: 628.0, y: 0.0, w: 612.0, h: 792.0, jpeg: &img }]).unwrap();
        w.add_page(595.0, 842.0, &[Placed { x: 0.0, y: 0.0, w: 595.0, h: 842.0, jpeg: &img }]).unwrap();
        assert!(!target.exists(), "only the partial file exists until finish");
        assert!(w.add_page(f64::NAN, 10.0, &[Placed { x: 0.0, y: 0.0, w: 1.0, h: 1.0, jpeg: &img }]).is_err());
        assert!(w.add_page(100.0, 100.0, &[Placed { x: 0.0, y: 0.0, w: 10.0, h: 10.0, jpeg: b"not a jpeg" }]).is_err());
        assert_eq!(w.finish("제목 Title").unwrap(), 2);
        let bytes = std::fs::read(&target).unwrap();
        assert!(bytes.starts_with(b"%PDF-1.7"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/Type /Pages /Count 2"));
        assert!(text.trim_end().ends_with("%%EOF"));
        // every xref offset points at "<n> 0 obj"
        let xref_at: usize = text[text.rfind("startxref").unwrap() + 9..].trim().lines().next().unwrap().trim().parse().unwrap();
        // offsets are byte offsets: slice the bytes, not the lossy text
        let table_owned = String::from_utf8_lossy(&bytes[xref_at..]).to_string();
        let table = table_owned.as_str();
        assert!(table.starts_with("xref\n0 "));
        let entries: Vec<&str> = table.lines().skip(3).take_while(|l| l.ends_with(" n ") || l.ends_with(" f ")).collect();
        for (i, line) in entries.iter().enumerate() {
            let off: usize = line[..10].parse().unwrap();
            let want = format!("{} 0 obj", i + 1);
            assert_eq!(&bytes[off..off + want.len()], want.as_bytes(), "object {}", i + 1);
        }
        assert!(!d.path().join("out.pdf.partial").exists());
        // abort leaves nothing behind
        let w2 = PdfWriter::create(d.path().join("gone.pdf")).unwrap();
        w2.abort();
        assert!(!d.path().join("gone.pdf").exists() && !d.path().join("gone.pdf.partial").exists());
    }
}
