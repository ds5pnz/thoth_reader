//! Allow-list sanitizer for XHTML content documents, inline/standalone SVG and
//! MathML (spec §5.2), plus canonical document assembly.
//!
//! Input is a parsed tree (quick-xml for well-formed XHTML, html5ever otherwise).
//! Output is a new tree containing only allow-listed HTML/SVG/MathML elements and
//! attributes, with every reference rewritten through a [`UrlPolicy`].

use std::collections::{HashMap, HashSet};

use super::css::{sanitize_style_attr, sanitize_stylesheet, CssReport, MissingSet, MAX_STYLESHEET_BYTES};
use super::dom::{
    clean_text, parse_html, serialize_document, serialize_fragment, Element, HtmlLimit, Node, Ns, MAX_DEPTH,
};
use super::paths::is_safe_id;
use super::refs::{CanonicalPolicy, DropReason, RefUse, Resolved, UrlPolicy};
use super::CONTENT_CSP;

/// Everything the sanitizer removed or noticed in one document.
#[derive(Debug, Default, Clone)]
pub(crate) struct DocReport {
    pub scripts_removed: bool,
    pub handlers_removed: bool,
    pub media_removed: bool,
    pub forms_removed: bool,
    pub embeds_removed: bool,
    pub foreign_object_removed: bool,
    pub unsafe_url_removed: bool,
    pub meta_removed: bool,
    pub mathml_fallback: bool,
    pub rtl: bool,
    /// Inline `<style>` content beyond [`MAX_STYLESHEET_BYTES`] per document was dropped.
    pub inline_style_truncated: bool,
    /// SVG `<use>` elements were dropped (chained references or over the caps).
    pub svg_use_removed: bool,
    pub missing: MissingSet,
    pub css: CssReport,
}

impl DocReport {
    fn note_drop(&mut self, r: &DropReason) {
        match r {
            DropReason::Remote => self.css.remote_removed = true,
            DropReason::Scheme | DropReason::Invalid | DropReason::Outside => self.unsafe_url_removed = true,
            DropReason::Missing(p) => self.missing.insert(p),
            DropReason::WrongKind => {}
        }
    }
}

/// A stylesheet reference in document order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StyleRef {
    /// Canonical href of a sanitized stylesheet resource.
    Link(String),
    /// Sanitized CSS of all inline `<style>` elements of the document, concatenated
    /// in document order (urls relative to `r/`). At most one per document, at the
    /// position of the first `<style>`.
    Inline(String),
}

#[derive(Debug, Clone)]
pub(crate) struct SanitizedDoc {
    pub body: Element,
    pub lang: Option<String>,
    pub dir: Option<String>,
    pub title: Option<String>,
    pub styles: Vec<StyleRef>,
    pub report: DocReport,
}

/// Canonical `<head>` inputs.
#[derive(Debug, Clone, Default)]
pub(crate) struct Head {
    pub title: String,
    pub links: Vec<String>,
    pub flags: Vec<String>,
    pub lang: Option<String>,
    pub dir: Option<String>,
}

// ---------------------------------------------------------------------------
// Allow-lists
// ---------------------------------------------------------------------------

const HTML_ALLOWED: &[&str] = &[
    "p", "div", "section", "article", "aside", "nav", "header", "footer", "h1", "h2", "h3", "h4",
    "h5", "h6", "hgroup", "blockquote", "pre", "hr", "ul", "ol", "li", "dl", "dt", "dd", "figure",
    "figcaption", "table", "caption", "thead", "tbody", "tfoot", "tr", "td", "th", "colgroup", "col",
    "address", "a", "abbr", "b", "bdi", "bdo", "cite", "code", "del", "dfn", "em", "i", "ins", "kbd",
    "mark", "q", "rp", "rt", "ruby", "rb", "rtc", "s", "samp", "small", "span", "strong", "sub", "sup",
    "time", "u", "var", "wbr", "br", "img",
];

const HTML_DROP_SCRIPT: &[&str] = &["script", "noframes", "noembed", "template", "xmp", "plaintext", "listing"];
const HTML_DROP_MEDIA: &[&str] = &["audio", "video", "source", "track", "canvas", "picture-source"];
const HTML_DROP_FORM: &[&str] =
    &["input", "button", "select", "option", "optgroup", "textarea", "datalist", "output", "keygen"];
