//! Integration tests for the EPUB security module (spec §10.2 T05, T07, T08).

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use quick_xml::events::Event;

use super::fixtures::*;
use super::*;
use crate::domain::{BlockKind, SafeNode, SourceLocation, TranslationPolicy};
use crate::error::{AppError, ErrorCode};

const EXTRACTION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Built {
    _dir: tempfile::TempDir,
    out: PathBuf,
    result: Result<SafePackage, AppError>,
}

fn build_bytes(bytes: &[u8], limits: &EpubLimits) -> Built {
    let dir = tempfile::tempdir().unwrap();
    let epub = dir.path().join("in.epub");
    fs::write(&epub, bytes).unwrap();
    let out = dir.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let result = build_safe_package(&epub, &out, limits);
    Built { _dir: dir, out, result }
}

fn read_doc(out: &Path, item: &SafeSpineItem) -> String {
    fs::read_to_string(out.join(&item.safe_href)).unwrap()
}

fn blocks_of(out: &Path, pkg: &SafePackage, item: &SafeSpineItem) -> Vec<crate::domain::Block> {
    let x = read_doc(out, item);
    let section = crate::domain::derive_section_id(EXTRACTION, item.index, &item.safe_href);
    extract_blocks(&x, item.index, &item.idref, &item.safe_href, &pkg.canonical_revision, EXTRACTION, &section).unwrap()
}

fn all_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            all_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

const NAMESPACE_URIS: &[&str] = &[
    "http://www.w3.org/1999/xhtml",
    "http://www.w3.org/2000/svg",
    "http://www.w3.org/1998/Math/MathML",
    "http://www.idpf.org/2007/opf",
    "http://purl.org/dc/elements/1.1/",
    "http://www.idpf.org/vocab/rendition/#",
];

/// Scan every output file: no executable content, no handlers, no non-link URLs.
fn assert_output_safe(out: &Path) {
    let mut files = Vec::new();
    all_files(out, &mut files);
    assert!(!files.is_empty());
    for f in files {
        let bytes = fs::read(&f).unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let lower = text.to_ascii_lowercase();
        let name = f.display().to_string();
        for bad in [
            "<script", "javascript:", "vbscript:", "<iframe", "<object", "<embed", "<form", "<input", "<button",
            "<textarea", "<select", "<base", "refresh", "foreignobject", "@import", "expression(", "-moz-binding",
            "behavior", "position:fixed", "position:sticky", "data:", "file:", "custom-app:", "<video", "<audio",
            "<source", "srcdoc", "<animate", "<set ", "<param", "<meta charset", "/etc/passwd", "2147483647", "99999",
        ] {
            assert!(!lower.contains(bad), "{name} contains {bad:?}:\n{text}");
        }
        // Event handler attributes: ` on<letters>=` inside any tag.
        for (i, _) in lower.match_indices('<') {
            let tag_end = lower[i..].find('>').map(|e| i + e).unwrap_or(lower.len());
            let tag = &lower[i..tag_end];
            for (j, _) in tag.match_indices(" on") {
                let rest = &tag[j + 3..];
                let letters: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
                assert!(
                    letters.is_empty() || !rest[letters.len()..].starts_with('='),
                    "{name} has handler in {tag}"
                );
            }
        }
        // External URLs only as clickable, marked <a href> links.
        for (i, _) in lower.match_indices("http") {
            let rest = &text[i..];
            if !(rest.starts_with("http://") || rest.starts_with("https://")) {
                continue;
            }
            if NAMESPACE_URIS.iter().any(|ns| rest.starts_with(ns)) {
                continue;
            }
            let tag_start = text[..i].rfind('<').unwrap_or(0);
            let tag_end = text[i..].find('>').map(|e| i + e).unwrap_or(text.len());
            let tag = &text[tag_start..tag_end];
            // The policy itself names the app's own document origin on Windows (WebView2
            // addresses the document protocol over http). Nothing else may appear there.
            if rest.starts_with("http://thoth-doc.localhost") && tag.starts_with("<meta http-equiv=\"Content-Security-Policy\"") && tag.contains(CONTENT_CSP) {
                continue;
            }
            assert!(
                tag.starts_with("<a ") && tag.contains("data-thoth-external=\"1\"") && text[..i].ends_with("href=\""),
                "{name}: unexpected external URL in {tag}"
            );
        }
        if name.ends_with(".xhtml") {
            assert!(text.contains(&format!("content=\"{CONTENT_CSP}\"")), "{name} lacks CSP");
            assert!(text.starts_with("<!DOCTYPE html><html xmlns=\"http://www.w3.org/1999/xhtml\""));
        }
    }
}

// ---------------------------------------------------------------------------
// Independent test-side CFI resolvers
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum TNode {
    Elem { id: Option<String>, children: Vec<TNode> },
    Text(String),
}

/// Minimal XML tree builder (independent of the module's parser).
fn xml_tree(xml: &str) -> TNode {
    let mut r = quick_xml::Reader::from_str(xml);
    r.config_mut().trim_text(false);
    let mut stack: Vec<(Option<String>, Vec<TNode>)> = vec![(None, Vec::new())];
    let push_text = |stack: &mut Vec<(Option<String>, Vec<TNode>)>, s: &str| {
        let kids = &mut stack.last_mut().unwrap().1;
        if let Some(TNode::Text(t)) = kids.last_mut() {
            t.push_str(s);
        } else {
            kids.push(TNode::Text(s.to_string()));
        }
    };
    let id_of = |e: &quick_xml::events::BytesStart<'_>| {
        e.attributes().flatten().find(|a| a.key.as_ref() == "id").map(|a| a.value.to_string())
    };
    loop {
        match r.read_event().unwrap() {
            Event::Eof => break,
            Event::Start(e) => stack.push((id_of(&e), Vec::new())),
            Event::Empty(e) => {
                let id = id_of(&e);
                stack.last_mut().unwrap().1.push(TNode::Elem { id, children: Vec::new() });
            }
            Event::End(_) => {
                let (id, children) = stack.pop().unwrap();
                stack.last_mut().unwrap().1.push(TNode::Elem { id, children });
            }
            Event::Text(t) => {
                if stack.len() > 1 {
                    push_text(&mut stack, &t.xml10_content());
                }
            }
            Event::GeneralRef(g) => {
                let name: &str = &g;
                let s = match name {
                    "amp" => "&".to_string(),
                    "lt" => "<".to_string(),
                    "gt" => ">".to_string(),
                    "quot" => "\"".to_string(),
                    "apos" => "'".to_string(),
                    _ => g.resolve_char_ref().unwrap().unwrap().to_string(),
                };
                push_text(&mut stack, &s);
            }
            _ => {}
        }
    }
    stack.pop().unwrap().1.into_iter().find(|n| matches!(n, TNode::Elem { .. })).unwrap()
}

