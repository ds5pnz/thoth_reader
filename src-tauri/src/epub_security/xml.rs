//! Strict XML parsing into [`Element`] with DTDs disabled.
//!
//! quick-xml never resolves DTD entities, but we go further and *reject* any
//! document whose DOCTYPE carries an internal subset or ENTITY declaration, so
//! billion-laughs / external-entity inputs fail cleanly instead of being half-parsed.

use std::collections::HashSet;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::dom::{Element, Node, Ns, MAX_DEPTH, MAX_TAG_ATTRS, NS_OPS, NS_XLINK, NS_XML};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum XmlError {
    /// DOCTYPE with internal subset / ENTITY declarations (always fatal).
    DtdEntities,
    /// Not well-formed or uses an unknown entity.
    Malformed(String),
}

const MAX_NODES: usize = 2_000_000;

/// Decode document bytes: UTF-8 (with/without BOM), UTF-16 with BOM, or the
/// ISO-8859-1 family when declared. Returns (text, lossy).
pub(crate) fn decode_text(bytes: &[u8]) -> (String, bool) {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return match std::str::from_utf8(rest) {
            Ok(s) => (s.to_string(), false),
            Err(_) => (String::from_utf8_lossy(rest).into_owned(), true),
        };
    }
    let utf16 = |le: bool, b: &[u8]| -> (String, bool) {
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) })
            .collect();
        let s = String::from_utf16_lossy(&units);
        let lossy = s.contains('\u{fffd}') || !b.len().is_multiple_of(2);
        (s, lossy)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(true, rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(false, rest);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => {
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).to_ascii_lowercase();
            if head.contains("iso-8859-1") || head.contains("latin1") || head.contains("windows-1252") {
                (bytes.iter().map(|&b| b as char).collect(), false)
            } else {
                (String::from_utf8_lossy(bytes).into_owned(), true)
            }
        }
    }
}

fn predefined(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

fn resolve_entity(name: &str, html_entities: bool, out: &mut String) -> bool {
    if let Some(num) = name.strip_prefix('#') {
        let v = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            num.parse::<u32>().ok()
        };
        return match v.and_then(char::from_u32) {
            Some(c) if c != '\0' => {
                out.push(c);
                true
            }
            _ => false,
        };
    }
    if let Some(c) = predefined(name) {
        out.push(c);
        return true;
    }
    if html_entities {
        let key = format!("{name};");
        if let Some(&(a, b)) = html5ever::data::NAMED_ENTITIES.get(key.as_str()) {
            if let Some(c) = char::from_u32(a) {
                out.push(c);
            }
            if b != 0 {
                if let Some(c) = char::from_u32(b) {
                    out.push(c);
                }
            }
            return true;
        }
    }
    false
}

/// Unescape an attribute value (XML attribute-value normalization of literal
/// whitespace, then character/entity references).
fn unescape_attr(raw: &str, html_entities: bool) -> Result<String, XmlError> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        push_normalized(&rest[..i], &mut out);
        let after = &rest[i + 1..];
        let Some(end) = after.find(';') else {
            return Err(XmlError::Malformed("dangling '&' in attribute".into()));
        };
        let name = &after[..end];
        if name.is_empty() || name.len() > 32 || !resolve_entity(name, html_entities, &mut out) {
            return Err(XmlError::Malformed("unknown entity in attribute".into()));
        }
        rest = &after[end + 1..];
    }
    push_normalized(rest, &mut out);
    Ok(out)
}

fn push_normalized(s: &str, out: &mut String) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\t' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
}

struct Scope {
    default_ns: Option<String>,
    prefixes: Vec<(String, String)>,
}

fn lookup_prefix<'a>(scopes: &'a [Scope], prefix: &str) -> Option<&'a str> {
    match prefix {
        "xml" => return Some(NS_XML),
        "xmlns" => return Some("http://www.w3.org/2000/xmlns/"),
        _ => {}
    }
    for s in scopes.iter().rev() {
        if let Some((_, uri)) = s.prefixes.iter().rev().find(|(p, _)| p == prefix) {
            return Some(uri.as_str());
        }
    }
    None
}

