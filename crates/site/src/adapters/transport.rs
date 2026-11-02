//! The ways a site reaches equipment, all with one shape: a connection is run until it ends, what
//! arrives goes down a channel, and sending says whether the thing can have gone out, because that
//! decides whether a retry is safe (`crate::adapter`). What a message means is the definition's.
//!
//! A definition can come from the platform, so what it may open here is the site's to limit: a
//! process only from `drivers_dir`, a serial port only under `/dev`, any address only under `allow`,
//! and a port to listen on only if `listen` names it. See [`Limits`].

use crate::adapter::Failure;
use async_trait::async_trait;
use ferrum_ops_contract::driver::{Frame, MAX_FRAME, Tls};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc};

/// Connections a listener holds. Past this a caller is turned away, not queued.
const MAX_PEERS: usize = 1024;
/// A caller that has said nothing for this long is gone: a cable pulled sends no goodbye.
const IDLE: Duration = Duration::from_secs(3600);
/// Longest a Modbus device gets to answer. A device that never answers must not hold the bus.
const MODBUS_ANSWER: Duration = Duration::from_secs(5);

/// One message from the equipment. `topic` is an MQTT topic, an HTTP path, an OPC UA node, a Modbus
/// table and address (`holding/100`), or who is calling on a listener, and is empty on a stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub topic: String,
    pub body: Vec<u8>,
    /// An HTTP status, where there is one.
    pub status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Bytes(Vec<u8>),
    /// Bytes for one caller of a listener, known by the `topic` of what it said.
    To {
        peer: String,
        bytes: Vec<u8>,
    },
    Publish {
        topic: String,
        body: Vec<u8>,
        expiry: Option<u32>,
        retain: bool,
    },
    Subscribe(String),
    Http {
        method: String,
        path: String,
        body: Option<Vec<u8>>,
        headers: Vec<(String, String)>,
    },
    Read {
        table: String,
        at: u16,
        count: u16,
        unit: Option<u8>,
    },
    Write {
        values: Vec<(u16, u16)>,
        unit: Option<u8>,
    },
    /// OPC UA: reads a node, or writes it if there is a value.
    Node {
        node: String,
        value: Option<Value>,
    },
    Call {
        object: String,
        method: String,
        args: Vec<Value>,
    },
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Connects and stays connected, passing on what arrives. Returning means it ended. `ready`
    /// is told once the connection is up, so that a login goes out after it and not before.
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String>;

    /// Sends one thing. The reply comes back here where the protocol pairs them.
    async fn send(&self, out: Out) -> Result<Option<Message>, Failure>;

    /// Whether equipment calls in here, so that bytes need saying who they are for.
    fn listening(&self) -> bool {
        false
    }
}

/// What this site lets a definition open.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// Where external drivers live. Without it, nothing runs.
    pub drivers_dir: Option<PathBuf>,
    /// Hosts may start with one of these. Empty means any.
    pub allow: Vec<String>,
    /// Addresses a definition may listen on, exactly. Empty means none: opening a port on a
    /// gateway is the site's decision, not a definition's.
    pub listen: Vec<String>,
    /// Certificates a definition can name, and OPC UA's own store.
    pub tls_dir: Option<PathBuf>,
}

impl Limits {
    fn host_allowed(&self, host: &str) -> bool {
        self.allow.is_empty() || self.allow.iter().any(|a| host.starts_with(a.as_str()))
    }

    /// Whether an address may be reached, for what opens connections of its own.
    pub fn reaches(&self, address: &str) -> bool {
        address.split_once("://").is_some_and(|(_, rest)| self.host_allowed(host_of(rest)))
    }

    /// The program a `process://` address names, which must be a file in `drivers_dir`, however it
    /// is written: a definition from the platform must not run whatever it likes on a gateway.
    fn driver(&self, name: &str) -> Result<PathBuf, String> {
        let dir = self.drivers_dir.as_ref().ok_or("this site runs no external drivers: set drivers_dir")?;
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(name).canonicalize().map_err(|e| format!("{name}: {e}"))?;
        if !path.starts_with(&dir) {
            return Err(format!("{name} is outside the drivers directory"));
        }
        Ok(path)
    }

    /// A file a definition names, which must be in `tls_dir`, however it is written.
    fn certificate(&self, name: &str) -> Result<Vec<u8>, String> {
        let dir = self.tls_dir.as_ref().ok_or("this site keeps no certificates")?;
        let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(name).canonicalize().map_err(|e| format!("{name}: {e}"))?;
        if !path.starts_with(&dir) {
            return Err(format!("{name} is outside the certificates directory"));
        }
        std::fs::read(&path).map_err(|e| format!("{name}: {e}"))
    }
}

fn host_of(address: &str) -> &str {
    let tail = address.rsplit('@').next().unwrap_or(address);
    let tail = tail.split(['/', '?']).next().unwrap_or(tail);
    match tail.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => tail,
    }
}

