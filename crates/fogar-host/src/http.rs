//! `GatewayHttp` over a tiny hand-written HTTP/1.1 client (fogar-secure-rust NET, port doc in
//! `fogar_ports::gateway_http`).
//!
//! Why not hyper/reqwest: the port needs exactly two fixed requests, one per connection, no
//! redirects, no pooling, no compression, and a body read into ONE pre-sized wiped buffer.
//! hyper would add ~10 crates, its own buffering (copies of the `/pair` reply we cannot wipe)
//! and connection reuse we would have to switch off. ~150 lines here are easier to review.
//!
//! Bounds: response head <= `MAX_HEAD_BYTES`, body <= `MAX_BODY_BYTES` (checked from
//! `Content-Length` before reading, and while reading when the length is unknown); every await
//! sits under `HttpConfig::call_timeout`, each connect attempt under `connect_timeout`.

use crate::net::{DialError, Resolver, SystemResolver, dial, platform_tls};
use fogar_ports::gateway::GatewayAddr;
use fogar_ports::gateway_http::{GatewayHttp, HttpCall, HttpError, HttpReply, MAX_BODY_BYTES};
use rustls::ClientConfig;
use secrecy::zeroize::Zeroizing;
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
            connect_timeout: Duration::from_secs(3),
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
        method: &str,
        call: HttpCall,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<HttpReply, HttpError> {
        let exchange = async {
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
                "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: fogar-agent\r\nAccept: application/json\r\nConnection: close\r\n",
                call.path(),
                addr.authority()
            );
            if let Some(b) = &body {
                head.push_str(&format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n",
                    b.len()
                ));
            }
            head.push_str("\r\n");
            // Head and body are separate writes: the body (pairing code) is never copied into
            // a String we cannot wipe.
            s.write_all(head.as_bytes()).await.map_err(transport)?;
            if let Some(b) = &body {
                s.write_all(b).await.map_err(transport)?;
            }
            s.flush().await.map_err(transport)?;
            drop(body);
            read_response(&mut s).await
        };
        tokio::time::timeout(self.cfg.call_timeout, exchange)
            .await
            .map_err(|_| transport(io::ErrorKind::TimedOut.into()))?
    }
}

fn transport(e: io::Error) -> HttpError {
    HttpError::Transport(Box::new(e))
}

fn malformed(what: &'static str) -> HttpError {
    transport(io::Error::new(io::ErrorKind::InvalidData, what))
}

impl<R: Resolver> GatewayHttp for HttpGateway<R> {
    async fn get(&self, addr: &GatewayAddr, call: HttpCall) -> Result<HttpReply, HttpError> {
        if matches!(call, HttpCall::Pair) {
            return Err(HttpError::Refused);
        }
        self.call(addr, "GET", call, None).await
    }

    async fn post_json(
        &self,
        addr: &GatewayAddr,
        call: HttpCall,
        body: Zeroizing<Vec<u8>>,
    ) -> Result<HttpReply, HttpError> {
        if !matches!(call, HttpCall::Pair) {
            return Err(HttpError::Refused);
        }
        self.call(addr, "POST", call, Some(body)).await
    }
}

/// Reads one response from a `Connection: close` stream into one pre-sized, wiped buffer.
pub(crate) async fn read_response<S: AsyncRead + Unpin>(s: &mut S) -> Result<HttpReply, HttpError> {
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
    let (status, content_length) = parse_head(&head[..head_end])?;
    let early = &head[head_end + 4..filled];

    let mut body = match content_length {
        Some(len) if len > MAX_BODY_BYTES => {
            return Err(HttpError::TooLarge {
                max: MAX_BODY_BYTES,
            });
        }
        Some(len) => {
            let mut body = Zeroizing::new(vec![0u8; len]);
            let n = early.len().min(len);
            body[..n].copy_from_slice(&early[..n]);
            s.read_exact(&mut body[n..]).await.map_err(transport)?;
            return Ok(HttpReply { status, body });
        }
        // shortcut: over TLS a server that closes without close_notify makes this a Transport
        // error (truncation guard); fine for uvicorn (always Content-Length), revisit if a
        // TLS proxy drops the length.
        // Length unknown: read to EOF (we sent `Connection: close`), one byte past the cap is
        // TooLarge.
        None => Zeroizing::new(vec![0u8; MAX_BODY_BYTES + 1]),
    };
    if early.len() > MAX_BODY_BYTES {
        return Err(HttpError::TooLarge {
            max: MAX_BODY_BYTES,
        });
    }
    body[..early.len()].copy_from_slice(early);
    let mut len = early.len();
    loop {
        if len > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge {
                max: MAX_BODY_BYTES,
            });
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

/// Status code and `Content-Length` of a response head (without the final CRLFCRLF).
fn parse_head(head: &[u8]) -> Result<(u16, Option<usize>), HttpError> {
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
            // shortcut: chunked replies are refused (uvicorn sends Content-Length for both
            // calls); add a bounded chunked decoder if a TLS proxy turns out to re-chunk them.
            return Err(malformed("Transfer-Encoding is not supported"));
        }
    }
    Ok((status, content_length))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &[u8]) -> Result<HttpReply, HttpError> {
        let mut r = raw;
        read_response(&mut r).await
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
            let r = read_response(&mut Trickle(raw)).await.unwrap();
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
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
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
}
