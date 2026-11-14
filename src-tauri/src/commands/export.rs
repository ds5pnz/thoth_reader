//! Export of a translated range as a standalone document (HTML, Markdown or plain text).
//! Text only: figures are not copied. The file is written by Rust to a path the user picks
//! in the native save dialog; the webview never sees a path.

use crate::commands::core::{check_ids, respond, AppCore, CommandMeta, Wire};
use crate::domain::{Block, BlockKind, SafeNode, SourceLocation};
use crate::error::AppError;
use crate::storage::repo::{documents, extraction};
use crate::translation::inline_marks::Mark;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{State, WebviewWindow};
use ts_rs::TS;

#[derive(Debug, Deserialize, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct ExportArgs {
    pub session_id: String,
    pub section_ids: Vec<String>,
    /// "html" | "markdown" | "text"
    pub format: String,
    /// Put the source paragraph before each translation.
    pub bilingual: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Html,
    Markdown,
    Text,
}

pub struct Item {
    pub block: Block,
    /// Translation text and its emphasis ranges; `None` when not translated.
    pub translation: Option<(String, Vec<Mark>)>,
}

const NON_BODY: [&str; 3] = ["running_header", "running_footer", "page_number"];

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

/// Pieces of `text` with the marks that cover each (bold, italic, colour).
fn pieces(text: &str, marks: &[Mark]) -> Vec<(String, bool, bool, Option<String>)> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let len = units.len() as u32;
    let valid: Vec<&Mark> = marks.iter().filter(|m| m.start_u16 < m.end_u16 && m.end_u16 <= len).collect();
    let mut cuts: Vec<u32> = vec![0, len];
    for m in &valid {
        cuts.push(m.start_u16);
        cuts.push(m.end_u16);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let Ok(piece) = String::from_utf16(&units[a as usize..b as usize]) else { continue };
        let covers = |k: &dyn Fn(&str) -> bool| valid.iter().find(|m| k(&m.kind) && m.start_u16 <= a && m.end_u16 >= b).map(|m| m.kind.clone());
        let bold = covers(&|k| k == "b").is_some();
        let italic = covers(&|k| k == "i").is_some();
        let color = covers(&|k| k.starts_with("c:")).map(|k| k[2..].to_string()).filter(|c| crate::domain::is_hex_color(c));
        out.push((piece, bold, italic, color));
    }
    out
}

pub fn inline_html(text: &str, marks: &[Mark]) -> String {
    inline(text, marks, Format::Html)
}

fn inline(text: &str, marks: &[Mark], f: Format) -> String {
    match f {
        Format::Text => text.to_string(),
        Format::Html => pieces(text, marks)
            .into_iter()
            .map(|(p, b, i, c)| {
                let mut s = esc(&p);
                if i {
                    s = format!("<em>{s}</em>");
                }
                if b {
                    s = format!("<strong>{s}</strong>");
                }
                if let Some(c) = c {
                    s = format!("<span style=\"color:{c}\">{s}</span>");
                }
                s
            })
            .collect(),
        Format::Markdown => pieces(text, marks)
            .into_iter()
            .map(|(p, b, i, _)| {
                // Markdown emphasis must hug the words: keep outer spaces outside the markers.
                let core = p.trim();
                if core.is_empty() || !(b || i) {
                    return p;
                }
                let lead = &p[..p.len() - p.trim_start().len()];
                let trail = &p[p.trim_end().len()..];
                let m = match (b, i) {
                    (true, true) => "***",
                    (true, false) => "**",
                    _ => "*",
                };
                format!("{lead}{m}{core}{m}{trail}")
            })
            .collect(),
    }
}

fn source_inline(nodes: &[SafeNode], f: Format, out: &mut String) {
    for n in nodes {
        match n {
            SafeNode::Text { v } | SafeNode::InlineCode { v } => out.push_str(&if f == Format::Html { esc(v) } else { v.clone() }),
            SafeNode::Strong { c } | SafeNode::Emphasis { c } | SafeNode::Color { c, .. } | SafeNode::Link { c, .. } | SafeNode::TableCell { c, .. } => source_inline(c, f, out),
            SafeNode::Break => out.push(if f == Format::Html { ' ' } else { '\n' }),
            SafeNode::ImageRef { .. } | SafeNode::MathRef { .. } => {}
        }
    }
}

