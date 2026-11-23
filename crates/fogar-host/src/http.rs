//! `GatewayHttp` over a tiny hand-written HTTP/1.1 client (fogar-secure-rust NET, port doc in
//! `fogar_ports::gateway_http`).
//!
//! Why not hyper/reqwest: the port needs a closed set of fixed requests, one per connection, no
//! redirects, no pooling, no compression, and a body read into ONE pre-sized wiped buffer.
//! hyper would add ~10 crates, its own buffering (copies of the `/pair` reply we cannot wipe)
//! and connection reuse we would have to switch off. A small client here is easier to review.
//! shortcut: over https the decrypted `/pair` reply (client_key) also passes through rustls'
//! internal plaintext buffers, which are not zeroized; only our own copies are wiped. Upgrade
//! if a rustls release offers zeroizing buffers.
//!
//! Bounds: response head <= `MAX_HEAD_BYTES`, body <= the call's `max_body()` (checked from
//! `Content-Length` before reading, and while reading when the length is unknown or `chunked`);
//! every await sits under `HttpConfig::call_timeout`, each connect attempt under
//! `connect_timeout`. The bearer token is written from its secret buffer straight to the socket
//! (never formatted into a `String`).
//!
//! Streams (`open_stream`): dial, request and response head under `call_timeout`; the
//! [`HttpStreamBody`] then has no deadline and no total cap (the core wraps each `chunk()`).
//! It owns the socket, the decoder state and one `MAX_CHUNK_BYTES` read buffer; `chunk()`
//! awaits only the socket read and decodes synchronously after it, so dropping it mid-read
//! loses nothing. Dropping the body closes the socket.

