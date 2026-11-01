//! The ways a site reaches equipment, all with one shape: a connection is run until it ends, what
//! arrives goes down a channel, and sending says whether the thing can have gone out, because that
//! decides whether a retry is safe (`crate::adapter`). What a message means is the definition's.
//!
//! A definition can come from the platform, so what it may open here is the site's to limit: a
//! process only from `drivers_dir`, a serial port only under `/dev`, and any address only under
//! `allow`. See [`Limits`].

use crate::adapter::Failure;
use async_trait::async_trait;
use ferrum_ops_contract::driver::{Frame, MAX_FRAME};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Mutex, mpsc};

/// One message from the equipment. `topic` is an MQTT topic, an HTTP path or a Modbus table and
/// address (`holding/100`), and is empty on a stream.
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
    Publish { topic: String, body: Vec<u8>, expiry: Option<u32> },
    Subscribe(String),
    Http { method: String, path: String, body: Option<Vec<u8>>, headers: Vec<(String, String)> },
    Read { table: String, at: u16, count: u16 },
    Write(Vec<(u16, u16)>),
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Connects and stays connected, passing on what arrives. Returning means it ended. `ready`
    /// is told once the connection is up, so that a login goes out after it and not before.
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String>;

    /// Sends one thing. The reply comes back here where the protocol pairs them.
    async fn send(&self, out: Out) -> Result<Option<Message>, Failure>;
}

/// What this site lets a definition open.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// Where external drivers live. Without it, nothing runs.
    pub drivers_dir: Option<PathBuf>,
    /// Hosts may start with one of these. Empty means any.
    pub allow: Vec<String>,
}

