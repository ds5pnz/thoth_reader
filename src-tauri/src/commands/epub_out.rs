//! Export of the translation as an EPUB 3 book: one XHTML document per source section, the
//! original chapter titles in the navigation, and the book's own (already sanitised) images.
//! Everything written here is built from the block model and escaped; no source markup is
//! copied through.

use super::export::{esc, inline_html, Item};
use crate::domain::{BlockKind, SafeNode, TranslationPolicy};
use crate::error::{AppError, AppResult};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub struct Chapter {
    pub title: String,
    pub items: Vec<Item>,
}

pub struct Image {
    pub name: String,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

pub struct Book<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub language: &'a str,
    pub authors: &'a [String],
    pub bilingual: bool,
}

const NON_BODY: [&str; 3] = ["running_header", "running_footer", "page_number"];
const CSS: &str = "body{line-height:1.7;margin:0 5%}\nh1,h2,h3,h4{line-height:1.3;margin:1.6em 0 .6em}\npre{white-space:pre-wrap;font-family:monospace;font-size:.85em;background:#f4f2ee;padding:.6em .8em}\nblockquote{margin:1em 0;padding-left:1em;border-left:3px solid #ccc}\nfigure{margin:1em 0;text-align:center}\nimg{max-width:100%;height:auto}\n.src{color:#7a808a;font-size:.88em;margin-bottom:.2em}\n.untranslated{color:#7a808a}\ntable{border-collapse:collapse;margin:1em 0}\ntd{border:1px solid #ccc;padding:.3em .6em;vertical-align:top}\n";

/// Source inline content as XHTML; `images` maps a resource id to its file name in the book.
fn source_xhtml(nodes: &[SafeNode], images: &HashMap<String, String>, out: &mut String) {
    for n in nodes {
        match n {
            SafeNode::Text { v } => out.push_str(&esc(v)),
            SafeNode::InlineCode { v } => out.push_str(&format!("<code>{}</code>", esc(v))),
            SafeNode::Strong { c } => wrap("strong", c, images, out),
            SafeNode::Emphasis { c } => wrap("em", c, images, out),
            SafeNode::Color { v, c } if crate::domain::is_hex_color(v) => {
                out.push_str(&format!("<span style=\"color:{v}\">"));
                source_xhtml(c, images, out);
                out.push_str("</span>");
            }
            SafeNode::Color { c, .. } | SafeNode::Link { c, .. } | SafeNode::TableCell { c, .. } => source_xhtml(c, images, out),
            SafeNode::Break => out.push_str("<br/>"),
            SafeNode::ImageRef { resource_id, alt } | SafeNode::MathRef { resource_id, alt } => {
                if let Some(name) = images.get(resource_id) {
                    out.push_str(&format!("<img src=\"images/{}\" alt=\"{}\"/>", esc(name), esc(alt)));
                } else if !alt.trim().is_empty() {
                    out.push_str(&esc(alt));
                }
            }
        }
    }
}

fn wrap(tag: &str, c: &[SafeNode], images: &HashMap<String, String>, out: &mut String) {
    out.push_str(&format!("<{tag}>"));
    source_xhtml(c, images, out);
    out.push_str(&format!("</{tag}>"));
}

fn has_image(nodes: &[SafeNode]) -> bool {
    nodes.iter().any(|n| match n {
        SafeNode::ImageRef { .. } | SafeNode::MathRef { .. } => true,
        SafeNode::Strong { c } | SafeNode::Emphasis { c } | SafeNode::Color { c, .. } | SafeNode::Link { c, .. } | SafeNode::TableCell { c, .. } => has_image(c),
        _ => false,
    })
}

