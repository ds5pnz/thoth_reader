//! Minimal owned DOM shared by the parsers, the sanitizer, the canonical serializer
//! and the block extractor. Comments, processing instructions and doctypes are never
//! represented, adjacent text is always merged.

use std::borrow::Cow;
use std::cell::Cell;
use std::time::{Duration, Instant};

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, Tracer, TreeBuilderOpts, TreeSink};
use html5ever::{parse_document, Attribute, ExpandedName, ParseOpts, QualName};
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

/// Upper bound on elements the HTML parser may create for one document. Slightly
/// above the XML parser's node cap so that re-parsing a canonical document that
/// came from a maximal XML tree (plus the generated head) is still accepted.
pub(crate) const MAX_HTML_NODES: usize = 2_100_000;
/// Upper bound on (open elements + active formatting elements) in the tree builder,
/// checked between input chunks. Scope checks walk the open-element stack and every
/// paragraph re-creates all active formatting elements, so this bounds the work
/// per token. The DOM is flattened below [`MAX_DEPTH`] anyway.
const MAX_HTML_OPEN: usize = 192;
/// Input is fed in chunks of this many bytes; limits are checked in between, so
/// the overshoot past any limit is bounded by what one chunk can cause.
const HTML_CHUNK_BYTES: usize = 512;
/// Budget for the tree operations that are linear in the number of siblings or
/// attributes (foster parenting, adoption agency, repeated `<html>`/`<body>`
/// attribute merging), which would otherwise be quadratic in the input size.
const MAX_HTML_SINK_WORK: usize = 50_000_000;
/// Wall-clock backstop for a single parse (tokenizer-internal worst cases that
/// the structural limits above cannot see). Benign documents take well under 1 s.
const MAX_HTML_PARSE_TIME: Duration = Duration::from_secs(30);

/// Attributes per tag accepted by either parser. Both tokenizers check every new
/// attribute name against all previous ones of the same tag (quadratic).
pub(crate) const MAX_TAG_ATTRS: usize = 512;

/// Upper bound on the number of attributes of any single tag the HTML tokenizer
/// can see in `text`, without tokenizing: the tag sub-automaton of the HTML
/// tokenizer (tag name / attribute name / value states) is evaluated for every
/// possible tag start at once with a backwards dynamic program, so the result
/// is exact for real tags and merely pessimistic for `<x ...` text that the real
/// tokenizer would treat as script, comment or attribute content. O(len).
pub(crate) fn max_attrs_per_tag(text: &str) -> usize {
    const TAG_NAME: usize = 0;
    const BEFORE_NAME: usize = 1;
    const NAME: usize = 2;
    const AFTER_NAME: usize = 3;
    const BEFORE_VALUE: usize = 4;
    const VALUE_DQ: usize = 5;
    const VALUE_SQ: usize = 6;
    const VALUE_UNQUOTED: usize = 7;
    const AFTER_VALUE: usize = 8;
    const SELF_CLOSING: usize = 9;
    const END: usize = 10;
    /// (next state, started a new attribute)
    fn step(state: usize, c: u8) -> (usize, bool) {
        let ws = matches!(c, b'\t' | b'\n' | 0x0c | b'\r' | b' ');
        match state {
            TAG_NAME => match c {
                _ if ws => (BEFORE_NAME, false),
                b'/' => (SELF_CLOSING, false),
                b'>' => (END, false),
                _ => (TAG_NAME, false),
            },
            BEFORE_NAME | AFTER_NAME => match c {
                _ if ws => (state, false),
                b'/' => (SELF_CLOSING, false),
                b'>' => (END, false),
                b'=' if state == AFTER_NAME => (BEFORE_VALUE, false),
                _ => (NAME, true),
            },
            NAME => match c {
                _ if ws => (AFTER_NAME, false),
                b'/' => (SELF_CLOSING, false),
                b'>' => (END, false),
                b'=' => (BEFORE_VALUE, false),
                _ => (NAME, false),
            },
            BEFORE_VALUE => match c {
                _ if ws => (BEFORE_VALUE, false),
                b'"' => (VALUE_DQ, false),
                b'\'' => (VALUE_SQ, false),
                b'>' => (END, false),
                _ => (VALUE_UNQUOTED, false),
            },
            VALUE_DQ => (if c == b'"' { AFTER_VALUE } else { VALUE_DQ }, false),
            VALUE_SQ => (if c == b'\'' { AFTER_VALUE } else { VALUE_SQ }, false),
            VALUE_UNQUOTED => match c {
                _ if ws => (BEFORE_NAME, false),
                b'>' => (END, false),
                _ => (VALUE_UNQUOTED, false),
            },
            // AFTER_VALUE and SELF_CLOSING reconsume anything else as BEFORE_NAME.
            _ => match c {
                _ if ws => (BEFORE_NAME, false),
                b'/' => (SELF_CLOSING, false),
                b'>' => (END, false),
                _ => (NAME, true),
            },
        }
    }
    let bytes = text.as_bytes();
    // next[q] / next2[q]: attributes counted when the automaton is in state `q`
    // at byte i+1 / i+2 and runs to the end of that tag (or of the input).
    let mut next = [0usize; 10];
    let mut next2 = [0usize; 10];
    let mut max = 0usize;
    for (i, &c) in bytes.iter().enumerate().rev() {
        let mut cur = [0usize; 10];
        for (q, slot) in cur.iter_mut().enumerate() {
            let (to, started) = step(q, c);
            let rest = next.get(to).copied().unwrap_or(0); // END -> 0
            *slot = rest.saturating_add(started as usize);
        }
        if c == b'<' {
            let b1 = bytes.get(i + 1).copied().unwrap_or(0);
            let b2 = bytes.get(i + 2).copied().unwrap_or(0);
            if b1.is_ascii_alphabetic() {
                max = max.max(next[TAG_NAME]);
            } else if b1 == b'/' && b2.is_ascii_alphabetic() {
                max = max.max(next2[TAG_NAME]);
            }
        }
        next2 = next;
        next = cur;
    }
    max
}

