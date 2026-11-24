//! The gateway's REST surface (well-known, `POST /pair`, the 5.0 calls; routes are keyed by
//! method and path WITH query, as `HttpCall::path` renders it):
//! - [`HttpRoutes`]: scripted replies + a log of what was asked;
//! - [`FakeGatewayHttp`]: an in-memory `GatewayHttp` over those routes (paused-time tests);
//! - [`MockHttpServer`]: the same routes over a real local HTTP/1.1 socket, for adapter
//!   contract tests and E2E.
//!
//! Streamed routes (`set_stream`) play a script of [`StreamStep`]s: over the socket as
//! `Transfer-Encoding: chunked` (one wire chunk per `Bytes`), in the fake through a bounded
//! channel of `FAKE_STREAM_DEPTH` (a full channel makes the script wait, never drop).

use fogar_ports::{
    gateway::GatewayAddr,
    gateway_http::{
        GatewayHttp, HttpCall, HttpError, HttpReply, MAX_CHUNK_BYTES, ResponseBody, StreamReply,
        request_allowed, stream_allowed,
    },
};
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroizing};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// One request the server saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRequest {
    pub method: String,
    pub path: String,
    pub content_type: Option<String>,
    /// The `Authorization` header as received. Test-only: this is where a test sees the token.
    pub authorization: Option<String>,
    /// The `Last-Event-ID` header (streams).
    pub last_event_id: Option<String>,
    /// The `Accept` header. The in-memory fake sees no headers: always `None` there.
    pub accept: Option<String>,
    pub body: Vec<u8>,
}

/// One step of a streamed reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStep {
    Bytes(Vec<u8>),
    Wait(Duration),
    /// Keep the stream open until the client closes it (counted by `disconnects`).
    HoldOpen,
}

/// Depth of the fake's stream channel.
const FAKE_STREAM_DEPTH: usize = 8;

#[derive(Debug, Clone, Default)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
    delay: Option<std::time::Duration>,
    /// Declares a huge Content-Length and streams forever.
    endless: bool,
    /// Sent as `Transfer-Encoding: chunked`, one wire chunk per entry (`body` is unused).
    chunks: Option<Vec<Vec<u8>>>,
    /// A streamed reply (`set_stream`).
    stream: Option<Vec<StreamStep>>,
}

/// Scripted routes shared by the fake and the real-socket mock. Unknown routes answer 404.
#[derive(Debug, Clone, Default)]
pub struct HttpRoutes {
    routes: Arc<Mutex<HashMap<(String, String), Reply>>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    disconnects: Arc<AtomicUsize>,
}

impl HttpRoutes {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set(&self, method: &str, path: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status,
                body: body.into(),
                ..Reply::default()
            },
        );
    }
    /// Replies after `delay` (cancellation tests).
    pub fn set_delayed(
        &self,
        method: &str,
        path: &str,
        status: u16,
        body: impl Into<Vec<u8>>,
        delay: std::time::Duration,
    ) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status,
                body: body.into(),
                delay: Some(delay),
                ..Reply::default()
            },
        );
    }
    /// Replies with `Transfer-Encoding: chunked`, one chunk per entry (empty ones are skipped).
    pub fn set_chunked(&self, method: &str, path: &str, status: u16, chunks: Vec<Vec<u8>>) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status,
                chunks: Some(chunks.into_iter().filter(|c| !c.is_empty()).collect()),
                ..Reply::default()
            },
        );
    }
    /// A streamed reply played from `script` (see [`StreamStep`]).
    pub fn set_stream(&self, method: &str, path: &str, status: u16, script: Vec<StreamStep>) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status,
                stream: Some(script),
                ..Reply::default()
            },
        );
    }
    /// Held-open streams (`StreamStep::HoldOpen`) the client closed.
    pub fn disconnects(&self) -> usize {
        self.disconnects.load(Ordering::SeqCst)
    }
    /// 200 with `Content-Length: 100000000` and a body that never ends.
    pub fn set_endless(&self, method: &str, path: &str) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status: 200,
                endless: true,
                ..Reply::default()
            },
        );
    }
    pub fn set_redirect(&self, method: &str, path: &str, to: &str) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status: 302,
                location: Some(to.into()),
                ..Reply::default()
            },
        );
    }
    /// A Fogar gateway named `name` (well-known on both paths).
    pub fn fogar(name: &str) -> Self {
        let r = Self::new();
        let wk = format!(r#"{{"service":"fogar-gateway","version":"1.29.0","name":"{name}"}}"#);
        r.set("GET", "/.well-known/fogar-gateway", 200, wk.clone());
        r
    }
    pub fn seen(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }
    fn answer(&self, req: SeenRequest) -> Reply {
        let key = (req.method.clone(), req.path.clone());
        self.seen.lock().unwrap().push(req);
        self.routes
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or(Reply {
                status: 404,
                body: br#"{"detail":"Not Found"}"#.to_vec(),
                ..Reply::default()
            })
    }
}

