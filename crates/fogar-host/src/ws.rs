//! WebSocket `GatewayLink` (tokio-tungstenite 0.30). Transport only; the protocol lives in core.
//!
//! Bounded everything (fogar-longrun-reliability rule 2, fogar-secure-rust NET-3): inbound
//! message/frame <= 1 MiB (the gateway's own limit), write buffer capped (tungstenite's default
//! `max_write_buffer_size` is `usize::MAX`), small eager read buffer (energy skill pitfall).
//! Deadlines: connect/write are applied by the core (`Config`); the close handshake here.
//!
//! TLS: not yet. `wss://` is refused with an explicit error and plain `ws://` is accepted
//! only for loopback hosts (ADR-0006, NET-1). The S1 host PR adds rustls and explicit LAN/tailnet.

use fogar_ports::gateway::{
    Frame, GatewayAddr, GatewayLink, LinkError, LinkEvent, LinkSession, Scheme,
};
use fogar_proto::limits::WS_MAX_FRAME_BYTES;
use futures_util::{SinkExt, StreamExt};
use std::{fmt, sync::Arc, time::Duration};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
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
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            close_timeout: Duration::from_secs(2),
            max_frame: WS_MAX_FRAME_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsConfigError {
    /// `https`/`wss` needs the TLS adapter (S1, fogar-host PR).
    TlsNotYetSupported,
    /// Plain `ws://` to a non-loopback host would send the client key in clear text.
    InsecureRemote,
}

impl fmt::Display for WsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WsConfigError::TlsNotYetSupported => "wss:// is not supported yet",
            WsConfigError::InsecureRemote => "plain ws:// is only allowed to a loopback address",
        })
    }
}

impl std::error::Error for WsConfigError {}

impl WsConfig {
    /// Transport policy for `addr` (NET-1, ADR-0006 item 4). Checked on every `connect`;
    /// callers may check early to report a usage error.
    pub fn check(addr: &GatewayAddr) -> Result<(), WsConfigError> {
        match addr.scheme() {
            Scheme::Tls => Err(WsConfigError::TlsNotYetSupported),
            Scheme::Plain if !is_loopback(addr.host()) => Err(WsConfigError::InsecureRemote),
            Scheme::Plain => Ok(()),
        }
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback())
}

#[derive(Debug, Clone)]
pub struct WsGatewayLink {
    cfg: Arc<WsConfig>,
}

impl WsGatewayLink {
    pub fn new(cfg: WsConfig) -> Self {
        Self { cfg: Arc::new(cfg) }
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
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
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

impl GatewayLink for WsGatewayLink {
    type Session = WsSession;

    async fn connect(&self, addr: &GatewayAddr) -> Result<WsSession, LinkError> {
        WsConfig::check(addr).map_err(|_| LinkError::NotConfigured)?;
        let url = format!("ws://{}/ws/stream", addr.authority());
        let (ws, _resp) = tokio_tungstenite::connect_async_with_config(
            url,
            Some(ws_config(self.cfg.max_frame)),
            true,
        )
        .await
        .map_err(transport)?;
        Ok(WsSession {
            ws,
            cfg: self.cfg.clone(),
            closed: false,
        })
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
        assert_eq!(
            p("wss://gw.example"),
            Err(WsConfigError::TlsNotYetSupported)
        );
        assert_eq!(p("100.64.0.2:8004"), Err(WsConfigError::InsecureRemote));
    }
}