fn default_ns(scopes: &[Scope]) -> Option<&str> {
    scopes.iter().rev().find_map(|s| s.default_ns.as_deref())
}

/// Parse a complete XML document and return its root element.
///
/// * `html_entities`: also accept HTML named entities (XHTML content documents
///   commonly rely on the XHTML DTD for `&nbsp;` etc.).
/// * `no_ns_is_html`: elements in no namespace are treated as XHTML.
pub(crate) fn parse_xml(text: &str, html_entities: bool, no_ns_is_html: bool) -> Result<Element, XmlError> {
    let mut reader = Reader::from_str(text);
    {
        let cfg = reader.config_mut();
        cfg.trim_text(false);
        cfg.check_end_names = true;
        cfg.expand_empty_elements = false;
    }
    let mut st = State::default();
    loop {
        let ev = reader.read_event().map_err(|e| XmlError::Malformed(e.to_string()))?;
        match ev {
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => {}
            Event::DocType(t) => {
                let s: &str = &t;
                let lower = s.to_ascii_lowercase();
                if s.contains('[') || lower.contains("entity") {
                    return Err(XmlError::DtdEntities);
                }
                if st.seen_root {
                    return Err(XmlError::Malformed("doctype after root".into()));
                }
            }
            Event::Start(e) => st.start(&e, false, html_entities, no_ns_is_html)?,
            Event::Empty(e) => st.start(&e, true, html_entities, no_ns_is_html)?,
            Event::End(_) => st.end()?,
            Event::Text(t) => {
                let s = t.xml10_content();
                st.text(&s, true)?;
            }
            Event::CData(c) => {
                let s: &str = &c;
                st.text(s, false)?;
            }
            Event::GeneralRef(r) => {
                let name: &str = &r;
                let mut s = String::new();
                if !resolve_entity(name, html_entities, &mut s) {
                    return Err(XmlError::Malformed(format!("unknown entity &{name};")));
                }
                st.text(&s, false)?;
            }
        }
    }
    if !st.stack.is_empty() {
        return Err(XmlError::Malformed("unclosed element".into()));
    }
    st.root.ok_or_else(|| XmlError::Malformed("no root element".into()))
}

#[derive(Default)]
struct State {
    stack: Vec<Element>,
    scopes: Vec<Scope>,
    /// Elements beyond MAX_DEPTH are not materialized; their text is appended to
    /// the deepest materialized element.
    flattened: usize,
    root: Option<Element>,
    nodes: usize,
    seen_root: bool,
}