/// Query parameters of an address, as `a=1&b=2`.
fn query(address: &str, key: &str) -> Option<String> {
    let (_, q) = address.split_once('?')?;
    q.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
}

/// A serial port's path, which must be one.
fn port_of(rest: &str) -> Result<&str, String> {
    let path = rest.split('?').next().unwrap_or(rest);
    if !path.starts_with("/dev/") || path.contains("..") {
        return Err(format!("{path} is not a serial port"));
    }
    Ok(path)
}

fn line_of(rest: &str) -> Result<serial::Line, String> {
    let number = |key: &str, default: u32| query(rest, key).map_or(Some(default), |v| v.parse().ok()).ok_or_else(|| format!("{key} is not a number"));
    Ok(serial::Line {
        baud: number("baud", 9600)?,
        bits: number("bits", 8)? as u8,
        parity: query(rest, "parity").and_then(|p| p.chars().next()).unwrap_or('N').to_ascii_uppercase(),
        stop: number("stop", 1)? as u8,
    })
}

/// What a connection needs besides its address.
#[derive(Debug, Default)]
pub struct Options<'a> {
    pub frame: Option<Frame>,
    pub subscribe: &'a [String],
    pub headers: &'a [(String, String)],
    pub tls: Option<&'a Tls>,
}

/// Opens what an address names, within what this site allows.
pub fn open(address: &str, options: Options<'_>, limits: &Limits) -> Result<Arc<dyn Transport>, String> {
    let Options { frame, subscribe, headers, tls } = options;
    let (scheme, rest) = address.split_once("://").ok_or_else(|| format!("{address} has no scheme"))?;
    let bind = rest.split('?').next().unwrap_or(rest);
    if matches!(scheme, "listen" | "listen+udp" | "webhook") {
        if !limits.listen.iter().any(|a| a == bind) {
            return Err(format!("{bind} is not in this site's listen list"));
        }
    } else if !matches!(scheme, "serial" | "process" | "modbus+rtu") && !limits.host_allowed(host_of(rest)) {
        return Err(format!("{} is not in this site's allow list", host_of(rest)));
    }
    Ok(match scheme {
        "tcp" => Arc::new(Stream::new(Opener::Tcp(rest.to_string()), frame)),
        "serial" => Arc::new(Stream::new(Opener::Serial(port_of(rest)?.to_string(), line_of(rest)?), frame)),
        "process" => {
            let mut words = rest.split_whitespace();
            let program = limits.driver(words.next().ok_or("name the driver")?)?;
            let args = words.map(str::to_string).collect();
            Arc::new(Stream::new(Opener::Process(program, args), Some(frame.unwrap_or(Frame::Line("\n".into())))))
        }
        "udp" => Arc::new(Udp { address: rest.to_string(), socket: Mutex::new(None) }),
        "listen" => Arc::new(Listen { address: bind.to_string(), frame, peers: Arc::default() }),
        "listen+udp" => Arc::new(ListenUdp { address: bind.to_string(), socket: Mutex::new(None) }),
        "webhook" => Arc::new(Webhook { address: bind.to_string(), token: query(rest, "token") }),
        "http" | "https" => Arc::new(Http::new(address, headers, tls, limits)?),
        #[cfg(feature = "ws")]
        "ws" | "wss" => {
            if tls.is_some() {
                return Err("a site's own certificates are for mqtts and https: use a public authority on wss".into());
            }
            Arc::new(Ws { address: address.to_string(), headers: headers.to_vec(), writer: Mutex::new(None) })
        }
        #[cfg(feature = "mqtt")]
        "mqtt" | "mqtts" => Arc::new(Mqtt::new(address, subscribe, tls, limits)?),
        #[cfg(feature = "modbus")]
        "modbus+tcp" => Arc::new(Modbus {
            target: ModbusTarget::Tcp(bind.to_string()),
            unit: query(rest, "unit").and_then(|u| u.parse().ok()).unwrap_or(1),
            conn: Mutex::new(None),
        }),
        #[cfg(feature = "modbus")]
        "modbus+rtu" => Arc::new(Modbus {
            target: ModbusTarget::Rtu(port_of(rest)?.to_string(), line_of(rest)?),
            unit: query(rest, "unit").and_then(|u| u.parse().ok()).unwrap_or(1),
            conn: Mutex::new(None),
        }),
        #[cfg(feature = "opcua")]
        "opcua" => Arc::new(super::opcua::Opcua::new(rest, subscribe, limits)?),
        other => return Err(format!("this build cannot open {other}")),
    })
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

enum Opener {
    Tcp(String),
    Serial(String, serial::Line),
    Process(PathBuf, Vec<String>),
}

/// TCP, a serial port and an external driver are all a stream of bytes both ways.
struct Stream {
    opener: Opener,
    frame: Option<Frame>,
    writer: Mutex<Option<Writer>>,
    /// Kept so that it dies with this connection, not after it.
    child: Mutex<Option<tokio::process::Child>>,
}

impl Stream {
    fn new(opener: Opener, frame: Option<Frame>) -> Stream {
        Stream { opener, frame, writer: Mutex::new(None), child: Mutex::new(None) }
    }

    async fn connect(&self) -> Result<(Reader, Writer), String> {
        match &self.opener {
            Opener::Tcp(address) => {
                let socket = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(address))
                    .await
                    .map_err(|_| "timed out connecting".to_string())?
                    .map_err(|e| e.to_string())?;
                let _ = socket.set_nodelay(true);
                let (r, w) = socket.into_split();
                Ok((Box::new(r), Box::new(w)))
            }
            Opener::Serial(path, line) => {
                let port = serial::open(Path::new(path), line).map_err(|e| format!("{path}: {e}"))?;
                let (r, w) = tokio::io::split(port);
                Ok((Box::new(r), Box::new(w)))
            }
            Opener::Process(program, args) => {
                let mut child = tokio::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("{}: {e}", program.display()))?;
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                if let Some(stderr) = child.stderr.take() {
                    let name = program.display().to_string();
                    tokio::spawn(async move {
                        use tokio::io::AsyncBufReadExt;
                        let mut lines = tokio::io::BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            tracing::warn!(driver = %name, "{line}");
                        }
                    });
                }
                *self.child.lock().await = Some(child);
                Ok((Box::new(stdout), Box::new(stdin)))
            }
        }
    }
}