/// Fakes apply the clear-text policy to IP literals (they resolve nothing; names pass).
pub(crate) fn policy_allows(addr: &GatewayAddr) -> bool {
    use fogar_ports::gateway::{Scheme, cleartext_allowed};
    match (addr.scheme(), addr.host().parse::<std::net::IpAddr>()) {
        (Scheme::Plain, Ok(ip)) => cleartext_allowed(ip),
        _ => true,
    }
}

/// In-memory `GatewayHttp`: answers for `addr()` only; any other address is `Unreachable`.
#[derive(Debug, Clone)]
pub struct FakeGatewayHttp {
    routes: HttpRoutes,
    addr: GatewayAddr,
}

impl FakeGatewayHttp {
    pub fn new(routes: HttpRoutes) -> Self {
        Self {
            routes,
            addr: GatewayAddr::parse("127.0.0.1:8004").expect("addr"),
        }
    }
    pub fn at(routes: HttpRoutes, addr: GatewayAddr) -> Self {
        Self { routes, addr }
    }
    pub fn addr(&self) -> GatewayAddr {
        self.addr.clone()
    }
    /// An address nothing answers on.
    pub fn dead_addr(&self) -> GatewayAddr {
        GatewayAddr::parse("127.0.0.1:1").expect("addr")
    }
    pub fn routes(&self) -> HttpRoutes {
        self.routes.clone()
    }
}

impl GatewayHttp for FakeGatewayHttp {
    async fn request(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: Option<&SecretString>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<HttpReply, HttpError> {
        if !request_allowed(call, token) || !policy_allows(addr) {
            return Err(HttpError::Refused);
        }
        // Yield once so a dropped in-flight call is a real cancellation point.
        tokio::task::yield_now().await;
        if *addr != self.addr {
            return Err(HttpError::Unreachable);
        }
        let r = self.routes.answer(SeenRequest {
            method: call.method().into(),
            path: call.path(),
            content_type: body.is_some().then(|| "application/json".into()),
            authorization: token.map(|t| format!("Bearer {}", t.expose_secret())),
            last_event_id: None,
            accept: None,
            body: body.map(|b| b.to_vec()).unwrap_or_default(),
        });
        if let Some(d) = r.delay {
            tokio::time::sleep(d).await;
        }
        let max = call.max_body();
        let len = r
            .chunks
            .as_ref()
            .map_or(r.body.len(), |c| c.iter().map(Vec::len).sum());
        if r.endless || len > max {
            return Err(HttpError::TooLarge { max });
        }
        let body = r.chunks.map_or(r.body, |c| c.concat());
        Ok(HttpReply::new(r.status, body))
    }

    type Body = FakeStreamBody;

    async fn open_stream(
        &self,
        addr: &GatewayAddr,
        call: &HttpCall,
        token: &SecretString,
        last_event_id: Option<&str>,
        body: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<StreamReply<FakeStreamBody>, HttpError> {
        if !stream_allowed(call, token, last_event_id) || !policy_allows(addr) {
            return Err(HttpError::Refused);
        }
        tokio::task::yield_now().await;
        if *addr != self.addr {
            return Err(HttpError::Unreachable);
        }
        let r = self.routes.answer(SeenRequest {
            method: call.method().into(),
            path: call.path(),
            content_type: body.is_some().then(|| "application/json".into()),
            authorization: Some(format!("Bearer {}", token.expose_secret())),
            last_event_id: last_event_id.map(Into::into),
            accept: None,
            body: body.map(|b| b.to_vec()).unwrap_or_default(),
        });
        let script = r
            .stream
            .unwrap_or_else(|| vec![StreamStep::Bytes(r.chunks.map_or(r.body, |c| c.concat()))]);
        let (tx, rx) = tokio::sync::mpsc::channel(FAKE_STREAM_DEPTH);
        let disconnects = self.routes.disconnects.clone();
        // Ends with the script, or when the body is dropped (a send fails).
        tokio::spawn(async move {
            for step in script {
                match step {
                    StreamStep::Bytes(b) => {
                        for piece in b.chunks(MAX_CHUNK_BYTES) {
                            if tx.send(piece.to_vec()).await.is_err() {
                                return;
                            }
                        }
                    }
                    StreamStep::Wait(d) => tokio::time::sleep(d).await,
                    StreamStep::HoldOpen => {
                        tx.closed().await;
                        disconnects.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });
        Ok(StreamReply {
            status: r.status,
            body: FakeStreamBody(rx),
        })
    }
}

/// The fake's streamed body: the script's bytes through a bounded channel.
#[derive(Debug)]
pub struct FakeStreamBody(tokio::sync::mpsc::Receiver<Vec<u8>>);

impl ResponseBody for FakeStreamBody {
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        // `recv` is cancel-safe.
        Ok(self.0.recv().await)
    }
}

/// The same routes over a real loopback HTTP/1.1 socket (`Connection: close`, one request per
/// connection). Stops when dropped.
#[derive(Debug)]
pub struct MockHttpServer {
    addr: GatewayAddr,
    dead: GatewayAddr,
    routes: HttpRoutes,
    task: JoinHandle<()>,
}

impl Drop for MockHttpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockHttpServer {
    pub async fn start(routes: HttpRoutes) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = GatewayAddr::parse(&listener.local_addr()?.to_string()).expect("addr");
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").await?;
            GatewayAddr::parse(&l.local_addr()?.to_string()).expect("addr")
        };
        let r = routes.clone();
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve_http(sock, r.clone()));
            }
        });
        Ok(Self {
            addr,
            dead,
            routes,
            task,
        })
    }
    pub fn addr(&self) -> GatewayAddr {
        self.addr.clone()
    }
    pub fn dead_addr(&self) -> GatewayAddr {
        self.dead.clone()
    }
    pub fn routes(&self) -> HttpRoutes {
        self.routes.clone()
    }
}

