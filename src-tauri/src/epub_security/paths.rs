//! Path and URL rules: ZIP entry names, package-relative href resolution, URL schemes.

use percent_encoding::percent_decode_str;

/// Why a ZIP entry name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathViolation {
    Empty,
    Nul,
    Backslash,
    Absolute,
    DrivePrefix,
    DotSegment,
    ControlChar,
    TooLong,
}

impl PathViolation {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Empty => "path_empty",
            Self::Nul => "path_nul",
            Self::Backslash => "path_backslash",
            Self::Absolute => "path_absolute",
            Self::DrivePrefix => "path_drive_prefix",
            Self::DotSegment => "path_traversal",
            Self::ControlChar => "path_control_char",
            Self::TooLong => "path_too_long",
        }
    }
}

const MAX_PATH_LEN: usize = 1024;

fn check_raw(name: &str) -> Result<(), PathViolation> {
    if name.is_empty() {
        return Err(PathViolation::Empty);
    }
    if name.len() > MAX_PATH_LEN {
        return Err(PathViolation::TooLong);
    }
    if name.contains('\0') {
        return Err(PathViolation::Nul);
    }
    if name.contains('\\') {
        return Err(PathViolation::Backslash);
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(PathViolation::ControlChar);
    }
    if name.starts_with('/') {
        return Err(PathViolation::Absolute);
    }
    let b = name.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err(PathViolation::DrivePrefix);
    }
    for (i, seg) in name.split('/').enumerate() {
        if seg == ".." || seg == "." {
            return Err(PathViolation::DotSegment);
        }
        // A leading segment with ':' reads as a drive / scheme ("C:", "x:").
        if i == 0 && seg.contains(':') {
            return Err(PathViolation::DrivePrefix);
        }
    }
    Ok(())
}

/// Validate a ZIP entry name, both raw and percent-decoded. Returns the
/// normalized (percent-decoded, NFC, empty-segment-free) key used for duplicate
/// detection.
pub(crate) fn validate_entry_name(name: &str) -> Result<String, PathViolation> {
    check_raw(name)?;
    let decoded = percent_decode_str(name).decode_utf8_lossy().into_owned();
    check_raw(&decoded)?;
    Ok(dup_key(&decoded))
}

fn dup_key(decoded: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfc: String = decoded.nfc().collect();
    nfc.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("/")
}

/// Directory part (with trailing '/') of a package path, "" for root files.
pub(crate) fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "",
    }
}

/// Resolve an already percent-decoded relative path against `base_dir` (a
/// package directory with trailing '/' or ""). `None` when the result would escape
/// the package root or contains forbidden characters.
pub(crate) fn resolve_decoded(base_dir: &str, rel: &str) -> Option<String> {
    if rel.is_empty() || rel.len() > MAX_PATH_LEN {
        return None;
    }
    if rel.contains('\0') || rel.contains('\\') || rel.chars().any(|c| c.is_control()) {
        return None;
    }
    if rel.starts_with('/') {
        return None;
    }
    let mut segs: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop()?;
            }
            s => segs.push(s),
        }
    }
    if segs.first().is_some_and(|s| s.contains(':')) {
        return None;
    }
    if segs.is_empty() {
        return None;
    }
    Some(segs.join("/"))
}

/// Split an href into (path, fragment) after removing tabs/newlines (URL spec) and
/// any query. The path part is percent-decoded; the fragment is decoded too.
pub(crate) fn split_href(href: &str) -> (String, Option<String>) {
    let cleaned: String = href.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let cleaned = cleaned.trim_matches(|c: char| c.is_ascii_whitespace() || c.is_control());
    let (before_frag, frag) = match cleaned.find('#') {
        Some(i) => (&cleaned[..i], Some(&cleaned[i + 1..])),
        None => (cleaned, None),
    };
    let path = match before_frag.find('?') {
        Some(i) => &before_frag[..i],
        None => before_frag,
    };
    let path = percent_decode_str(path).decode_utf8_lossy().into_owned();
    let frag = frag.map(|f| percent_decode_str(f).decode_utf8_lossy().into_owned());
    (path, frag)
}