const HTML_DROP_EMBED: &[&str] =
    &["iframe", "frame", "frameset", "object", "embed", "applet", "param", "portal", "map", "area"];
const HTML_DROP_META: &[&str] = &["meta", "base"];

const SVG_ALLOWED: &[&str] = &[
    "svg", "g", "defs", "title", "desc", "symbol", "use", "path", "rect", "circle", "ellipse", "line",
    "polyline", "polygon", "text", "tspan", "textPath", "linearGradient", "radialGradient", "stop",
    "clipPath", "mask", "pattern", "image", "marker",
];

const SVG_ATTRS: &[&str] = &[
    "id", "class", "style", "transform", "d", "x", "y", "x1", "y1", "x2", "y2", "cx", "cy", "r", "rx",
    "ry", "fx", "fy", "width", "height", "viewBox", "preserveAspectRatio", "points", "pathLength",
    "fill", "fill-opacity", "fill-rule", "stroke", "stroke-width", "stroke-linecap",
    "stroke-linejoin", "stroke-dasharray", "stroke-dashoffset", "stroke-opacity",
    "stroke-miterlimit", "opacity", "offset", "stop-color", "stop-opacity", "gradientUnits",
    "gradientTransform", "spreadMethod", "clip-path", "clip-rule", "clipPathUnits", "mask",
    "maskUnits", "maskContentUnits", "font-family", "font-size", "font-weight", "font-style",
    "font-variant", "text-anchor", "dominant-baseline", "alignment-baseline", "baseline-shift",
    "letter-spacing", "word-spacing", "text-decoration", "dx", "dy", "rotate", "textLength",
    "lengthAdjust", "startOffset", "patternUnits", "patternContentUnits", "patternTransform",
    "markerWidth", "markerHeight", "refX", "refY", "orient", "markerUnits", "marker-start",
    "marker-mid", "marker-end", "version", "visibility", "display", "color", "direction",
    "writing-mode", "overflow", "vector-effect", "shape-rendering", "text-rendering",
    "image-rendering", "color-interpolation",
];

const MATH_ALLOWED: &[&str] = &[
    "math", "mrow", "mi", "mn", "mo", "ms", "mtext", "mspace", "msup", "msub", "msubsup", "mfrac",
    "msqrt", "mroot", "mfenced", "mtable", "mtr", "mtd", "munder", "mover", "munderover", "mstyle",
    "mpadded", "mphantom", "menclose", "merror", "mmultiscripts", "mprescripts", "none", "semantics",
];

const MATH_ATTRS: &[&str] = &[
    "id", "class", "display", "alttext", "mathvariant", "mathsize", "displaystyle", "scriptlevel",
    "open", "close", "separators", "fence", "stretchy", "symmetric", "largeop", "movablelimits",
    "accent", "accentunder", "linethickness", "notation", "columnalign", "rowalign", "columnspan",
    "rowspan", "columnlines", "rowlines", "frame", "lspace", "rspace", "width", "height", "depth",
    "dir", "form", "minsize", "maxsize", "bevelled", "numalign", "denomalign",
];

const MATH_DROP: &[&str] = &["annotation", "annotation-xml", "mglyph", "malignmark", "maligngroup"];

