//! Reference resolution policy: maps an author URL to the flat safe layout
//! (`x/<spineIndex>.xhtml`, `r/<resourceId>.<ext>`) or drops it.

use std::collections::HashMap;

use super::paths::{is_safe_id, resolve_decoded, scheme_of, split_href, validate_external};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefUse {
    /// `<a href>`: spine documents (+fragment) or external http(s)
    Link,
    /// `<img src>`, SVG `<image href>`
    Image,
    /// `<link rel=stylesheet href>`
    Stylesheet,
    /// CSS `url()` outside `@font-face src`
    CssImage,
    /// `@font-face { src: url() }`
    CssFont,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DropReason {
    Remote,
    Scheme,
    Missing(String),
    Outside,
    WrongKind,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Rewritten reference, relative to the output location.
    Internal(String),
    /// Validated external http(s) link (only for [`RefUse::Link`]).
    External(String),
    Drop(DropReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResKind {
    Image,
    Font,
    Css,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Spine(u32),
    Resource { id: String, ext: String, kind: ResKind },
}

#[derive(Debug, Default)]
pub(crate) struct LinkMap {
    pub by_path: HashMap<String, Target>,
}

pub(crate) trait UrlPolicy {
    fn resolve(&self, raw: &str, use_: RefUse) -> Resolved;
}

fn res_prefix(from_spine: Option<u32>) -> &'static str {
    if from_spine.is_some() {
        "../r/"
    } else {
        ""
    }
}

fn spine_link(j: u32, frag: Option<&str>) -> String {
    match frag {
        Some(f) if is_safe_id(f) => format!("{j}.xhtml#{f}"),
        _ => format!("{j}.xhtml"),
    }
}

fn classify_scheme(raw: &str, use_: RefUse) -> Option<Resolved> {
    let t = raw.trim_matches(|c: char| c.is_ascii_whitespace() || c.is_control());
    if t.is_empty() {
        return Some(Resolved::Drop(DropReason::Invalid));
    }
    if let Some(s) = scheme_of(t) {
        return Some(if s == "http" || s == "https" {
            if use_ == RefUse::Link {
                match validate_external(t) {
                    Some(u) => Resolved::External(u),
                    None => Resolved::Drop(DropReason::Invalid),
                }
            } else {
                Resolved::Drop(DropReason::Remote)
            }
        } else {
            Resolved::Drop(DropReason::Scheme)
        });
    }
    if t.starts_with("//") || t.starts_with("\\\\") || t.starts_with("/\\") {
        return Some(Resolved::Drop(DropReason::Remote));
    }
    None
}

/// Resolves author references against the original package layout.
pub(crate) struct PackagePolicy<'a> {
    pub base_dir: String,
    pub from_spine: Option<u32>,
    pub map: &'a LinkMap,
}

impl UrlPolicy for PackagePolicy<'_> {
    fn resolve(&self, raw: &str, use_: RefUse) -> Resolved {
        if let Some(r) = classify_scheme(raw, use_) {
            return r;
        }
        let (path, frag) = split_href(raw);
        if path.is_empty() {
            return match (use_, self.from_spine, frag.as_deref()) {
                (RefUse::Link, Some(i), Some(f)) if is_safe_id(f) => Resolved::Internal(spine_link(i, Some(f))),
                _ => Resolved::Drop(DropReason::Invalid),
            };
        }
        let Some(full) = resolve_decoded(&self.base_dir, &path) else {
            return Resolved::Drop(DropReason::Outside);
        };
        let Some(target) = self.map.by_path.get(&full) else {
            return Resolved::Drop(DropReason::Missing(full));
        };
        match (use_, target) {
            (RefUse::Link, Target::Spine(j)) if self.from_spine.is_some() => {
                Resolved::Internal(spine_link(*j, frag.as_deref()))
            }
            (RefUse::Image | RefUse::CssImage, Target::Resource { id, ext, kind: ResKind::Image })
            | (RefUse::CssFont, Target::Resource { id, ext, kind: ResKind::Font })
            | (RefUse::Stylesheet, Target::Resource { id, ext, kind: ResKind::Css }) => {
                Resolved::Internal(format!("{}{id}.{ext}", res_prefix(self.from_spine)))
            }
            _ => Resolved::Drop(DropReason::WrongKind),
        }
    }
}

/// Accepts only references that are already in canonical form (used when the
/// canonical output is re-parsed to verify the serialization is a fixpoint).
pub(crate) struct CanonicalPolicy {
    pub from_spine: Option<u32>,
}

const IMAGE_EXTS: &[&str] = &["png", "jpg", "gif", "webp", "svg"];
const FONT_EXTS: &[&str] = &["ttf", "otf", "woff", "woff2"];

impl UrlPolicy for CanonicalPolicy {
    fn resolve(&self, raw: &str, use_: RefUse) -> Resolved {
        if let Some(r) = classify_scheme(raw, use_) {
            return r;
        }
        if use_ == RefUse::Link {
            let (doc, frag) = match raw.split_once('#') {
                Some((d, f)) => (d, Some(f)),
                None => (raw, None),
            };
            let Some(num) = doc.strip_suffix(".xhtml") else {
                return Resolved::Drop(DropReason::Invalid);
            };
            if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) || num.len() > 9 {
                return Resolved::Drop(DropReason::Invalid);
            }
            if let Some(f) = frag {
                if !is_safe_id(f) {
                    return Resolved::Drop(DropReason::Invalid);
                }
            }
            return Resolved::Internal(raw.to_string());
        }
        let Some(rest) = raw.strip_prefix(res_prefix(self.from_spine)) else {
            return Resolved::Drop(DropReason::Invalid);
        };
        let Some((id, ext)) = rest.split_once('.') else {
            return Resolved::Drop(DropReason::Invalid);
        };
        if id.len() != 24 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Resolved::Drop(DropReason::Invalid);
        }
        let ok = match use_ {
            RefUse::Image | RefUse::CssImage => IMAGE_EXTS.contains(&ext),
            RefUse::CssFont => FONT_EXTS.contains(&ext),
            RefUse::Stylesheet => ext == "css",
            RefUse::Link => false,
        };
        if ok {
            Resolved::Internal(raw.to_string())
        } else {
            Resolved::Drop(DropReason::WrongKind)
        }
    }
}
