//! UTF-16 <-> UTF-8 boundary conversion. IPC never exposes UTF-8 byte offsets.

use unicode_segmentation::UnicodeSegmentation;

pub fn len_u16(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// UTF-8 byte offset -> UTF-16 offset. `byte` must be a char boundary.
pub fn byte_to_u16(s: &str, byte: usize) -> Option<u32> {
    if byte > s.len() || !s.is_char_boundary(byte) {
        return None;
    }
    Some(s[..byte].encode_utf16().count() as u32)
}

/// UTF-16 offset -> UTF-8 byte offset. Rejects offsets inside a surrogate pair.
pub fn u16_to_byte(s: &str, u16_off: u32) -> Option<usize> {
    let mut acc = 0u32;
    for (b, ch) in s.char_indices() {
        if acc == u16_off {
            return Some(b);
        }
        acc += ch.len_utf16() as u32;
        if acc > u16_off {
            return None; // inside surrogate pair
        }
    }
    if acc == u16_off {
        Some(s.len())
    } else {
        None
    }
}

/// A range is valid when both ends sit on extended grapheme cluster boundaries,
/// so surrogate pairs, combining marks and emoji sequences are never split.
pub fn is_valid_range_u16(s: &str, start: u32, end: u32) -> bool {
    if start > end {
        return false;
    }
    let (Some(bs), Some(be)) = (u16_to_byte(s, start), u16_to_byte(s, end)) else {
        return false;
    };
    is_grapheme_boundary(s, bs) && is_grapheme_boundary(s, be)
}

fn is_grapheme_boundary(s: &str, byte: usize) -> bool {
    if byte == 0 || byte == s.len() {
        return true;
    }
    s.grapheme_indices(true).any(|(i, _)| i == byte)
}

pub fn slice_u16(s: &str, start: u32, end: u32) -> Option<&str> {
    let bs = u16_to_byte(s, start)?;
    let be = u16_to_byte(s, end)?;
    s.get(bs..be)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogate_and_combining_boundaries() {
        let s = "a😀e\u{301}b"; // a, U+1F600 (2 u16), e+combining, b
        assert_eq!(len_u16(s), 6);
        assert_eq!(u16_to_byte(s, 2), None); // middle of surrogate pair
        assert!(is_valid_range_u16(s, 0, 3));
        assert!(!is_valid_range_u16(s, 0, 4)); // splits e + combining acute
        assert!(is_valid_range_u16(s, 3, 5));
        assert_eq!(slice_u16(s, 1, 3), Some("😀"));
        assert_eq!(byte_to_u16(s, 5), Some(3));
    }

    #[test]
    fn zwj_emoji_not_split() {
        let s = "x👩\u{200d}💻y";
        // 👩 = 2, ZWJ = 1, 💻 = 2 -> cluster occupies u16 1..6
        assert!(is_valid_range_u16(s, 1, 6));
        assert!(!is_valid_range_u16(s, 1, 3));
    }
}