fn valid_lang(v: &str) -> bool {
    !v.is_empty() && v.len() <= 35 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_int(v: &str, min: i64, max: i64) -> bool {
    v.len() <= 6 && v.parse::<i64>().map(|n| n >= min && n <= max).unwrap_or(false)
}

fn valid_dimension(v: &str) -> bool {
    let d = v.strip_suffix('%').unwrap_or(v);
    !d.is_empty() && d.len() <= 6 && d.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

fn clean_class(v: &str) -> Option<String> {
    let toks: Vec<&str> = v
        .split_ascii_whitespace()
        .filter(|t| {
            t.len() <= 64
                && !t.starts_with("thoth")
                && t.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        })
        .take(32)
        .collect();
    if toks.is_empty() {
        None
    } else {
        Some(toks.join(" "))
    }
}

fn clean_epub_type(v: &str) -> Option<String> {
    let toks: Vec<&str> = v
        .split_ascii_whitespace()
        .filter(|t| t.len() <= 64 && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-')))
        .take(16)
        .collect();
    if toks.is_empty() {
        None
    } else {
        Some(toks.join(" "))
    }
}

fn short_text(v: &str, max_chars: usize) -> String {
    clean_text(v).chars().take(max_chars).collect()
}

/// `<use>` elements kept per document.
const MAX_SVG_USE: usize = 1000;
/// Total number of elements all kept `<use>` references may instantiate.
const MAX_SVG_USE_INSTANCES: usize = 100_000;

/// `url(#id)` (with optional whitespace) is the only url form allowed in SVG
/// presentation attributes.
fn svg_paint_ok(v: &str) -> bool {
    // CSS escapes (`\75rl(...)`) would hide a url() from the substring scan below.
    if v.contains('\\') {
        return false;
    }
    let l = v.to_ascii_lowercase();
    if l.contains("javascript:") || l.contains("expression") {
        return false;
    }
    let mut rest = l.as_str();
    while let Some(i) = rest.find("url(") {
        let after = &rest[i + 4..];
        let Some(end) = after.find(')') else { return false };
        let inner = after[..end].trim().trim_matches(|c| c == '"' || c == '\'');
        match inner.strip_prefix('#') {
            Some(id) if is_safe_id(id) => {}
            _ => return false,
        }
        rest = &after[end + 1..];
    }
    true
}

// ---------------------------------------------------------------------------
// Sanitizer
// ---------------------------------------------------------------------------

pub(crate) struct Sanitizer<'a> {
    policy: &'a dyn UrlPolicy,
    /// Resolves url()s inside inline `<style>` elements (output relative to `r/`).
    style_policy: Option<&'a dyn UrlPolicy>,
    ids: HashSet<String>,
    pub report: DocReport,
    styles: Vec<StyleRef>,
    title: Option<String>,
}

impl<'a> Sanitizer<'a> {
    pub(crate) fn new(policy: &'a dyn UrlPolicy, style_policy: Option<&'a dyn UrlPolicy>) -> Self {
        Sanitizer { policy, style_policy, ids: HashSet::new(), report: DocReport::default(), styles: Vec::new(), title: None }
    }

    fn take_id(&mut self, v: &str) -> Option<String> {
        if is_safe_id(v) && self.ids.insert(v.to_string()) {
            Some(v.to_string())
        } else {
            None
        }
    }

    fn style_attr(&mut self, v: &str) -> Option<String> {
        let (out, rep) = sanitize_style_attr(v, self.policy);
        self.report.css.merge(&rep);
        out
    }

    /// Sanitize a full content document (`<html>` root).
    pub(crate) fn document(mut self, root: &Element) -> SanitizedDoc {
        let mut body_src: Option<&Element> = None;
        let mut lang = None;
        let mut dir = None;
        for (k, v) in &root.attrs {
            match k.as_str() {
                "lang" | "xml:lang" if valid_lang(v) && lang.is_none() => lang = Some(v.clone()),
                "dir" if matches!(v.as_str(), "ltr" | "rtl" | "auto") => dir = Some(v.clone()),
                _ => {}
            }
        }
        let mut loose = Element::new(Ns::Html, "body");
        for c in &root.children {
            match c {
                Node::Element(e) if e.is_html("head") => self.head(e),
                Node::Element(e) if e.is_html("body") && body_src.is_none() => body_src = Some(e),
                other => loose.children.push(other.clone()),
            }
        }
        let body_src = match body_src {
            Some(b) => b,
            None => &loose,
        };
        let mut body = Element::new(Ns::Html, "body");
        self.html_attrs(body_src, &mut body);
        if body.attr("dir") == Some("rtl") || dir.as_deref() == Some("rtl") {
            self.report.rtl = true;
        }
        self.html_children(body_src, &mut body, 1);
        self.limit_svg_use(&mut body);
        SanitizedDoc { body, lang, dir, title: self.title.take(), styles: std::mem::take(&mut self.styles), report: self.report }
    }

    fn head(&mut self, head: &Element) {
        for c in head.child_elements() {
            self.head_element(c);
        }
    }

    fn head_element(&mut self, e: &Element) {
        if e.ns != Ns::Html {
            return;
        }
        match e.name.as_str() {
            "title" => {
                if self.title.is_none() {
                    let t = short_text(&e.text_content(), 512);
                    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !t.is_empty() {
                        self.title = Some(t);
                    }
                }
            }
            "style" => self.inline_style(e),
            "link" => self.link(e),
            "script" | "noscript" => self.report.scripts_removed = true,
            "meta" | "base" => self.report.meta_removed = true,
            _ => {}
        }
    }

    fn inline_style(&mut self, e: &Element) {
        let Some(sp) = self.style_policy else { return };
        let css = e.text_content();
        let (out, rep) = sanitize_stylesheet(&css, sp);
        self.report.css.merge(&rep);
        let Some(out) = out else { return };
        if out.trim().is_empty() {
            return;
        }
        // All inline styles of a document share one resource, capped in size.
        let existing = self.styles.iter_mut().find_map(|s| match s {
            StyleRef::Inline(css) => Some(css),
            StyleRef::Link(_) => None,
        });
        let used = existing.as_ref().map(|c| c.len()).unwrap_or(0);
        if used.saturating_add(out.len()) > MAX_STYLESHEET_BYTES {
            self.report.inline_style_truncated = true;
            return;
        }
        match existing {
            Some(css) => css.push_str(&out),
            None => self.styles.push(StyleRef::Inline(out)),
        }
    }

    /// Bound SVG `<use>` instantiation: no `<use>` may reference a subtree that
    /// itself contains (or is) a `<use>` (reference chains multiply in the
    /// renderer), at most [`MAX_SVG_USE`] are kept, and together they may clone at
    /// most [`MAX_SVG_USE_INSTANCES`] elements. Ids are unique after sanitizing.
    fn limit_svg_use(&mut self, root: &mut Element) {
        /// Returns (element count, contains-or-is `<use>`) of `e`'s subtree.
        fn index(e: &Element, targets: &mut HashMap<String, (usize, bool)>, uses: &mut usize) -> (usize, bool) {
            let is_use = e.is(Ns::Svg, "use");
            if is_use {
                *uses += 1;
            }
            let mut size = 1usize;
            let mut has_use = is_use;
            for c in e.child_elements() {
                let (s, u) = index(c, targets, uses);
                size = size.saturating_add(s);
                has_use |= u;
            }
            if let Some(id) = e.attr("id") {
                targets.insert(id.to_string(), (size, has_use));
            }
            (size, has_use)
        }
        struct Prune<'t> {
            targets: &'t HashMap<String, (usize, bool)>,
            kept: usize,
            instances: usize,
            removed: bool,
        }
        fn prune(e: &mut Element, st: &mut Prune<'_>) {
            e.children.retain_mut(|c| {
                let Node::Element(ch) = c else { return true };
                if !ch.is(Ns::Svg, "use") {
                    prune(ch, st);
                    return true;
                }
                let target = ch.attr("href").and_then(|h| h.strip_prefix('#')).and_then(|id| st.targets.get(id));
                let keep = match target {
                    // Dangling reference: renders nothing, costs nothing.
                    None => st.kept < MAX_SVG_USE,
                    Some((size, has_use)) => {
                        !has_use
                            && st.kept < MAX_SVG_USE
                            && st.instances.saturating_add(*size) <= MAX_SVG_USE_INSTANCES
                    }
                };
                if keep {
                    st.kept += 1;
                    st.instances = st.instances.saturating_add(target.map(|t| t.0).unwrap_or(0));
                    // Children of <use> (title/desc) cannot hold further <use>.
                    prune(ch, st);
                } else {
                    st.removed = true;
                }
                keep
            });
        }
        let mut targets = HashMap::new();
        let mut uses = 0usize;
        index(root, &mut targets, &mut uses);
        if uses == 0 {
            return;
        }
        let mut st = Prune { targets: &targets, kept: 0, instances: 0, removed: false };
        prune(root, &mut st);
        if st.removed {
            self.report.svg_use_removed = true;
        }
    }

    fn link(&mut self, e: &Element) {
        let rel = e.attr("rel").unwrap_or("").to_ascii_lowercase();
        if !rel.split_ascii_whitespace().any(|t| t == "stylesheet") || rel.contains("alternate") {
            return;
        }
        let Some(href) = e.attr("href") else { return };
        match self.policy.resolve(href, RefUse::Stylesheet) {
            Resolved::Internal(h) => {
                if !self.styles.contains(&StyleRef::Link(h.clone())) {
                    self.styles.push(StyleRef::Link(h));
                }
            }
            Resolved::External(_) => self.report.css.remote_removed = true,
            Resolved::Drop(r) => self.report.note_drop(&r),
        }
    }

    fn html_children(&mut self, src: &Element, out: &mut Element, depth: usize) {
        for c in &src.children {
            match c {
                Node::Text(t) => out.push_text(&clean_text(t)),
                Node::Element(e) => self.any_element(e, out, depth),
            }
        }
    }

    fn any_element(&mut self, e: &Element, out: &mut Element, depth: usize) {
        if depth >= MAX_DEPTH {
            out.push_text(&clean_text(&e.text_content()));
            return;
        }
        match e.ns {
            Ns::Html => self.html_element(e, out, depth),
            Ns::Svg if e.name == "svg" => {
                if let Some(svg) = self.svg_element(e, depth) {
                    out.children.push(Node::Element(svg));
                }
            }
            Ns::MathMl if e.name == "math" => {
                let m = self.math_element(e, depth);
                out.children.push(Node::Element(m));
            }
            Ns::Ops if e.name == "switch" => {
                // epub:switch -> use the epub:default fallback content.
                if let Some(d) = e.child_elements().find(|c| c.is(Ns::Ops, "default")) {
                    self.html_children(d, out, depth + 1);
                }
            }
            _ => {}
        }
    }

    fn html_attrs(&mut self, src: &Element, out: &mut Element) {
        let name = src.name.as_str();
        let mut lang: Option<String> = None;
        for (k, v) in &src.attrs {
            let kl = k.to_ascii_lowercase();
            if kl.starts_with("on") {
                self.report.handlers_removed = true;
                continue;
            }
            let kept: Option<(&str, String)> = match kl.as_str() {
                "id" => self.take_id(v).map(|v| ("id", v)),
                "class" => clean_class(v).map(|v| ("class", v)),
                "title" => Some(("title", short_text(v, 1024))),
                "lang" | "xml:lang" => {
                    if valid_lang(v) && lang.is_none() {
                        lang = Some(v.clone());
                    }
                    None
                }
                "dir" if matches!(v.as_str(), "ltr" | "rtl" | "auto") => Some(("dir", v.clone())),
                "style" => self.style_attr(v).map(|v| ("style", v)),
                "epub:type" | "data-epub-type" => clean_epub_type(v).map(|v| ("data-epub-type", v)),
                "alt" if name == "img" => Some(("alt", short_text(v, 2048))),
                "width" | "height" if name == "img" && valid_dimension(v) => {
                    Some((if kl == "width" { "width" } else { "height" }, v.clone()))
                }
                "colspan" | "rowspan" if matches!(name, "td" | "th") && valid_int(v, 1, 1000) => {
                    Some((if kl == "colspan" { "colspan" } else { "rowspan" }, v.clone()))
                }
                "scope" if name == "th" && matches!(v.as_str(), "row" | "col" | "rowgroup" | "colgroup") => {
                    Some(("scope", v.clone()))
                }
                "span" if matches!(name, "col" | "colgroup") && valid_int(v, 1, 1000) => Some(("span", v.clone())),
                "start" if name == "ol" && valid_int(v, -99999, 999999) => Some(("start", v.clone())),
                "reversed" if name == "ol" => Some(("reversed", "reversed".to_string())),
                "type" if name == "ol" && matches!(v.as_str(), "1" | "a" | "A" | "i" | "I") => Some(("type", v.clone())),
                "value" if name == "li" && valid_int(v, -99999, 999999) => Some(("value", v.clone())),
                "datetime" if name == "time" && v.len() <= 64 && !v.contains(['<', '>']) => {
                    Some(("datetime", short_text(v, 64)))
                }
                _ => None,
            };
            if let Some((k2, v2)) = kept {
                if out.attr(k2).is_none() {
                    out.attrs.push((k2.to_string(), v2));
                }
            }
        }
        if let Some(l) = lang {
            out.attrs.push(("lang".into(), l));
        }
        if out.attr("dir") == Some("rtl") {
            self.report.rtl = true;
        }
    }

    fn html_element(&mut self, e: &Element, out: &mut Element, depth: usize) {
        let name = e.name.as_str();
        if HTML_DROP_SCRIPT.contains(&name) {
            self.report.scripts_removed = true;
            return;
        }
        if HTML_DROP_MEDIA.contains(&name) {
            self.report.media_removed = true;
            return;
        }
        if HTML_DROP_FORM.contains(&name) {
            self.report.forms_removed = true;
            return;
        }
        if HTML_DROP_EMBED.contains(&name) {
            self.report.embeds_removed = true;
            return;
        }
        if HTML_DROP_META.contains(&name) {
            self.report.meta_removed = true;
            return;
        }
        match name {
            "style" => {
                self.inline_style(e);
                return;
            }
            "link" => {
                self.link(e);
                return;
            }
            "title" | "head" => {
                if name == "head" {
                    self.head(e);
                }
                return;
            }
            "form" => self.report.forms_removed = true,
            _ => {}
        }
        if !HTML_ALLOWED.contains(&name) {
            // Unknown / presentational wrappers (font, center, form, noscript, custom
            // elements, ...) are unwrapped: their children survive.
            self.html_children(e, out, depth + 1);
            return;
        }
        let mut el = Element::new(Ns::Html, name);
        self.html_attrs(e, &mut el);
        match name {
            "a" => {
                if let Some(href) = e.attr("href") {
                    match self.policy.resolve(href, RefUse::Link) {
                        Resolved::Internal(h) => el.attrs.push(("href".into(), h)),
                        Resolved::External(u) => {
                            el.attrs.push(("href".into(), u));
                            el.attrs.push(("data-thoth-external".into(), "1".into()));
                        }
                        Resolved::Drop(r) => self.report.note_drop(&r),
                    }
                }
            }
            "img" => {
                let Some(src) = e.attr("src") else { return };
                match self.policy.resolve(src, RefUse::Image) {
                    Resolved::Internal(h) => el.attrs.insert(0, ("src".into(), h)),
                    Resolved::External(_) => {
                        self.report.css.remote_removed = true;
                        return;
                    }
                    Resolved::Drop(r) => {
                        self.report.note_drop(&r);
                        return;
                    }
                }
            }
            _ => {}
        }
        if !super::dom::is_void_html(name) {
            self.html_children(e, &mut el, depth + 1);
        }
        if name == "pre" {
            // The HTML parser drops a newline right after <pre>; keep both parsers equal.
            if let Some(Node::Text(t)) = el.children.first_mut() {
                let trimmed = t.trim_start_matches('\n').to_string();
                if trimmed.is_empty() {
                    el.children.remove(0);
                } else {
                    *t = trimmed;
                }
            }
        }
        out.children.push(Node::Element(el));
    }

    // ---------------------------------------------------------------- SVG

    pub(crate) fn svg_element(&mut self, e: &Element, depth: usize) -> Option<Element> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let name = e.name.as_str();
        match name {
            "script" => {
                self.report.scripts_removed = true;
                return None;
            }
            "foreignObject" => {
                self.report.foreign_object_removed = true;
                return None;
            }
            _ => {}
        }
        // SVG <a> becomes a plain group (links inside images are not followed).
        let out_name = if name == "a" { "g" } else { name };
        if !SVG_ALLOWED.contains(&out_name) {
            return None;
        }
        let mut el = Element::new(Ns::Svg, out_name);
        for (k, v) in &e.attrs {
            if k.to_ascii_lowercase().starts_with("on") {
                self.report.handlers_removed = true;
                continue;
            }
            match k.as_str() {
                "href" | "xlink:href" => {
                    if name == "a" || el.attr("href").is_some() {
                        continue;
                    }
                    if out_name == "image" {
                        match self.policy.resolve(v, RefUse::Image) {
                            // No SVG-in-SVG: prevents recursive image references.
                            Resolved::Internal(h) if !h.ends_with(".svg") => el.attrs.push(("href".into(), h)),
                            Resolved::Internal(_) => self.report.unsafe_url_removed = true,
                            Resolved::External(_) => self.report.css.remote_removed = true,
                            Resolved::Drop(r) => self.report.note_drop(&r),
                        }
                    } else {
                        match v.trim().strip_prefix('#') {
                            Some(id) if is_safe_id(id) => el.attrs.push(("href".into(), format!("#{id}"))),
                            _ => {
                                if v.contains("://") || v.starts_with("//") {
                                    self.report.css.remote_removed = true;
                                } else {
                                    self.report.unsafe_url_removed = true;
                                }
                            }
                        }
                    }
                }
                "id" => {
                    if let Some(id) = self.take_id(v) {
                        el.attrs.push(("id".into(), id));
                    }
                }
                "style" => {
                    if let Some(s) = self.style_attr(v) {
                        el.attrs.push(("style".into(), s));
                    }
                }
                "class" => {
                    if let Some(c) = clean_class(v) {
                        el.attrs.push(("class".into(), c));
                    }
                }
                k if SVG_ATTRS.contains(&k) => {
                    if svg_paint_ok(v) {
                        el.attrs.push((k.to_string(), short_text(v, 64 * 1024)));
                    } else {
                        self.report.unsafe_url_removed = true;
                    }
                }
                _ => {}
            }
        }
        if matches!(out_name, "image" | "use") && el.attr("href").is_none() {
            return None;
        }
        for c in &e.children {
            match c {
                Node::Text(t) => el.push_text(&clean_text(t)),
                Node::Element(ch) if ch.ns == Ns::Svg => {
                    if let Some(s) = self.svg_element(ch, depth + 1) {
                        el.children.push(Node::Element(s));
                    }
                }
                Node::Element(ch) => {
                    if ch.is_html("script") || ch.name == "script" {
                        self.report.scripts_removed = true;
                    }
                }
            }
        }
        Some(el)
    }

    // ------------------------------------------------------------- MathML

    fn math_element(&mut self, e: &Element, depth: usize) -> Element {
        let name = e.name.as_str();
        let mut el = Element::new(Ns::MathMl, name);
        for (k, v) in &e.attrs {
            if k.to_ascii_lowercase().starts_with("on") {
                self.report.handlers_removed = true;
                continue;
            }
            match k.as_str() {
                "id" => {
                    if let Some(id) = self.take_id(v) {
                        el.attrs.push(("id".into(), id));
                    }
                }
                "style" => {
                    if let Some(s) = self.style_attr(v) {
                        el.attrs.push(("style".into(), s));
                    }
                }
                "class" => {
                    if let Some(c) = clean_class(v) {
                        el.attrs.push(("class".into(), c));
                    }
                }
                k if MATH_ATTRS.contains(&k) => {
                    let l = v.to_ascii_lowercase();
                    if !l.contains("url(") && !l.contains("javascript:") {
                        el.attrs.push((k.to_string(), short_text(v, 4096)));
                    }
                }
                _ => {}
            }
        }
        let mut seen_semantics_child = false;
        for c in &e.children {
            match c {
                Node::Text(t) => el.push_text(&clean_text(t)),
                Node::Element(ch) => {
                    if name == "semantics" {
                        if seen_semantics_child {
                            continue;
                        }
                        seen_semantics_child = true;
                    }
                    if depth + 1 >= MAX_DEPTH {
                        el.push_text(&clean_text(&ch.text_content()));
                        continue;
                    }
                    if ch.is(Ns::MathMl, "maction") {
                        // Interactive: render statically as its first (selected) child.
                        self.report.mathml_fallback = true;
                        if let Some(first) = ch.child_elements().next() {
                            let mut wrapper = Element::new(Ns::MathMl, "mrow");
                            wrapper.children.push(Node::Element(first.clone()));
                            let m = self.math_element(&wrapper, depth + 1);
                            el.children.push(Node::Element(m));
                        }
                    } else if ch.ns == Ns::MathMl && MATH_ALLOWED.contains(&ch.name.as_str()) && ch.name != "math" {
                        let m = self.math_element(ch, depth + 1);
                        el.children.push(Node::Element(m));
                    } else if ch.ns == Ns::MathMl && MATH_DROP.contains(&ch.name.as_str()) {
                        // annotations are dropped
                    } else {
                        // Unsupported -> static text fallback.
                        self.report.mathml_fallback = true;
                        if ch.is_html("script") || ch.name == "script" {
                            self.report.scripts_removed = true;
                            continue;
                        }
                        let t = clean_text(&ch.text_content());
                        if !t.trim().is_empty() {
                            let mut mt = Element::new(Ns::MathMl, "mtext");
                            mt.push_text(&t);
                            el.children.push(Node::Element(mt));
                        }
                    }
                }
            }
        }
        el
    }
}

