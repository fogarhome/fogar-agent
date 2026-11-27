//! Server-sent events reader for `GET /events` (gateway 5.0, `client-protocol.md` §16), sans-I/O:
//! feed it the body's bytes as they arrive, it calls back once per complete event.
//!
//! Follows the WHATWG «event stream interpretation» for what the gateway sends: LF, CR and CRLF
//! line ends (also a CR ending one chunk and its LF starting the next), a BOM only at the very
//! start, `field: value` with one leading space dropped, several `data:` lines joined with `\n`,
//! comments (`: ping`), and `event:`, `retry:` and unknown fields ignored. An event with no `data`
//! is not dispatched, nor is one cut by the end of the stream.
//!
//! Deviation: `id` is the event's OWN `id:` line, not the browser's sticky `lastEventId`. The
//! gateway sends `ready` without an id on resume and push `audio` without one between numbered
//! events; neither must look like it carries the previous event's id. The caller keeps
//! `Last-Event-ID` itself. An `id` containing NUL is ignored (as in the spec).
//!
//! Bounded: line, data and id share one budget of [`MAX_EVENT_BYTES`] (capacity, not just length).
//! A bigger event is reported once as [`SseItem::Oversize`] and its bytes are dropped up to its
//! blank line without being buffered.

use alloc::{string::String, vec::Vec};

/// Largest event (all its lines) the reader keeps. GUESSED: push `audio` events (base64 PCM
/// frames, `services/events.py`) are the largest the gateway sends; the doc gives no size.
pub const MAX_EVENT_BYTES: usize = 128 * 1024;

/// Longest line: a line shares its event's budget.
pub const MAX_LINE_BYTES: usize = MAX_EVENT_BYTES;

/// One thing found in the stream. Borrowed from the reader: valid only during the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseItem<'a> {
    /// A dispatched event. `data` is lossy UTF-8 (invalid sequences become U+FFFD, as browsers do).
    Event { id: Option<&'a str>, data: &'a str },
    /// A `:` line (the gateway's `: ping` keepalive).
    Comment,
    /// An event over [`MAX_EVENT_BYTES`] was dropped (reported once per event).
    Oversize,
}

/// Incremental SSE parser. One per connection: a new stream starts with a new reader, so no
/// partial line or event carries across reconnects.
pub struct SseReader {
    /// `buf[..data_len]` is the event's data so far (each line followed by `\n`); the rest is the
    /// current, unfinished line.
    buf: Vec<u8>,
    data_len: usize,
    id: Option<Vec<u8>>,
    /// The last byte seen was a CR: an LF right after it is part of the same line end.
    after_cr: bool,
    /// Still before the first line end: a leading BOM is dropped there.
    at_start: bool,
    /// Dropping an oversized event up to its blank line.
    skipping: bool,
    /// While skipping: the current line has content (so its end is not the blank line).
    skip_line_has_bytes: bool,
}

impl core::fmt::Debug for SseReader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Sizes only: the buffer holds user text.
        f.debug_struct("SseReader")
            .field("buffered", &self.buf.len())
            .field("skipping", &self.skipping)
            .finish()
    }
}

impl Default for SseReader {
    fn default() -> Self {
        Self::new()
    }
}

const BOM: &[u8] = b"\xEF\xBB\xBF";