impl Limits {
    fn host_allowed(&self, host: &str) -> bool {
        self.allow.is_empty() || self.allow.iter().any(|a| host.starts_with(a.as_str()))
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

/// Opens what an address names, within what this site allows.
pub fn open(address: &str, frame: Option<Frame>, subscribe: &[String], headers: &[(String, String)], limits: &Limits) -> Result<Arc<dyn Transport>, String> {
    let (scheme, rest) = address.split_once("://").ok_or_else(|| format!("{address} has no scheme"))?;
    if !matches!(scheme, "serial" | "process") && !limits.host_allowed(host_of(rest)) {
        return Err(format!("{} is not in this site's allow list", host_of(rest)));
    }
    Ok(match scheme {
        "tcp" => Arc::new(Stream::new(Opener::Tcp(rest.to_string()), frame)),
        "serial" => {
            let path = rest.split('?').next().unwrap_or(rest);
            if !path.starts_with("/dev/") || path.contains("..") {
                return Err(format!("{path} is not a serial port"));
            }
            let baud = query(rest, "baud").and_then(|b| b.parse().ok()).unwrap_or(9600);
            Arc::new(Stream::new(Opener::Serial(path.to_string(), baud), frame))
        }
        "process" => {
            let mut words = rest.split_whitespace();
            let program = limits.driver(words.next().ok_or("name the driver")?)?;
            let args = words.map(str::to_string).collect();
            Arc::new(Stream::new(Opener::Process(program, args), Some(frame.unwrap_or(Frame::Line("\n".into())))))
        }
        "udp" => Arc::new(Udp { address: rest.to_string(), socket: Mutex::new(None) }),
        "http" | "https" => Arc::new(Http::new(address, headers)?),
        #[cfg(feature = "ws")]
        "ws" | "wss" => Arc::new(Ws { address: address.to_string(), headers: headers.to_vec(), writer: Mutex::new(None) }),
        #[cfg(feature = "mqtt")]
        "mqtt" | "mqtts" => Arc::new(Mqtt::new(address, subscribe)?),
        #[cfg(feature = "modbus")]
        "modbus+tcp" => Arc::new(Modbus {
            address: rest.split('?').next().unwrap_or(rest).to_string(),
            unit: query(rest, "unit").and_then(|u| u.parse().ok()).unwrap_or(1),
            conn: Mutex::new(None),
        }),
        other => return Err(format!("this build cannot open {other}")),
    })
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

enum Opener {
    Tcp(String),
    Serial(String, u32),
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
            Opener::Serial(path, baud) => {
                let port = serial::open(Path::new(path), *baud).map_err(|e| format!("{path}: {e}"))?;
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

#[async_trait]
impl Transport for Stream {
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        let (mut reader, writer) = self.connect().await?;
        *self.writer.lock().await = Some(writer);
        let _ = ready.send(()).await;
        let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 8192]);
        let ended = loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) => break "closed".to_string(),
                Ok(n) => n,
                Err(e) => break e.to_string(),
            };
            buf.extend_from_slice(&chunk[..n]);
            match &self.frame {
                Some(frame) => {
                    while let Some(body) = frame.take(&mut buf) {
                        if tx.send(Message { body, ..Default::default() }).await.is_err() {
                            return Ok(());
                        }
                    }
                    // A stream that never frames costs this much, and then starts again.
                    if buf.len() > MAX_FRAME {
                        buf.clear();
                    }
                }
                None => {
                    if tx.send(Message { body: std::mem::take(&mut buf), ..Default::default() }).await.is_err() {
                        return Ok(());
                    }
                }
            }
        };
        *self.writer.lock().await = None;
        *self.child.lock().await = None;
        Err(ended)
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

struct Http {
    base: String,
    client: reqwest::Client,
    headers: Vec<(String, String)>,
}

impl Http {
    fn new(base: &str, headers: &[(String, String)]) -> Result<Http, String> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).connect_timeout(Duration::from_secs(5)).build().map_err(|e| e.to_string())?;
        Ok(Http { base: base.trim_end_matches('/').to_string(), client, headers: headers.to_vec() })
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
    fn new(address: &str, subscribe: &[String]) -> Result<Mqtt, String> {
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
            options.set_transport(rumqttc::Transport::tls_with_default_config());
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
            Out::Publish { topic, body, expiry } => {
                let properties = PublishProperties { message_expiry_interval: expiry, ..Default::default() };
                self.client
                    .publish_with_properties(topic, QoS::AtLeastOnce, false, body, properties)
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
struct Modbus {
    address: String,
    unit: u8,
    conn: Mutex<Option<tokio_modbus::client::Context>>,
}

#[cfg(feature = "modbus")]
#[async_trait]
impl Transport for Modbus {
    /// Connects, and then waits: nothing arrives on Modbus unasked.
    async fn run(&self, tx: mpsc::Sender<Message>, ready: mpsc::Sender<()>) -> Result<(), String> {
        use tokio_modbus::slave::Slave;
        let address = tokio::net::lookup_host(&self.address).await.map_err(|e| e.to_string())?.next().ok_or("no such host")?;
        let context = tokio::time::timeout(Duration::from_secs(10), tokio_modbus::client::tcp::connect_slave(address, Slave(self.unit)))
            .await
            .map_err(|_| "timed out connecting".to_string())?
            .map_err(|e| e.to_string())?;
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
        use tokio_modbus::client::{Reader, Writer};
        let mut guard = self.conn.lock().await;
        let context = guard.as_mut().ok_or_else(|| Failure::NotSent("not connected".into()))?;
        match out {
            Out::Read { table, at, count } => {
                let words = |w: Vec<u16>| w.iter().flat_map(|x| x.to_be_bytes()).collect::<Vec<u8>>();
                let bits = |b: Vec<bool>| b.iter().map(|x| u8::from(*x)).collect::<Vec<u8>>();
                let read = match table.as_str() {
                    "input" => context.read_input_registers(at, count).await.map(|r| r.map(words)),
                    "coil" => context.read_coils(at, count).await.map(|r| r.map(bits)),
                    "discrete" => context.read_discrete_inputs(at, count).await.map(|r| r.map(bits)),
                    _ => context.read_holding_registers(at, count).await.map(|r| r.map(words)),
                };
                match read {
                    Ok(Ok(body)) => Ok(Some(Message { topic: format!("{table}/{at}"), body, status: None })),
                    Ok(Err(code)) => Err(Failure::NotSent(format!("the device refused: {code:?}"))),
                    Err(e) => {
                        *guard = None;
                        Err(Failure::NotSent(e.to_string()))
                    }
                }
            }
            Out::Write(values) => {
                // The first write changes nothing on its own in a handshake, and the last is what
                // starts it. Once anything has gone, a lost answer leaves it unknown.
                let last = values.len().saturating_sub(1);
                for (i, (at, value)) in values.into_iter().enumerate() {
                    let Some(context) = guard.as_mut() else { return Err(Failure::NotSent("not connected".into())) };
                    match context.write_single_register(at, value).await {
                        Ok(Ok(())) => {}
                        Ok(Err(code)) if i == 0 => return Err(Failure::NotSent(format!("the device refused: {code:?}"))),
                        Ok(Err(code)) => return Err(Failure::Unknown(format!("the device refused part way: {code:?}"))),
                        Err(e) => {
                            *guard = None;
                            return Err(if i == last { Failure::Unknown(e.to_string()) } else { Failure::NotSent(e.to_string()) });
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

    pub struct Port(AsyncFd<File>);

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

    /// Raw, eight bits, no parity, one stop bit. Anything else is a setting a device needs, which
    /// is when to add it.
    pub fn open(path: &Path, baud: u32) -> io::Result<Port> {
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
            libc::cfsetispeed(&mut tio, speed(baud));
            libc::cfsetospeed(&mut tio, speed(baud));
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
        let limits = Limits { drivers_dir: None, allow: vec!["10.0.".into(), "plc-".into()] };
        assert!(open("tcp://10.0.0.7:9100", None, &[], &[], &limits).is_ok());
        assert!(open("tcp://plc-3:502", None, &[], &[], &limits).is_ok());
        assert!(open("tcp://192.168.1.1:22", None, &[], &[], &limits).is_err_and(|e| e.contains("allow list")));
        assert!(open("http://user:pw@evil.example/x", None, &[], &[], &limits).is_err());
        assert!(open("process://anything", None, &[], &[], &limits).is_err_and(|e| e.contains("no external drivers")));
        assert!(open("serial:///etc/passwd", None, &[], &[], &limits).is_err_and(|e| e.contains("not a serial port")));
        assert!(open("serial:///dev/../etc/passwd", None, &[], &[], &limits).is_err());
        assert!(open("gopher://x", None, &[], &[], &Limits::default()).is_err());
    }

    #[test]
    fn a_driver_must_be_in_the_drivers_directory() {
        let dir = std::env::temp_dir().join(format!("ferrum-drivers-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.sh"), "#!/bin/sh\n").unwrap();
        let limits = Limits { drivers_dir: Some(dir.clone()), allow: vec![] };
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