/// Build the canonical `<html>` tree.
pub(crate) fn assemble(head: &Head, body: Element) -> Element {
    let mut html = Element::new(Ns::Html, "html");
    if let Some(l) = &head.lang {
        html.attrs.push(("lang".into(), l.clone()));
    }
    if let Some(d) = &head.dir {
        html.attrs.push(("dir".into(), d.clone()));
    }
    let mut h = Element::new(Ns::Html, "head");
    let mut csp = Element::new(Ns::Html, "meta");
    csp.attrs.push(("http-equiv".into(), "Content-Security-Policy".into()));
    csp.attrs.push(("content".into(), CONTENT_CSP.into()));
    h.children.push(Node::Element(csp));
    if !head.flags.is_empty() {
        let mut m = Element::new(Ns::Html, "meta");
        m.attrs.push(("name".into(), "thoth-restrictions".into()));
        m.attrs.push(("content".into(), head.flags.join(" ")));
        h.children.push(Node::Element(m));
    }
    let mut t = Element::new(Ns::Html, "title");
    t.push_text(&clean_text(&head.title));
    h.children.push(Node::Element(t));
    for l in &head.links {
        let mut link = Element::new(Ns::Html, "link");
        link.attrs.push(("rel".into(), "stylesheet".into()));
        link.attrs.push(("type".into(), "text/css".into()));
        link.attrs.push(("href".into(), l.clone()));
        h.children.push(Node::Element(link));
    }
    html.children.push(Node::Element(h));
    html.children.push(Node::Element(body));
    html
}

