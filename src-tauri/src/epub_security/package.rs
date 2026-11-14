//! Package validation and safe package generation (spec §5.1, §5.3).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use super::css::{sanitize_stylesheet, CssReport};
use super::dom::{clean_text, parse_html, Element, Ns};
use super::html::{canonicalize, sanitize_svg_resource, DocReport, Head, Sanitizer, StyleRef};
use super::paths::{dir_of, is_safe_id, resolve_decoded, scheme_of, split_href};
use super::refs::{LinkMap, PackagePolicy, ResKind, Target};
use super::xml::{decode_text, parse_xml, XmlError};
use super::zip_guard::{drm, unsupported, SafeZip};
use super::{canonical_revision, resource_id, EpubLimits, SafePackage, SafeResource, SafeSpineItem, TocEntry};
use crate::error::AppError;

const FONT_OBFUSCATION: &[&str] = &["http://www.idpf.org/2008/embedding", "http://ns.adobe.com/pdf/enc#RC"];
const MAX_TOC_ENTRIES: usize = 5000;
const MAX_TOC_DEPTH: usize = 32;

struct Item {
    path: String,
    media_type: String,
    properties: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Raster,
    Svg,
    Css,
    Font,
    Xhtml,
    Other,
}

fn kind_of(media_type: &str, path: &str) -> Kind {
    let mt = media_type.trim().to_ascii_lowercase();
    let mt = if mt.is_empty() || mt == "application/octet-stream" {
        mime_guess::from_path(path).first_raw().unwrap_or("").to_string()
    } else {
        mt
    };
    match mt.as_str() {
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" => Kind::Raster,
        "image/svg+xml" => Kind::Svg,
        "text/css" => Kind::Css,
        "application/xhtml+xml" | "text/html" => Kind::Xhtml,
        m if m.starts_with("font/")
            || matches!(
                m,
                "application/vnd.ms-opentype"
                    | "application/font-sfnt"
                    | "application/x-font-ttf"
                    | "application/x-font-truetype"
                    | "application/x-font-opentype"
                    | "application/font-woff"
                    | "application/x-font-woff"
                    | "application/font-woff2"
            ) =>
        {
            Kind::Font
        }
        _ => Kind::Other,
    }
}

fn sniff_image(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("jpg", "image/jpeg"))
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else {
        None
    }
}

fn sniff_font(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.starts_with(&[0x00, 0x01, 0x00, 0x00]) || b.starts_with(b"true") {
        Some(("ttf", "font/ttf"))
    } else if b.starts_with(b"OTTO") {
        Some(("otf", "font/otf"))
    } else if b.starts_with(b"wOFF") {
        Some(("woff", "font/woff"))
    } else if b.starts_with(b"wOF2") {
        Some(("woff2", "font/woff2"))
    } else {
        None
    }
}

/// Parse a package-level XML file (container, OPF, NCX, encryption, nav).
fn parse_xml_doc(bytes: &[u8], html_entities: bool, reason: &str) -> Result<Element, AppError> {
    let (text, _) = decode_text(bytes);
    parse_xml(&text, html_entities, html_entities).map_err(|e| match e {
        XmlError::DtdEntities => unsupported("xml_entity"),
        XmlError::Malformed(_) => unsupported(reason),
    })
}

/// Parse an XHTML content document: strict XML first, HTML parser as fallback.
/// Entity declarations are fatal either way.
fn parse_content_doc(text: &str) -> Result<(Element, bool), AppError> {
    match parse_xml(text, true, true) {
        Ok(root) => Ok((root, true)),
        Err(XmlError::DtdEntities) => Err(unsupported("xml_entity")),
        Err(XmlError::Malformed(_)) => {
            let lower_head: String = text.chars().take(4096).collect::<String>().to_ascii_lowercase();
            if lower_head.contains("<!entity") {
                return Err(unsupported("xml_entity"));
            }
            Ok((parse_html(text), false))
        }
    }
}

fn find_all<'a>(e: &'a Element, local: &str, out: &mut Vec<&'a Element>) {
    for c in e.child_elements() {
        if c.name == local {
            out.push(c);
        }
        find_all(c, local, out);
    }
}

fn first_child<'a>(e: &'a Element, local: &str) -> Option<&'a Element> {
    e.child_elements().find(|c| c.name == local)
}