/// The HTML parser hit one of its resource limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HtmlLimit;

/// `RcDom` wrapper that counts created elements and sibling-linear work. Once a
/// budget is exhausted the sink goes inert (no more tree mutations); the driver
/// notices after the current chunk and aborts.
struct BoundedDom {
    inner: RcDom,
    max_nodes: usize,
    created: Cell<usize>,
    work: Cell<usize>,
    exceeded: Cell<bool>,
}

impl BoundedDom {
    fn charge(&self, units: usize) -> bool {
        if self.exceeded.get() {
            return false;
        }
        let w = self.work.get().saturating_add(units);
        self.work.set(w);
        if w > MAX_HTML_SINK_WORK {
            self.exceeded.set(true);
            return false;
        }
        true
    }

    fn child_count(node: &Handle) -> usize {
        node.children.borrow().len()
    }

    /// Number of children of `node`'s parent (0 when detached).
    fn sibling_count(node: &Handle) -> usize {
        let parent = node.parent.take();
        let n = parent.as_ref().and_then(|w| w.upgrade()).map(|p| p.children.borrow().len()).unwrap_or(0);
        node.parent.set(parent);
        n
    }
}

impl TreeSink for BoundedDom {
    type Handle = Handle;
    type Output = RcDom;
    type ElemName<'a>
        = ExpandedName<'a>
    where
        Self: 'a;