use crate::net::{Conn, DialError, Resolver, SystemResolver, dial, platform_tls};
use fogar_ports::gateway::GatewayAddr;
use fogar_ports::gateway_http::{
    GatewayHttp, HttpCall, HttpError, HttpReply, MAX_CHUNK_BYTES, ResponseBody, StreamReply,
    request_allowed, stream_allowed,
};
use rustls::ClientConfig;
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroizing};
use std::{io, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Largest response head (status line + headers) accepted.
const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Adapter tuning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    /// Per resolved address (and for the name lookup).
    pub connect_timeout: Duration,
    /// Whole call: resolve + connect + request + response.
    pub call_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            // Below the core's 1-2 s probe deadline, so a dead first address still leaves time
            // for the next one.
            connect_timeout: Duration::from_secs(1),
            call_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpGateway<R = SystemResolver> {
    cfg: HttpConfig,
    resolver: R,
    tls: Option<Arc<ClientConfig>>,
}

impl HttpGateway<SystemResolver> {
    pub fn new(cfg: HttpConfig) -> Self {
        Self::with_resolver(cfg, SystemResolver)
    }
}

impl<R: Resolver> HttpGateway<R> {
    /// `https` uses the OS trust store (or is refused if the platform verifier is unavailable).
    pub fn with_resolver(cfg: HttpConfig, resolver: R) -> Self {
        Self {
            cfg,
            resolver,
            tls: platform_tls(),
        }
    }

    /// Replaces the TLS configuration (tests: a private test CA). The config must verify
    /// certificates; this crate never builds one that does not. Hidden: production wiring uses
    /// the platform verifier from the constructors.
    #[doc(hidden)]
    pub fn with_tls(mut self, tls: Arc<ClientConfig>) -> Self {
        self.tls = Some(tls);
        self
    }

    async fn call(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: Option<&SecretString>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<HttpReply, HttpError> {
        let exchange = async {
            let mut s = self
                .send(addr, call, "application/json", token, None, body)
                .await?;
            read_response(&mut s, call.max_body()).await
        };
        tokio::time::timeout(self.cfg.call_timeout, exchange)
            .await
            .map_err(|_| transport(io::ErrorKind::TimedOut.into()))?
    }

    /// Dials `addr` and writes the request; returns the connection for the response.
    async fn send(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        accept: &str,
        token: Option<&SecretString>,
        last_event_id: Option<&str>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<Conn, HttpError> {
        let mut s = dial(
            &self.resolver,
            self.tls.as_ref(),
            addr,
            self.cfg.connect_timeout,
        )
        .await
        .map_err(|e| match e {
            DialError::Refused => HttpError::Refused,
            DialError::Unreachable(_) => HttpError::Unreachable,
            DialError::Tls(e) => transport(e),
        })?;
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: fogar-agent\r\nAccept: {accept}\r\nConnection: close\r\n",
            call.method(),
            call.path(),
            addr.authority()
        );
        // Not a secret; checked printable ASCII by `stream_allowed`.
        if let Some(id) = last_event_id {
            head.push_str(&format!("Last-Event-ID: {id}\r\n"));
        }
        if let Some(b) = &body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        }
        // Head, token and body are separate writes: neither the token nor the body (pairing
        // code) is ever copied into a String we cannot wipe.
        s.write_all(head.as_bytes()).await.map_err(transport)?;
        if let Some(t) = token {
            s.write_all(b"Authorization: Bearer ")
                .await
                .map_err(transport)?;
            s.write_all(t.expose_secret().as_bytes())
                .await
                .map_err(transport)?;
            s.write_all(b"\r\n").await.map_err(transport)?;
        }
        s.write_all(b"\r\n").await.map_err(transport)?;
        if let Some(b) = &body {
            s.write_all(b).await.map_err(transport)?;
        }
        s.flush().await.map_err(transport)?;
        Ok(s)
    }
}

fn transport(e: io::Error) -> HttpError {
    HttpError::Transport(Box::new(e))
}

fn malformed(what: &'static str) -> HttpError {
    transport(io::Error::new(io::ErrorKind::InvalidData, what))
}

impl<R: Resolver> GatewayHttp for HttpGateway<R> {
    async fn request(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: Option<&SecretString>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<HttpReply, HttpError> {
        if !request_allowed(call, token) {
            return Err(HttpError::Refused);
        }
        self.call(addr, call, token, body).await
    }

    type Body = HttpStreamBody;

    async fn open_stream(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: &SecretString,
        last_event_id: Option<&str>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<StreamReply<HttpStreamBody>, HttpError> {
        if !stream_allowed(call, token, last_event_id) {
            return Err(HttpError::Refused);
        }
        let accept = match call {
            HttpCall::Events { .. } => "text/event-stream",
            _ => "application/octet-stream",
        };
        let open = async {
            let mut s = self
                .send(addr, call, accept, Some(token), last_event_id, body)
                .await?;
            let (status, framing, early) = read_head(&mut s).await?;
            Ok(StreamReply {
                status,
                body: HttpStreamBody::new(s, framing, early),
            })
        };
        tokio::time::timeout(self.cfg.call_timeout, open)
            .await
            .map_err(|_| transport(io::ErrorKind::TimedOut.into()))?
    }
}

/// Body of an `open_stream` reply (see the module doc). Dropping it closes the connection.
pub struct HttpStreamBody {
    conn: Conn,
    state: BodyState,
    /// Body bytes that arrived with the response head; decoded before the next read.
    early: Zeroizing<Vec<u8>>,
    buf: Vec<u8>,
}

enum BodyState {
    Length(usize),
    Chunked(Chunked),
    ToEof,
    Done,
}

impl std::fmt::Debug for HttpStreamBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpStreamBody")
    }
}

impl HttpStreamBody {
    fn new(conn: Conn, framing: Framing, early: Zeroizing<Vec<u8>>) -> Self {
        let state = match framing {
            Framing::Length(0) => BodyState::Done,
            Framing::Length(n) => BodyState::Length(n),
            // No total cap on a stream.
            Framing::Chunked => BodyState::Chunked(Chunked::new(usize::MAX)),
            Framing::ToEof => BodyState::ToEof,
        };
        Self {
            conn,
            state,
            early,
            buf: vec![0u8; MAX_CHUNK_BYTES],
        }
    }
}

/// Decodes raw wire bytes into body bytes. The output is never longer than `input`.
fn decode(state: &mut BodyState, input: &[u8]) -> Result<Vec<u8>, HttpError> {
    Ok(match state {
        BodyState::Length(left) => {
            let n = (*left).min(input.len());
            *left -= n;
            let out = input[..n].to_vec();
            if *left == 0 {
                *state = BodyState::Done;
            }
            out
        }
        BodyState::Chunked(dec) => {
            let mut out = Vec::new();
            if dec.feed(input, &mut out)?.done {
                *state = BodyState::Done;
            }
            out
        }
        BodyState::ToEof => input.to_vec(),
        BodyState::Done => Vec::new(),
    })
}

impl ResponseBody for HttpStreamBody {
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        // `early` is at most MAX_HEAD_BYTES (< MAX_CHUNK_BYTES).
        let early = std::mem::take(&mut self.early);
        let out = decode(&mut self.state, &early)?;
        if !out.is_empty() {
            return Ok(Some(out));
        }
        loop {
            if matches!(self.state, BodyState::Done) {
                return Ok(None);
            }
            // The only await: a socket read is cancel-safe, and everything after it is
            // synchronous, so a dropped `chunk()` never holds read bytes.
            let n = self.conn.read(&mut self.buf).await.map_err(transport)?;
            if n == 0 {
                return match self.state {
                    BodyState::ToEof => {
                        self.state = BodyState::Done;
                        Ok(None)
                    }
                    _ => Err(malformed("connection closed inside a streamed body")),
                };
            }
            let out = decode(&mut self.state, &self.buf[..n])?;
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

/// Reads one response from a `Connection: close` stream into one pre-sized, wiped buffer.
/// `max` is the body cap, enforced while reading.
pub(crate) async fn read_response<S: AsyncRead + Unpin>(
    s: &mut S,
    max: usize,
) -> Result<HttpReply, HttpError> {
    let (status, framing, early) = read_head(s).await?;
    let early = &early[..];

    let mut body = match framing {
        Framing::Length(len) if len > max => return Err(HttpError::TooLarge { max }),
        Framing::Length(len) => {
            let mut body = Zeroizing::new(vec![0u8; len]);
            let n = early.len().min(len);
            body[..n].copy_from_slice(&early[..n]);
            s.read_exact(&mut body[n..]).await.map_err(transport)?;
            return Ok(HttpReply { status, body });
        }
        Framing::Chunked => {
            let mut dec = Chunked::new(max);
            // Reserved once up to the cap and never grown (the decoder refuses past it), so no
            // stray copy is left behind.
            let mut body = Zeroizing::new(Vec::with_capacity(max));
            let mut input = early;
            let mut buf = Zeroizing::new(vec![0u8; 4096]);
            loop {
                if dec.feed(input, &mut body)?.done {
                    return Ok(HttpReply { status, body });
                }
                let n = s.read(&mut buf[..]).await.map_err(transport)?;
                if n == 0 {
                    return Err(malformed("connection closed inside a chunked body"));
                }
                input = &buf[..n];
            }
        }
        // shortcut: over TLS a server that closes without close_notify makes this a Transport
        // error (truncation guard); fine for uvicorn (always Content-Length), revisit if a
        // TLS proxy drops the length.
        // Length unknown: read to EOF (we sent `Connection: close`), one byte past the cap is
        // TooLarge.
        Framing::ToEof => Zeroizing::new(vec![0u8; max + 1]),
    };
    if early.len() > max {
        return Err(HttpError::TooLarge { max });
    }
    body[..early.len()].copy_from_slice(early);
    let mut len = early.len();
    loop {
        if len > max {
            return Err(HttpError::TooLarge { max });
        }
        let n = s.read(&mut body[len..]).await.map_err(transport)?;
        if n == 0 {
            break;
        }
        len += n;
    }
    // Shrinks in place (no reallocation, so no stray copy); the tail stays zero.
    body.truncate(len);
    Ok(HttpReply { status, body })
}

/// Reads the response head; returns the status, the framing and the body bytes that arrived
/// with the head (wiped on drop).
async fn read_head<S: AsyncRead + Unpin>(
    s: &mut S,
) -> Result<(u16, Framing, Zeroizing<Vec<u8>>), HttpError> {
    // The head buffer may also hold the first body bytes: wiped on drop.
    let mut head = Zeroizing::new(vec![0u8; MAX_HEAD_BYTES]);
    let (mut filled, mut scanned) = (0usize, 0usize);
    let head_end = loop {
        // Only the newly read bytes (plus 3 of overlap) can complete the terminator.
        let from = scanned.saturating_sub(3);
        if let Some(i) = head[from..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            break from + i;
        }
        scanned = filled;
        if filled == head.len() {
            return Err(malformed("response head too large"));
        }
        let n = s.read(&mut head[filled..]).await.map_err(transport)?;
        if n == 0 {
            return Err(malformed("connection closed inside the response head"));
        }
        filled += n;
    };
    let (status, framing) = parse_head(&head[..head_end])?;
    // In place (no reallocation): the early body bytes move to the front of the same buffer.
    head.truncate(filled);
    head.drain(..head_end + 4);
    Ok((status, framing, head))
}

/// How the response body is delimited.
#[derive(Debug, PartialEq, Eq)]
enum Framing {
    Length(usize),
    Chunked,
    ToEof,
}

#[derive(Debug, Clone, Copy)]
enum St {
    /// Chunk-size line: hex digits, then an optional `;extension` up to the CR.
    Size {
        val: usize,
        digits: u8,
        ext: bool,
    },
    SizeLf {
        val: usize,
    },
    Data {
        left: usize,
    },
    DataCr,
    DataLf,
    /// After the last chunk: trailer lines until an empty one.
    TrailerStart,
    TrailerLine,
    TrailerLineLf,
    TrailerEndLf,
    Done,
}

/// Incremental `Transfer-Encoding: chunked` decoder (RFC 9112 §7.1), sans-I/O, with every bound
/// enforced as bytes arrive: chunk sizes of at most 8 hex digits, the decoded total against
/// `max` (a chunk that would cross it is refused before its data is read), each chunk-size line
/// and the trailers (together) against `MAX_HEAD_BYTES`.
#[derive(Debug)]
pub(crate) struct Chunked {
    st: St,
    max: usize,
    total: usize,
    /// Bytes of the current chunk-size line.
    line: usize,
    /// Trailer bytes seen so far.
    trailer: usize,
}

/// Result of one `feed`.
pub(crate) struct Fed {
    /// The terminating chunk and trailers were fully read.
    pub done: bool,
}

impl Chunked {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            st: St::Size {
                val: 0,
                digits: 0,
                ext: false,
            },
            max,
            total: 0,
            line: 0,
            trailer: 0,
        }
    }

    /// Decodes `input`, appending chunk data to `out`. Bytes after the end are ignored.
    pub(crate) fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<Fed, HttpError> {
        while let Some((&b, rest)) = input.split_first() {
            if let St::Data { left } = self.st {
                let n = left.min(input.len());
                out.extend_from_slice(&input[..n]);
                self.st = if n == left {
                    St::DataCr
                } else {
                    St::Data { left: left - n }
                };
                input = &input[n..];
                continue;
            }
            input = rest;
            self.st = match self.st {
                St::Size { val, digits, ext } => {
                    self.line += 1;
                    if self.line > MAX_HEAD_BYTES {
                        return Err(malformed("chunk-size line too long"));
                    }
                    match b {
                        b'\r' if digits > 0 => St::SizeLf { val },
                        b';' if digits > 0 && !ext => St::Size {
                            val,
                            digits,
                            ext: true,
                        },
                        _ if ext && b != b'\r' && b != b'\n' => St::Size { val, digits, ext },
                        _ if !ext && b.is_ascii_hexdigit() && digits < 8 => St::Size {
                            val: val * 16 + (b as char).to_digit(16).unwrap_or(0) as usize,
                            digits: digits + 1,
                            ext,
                        },
                        _ => return Err(malformed("bad chunk size")),
                    }
                }
                St::SizeLf { val } => {
                    if b != b'\n' {
                        return Err(malformed("bad chunk-size line ending"));
                    }
                    self.line = 0;
                    if val == 0 {
                        St::TrailerStart
                    } else if val > self.max - self.total {
                        return Err(HttpError::TooLarge { max: self.max });
                    } else {
                        self.total += val;
                        St::Data { left: val }
                    }
                }
                St::DataCr if b == b'\r' => St::DataLf,
                St::DataLf if b == b'\n' => St::Size {
                    val: 0,
                    digits: 0,
                    ext: false,
                },
                St::DataCr | St::DataLf => return Err(malformed("bad chunk terminator")),
                St::TrailerStart | St::TrailerLine | St::TrailerLineLf | St::TrailerEndLf => {
                    self.trailer += 1;
                    if self.trailer > MAX_HEAD_BYTES {
                        return Err(malformed("chunked trailers too long"));
                    }
                    match (self.st, b) {
                        (St::TrailerStart, b'\r') => St::TrailerEndLf,
                        (St::TrailerLine, b'\r') => St::TrailerLineLf,
                        (St::TrailerStart | St::TrailerLine, _) => St::TrailerLine,
                        (St::TrailerLineLf, b'\n') => St::TrailerStart,
                        (St::TrailerEndLf, b'\n') => St::Done,
                        _ => return Err(malformed("bad chunked trailer")),
                    }
                }
                St::Done => break,
                St::Data { .. } => unreachable!("handled above"),
            };
            if matches!(self.st, St::Done) {
                break;
            }
        }
        Ok(Fed {
            done: matches!(self.st, St::Done),
        })
    }
}

/// Status code and body framing of a response head (without the final CRLFCRLF).
fn parse_head(head: &[u8]) -> Result<(u16, Framing), HttpError> {
    let head = std::str::from_utf8(head).map_err(|_| malformed("response head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| status_line.strip_prefix("HTTP/1.0 "))
        // Exactly three digits, then a space or the end of the line (`2000`, `200OK` are bad).
        .filter(|r| r.len() == 3 || r.as_bytes().get(3) == Some(&b' '))
        .and_then(|r| r.get(..3))
        .filter(|c| c.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|c| (200..600).contains(c))
        .ok_or_else(|| malformed("bad status line"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed("bad header line"))?;
        // RFC 9112 §5.1: no whitespace before the colon (`Content-Length : 5` must not slip past).
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        {
            return Err(malformed("bad header name"));
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(malformed("bad Content-Length"));
            }
            // Only overflow is left to fail: an absurd length is still TooLarge.
            let n = value.parse::<usize>().unwrap_or(usize::MAX);
            if content_length.is_some_and(|c| c != n) {
                return Err(malformed("conflicting Content-Length"));
            }
            content_length = Some(n);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Only a lone `chunked` (no gzip & co: we never ask for them); repeated = ambiguous.
            if chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(malformed("unsupported Transfer-Encoding"));
            }
            chunked = true;
        }
    }
    // Both framings at once is the request-smuggling shape: refuse.
    let framing = match (chunked, content_length) {
        (true, Some(_)) => return Err(malformed("Content-Length with Transfer-Encoding")),
        (true, None) => Framing::Chunked,
        (false, Some(n)) => Framing::Length(n),
        (false, None) => Framing::ToEof,
    };
    Ok((status, framing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fogar_ports::gateway_http::MAX_BODY_BYTES;

    async fn parse(raw: &[u8]) -> Result<HttpReply, HttpError> {
        let mut r = raw;
        read_response(&mut r, MAX_BODY_BYTES).await
    }

    #[test]
    fn parses_and_bounds_responses() {
        fogar_testkit::run_async(parses_and_bounds());
    }

    /// Hands out one byte per `read`: the head terminator and the body cross read boundaries.
    struct Trickle<'a>(&'a [u8]);
    impl tokio::io::AsyncRead for Trickle<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some((b, rest)) = self.0.split_first() {
                buf.put_slice(&[*b]);
                self.0 = rest;
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    async fn parses_and_bounds() {
        for raw in [
            &b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}"[..],
            b"HTTP/1.1 201 Created\r\n\r\n{}",
        ] {
            let r = read_response(&mut Trickle(raw), MAX_BODY_BYTES)
                .await
                .unwrap();
            assert_eq!((r.status, &r.body[..]), (201, &b"{}"[..]));
        }
        let r = parse(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap();
        assert_eq!((r.status, &r.body[..]), (201, &b"{}"[..]));
        // No Content-Length: read to EOF.
        let r = parse(b"HTTP/1.0 200 OK\r\n\r\nabc").await.unwrap();
        assert_eq!(&r.body[..], b"abc");
        // Extra bytes after Content-Length are ignored.
        let r = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nab")
            .await
            .unwrap();
        assert_eq!(&r.body[..], b"a");

        let mut big = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        big.extend(vec![b' '; MAX_BODY_BYTES + 1]);
        assert!(matches!(parse(&big).await, Err(HttpError::TooLarge { .. })));
        let mut exact = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        exact.extend(vec![b' '; MAX_BODY_BYTES]);
        assert_eq!(parse(&exact).await.unwrap().body.len(), MAX_BODY_BYTES);
        assert!(matches!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999999\r\n\r\n").await,
            Err(HttpError::TooLarge { .. })
        ));

        for bad in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: +2\r\n\r\n{}",
            // Unsupported / ambiguous framing.
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 2\r\n\r\n0\r\n\r\n",
            // Bad chunked bodies.
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}X\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\n{}\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n000000002\r\n{}\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX: 1\r\n",
            b"HTTP/1.1 100 Continue\r\n\r\n",
            b"HTTP/2 200\r\n\r\n",
            b"HTTP/1.1 2000 OK\r\n\r\n",
            b"HTTP/1.1 200OK\r\n\r\n",
            b"SSH-2.0-OpenSSH\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nno-colon\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding : chunked\r\n\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n Content-Length: 2\r\n\r\n{}",
            b"HTTP/1.1 200 OK\r\n",
        ] {
            assert!(
                matches!(parse(bad).await, Err(HttpError::Transport(_))),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let mut endless_head = b"HTTP/1.1 200 OK\r\n".to_vec();
        endless_head.extend(vec![b'x'; MAX_HEAD_BYTES]);
        assert!(matches!(
            parse(&endless_head).await,
            Err(HttpError::Transport(_))
        ));
    }

    /// A stream body over an in-memory pipe: the server side writes `wire` then closes.
    async fn stream_of(wire: &[u8]) -> Result<(u16, Vec<Vec<u8>>), HttpError> {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        server.write_all(wire).await.map_err(transport)?;
        drop(server);
        let mut conn: Conn = Box::new(client);
        let (status, framing, early) = read_head(&mut conn).await?;
        let mut body = HttpStreamBody::new(conn, framing, early);
        let mut chunks = Vec::new();
        while let Some(c) = body.chunk().await? {
            assert!(!c.is_empty() && c.len() <= MAX_CHUNK_BYTES);
            chunks.push(c);
        }
        Ok((status, chunks))
    }

    #[test]
    fn stream_body_handles_every_framing() {
        fogar_testkit::run_async(async {
            // Content-Length: stops at the length (extra bytes ignored), even mid-read.
            let (st, c) = stream_of(b"HTTP/1.1 503 X\r\nContent-Length: 3\r\n\r\nabcdef")
                .await
                .unwrap();
            assert_eq!((st, c.concat()), (503, b"abc".to_vec()));
            let (_, c) = stream_of(b"HTTP/1.1 204 X\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            assert!(c.is_empty());
            // A large Content-Length body arrives in bounded chunks.
            let mut big = b"HTTP/1.1 200 X\r\nContent-Length: 40000\r\n\r\n".to_vec();
            big.extend((0..40_000).map(|i| (i % 251) as u8));
            let (_, c) = stream_of(&big).await.unwrap();
            assert_eq!(c.concat(), big[big.len() - 40_000..]);
            // No framing: to EOF.
            let (_, c) = stream_of(b"HTTP/1.1 200 X\r\n\r\nxyz").await.unwrap();
            assert_eq!(c.concat(), b"xyz");
            // Chunked, partly inside the head read, then the rest.
            let (_, c) = stream_of(
                b"HTTP/1.1 200 X\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
            assert_eq!(c.concat(), b"abcde");
            // Truncated bodies are errors, not a clean end.
            for cut in [
                &b"HTTP/1.1 200 X\r\nContent-Length: 5\r\n\r\nab"[..],
                b"HTTP/1.1 200 X\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab",
            ] {
                assert!(matches!(stream_of(cut).await, Err(HttpError::Transport(_))));
            }
        });
    }

    #[test]
    fn decodes_and_bounds_chunked() {
        fogar_testkit::run_async(chunked());
    }

    async fn chunked() {
        let ok = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n";
        let with = |tail: &[u8]| [&ok[..], tail].concat();
        let r = parse(&with(
            b"3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nX-T: 1\r\n\r\nignored",
        ))
        .await
        .unwrap();
        assert_eq!(&r.body[..], b"abcde");
        // Same bytes, one at a time: every state crosses a read boundary.
        let raw = with(b"3\r\nabc\r\n2\r\nde\r\n0\r\nX-T: 1\r\n\r\n");
        let r = read_response(&mut Trickle(&raw), MAX_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(&r.body[..], b"abcde");
        assert!(parse(&with(b"0\r\n\r\n")).await.unwrap().body.is_empty());

        // Exactly the cap is fine, one byte more is TooLarge before its data is read.
        let sized = |n: usize| with(format!("{n:x}\r\n").as_bytes());
        let mut exact = sized(MAX_BODY_BYTES);
        exact.extend(vec![b' '; MAX_BODY_BYTES]);
        exact.extend(b"\r\n0\r\n\r\n");
        assert_eq!(parse(&exact).await.unwrap().body.len(), MAX_BODY_BYTES);
        assert!(matches!(
            parse(&sized(MAX_BODY_BYTES + 1)).await,
            Err(HttpError::TooLarge { .. })
        ));
        // Two chunks crossing the cap together.
        let mut two = sized(MAX_BODY_BYTES);
        two.extend(vec![b' '; MAX_BODY_BYTES]);
        two.extend(b"\r\n1\r\n");
        assert!(matches!(parse(&two).await, Err(HttpError::TooLarge { .. })));
        // An absurd 8-digit size is TooLarge; 9 digits is malformed.
        assert!(matches!(
            parse(&with(b"ffffffff\r\n")).await,
            Err(HttpError::TooLarge { .. })
        ));
        // Endless trailers / endless chunk-size line are cut at MAX_HEAD_BYTES.
        let mut trailers = with(b"0\r\n");
        trailers.extend(b"X: ".iter().chain(&vec![b'y'; 2 * MAX_HEAD_BYTES]));
        assert!(matches!(
            parse(&trailers).await,
            Err(HttpError::Transport(_))
        ));
        let mut ext = with(b"1;");
        ext.extend(vec![b'e'; 2 * MAX_HEAD_BYTES]);
        assert!(matches!(parse(&ext).await, Err(HttpError::Transport(_))));
    }
}