fn collapse_ws(s: &str, max_chars: usize) -> String {
    clean_text(s).split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max_chars).collect()
}

fn xml_escape(s: &str) -> String {
    let mut out = String::new();
    super::dom::escape_attr(s, &mut out);
    out
}

/// Encrypted resources: font obfuscation → returned set; anything else → DRM error.
fn check_encryption(zip: &mut SafeZip) -> Result<HashSet<String>, AppError> {
    let mut obfuscated = HashSet::new();
    if zip.contains("META-INF/rights.xml") || zip.contains("META-INF/license.lcpl") {
        return Err(drm());
    }
    let Some(bytes) = zip.read("META-INF/encryption.xml")? else { return Ok(obfuscated) };
    let root = parse_xml_doc(&bytes, false, "encryption_invalid")?;
    let mut datas = Vec::new();
    find_all(&root, "EncryptedData", &mut datas);
    for d in datas {
        let algo = d
            .find(&|e| e.name == "EncryptionMethod")
            .and_then(|m| m.attr_local("Algorithm"))
            .unwrap_or("")
            .trim()
            .to_string();
        let uri = d.find(&|e| e.name == "CipherReference").and_then(|c| c.attr_local("URI")).unwrap_or("");
        if !FONT_OBFUSCATION.contains(&algo.as_str()) {
            return Err(drm());
        }
        let (p, _) = split_href(uri);
        if let Some(path) = resolve_decoded("", &p) {
            obfuscated.insert(path);
        }
    }
    Ok(obfuscated)
}

struct Builder<'a> {
    out_dir: &'a Path,
    rev: String,
    restr: BTreeSet<String>,
    resources: Vec<SafeResource>,
    written: HashSet<String>,
}

impl Builder<'_> {
    fn write_resource(&mut self, id: &str, ext: &str, mime: &str, bytes: &[u8]) -> Result<String, AppError> {
        let name = format!("{id}.{ext}");
        if self.written.insert(name.clone()) {
            fs::write(self.out_dir.join("r").join(&name), bytes)?;
            self.resources.push(SafeResource {
                resource_id: id.to_string(),
                safe_href: format!("r/{name}"),
                mime: mime.to_string(),
            });
        }
        Ok(name)
    }

    fn note_css(&mut self, rep: &CssReport, manifest_paths: &HashSet<String>) {
        if rep.remote_removed {
            self.restr.insert("remote_resource_removed".into());
        }
        if rep.rejected {
            self.restr.insert("css_rejected".into());
        }
        for m in &rep.missing {
            if !manifest_paths.contains(m) {
                self.restr.insert(format!("missing_resource:{m}"));
            }
        }
    }

    fn note_doc(&mut self, rep: &DocReport, manifest_paths: &HashSet<String>) {
        if rep.scripts_removed || rep.handlers_removed {
            self.restr.insert("scripted_content_removed".into());
        }
        if rep.media_removed {
            self.restr.insert("media_removed".into());
        }
        if rep.forms_removed {
            self.restr.insert("forms_removed".into());
        }
        if rep.embeds_removed {
            self.restr.insert("embedded_content_removed".into());
        }
        if rep.foreign_object_removed {
            self.restr.insert("svg_foreign_object_removed".into());
        }
        if rep.mathml_fallback {
            self.restr.insert("mathml_fallback".into());
        }
        for m in &rep.missing {
            if !manifest_paths.contains(m) {
                self.restr.insert(format!("missing_resource:{m}"));
            }
        }
        self.note_css(&rep.css, manifest_paths);
    }
}