/// One chapter as a complete XHTML document. Pure (unit-tested).
pub fn chapter_xhtml(ch: &Chapter, book: &Book<'_>, images: &HashMap<String, String>) -> String {
    let mut o = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"{l}\" lang=\"{l}\">\n<head>\n<meta charset=\"utf-8\"/>\n<title>{t}</title>\n<link rel=\"stylesheet\" type=\"text/css\" href=\"style.css\"/>\n</head>\n<body>\n",
        l = esc(book.language),
        t = esc(&ch.title)
    );
    let mut in_list = false;
    for it in &ch.items {
        let b = &it.block;
        if b.roles.iter().any(|r| NON_BODY.contains(&r.as_str())) {
            continue;
        }
        if b.kind != BlockKind::ListItem && in_list {
            o.push_str("</ul>\n");
            in_list = false;
        }
        let mut src = String::new();
        source_xhtml(&b.content, images, &mut src);
        if src.trim().is_empty() {
            src = esc(&b.source_text);
        }
        // A block that carries an image keeps its source form (the image is the content).
        let translated = it.translation.as_ref().filter(|(t, _)| !t.trim().is_empty() && !has_image(&b.content));
        let body = match translated {
            Some((t, m)) => inline_html(t, m),
            None => src.clone(),
        };
        if body.trim().is_empty() {
            continue;
        }
        let show_src = book.bilingual && translated.is_some();
        let src_div = if show_src { format!("<div class=\"src\">{src}</div>") } else { String::new() };
        match b.kind {
            BlockKind::Heading => {
                let n = b.roles.iter().find_map(|r| r.strip_prefix('h').and_then(|x| x.parse::<usize>().ok())).filter(|n| (1..=6).contains(n)).unwrap_or(2);
                o.push_str(&format!("<h{n}>{body}</h{n}>\n"));
                if show_src {
                    o.push_str(&format!("<p class=\"src\">{src}</p>\n"));
                }
            }
            BlockKind::Code => o.push_str(&format!("<pre>{}</pre>\n", esc(&b.raw_text))),
            BlockKind::Table => {
                o.push_str("<table>\n");
                for row in b.raw_text.lines() {
                    o.push_str("<tr>");
                    for cell in row.split('\t') {
                        o.push_str(&format!("<td>{}</td>", esc(cell.trim())));
                    }
                    o.push_str("</tr>\n");
                }
                o.push_str("</table>\n");
            }
            BlockKind::ListItem => {
                if !in_list {
                    o.push_str("<ul>\n");
                    in_list = true;
                }
                o.push_str(&format!("<li>{src_div}{body}</li>\n"));
            }
            BlockKind::Quote => o.push_str(&format!("<blockquote>{src_div}<p>{body}</p></blockquote>\n")),
            BlockKind::Figure => o.push_str(&format!("<figure>{body}</figure>\n")),
            _ => {
                let cls = if translated.is_none() && b.translation_policy == TranslationPolicy::Auto && !has_image(&b.content) { " class=\"untranslated\"" } else { "" };
                if show_src {
                    o.push_str(&format!("<p class=\"src\">{src}</p>\n"));
                }
                o.push_str(&format!("<p{cls}>{body}</p>\n"));
            }
        }
    }
    if in_list {
        o.push_str("</ul>\n");
    }
    o.push_str("</body>\n</html>\n");
    o
}