fn parse_steps(path: &str) -> Vec<(u32, Option<String>)> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match s.split_once('[') {
            Some((n, id)) => (n.parse().unwrap(), Some(id.trim_end_matches(']').to_string())),
            None => (s.parse().unwrap(), None),
        })
        .collect()
}

fn xml_resolve(root: &TNode, path: &str) -> Option<String> {
    let mut cur = root;
    for (n, id) in parse_steps(path) {
        let TNode::Elem { children, .. } = cur else { return None };
        if n % 2 == 0 {
            let el = children.iter().filter(|c| matches!(c, TNode::Elem { .. })).nth((n / 2 - 1) as usize)?;
            if let (Some(want), TNode::Elem { id: have, .. }) = (&id, el) {
                assert_eq!(have.as_ref(), Some(want), "id assertion mismatch at {path}");
            }
            cur = el;
        } else {
            cur = children.iter().filter(|c| matches!(c, TNode::Text(_))).nth(((n - 1) / 2) as usize)?;
        }
    }
    match cur {
        TNode::Text(t) => Some(t.clone()),
        _ => None,
    }
}

/// Same resolution over the html5ever rcdom (what epub.js' default srcdoc
/// iframe would build with the HTML parser).
fn rc_resolve(doc: &Handle, path: &str) -> Option<String> {
    let html = doc.children.borrow().iter().find(|c| matches!(c.data, NodeData::Element { .. })).cloned()?;
    let mut cur = html;
    for (n, id) in parse_steps(path) {
        let next = {
            let kids = cur.children.borrow();
            if n % 2 == 0 {
                let el = kids.iter().filter(|c| matches!(c.data, NodeData::Element { .. })).nth((n / 2 - 1) as usize)?.clone();
                if let (Some(want), NodeData::Element { attrs, .. }) = (&id, &el.data) {
                    let have = attrs.borrow().iter().find(|a| &*a.name.local == "id").map(|a| a.value.to_string());
                    assert_eq!(have.as_ref(), Some(want));
                }
                el
            } else {
                kids.iter().filter(|c| matches!(c.data, NodeData::Text { .. })).nth(((n - 1) / 2) as usize)?.clone()
            }
        };
        cur = next;
    }
    match &cur.data {
        NodeData::Text { contents } => Some(contents.borrow().to_string()),
        _ => None,
    }
}

fn u16_slice(s: &str, start: u32, len: u32) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    String::from_utf16(&units[start as usize..(start + len) as usize]).unwrap()
}

/// Every text-node run must resolve (XML and HTML DOM) to text equal to the raw
/// range it claims; uncovered raw characters may only be synthetic separators;
/// the cfiRange endpoints must equal the first/last run.
fn verify_cfis(xhtml: &str, blocks: &[crate::domain::Block], base: &str) {
    let xml_root = xml_tree(xhtml);
    let rc = html5ever::parse_document(RcDom::default(), Default::default()).one(xhtml);
    for b in blocks {
        let mut covered = vec![false; b.raw_text.encode_utf16().count()];
        for r in &b.mapping.text_nodes {
            let node_text = xml_resolve(&xml_root, &r.path).unwrap_or_else(|| panic!("unresolved {} in {:?}", r.path, b.raw_text));
            let rc_text = rc_resolve(&rc.document, &r.path).expect("rcdom path");
            assert_eq!(node_text, rc_text, "XML and HTML DOM disagree at {}", r.path);
            let from_node = u16_slice(&node_text, r.node_offset_u16, r.raw_len_u16);
            let from_raw = u16_slice(&b.raw_text, r.raw_start_u16, r.raw_len_u16);
            assert_eq!(from_node, from_raw, "run mismatch at {}", r.path);
            for c in covered.iter_mut().skip(r.raw_start_u16 as usize).take(r.raw_len_u16 as usize) {
                *c = true;
            }
        }
        let units: Vec<u16> = b.raw_text.encode_utf16().collect();
        for (i, c) in covered.iter().enumerate() {
            if !c {
                assert!(matches!(units[i], 0x0A | 0x09 | 0x20), "uncovered non-separator in {:?}", b.raw_text);
            }
        }
        let SourceLocation::Epub { cfi_range, text_path, start_u16, end_u16, quote, .. } = &b.location else { panic!() };
        assert_eq!(*start_u16, 0);
        assert_eq!(*end_u16, units.len() as u32);
        assert!(cfi_range.starts_with(&format!("epubcfi({base}!")), "{cfi_range}");
        assert!(text_path.iter().all(|n| n % 2 == 0));
        assert!(quote.exact.encode_utf16().count() <= 256);
        assert!(quote.prefix.encode_utf16().count() <= 32 && quote.suffix.encode_utf16().count() <= 32);
        let inner = &cfi_range[8..cfi_range.len() - 1];
        let (_, path) = inner.split_once('!').unwrap();
        let parts: Vec<&str> = path.split(',').collect();
        if let (Some(first), Some(last)) = (b.mapping.text_nodes.first(), b.mapping.text_nodes.last()) {
            assert_eq!(parts.len(), 3, "{cfi_range}");
            let (sp, so) = parts[1].rsplit_once(':').unwrap();
            let (ep, eo) = parts[2].rsplit_once(':').unwrap();
            let common = if parts[0] == "/" { "" } else { parts[0] };
            assert_eq!(format!("{common}{sp}"), first.path);
            assert_eq!(so.parse::<u32>().unwrap(), first.node_offset_u16);
            assert_eq!(format!("{common}{ep}"), last.path);
            assert_eq!(eo.parse::<u32>().unwrap(), last.node_offset_u16 + last.raw_len_u16);
        } else {
            assert_eq!(parts.len(), 1);
        }
    }
}

// ---------------------------------------------------------------------------
// Good fixtures
// ---------------------------------------------------------------------------