/// Serialize, then re-parse with the HTML parser and re-sanitize until the output
/// is a fixpoint, so that the XML DOM and the HTML (srcdoc) DOM are identical.
/// Returns (xhtml, converged), or [`HtmlLimit`] when re-parsing exhausts the HTML
/// parser budgets or a pass grows the document instead of converging.
pub(crate) fn canonicalize(head: &Head, body: Element, spine_index: u32) -> Result<(String, bool), HtmlLimit> {
    let mut xhtml = serialize_document(&assemble(head, body));
    for _ in 0..4 {
        let reparsed = parse_html(&xhtml)?;
        let pol = CanonicalPolicy { from_spine: Some(spine_index) };
        let doc = Sanitizer::new(&pol, None).document(&reparsed);
        let next = serialize_document(&assemble(head, doc.body));
        if next == xhtml {
            return Ok((xhtml, true));
        }
        // A re-parse may legitimately reshuffle a little markup, never multiply it.
        if next.len() > xhtml.len().saturating_mul(2).saturating_add(CANONICAL_GROWTH_SLACK) {
            return Err(HtmlLimit);
        }
        xhtml = next;
    }
    Ok((xhtml, false))
}

const CANONICAL_GROWTH_SLACK: usize = 64 * 1024;

/// Sanitize a standalone SVG image resource. Returns serialized XML or None.
pub(crate) fn sanitize_svg_resource(root: &Element, policy: &dyn UrlPolicy) -> (Option<String>, DocReport) {
    let mut s = Sanitizer::new(policy, None);
    if root.name != "svg" || !matches!(root.ns, Ns::Svg | Ns::Other | Ns::Html) {
        return (None, s.report);
    }
    // Normalize namespace-less <svg> roots into the SVG namespace.
    let mut fixed = root.clone();
    fn set_svg(e: &mut Element) {
        if e.ns != Ns::Svg {
            e.ns = Ns::Svg;
        }
        for c in &mut e.children {
            if let Node::Element(ch) = c {
                set_svg(ch);
            }
        }
    }
    if root.ns != Ns::Svg {
        set_svg(&mut fixed);
    }
    let mut out = s.svg_element(&fixed, 0);
    if let Some(svg) = out.as_mut() {
        // The root itself is never a <use>; everything below it is checked.
        s.limit_svg_use(svg);
    }
    let report = s.report.clone();
    match out {
        Some(svg) => {
            let mut text = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            text.push_str(&serialize_fragment(&svg));
            (Some(text), report)
        }
        None => (None, report),
    }
}