/// Answers one request on `sock` from `routes` (`Connection: close`).
pub(crate) async fn serve_http(mut sock: TcpStream, routes: HttpRoutes) {
    let Some(req) = read_request(&mut sock).await else {
        return;
    };
    let rep = routes.answer(req);
    if let Some(d) = rep.delay {
        tokio::time::sleep(d).await;
    }
    if let Some(script) = rep.stream {
        return serve_stream(sock, rep.status, script, &routes).await;
    }
    if rep.endless {
        let _ = sock
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000000\r\n\r\n")
            .await;
        let chunk = vec![b' '; 4096];
        while sock.write_all(&chunk).await.is_ok() {}
        return;
    }
    let framing = match &rep.chunks {
        Some(_) => "Transfer-Encoding: chunked".to_owned(),
        None => format!("Content-Length: {}", rep.body.len()),
    };
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\n{framing}\r\nConnection: close\r\n",
        rep.status
    );
    if let Some(l) = rep.location {
        head.push_str(&format!("Location: {l}\r\n"));
    }
    head.push_str("\r\n");
    let _ = sock.write_all(head.as_bytes()).await;
    if let Some(chunks) = &rep.chunks {
        for c in chunks {
            let _ = sock
                .write_all(format!("{:x}\r\n", c.len()).as_bytes())
                .await;
            let _ = sock.write_all(c).await;
            let _ = sock.write_all(b"\r\n").await;
        }
        let _ = sock.write_all(b"0\r\n\r\n").await;
    } else {
        let _ = sock.write_all(&rep.body).await;
    }
    let _ = sock.shutdown().await;
}

/// Plays `script` as a `Transfer-Encoding: chunked` reply; ends with the terminating chunk
/// unless it holds the connection open.
async fn serve_stream(
    mut sock: TcpStream,
    status: u16,
    script: Vec<StreamStep>,
    routes: &HttpRoutes,
) {
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for step in script {
        match step {
            StreamStep::Bytes(b) if !b.is_empty() => {
                let mut wire = format!("{:x}\r\n", b.len()).into_bytes();
                wire.extend_from_slice(&b);
                wire.extend_from_slice(b"\r\n");
                if sock.write_all(&wire).await.is_err() {
                    return;
                }
            }
            StreamStep::Bytes(_) => {}
            StreamStep::Wait(d) => tokio::time::sleep(d).await,
            StreamStep::HoldOpen => {
                // EOF or a reset (unread data on the client side): the client closed.
                let _ = sock.read(&mut [0u8; 1]).await;
                routes.disconnects.fetch_add(1, Ordering::SeqCst);
                return;
            }
        }
    }
    let _ = sock.write_all(b"0\r\n\r\n").await;
    let _ = sock.shutdown().await;
}

async fn read_request(sock: &mut TcpStream) -> Option<SeenRequest> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > 16 * 1024 || sock.read_buf(&mut buf).await.ok()? == 0 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?.to_owned(), first.next()?.to_owned());
    let mut len = 0usize;
    let mut content_type = None;
    let mut authorization = None;
    let mut last_event_id = None;
    let mut accept = None;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().ok()?,
                "content-type" => content_type = Some(v.trim().to_owned()),
                "authorization" => authorization = Some(v.trim().to_owned()),
                "last-event-id" => last_event_id = Some(v.trim().to_owned()),
                "accept" => accept = Some(v.trim().to_owned()),
                _ => {}
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        if sock.read_buf(&mut body).await.ok()? == 0 {
            return None;
        }
    }
    body.truncate(len);
    Some(SeenRequest {
        method,
        path,
        content_type,
        authorization,
        last_event_id,
        accept,
        body,
    })
}
