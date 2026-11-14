//! Parser-based CSS sanitizer (spec §5.2). The input is tokenized with cssparser
//! into an owned component tree, then rebuilt from an allow-list of rules,
//! properties and functions. Every `url()` goes through the [`UrlPolicy`].

use cssparser::{serialize_identifier, serialize_name, serialize_string, Parser, ToCss, Token};

use super::refs::{DropReason, RefUse, Resolved, UrlPolicy};

pub(crate) const MAX_STYLESHEET_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_STYLE_ATTR_BYTES: usize = 16 * 1024;
const MAX_CSS_DEPTH: usize = 16;
const MAX_CSS_TOKENS: usize = 500_000;
const MAX_Z_INDEX: i64 = 100;

/// Side effects noticed while sanitizing (feeds package restrictions).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct CssReport {
    pub vertical: bool,
    pub rtl: bool,
    pub remote_removed: bool,
    pub import_removed: bool,
    pub missing: Vec<String>,
    pub rejected: bool,
}

impl CssReport {
    pub(crate) fn merge(&mut self, o: &CssReport) {
        self.vertical |= o.vertical;
        self.rtl |= o.rtl;
        self.remote_removed |= o.remote_removed;
        self.import_removed |= o.import_removed;
        self.rejected |= o.rejected;
        for m in &o.missing {
            if !self.missing.contains(m) {
                self.missing.push(m.clone());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum C {
    Ws,
    Ident(String),
    AtKw(String),
    Hash(String),
    Str(String),
    Url(String),
    Delim(char),
    Colon,
    Semi,
    Comma,
    Func(String, Vec<C>),
    Paren(Vec<C>),
    Square(Vec<C>),
    Curly(Vec<C>),
    /// Numbers, dimensions, percentages, attribute matchers: already serialized.
    Raw(String),
    Bad,
}

#[derive(Default)]
struct ReadState {
    tokens: usize,
    too_big: bool,
}

fn read_list(p: &mut Parser<'_>, depth: usize, st: &mut ReadState) -> Vec<C> {
    let mut out = Vec::new();
    while let Ok(t) = p.next_including_whitespace_and_comments() {
        let tok = t.clone();
        st.tokens += 1;
        if st.tokens > MAX_CSS_TOKENS {
            st.too_big = true;
        }
        let node = match tok {
            Token::WhiteSpace(_) | Token::Comment(_) => C::Ws,
            Token::Ident(s) => C::Ident(s.to_string()),
            Token::AtKeyword(s) => C::AtKw(s.to_string()),
            Token::Hash(s) | Token::IDHash(s) => C::Hash(s.to_string()),
            Token::QuotedString(s) => C::Str(s.to_string()),
            Token::UnquotedUrl(s) => C::Url(s.to_string()),
            Token::Delim(c) => C::Delim(c),
            Token::Colon => C::Colon,
            Token::Semicolon => C::Semi,
            Token::Comma => C::Comma,
            Token::BadUrl(_) | Token::BadString(_) => C::Bad,
            Token::CloseParenthesis | Token::CloseSquareBracket | Token::CloseCurlyBracket => C::Bad,
            Token::CDO | Token::CDC => C::Ws,
            Token::Function(_) | Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                let inner = if depth + 1 > MAX_CSS_DEPTH || st.too_big {
                    st.too_big = true;
                    let _ = p.parse_nested_block(|p2| -> Result<(), cssparser::ParseError<()>> {
                        while p2.next_including_whitespace_and_comments().is_ok() {}
                        Ok(())
                    });
                    Vec::new()
                } else {
                    p.parse_nested_block(|p2| -> Result<Vec<C>, cssparser::ParseError<()>> {
                        Ok(read_list(p2, depth + 1, st))
                    })
                    .unwrap_or_default()
                };
                match tok {
                    Token::Function(name) => {
                        let lname = name.to_ascii_lowercase();
                        if lname == "url" {
                            // url("...") form: exactly one string inside.
                            let strs: Vec<&C> = inner.iter().filter(|c| **c != C::Ws).collect();
                            match strs.as_slice() {
                                [C::Str(s)] => C::Url(s.clone()),
                                _ => C::Bad,
                            }
                        } else {
                            C::Func(lname, inner)
                        }
                    }
                    Token::ParenthesisBlock => C::Paren(inner),
                    Token::SquareBracketBlock => C::Square(inner),
                    _ => C::Curly(inner),
                }
            }
            other => C::Raw(other.to_css_string()),
        };
        // Collapse runs of whitespace.
        if node == C::Ws && matches!(out.last(), Some(C::Ws)) {
            continue;
        }
        out.push(node);
    }
    out
}

fn parse_components(src: &str) -> Option<Vec<C>> {
    let mut p = Parser::new(src);
    let mut st = ReadState::default();
    let list = read_list(&mut p, 0, &mut st);
    if st.too_big {
        None
    } else {
        Some(list)
    }
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

fn ser(list: &[C], out: &mut String) {
    for c in list {
        ser_one(c, out);
    }
}

fn ser_one(c: &C, out: &mut String) {
    match c {
        C::Ws => out.push(' '),
        C::Ident(s) => {
            let _ = serialize_identifier(s, out);
        }
        C::AtKw(s) => {
            out.push('@');
            let _ = serialize_identifier(s, out);
        }
        C::Hash(s) => {
            out.push('#');
            let _ = serialize_name(s, out);
        }
        C::Str(s) => {
            let _ = serialize_string(s, out);
        }
        C::Url(s) => {
            out.push_str("url(");
            let _ = serialize_string(s, out);
            out.push(')');
        }
        C::Delim(ch) => out.push(*ch),
        C::Colon => out.push(':'),
        C::Semi => out.push(';'),
        C::Comma => out.push(','),
        C::Func(n, inner) => {
            let _ = serialize_identifier(n, out);
            out.push('(');
            ser(inner, out);
            out.push(')');
        }
        C::Paren(inner) => {
            out.push('(');
            ser(inner, out);
            out.push(')');
        }
        C::Square(inner) => {
            out.push('[');
            ser(inner, out);
            out.push(']');
        }
        C::Curly(inner) => {
            out.push('{');
            ser(inner, out);
            out.push('}');
        }
        C::Raw(s) => out.push_str(s),
        C::Bad => {}
    }
}

fn trim_ws(list: &[C]) -> &[C] {
    let mut a = 0;
    let mut b = list.len();
    while a < b && list[a] == C::Ws {
        a += 1;
    }
    while b > a && list[b - 1] == C::Ws {
        b -= 1;
    }
    &list[a..b]
}

// ---------------------------------------------------------------------------
// Allow-lists
// ---------------------------------------------------------------------------

const ALLOWED_PROPERTIES: &[&str] = &[
    "color", "background", "background-color", "background-image", "background-repeat",
    "background-position", "background-size", "background-clip", "background-origin",
    "border", "border-top", "border-right", "border-bottom", "border-left", "border-color",
    "border-style", "border-width", "border-top-color", "border-right-color",
    "border-bottom-color", "border-left-color", "border-top-style", "border-right-style",
    "border-bottom-style", "border-left-style", "border-top-width", "border-right-width",
    "border-bottom-width", "border-left-width", "border-radius", "border-collapse",
    "border-spacing", "margin", "margin-top", "margin-right", "margin-bottom", "margin-left",
    "padding", "padding-top", "padding-right", "padding-bottom", "padding-left", "font",
    "font-family", "font-size", "font-style", "font-weight", "font-variant", "font-stretch",
    "font-feature-settings", "font-variant-ligatures", "font-variant-numeric",
    "font-variant-east-asian", "font-variant-caps", "font-kerning", "line-height",
    "letter-spacing", "word-spacing", "word-break", "overflow-wrap", "word-wrap", "text-align",
    "text-align-last", "text-indent", "text-decoration", "text-decoration-line",
    "text-decoration-style", "text-decoration-color", "text-underline-position",
    "text-transform", "text-shadow", "text-emphasis", "text-emphasis-style",
    "text-emphasis-color", "text-emphasis-position", "white-space", "vertical-align",
    "list-style", "list-style-type", "list-style-position", "list-style-image", "display",
    "width", "height", "max-width", "max-height", "min-width", "min-height", "float", "clear",
    "caption-side", "empty-cells", "table-layout", "page-break-before", "page-break-after",
    "page-break-inside", "break-before", "break-after", "break-inside", "orphans", "widows",
    "hyphens", "-webkit-hyphens", "-epub-hyphens", "direction", "unicode-bidi", "writing-mode",
    "-webkit-writing-mode", "-epub-writing-mode", "text-orientation", "-webkit-text-orientation",
    "-epub-text-orientation", "text-combine-upright", "-webkit-text-combine",
    "-epub-text-combine", "ruby-position", "ruby-align", "quotes", "opacity", "overflow",
    "overflow-x", "overflow-y", "position", "top", "right", "bottom", "left", "z-index",
    "visibility", "box-sizing", "box-shadow", "outline", "outline-color", "outline-style",
    "outline-width", "content", "counter-reset", "counter-increment", "src", "unicode-range",
    "font-display", "line-break", "-webkit-line-break", "-epub-line-break", "text-justify",
    "column-count", "column-gap", "column-width", "columns", "object-fit", "object-position",
];

const ALLOWED_FUNCTIONS: &[&str] = &[
    "rgb", "rgba", "hsl", "hsla", "calc", "min", "max", "clamp", "linear-gradient",
    "radial-gradient", "repeating-linear-gradient", "repeating-radial-gradient", "counter",
    "counters", "format", "local", "attr",
];

const ALLOWED_PSEUDO_FUNCTIONS: &[&str] = &[
    "not", "is", "where", "nth-child", "nth-last-child", "nth-of-type", "nth-last-of-type",
    "lang", "dir", "has",
];

struct Ctx<'a> {
    policy: &'a dyn UrlPolicy,
    report: CssReport,
}

impl Ctx<'_> {
    fn note_drop(&mut self, r: &DropReason) {
        match r {
            DropReason::Remote => self.report.remote_removed = true,
            DropReason::Missing(p) if !self.report.missing.contains(p) => {
                self.report.missing.push(p.clone());
            }
            _ => {}
        }
    }

    /// Validate value components recursively, rewriting urls. `false` drops the
    /// whole declaration.
    fn clean_value(&mut self, list: &mut [C], url_use: Option<RefUse>) -> bool {
        for c in list.iter_mut() {
            match c {
                C::Bad | C::Curly(_) | C::AtKw(_) | C::Semi => return false,
                C::Url(u) => {
                    let Some(use_) = url_use else { return false };
                    match self.policy.resolve(u, use_) {
                        Resolved::Internal(s) => *u = s,
                        Resolved::Drop(r) => {
                            self.note_drop(&r);
                            return false;
                        }
                        Resolved::External(_) => {
                            self.report.remote_removed = true;
                            return false;
                        }
                    }
                }
                C::Func(name, inner) => {
                    if !ALLOWED_FUNCTIONS.contains(&name.as_str()) {
                        return false;
                    }
                    if !self.clean_value(inner, url_use) {
                        return false;
                    }
                }
                C::Paren(inner) | C::Square(inner) => {
                    if !self.clean_value(inner, url_use) {
                        return false;
                    }
                }
                C::Ident(s) => {
                    let l = s.to_ascii_lowercase();
                    if l.contains("expression") || l.contains("javascript") || l.starts_with("--") {
                        return false;
                    }
                }
                C::Str(s) => {
                    let l = s.to_ascii_lowercase();
                    if l.contains("javascript:") || l.contains("expression(") {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// Returns the sanitized `name:value[ !important]` text, or None to drop.
    fn declaration(&mut self, decl: &[C], in_font_face: bool) -> Option<String> {
        let decl = trim_ws(decl);
        let mut it = decl.iter().enumerate().filter(|(_, c)| **c != C::Ws);
        let (_, C::Ident(name)) = it.next()? else { return None };
        let (ci, C::Colon) = it.next()? else { return None };
        let name = name.to_ascii_lowercase();
        if name.starts_with("--") || !ALLOWED_PROPERTIES.contains(&name.as_str()) {
            return None;
        }
        if (name == "src" || name == "unicode-range" || name == "font-display") && !in_font_face {
            return None;
        }
        let mut value: Vec<C> = trim_ws(&decl[ci + 1..]).to_vec();
        // Strip `! important`.
        let mut important = false;
        {
            let sig: Vec<usize> = value.iter().enumerate().filter(|(_, c)| **c != C::Ws).map(|(i, _)| i).collect();
            if sig.len() >= 2 {
                let (a, b) = (sig[sig.len() - 2], sig[sig.len() - 1]);
                if value[a] == C::Delim('!') && matches!(&value[b], C::Ident(s) if s.eq_ignore_ascii_case("important")) {
                    important = true;
                    value.truncate(a);
                    value = trim_ws(&value).to_vec();
                }
            }
        }
        if value.is_empty() {
            return None;
        }
        let first_ident = value.iter().find_map(|c| match c {
            C::Ident(s) => Some(s.to_ascii_lowercase()),
            _ => None,
        });
        match name.as_str() {
            "position" => {
                let v = first_ident?;
                if !matches!(v.as_str(), "static" | "relative" | "absolute") || value.len() != 1 {
                    return None;
                }
            }
            "z-index" => match value.as_slice() {
                [C::Ident(s)] if s.eq_ignore_ascii_case("auto") => {}
                [C::Raw(n)] => {
                    let v: i64 = n.parse().ok()?;
                    if v.abs() > MAX_Z_INDEX {
                        return None;
                    }
                }
                _ => return None,
            },
            "writing-mode" | "-webkit-writing-mode" | "-epub-writing-mode" => {
                if let Some(v) = &first_ident {
                    if v.starts_with("vertical") || v.starts_with("tb") || v.starts_with("sideways") {
                        self.report.vertical = true;
                    }
                }
            }
            "direction" if first_ident.as_deref() == Some("rtl") => self.report.rtl = true,
            _ => {}
        }
        let url_use = match name.as_str() {
            "src" => Some(RefUse::CssFont),
            "background" | "background-image" | "list-style" | "list-style-image" | "content" => {
                Some(RefUse::CssImage)
            }
            _ => None,
        };
        if name == "src" {
            // Keep only the comma-separated entries whose url is internal.
            let mut kept: Vec<Vec<C>> = Vec::new();
            for part in value.split(|c| *c == C::Comma) {
                let mut part = trim_ws(part).to_vec();
                if self.clean_value(&mut part, url_use) && !part.is_empty() {
                    kept.push(part);
                }
            }
            if kept.is_empty() {
                return None;
            }
            value = Vec::new();
            for (i, p) in kept.into_iter().enumerate() {
                if i > 0 {
                    value.push(C::Comma);
                }
                value.extend(p);
            }
        } else if !self.clean_value(&mut value, url_use) {
            return None;
        }
        let mut s = String::new();
        let _ = serialize_identifier(&name, &mut s);
        s.push(':');
        ser(&value, &mut s);
        if important {
            s.push_str(" !important");
        }
        Some(s)
    }

    fn declarations(&mut self, body: &[C], in_font_face: bool) -> Vec<String> {
        body.split(|c| *c == C::Semi)
            .filter_map(|d| self.declaration(d, in_font_face))
            .collect()
    }

    fn selector_ok(list: &[C]) -> bool {
        list.iter().all(|c| match c {
            C::Bad | C::Url(_) | C::Curly(_) | C::Str(_) | C::AtKw(_) | C::Semi => false,
            C::Func(n, inner) => ALLOWED_PSEUDO_FUNCTIONS.contains(&n.as_str()) && Self::selector_ok(inner),
            C::Paren(inner) => Self::selector_ok(inner),
            C::Square(inner) => inner.iter().all(|c| !matches!(c, C::Bad | C::Url(_) | C::Curly(_))),
            _ => true,
        })
    }

    fn media_prelude_ok(list: &[C]) -> bool {
        list.iter().all(|c| match c {
            C::Ws | C::Ident(_) | C::Colon | C::Comma | C::Raw(_) | C::Delim(_) => true,
            C::Paren(inner) => Self::media_prelude_ok(inner),
            _ => false,
        })
    }

    fn rules(&mut self, list: &[C], depth: usize, out: &mut String) {
        let mut i = 0;
        while i < list.len() {
            match &list[i] {
                C::Ws | C::Semi | C::Bad => {
                    i += 1;
                }
                C::AtKw(name) => {
                    let name = name.to_ascii_lowercase();
                    let start = i + 1;
                    let mut j = start;
                    while j < list.len() && !matches!(list[j], C::Semi | C::Curly(_)) {
                        j += 1;
                    }
                    let prelude = trim_ws(&list[start..j.min(list.len())]);
                    let block = match list.get(j) {
                        Some(C::Curly(b)) => Some(b.as_slice()),
                        _ => None,
                    };
                    i = j + 1;
                    match (name.as_str(), block) {
                        ("import", _) => self.report.import_removed = true,
                        ("font-face", Some(b)) => {
                            let decls = self.declarations(b, true);
                            if decls.iter().any(|d| d.starts_with("src:")) {
                                out.push_str("@font-face{");
                                out.push_str(&decls.join(";"));
                                out.push_str("}\n");
                            }
                        }
                        ("media" | "supports", Some(b)) if depth < 4 && Self::media_prelude_ok(prelude) => {
                            let mut inner = String::new();
                            self.rules(b, depth + 1, &mut inner);
                            if !inner.is_empty() {
                                out.push('@');
                                out.push_str(&name);
                                out.push(' ');
                                ser(prelude, out);
                                out.push('{');
                                out.push_str(inner.trim_end());
                                out.push_str("}\n");
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    let start = i;
                    let mut j = i;
                    while j < list.len() && !matches!(list[j], C::Curly(_)) {
                        j += 1;
                    }
                    let Some(C::Curly(body)) = list.get(j) else { break };
                    let prelude = trim_ws(&list[start..j]);
                    i = j + 1;
                    if prelude.is_empty() || !Self::selector_ok(prelude) {
                        continue;
                    }
                    let decls = self.declarations(body, false);
                    if decls.is_empty() {
                        continue;
                    }
                    ser(prelude, out);
                    out.push('{');
                    out.push_str(&decls.join(";"));
                    out.push_str("}\n");
                }
            }
        }
    }
}

/// Sanitize a whole stylesheet. `None` means it was rejected entirely (too large,
/// too deeply nested, too many tokens).
pub(crate) fn sanitize_stylesheet(src: &str, policy: &dyn UrlPolicy) -> (Option<String>, CssReport) {
    let mut ctx = Ctx { policy, report: CssReport::default() };
    if src.len() > MAX_STYLESHEET_BYTES {
        ctx.report.rejected = true;
        return (None, ctx.report);
    }
    let Some(list) = parse_components(src) else {
        ctx.report.rejected = true;
        return (None, ctx.report);
    };
    let mut out = String::new();
    ctx.rules(&list, 0, &mut out);
    (Some(out), ctx.report)
}

/// Sanitize a `style` attribute (a declaration list). Empty result → drop attribute.
pub(crate) fn sanitize_style_attr(src: &str, policy: &dyn UrlPolicy) -> (Option<String>, CssReport) {
    let mut ctx = Ctx { policy, report: CssReport::default() };
    if src.len() > MAX_STYLE_ATTR_BYTES {
        ctx.report.rejected = true;
        return (None, ctx.report);
    }
    let Some(list) = parse_components(src) else {
        ctx.report.rejected = true;
        return (None, ctx.report);
    };
    let decls = ctx.declarations(&list, false);
    if decls.is_empty() {
        (None, ctx.report)
    } else {
        (Some(decls.join(";")), ctx.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epub_security::refs::{LinkMap, PackagePolicy, ResKind, Target};

    fn map() -> LinkMap {
        let mut m = LinkMap::default();
        m.by_path.insert(
            "OEBPS/img/a.png".into(),
            Target::Resource { id: "0123456789abcdef01234567".into(), ext: "png".into(), kind: ResKind::Image },
        );
        m.by_path.insert(
            "OEBPS/fonts/f.woff".into(),
            Target::Resource { id: "abcdef0123456789abcdef01".into(), ext: "woff".into(), kind: ResKind::Font },
        );
        m
    }

    #[test]
    fn blocks_dangerous_css() {
        let m = map();
        let pol = PackagePolicy { base_dir: "OEBPS/css/".into(), from_spine: None, map: &m };
        let src = r#"
@import url("http://evil.com/x.css");
@import "local.css";
body { color: red; position: fixed; z-index: 99999; behavior: url(x.htc); -moz-binding: url(x.xml#y) }
p { background: url(../img/a.png) no-repeat; background-image: url(https://track.example/p.gif) }
.x { width: expression(alert(1)); color: blue !important }
@font-face { font-family: F; src: url(https://fonts.example/f.woff), url(../fonts/f.woff) format("woff"), local(Arial) }
@font-face { font-family: G; src: url(http://evil/g.woff) }
div { position: sticky } span { position: relative }
.v { writing-mode: vertical-rl }
@media screen and (min-width: 10px) { h1 { color: green } }
@-moz-document url-prefix() { a { color: red } }
"#;
        let (out, rep) = sanitize_stylesheet(src, &pol);
        let out = out.unwrap();
        assert!(!out.contains("@import"), "{out}");
        assert!(!out.contains("http"), "{out}");
        assert!(!out.contains("fixed"));
        assert!(!out.contains("99999"));
        assert!(!out.contains("behavior") && !out.contains("binding") && !out.contains("expression"));
        assert!(!out.contains("sticky"));
        assert!(out.contains("body{color:red}"), "{out}");
        assert!(out.contains("url(\"0123456789abcdef01234567.png\") no-repeat"), "{out}");
        assert!(out.contains("color:blue !important"));
        assert!(out.contains("src:url(\"abcdef0123456789abcdef01.woff\") format(\"woff\"),local(Arial)"), "{out}");
        assert!(!out.contains("font-family:G"));
        assert!(out.contains("span{position:relative}"));
        assert!(out.contains("@media screen and (min-width: 10px){h1{color:green}}"), "{out}");
        assert!(!out.contains("moz-document"));
        assert!(rep.vertical && rep.import_removed && rep.remote_removed);
        // Idempotent on its own output (canonical fixpoint relies on this).
        let (again, _) = sanitize_stylesheet(&out, &crate::epub_security::refs::CanonicalPolicy { from_spine: None });
        assert_eq!(again.unwrap(), out);
    }

    #[test]
    fn limits_depth_and_size() {
        let m = map();
        let pol = PackagePolicy { base_dir: "".into(), from_spine: None, map: &m };
        let deep = format!("a{{b:{}1{}}}", "(".repeat(100), ")".repeat(100));
        let (out, rep) = sanitize_stylesheet(&deep, &pol);
        assert!(out.is_none() && rep.rejected);
        let big = "a{color:red}".repeat(MAX_STYLESHEET_BYTES / 10);
        assert!(sanitize_stylesheet(&big, &pol).0.is_none());
        let (attr, _) = sanitize_style_attr("color: red; background: url(javascript:alert(1)); top: 0", &pol);
        assert_eq!(attr.as_deref(), Some("color:red;top:0"));
    }
}
