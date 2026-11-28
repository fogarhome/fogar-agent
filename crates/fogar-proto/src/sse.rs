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
//! `Last-Event-ID` itself. An `id` containing NUL is ignored (as in the spec), and so is one
//! longer than [`MAX_ID_BYTES`] (the gateway's ids are `u64`s).
//!
//! Bounded: line, data and id share one budget of [`MAX_EVENT_BYTES`] (capacity, not just length).
//! A `data` line that outgrows it drops its whole event: reported once as [`SseItem::Oversize`]
//! when that line ends (only then is its field name surely known, whatever the chunking), and
//! its bytes are dropped up to its blank line without being buffered. Any other line that
//! outgrows it (a comment, an unknown field) is just dropped. Data that is not UTF-8 is reported
//! as [`SseItem::Oversize`] too (never decoded lossily).

use alloc::vec::Vec;

/// Longest `id` kept: `u64::MAX` has 20 digits.
pub const MAX_ID_BYTES: usize = 20;

/// Largest event (all its lines) the reader keeps. GUESSED: push `audio` events (base64 PCM
/// frames, `services/events.py`) are the largest the gateway sends; the doc gives no size.
pub const MAX_EVENT_BYTES: usize = 128 * 1024;

/// Longest line: a line shares its event's budget.
pub const MAX_LINE_BYTES: usize = MAX_EVENT_BYTES;

/// One thing found in the stream. Borrowed from the reader: valid only during the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseItem<'a> {
    /// A dispatched event (strict UTF-8 `data`).
    Event { id: Option<&'a str>, data: &'a str },
    /// A `:` line (the gateway's `: ping` keepalive).
    Comment,
    /// An event was dropped (reported once per event): its `data` outgrew [`MAX_EVENT_BYTES`]
    /// or was not UTF-8. `id`: the event's id if its `id:` line came before the drop.
    Oversize { id: Option<&'a str> },
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
    /// The current line outgrew the budget: only its first bytes are kept (`head`, `len`), and
    /// at its end they say whether it was `data` (the event is dropped) or not (the line is).
    overlong: Option<([u8; 8], usize)>,
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
            overlong: None,
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
            self.push(&rest[..i]);
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
        self.push(rest);
    }

    fn used(&self) -> usize {
        self.buf.len() + self.id.as_ref().map_or(0, Vec::len)
    }

    /// Appends part of the current line; a line that outgrows the budget keeps only its head
    /// (decided at its end).
    fn push(&mut self, part: &[u8]) {
        if part.is_empty() {
            return;
        }
        if self.skipping {
            self.skip_line_has_bytes = true;
            return;
        }
        if let Some((head, n)) = self.overlong.as_mut() {
            for &b in part {
                if *n == head.len() {
                    break;
                }
                head[*n] = b;
                *n += 1;
            }
            return;
        }
        let need = self.used() + part.len();
        if need > MAX_EVENT_BYTES {
            // Decided at the line's end, whatever the chunking (the field name may not be
            // here yet).
            let mut head = [0u8; 8];
            let mut n = 0;
            for &b in self.buf[self.data_len..]
                .iter()
                .chain(part)
                .take(head.len())
            {
                head[n] = b;
                n += 1;
            }
            self.buf.truncate(self.data_len);
            self.overlong = Some((head, n));
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
        if let Some((head, n)) = self.overlong.take() {
            let mut head = &head[..n];
            if self.at_start {
                self.at_start = false;
                head = head.strip_prefix(BOM).unwrap_or(head);
            }
            if head.starts_with(b"data:") || head == b"data" {
                // The rest of this event is skipped up to its blank line.
                let id = self.id.take();
                self.reset();
                self.skipping = true;
                self.skip_line_has_bytes = false;
                out(SseItem::Oversize {
                    id: id.as_deref().and_then(|v| core::str::from_utf8(v).ok()),
                });
            }
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
                let id = self
                    .id
                    .as_deref()
                    .and_then(|v| core::str::from_utf8(v).ok());
                match core::str::from_utf8(&self.buf[..self.data_len - 1]) {
                    Ok(data) => out(SseItem::Event { id, data }),
                    Err(_) => out(SseItem::Oversize { id }),
                }
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
                if !v.contains(&0) && v.len() <= MAX_ID_BYTES {
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

    use alloc::{
        format,
        string::{String, ToString},
    };

    /// Every item, owned: `(id, data)`, data `None` for a dropped event.
    fn items(input: &[u8]) -> Vec<(Option<String>, Option<String>)> {
        let mut out = Vec::new();
        SseReader::new().feed(input, &mut |it| match it {
            SseItem::Event { id, data } => {
                out.push((id.map(ToString::to_string), Some(data.to_string())));
            }
            SseItem::Oversize { id } => out.push((id.map(ToString::to_string), None)),
            SseItem::Comment => {}
        });
        out
    }

    /// Review L1: invalid UTF-8 is never decoded lossily; an id over 20 bytes is ignored.
    #[test]
    fn strict_utf8_and_short_ids() {
        assert_eq!(
            items(b"id: 7\ndata: \xff\xfe\n\ndata: ok\n\n"),
            [(Some("7".into()), None), (None, Some("ok".into()))]
        );
        let long = format!("id: {}\ndata: x\n\n", "1".repeat(21));
        assert_eq!(items(long.as_bytes()), [(None, Some("x".into()))]);
        let max = format!("id: {}\ndata: x\n\n", u64::MAX);
        assert_eq!(
            items(max.as_bytes()),
            [(Some(u64::MAX.to_string()), Some("x".into()))]
        );
    }

    /// Review L3/L2: only an overflowing `data` line drops its event (reported with the id
    /// seen before it); an overlong comment or unknown field is just dropped.
    #[test]
    fn only_an_overflowing_data_line_drops_the_event() {
        let big = "y".repeat(MAX_EVENT_BYTES);
        let comment = format!("id: 3\n: {big}\ndata: kept\n\n");
        assert_eq!(
            items(comment.as_bytes()),
            [(Some("3".into()), Some("kept".into()))]
        );
        let field = format!("id: 4\nnoise: {big}\ndata: kept\n\n");
        assert_eq!(
            items(field.as_bytes()),
            [(Some("4".into()), Some("kept".into()))]
        );
        let data = format!("id: 5\ndata: {big}\ndata: more\n\ndata: next\n\n");
        assert_eq!(
            items(data.as_bytes()),
            [(Some("5".into()), None), (None, Some("next".into()))]
        );
    }

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
        r.feed(b"\n", &mut |_| items += 1);
        assert_eq!(items, 1, "reported once, at the line's end");
        // Many short lines too: data plus the current line stay within the budget.
        let mut r = SseReader::new();
        for _ in 0..100_000 {
            r.feed(b"data: abcdef\n", &mut |_| {});
            assert!(r.buf.capacity() <= MAX_EVENT_BYTES);
        }
    }
}