#[test]
fn normal_epub3_builds_safe_package() {
    let b = build_bytes(&normal_epub3(), &EpubLimits::default());
    let pkg = b.result.as_ref().unwrap();
    assert_eq!(pkg.title, "A Normal Book");
    assert_eq!(pkg.language.as_deref(), Some("en"));
    assert_eq!(pkg.canonical_revision, canonical_revision());
    assert_eq!(pkg.spine.len(), 3);
    assert_eq!(pkg.spine[0].original_href, "OEBPS/text/ch1.xhtml");
    assert_eq!(pkg.spine[2].safe_href, "x/2.xhtml");
    assert_eq!(pkg.spine[1].idref, "ch2");
    assert!(pkg.restrictions.is_empty(), "{:?}", pkg.restrictions);
    // TOC from nav
    assert_eq!(pkg.toc.len(), 3);
    assert_eq!(pkg.toc[0].label, "Chapter One");
    assert_eq!(pkg.toc[0].children[0].label, "Section & Two");
    assert_eq!(pkg.toc[0].children[0].fragment.as_deref(), Some("s2"));
    assert_eq!((pkg.toc[2].spine_index, pkg.toc[2].fragment.as_deref()), (2, Some("end")));
    // resources: png, woff, style.css, inline <style> css
    let mimes: Vec<&str> = pkg.resources.iter().map(|r| r.mime.as_str()).collect();
    assert_eq!(mimes.iter().filter(|m| **m == "text/css").count(), 2, "{mimes:?}");
    assert!(mimes.contains(&"image/png") && mimes.contains(&"font/woff"));
    let pic = resource_id("OEBPS/img/pic.png", &pkg.canonical_revision);
    assert!(pkg.resources.iter().any(|r| r.safe_href == format!("r/{pic}.png")));
    for r in &pkg.resources {
        assert!(b.out.join(&r.safe_href).exists());
        assert_eq!(r.resource_id.len(), 24);
    }
    let ch1 = read_doc(&b.out, &pkg.spine[0]);
    assert!(ch1.contains(&format!("<img src=\"../r/{pic}.png\" alt=\"A picture\"/>")), "{ch1}");
    assert!(ch1.contains("href=\"1.xhtml#target\""));
    assert!(ch1.contains("href=\"0.xhtml#fn1\""));
    assert!(ch1.contains("<a href=\"https://example.com/page?q=1\" data-thoth-external=\"1\">example</a>"));
    assert!(ch1.contains("data-epub-type=\"footnote\""));
    assert!(ch1.contains("<math xmlns=\"http://www.w3.org/1998/Math/MathML\""));
    let css = fs::read_to_string(b.out.join(format!("r/{}.css", resource_id("OEBPS/css/style.css", &pkg.canonical_revision)))).unwrap();
    assert!(css.contains(&format!("url(\"{pic}.png\")")), "{css}");
    assert!(css.contains(".woff\") format(\"woff\")"), "{css}");
    // package.opf: metadata, manifest, spine are element children 1..3 → spine step /6
    let opf = fs::read_to_string(b.out.join("package.opf")).unwrap();
    let TNode::Elem { children, .. } = xml_tree(&opf) else { panic!() };
    assert_eq!(children.iter().filter(|c| matches!(c, TNode::Elem { .. })).count(), 3);
    assert!(opf.contains("<itemref idref=\"item-ch1\" id=\"ch1\"/>"));
    assert!(fs::read_to_string(b.out.join("META-INF/container.xml")).unwrap().contains("full-path=\"package.opf\""));
    assert_output_safe(&b.out);
}

#[test]
fn blocks_kinds_policies_and_structure() {
    let b = build_bytes(&normal_epub3(), &EpubLimits::default());
    let pkg = b.result.as_ref().unwrap();
    let blocks = blocks_of(&b.out, pkg, &pkg.spine[0]);
    let kinds: HashSet<BlockKind> = blocks.iter().map(|b| b.kind).collect();
    for k in [
        BlockKind::Heading, BlockKind::Paragraph, BlockKind::ListItem, BlockKind::Quote, BlockKind::Code,
        BlockKind::Table, BlockKind::Figure, BlockKind::Caption, BlockKind::Formula,
    ] {
        assert!(kinds.contains(&k), "missing {k:?}");
    }
    for (i, bl) in blocks.iter().enumerate() {
        assert_eq!(bl.ordinal, i as u32);
        assert_eq!(bl.source_hash, crate::domain::sha256_hex_str(&bl.source_text));
        assert_eq!(bl.translation_policy, bl.kind.default_policy());
        assert!(crate::domain::is_valid_id(&bl.id));
    }
    let by_text = |t: &str| blocks.iter().find(|b| b.source_text == t).unwrap_or_else(|| panic!("no block {t:?}"));
    assert_eq!(by_text("Chapter One").kind, BlockKind::Heading);
    assert_eq!(by_text("Nested item").kind, BlockKind::ListItem);
    assert_eq!(by_text("Item two").kind, BlockKind::ListItem);
    assert_eq!(by_text("Quoted paragraph one.").kind, BlockKind::Quote);
    let br = blocks.iter().find(|b| b.raw_text.starts_with("Line one")).unwrap();
    assert_eq!(br.raw_text, "Line one\nline two with emoji 😀 and e\u{301} combining.");
    assert_eq!(br.source_text, "Line one line two with emoji 😀 and \u{e9} combining.");
    assert!(br.content.contains(&SafeNode::Break));
    let code = blocks.iter().find(|b| b.kind == BlockKind::Code).unwrap();
    assert_eq!(code.translation_policy, TranslationPolicy::Preserve);
    assert!(code.raw_text.contains("println!(\"hi <3\");"));
    let table = blocks.iter().find(|b| b.kind == BlockKind::Table).unwrap();
    assert_eq!(table.raw_text, "H1\tH2\na\tb c");
    assert!(matches!(table.content[0], SafeNode::TableCell { header: true, .. }));
    let tcap = by_text("Table caption");
    assert_eq!(tcap.kind, BlockKind::Caption);
    assert_eq!(tcap.parent_id.as_deref(), Some(table.id.as_str()));
    assert_eq!(tcap.translation_policy, TranslationPolicy::Auto);
    let fig = blocks.iter().find(|b| b.kind == BlockKind::Figure).unwrap();
    assert!(matches!(&fig.content[0], SafeNode::ImageRef { alt, .. } if alt == "A picture"));
    assert_eq!(by_text("Figure 1. A caption.").parent_id.as_deref(), Some(fig.id.as_str()));
    let formula = blocks.iter().find(|b| b.kind == BlockKind::Formula).unwrap();
    assert_eq!(formula.translation_policy, TranslationPolicy::Preserve);
    let p1 = blocks.iter().find(|b| b.raw_text.starts_with("First paragraph")).unwrap();
    assert!(p1.content.iter().any(|n| matches!(n, SafeNode::Link { href, .. } if href == "1.xhtml#target")));
    assert!(p1.content.iter().any(|n| matches!(n, SafeNode::InlineCode { v } if v == "code()")));
    let ext = by_text("Visit example.");
    assert!(ext.content.iter().any(|n| matches!(n, SafeNode::Link { href, .. } if href == "https://example.com/page?q=1")));
    assert_eq!(by_text("The footnote text.").roles, vec!["footnote".to_string()]);
    // Mixed-content div: loose text groups and the inner paragraph are separate leaves.
    let ch2 = blocks_of(&b.out, pkg, &pkg.spine[1]);
    let texts: Vec<&str> = ch2.iter().map(|b| b.source_text.as_str()).collect();
    assert!(texts.contains(&"Loose text in a div with span."), "{texts:?}");
    assert!(texts.contains(&"Then a paragraph."));
    assert!(texts.contains(&"Trailing loose text."));
    // word-level offset mapping covers every source word
    for bl in blocks.iter().chain(ch2.iter()) {
        if bl.source_text.is_empty() {
            continue;
        }
        let last = bl.mapping.offsets.last().unwrap();
        assert_eq!(last.source_end, bl.source_text.encode_utf16().count() as u32);
    }
}