/// Reads until the stream ends or nobody wants what it says, cutting it into messages. `Ok` is the
/// latter, and `Err` says how the stream ended.
async fn pump(mut reader: Reader, frame: Option<&Frame>, topic: &str, idle: Option<Duration>, tx: &mpsc::Sender<Message>) -> Result<(), String> {
    let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 8192]);
    let message = |body| Message { topic: topic.to_string(), body, status: None };
    loop {
        let read = reader.read(&mut chunk);
        let n = match idle {
            Some(idle) => tokio::time::timeout(idle, read).await.map_err(|_| "silent for too long".to_string())?,
            None => read.await,
        };
        let n = match n {
            Ok(0) => return Err("closed".to_string()),
            Ok(n) => n,
            Err(e) => return Err(e.to_string()),
        };
        buf.extend_from_slice(&chunk[..n]);
        match frame {
            Some(frame) => {
                while let Some(body) = frame.take(&mut buf) {
                    if tx.send(message(body)).await.is_err() {
                        return Ok(());
                    }
                }
                // A stream that never frames costs this much, and then starts again.
                if buf.len() > MAX_FRAME {
                    buf.clear();
                }
            }
            None => {
                if tx.send(message(std::mem::take(&mut buf))).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[async_trait]
impl Transport for Stream {
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let (reader, writer) = self.connect().await?;
        *self.writer.lock().await = Some(writer);
        let _ = ready.send(()).await;
        let ended = pump(reader, self.frame.as_ref(), "", None, &tx).await;
        if ended.is_ok() {
            return Ok(());
        }
        *self.writer.lock().await = None;
        *self.child.lock().await = None;
        ended
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        let Out::Bytes(bytes) = out else { return Err(Failure::NotSent("a stream takes bytes".into())) };
        let mut guard = self.writer.lock().await;
        let w = guard.as_mut().ok_or_else(|| Failure::NotSent("not connected".into()))?;
        // Once a write has started, some of it may be on the wire.
        match w.write_all(&bytes).await.and(w.flush().await) {
            Ok(()) => Ok(None),
            Err(e) => {
                *guard = None;
                Err(Failure::Unknown(e.to_string()))
            }
        }
    }
}

struct Udp {
    address: String,
    socket: Mutex<Option<Arc<UdpSocket>>>,
}

#[async_trait]
impl Transport for Udp {
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
        socket.connect(&self.address).await.map_err(|e| e.to_string())?;
        let socket = Arc::new(socket);
        *self.socket.lock().await = Some(socket.clone());
        let _ = ready.send(()).await;
        let mut buf = vec![0u8; 65536];
        loop {
            // A datagram refused by the far end is news, not the end of the connection.
            match socket.recv(&mut buf).await {
                Ok(n) => {
                    if tx.send(Message { body: buf[..n].to_vec(), ..Default::default() }).await.is_err() {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => tokio::time::sleep(Duration::from_millis(200)).await,
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        let Out::Bytes(bytes) = out else { return Err(Failure::NotSent("a datagram takes bytes".into())) };
        let socket = self.socket.lock().await.clone().ok_or_else(|| Failure::NotSent("not connected".into()))?;
        socket.send(&bytes).await.map(|_| None).map_err(|e| Failure::NotSent(e.to_string()))
    }
}

/// Equipment that calls in, as many trackers, terminals and cheap controllers do. Each caller is
/// known by its address, which is the `topic` of what it says, so an answer finds its way back.
struct Listen {
    address: String,
    frame: Option<Frame>,
    peers: Arc<Mutex<HashMap<String, tokio::net::tcp::OwnedWriteHalf>>>,
}

#[async_trait]
impl Transport for Listen {
    fn listening(&self) -> bool {
        true
    }

    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let listener = TcpListener::bind(&self.address).await.map_err(|e| format!("{}: {e}", self.address))?;
        let _ = ready.send(()).await;
        let mut callers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, peer) = accepted.map_err(|e| e.to_string())?;
                    if self.peers.lock().await.len() >= MAX_PEERS {
                        continue;
                    }
                    let _ = socket.set_nodelay(true);
                    let (reader, writer) = socket.into_split();
                    let peer = peer.to_string();
                    self.peers.lock().await.insert(peer.clone(), writer);
                    let (frame, tx, peers) = (self.frame.clone(), tx.clone(), self.peers.clone());
                    callers.spawn(async move {
                        let _ = pump(Box::new(reader), frame.as_ref(), &peer, Some(IDLE), &tx).await;
                        peers.lock().await.remove(&peer);
                    });
                }
                Some(_) = callers.join_next(), if !callers.is_empty() => {}
                () = tx.closed() => return Ok(()),
            }
        }
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        let Out::To { peer, bytes } = out else { return Err(Failure::NotSent("say who this is for".into())) };
        let mut peers = self.peers.lock().await;
        let writer = peers.get_mut(&peer).ok_or_else(|| Failure::NotSent(format!("{peer} has gone")))?;
        match writer.write_all(&bytes).await {
            Ok(()) => Ok(None),
            Err(e) => {
                peers.remove(&peer);
                Err(Failure::Unknown(e.to_string()))
            }
        }
    }
}