impl State {
    fn start(
        &mut self,
        e: &quick_xml::events::BytesStart<'_>,
        is_empty: bool,
        html_entities: bool,
        no_ns_is_html: bool,
    ) -> Result<(), XmlError> {
        if self.seen_root && self.stack.is_empty() && self.flattened == 0 {
            return Err(XmlError::Malformed("multiple roots".into()));
        }
        self.seen_root = true;
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(XmlError::Malformed("too many nodes".into()));
        }
        if self.stack.len() >= MAX_DEPTH || self.flattened > 0 {
            if !is_empty {
                self.flattened += 1;
            }
            return Ok(());
        }
        let mut scope = Scope { default_ns: None, prefixes: Vec::new() };
        let mut raw_attrs: Vec<(String, String)> = Vec::new();
        // quick-xml's own duplicate check is quadratic in the attribute count.
        let mut attributes = e.attributes();
        attributes.with_checks(false);
        let mut seen: HashSet<String> = HashSet::new();
        for a in attributes {
            let a = a.map_err(|e| XmlError::Malformed(e.to_string()))?;
            if seen.len() >= MAX_TAG_ATTRS {
                return Err(XmlError::Malformed("too many attributes".into()));
            }
            let key: &str = a.key.as_ref();
            if !seen.insert(key.to_string()) {
                return Err(XmlError::Malformed("duplicate attribute".into()));
            }
            let value = unescape_attr(&a.value, html_entities)?;
            if key == "xmlns" {
                scope.default_ns = Some(value);
            } else if let Some(p) = key.strip_prefix("xmlns:") {
                scope.prefixes.push((p.to_string(), value));
            } else {
                raw_attrs.push((key.to_string(), value));
            }
        }
        self.scopes.push(scope);
        let qname_owned = e.name();
        let qname: &str = qname_owned.as_ref();
        let (prefix, local) = match qname.split_once(':') {
            Some((p, l)) => (Some(p), l),
            None => (None, qname),
        };
        let uri = match prefix {
            Some(p) => lookup_prefix(&self.scopes, p).map(str::to_string),
            None => default_ns(&self.scopes).map(str::to_string),
        };
        let ns = match uri.as_deref() {
            Some("") | None if no_ns_is_html && prefix.is_none() => Ns::Html,
            Some(u) => Ns::from_uri(u),
            None => Ns::Other,
        };
        let mut el = Element::new(ns, local);
        for (k, v) in raw_attrs {
            let key = match k.split_once(':') {
                None => k,
                Some((p, l)) => match lookup_prefix(&self.scopes, p) {
                    Some(NS_XML) => format!("xml:{l}"),
                    Some(NS_XLINK) => format!("xlink:{l}"),
                    Some(NS_OPS) => format!("epub:{l}"),
                    Some(other) => format!("{{{other}}}{l}"),
                    None => k,
                },
            };
            el.attrs.push((key, v));
        }
        if is_empty {
            self.scopes.pop();
            self.attach(el);
        } else {
            self.stack.push(el);
        }
        Ok(())
    }

    fn attach(&mut self, el: Element) {
        match self.stack.last_mut() {
            Some(parent) => parent.children.push(Node::Element(el)),
            None => self.root = Some(el),
        }
    }

    fn end(&mut self) -> Result<(), XmlError> {
        if self.flattened > 0 {
            self.flattened -= 1;
            return Ok(());
        }
        let el = self.stack.pop().ok_or_else(|| XmlError::Malformed("unbalanced end".into()))?;
        self.scopes.pop();
        self.attach(el);
        Ok(())
    }

    fn text(&mut self, s: &str, whitespace_ok_outside: bool) -> Result<(), XmlError> {
        match self.stack.last_mut() {
            Some(p) => {
                p.push_text(s);
                Ok(())
            }
            None if whitespace_ok_outside && s.chars().all(|c| c.is_whitespace()) => Ok(()),
            None => Err(XmlError::Malformed("content outside root".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_entity_declarations() {
        let bomb = r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;&lol;">]><lolz>&lol2;</lolz>"#;
        assert_eq!(parse_xml(bomb, false, false), Err(XmlError::DtdEntities));
        let ext = r#"<!DOCTYPE x [<!ENTITY xxe SYSTEM "file:///etc/passwd">]><x>&xxe;</x>"#;
        assert_eq!(parse_xml(ext, false, false), Err(XmlError::DtdEntities));
    }

    #[test]
    fn accepts_public_doctype_and_html_entities() {
        let doc = r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><p epub:type="x">a&nbsp;b&amp;&#x41;<a/></p></body></html>"#;
        let root = parse_xml(doc, true, true).unwrap();
        assert_eq!(root.ns, Ns::Html);
        let p = root.find(&|e| e.name == "p").unwrap();
        assert_eq!(p.attr("epub:type"), Some("x"));
        assert_eq!(p.children[0], Node::Text("a\u{a0}b&A".into()));
        assert!(parse_xml(doc, false, true).is_err());
    }

    #[test]
    fn deep_nesting_is_flattened() {
        let mut s = String::from("<r>");
        for _ in 0..1000 {
            s.push_str("<d>");
        }
        s.push('x');
        for _ in 0..1000 {
            s.push_str("</d>");
        }
        s.push_str("</r>");
        let root = parse_xml(&s, false, false).unwrap();
        assert_eq!(root.text_content(), "x");
    }
}