/// The URL's scheme (lowercase) if it has one, after URL-spec whitespace stripping.
pub(crate) fn scheme_of(href: &str) -> Option<String> {
    let cleaned: String = href
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect::<String>()
        .trim_matches(|c: char| c.is_ascii_whitespace() || c.is_control())
        .to_string();
    let mut chars = cleaned.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() => {}
        _ => return None,
    }
    for (i, c) in chars {
        if c == ':' {
            return Some(cleaned[..i].to_ascii_lowercase());
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return None;
        }
    }
    None
}

/// Validate an external http(s) URL. Returns the URL to keep (scheme lowercased).
pub(crate) fn validate_external(href: &str) -> Option<String> {
    let s = href.trim_matches(|c: char| c.is_ascii_whitespace());
    if s.len() > 2048 || s.is_empty() {
        return None;
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\' | '`')) {
        return None;
    }
    let scheme = scheme_of(s)?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = &s[scheme.len() + 1..];
    let rest = rest.strip_prefix("//")?;
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..host_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("{scheme}:{}", &s[scheme.len() + 1..]))
}

/// Element ids / fragments that are safe to embed in an epub.js CFI step assertion
/// and in `href="N.xhtml#frag"`: no CFI special characters (`^[](),;=!/:`), no spaces.
pub(crate) fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names() {
        assert!(validate_entry_name("OEBPS/a.xhtml").is_ok());
        assert_eq!(validate_entry_name("../x"), Err(PathViolation::DotSegment));
        assert_eq!(validate_entry_name("a/../../x"), Err(PathViolation::DotSegment));
        assert_eq!(validate_entry_name("/etc/passwd"), Err(PathViolation::Absolute));
        assert_eq!(validate_entry_name("C:/x"), Err(PathViolation::DrivePrefix));
        assert_eq!(validate_entry_name("a\\..\\x"), Err(PathViolation::Backslash));
        assert_eq!(validate_entry_name("%2e%2e/x"), Err(PathViolation::DotSegment));
        assert_eq!(validate_entry_name("a%5c..%5cx"), Err(PathViolation::Backslash));
        assert_eq!(validate_entry_name("a\0b"), Err(PathViolation::Nul));
        assert_eq!(validate_entry_name("a%00b"), Err(PathViolation::Nul));
        assert_eq!(validate_entry_name("a//b").unwrap(), "a/b");
    }

    #[test]
    fn href_resolution() {
        assert_eq!(resolve_decoded("OEBPS/text/", "../img/a.png").as_deref(), Some("OEBPS/img/a.png"));
        assert_eq!(resolve_decoded("OEBPS/", "../../x"), None);
        assert_eq!(resolve_decoded("", "../x"), None);
        assert_eq!(resolve_decoded("OEBPS/", "/abs"), None);
        let (p, f) = split_href("..%2F..%2Fsecret.xhtml#x%20y");
        assert_eq!(p, "../../secret.xhtml");
        assert_eq!(f.as_deref(), Some("x y"));
        assert_eq!(resolve_decoded("OEBPS/", &p), None);
    }

    #[test]
    fn schemes() {
        assert_eq!(scheme_of("java\tscript:alert(1)").as_deref(), Some("javascript"));
        assert_eq!(scheme_of(" JAVASCRIPT:x").as_deref(), Some("javascript"));
        assert_eq!(scheme_of("a/b:c"), None);
        assert_eq!(validate_external("HTTPS://example.com/x").as_deref(), Some("https://example.com/x"));
        assert_eq!(validate_external("https://user@evil.com/"), None);
        assert_eq!(validate_external("javascript:alert(1)"), None);
        assert_eq!(validate_external("https:///nohost"), None);
    }
}