pub(crate) fn build(epub_path: &Path, out_dir: &Path, limits: &EpubLimits) -> Result<SafePackage, AppError> {
    let mut zip = SafeZip::open(epub_path, limits)?;
    let mut b = Builder {
        out_dir,
        rev: canonical_revision(),
        restr: BTreeSet::new(),
        resources: Vec::new(),
        written: HashSet::new(),
    };

    // --- mimetype -----------------------------------------------------------
    match zip.read("mimetype")? {
        Some(m) => {
            if m.trim_ascii() != b"application/epub+zip" {
                return Err(unsupported("wrong_mimetype"));
            }
        }
        None => {
            b.restr.insert("mimetype_missing".into());
        }
    }

    // --- encryption / DRM ---------------------------------------------------
    let obfuscated = check_encryption(&mut zip)?;

    // --- container ----------------------------------------------------------
    let container = zip.read("META-INF/container.xml")?.ok_or_else(|| unsupported("container_missing"))?;
    let croot = parse_xml_doc(&container, false, "container_invalid")?;
    let mut rootfiles = Vec::new();
    find_all(&croot, "rootfile", &mut rootfiles);
    let rootfile = rootfiles
        .iter()
        .find(|r| r.attr_local("media-type") == Some("application/oebps-package+xml"))
        .or(rootfiles.first())
        .ok_or_else(|| unsupported("container_invalid"))?;
    let full_path = rootfile.attr_local("full-path").unwrap_or("");
    let (fp, _) = split_href(full_path);
    let mut opf_path = resolve_decoded("", &fp).ok_or_else(|| unsupported("opf_path_invalid"))?;
    if !zip.contains(&opf_path) {
        // Recovery for a broken container.xml: exactly one package document in the archive.
        let opfs: Vec<String> = zip.names().into_iter().filter(|n| n.to_ascii_lowercase().ends_with(".opf")).collect();
        if opfs.len() == 1 {
            opf_path = opfs[0].clone();
            b.restr.insert("rootfile_mismatch".into());
        }
    }
    let opf_bytes = zip.read(&opf_path)?.ok_or_else(|| unsupported("opf_missing"))?;
    let opf = parse_xml_doc(&opf_bytes, false, "opf_invalid")?;
    if opf.name != "package" {
        return Err(unsupported("opf_invalid"));
    }
    let opf_dir = dir_of(&opf_path).to_string();

    // --- metadata -----------------------------------------------------------
    let metadata = first_child(&opf, "metadata");
    let mut title = String::new();
    let mut language: Option<String> = None;
    let mut fixed_layout = false;
    if let Some(md) = metadata {
        if let Some(t) = md.find(&|e| e.name == "title") {
            title = collapse_ws(&t.text_content(), 512);
        }
        if let Some(l) = md.find(&|e| e.name == "language") {
            let l = collapse_ws(&l.text_content(), 35);
            if !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                language = Some(l);
            }
        }
        let mut metas = Vec::new();
        find_all(md, "meta", &mut metas);
        for m in metas {
            if m.attr_local("property") == Some("rendition:layout") && m.text_content().trim() == "pre-paginated" {
                fixed_layout = true;
            }
            if m.attr_local("name") == Some("fixed-layout") && m.attr_local("content") == Some("true") {
                fixed_layout = true;
            }
        }
    }
    if title.is_empty() {
        title = "Untitled".into();
    }

    // --- manifest -----------------------------------------------------------
    let mut items: Vec<Item> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    if let Some(manifest) = first_child(&opf, "manifest") {
        for it in manifest.child_elements().filter(|e| e.name == "item") {
            let (Some(id), Some(href)) = (it.attr_local("id"), it.attr_local("href")) else { continue };
            if by_id.contains_key(id) {
                continue;
            }
            if scheme_of(href).is_some() || href.trim_start().starts_with("//") {
                b.restr.insert("remote_resource_removed".into());
                continue;
            }
            let (p, _) = split_href(href);
            let Some(path) = resolve_decoded(&opf_dir, &p) else {
                b.restr.insert("manifest_href_outside_root".into());
                continue;
            };
            let properties: Vec<String> =
                it.attr_local("properties").unwrap_or("").split_ascii_whitespace().map(str::to_string).collect();
            if properties.iter().any(|p| p == "scripted") {
                b.restr.insert("scripted_content_removed".into());
            }
            by_id.insert(id.to_string(), items.len());
            items.push(Item {
                path,
                media_type: it.attr_local("media-type").unwrap_or("").to_string(),
                properties,
            });
        }
    }
    let manifest_paths: HashSet<String> = items.iter().map(|i| i.path.clone()).collect();

    // --- spine --------------------------------------------------------------
    let spine_el = first_child(&opf, "spine").ok_or_else(|| unsupported("spine_missing"))?;
    let rtl_progression = spine_el.attr_local("page-progression-direction") == Some("rtl");
    if rtl_progression {
        b.restr.insert("rtl_progression".into());
    }
    let mut spine: Vec<(usize, String, bool)> = Vec::new(); // (item idx, safe idref, linear)
    let mut used_idrefs: HashSet<String> = HashSet::new();
    let mut used_items: HashSet<usize> = HashSet::new();
    for ir in spine_el.child_elements().filter(|e| e.name == "itemref") {
        let idref = ir.attr_local("idref").unwrap_or("");
        let Some(&idx) = by_id.get(idref) else {
            b.restr.insert(format!("spine_ref_missing:{}", collapse_ws(idref, 64)));
            continue;
        };
        let item = &items[idx];
        if kind_of(&item.media_type, &item.path) != Kind::Xhtml {
            b.restr.insert(format!("spine_item_unsupported:{}", item.path));
            continue;
        }
        if !zip.contains(&item.path) {
            b.restr.insert(format!("missing_resource:{}", item.path));
            continue;
        }
        if !used_items.insert(idx) {
            continue;
        }
        if ir.attr_local("properties").unwrap_or("").contains("rendition:layout-pre-paginated") {
            fixed_layout = true;
        }
        let pos = spine.len();
        let safe = if is_safe_id(idref)
            && !idref.starts_with("item-")
            && !idref.starts_with("res-")
            && !used_idrefs.contains(idref)
        {
            idref.to_string()
        } else {
            format!("s{pos}")
        };
        used_idrefs.insert(safe.clone());
        let linear = ir.attr_local("linear") != Some("no");
        spine.push((idx, safe, linear));
    }
    if spine.is_empty() {
        return Err(unsupported("no_readable_spine"));
    }
    if fixed_layout {
        b.restr.insert("fixed_layout".into());
    }

    fs::create_dir_all(out_dir.join("x"))?;
    fs::create_dir_all(out_dir.join("r"))?;
    fs::create_dir_all(out_dir.join("META-INF"))?;

    let mut map = LinkMap::default();
    for (pos, (idx, _, _)) in spine.iter().enumerate() {
        map.by_path.insert(items[*idx].path.clone(), Target::Spine(pos as u32));
    }

    // --- resources: raster images & fonts, then SVG, then CSS ---------------
    let mut css_flags: HashMap<String, (bool, bool)> = HashMap::new();
    for pass in [0u8, 1, 2] {
        for item in &items {
            if map.by_path.contains_key(&item.path) {
                continue;
            }
            let kind = kind_of(&item.media_type, &item.path);
            let wanted = match pass {
                0 => matches!(kind, Kind::Raster | Kind::Font),
                1 => kind == Kind::Svg,
                _ => kind == Kind::Css,
            };
            if !wanted {
                if pass == 2 {
                    let mt = item.media_type.to_ascii_lowercase();
                    if mt.starts_with("audio/") || mt.starts_with("video/") {
                        b.restr.insert("media_removed".into());
                    } else if mt.contains("javascript") || mt.contains("ecmascript") {
                        b.restr.insert("scripted_content_removed".into());
                    }
                }
                continue;
            }
            if obfuscated.contains(&item.path) {
                b.restr.insert(format!("obfuscated_font_removed:{}", item.path));
                continue;
            }
            let Some(bytes) = zip.read(&item.path)? else {
                b.restr.insert(format!("missing_resource:{}", item.path));
                continue;
            };
            let id = resource_id(&item.path, &b.rev);
            match kind {
                Kind::Raster | Kind::Font => {
                    let sniffed = if kind == Kind::Raster { sniff_image(&bytes) } else { sniff_font(&bytes) };
                    let Some((ext, mime)) = sniffed else {
                        b.restr.insert(format!("invalid_resource:{}", item.path));
                        continue;
                    };
                    b.write_resource(&id, ext, mime, &bytes)?;
                    let rk = if kind == Kind::Raster { ResKind::Image } else { ResKind::Font };
                    map.by_path.insert(item.path.clone(), Target::Resource { id, ext: ext.into(), kind: rk });
                }
                Kind::Svg => {
                    let root = parse_xml_doc(&bytes, false, "svg_invalid");
                    let root = match root {
                        Ok(r) => r,
                        Err(e) if e.details.get("reason").and_then(|v| v.as_str()) == Some("xml_entity") => {
                            return Err(e)
                        }
                        Err(_) => {
                            b.restr.insert(format!("invalid_resource:{}", item.path));
                            continue;
                        }
                    };
                    let pol = PackagePolicy { base_dir: dir_of(&item.path).to_string(), from_spine: None, map: &map };
                    let (out, rep) = sanitize_svg_resource(&root, &pol);
                    b.note_doc(&rep, &manifest_paths);
                    let Some(out) = out else {
                        b.restr.insert(format!("invalid_resource:{}", item.path));
                        continue;
                    };
                    b.write_resource(&id, "svg", "image/svg+xml", out.as_bytes())?;
                    map.by_path.insert(item.path.clone(), Target::Resource { id, ext: "svg".into(), kind: ResKind::Image });
                }
                Kind::Css => {
                    let (text, _) = decode_text(&bytes);
                    let pol = PackagePolicy { base_dir: dir_of(&item.path).to_string(), from_spine: None, map: &map };
                    let (out, rep) = sanitize_stylesheet(&text, &pol);
                    b.note_css(&rep, &manifest_paths);
                    let Some(out) = out else { continue };
                    let name = b.write_resource(&id, "css", "text/css", out.as_bytes())?;
                    css_flags.insert(name, (rep.vertical, rep.rtl));
                    map.by_path.insert(item.path.clone(), Target::Resource { id, ext: "css".into(), kind: ResKind::Css });
                }
                _ => {}
            }
        }
    }

    // --- spine documents ----------------------------------------------------
    let mut safe_spine = Vec::new();
    for (pos, (idx, idref, linear)) in spine.iter().enumerate() {
        let item = &items[*idx];
        let pos = pos as u32;
        let bytes = zip.read(&item.path)?.ok_or_else(|| unsupported("spine_item_missing"))?;
        let (text, lossy) = decode_text(&bytes);
        if lossy {
            b.restr.insert(format!("encoding_fallback:{}", item.path));
        }
        let (root, well_formed) = parse_content_doc(&text)?;
        if !well_formed {
            b.restr.insert(format!("xhtml_not_wellformed:{}", item.path));
        }
        let base_dir = dir_of(&item.path).to_string();
        let pol = PackagePolicy { base_dir: base_dir.clone(), from_spine: Some(pos), map: &map };
        let style_pol = PackagePolicy { base_dir, from_spine: None, map: &map };
        let doc = Sanitizer::new(&pol, Some(&style_pol)).document(&root);
        b.note_doc(&doc.report, &manifest_paths);

        let mut vertical = doc.report.css.vertical;
        let mut rtl = doc.report.rtl || doc.report.css.rtl || rtl_progression;
        let mut links = Vec::new();
        for (k, s) in doc.styles.iter().enumerate() {
            match s {
                StyleRef::Link(h) => {
                    if let Some((v, r)) = h.strip_prefix("../r/").and_then(|n| css_flags.get(n)) {
                        vertical |= *v;
                        rtl |= *r;
                    }
                    links.push(h.clone());
                }
                StyleRef::Inline(css) => {
                    let id = resource_id(&format!("{}#style{k}", item.path), &b.rev);
                    let name = b.write_resource(&id, "css", "text/css", css.as_bytes())?;
                    links.push(format!("../r/{name}"));
                }
            }
        }
        let mut flags = Vec::new();
        if fixed_layout {
            flags.push("fixed_layout".to_string());
        }
        if rtl {
            flags.push("rtl".to_string());
            b.restr.insert("rtl_text".into());
        }
        if vertical {
            flags.push("vertical_writing".to_string());
            b.restr.insert("vertical_writing".into());
        }
        let head = Head {
            title: doc.title.clone().unwrap_or_else(|| title.clone()),
            links,
            flags,
            lang: doc.lang.clone().or_else(|| language.clone()),
            dir: doc.dir.clone(),
        };
        let (xhtml, converged) = canonicalize(&head, doc.body, pos);
        if !converged {
            b.restr.insert(format!("markup_unstable:{pos}"));
        }
        fs::write(out_dir.join("x").join(format!("{pos}.xhtml")), xhtml.as_bytes())?;
        safe_spine.push(SafeSpineItem {
            index: pos,
            idref: idref.clone(),
            original_href: item.path.clone(),
            safe_href: format!("x/{pos}.xhtml"),
            linear: *linear,
        });
    }

    // --- table of contents ----------------------------------------------------
    let spine_paths: HashMap<String, u32> =
        safe_spine.iter().map(|s| (s.original_href.clone(), s.index)).collect();
    let mut toc = Vec::new();
    if let Some(nav_item) = items.iter().find(|i| i.properties.iter().any(|p| p == "nav")) {
        if let Some(bytes) = zip.read(&nav_item.path)? {
            let (text, _) = decode_text(&bytes);
            let (root, _) = parse_content_doc(&text)?;
            toc = toc_from_nav(&root, dir_of(&nav_item.path), &spine_paths);
        }
    }
    if toc.is_empty() {
        let ncx_item = spine_el
            .attr_local("toc")
            .and_then(|id| by_id.get(id).map(|&i| &items[i]))
            .or_else(|| items.iter().find(|i| i.media_type == "application/x-dtbncx+xml"));
        if let Some(ncx) = ncx_item {
            if let Some(bytes) = zip.read(&ncx.path)? {
                let root = parse_xml_doc(&bytes, true, "ncx_invalid")?;
                toc = toc_from_ncx(&root, dir_of(&ncx.path), &spine_paths);
            }
        }
    }
    if toc.is_empty() {
        b.restr.insert("toc_missing".into());
    }

    // --- package.opf + container.xml -----------------------------------------
    write_package(out_dir, &b, &title, language.as_deref(), &safe_spine, fixed_layout, rtl_progression)?;

    let mut resources = std::mem::take(&mut b.resources);
    resources.sort_by(|a, c| a.safe_href.cmp(&c.safe_href));
    Ok(SafePackage {
        title,
        language,
        canonical_revision: b.rev.clone(),
        spine: safe_spine,
        toc,
        resources,
        restrictions: b.restr.into_iter().collect(),
    })
}

