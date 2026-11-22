//! A Server-Sent Events parser that is indifferent to how the bytes are chunked.

/// One dispatched event: its `event:` name (if any) and its joined `data:` lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental parser. Lines may end in `\n`, `\r\n` or `\r`; a line is only decoded once it is
/// complete, so multi-byte characters split across chunks come out whole.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    /// The previous byte was `\r`, so a following `\n` belongs to the same line ending.
    after_cr: bool,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &b in chunk {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut out),
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut out);
                }
                _ => self.line.push(b),
            }
        }
        out
    }

    /// End of stream: a trailing line or event without its blank line is still delivered.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        let line = String::from_utf8_lossy(&line);
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.has_data {
            out.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data),
            });
        }
        self.event = None;
        self.has_data = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(input: &[u8], step: usize) -> Vec<SseEvent> {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        for c in input.chunks(step) {
            out.extend(p.feed(c));
        }
        out.extend(p.finish());
        out
    }

    #[test]
    fn sse_basic() {
        let evs = feed_all(b"data: a\n\n: comment\nevent: x\ndata: b\ndata:c\n\n", 1024);
        assert_eq!(
            evs,
            vec![
                SseEvent {
                    event: None,
                    data: "a".into()
                },
                SseEvent {
                    event: Some("x".into()),
                    data: "b\nc".into()
                },
            ]
        );
    }

    #[test]
    fn sse_chunking_is_invisible() {
        for fixture in [
            include_str!("../../tests/fixtures/openai_text.sse"),
            include_str!("../../tests/fixtures/openai_reasoning.sse"),
            include_str!("../../tests/fixtures/openai_tools.sse"),
        ] {
            let whole = feed_all(fixture.as_bytes(), usize::MAX);
            assert!(whole.len() >= 4);
            assert_eq!(feed_all(fixture.as_bytes(), 1), whole);
            assert_eq!(feed_all(fixture.as_bytes(), 7), whole);
            let crlf = fixture.replace('\n', "\r\n");
            assert_eq!(feed_all(crlf.as_bytes(), 1), whole);
            let cr = fixture.replace('\n', "\r");
            assert_eq!(feed_all(cr.as_bytes(), 3), whole);
        }
    }

    #[test]
    fn sse_dispatches_trailing_event_at_eof() {
        assert_eq!(
            feed_all(b"data: tail", 4),
            vec![SseEvent {
                event: None,
                data: "tail".into()
            }]
        );
    }
}
