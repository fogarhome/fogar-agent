//! WebSocket `GatewayLink` (tokio-tungstenite 0.30). Transport only; the protocol lives in core.
//!
//! Bounded everything (fogar-longrun-reliability rule 2, fogar-secure-rust NET-3): inbound
//! message/frame <= 1 MiB (the gateway's own limit), write buffer capped (tungstenite's default
//! `max_write_buffer_size` is `usize::MAX`), small eager read buffer (energy skill pitfall).
//! Deadlines: connect/write are applied by the core (`Config`); the close handshake here.
//!
//! TLS: NOT in slice 0. `wss://` is refused with an explicit error and plain `ws://` is accepted
//! only for loopback hosts (ADR-0006, NET-1). S1 adds rustls (platform verifier).

use fogar_ports::gateway::{Frame, GatewayLink, LinkError, LinkEvent, LinkSession};
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

#[derive(Clone, PartialEq, Eq)]
pub struct WsConfig {
    url: String,
    pub close_timeout: Duration,
    pub max_frame: usize,
}

impl fmt::Debug for WsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The URL carries no credentials (the key travels in the first frame), but keep the
        // userinfo out of logs anyway (LOG-2).
        let shown = self
            .url
            .split_once('@')
            .map_or(self.url.as_str(), |(_, rest)| rest);
        f.debug_struct("WsConfig")
            .field("url", &shown)
            .field("close_timeout", &self.close_timeout)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsConfigError {
    /// Not `ws://host[:port]/path`.
    BadUrl,
    /// `wss://` needs the S1 TLS adapter.
    TlsNotYetSupported,
    /// Plain `ws://` to a non-loopback host would send the client key in clear text.
    InsecureRemote,
}

impl fmt::Display for WsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WsConfigError::BadUrl => "gateway url must look like ws://host:port/ws/stream",
            WsConfigError::TlsNotYetSupported => "wss:// is not supported before slice S1",
            WsConfigError::InsecureRemote => "plain ws:// is only allowed to a loopback address",
        })
    }
}

impl std::error::Error for WsConfigError {}

impl WsConfig {
    pub fn new(url: impl Into<String>) -> Result<Self, WsConfigError> {
        let url = url.into();
        if url.starts_with("wss://") {
            return Err(WsConfigError::TlsNotYetSupported);
        }
        let rest = url.strip_prefix("ws://").ok_or(WsConfigError::BadUrl)?;
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty() || authority.contains('@') {
            return Err(WsConfigError::BadUrl);
        }
        let host = if let Some(v6) = authority.strip_prefix('[') {
            v6.split(']').next().unwrap_or_default()
        } else {
            authority.split(':').next().unwrap_or_default()
        };
        if !is_loopback(host) {
            return Err(WsConfigError::InsecureRemote);
        }
        Ok(Self {
            url,
            close_timeout: Duration::from_secs(2),
            max_frame: WS_MAX_FRAME_BYTES,
        })
    }
    pub fn url(&self) -> &str {
        &self.url
    }
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
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

    async fn connect(&self) -> Result<WsSession, LinkError> {
        let (ws, _resp) = tokio_tungstenite::connect_async_with_config(
            self.cfg.url(),
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
        assert!(WsConfig::new("ws://127.0.0.1:8004/ws/stream").is_ok());
        assert!(WsConfig::new("ws://localhost:8004/ws/stream").is_ok());
        assert!(WsConfig::new("ws://[::1]:8004/ws/stream").is_ok());
        assert_eq!(
            WsConfig::new("wss://gw.example/ws/stream"),
            Err(WsConfigError::TlsNotYetSupported)
        );
        assert_eq!(
            WsConfig::new("ws://100.64.0.2:8004/ws/stream"),
            Err(WsConfigError::InsecureRemote)
        );
        assert_eq!(
            WsConfig::new("ws://u:p@127.0.0.1/ws"),
            Err(WsConfigError::BadUrl)
        );
        assert_eq!(
            WsConfig::new("http://127.0.0.1/"),
            Err(WsConfigError::BadUrl)
        );
        assert_eq!(WsConfig::new("ws:///x"), Err(WsConfigError::BadUrl));
    }
}
