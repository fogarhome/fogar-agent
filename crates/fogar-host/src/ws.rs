//! WebSocket `GatewayLink` (tokio-tungstenite 0.30). Transport only; the protocol lives in core.
//!
//! Bounded everything (fogar-longrun-reliability rule 2, fogar-secure-rust NET-3): inbound
//! message/frame <= 1 MiB (the gateway's own limit), write buffer capped (tungstenite's default
//! `max_write_buffer_size` is `usize::MAX`), small eager read buffer (energy skill pitfall).
//! Deadlines: the whole connect (resolve + TCP + upgrade) under `connect_timeout`, each TCP
//! attempt under `attempt_timeout`, the close handshake under `close_timeout`; send/recv are
//! bounded by the core.
//!
//! Transport policy (NET-1, ADR-0006 item 4): the name is resolved ONCE by [`Resolver`],
//! `cleartext_allowed` is applied to every resolved address, and the upgrade runs over the
//! socket already connected to one of them (tungstenite never resolves again). `wss://` runs
//! over rustls with the OS trust store (`net::platform_tls`); no clear-text fallback.

use crate::net::{Conn, DialError, Resolver, SystemResolver, dial, platform_tls};
use fogar_ports::gateway::{
    Frame, GatewayAddr, GatewayLink, LinkError, LinkEvent, LinkSession, Scheme, cleartext_allowed,
};
use fogar_proto::limits::WS_MAX_FRAME_BYTES;
use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use std::{fmt, io, sync::Arc, time::Duration};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    },
};

/// Close code reported when the peer's close frame carried no status (RFC 6455 §7.1.5).
const CLOSE_NO_STATUS: u16 = 1005;

/// Adapter tuning. The gateway address is per call (`GatewayLink::connect`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsConfig {
    pub close_timeout: Duration,
    pub max_frame: usize,
    /// Whole `connect`: resolve + TCP + WebSocket upgrade.
    pub connect_timeout: Duration,
    /// Per resolved address (and for the name lookup).
    pub attempt_timeout: Duration,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            close_timeout: Duration::from_secs(2),
            max_frame: WS_MAX_FRAME_BYTES,
            connect_timeout: Duration::from_secs(10),
            attempt_timeout: Duration::from_secs(3),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsConfigError {
    /// Plain `ws://` to an IP outside loopback/LAN/tailnet would send the client key in clear
    /// text.
    InsecureRemote,
}

impl fmt::Display for WsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WsConfigError::InsecureRemote => {
                "plain ws:// is only allowed to loopback, LAN or tailnet addresses"
            }
        })
    }
}

impl std::error::Error for WsConfigError {}

impl WsConfig {
    /// Early usage check of the transport policy (NET-1): IP literals only. A name passes here;
    /// `connect` checks every address it resolves to.
    pub fn check(addr: &GatewayAddr) -> Result<(), WsConfigError> {
        match addr.scheme() {
            Scheme::Tls => Ok(()),
            Scheme::Plain
                if addr
                    .host()
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| !cleartext_allowed(ip)) =>
            {
                Err(WsConfigError::InsecureRemote)
            }
            Scheme::Plain => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WsGatewayLink<R = SystemResolver> {
    cfg: Arc<WsConfig>,
    resolver: Arc<R>,
    tls: Option<Arc<ClientConfig>>,
}

impl WsGatewayLink<SystemResolver> {
    pub fn new(cfg: WsConfig) -> Self {
        Self::with_resolver(cfg, SystemResolver)
    }
}

impl<R: Resolver> WsGatewayLink<R> {
    pub fn with_resolver(cfg: WsConfig, resolver: R) -> Self {
        Self {
            cfg: Arc::new(cfg),
            resolver: Arc::new(resolver),
            tls: platform_tls(),
        }
    }

    /// Replaces the TLS configuration (tests: a private test CA). The config must verify
    /// certificates; this crate never builds one that does not.
    pub fn with_tls(mut self, tls: Arc<ClientConfig>) -> Self {
        self.tls = Some(tls);
        self
    }
}

fn ws_config(max: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(max))
        .max_frame_size(Some(max))
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(2 * max)
}

pub struct WsSession {
    ws: WebSocketStream<Conn>,
    cfg: Arc<WsConfig>,
    closed: bool,
}

impl fmt::Debug for WsSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsSession")
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

fn transport(e: impl std::error::Error + Send + Sync + 'static) -> LinkError {
    LinkError::Transport(Box::new(e))
}

impl LinkSession for WsSession {
    fn max_frame(&self) -> usize {
        self.cfg.max_frame
    }