#[test]
fn cfi_paths_resolve_in_xml_and_html_dom() {
    for bytes in [normal_epub3(), epub2_ncx(), rtl_vertical_fixed()] {
        let b = build_bytes(&bytes, &EpubLimits::default());
        let pkg = b.result.as_ref().unwrap();
        for item in &pkg.spine {
            let x = read_doc(&b.out, item);
            let blocks = blocks_of(&b.out, pkg, item);
            assert!(!blocks.is_empty());
            let base = format!("/6/{}[{}]", (item.index + 1) * 2, item.idref);
            verify_cfis(&x, &blocks, &base);
        }
    }
}

#[test]
fn cfi_matches_epubjs_expected_strings() {
    let b = build_bytes(&normal_epub3(), &EpubLimits::default());
    let pkg = b.result.as_ref().unwrap();
    let blocks = blocks_of(&b.out, pkg, &pkg.spine[0]);
    let cfi = |t: &str| match &blocks.iter().find(|b| b.source_text == t).unwrap().location {
        SourceLocation::Epub { cfi_range, text_path, .. } => (cfi_range.clone(), text_path.clone()),
        _ => unreachable!(),
    };
    // body=/4, section#ch1 = 1st element child (/2[ch1]), h1#h1 = /2[h1].
    // h1 children: "Chapter " (text 0 → /1) and <em> (/2) containing "One" (/1).
    assert_eq!(cfi("Chapter One"), ("epubcfi(/6/2[ch1]!/4/2[ch1]/2[h1],/1:0,/2/1:3)".to_string(), vec![4, 2, 2]));
    // The 3rd <p> in the section (4th element child): a single text node.
    let dup = cfi(DUP_SENTENCE);
    assert_eq!(dup.0, format!("epubcfi(/6/2[ch1]!/4/2[ch1]/8,/1:0,/1:{})", DUP_SENTENCE.len()));
}

#[test]
fn deterministic_output_and_ids() {
    let bytes = normal_epub3();
    let a = build_bytes(&bytes, &EpubLimits::default());
    let b = build_bytes(&bytes, &EpubLimits::default());
    let (pa, pb) = (a.result.as_ref().unwrap(), b.result.as_ref().unwrap());
    assert_eq!(pa, pb);
    let mut fa = Vec::new();
    all_files(&a.out, &mut fa);
    fa.sort();
    for f in &fa {
        let rel = f.strip_prefix(&a.out).unwrap();
        assert_eq!(fs::read(f).unwrap(), fs::read(b.out.join(rel)).unwrap(), "{}", rel.display());
    }
    for item in &pa.spine {
        assert_eq!(blocks_of(&a.out, pa, item), blocks_of(&b.out, pb, item));
    }
}