/// Write the whole book to `out`. `images` keys are resource ids.
pub fn write_epub<W: Write + std::io::Seek>(out: W, book: &Book<'_>, chapters: &[Chapter], images: &BTreeMap<String, Image>) -> AppResult<()> {
    if chapters.is_empty() {
        return Err(AppError::invalid("내보낼 본문이 없습니다."));
    }
    let zerr = |_| AppError::internal("EPUB 파일을 만들 수 없습니다.");
    let mut z = ZipWriter::new(out);
    // The mimetype entry must come first and be stored uncompressed.
    z.start_file("mimetype", SimpleFileOptions::default().compression_method(CompressionMethod::Stored)).map_err(zerr)?;
    z.write_all(b"application/epub+zip")?;
    let deflate = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    z.start_file("META-INF/container.xml", deflate).map_err(zerr)?;
    z.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n<rootfiles>\n<rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\"/>\n</rootfiles>\n</container>\n")?;
    z.start_file("OEBPS/style.css", deflate).map_err(zerr)?;
    z.write_all(CSS.as_bytes())?;

    let names: HashMap<String, String> = images.iter().map(|(id, im)| (id.clone(), im.name.clone())).collect();
    for (i, ch) in chapters.iter().enumerate() {
        z.start_file(format!("OEBPS/c{:04}.xhtml", i + 1), deflate).map_err(zerr)?;
        z.write_all(chapter_xhtml(ch, book, &names).as_bytes())?;
    }
    for im in images.values() {
        z.start_file(format!("OEBPS/images/{}", im.name), stored).map_err(zerr)?;
        z.write_all(&im.bytes)?;
    }
    // Navigation document
    let mut nav = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" xml:lang=\"{l}\" lang=\"{l}\">\n<head><meta charset=\"utf-8\"/><title>{t}</title></head>\n<body>\n<nav epub:type=\"toc\" id=\"toc\">\n<ol>\n",
        l = esc(book.language),
        t = esc(book.title)
    );
    for (i, ch) in chapters.iter().enumerate() {
        nav.push_str(&format!("<li><a href=\"c{:04}.xhtml\">{}</a></li>\n", i + 1, esc(&ch.title)));
    }
    nav.push_str("</ol>\n</nav>\n</body>\n</html>\n");
    z.start_file("OEBPS/nav.xhtml", deflate).map_err(zerr)?;
    z.write_all(nav.as_bytes())?;
    // Package document
    let mut opf = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"bookid\" xml:lang=\"{l}\">\n<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n<dc:identifier id=\"bookid\">urn:uuid:{id}</dc:identifier>\n<dc:title>{t}</dc:title>\n<dc:language>{l}</dc:language>\n",
        l = esc(book.language),
        id = esc(book.id),
        t = esc(book.title)
    );
    for a in book.authors.iter().take(8) {
        opf.push_str(&format!("<dc:creator>{}</dc:creator>\n", esc(a)));
    }
    // dcterms:modified is required; a fixed-format UTC timestamp from the clock.
    let secs = crate::domain::now_ms() / 1000;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    opf.push_str(&format!("<meta property=\"dcterms:modified\">{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z</meta>\n</metadata>\n<manifest>\n<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n<item id=\"css\" href=\"style.css\" media-type=\"text/css\"/>\n", rem / 3600, rem % 3600 / 60, rem % 60));
    for i in 0..chapters.len() {
        opf.push_str(&format!("<item id=\"c{n:04}\" href=\"c{n:04}.xhtml\" media-type=\"application/xhtml+xml\"/>\n", n = i + 1));
    }
    for (k, im) in images.values().enumerate() {
        opf.push_str(&format!("<item id=\"img{k}\" href=\"images/{}\" media-type=\"{}\"/>\n", esc(&im.name), im.mime));
    }
    opf.push_str("</manifest>\n<spine>\n");
    for i in 0..chapters.len() {
        opf.push_str(&format!("<itemref idref=\"c{:04}\"/>\n", i + 1));
    }
    opf.push_str("</spine>\n</package>\n");
    z.start_file("OEBPS/content.opf", deflate).map_err(zerr)?;
    z.write_all(opf.as_bytes())?;
    z.finish().map_err(zerr)?;
    Ok(())
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::translation::inline_marks::Mark;

    fn block(kind: BlockKind, text: &str, content: Vec<SafeNode>, roles: &[&str]) -> Block {
        Block {
            id: "b".into(),
            extraction_id: "e".into(),
            section_id: "s".into(),
            parent_id: None,
            ordinal: 0,
            kind,
            raw_text: text.into(),
            source_text: text.into(),
            source_hash: sha256_hex_str(text),
            content,
            location: SourceLocation::Pdf { page_index: 0, view_box: vec![0.0, 0.0, 1.0, 1.0], rotation: 0, rects: vec![], item_ranges: vec![] },
            quality: Quality { level: QualityLevel::High, reasons: vec![] },
            translation_policy: TranslationPolicy::Auto,
            mapping: BlockMapping::default(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
        }
    }
    fn t(s: &str) -> SafeNode {
        SafeNode::Text { v: s.into() }
    }

    /// Strict XML well-formedness (matching end tags included).
    fn well_formed(x: &str) -> bool {
        let mut r = quick_xml::Reader::from_str(x);
        r.config_mut().check_end_names = true;
        let mut depth = 0i32;
        loop {
            match r.read_event() {
                Ok(quick_xml::events::Event::Start(_)) => depth += 1,
                Ok(quick_xml::events::Event::End(_)) => depth -= 1,
                Ok(quick_xml::events::Event::Eof) => return depth == 0,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    fn sample() -> (Vec<Chapter>, BTreeMap<String, Image>) {
        let ch = Chapter {
            title: "One & <Two>".into(),
            items: vec![
                Item { block: block(BlockKind::Heading, "Title", vec![t("Title")], &["h1"]), translation: Some(("제목".into(), vec![])) },
                Item { block: block(BlockKind::Paragraph, "a < b", vec![t("a < b")], &[]), translation: Some(("가 < 나".into(), vec![Mark { start_u16: 0, end_u16: 1, kind: "b".into() }])) },
                Item { block: block(BlockKind::Figure, "", vec![SafeNode::ImageRef { resource_id: "r1".into(), alt: "A \"cat\"".into() }], &[]), translation: None },
                Item { block: block(BlockKind::Paragraph, "left alone", vec![t("left alone")], &[]), translation: None },
                Item { block: block(BlockKind::Paragraph, "running", vec![t("running")], &["running_header"]), translation: None },
            ],
        };
        let mut images = BTreeMap::new();
        images.insert("r1".to_string(), Image { name: "r1.png".into(), mime: "image/png", bytes: vec![0x89, b'P', b'N', b'G'] });
        (vec![ch], images)
    }

    #[test]
    fn chapter_markup_is_escaped_and_keeps_images() {
        let (chs, _) = sample();
        let names: HashMap<String, String> = [("r1".to_string(), "r1.png".to_string())].into();
        let book = Book { id: "x", title: "T", language: "ko", authors: &[], bilingual: true };
        let x = chapter_xhtml(&chs[0], &book, &names);
        assert!(x.contains("<title>One &amp; &lt;Two&gt;</title>"));
        assert!(x.contains("<h1>제목</h1>\n<p class=\"src\">Title</p>"));
        assert!(x.contains("<p class=\"src\">a &lt; b</p>\n<p><strong>가</strong> &lt; 나</p>"));
        assert!(x.contains("<figure><img src=\"images/r1.png\" alt=\"A &quot;cat&quot;\"/></figure>"));
        assert!(x.contains("<p class=\"untranslated\">left alone</p>"));
        assert!(!x.contains("running"));
        // well-formed: it parses with the app's own strict XML parser
        assert!(well_formed(&x), "{x}");
    }

    #[test]
    fn package_is_a_valid_epub_layout() {
        let (chs, images) = sample();
        let authors = vec!["A. Writer".to_string()];
        let book = Book { id: "0f0e0d0c-0000-4000-8000-000000000000", title: "책 <1>", language: "ko", authors: &authors, bilingual: false };
        let mut buf = std::io::Cursor::new(Vec::new());
        write_epub(&mut buf, &book, &chs, &images).unwrap();
        let bytes = buf.into_inner();
        // mimetype first, stored: its content sits right after the 30-byte local header + name
        assert_eq!(&bytes[30..38], b"mimetype");
        assert_eq!(&bytes[38..58], b"application/epub+zip");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        for want in ["mimetype", "META-INF/container.xml", "OEBPS/content.opf", "OEBPS/nav.xhtml", "OEBPS/style.css", "OEBPS/c0001.xhtml", "OEBPS/images/r1.png"] {
            assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
        }
        let mut opf = String::new();
        std::io::Read::read_to_string(&mut z.by_name("OEBPS/content.opf").unwrap(), &mut opf).unwrap();
        assert!(opf.contains("<dc:title>책 &lt;1&gt;</dc:title>") && opf.contains("<dc:creator>A. Writer</dc:creator>"));
        assert!(opf.contains("properties=\"nav\"") && opf.contains("<itemref idref=\"c0001\"/>") && opf.contains("media-type=\"image/png\""));
        assert!(well_formed(&opf));
        assert!(write_epub(std::io::Cursor::new(Vec::new()), &book, &[], &images).is_err());
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_771), (2026, 11, 14));
    }
}