/// Datagrams from whoever sends them.
struct ListenUdp {
    address: String,
    socket: Mutex<Option<Arc<UdpSocket>>>,
}

#[async_trait]
impl Transport for ListenUdp {
    fn listening(&self) -> bool {
        true
    }

    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let socket = Arc::new(UdpSocket::bind(&self.address).await.map_err(|e| format!("{}: {e}", self.address))?);
        *self.socket.lock().await = Some(socket.clone());
        let _ = ready.send(()).await;
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, peer) = socket.recv_from(&mut buf).await.map_err(|e| e.to_string())?;
            if tx.send(Message { topic: peer.to_string(), body: buf[..n].to_vec(), status: None }).await.is_err() {
                return Ok(());
            }
        }
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        let Out::To { peer, bytes } = out else { return Err(Failure::NotSent("say who this is for".into())) };
        let socket = self.socket.lock().await.clone().ok_or_else(|| Failure::NotSent("not listening".into()))?;
        socket.send_to(&bytes, &peer).await.map(|_| None).map_err(|e| Failure::NotSent(e.to_string()))
    }
}

/// What other systems post here: a fleet manager's events, a scanner's reads. This is how something
/// that only knows how to call a URL reaches a site with no platform in sight.
struct Webhook {
    address: String,
    token: Option<String>,
}

#[derive(Clone)]
struct Hook {
    tx: mpsc::Sender<Message>,
    token: Option<Vec<u8>>,
}

async fn hook(
    axum::extract::State(s): axum::extract::State<Hook>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    use sha2::{Digest, Sha256};
    if let Some(want) = &s.token {
        let said = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
        // Hashed first, so the comparison says nothing about how much of it was right.
        if Sha256::digest(said) != Sha256::digest(want) {
            return StatusCode::UNAUTHORIZED;
        }
    }
    match s.tx.try_send(Message { topic: uri.path().to_string(), body: body.to_vec(), status: None }) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[async_trait]
impl Transport for Webhook {
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let listener = TcpListener::bind(&self.address).await.map_err(|e| format!("{}: {e}", self.address))?;
        let state = Hook { tx: tx.clone(), token: self.token.as_ref().map(|t| t.clone().into_bytes()) };
        let app = axum::Router::new().fallback(hook).layer(axum::extract::DefaultBodyLimit::max(MAX_FRAME)).with_state(state);
        let _ = ready.send(()).await;
        axum::serve(listener, app).with_graceful_shutdown(async move { tx.closed().await }).await.map_err(|e| e.to_string())
    }

    async fn send(&self, _: Out) -> Result<Option<Message>, Failure> {
        Err(Failure::NotSent("a webhook only listens".into()))
    }
}

struct Http {
    base: String,
    client: reqwest::Client,
    headers: Vec<(String, String)>,
}

impl Http {
    fn new(base: &str, headers: &[(String, String)], tls: Option<&Tls>, limits: &Limits) -> Result<Http, String> {
        let mut client = reqwest::Client::builder().timeout(Duration::from_secs(15)).connect_timeout(Duration::from_secs(5));
        if let Some(tls) = tls {
            if let Some(ca) = &tls.ca {
                let ca = reqwest::Certificate::from_pem(&limits.certificate(ca)?).map_err(|e| e.to_string())?;
                client = client.tls_built_in_root_certs(false).add_root_certificate(ca);
            }
            if let (Some(cert), Some(key)) = (&tls.cert, &tls.key) {
                let pem = [limits.certificate(cert)?, limits.certificate(key)?].concat();
                client = client.identity(reqwest::Identity::from_pem(&pem).map_err(|e| e.to_string())?);
            }
        }
        Ok(Http { base: base.trim_end_matches('/').to_string(), client: client.build().map_err(|e| e.to_string())?, headers: headers.to_vec() })
    }
}