#[test]
fn duplicate_sentences_get_distinct_block_ids() {
    let b = build_bytes(&normal_epub3(), &EpubLimits::default());
    let pkg = b.result.as_ref().unwrap();
    let mut dups = Vec::new();
    let mut all_ids = HashSet::new();
    for item in &pkg.spine {
        for bl in blocks_of(&b.out, pkg, item) {
            assert!(all_ids.insert(bl.id.clone()), "duplicate block id");
            if bl.source_text == DUP_SENTENCE {
                dups.push(bl);
            }
        }
    }
    assert_eq!(dups.len(), 3);
    assert_eq!(dups.iter().map(|b| b.source_hash.clone()).collect::<HashSet<_>>().len(), 1);
    assert_eq!(dups.iter().map(|b| b.id.clone()).collect::<HashSet<_>>().len(), 3);
    let cfis: HashSet<String> = dups
        .iter()
        .map(|b| match &b.location {
            SourceLocation::Epub { cfi_range, .. } => cfi_range.clone(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(cfis.len(), 3);
    // quote context disambiguates within a chapter
    let q: Vec<_> = dups
        .iter()
        .map(|b| match &b.location {
            SourceLocation::Epub { quote, .. } => (quote.prefix.clone(), quote.suffix.clone()),
            _ => unreachable!(),
        })
        .collect();
    assert_ne!(q[1], q[2]);
}

#[test]
fn epub2_with_ncx_and_tag_soup() {
    let b = build_bytes(&epub2_ncx(), &EpubLimits::default());
    let pkg = b.result.as_ref().unwrap();
    assert_eq!(pkg.title, "Old Book");
    assert_eq!(pkg.language.as_deref(), Some("fr"));
    assert_eq!(pkg.spine.len(), 2);
    assert_eq!(pkg.spine[0].original_href, "OEBPS/a b.html");
    assert!(!pkg.spine[1].linear);
    assert_eq!(pkg.toc.len(), 2);
    assert_eq!(pkg.toc[0].label, "Début");
    assert_eq!(pkg.toc[0].children[0].fragment.as_deref(), Some("s"));
    assert_eq!(pkg.toc[1].spine_index, 1);
    assert!(pkg.restrictions.contains(&"xhtml_not_wellformed:OEBPS/b.html".to_string()), "{:?}", pkg.restrictions);
    let blocks = blocks_of(&b.out, pkg, &pkg.spine[0]);
    let texts: Vec<&str> = blocks.iter().map(|b| b.raw_text.as_str()).collect();
    assert_eq!(texts, vec!["Titre\u{a0}un", "Un\u{a0}paragraphe «cité».", "Après l'ancre."]);
    // self-closing <a id="anchor"/> stays empty (XML semantics preserved)
    assert!(read_doc(&b.out, &pkg.spine[0]).contains("<a id=\"anchor\"></a>"));
    let soup = blocks_of(&b.out, pkg, &pkg.spine[1]);
    assert_eq!(soup.iter().map(|b| b.raw_text.as_str()).collect::<Vec<_>>(), vec!["Annexe", "Soup"]);
    assert_output_safe(&b.out);
}

#[test]
fn rtl_vertical_fixed_layout_are_restricted() {
    let b = build_bytes(&rtl_vertical_fixed(), &EpubLimits::default());
    let pkg = b.result.as_ref().unwrap();
    for r in ["fixed_layout", "rtl_progression", "vertical_writing", "rtl_text"] {
        assert!(pkg.restrictions.contains(&r.to_string()), "{r} missing: {:?}", pkg.restrictions);
    }
    let opf = fs::read_to_string(b.out.join("package.opf")).unwrap();
    assert!(opf.contains("page-progression-direction=\"rtl\"") && opf.contains("pre-paginated"));
    for bl in blocks_of(&b.out, pkg, &pkg.spine[0]) {
        assert_eq!(bl.translation_policy, TranslationPolicy::ManualOnly);
        assert_eq!(bl.quality.level, crate::domain::QualityLevel::Medium);
        assert!(bl.quality.reasons.contains(&"vertical_writing".to_string()));
    }
}

#[test]
fn extract_blocks_rejects_non_canonical_input() {
    assert!(extract_blocks("<html><p>unclosed", 0, "a", "x/0.xhtml", "r", EXTRACTION, "s").is_err());
    assert!(extract_blocks("<!DOCTYPE x [<!ENTITY a 'b'>]><html/>", 0, "a", "x/0.xhtml", "r", EXTRACTION, "s").is_err());
}

// ---------------------------------------------------------------------------
// Malicious corpus
// ---------------------------------------------------------------------------

fn check_case(name: &str, bytes: &[u8], limits: &EpubLimits, expect: &Expect) {
    let b = build_bytes(bytes, limits);
    match expect {
        Expect::Reject(code, reason) => {
            let err = b.result.as_ref().err().unwrap_or_else(|| panic!("{name}: expected rejection"));
            assert_eq!(err.code, *code, "{name}: {err:?}");
            if let Some(r) = reason {
                assert_eq!(err.details.get("reason").and_then(|v| v.as_str()), Some(*r), "{name}: {err:?}");
            }
            // Nothing escapes the output directory: only our own layout may exist.
            let mut files = Vec::new();
            all_files(&b.out, &mut files);
            for f in files {
                assert!(f.starts_with(&b.out));
            }
        }
        Expect::RejectAny(codes) => {
            let err = b.result.as_ref().err().unwrap_or_else(|| panic!("{name}: expected rejection"));
            assert!(codes.contains(&err.code), "{name}: {err:?}");
        }
        Expect::Sanitized(restrictions) => {
            let pkg = b.result.as_ref().unwrap_or_else(|e| panic!("{name}: {e:?}"));
            for r in *restrictions {
                assert!(pkg.restrictions.iter().any(|x| x.starts_with(r)), "{name}: {r} not in {:?}", pkg.restrictions);
            }
            assert!(!pkg.restrictions.iter().any(|x| x.starts_with("markup_unstable")), "{name}: {:?}", pkg.restrictions);
            assert_output_safe(&b.out);
            for item in &pkg.spine {
                let x = read_doc(&b.out, item);
                let blocks = blocks_of(&b.out, pkg, item);
                verify_cfis(&x, &blocks, &format!("/6/{}[{}]", (item.index + 1) * 2, item.idref));
            }
        }
    }
}

#[test]
fn malicious_corpus_is_blocked_or_neutralized() {
    let cases = malicious_cases();
    assert!(cases.len() >= 20, "corpus has {} cases", cases.len());
    let mut names = HashSet::new();
    for c in &cases {
        assert!(names.insert(c.name), "duplicate case name");
        check_case(c.name, &c.bytes, &c.limits, &c.expect);
    }
}

#[test]
fn sanitized_details() {
    let cases: BTreeMap<&str, Case> = malicious_cases().into_iter().map(|c| (c.name, c)).collect();
    let run = |name: &str| {
        let c = &cases[name];
        let b = build_bytes(&c.bytes, &c.limits);
        let pkg = b.result.as_ref().unwrap().clone();
        let x = read_doc(&b.out, &pkg.spine[0]);
        (b, pkg, x)
    };
    let (_b, _, x) = run("script_and_handlers");
    assert!(x.contains("<p>Click <a>js</a>"), "{x}");
    assert!(x.contains("<div>after</div>"));
    let (_b, _, x) = run("svg_script_foreign_object");
    assert!(x.contains("<use href=\"#ok\"/>"), "{x}");
    assert!(x.contains("<rect fill=\"url(#grad)\"/>"), "{x}");
    assert!(x.contains("<g><rect id=\"ok\" width=\"5\" height=\"5\"/></g>"), "{x}");
    let (b, pkg, x) = run("css_attacks");
    assert!(x.contains("<p style=\"color:red\">Styled</p>"), "{x}");
    assert!(x.contains("<div>E</div>"));
    let css_texts: Vec<String> = pkg
        .resources
        .iter()
        .filter(|r| r.mime == "text/css")
        .map(|r| fs::read_to_string(b.out.join(&r.safe_href)).unwrap())
        .collect();
    assert!(css_texts.iter().any(|c| c.contains("a{color:green}")), "{css_texts:?}");
    assert!(css_texts.iter().any(|c| c.contains("body{top:0}")), "{css_texts:?}");
    assert!(css_texts.iter().any(|c| c.contains("p{color:blue}")), "{css_texts:?}");
    let (_b, _, x) = run("iframe_object_form");
    assert!(x.contains("<p>Inside form</p>"), "{x}");
    let (_b, pkg, _) = run("obfuscated_font");
    assert!(!pkg.resources.iter().any(|r| r.mime.starts_with("font/")));
    let (_b, _, x) = run("deep_nesting");
    assert!(x.contains("deep text"));
}

#[test]
fn committed_fixture_files_behave() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/epub");
    if !root.exists() {
        return;
    }
    for name in ["normal_epub3", "epub2_ncx", "rtl_vertical_fixed"] {
        let bytes = fs::read(root.join(format!("good/{name}.epub"))).unwrap();
        let b = build_bytes(&bytes, &EpubLimits::default());
        assert!(b.result.is_ok(), "{name}: {:?}", b.result.err());
    }
    let cases = malicious_cases();
    let mut seen = 0;
    for c in &cases {
        let p = root.join(format!("malicious/{}.epub", c.name));
        let Ok(bytes) = fs::read(&p) else { continue };
        seen += 1;
        check_case(c.name, &bytes, &c.limits, &c.expect);
    }
    assert_eq!(seen, cases.len(), "regenerate fixtures: cargo test --lib generate_epub_fixtures -- --ignored");
}

#[test]
fn error_codes_are_stable() {
    let b = build_bytes(b"not a zip at all", &EpubLimits::default());
    let e = b.result.unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFormat);
    assert_eq!(e.details.get("reason").and_then(|v| v.as_str()), Some("not_a_zip"));
}

/// Real-world broken container.xml: rootfile path points to a missing OPF, but the archive has
/// exactly one package document. It opens with a `rootfile_mismatch` restriction.
#[test]
fn rootfile_mismatch_recovers_single_opf() {
    use super::fixtures::*;
    let bytes = EpubBuilder::empty()
        .stored("mimetype", b"application/epub+zip")
        .file("META-INF/container.xml", container_xml("OEBPS/wrong.opf").as_bytes())
        .file("OEBPS/content.opf", opf3("T", &[("c1", "text/c1.xhtml", "application/xhtml+xml", "")], &["c1"], "", "").as_bytes())
        .file("OEBPS/text/c1.xhtml", xhtml("C", "", "<p>Hello</p>").as_bytes())
        .build();
    let b = build_bytes(&bytes, &EpubLimits::default());
    let pkg = b.result.expect("recovered");
    assert!(pkg.restrictions.iter().any(|r| r == "rootfile_mismatch"));
    // Two OPFs => ambiguous => still rejected.
    let bytes2 = EpubBuilder::empty()
        .stored("mimetype", b"application/epub+zip")
        .file("META-INF/container.xml", container_xml("OEBPS/wrong.opf").as_bytes())
        .file("OEBPS/a.opf", opf3("T", &[("c1", "text/c1.xhtml", "application/xhtml+xml", "")], &["c1"], "", "").as_bytes())
        .file("OEBPS/b.opf", opf3("T", &[("c1", "text/c1.xhtml", "application/xhtml+xml", "")], &["c1"], "", "").as_bytes())
        .file("OEBPS/text/c1.xhtml", xhtml("C", "", "<p>Hello</p>").as_bytes())
        .build();
    assert!(build_bytes(&bytes2, &EpubLimits::default()).result.is_err());
}

// ---------------------------------------------------------------------------
// Security-review regressions (resource exhaustion, CSS allow-list bypass)
// ---------------------------------------------------------------------------

fn reason_of(e: &AppError) -> Option<&str> {
    e.details.get("reason").and_then(|v| v.as_str())
}

/// One-chapter book whose chapter file is given verbatim (may be tag soup).
fn raw_chapter(doc: &str) -> Vec<u8> {
    EpubBuilder::new()
        .file("OEBPS/content.opf", opf3("T", &[("c1", "text/c1.xhtml", "application/xhtml+xml", "")], &["c1"], "", "").as_bytes())
        .file("OEBPS/text/c1.xhtml", doc.as_bytes())
        .build()
}

/// Finding 1: html5ever tree building is bounded (formatting-element re-creation,
/// unclosed nesting, canonical re-parse growth).
#[test]
fn html_parser_blowups_are_bounded() {
    use super::dom::{max_attrs_per_tag, parse_html, parse_html_bounded};
    let started = std::time::Instant::now();

    // 2000 distinct unclosed formatting elements, re-created for each of 50,000
    // paragraphs: 10^8 nodes when unbounded.
    let mut soup = String::from("<p>");
    for i in 0..2000 {
        soup.push_str(&format!("<b class={i}>"));
    }
    soup.push_str(&"<p>x".repeat(50_000));
    assert!(parse_html(&soup).is_err());
    let b = build_bytes(&raw_chapter(&soup), &EpubLimits::default());
    let err = b.result.as_ref().err().expect("rejected");
    assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
    assert_eq!(reason_of(err), Some("markup_too_complex"));

    // Fewer formatting elements stay under the stack limit; the node budget stops
    // it instead (2,000,000 elements when unbounded; small budget keeps this cheap).
    let mut soup = String::from("<p>");
    for i in 0..100 {
        soup.push_str(&format!("<b class=\"c{i}\">"));
    }
    soup.push_str(&"<p>x</p>".repeat(20_000));
    assert!(parse_html_bounded(&soup, 50_000).is_err());
    // ... and the same budget does not reject an ordinary document of that size.
    let plain = format!("<html><body>{}</body></html>", "<p>x <b>y</b></p>".repeat(20_000));
    let root = parse_html_bounded(&plain, 50_000).expect("plain document parses");
    assert_eq!(root.find(&|e| e.name == "body").map(|b| b.children.len()), Some(20_000));

    // Hundreds of thousands of unclosed nested elements: quadratic when unbounded.
    assert!(parse_html(&"<div>".repeat(200_000)).is_err());

    // Well-formed XHTML that only blows up when the canonical output is re-parsed
    // as HTML (formatting elements wrapped around many paragraphs).
    let mut body = String::from("<p>");
    for i in 0..100 {
        body.push_str(&format!("<b class=\"c{i}\">"));
    }
    body.push_str(&"<p>x</p>".repeat(1000));
    body.push_str(&"</b>".repeat(100));
    body.push_str("</p>");
    let b = build_bytes(&one_chapter("", &body, &[]).build(), &EpubLimits::default());
    let err = b.result.as_ref().err().expect("rejected");
    assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
    assert_eq!(reason_of(err), Some("markup_too_complex"));

    // One tag with a huge number of attributes (quadratic duplicate check in both
    // tokenizers), as tag soup and as well-formed XHTML.
    assert_eq!(max_attrs_per_tag("<p a b='x y' c=d e = \"f\"/><q x>"), 4);
    assert_eq!(max_attrs_per_tag("</p a b c>"), 3);
    assert_eq!(max_attrs_per_tag("<p a=\"<q b c d e\">"), 4);
    let attrs: String = (0..100_000).map(|i| format!(" a{i}=\">\"")).collect();
    assert_eq!(max_attrs_per_tag(&format!("<br><p{attrs}>x")), 100_000);
    for doc in [format!("<p{attrs}>x<br>"), xhtml("C", "", &format!("<p{attrs}>x</p>"))] {
        let b = build_bytes(&raw_chapter(&doc), &EpubLimits::default());
        let err = b.result.as_ref().err().expect("rejected");
        assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
        assert_eq!(reason_of(err), Some("markup_too_complex"));
    }

    // Tag soup beyond the size accepted by the error-recovering parser.
    let big = format!("<p>unclosed{}", "<p>paragraph text".repeat(300_000));
    let b = build_bytes(&raw_chapter(&big), &EpubLimits::default());
    assert_eq!(b.result.as_ref().err().and_then(reason_of), Some("html_document_too_large"));

    assert!(started.elapsed() < std::time::Duration::from_secs(20), "took {:?}", started.elapsed());
}

/// Finding 2: a bad token nested inside an attribute selector must not vanish and
/// forge a comment opener that swallows the rest of the stylesheet.
#[test]
fn css_bad_token_cannot_forge_comment() {
    use super::css::{sanitize_style_attr, sanitize_stylesheet};
    use super::refs::CanonicalPolicy;
    let pol = CanonicalPolicy { from_spine: None };
    let src = concat!(
        "a[b(/url(x y)*)]{color:red}",
        r#"p{content:"*/)]{} *{position:fixed;z-index:99999;background:url(http://evil/x)} x{color:"}"#
    );
    let (out, _) = sanitize_stylesheet(src, &pol);
    let out = out.unwrap_or_default();
    assert!(!out.contains("/*"), "{out}");
    assert!(!out.contains("position:fixed"), "{out}");
    assert!(!out.contains("evil"), "{out}");
    assert!(!out.contains("a["), "{out}");
    // Nested blocks anywhere inside [...] are refused, plain attribute selectors stay.
    let (out, _) = sanitize_stylesheet(r#"a[x=(y[url(a b)])]{color:red} a[href$=".png"]{color:blue}"#, &pol);
    assert_eq!(out.as_deref(), Some("a[href$=\".png\"]{color:blue}\n"));
    // Strings may not carry comment delimiters either.
    let (out, _) = sanitize_stylesheet(r#"p::before{content:"/*"} q::after{content:"*/";color:red} a[title="/*"]{color:red}"#, &pol);
    assert_eq!(out.as_deref(), Some("q::after{color:red}\n"));
    let (attr, _) = sanitize_style_attr("color: red; margin: 1px/url(x y)*2", &pol);
    assert_eq!(attr.as_deref(), Some("color:red"));

    // End to end through an inline <style>.
    let head = format!("<style>{}</style>", src.replace('<', ""));
    let b = build_bytes(&one_chapter(&head, "<p>x</p>", &[]).build(), &EpubLimits::default());
    b.result.as_ref().expect("builds");
    assert_output_safe(&b.out);
    let mut files = Vec::new();
    all_files(&b.out, &mut files);
    for f in files {
        let text = String::from_utf8_lossy(&fs::read(&f).unwrap()).to_string();
        assert!(!text.contains("/*") && !text.contains("evil"), "{}: {text}", f.display());
    }
}

/// Finding 3: missing references are de-duplicated without a quadratic scan and
/// only a bounded number is reported.
#[test]
fn missing_references_are_capped() {
    use super::css::MAX_MISSING_RECORDED;
    let mut body = String::new();
    for i in 0..30_000 {
        body.push_str(&format!("<img src=\"m{i}.png\" alt=\"\"/>"));
    }
    body.push_str("<img src=\"m0.png\" alt=\"\"/><p style=\"background:url(bg.png)\">x</p>");
    let css = (0..5000).map(|i| format!("p.c{i}{{background:url(n{i}.png)}}")).collect::<String>();
    let head = format!("<style>{css}</style>");
    let started = std::time::Instant::now();
    let b = build_bytes(&one_chapter(&head, &body, &[]).build(), &EpubLimits::default());
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "took {:?}", started.elapsed());
    let pkg = b.result.as_ref().expect("builds");
    let missing: Vec<&String> = pkg.restrictions.iter().filter(|r| r.starts_with("missing_resource:")).collect();
    assert_eq!(missing.len(), MAX_MISSING_RECORDED);
    assert!(pkg.restrictions.iter().any(|r| r == "missing_resources_truncated"), "{:?}", &pkg.restrictions[..5]);
    let mut sorted = pkg.restrictions.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, pkg.restrictions);

    // Below the cap nothing changes: every distinct path once, no marker.
    let b = build_bytes(
        &one_chapter("", "<img src=\"a.png\" alt=\"\"/><img src=\"a.png\" alt=\"\"/><img src=\"b.png\" alt=\"\"/>", &[]).build(),
        &EpubLimits::default(),
    );
    let pkg = b.result.as_ref().expect("builds");
    let missing: Vec<&str> =
        pkg.restrictions.iter().filter(|r| r.starts_with("missing_resource")).map(|s| s.as_str()).collect();
    assert_eq!(missing, ["missing_resource:OEBPS/text/a.png", "missing_resource:OEBPS/text/b.png"]);
}

/// Finding 4: all inline `<style>` elements of a document become one bounded resource.
#[test]
fn inline_styles_share_one_bounded_resource() {
    use super::css::MAX_STYLESHEET_BYTES;
    let css_files = |b: &Built| -> Vec<String> {
        let pkg = b.result.as_ref().expect("builds");
        pkg.resources.iter().filter(|r| r.mime == "text/css").map(|r| fs::read_to_string(b.out.join(&r.safe_href)).unwrap()).collect()
    };
    let body = format!("<p>x</p>{}", (0..500).map(|i| format!("<style>.b{i}{{color:blue}}</style>")).collect::<String>());
    let b = build_bytes(
        &one_chapter("<style>.h1{color:red}</style><style>.h2{color:green}</style>", &body, &[]).build(),
        &EpubLimits::default(),
    );
    let files = css_files(&b);
    assert_eq!(files.len(), 1, "one resource for 502 <style> elements");
    assert!(files[0].starts_with(".h1{color:red}\n.h2{color:green}\n.b0{color:blue}\n.b1{color:blue}\n"), "{}", &files[0][..80]);
    assert!(files[0].ends_with(".b499{color:blue}\n"));
    let mut r = Vec::new();
    all_files(&b.out.join("r"), &mut r);
    assert_eq!(r.len(), 1);
    let x = read_doc(&b.out, &b.result.as_ref().unwrap().spine[0]);
    assert_eq!(x.matches("<link ").count(), 1, "{x}");
    assert!(!b.result.as_ref().unwrap().restrictions.iter().any(|r| r == "inline_style_truncated"));

    // Total size is capped at MAX_STYLESHEET_BYTES; the rest is dropped and reported.
    let chunk = "p{color:red}".repeat(20_000); // ~260 KiB once sanitized
    let head = (0..6).map(|i| format!("<style>.k{i}{{top:0}}{chunk}</style>")).collect::<String>();
    let b = build_bytes(&one_chapter(&head, "<p>x</p>", &[]).build(), &EpubLimits::default());
    let files = css_files(&b);
    assert_eq!(files.len(), 1);
    assert!(files[0].len() <= MAX_STYLESHEET_BYTES, "{}", files[0].len());
    assert!(files[0].contains(".k0{top:0}") && files[0].contains(".k3{top:0}") && !files[0].contains(".k4{top:0}"));
    assert!(b.result.as_ref().unwrap().restrictions.iter().any(|r| r == "inline_style_truncated"));
}

/// Finding 5: a zip64 record is honoured whenever the zip crate would use it (also
/// when only the directory size/offset is saturated), before anything is allocated.
#[test]
fn zip64_declared_entry_count_is_checked() {
    // ~300 KiB so that the declared count is plausible for the file size.
    let mut bytes = one_chapter("", "<p>hello</p>", &[]).stored("OEBPS/pad.bin", &vec![0x55u8; 300 * 1024]).build();
    let eocd = bytes.len() - 22;
    assert_eq!(&bytes[eocd..eocd + 4], b"PK\x05\x06");
    let mut tail = bytes.split_off(eocd);
    let zip64_at = bytes.len() as u64;
    let declared: u64 = 5000;
    // zip64 end of central directory record
    bytes.extend_from_slice(b"PK\x06\x06");
    bytes.extend_from_slice(&44u64.to_le_bytes());
    bytes.extend_from_slice(&45u16.to_le_bytes());
    bytes.extend_from_slice(&45u16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&declared.to_le_bytes());
    bytes.extend_from_slice(&declared.to_le_bytes());
    bytes.extend_from_slice(&(declared * 46).to_le_bytes());
    bytes.extend_from_slice(&6000u64.to_le_bytes());
    // zip64 locator
    bytes.extend_from_slice(b"PK\x06\x07");
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&zip64_at.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    // The 16-bit entry count stays small and honest; only the directory size is saturated.
    tail[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(&tail);

    let limits = EpubLimits { max_entries: 100, ..EpubLimits::default() };
    let b = build_bytes(&bytes, &limits);
    let err = b.result.as_ref().err().expect("rejected");
    assert_eq!(err.code, ErrorCode::LimitExceeded, "{err:?}");
    assert_eq!(reason_of(err), Some("too_many_entries"));

    // The untouched archive is fine under the same limits.
    let ok = one_chapter("", "<p>hello</p>", &[]).stored("OEBPS/pad.bin", &vec![0x55u8; 300 * 1024]).build();
    assert!(build_bytes(&ok, &limits).result.is_ok());
}

/// Finding 6: SVG `<use>` chains and floods are cut down.
#[test]
fn svg_use_chains_and_floods_are_limited() {
    let svg = |inner: &str| format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 9 9\">{inner}</svg>");
    // Chain: l1 = 3 x l0, l2 = 3 x l1, ... each level multiplies the shapes.
    let mut defs = String::from("<defs><rect id=\"l0\" width=\"1\" height=\"1\"/>");
    for i in 1..=10 {
        defs.push_str(&format!("<g id=\"l{i}\">{}</g>", format!("<use href=\"#l{}\"/>", i - 1).repeat(3)));
    }
    defs.push_str("</defs><use href=\"#l10\"/><use xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:href=\"#l1\"/><use href=\"#l0\"/>");
    let b = build_bytes(&one_chapter("", &svg(&defs), &[]).build(), &EpubLimits::default());
    let pkg = b.result.as_ref().expect("builds");
    assert!(pkg.restrictions.iter().any(|r| r == "svg_use_removed"), "{:?}", pkg.restrictions);
    assert!(!pkg.restrictions.iter().any(|r| r.starts_with("markup_unstable")));
    let x = read_doc(&b.out, &pkg.spine[0]);
    // Only references to the plain shape survive: 3 inside l1 and the direct one.
    assert_eq!(x.matches("<use ").count(), 4, "{x}");
    assert_eq!(x.matches("<use href=\"#l0\"/>").count(), 4, "{x}");

    // Flood of direct references: capped per document.
    let flood = format!("<defs><rect id=\"s\" width=\"1\" height=\"1\"/></defs>{}", "<use href=\"#s\"/>".repeat(1500));
    let b = build_bytes(&one_chapter("", &svg(&flood), &[]).build(), &EpubLimits::default());
    let pkg = b.result.as_ref().expect("builds");
    assert!(pkg.restrictions.iter().any(|r| r == "svg_use_removed"));
    assert_eq!(read_doc(&b.out, &pkg.spine[0]).matches("<use ").count(), 1000);

    // A few references to a large group: capped by total instantiated elements.
    let big = format!(
        "<defs><g id=\"big\">{}</g></defs>{}",
        "<rect width=\"1\" height=\"1\"/>".repeat(30_000),
        "<use href=\"#big\"/>".repeat(10)
    );
    let b = build_bytes(&one_chapter("", &svg(&big), &[]).build(), &EpubLimits::default());
    let pkg = b.result.as_ref().expect("builds");
    assert_eq!(read_doc(&b.out, &pkg.spine[0]).matches("<use ").count(), 3);

    // Standalone SVG resources get the same treatment; a benign one is untouched.
    let res = svg("<g id=\"a\"><use href=\"#b\"/></g><g id=\"b\"><use href=\"#a\"/></g><rect id=\"ok\" width=\"1\" height=\"1\"/><use href=\"#ok\"/>");
    let b = build_bytes(
        &one_chapter("", "<p><img src=\"../i.svg\" alt=\"\"/></p>", &[("i.svg", res.as_bytes(), "image/svg+xml")]).build(),
        &EpubLimits::default(),
    );
    let pkg = b.result.as_ref().expect("builds");
    let r = pkg.resources.iter().find(|r| r.mime == "image/svg+xml").expect("svg resource");
    let text = fs::read_to_string(b.out.join(&r.safe_href)).unwrap();
    assert_eq!(text.matches("<use ").count(), 1, "{text}");
    assert!(text.contains("<use href=\"#ok\"/>"), "{text}");
}

/// Finding 7: CSS-escaped `url(` in SVG presentation attributes is refused.
#[test]
fn svg_paint_rejects_css_escapes() {
    let body = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 9 9">
<linearGradient id="grad"><stop offset="0" stop-color="red"/></linearGradient>
<rect id="a" fill="\75rl(http://evil.example/f)" stroke="\55 RL(//evil.example/s)" width="1" height="1"/>
<rect id="b" mask="u\72l(http://evil.example/m)" clip-path="\000075rl(http://evil.example/c)" marker-end="ur\6c (http://evil.example/e)"/>
<rect id="c" fill="url(#grad)" stroke="#fff" width="2" height="2"/></svg>"##;
    let b = build_bytes(&one_chapter("", body, &[]).build(), &EpubLimits::default());
    let pkg = b.result.as_ref().expect("builds");
    assert_output_safe(&b.out);
    let x = read_doc(&b.out, &pkg.spine[0]);
    assert!(!x.contains('\\') && !x.contains("evil"), "{x}");
    assert!(x.contains("<rect id=\"a\" width=\"1\" height=\"1\"/>"), "{x}");
    assert!(x.contains("<rect id=\"b\"/>"), "{x}");
    assert!(x.contains("<rect id=\"c\" fill=\"url(#grad)\" stroke=\"#fff\" width=\"2\" height=\"2\"/>"), "{x}");
}