    fn finish(self) -> RcDom {
        self.inner
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> Handle {
        self.inner.get_document()
    }

    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        self.inner.elem_name(target)
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Handle {
        let n = self.created.get().saturating_add(1);
        self.created.set(n);
        if n > self.max_nodes {
            self.exceeded.set(true);
        }
        // The tree builder identifies elements by name, so a real (detached,
        // attribute-less) element is still handed out once the sink is inert.
        let attrs = if self.exceeded.get() { Vec::new() } else { attrs };
        self.inner.create_element(name, attrs, flags)
    }

    fn create_comment(&self, text: StrTendril) -> Handle {
        // Comments are never represented in the converted tree.
        self.inner.create_comment(if self.exceeded.get() { StrTendril::new() } else { text })
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Handle {
        self.inner.create_pi(target, data)
    }

    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        if !self.exceeded.get() {
            self.inner.append(parent, child);
        }
    }

    fn append_based_on_parent_node(&self, element: &Handle, prev_element: &Handle, child: NodeOrText<Handle>) {
        if self.charge(Self::sibling_count(element)) {
            self.inner.append_based_on_parent_node(element, prev_element, child);
        }
    }

    fn append_doctype_to_document(&self, _name: StrTendril, _public_id: StrTendril, _system_id: StrTendril) {}

    fn get_template_contents(&self, target: &Handle) -> Handle {
        self.inner.get_template_contents(target)
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        self.inner.same_node(x, y)
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.inner.set_quirks_mode(mode);
    }

    fn append_before_sibling(&self, sibling: &Handle, new_node: NodeOrText<Handle>) {
        if self.charge(Self::sibling_count(sibling)) {
            self.inner.append_before_sibling(sibling, new_node);
        }
    }

    fn add_attrs_if_missing(&self, target: &Handle, attrs: Vec<Attribute>) {
        if attrs.is_empty() {
            return;
        }
        let existing = match &target.data {
            NodeData::Element { attrs, .. } => attrs.borrow().len(),
            _ => 0,
        };
        if self.charge(existing.saturating_add(attrs.len())) {
            self.inner.add_attrs_if_missing(target, attrs);
        }
    }

    fn remove_from_parent(&self, target: &Handle) {
        if self.charge(Self::sibling_count(target)) {
            self.inner.remove_from_parent(target);
        }
    }

    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        if self.charge(Self::child_count(node)) {
            self.inner.reparent_children(node, new_parent);
        }
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Handle) -> bool {
        self.inner.is_mathml_annotation_xml_integration_point(handle)
    }

    // `maybe_clone_an_option_into_selectedcontent` keeps its default no-op: it
    // deep-clones subtrees outside the node budget and `<select>` content is
    // dropped by the sanitizer anyway.
}

struct HandleCounter(Cell<usize>);

impl Tracer for HandleCounter {
    type Handle = Handle;
    fn trace_handle(&self, _node: &Handle) {
        self.0.set(self.0.get().saturating_add(1));
    }
}

/// Parse with the WHATWG HTML parser (scripting disabled so `<noscript>` content is
/// parsed as markup). Returns the `<html>` element, or [`HtmlLimit`] when the
/// document would need more than [`MAX_HTML_NODES`] elements, nests / stacks
/// formatting elements absurdly deep, or otherwise exhausts the parse budgets.
pub(crate) fn parse_html(text: &str) -> Result<Element, HtmlLimit> {
    parse_html_bounded(text, MAX_HTML_NODES)
}

pub(crate) fn parse_html_bounded(text: &str, max_nodes: usize) -> Result<Element, HtmlLimit> {
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts { scripting_enabled: false, ..Default::default() },
        ..Default::default()
    };
    let sink = BoundedDom {
        inner: RcDom::default(),
        max_nodes,
        created: Cell::new(0),
        work: Cell::new(0),
        exceeded: Cell::new(false),
    };
    let mut parser = parse_document(sink, opts);
    let started = Instant::now();
    let mut rest = text;
    while !rest.is_empty() {
        let mut n = rest.len().min(HTML_CHUNK_BYTES);
        while !rest.is_char_boundary(n) {
            n += 1;
        }
        let (head, tail) = rest.split_at(n);
        rest = tail;
        parser.process(StrTendril::from_slice(head));
        let builder = &parser.tokenizer.sink;
        if builder.sink.exceeded.get() {
            return Err(HtmlLimit);
        }
        let open = HandleCounter(Cell::new(0));
        builder.trace_handles(&open);
        if open.0.get() > MAX_HTML_OPEN || started.elapsed() > MAX_HTML_PARSE_TIME {
            return Err(HtmlLimit);
        }
    }
    let exceeded = parser.tokenizer.sink.sink.exceeded.get();
    if exceeded {
        return Err(HtmlLimit);
    }
    let dom = parser.finish();
    let doc = dom.document.clone();
    for child in doc.children.borrow().iter() {
        if let NodeData::Element { .. } = child.data {
            return Ok(convert_rc(child, 0).unwrap_or_else(|| Element::new(Ns::Html, "html")));
        }
    }
    Ok(Element::new(Ns::Html, "html"))
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
