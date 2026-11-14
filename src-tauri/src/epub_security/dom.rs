//! Minimal owned DOM shared by the parsers, the sanitizer, the canonical serializer
//! and the block extractor. Comments, processing instructions and doctypes are never
//! represented, adjacent text is always merged.

use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{parse_document, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

pub(crate) const NS_HTML: &str = "http://www.w3.org/1999/xhtml";
pub(crate) const NS_SVG: &str = "http://www.w3.org/2000/svg";
pub(crate) const NS_MATHML: &str = "http://www.w3.org/1998/Math/MathML";
pub(crate) const NS_XLINK: &str = "http://www.w3.org/1999/xlink";
pub(crate) const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";
pub(crate) const NS_OPS: &str = "http://www.idpf.org/2007/ops";

/// Element nesting deeper than this is flattened to text (bounds every recursion).
pub(crate) const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ns {
    Html,
    Svg,
    MathMl,
    /// EPUB OPS namespace (epub:switch / epub:case / epub:default)
    Ops,
    Other,
}

impl Ns {
    pub(crate) fn from_uri(uri: &str) -> Ns {
        match uri {
            NS_HTML => Ns::Html,
            NS_SVG => Ns::Svg,
            NS_MATHML => Ns::MathMl,
            NS_OPS => Ns::Ops,
            _ => Ns::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Element {
    pub ns: Ns,
    /// Local name (case preserved; HTML names are lowercase).
    pub name: String,
    /// Attribute names: plain local names, or `xlink:href`, `xml:lang`, `epub:type`
    /// for the well-known namespaces; anything else keeps its raw qualified name.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
}

impl Element {
    pub(crate) fn new(ns: Ns, name: &str) -> Self {
        Element { ns, name: name.to_string(), attrs: Vec::new(), children: Vec::new() }
    }
    pub(crate) fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    /// Attribute lookup ignoring any namespace prefix (`opf:role` matches `role`).
    pub(crate) fn attr_local(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == local || k.rsplit(':').next() == Some(local))
            .map(|(_, v)| v.as_str())
    }
    pub(crate) fn is(&self, ns: Ns, name: &str) -> bool {
        self.ns == ns && self.name == name
    }
    pub(crate) fn is_html(&self, name: &str) -> bool {
        self.is(Ns::Html, name)
    }
    pub(crate) fn push_text(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        if let Some(Node::Text(prev)) = self.children.last_mut() {
            prev.push_str(t);
        } else {
            self.children.push(Node::Text(t.to_string()));
        }
    }
    pub(crate) fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }
    /// Concatenated descendant text.
    pub(crate) fn text_content(&self) -> String {
        let mut s = String::new();
        collect_text(self, &mut s);
        s
    }
    /// Depth-first search for the first element matching `pred`.
    pub(crate) fn find<'a>(&'a self, pred: &dyn Fn(&Element) -> bool) -> Option<&'a Element> {
        for c in self.child_elements() {
            if pred(c) {
                return Some(c);
            }
            if let Some(f) = c.find(pred) {
                return Some(f);
            }
        }
        None
    }
}

fn collect_text(e: &Element, out: &mut String) {
    for c in &e.children {
        match c {
            Node::Text(t) => out.push_str(t),
            Node::Element(ch) => collect_text(ch, out),
        }
    }
}

/// Characters that are not allowed in XML 1.0 documents or that the HTML parser
/// would rewrite (NUL, CR, other C0/C1 controls, noncharacters).
pub(crate) fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 => {}
            '\u{7f}'..='\u{9f}' => {}
            '\u{fdd0}'..='\u{fdef}' | '\u{fffe}' | '\u{ffff}' => {}
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// html5ever (lenient HTML parser) -> Element
// ---------------------------------------------------------------------------