fn write_package(
    out_dir: &Path,
    b: &Builder<'_>,
    title: &str,
    language: Option<&str>,
    spine: &[SafeSpineItem],
    fixed_layout: bool,
    rtl: bool,
) -> Result<(), AppError> {
    let mut opf = String::new();
    opf.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    opf.push_str("<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"uid\" prefix=\"rendition: http://www.idpf.org/vocab/rendition/#\">\n");
    // <metadata>, <manifest>, <spine> must stay the 1st/2nd/3rd element children:
    // epub.js derives the CFI spine step `/6` from the spine element position.
    opf.push_str("<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">");
    opf.push_str(&format!("<dc:identifier id=\"uid\">urn:thoth:{}</dc:identifier>", b.rev));
    opf.push_str(&format!("<dc:title>{}</dc:title>", xml_escape(title)));
    opf.push_str(&format!("<dc:language>{}</dc:language>", xml_escape(language.unwrap_or("und"))));
    opf.push_str("<meta property=\"dcterms:modified\">2000-01-01T00:00:00Z</meta>");
    if fixed_layout {
        opf.push_str("<meta property=\"rendition:layout\">pre-paginated</meta>");
    }
    opf.push_str("</metadata>\n<manifest>\n");
    for s in spine {
        opf.push_str(&format!(
            "<item id=\"item-{}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>\n",
            s.idref, s.safe_href
        ));
    }
    let mut res: Vec<&SafeResource> = b.resources.iter().collect();
    res.sort_by(|a, c| a.safe_href.cmp(&c.safe_href));
    for r in res {
        opf.push_str(&format!(
            "<item id=\"res-{}\" href=\"{}\" media-type=\"{}\"/>\n",
            r.resource_id, r.safe_href, r.mime
        ));
    }
    opf.push_str("</manifest>\n");
    if rtl {
        opf.push_str("<spine page-progression-direction=\"rtl\">\n");
    } else {
        opf.push_str("<spine>\n");
    }
    for s in spine {
        let linear = if s.linear { "" } else { " linear=\"no\"" };
        // itemref@id is what epub.js puts in the `[...]` spine assertion.
        opf.push_str(&format!("<itemref idref=\"item-{0}\" id=\"{0}\"{linear}/>\n", s.idref));
    }
    opf.push_str("</spine>\n</package>\n");
    fs::write(out_dir.join("package.opf"), opf.as_bytes())?;
    fs::write(
        out_dir.join("META-INF").join("container.xml"),
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\"><rootfiles><rootfile full-path=\"package.opf\" media-type=\"application/oebps-package+xml\"/></rootfiles></container>\n",
    )?;
    fs::write(out_dir.join("mimetype"), b"application/epub+zip")?;
    Ok(())
}

fn resolve_toc_href(href: &str, base_dir: &str, spine: &HashMap<String, u32>) -> Option<(u32, Option<String>)> {
    if scheme_of(href).is_some() {
        return None;
    }
    let (p, frag) = split_href(href);
    let path = resolve_decoded(base_dir, &p)?;
    let idx = *spine.get(&path)?;
    Some((idx, frag.filter(|f| is_safe_id(f))))
}

fn finalize_entry(label: String, target: Option<(u32, Option<String>)>, children: Vec<TocEntry>) -> Option<TocEntry> {
    let (spine_index, fragment) = match target {
        Some(t) => t,
        None => (children.first()?.spine_index, None),
    };
    Some(TocEntry { label, spine_index, fragment, children })
}

fn toc_from_nav(root: &Element, base_dir: &str, spine: &HashMap<String, u32>) -> Vec<TocEntry> {
    let is_toc_nav = |e: &Element| {
        e.ns == Ns::Html
            && e.name == "nav"
            && e.attr("epub:type").or(e.attr("data-epub-type")).unwrap_or("").split_whitespace().any(|t| t == "toc")
    };
    let nav = root.find(&is_toc_nav).or_else(|| root.find(&|e: &Element| e.ns == Ns::Html && e.name == "nav"));
    let Some(nav) = nav else { return Vec::new() };
    let Some(ol) = nav.find(&|e: &Element| e.name == "ol" || e.name == "ul") else { return Vec::new() };
    let mut count = 0usize;
    nav_list(ol, base_dir, spine, 0, &mut count)
}

fn nav_list(ol: &Element, base_dir: &str, spine: &HashMap<String, u32>, depth: usize, count: &mut usize) -> Vec<TocEntry> {
    let mut out = Vec::new();
    if depth > MAX_TOC_DEPTH {
        return out;
    }
    for li in ol.child_elements().filter(|e| e.name == "li") {
        if *count >= MAX_TOC_ENTRIES {
            break;
        }
        *count += 1;
        let head = li.child_elements().find(|e| e.name == "a" || e.name == "span");
        let label = head.map(|h| collapse_ws(&h.text_content(), 512)).unwrap_or_default();
        let target = head.filter(|h| h.name == "a").and_then(|a| a.attr("href")).and_then(|h| resolve_toc_href(h, base_dir, spine));
        let children = li
            .child_elements()
            .find(|e| e.name == "ol" || e.name == "ul")
            .map(|sub| nav_list(sub, base_dir, spine, depth + 1, count))
            .unwrap_or_default();
        if let Some(e) = finalize_entry(label, target, children) {
            out.push(e);
        }
    }
    out
}

fn toc_from_ncx(root: &Element, base_dir: &str, spine: &HashMap<String, u32>) -> Vec<TocEntry> {
    let Some(nav_map) = root.find(&|e: &Element| e.name == "navMap") else { return Vec::new() };
    let mut count = 0usize;
    ncx_points(nav_map, base_dir, spine, 0, &mut count)
}

fn ncx_points(parent: &Element, base_dir: &str, spine: &HashMap<String, u32>, depth: usize, count: &mut usize) -> Vec<TocEntry> {
    let mut out = Vec::new();
    if depth > MAX_TOC_DEPTH {
        return out;
    }
    for np in parent.child_elements().filter(|e| e.name == "navPoint") {
        if *count >= MAX_TOC_ENTRIES {
            break;
        }
        *count += 1;
        let label = first_child(np, "navLabel")
            .and_then(|l| first_child(l, "text"))
            .map(|t| collapse_ws(&t.text_content(), 512))
            .unwrap_or_default();
        let target = first_child(np, "content").and_then(|c| c.attr_local("src")).and_then(|s| resolve_toc_href(s, base_dir, spine));
        let children = ncx_points(np, base_dir, spine, depth + 1, count);
        if let Some(e) = finalize_entry(label, target, children) {
            out.push(e);
        }
    }
    out
}