    async fn send(&mut self, frame: Frame) -> Result<(), LinkError> {
        if self.closed {
            return Err(LinkError::Disconnected);
        }
        if frame.len() > self.cfg.max_frame {
            return Err(LinkError::TooLarge {
                len: frame.len(),
                max: self.cfg.max_frame,
            });
        }
        let m = match frame {
            Frame::Text(t) => Message::Text(t.into()),
            Frame::Binary(b) => Message::Binary(b.into()),
        };
        // `send` = feed + flush: small control frames must not sit in the write buffer.
        self.ws.send(m).await.map_err(transport)
    }

    async fn recv(&mut self) -> Option<LinkEvent> {
        // Cancel-safe: `StreamExt::next` is; pings are answered by tungstenite while we read.
        loop {
            return match self.ws.next().await? {
                Ok(Message::Text(t)) => Some(LinkEvent::Frame(Frame::Text(t.as_str().to_owned()))),
                Ok(Message::Binary(b)) => Some(LinkEvent::Frame(Frame::Binary(b.to_vec()))),
                Ok(Message::Close(Some(cf))) => Some(LinkEvent::Closed {
                    code: u16::from(cf.code),
                    reason: cf.reason.as_str().to_owned(),
                }),
                Ok(Message::Close(None)) => Some(LinkEvent::Closed {
                    code: CLOSE_NO_STATUS,
                    reason: String::new(),
                }),
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => continue,
                Err(_) => None,
            };
        }
    }

    async fn close(&mut self, code: u16, reason: &str) -> Result<(), LinkError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        };
        let t = self.cfg.close_timeout;
        let ws = &mut self.ws;
        // RFC 6455 §7.1.1: send our close, then wait (bounded) for the server to finish.
        let _ = tokio::time::timeout(t, async {
            if ws.close(Some(frame)).await.is_ok() {
                while ws.next().await.is_some() {}
            }
        })
        .await;
        Ok(())
    }
}

impl<R: Resolver> GatewayLink for WsGatewayLink<R> {
    type Session = WsSession;

    async fn connect(&self, addr: &GatewayAddr) -> Result<WsSession, LinkError> {
        let cfg = &self.cfg;
        let connect = async {
            let conn = match dial(
                &*self.resolver,
                self.tls.as_ref(),
                addr,
                cfg.attempt_timeout,
            )
            .await
            {
                Ok(s) => s,
                Err(DialError::Refused) => return Err(LinkError::NotConfigured),
                Err(DialError::Unreachable(e) | DialError::Tls(e)) => return Err(transport(e)),
            };
            let scheme = match addr.scheme() {
                Scheme::Plain => "ws",
                Scheme::Tls => "wss",
            };
            let url = format!("{scheme}://{}/ws/stream", addr.authority());
            let (ws, _resp) = tokio_tungstenite::client_async_with_config(
                url,
                conn,
                Some(ws_config(cfg.max_frame)),
            )
            .await
            .map_err(transport)?;
            Ok(WsSession {
                ws,
                cfg: cfg.clone(),
                closed: false,
            })
        };
        tokio::time::timeout(cfg.connect_timeout, connect)
            .await
            .map_err(|_| transport(io::Error::from(io::ErrorKind::TimedOut)))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy() {
        let p = |s: &str| WsConfig::check(&GatewayAddr::parse(s).unwrap());
        assert!(p("127.0.0.1:8004").is_ok());
        assert!(p("localhost").is_ok());
        assert!(p("[::1]:8004").is_ok());
        assert!(p("100.64.0.2:8004").is_ok());
        assert!(p("192.168.1.19").is_ok());
        assert!(
            p("green-house").is_ok(),
            "names are checked after resolving"
        );
        assert!(p("wss://gw.example").is_ok());
        assert_eq!(p("203.0.113.7:8004"), Err(WsConfigError::InsecureRemote));
    }
}