impl SseReader {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            data_len: 0,
            id: None,
            after_cr: false,
            at_start: true,
            skipping: false,
            skip_line_has_bytes: false,
        }
    }

    /// Feeds the next bytes of the body; `out` is called for every complete item, in order.
    pub fn feed(&mut self, bytes: &[u8], out: &mut impl FnMut(SseItem<'_>)) {
        let mut rest = bytes;
        if self.after_cr && !rest.is_empty() {
            self.after_cr = false;
            if rest[0] == b'\n' {
                rest = &rest[1..];
            }
        }
        while let Some(i) = rest.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.push(&rest[..i], out);
            let cr = rest[i] == b'\r';
            rest = &rest[i + 1..];
            if cr {
                match rest.first() {
                    Some(b'\n') => rest = &rest[1..],
                    Some(_) => {}
                    None => self.after_cr = true,
                }
            }
            self.end_line(out);
        }
        self.push(rest, out);
    }

    fn used(&self) -> usize {
        self.buf.len() + self.id.as_ref().map_or(0, Vec::len)
    }

    /// Appends part of the current line, or starts skipping when the event outgrows the budget.
    fn push(&mut self, part: &[u8], out: &mut impl FnMut(SseItem<'_>)) {
        if part.is_empty() {
            return;
        }
        if self.skipping {
            self.skip_line_has_bytes = true;
            return;
        }
        let need = self.used() + part.len();
        if need > MAX_EVENT_BYTES {
            self.reset();
            self.skipping = true;
            self.skip_line_has_bytes = true;
            self.at_start = false;
            out(SseItem::Oversize);
            return;
        }
        if self.buf.len() + part.len() > self.buf.capacity() {
            // Grow like `Vec` would, but never past the budget.
            let target = (self.buf.capacity() * 2).clamp(need.max(64), MAX_EVENT_BYTES);
            self.buf.reserve_exact(target - self.buf.len());
        }
        self.buf.extend_from_slice(part);
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.data_len = 0;
        self.id = None;
    }

    fn end_line(&mut self, out: &mut impl FnMut(SseItem<'_>)) {
        if self.skipping {
            if !self.skip_line_has_bytes {
                self.skipping = false; // the blank line ending the oversized event
            }
            self.skip_line_has_bytes = false;
            return;
        }
        let mut start = self.data_len;
        if self.at_start {
            self.at_start = false;
            if self.buf[start..].starts_with(BOM) {
                start += BOM.len();
            }
        }
        let line = &self.buf[start..];
        if line.is_empty() {
            if self.data_len > 0 {
                let data = String::from_utf8_lossy(&self.buf[..self.data_len - 1]);
                let id = self.id.as_deref().map(String::from_utf8_lossy);
                out(SseItem::Event {
                    id: id.as_deref(),
                    data: &data,
                });
            }
            self.reset();
            return;
        }
        if line[0] == b':' {
            self.buf.truncate(self.data_len);
            out(SseItem::Comment);
            return;
        }
        let (name, value) = match line.iter().position(|&b| b == b':') {
            Some(c) => {
                let v = start + c + 1;
                let v = if self.buf.get(v) == Some(&b' ') {
                    v + 1
                } else {
                    v
                };
                (&self.buf[start..start + c], v)
            }
            None => (line, self.buf.len()),
        };
        match name {
            b"data" => {
                let len = self.buf.len() - value;
                self.buf.copy_within(value.., self.data_len);
                self.data_len += len;
                self.buf.truncate(self.data_len);
                // Fits: the line was at least `data` long, so the data shrank by >= 3 bytes.
                self.buf.push(b'\n');
                self.data_len += 1;
                return;
            }
            b"id" => {
                let v = &self.buf[value..];
                if !v.contains(&0) {
                    self.id = Some(v.to_vec());
                }
            }
            _ => {} // `event`, `retry` and unknown fields
        }
        self.buf.truncate(self.data_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_stays_within_the_budget() {
        let mut r = SseReader::new();
        let mut items = 0;
        let mut chunk = alloc::vec![b'x'; 1000];
        chunk[..6].copy_from_slice(b"data: ");
        for _ in 0..300 {
            r.feed(&chunk, &mut |_| items += 1);
            assert!(r.buf.capacity() <= MAX_EVENT_BYTES, "{}", r.buf.capacity());
        }
        assert_eq!(items, 1, "reported once");
        // Many short lines too: data plus the current line stay within the budget.
        let mut r = SseReader::new();
        for _ in 0..100_000 {
            r.feed(b"data: abcdef\n", &mut |_| {});
            assert!(r.buf.capacity() <= MAX_EVENT_BYTES);
        }
    }
}