/// Parse with the WHATWG HTML parser (scripting disabled so `<noscript>` content is
/// parsed as markup). Returns the `<html>` element.
pub(crate) fn parse_html(text: &str) -> Element {
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts { scripting_enabled: false, ..Default::default() },
        ..Default::default()
    };
    let dom = parse_document(RcDom::default(), opts).one(text);
    let doc = dom.document.clone();
    for child in doc.children.borrow().iter() {
        if let NodeData::Element { .. } = child.data {
            return convert_rc(child, 0).unwrap_or_else(|| Element::new(Ns::Html, "html"));
        }
    }
    Element::new(Ns::Html, "html")
}

fn convert_rc(h: &Handle, depth: usize) -> Option<Element> {
    let NodeData::Element { name, attrs, .. } = &h.data else { return None };
    let ns = Ns::from_uri(&name.ns);
    let mut el = Element::new(ns, &name.local);
    for a in attrs.borrow().iter() {
        let local: &str = &a.name.local;
        let key = match &*a.name.ns {
            "" => local.to_string(),
            NS_XLINK => format!("xlink:{local}"),
            NS_XML => format!("xml:{local}"),
            "http://www.w3.org/2000/xmlns/" => continue,
            other => format!("{{{other}}}{local}"),
        };
        el.attrs.push((key, a.value.to_string()));
    }
    for c in h.children.borrow().iter() {
        match &c.data {
            NodeData::Text { contents } => el.push_text(&contents.borrow()),
            NodeData::Element { .. } => {
                if depth + 1 >= MAX_DEPTH {
                    let mut s = String::new();
                    rc_text(c, &mut s);
                    el.push_text(&s);
                } else if let Some(ch) = convert_rc(c, depth + 1) {
                    el.children.push(Node::Element(ch));
                }
            }
            _ => {}
        }
    }
    Some(el)
}

/// Iterative text collection (used past the depth limit).
fn rc_text(h: &Handle, out: &mut String) {
    let mut stack: Vec<Handle> = vec![h.clone()];
    while let Some(n) = stack.pop() {
        match &n.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { .. } => {
                for c in n.children.borrow().iter().rev() {
                    stack.push(c.clone());
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical (polyglot) XHTML serializer
// ---------------------------------------------------------------------------

const VOID_HTML: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr",
];

pub(crate) fn is_void_html(name: &str) -> bool {
    VOID_HTML.contains(&name)
}

pub(crate) fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

pub(crate) fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            c => out.push(c),
        }
    }
}

/// Serialize a sanitized `<html>` tree as polyglot XHTML. The sanitizer guarantees
/// that only HTML/SVG/MathML elements with safe names remain.
pub(crate) fn serialize_document(html: &Element) -> String {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str("<!DOCTYPE html>");
    serialize_element(html, None, &mut out);
    out
}

/// Serialize a standalone element (e.g. an SVG resource root) with its xmlns.
pub(crate) fn serialize_fragment(e: &Element) -> String {
    let mut out = String::new();
    serialize_element(e, None, &mut out);
    out
}

fn serialize_element(e: &Element, parent_ns: Option<Ns>, out: &mut String) {
    out.push('<');
    out.push_str(&e.name);
    let needs_xmlns = match e.ns {
        Ns::Html => parent_ns.is_none(),
        Ns::Svg | Ns::MathMl => parent_ns != Some(e.ns),
        _ => false,
    };
    if needs_xmlns {
        let uri = match e.ns {
            Ns::Html => NS_HTML,
            Ns::Svg => NS_SVG,
            _ => NS_MATHML,
        };
        out.push_str(" xmlns=\"");
        out.push_str(uri);
        out.push('"');
    }
    for (k, v) in &e.attrs {
        out.push(' ');
        out.push_str(k);
        out.push_str("=\"");
        escape_attr(v, out);
        out.push('"');
    }
    if e.children.is_empty() {
        if e.ns != Ns::Html || is_void_html(&e.name) {
            out.push_str("/>");
        } else {
            out.push_str("></");
            out.push_str(&e.name);
            out.push('>');
        }
        return;
    }
    out.push('>');
    for c in &e.children {
        match c {
            Node::Text(t) => escape_text(t, out),
            Node::Element(ch) => serialize_element(ch, Some(e.ns), out),
        }
    }
    out.push_str("</");
    out.push_str(&e.name);
    out.push('>');
}