#[async_trait]
impl Transport for Http {
    /// Nothing to hold open: every exchange stands alone.
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let _ = ready.send(()).await;
        tx.closed().await;
        Ok(())
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        let Out::Http { method, path, body, headers } = out else { return Err(Failure::NotSent("HTTP takes a method and path".into())) };
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| Failure::NotSent(e.to_string()))?;
        let mut request = self.client.request(method.clone(), format!("{}{path}", self.base));
        for (k, v) in self.headers.iter().chain(&headers) {
            request = request.header(k, v);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let response = match request.send().await {
            Ok(r) => r,
            // Refused means it went nowhere. After that, a request may have reached it.
            Err(e) if e.is_connect() => return Err(Failure::NotSent(e.to_string())),
            Err(e) if method == reqwest::Method::GET => return Err(Failure::NotSent(e.to_string())),
            Err(e) => return Err(Failure::Unknown(e.to_string())),
        };
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(|e| Failure::Unknown(e.to_string()))?.to_vec();
        Ok(Some(Message { topic: path, body, status: Some(status) }))
    }
}

#[cfg(feature = "ws")]
struct Ws {
    address: String,
    headers: Vec<(String, String)>,
    writer: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
}

#[cfg(feature = "ws")]
#[async_trait]
impl Transport for Ws {
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{self, Message as Frame, client::IntoClientRequest, http::HeaderName, http::HeaderValue};
        let mut request = self.address.as_str().into_client_request().map_err(|e| e.to_string())?;
        for (k, v) in &self.headers {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| e.to_string())?;
            request.headers_mut().insert(name, HeaderValue::from_str(v).map_err(|e| e.to_string())?);
        }
        let (socket, _) = tokio_tungstenite::connect_async(request).await.map_err(|e| e.to_string())?;
        let (mut sink, mut stream) = socket.split();
        let (out, mut outgoing) = mpsc::channel::<Vec<u8>>(64);
        *self.writer.lock().await = Some(out);
        let _ = ready.send(()).await;
        let ended = loop {
            tokio::select! {
                next = stream.next() => match next {
                    Some(Ok(Frame::Text(t))) => {
                        if tx.send(Message { body: t.as_bytes().to_vec(), ..Default::default() }).await.is_err() {
                            break "stopped".to_string();
                        }
                    }
                    Some(Ok(Frame::Binary(b))) => {
                        if tx.send(Message { body: b.to_vec(), ..Default::default() }).await.is_err() {
                            break "stopped".to_string();
                        }
                    }
                    Some(Ok(Frame::Ping(p))) => {
                        let _ = sink.send(Frame::Pong(p)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(tungstenite::Error::ConnectionClosed)) | None => break "closed".to_string(),
                    Some(Err(e)) => break e.to_string(),
                },
                bytes = outgoing.recv() => match bytes {
                    Some(b) => {
                        let text = String::from_utf8(b).map(|t| Frame::Text(t.into())).unwrap_or_else(|e| Frame::Binary(e.into_bytes().into()));
                        if let Err(e) = sink.send(text).await {
                            break e.to_string();
                        }
                    }
                    None => break "stopped".to_string(),
                },
            }
        };
        *self.writer.lock().await = None;
        Err(ended)
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        let Out::Bytes(bytes) = out else { return Err(Failure::NotSent("a WebSocket takes bytes".into())) };
        let writer = self.writer.lock().await.clone().ok_or_else(|| Failure::NotSent("not connected".into()))?;
        writer.send(bytes).await.map(|()| None).map_err(|e| Failure::Unknown(e.to_string()))
    }
}

#[cfg(feature = "mqtt")]
struct Mqtt {
    client: rumqttc::v5::AsyncClient,
    events: Mutex<rumqttc::v5::EventLoop>,
    subscribe: Vec<String>,
    connected: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "mqtt")]
