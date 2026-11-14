//! Incremental newline-delimited JSON record splitter.
//!
//! Records are assembled from arbitrary network chunks; only complete lines are decoded, so
//! UTF-8 sequences split across chunks are handled. Blank lines are skipped and a trailing
//! '\r' is removed. Record size is bounded.

use super::sse::StreamParseError;
use super::MAX_RECORD_BYTES;

#[derive(Debug)]
pub struct NdjsonParser {
    buf: Vec<u8>,
    max_record: usize,
}

impl Default for NdjsonParser {
    fn default() -> Self {
        Self::new(MAX_RECORD_BYTES)
    }
}

impl NdjsonParser {
    pub fn new(max_record: usize) -> Self {
        Self { buf: Vec::new(), max_record }
    }

    /// Feed a chunk; returns the complete (non-blank) records it finished.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, StreamParseError> {
        let mut out = Vec::new();
        let mut start = 0;
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'\n' {
                self.buf.extend_from_slice(&chunk[start..i]);
                self.check()?;
                let line = std::mem::take(&mut self.buf);
                if let Some(r) = decode(&line)? {
                    out.push(r);
                }
                start = i + 1;
            }
        }
        self.buf.extend_from_slice(&chunk[start..]);
        self.check()?;
        Ok(out)
    }

    /// End of stream: returns a final record that was not newline-terminated, if any.
    pub fn finish(&mut self) -> Result<Option<String>, StreamParseError> {
        let line = std::mem::take(&mut self.buf);
        decode(&line)
    }

    fn check(&self) -> Result<(), StreamParseError> {
        if self.buf.len() > self.max_record {
            Err(StreamParseError::RecordTooLarge)
        } else {
            Ok(())
        }
    }
}

fn decode(line: &[u8]) -> Result<Option<String>, StreamParseError> {
    let s = std::str::from_utf8(line).map_err(|_| StreamParseError::InvalidUtf8)?;
    let s = s.strip_suffix('\r').unwrap_or(s);
    if s.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_records_any_boundary() {
        let s = "{\"a\":\"가나😀\"}\n\r\n{\"b\":2}\r\n{\"c\":3}";
        let bytes = s.as_bytes();
        for i in 0..=bytes.len() {
            let mut p = NdjsonParser::default();
            let mut out = p.push(&bytes[..i]).unwrap();
            out.extend(p.push(&bytes[i..]).unwrap());
            out.extend(p.finish().unwrap());
            assert_eq!(out, vec!["{\"a\":\"가나😀\"}", "{\"b\":2}", "{\"c\":3}"], "split {i}");
        }
    }

    #[test]
    fn limits_and_utf8() {
        let mut p = NdjsonParser::new(8);
        assert_eq!(p.push(b"0123456789"), Err(StreamParseError::RecordTooLarge));
        let mut p = NdjsonParser::default();
        assert_eq!(p.push(b"\xc3\x28\n"), Err(StreamParseError::InvalidUtf8));
    }
}
