//! The gateway's pre-session REST surface (`/.well-known/fogar-gateway`, `POST /pair`):
//! - [`HttpRoutes`]: scripted replies + a log of what was asked;
//! - [`FakeGatewayHttp`]: an in-memory `GatewayHttp` over those routes (paused-time tests);
//! - [`MockHttpServer`]: the same routes over a real local HTTP/1.1 socket, for adapter
//!   contract tests and E2E.

use fogar_ports::{
    gateway::GatewayAddr,
    gateway_http::{GatewayHttp, HttpCall, HttpError, HttpReply, MAX_BODY_BYTES},
};
use secrecy::zeroize::Zeroizing;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

/// One request the server saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRequest {
    pub method: String,
    pub path: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
}

/// Scripted routes shared by the fake and the real-socket mock. Unknown routes answer 404.
#[derive(Debug, Clone, Default)]
pub struct HttpRoutes {
    routes: Arc<Mutex<HashMap<(String, String), Reply>>>,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
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
                location: None,
            },
        );
    }
    pub fn set_redirect(&self, method: &str, path: &str, to: &str) {
        self.routes.lock().unwrap().insert(
            (method.into(), path.into()),
            Reply {
                status: 302,
                body: Vec::new(),
                location: Some(to.into()),
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
                location: None,
            })
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
    fn call(
        &self,
        addr: &GatewayAddr,
        method: &str,
        call: HttpCall,
        body: Vec<u8>,
    ) -> Result<HttpReply, HttpError> {
        if *addr != self.addr {
            return Err(HttpError::Unreachable);
        }
        let r = self.routes.answer(SeenRequest {
            method: method.into(),
            path: call.path().into(),
            content_type: (method == "POST").then(|| "application/json".into()),
            body,
        });
        if r.body.len() > MAX_BODY_BYTES {
            return Err(HttpError::TooLarge {
                max: MAX_BODY_BYTES,
            });
        }
        Ok(HttpReply::new(r.status, r.body))
    }
}

impl GatewayHttp for FakeGatewayHttp {
    async fn get(&self, addr: &GatewayAddr, call: HttpCall) -> Result<HttpReply, HttpError> {
        if !matches!(call, HttpCall::WellKnown(_)) {
            return Err(HttpError::Refused);
        }
        // Yield once so a dropped in-flight call is a real cancellation point.
        tokio::task::yield_now().await;
        self.call(addr, "GET", call, Vec::new())
    }
    async fn post_json(
        &self,
        addr: &GatewayAddr,
        call: HttpCall,
        body: Zeroizing<Vec<u8>>,
    ) -> Result<HttpReply, HttpError> {
        tokio::task::yield_now().await;
        self.call(addr, "POST", call, body.to_vec())
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
            while let Ok((mut sock, _)) = listener.accept().await {
                let r = r.clone();
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut sock).await else {
                        return;
                    };
                    let rep = r.answer(req);
                    let mut head = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                        rep.status,
                        rep.body.len()
                    );
                    if let Some(l) = rep.location {
                        head.push_str(&format!("Location: {l}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&rep.body).await;
                    let _ = sock.shutdown().await;
                });
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

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<SeenRequest> {
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
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().ok()?,
                "content-type" => content_type = Some(v.trim().to_owned()),
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
        body,
    })
}