impl Mqtt {
    /// `mqtt://user:password@host:port`, and `?client=` for the id a broker's rules expect.
    fn new(address: &str, subscribe: &[String], tls: Option<&Tls>, limits: &Limits) -> Result<Mqtt, String> {
        use rumqttc::v5::{AsyncClient, MqttOptions};
        let (scheme, rest) = address.split_once("://").ok_or("no scheme")?;
        let (auth, tail) = rest.rsplit_once('@').map_or((None, rest), |(a, t)| (Some(a), t));
        let server = tail.split(['/', '?']).next().unwrap_or(tail);
        let (host, port) = server.rsplit_once(':').map_or((server, 0), |(h, p)| (h, p.parse().unwrap_or(0)));
        let port = if port != 0 {
            port
        } else if scheme == "mqtts" {
            8883
        } else {
            1883
        };
        let client = query(rest, "client").unwrap_or_else(|| format!("ferrum-site-{}", uuid::Uuid::now_v7().simple()));
        let mut options = MqttOptions::new(client, host, port);
        options.set_keep_alive(Duration::from_secs(30));
        if let Some((user, password)) = auth.and_then(|a| a.split_once(':')) {
            options.set_credentials(user, password);
        }
        if scheme == "mqtts" {
            options.set_transport(match tls {
                Some(tls) => {
                    let ca = limits.certificate(tls.ca.as_deref().ok_or("mqtts with its own certificates needs ca")?)?;
                    let client_auth = match (&tls.cert, &tls.key) {
                        (Some(cert), Some(key)) => Some((limits.certificate(cert)?, limits.certificate(key)?)),
                        _ => None,
                    };
                    rumqttc::Transport::tls_with_config(rumqttc::TlsConfiguration::Simple { ca, alpn: None, client_auth })
                }
                None => rumqttc::Transport::tls_with_default_config(),
            });
        }
        let (client, events) = AsyncClient::new(options, 256);
        Ok(Mqtt { client, events: Mutex::new(events), subscribe: subscribe.to_vec(), connected: false.into() })
    }
}

#[cfg(feature = "mqtt")]
#[async_trait]
impl Transport for Mqtt {
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::Packet;
        use rumqttc::v5::{Event, Incoming};
        use std::sync::atomic::Ordering;
        let mut events = self.events.lock().await;
        loop {
            match events.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    self.connected.store(true, Ordering::Relaxed);
                    for topic in &self.subscribe {
                        self.client.subscribe(topic.clone(), QoS::AtLeastOnce).await.map_err(|e| e.to_string())?;
                    }
                    let _ = ready.send(()).await;
                }
                Ok(Event::Incoming(Incoming::Publish(p))) => {
                    let topic = String::from_utf8_lossy(&p.topic).to_string();
                    if tx.send(Message { topic, body: p.payload.to_vec(), status: None }).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    self.connected.store(false, Ordering::Relaxed);
                    return Err(e.to_string());
                }
            }
        }
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        use rumqttc::v5::mqttbytes::QoS;
        use rumqttc::v5::mqttbytes::v5::PublishProperties;
        use std::sync::atomic::Ordering;
        if !self.connected.load(Ordering::Relaxed) {
            return Err(Failure::NotSent("not connected to the broker".into()));
        }
        match out {
            Out::Publish { topic, body, expiry, retain } => {
                let properties = PublishProperties { message_expiry_interval: expiry, ..Default::default() };
                self.client
                    .publish_with_properties(topic, QoS::AtLeastOnce, retain, body, properties)
                    .await
                    .map(|()| None)
                    .map_err(|e| Failure::Unknown(e.to_string()))
            }
            Out::Subscribe(topic) => self.client.subscribe(topic, QoS::AtLeastOnce).await.map(|()| None).map_err(|e| Failure::NotSent(e.to_string())),
            _ => Err(Failure::NotSent("MQTT publishes to a topic: say which".into())),
        }
    }
}

#[cfg(feature = "modbus")]
enum ModbusTarget {
    Tcp(String),
    Rtu(String, serial::Line),
}

#[cfg(feature = "modbus")]
struct Modbus {
    target: ModbusTarget,
    unit: u8,
    conn: Mutex<Option<tokio_modbus::client::Context>>,
}