fn heading_level(b: &Block) -> usize {
    b.roles.iter().find_map(|r| r.strip_prefix('h').and_then(|n| n.parse::<usize>().ok())).filter(|n| (1..=6).contains(n)).unwrap_or(2)
}

fn page_of(b: &Block) -> Option<u32> {
    match &b.location {
        SourceLocation::Pdf { page_index, .. } => Some(*page_index),
        _ => None,
    }
}

/// Render the document. Pure (unit-tested).
pub fn render(title: &str, lang: &str, items: &[Item], format: Format, bilingual: bool) -> String {
    let mut o = String::new();
    match format {
        Format::Html => {
            o.push_str(&format!(
                "<!doctype html>\n<html lang=\"{}\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>\nbody{{max-width:44rem;margin:2.5rem auto;padding:0 1.2rem;font:17px/1.7 -apple-system,\"Apple SD Gothic Neo\",\"Noto Sans KR\",sans-serif;color:#1f2328}}\nh1,h2,h3,h4{{line-height:1.3;margin:2em 0 .6em}}\npre{{background:#f4f2ee;padding:.8em 1em;overflow:auto;font:13px/1.5 ui-monospace,Menlo,monospace;white-space:pre-wrap}}\nblockquote{{margin:1em 0;padding-left:1em;border-left:3px solid #d8d4cc;color:#444}}\n.src{{color:#7a808a;font-size:.88em;margin-bottom:.2em}}\n.pg{{margin:2.2em 0 1em;text-align:center;color:#9aa0a8;font-size:.8em}}\n.untranslated{{color:#7a808a}}\ntable{{border-collapse:collapse;margin:1em 0}}td,th{{border:1px solid #d8d4cc;padding:.3em .6em;vertical-align:top}}\n@media print{{body{{margin:0;max-width:none}}.pg{{break-before:page}}}}\n</style>\n</head>\n<body>\n<h1>{}</h1>\n",
                esc(lang),
                esc(title),
                esc(title)
            ));
        }
        Format::Markdown => o.push_str(&format!("# {}\n\n", title.replace('\n', " "))),
        Format::Text => o.push_str(&format!("{title}\n\n")),
    }
    let mut last_page: Option<u32> = None;
    let mut in_list = false;
    let close_list = |o: &mut String, in_list: &mut bool| {
        if *in_list && format == Format::Html {
            o.push_str("</ul>\n");
        }
        *in_list = false;
    };
    for it in items {
        let b = &it.block;
        if b.roles.iter().any(|r| NON_BODY.contains(&r.as_str())) {
            continue;
        }
        if let Some(p) = page_of(b) {
            if last_page != Some(p) {
                close_list(&mut o, &mut in_list);
                match format {
                    Format::Html => o.push_str(&format!("<div class=\"pg\">— {} —</div>\n", p + 1)),
                    Format::Markdown => o.push_str(&format!("<!-- p. {} -->\n\n", p + 1)),
                    Format::Text => o.push_str(&format!("[{}]\n\n", p + 1)),
                }
                last_page = Some(p);
            }
        }
        if b.kind != BlockKind::ListItem {
            close_list(&mut o, &mut in_list);
        }
        let mut src = String::new();
        source_inline(&b.content, format, &mut src);
        if src.trim().is_empty() {
            src = if format == Format::Html { esc(&b.source_text) } else { b.source_text.clone() };
        }
        // What to show as the body of this block.
        let translated = it.translation.as_ref().filter(|(t, _)| !t.trim().is_empty());
        let body = match translated {
            Some((t, m)) => inline(t, m, format),
            None => src.clone(),
        };
        let show_src = bilingual && translated.is_some();
        match (b.kind, format) {
            (BlockKind::Figure, _) | (BlockKind::Formula, _) if src.trim().is_empty() => {}
            (BlockKind::Heading, Format::Html) => {
                let n = heading_level(b).max(2);
                if show_src {
                    o.push_str(&format!("<h{n}>{body}</h{n}>\n<p class=\"src\">{src}</p>\n"));
                } else {
                    o.push_str(&format!("<h{n}>{body}</h{n}>\n"));
                }
            }
            (BlockKind::Heading, Format::Markdown) => {
                o.push_str(&format!("{} {}\n\n", "#".repeat(heading_level(b).max(2)), body.replace('\n', " ")));
                if show_src {
                    o.push_str(&format!("> {}\n\n", src.replace('\n', " ")));
                }
            }
            (BlockKind::Code, Format::Html) => o.push_str(&format!("<pre>{}</pre>\n", esc(&b.raw_text))),
            (BlockKind::Code, Format::Markdown) => o.push_str(&format!("```\n{}\n```\n\n", b.raw_text.trim_end())),
            (BlockKind::Code, Format::Text) => o.push_str(&format!("{}\n\n", b.raw_text.trim_end())),
            (BlockKind::Table, Format::Html) => {
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
            (BlockKind::Table, _) => o.push_str(&format!("{}\n\n", b.raw_text.replace('\t', " | "))),
            (BlockKind::ListItem, Format::Html) => {
                if !in_list {
                    o.push_str("<ul>\n");
                    in_list = true;
                }
                if show_src {
                    o.push_str(&format!("<li><div class=\"src\">{src}</div>{body}</li>\n"));
                } else {
                    o.push_str(&format!("<li>{body}</li>\n"));
                }
            }
            (BlockKind::ListItem, Format::Markdown) => {
                in_list = true;
                let t = body.trim_start_matches(['•', '-', '*', '·', ' ']);
                o.push_str(&format!("- {}\n", t.replace('\n', " ")));
                if show_src {
                    o.push_str(&format!("  > {}\n", src.replace('\n', " ")));
                }
            }
            (BlockKind::Quote, Format::Html) => o.push_str(&format!("<blockquote>{}{body}</blockquote>\n", if show_src { format!("<div class=\"src\">{src}</div>") } else { String::new() })),
            (BlockKind::Quote, Format::Markdown) => o.push_str(&format!("> {}\n\n", body.replace('\n', " "))),
            (_, Format::Html) => {
                let cls = if translated.is_none() && b.translation_policy == crate::domain::TranslationPolicy::Auto { " class=\"untranslated\"" } else { "" };
                if show_src {
                    o.push_str(&format!("<p class=\"src\">{src}</p>\n"));
                }
                o.push_str(&format!("<p{cls}>{body}</p>\n"));
            }
            (_, Format::Markdown) => {
                if in_list {
                    o.push('\n');
                    in_list = false;
                }
                if show_src {
                    o.push_str(&format!("> {}\n\n", src.replace('\n', " ")));
                }
                o.push_str(&format!("{body}\n\n"));
            }
            (_, Format::Text) => {
                if show_src {
                    o.push_str(&format!("{src}\n"));
                }
                o.push_str(&format!("{body}\n\n"));
            }
        }
    }
    close_list(&mut o, &mut in_list);
    if format == Format::Html {
        o.push_str("</body>\n</html>\n");
    }
    o
}

/// Native "save as" dialog; `None` when the user cancels. The webview never sees the path.
pub async fn pick_save_path(window: &WebviewWindow, file_name: String, ext: &'static str) -> Option<std::path::PathBuf> {
    use tauri_plugin_dialog::DialogExt;
    // Dev-only: self-tests cannot drive a native dialog.
    #[cfg(debug_assertions)]
    if let Ok(dir) = std::env::var("THOTH_SELFTEST_EXPORT_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        return Some(std::path::PathBuf::from(dir).join(format!("{}.{ext}", crate::domain::new_uuid())));
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    window.dialog().file().set_parent(window).set_file_name(file_name).add_filter(ext, &[ext]).save_file(move |p| {
        let _ = tx.send(p);
    });
    rx.await.ok().flatten().and_then(|p| p.into_path().ok())
}

pub fn safe_file_stem(title: &str) -> String {
    let s: String = title.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(80).collect();
    if s.is_empty() { "translation".into() } else { s }
}

#[tauri::command]
pub async fn export_translation(core: State<'_, Arc<AppCore>>, window: WebviewWindow, meta: CommandMeta, args: ExportArgs) -> Result<Wire, ()> {
    let r = async {
        let s = core.sessions.get(&args.session_id, window.label())?;
        check_ids(&args.section_ids, 100_000, "sectionIds")?;
        let epub = args.format == "epub";
        let format = match args.format.as_str() {
            "epub" => Format::Html,
            "html" => Format::Html,
            "markdown" => Format::Markdown,
            "text" => Format::Text,
            _ => return Err(AppError::invalid("내보내기 형식이 올바르지 않습니다.")),
        };
        let ex = s.extraction()?;
        let wanted: std::collections::HashSet<&String> = args.section_ids.iter().collect();
        let sections: Vec<String> = extraction::list_sections(&core.db.reader, &ex).await?.into_iter().filter(|x| wanted.contains(&x.id) && x.state == "ready").map(|x| x.id).collect();
        if sections.is_empty() {
            return Err(AppError::invalid("내보낼 범위에 추출된 본문이 없습니다."));
        }
        let mut items: Vec<Item> = Vec::new();
        let mut per_section: Vec<(String, usize)> = Vec::new(); // (section id, first item index)
        for sec in &sections {
            per_section.push((sec.clone(), items.len()));
            let mut after: Option<u32> = None;
            loop {
                let page = extraction::blocks_of_section(&core.db.reader, &ex, sec, after, 300).await?;
                let Some(last) = page.last() else { break };
                after = Some(last.ordinal);
                let n = page.len();
                let display = core.translation.display_for_blocks(&s, &page).await?;
                let mut by: std::collections::HashMap<String, (String, Vec<Mark>)> = display.into_iter().filter_map(|d| d.shown.map(|r| (d.block_id, (r.text, r.marks)))).collect();
                for b in page {
                    // Containers repeat their children's text: export leaves only.
                    if !extraction::is_leaf(&core.db.reader, &b.id).await? {
                        continue;
                    }
                    let translation = by.remove(&b.id);
                    items.push(Item { block: b, translation });
                }
                if n < 300 {
                    break;
                }
            }
        }
        let translatable = items.iter().filter(|i| i.block.translation_policy == crate::domain::TranslationPolicy::Auto && i.block.source_text.chars().any(|c| c.is_alphabetic())).count();
        let untranslated = items
            .iter()
            .filter(|i| i.translation.is_none() && i.block.translation_policy == crate::domain::TranslationPolicy::Auto && i.block.source_text.chars().any(|c| c.is_alphabetic()))
            .count();
        let doc = documents::get(&core.db.reader, &s.document_id).await?.ok_or_else(|| AppError::not_found("문서"))?;
        let title = doc.metadata.get("title").and_then(|v| v.as_str()).or_else(|| doc.metadata.get("pdfTitle").and_then(|v| v.as_str())).unwrap_or(&doc.title).to_string();
        let lang = s.state.lock().unwrap().target_language.clone();
        if epub {
            let report = serde_json::json!({"saved": true, "paragraphs": items.len(), "translatable": translatable, "untranslated": untranslated});
            let Some(path) = pick_save_path(&window, format!("{} ({lang}).epub", safe_file_stem(&title)), "epub").await else {
                return Ok(serde_json::json!({"saved": false}));
            };
            // Chapters: one per section, titled from the table of contents where it points there.
            let ordinals: std::collections::HashMap<String, i64> = extraction::list_sections(&core.db.reader, &ex).await?.into_iter().map(|x| (x.id, x.ordinal)).collect();
            let mut toc_titles: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
            fn walk(v: &serde_json::Value, out: &mut std::collections::HashMap<i64, String>) {
                for e in v.as_array().into_iter().flatten() {
                    if let (Some(i), Some(l)) = (e.get("spineIndex").and_then(|x| x.as_i64()), e.get("label").and_then(|x| x.as_str())) {
                        out.entry(i).or_insert_with(|| l.trim().to_string());
                    }
                    if let Some(c) = e.get("children") {
                        walk(c, out);
                    }
                }
            }
            if let Some(t) = doc.metadata.get("toc") {
                walk(t, &mut toc_titles);
            }
            let mut all = items;
            let mut chapters: Vec<super::epub_out::Chapter> = Vec::new();
            for (k, (sec, start)) in per_section.iter().enumerate().rev() {
                let tail = all.split_off(*start);
                if tail.is_empty() {
                    continue;
                }
                let ord = ordinals.get(sec).copied().unwrap_or(k as i64);
                let from_heading = tail.iter().find(|i| i.block.kind == BlockKind::Heading).map(|i| i.translation.as_ref().map(|t| t.0.clone()).unwrap_or_else(|| i.block.source_text.clone()));
                let title_c = from_heading.or_else(|| toc_titles.get(&ord).cloned()).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| format!("{}", ord + 1));
                chapters.push(super::epub_out::Chapter { title: title_c.chars().take(200).collect(), items: tail });
            }
            chapters.reverse();
            // The book's own images (sanitised copies), only those the exported blocks show.
            let mut images: std::collections::BTreeMap<String, super::epub_out::Image> = std::collections::BTreeMap::new();
            if let (Some(dir), Some(map)) = (s.derived_dir.read().unwrap().clone(), doc.metadata.get("resources").and_then(|v| v.as_object())) {
                fn ids(nodes: &[SafeNode], out: &mut Vec<String>) {
                    for n in nodes {
                        match n {
                            SafeNode::ImageRef { resource_id, .. } | SafeNode::MathRef { resource_id, .. } => out.push(resource_id.clone()),
                            SafeNode::Strong { c } | SafeNode::Emphasis { c } | SafeNode::Color { c, .. } | SafeNode::Link { c, .. } | SafeNode::TableCell { c, .. } => ids(c, out),
                            _ => {}
                        }
                    }
                }
                let mut wanted_ids = Vec::new();
                for ch in &chapters {
                    for it in &ch.items {
                        ids(&it.block.content, &mut wanted_ids);
                    }
                }
                let mut total: u64 = 0;
                for id in wanted_ids {
                    if images.contains_key(&id) || !crate::domain::is_valid_id(&id) && !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                        continue;
                    }
                    let Some(href) = map.get(&id).and_then(|v| v.as_str()) else { continue };
                    let mime = match href.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
                        Some("png") => "image/png",
                        Some("jpg") | Some("jpeg") => "image/jpeg",
                        Some("gif") => "image/gif",
                        Some("webp") => "image/webp",
                        _ => continue, // SVG and anything else stays out
                    };
                    let Some(p) = crate::document::paths::safe_join(&dir, href) else { continue };
                    let Ok(bytes) = std::fs::read(&p) else { continue };
                    total += bytes.len() as u64;
                    if total > 600 * 1024 * 1024 {
                        break;
                    }
                    let ext = href.rsplit('.').next().unwrap_or("img").to_ascii_lowercase();
                    images.insert(id.clone(), super::epub_out::Image { name: format!("{id}.{ext}"), mime, bytes });
                }
            }
            let authors: Vec<String> = doc.metadata.get("authors").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
            let (doc_id, bilingual) = (doc.id.clone(), args.bilingual);
            tokio::task::spawn_blocking(move || {
                let partial = path.with_extension("epub.partial");
                let book = super::epub_out::Book { id: &doc_id, title: &title, language: &lang, authors: &authors, bilingual };
                let r = std::fs::File::create(&partial).map_err(AppError::from).and_then(|f| super::epub_out::write_epub(std::io::BufWriter::new(f), &book, &chapters, &images));
                match r {
                    Ok(()) => std::fs::rename(&partial, &path).map_err(AppError::from),
                    Err(e) => {
                        let _ = std::fs::remove_file(&partial);
                        Err(e)
                    }
                }
            })
            .await
            .map_err(|_| AppError::internal("내보내기 작업이 중단되었습니다."))??;
            return Ok(report);
        }
        let text = render(&title, &lang, &items, format, args.bilingual);
        let ext = match format {
            Format::Html => "html",
            Format::Markdown => "md",
            Format::Text => "txt",
        };
        let Some(path) = pick_save_path(&window, format!("{} ({lang}).{ext}", safe_file_stem(&title)), ext).await else {
            return Ok(serde_json::json!({"saved": false}));
        };
        tokio::task::spawn_blocking(move || std::fs::write(&path, text.as_bytes()).map(|_| path))
            .await
            .map_err(|_| AppError::internal("내보내기 작업이 중단되었습니다."))?
            .map_err(|_| AppError::internal("파일을 저장할 수 없습니다."))?;
        Ok(serde_json::json!({"saved": true, "paragraphs": items.len(), "translatable": translatable, "untranslated": untranslated}))
    }
    .await;
    Ok(respond(&meta, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;

    fn block(kind: BlockKind, text: &str, page: u32, roles: &[&str]) -> Block {
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
            content: vec![SafeNode::Text { v: text.into() }],
            location: SourceLocation::Pdf { page_index: page, view_box: vec![0.0, 0.0, 1.0, 1.0], rotation: 0, rects: vec![], item_ranges: vec![] },
            quality: Quality { level: QualityLevel::High, reasons: vec![] },
            translation_policy: if kind == BlockKind::Code { TranslationPolicy::Preserve } else { TranslationPolicy::Auto },
            mapping: BlockMapping::default(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
        }
    }

    fn items() -> Vec<Item> {
        vec![
            Item { block: block(BlockKind::Paragraph, "Header text", 0, &["running_header"]), translation: None },
            Item { block: block(BlockKind::Heading, "A <Title>", 0, &["h1"]), translation: Some(("제목 & 부제".into(), vec![])) },
            Item {
                block: block(BlockKind::Paragraph, "See Cloud now.", 0, &[]),
                translation: Some(("지금 클라우드를 보세요.".into(), vec![Mark { start_u16: 3, end_u16: 7, kind: "b".into() }, Mark { start_u16: 3, end_u16: 7, kind: "c:#b00000".into() }])),
            },
            Item { block: block(BlockKind::ListItem, "one", 1, &[]), translation: Some(("하나".into(), vec![])) },
            Item { block: block(BlockKind::ListItem, "two", 1, &[]), translation: None },
            Item { block: block(BlockKind::Code, "let x = 1 < 2;", 1, &[]), translation: None },
        ]
    }

    #[test]
    fn html_is_escaped_structured_and_keeps_marks() {
        let h = render("My <Book>", "ko", &items(), Format::Html, false);
        assert!(h.starts_with("<!doctype html>"));
        assert!(h.contains("<title>My &lt;Book&gt;</title>"));
        assert!(!h.contains("Header text"), "running headers are not exported");
        assert!(h.contains("<h2>제목 &amp; 부제</h2>"));
        assert!(h.contains("지금 <span style=\"color:#b00000\"><strong>클라우드</strong></span>를 보세요."));
        assert!(h.contains("<div class=\"pg\">— 1 —</div>") && h.contains("<div class=\"pg\">— 2 —</div>"));
        assert!(h.contains("<ul>\n<li>하나</li>\n<li>two</li>\n</ul>"), "untranslated items fall back to the source");
        assert!(h.contains("<pre>let x = 1 &lt; 2;</pre>"));
        assert!(!h.contains("<script"));
    }

    #[test]
    fn bilingual_markdown_and_text() {
        let m = render("T", "ko", &items(), Format::Markdown, true);
        assert!(m.contains("## 제목 & 부제\n\n> A <Title>\n"));
        assert!(m.contains("지금 **클라우드**를 보세요."));
        assert!(m.contains("- 하나\n  > one\n- two\n"));
        assert!(m.contains("```\nlet x = 1 < 2;\n```"));
        let t = render("T", "ko", &items(), Format::Text, false);
        assert!(t.contains("[1]\n\n제목 & 부제\n\n지금 클라우드를 보세요.\n\n[2]\n\n하나\n\ntwo\n"));
    }

    #[test]
    fn marks_out_of_range_or_malformed_are_ignored() {
        let it = vec![Item {
            block: block(BlockKind::Paragraph, "x", 0, &[]),
            translation: Some(("가나다".into(), vec![Mark { start_u16: 2, end_u16: 99, kind: "b".into() }, Mark { start_u16: 0, end_u16: 1, kind: "c:red;x".into() }])),
        }];
        let h = render("T", "ko", &it, Format::Html, false);
        assert!(h.contains("<p>가나다</p>"));
        assert_eq!(safe_file_stem("a/b:c*?  d"), "a b c d");
    }
}
