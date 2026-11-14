//! Incremental Server-Sent Events parser (WHATWG HTML §9.2.6).
//!
//! Network chunk boundaries are irrelevant: bytes are buffered and only complete lines are
//! decoded, so UTF-8 sequences split across chunks decode correctly. Handles CRLF / LF / CR
//! line endings (including a CR at the end of one chunk followed by LF in the next),
//! multi-line `data:` fields, comments and a leading BOM. Event size is bounded.

use super::MAX_RECORD_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` field, if any.
    pub event: Option<String>,
    /// `data:` lines joined with '\n'.
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamParseError {
    #[error("record too large")]
    RecordTooLarge,
    #[error("invalid utf-8 in stream")]
    InvalidUtf8,
}

#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    event: Option<String>,
    skip_lf: bool,
    started: bool,
    max_record: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new(MAX_RECORD_BYTES)
    }
}

impl SseParser {
    pub fn new(max_record: usize) -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            has_data: false,
            event: None,
            skip_lf: false,
            started: false,
            max_record,
        }
    }

    /// Feed a network chunk; returns every event completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, StreamParseError> {
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut i = 0usize;
        while i < chunk.len() {
            let b = chunk[i];
            if self.skip_lf {
                self.skip_lf = false;
                if b == b'\n' {
                    i += 1;
                    start = i;
                    continue;
                }
            }
            if b == b'\n' || b == b'\r' {
                self.line.extend_from_slice(&chunk[start..i]);
                self.check_size()?;
                let line = std::mem::take(&mut self.line);
                self.process_line(&line, &mut out)?;
                if b == b'\r' {
                    self.skip_lf = true;
                }
                i += 1;
                start = i;
                continue;
            }
            i += 1;
        }
        self.line.extend_from_slice(&chunk[start..]);
        self.check_size()?;
        Ok(out)
    }

    /// End of stream. Leniently dispatches a final event that lacked its terminating blank
    /// line (some servers end with `data: [DONE]\n`). Callers still require a normal
    /// completion frame, so a truncated record cannot turn into a success on its own.
    pub fn finish(&mut self) -> Result<Vec<SseEvent>, StreamParseError> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, &mut out)?;
        }
        self.process_line(b"", &mut out)?;
        Ok(out)
    }

    fn check_size(&self) -> Result<(), StreamParseError> {
        if self.line.len() + self.data.len() > self.max_record {
            return Err(StreamParseError::RecordTooLarge);
        }
        Ok(())
    }

    fn process_line(&mut self, raw: &[u8], out: &mut Vec<SseEvent>) -> Result<(), StreamParseError> {
        let mut line = std::str::from_utf8(raw).map_err(|_| StreamParseError::InvalidUtf8)?;
        if !self.started {
            self.started = true;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        if line.is_empty() {
            // dispatch
            if self.has_data {
                out.push(SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data) });
            } else {
                self.event = None;
            }
            self.has_data = false;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(()); // comment / keep-alive
        }
        let (field, value) = match line.find(':') {
            Some(p) => {
                let v = &line[p + 1..];
                (&line[..p], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
                if self.data.len() > self.max_record {
                    return Err(StreamParseError::RecordTooLarge);
                }
            }
            "event" => self.event = Some(value.to_string()),
            _ => {} // id / retry / unknown fields are ignored
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Result<Vec<SseEvent>, StreamParseError> {
        let mut p = SseParser::default();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.push(c)?);
        }
        Ok(out)
    }

    fn data(evts: &[SseEvent]) -> Vec<&str> {
        evts.iter().map(|e| e.data.as_str()).collect()
    }

    #[test]
    fn basic_lf_crlf_cr() {
        for sep in ["\n", "\r\n", "\r"] {
            let s = format!("data: a{sep}{sep}data: b{sep}{sep}");
            let e = parse_all(&[s.as_bytes()]).unwrap();
            assert_eq!(data(&e), vec!["a", "b"], "sep={sep:?}");
        }
    }

    #[test]
    fn every_split_point_gives_same_result() {
        let s = "\u{feff}: comment\r\ndata: {\"t\":\"한글😀\"}\r\n\r\nevent: x\ndata: line1\ndata:line2\n\ndata: [DONE]\r\n\r\n";
        let bytes = s.as_bytes();
        let whole = parse_all(&[bytes]).unwrap();
        assert_eq!(data(&whole), vec!["{\"t\":\"한글😀\"}", "line1\nline2", "[DONE]"]);
        assert_eq!(whole[1].event.as_deref(), Some("x"));
        for i in 0..=bytes.len() {
            for j in i..=bytes.len() {
                let got = parse_all(&[&bytes[..i], &bytes[i..j], &bytes[j..]]).unwrap();
                assert_eq!(got, whole, "split at {i},{j}");
            }
        }
    }

    #[test]
    fn cr_at_chunk_end_then_lf() {
        let e = parse_all(&[b"data: a\r", b"\n\r", b"\n"]).unwrap();
        assert_eq!(data(&e), vec!["a"]);
    }

    #[test]
    fn empty_data_and_no_space() {
        let e = parse_all(&[b"data:\n\ndata:x\n\n"]).unwrap();
        assert_eq!(data(&e), vec!["", "x"]);
        // event without data is not dispatched
        let e = parse_all(&[b"event: ping\n\n"]).unwrap();
        assert!(e.is_empty());
    }

    #[test]
    fn record_limit() {
        let mut p = SseParser::new(16);
        assert_eq!(p.push(b"data: 0123456789abcdefghij"), Err(StreamParseError::RecordTooLarge));
        let mut p = SseParser::new(16);
        p.push(b"data: 0123456789\n").unwrap();
        assert_eq!(p.push(b"data: 0123456789\n"), Err(StreamParseError::RecordTooLarge));
    }

    #[test]
    fn invalid_utf8_and_unterminated() {
        let mut p = SseParser::default();
        assert_eq!(p.push(b"data: \xff\n"), Err(StreamParseError::InvalidUtf8));
        let mut p = SseParser::default();
        assert!(p.push(b"data: partial").unwrap().is_empty());
        assert_eq!(p.finish().unwrap(), vec![SseEvent { event: None, data: "partial".into() }]);
        let mut p = SseParser::default();
        p.push(b"data: [DONE]\n").unwrap();
        assert_eq!(p.finish().unwrap()[0].data, "[DONE]");
        let mut p = SseParser::default();
        p.push(b"data: x\n\n").unwrap();
        assert!(p.finish().unwrap().is_empty());
    }
}