#[cfg(feature = "modbus")]
#[async_trait]
impl Transport for Modbus {
    /// Connects, and then waits: nothing arrives on Modbus unasked.
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        use tokio_modbus::slave::Slave;
        let context = match &self.target {
            ModbusTarget::Tcp(address) => {
                let address = tokio::net::lookup_host(address).await.map_err(|e| e.to_string())?.next().ok_or("no such host")?;
                tokio::time::timeout(Duration::from_secs(10), tokio_modbus::client::tcp::connect_slave(address, Slave(self.unit)))
                    .await
                    .map_err(|_| "timed out connecting".to_string())?
                    .map_err(|e| e.to_string())?
            }
            ModbusTarget::Rtu(path, line) => {
                let port = serial::open(Path::new(path), line).map_err(|e| format!("{path}: {e}"))?;
                tokio_modbus::client::rtu::attach_slave(port, Slave(self.unit))
            }
        };
        *self.conn.lock().await = Some(context);
        let _ = ready.send(()).await;
        loop {
            tokio::select! {
                () = tx.closed() => return Ok(()),
                () = tokio::time::sleep(Duration::from_millis(200)) => {
                    // `send` drops a connection that failed: come back here to make a new one.
                    if self.conn.lock().await.is_none() {
                        return Err("the connection was lost".into());
                    }
                }
            }
        }
    }

    async fn send(&self, out: Out) -> Result<Option<Message>, Failure> {
        use tokio::time::timeout;
        use tokio_modbus::client::{Reader, Writer};
        use tokio_modbus::slave::{Slave, SlaveContext};
        let mut guard = self.conn.lock().await;
        let context = guard.as_mut().ok_or_else(|| Failure::NotSent("not connected".into()))?;
        // Answers are matched to questions by order alone, so one that comes late would be taken
        // for the next: after a silence the connection starts again.
        let silent = || "it did not answer".to_string();
        match out {
            Out::Read { table, at, count, unit } => {
                context.set_slave(Slave(unit.unwrap_or(self.unit)));
                let words = |w: Vec<u16>| w.iter().flat_map(|x| x.to_be_bytes()).collect::<Vec<u8>>();
                let bits = |b: Vec<bool>| b.iter().map(|x| u8::from(*x)).collect::<Vec<u8>>();
                let read = timeout(MODBUS_ANSWER, async {
                    match table.as_str() {
                        "input" => context.read_input_registers(at, count).await.map(|r| r.map(words)),
                        "coil" => context.read_coils(at, count).await.map(|r| r.map(bits)),
                        "discrete" => context.read_discrete_inputs(at, count).await.map(|r| r.map(bits)),
                        _ => context.read_holding_registers(at, count).await.map(|r| r.map(words)),
                    }
                })
                .await;
                match read {
                    Ok(Ok(Ok(body))) => Ok(Some(Message { topic: format!("{table}/{at}"), body, status: None })),
                    Ok(Ok(Err(code))) => Err(Failure::NotSent(format!("the device refused: {code:?}"))),
                    Ok(Err(e)) => {
                        *guard = None;
                        Err(Failure::NotSent(e.to_string()))
                    }
                    Err(_) => {
                        *guard = None;
                        Err(Failure::NotSent(silent()))
                    }
                }
            }
            Out::Write { values, unit } => {
                context.set_slave(Slave(unit.unwrap_or(self.unit)));
                // The first write changes nothing on its own in a handshake, and the last is what
                // starts it. Once anything has gone, a lost answer leaves it unknown.
                let last = values.len().saturating_sub(1);
                for (i, (at, value)) in values.into_iter().enumerate() {
                    let Some(context) = guard.as_mut() else { return Err(Failure::NotSent("not connected".into())) };
                    match timeout(MODBUS_ANSWER, context.write_single_register(at, value)).await {
                        Ok(Ok(Ok(()))) => {}
                        Ok(Ok(Err(code))) if i == 0 => return Err(Failure::NotSent(format!("the device refused: {code:?}"))),
                        Ok(Ok(Err(code))) => return Err(Failure::Unknown(format!("the device refused part way: {code:?}"))),
                        Ok(Err(e)) => {
                            *guard = None;
                            return Err(if i == last { Failure::Unknown(e.to_string()) } else { Failure::NotSent(e.to_string()) });
                        }
                        Err(_) => {
                            *guard = None;
                            return Err(if i == last { Failure::Unknown(silent()) } else { Failure::NotSent(silent()) });
                        }
                    }
                }
                Ok(None)
            }
            _ => Err(Failure::NotSent("Modbus takes read and write".into())),
        }
    }
}

mod serial {
    //! A serial port with only what the C library offers: an open, a line discipline and readiness.
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};
    use tokio::io::unix::AsyncFd;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    #[derive(Debug)]
    pub struct Port(AsyncFd<File>);

    /// How a port is set: `?baud=19200&parity=E&stop=1&bits=8`.
    #[derive(Debug, Clone, Copy)]
    pub struct Line {
        pub baud: u32,
        pub bits: u8,
        /// `N`, `E` or `O`.
        pub parity: char,
        pub stop: u8,
    }

    fn speed(baud: u32) -> libc::speed_t {
        match baud {
            1200 => libc::B1200,
            2400 => libc::B2400,
            4800 => libc::B4800,
            19200 => libc::B19200,
            38400 => libc::B38400,
            57600 => libc::B57600,
            115200 => libc::B115200,
            230400 => libc::B230400,
            _ => libc::B9600,
        }
    }

    /// Raw, with no line discipline in the way.
    pub fn open(path: &Path, line: &Line) -> io::Result<Port> {
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path)?;
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is open and `tio` is ours to fill; each call only reads or sets that terminal.
        unsafe {
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut tio) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut tio);
            tio.c_cflag |= libc::CLOCAL | libc::CREAD;
            tio.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB);
            tio.c_cflag |= if line.bits == 7 { libc::CS7 } else { libc::CS8 };
            match line.parity {
                'E' => tio.c_cflag |= libc::PARENB,
                'O' => tio.c_cflag |= libc::PARENB | libc::PARODD,
                _ => {}
            }
            if line.stop == 2 {
                tio.c_cflag |= libc::CSTOPB;
            }
            libc::cfsetispeed(&mut tio, speed(line.baud));
            libc::cfsetospeed(&mut tio, speed(line.baud));
            if libc::tcsetattr(fd, libc::TCSANOW, &tio) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: the file belongs to the AsyncFd from here on, so its descriptor outlives the registration.
        Ok(Port(unsafe { AsyncFd::register(file) }.map_err(|e| e.into_parts().1)?))
    }

    impl AsyncRead for Port {
        fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            loop {
                let mut guard = ready!(self.0.poll_read_ready(cx))?;
                match guard.try_io(|fd| fd.get_ref().read(buf.initialize_unfilled())) {
                    Ok(Ok(n)) => {
                        buf.advance(n);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(e)) => return Poll::Ready(Err(e)),
                    Err(_) => continue,
                }
            }
        }
    }

    impl AsyncWrite for Port {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
            loop {
                let mut guard = ready!(self.0.poll_write_ready(cx))?;
                match guard.try_io(|fd| fd.get_ref().write(data)) {
                    Ok(result) => return Poll::Ready(result),
                    Err(_) => continue,
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_opens_only_what_the_site_allows() {
        let limits = Limits { allow: vec!["10.0.".into(), "plc-".into()], ..Default::default() };
        assert!(open("tcp://10.0.0.7:9100", Options::default(), &limits).is_ok());
        assert!(open("tcp://plc-3:502", Options::default(), &limits).is_ok());
        assert!(open("tcp://192.168.1.1:22", Options::default(), &limits).is_err_and(|e| e.contains("allow list")));
        assert!(open("http://user:pw@evil.example/x", Options::default(), &limits).is_err());
        assert!(open("process://anything", Options::default(), &limits).is_err_and(|e| e.contains("no external drivers")));
        assert!(open("serial:///etc/passwd", Options::default(), &limits).is_err_and(|e| e.contains("not a serial port")));
        assert!(open("serial:///dev/../etc/passwd", Options::default(), &limits).is_err());
        assert!(open("gopher://x", Options::default(), &Limits::default()).is_err());
    }

    #[test]
    fn a_definition_opens_a_port_only_if_the_site_said_so() {
        let limits = Limits { listen: vec!["0.0.0.0:5020".into()], ..Default::default() };
        assert!(open("listen://0.0.0.0:5020", Options::default(), &limits).is_ok());
        assert!(open("listen://0.0.0.0:5021", Options::default(), &limits).is_err_and(|e| e.contains("listen list")));
        assert!(open("webhook://0.0.0.0:80?token=x", Options::default(), &limits).is_err_and(|e| e.contains("listen list")));
        assert!(open("listen+udp://0.0.0.0:5020", Options::default(), &Limits::default()).is_err(), "and by default it never does");
    }

    #[test]
    fn a_certificate_a_definition_names_must_be_in_the_sites_store() {
        let dir = std::env::temp_dir().join(format!("ferrum-tls-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plant.pem"), "pem").unwrap();
        std::fs::write(dir.with_extension("secret"), "key").unwrap();
        let limits = Limits { tls_dir: Some(dir.clone()), ..Default::default() };
        assert_eq!(limits.certificate("plant.pem").unwrap(), b"pem");
        let outside = format!("../{}", dir.with_extension("secret").file_name().unwrap().to_string_lossy());
        assert!(limits.certificate(&outside).is_err_and(|e| e.contains("outside")));
        assert!(limits.certificate("/etc/passwd").is_err_and(|e| e.contains("outside")));
        assert!(Limits::default().certificate("plant.pem").is_err());
        let _ = std::fs::remove_file(dir.with_extension("secret"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_serial_line_says_how_it_is_set() {
        let line = line_of("/dev/ttyUSB0?baud=19200&parity=e&stop=2&bits=7").unwrap();
        assert_eq!((line.baud, line.bits, line.parity, line.stop), (19200, 7, 'E', 2));
        assert!(line_of("/dev/ttyUSB0?baud=fast").is_err());
        assert_eq!(port_of("/dev/ttyUSB0?baud=9600").unwrap(), "/dev/ttyUSB0");
        assert!(open("modbus+rtu:///etc/passwd", Options::default(), &Limits::default()).is_err_and(|e| e.contains("not a serial port")));
    }

    #[test]
    fn a_driver_must_be_in_the_drivers_directory() {
        let dir = std::env::temp_dir().join(format!("ferrum-drivers-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.sh"), "#!/bin/sh\n").unwrap();
        let limits = Limits { drivers_dir: Some(dir.clone()), ..Default::default() };
        assert!(limits.driver("ok.sh").is_ok());
        assert!(limits.driver("../../../bin/sh").is_err_and(|e| e.contains("outside") || e.contains("No such")));
        assert!(limits.driver("/bin/sh").is_err_and(|e| e.contains("outside")), "an absolute path is still a path");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_address_gives_up_its_host() {
        assert_eq!(host_of("10.0.0.7:9100"), "10.0.0.7");
        assert_eq!(host_of("user:pw@broker.local:1883/x?y=1"), "broker.local");
        assert_eq!(host_of("robot.local/api"), "robot.local");
    }
}
