//! The adapters on the wire: real sockets, real packets, and each protocol's own way of going wrong.
//! The broker and the controller here are a few dozen lines that speak just enough of MQTT 5 and
//! Modbus TCP, so what is exercised is the client we ship. They are stand-ins: passing here says the
//! adapter speaks the protocol, not that any vendor's device agrees (docs/compatibility.md).

use chrono::{Duration, Utc};
use ferrum_ops_contract::Command;
use ferrum_ops_contract::edge::DeviceLink;
use ferrum_ops_contract::sync::SiteDevice;
use ferrum_site::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use ferrum_site::adapters::defined::Defined;
use ferrum_site::adapters::modbus::{Handshake, Modbus, ModbusConfig, Register};
use ferrum_site::adapters::mqtt::{Mqtt, MqttConfig, Subscription};
use ferrum_site::adapters::transport::Limits;
use ferrum_site::adapters::vda5050::{Vda, VdaConfig};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use uuid::Uuid;

fn command(operation: &str, payload: Value) -> Command {
    Command {
        id: Uuid::now_v7(),
        tenant_id: Uuid::nil(),
        site_id: Uuid::nil(),
        domain: "north".into(),
        target_id: Uuid::nil(),
        operation: operation.into(),
        schema_version: 1,
        issued_by: None,
        approval_ref: None,
        epoch: 1,
        expected_state: Value::Null,
        issued_at: Utc::now(),
        expires_at: Utc::now() + Duration::minutes(10),
        payload,
        payload_hash: String::new(),
        workflow_id: None,
        step_id: None,
        correlation_id: None,
        config_version: None,
    }
}

fn device(code: &str) -> SiteDevice {
    SiteDevice { id: Uuid::nil(), code: code.into(), kind: "Robot".into(), lifecycle: "Active".into(), operations: vec![], limits: BTreeMap::new() }
}

async fn until(what: &str, mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if done().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("gave up waiting for {what}");
}

// ───── just enough of an MQTT 5 broker ─────

#[derive(Clone)]
struct Published {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    expiry: Option<u32>,
}

#[derive(Default)]
struct Hub {
    clients: Vec<(Vec<String>, mpsc::UnboundedSender<Vec<u8>>)>,
    seen: Vec<Published>,
}

#[derive(Clone)]
struct Broker {
    port: u16,
    hub: Arc<Mutex<Hub>>,
}

async fn read_varint<R: AsyncRead + Unpin>(r: &mut R) -> Option<usize> {
    let (mut value, mut shift) = (0usize, 0);
    loop {
        let byte = r.read_u8().await.ok()?;
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

fn varint(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (n % 128) as u8;
        n /= 128;
        out.push(if n > 0 { byte | 0x80 } else { byte });
        if n == 0 {
            return out;
        }
    }
}

fn slice_varint(b: &[u8]) -> (usize, usize) {
    let (mut value, mut shift, mut used) = (0usize, 0, 0);
    for byte in b {
        used += 1;
        value |= usize::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }
    (value, used)
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![first];
    out.extend(varint(body.len()));
    out.extend_from_slice(body);
    out
}

fn publish_packet(topic: &str, payload: &[u8]) -> Vec<u8> {
    let mut body = (topic.len() as u16).to_be_bytes().to_vec();
    body.extend_from_slice(topic.as_bytes());
    body.push(0); // no properties
    body.extend_from_slice(payload);
    packet(0x30, &body)
}

fn filter_matches(filter: &str, topic: &str) -> bool {
    ferrum_site::adapters::mqtt::matches(filter, topic)
}

impl Broker {
    async fn start() -> Broker {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let broker = Broker { port: listener.local_addr().unwrap().port(), hub: Arc::default() };
        let hub = broker.hub.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(client(socket, hub.clone()));
            }
        });
        broker
    }

    /// What a device says.
    fn device_says(&self, topic: &str, payload: impl AsRef<[u8]>) {
        let hub = self.hub.lock().unwrap();
        for (filters, tx) in &hub.clients {
            if filters.iter().any(|f| filter_matches(f, topic)) {
                let _ = tx.send(publish_packet(topic, payload.as_ref()));
            }
        }
    }

    fn sent_to(&self, topic: &str) -> Vec<Published> {
        self.hub.lock().unwrap().seen.iter().filter(|p| p.topic == topic).cloned().collect()
    }

    fn subscribed(&self, filter: &str) -> bool {
        self.hub.lock().unwrap().clients.iter().any(|(f, _)| f.iter().any(|x| x == filter))
    }

    /// The network goes: every client is cut off, and may come back.
    fn cut_off(&self) {
        let mut hub = self.hub.lock().unwrap();
        for (_, tx) in hub.clients.drain(..) {
            let _ = tx.send(Vec::new());
        }
    }
}

async fn client(socket: tokio::net::TcpStream, hub: Arc<Mutex<Hub>>) {
    let (mut reader, mut writer) = socket.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let index = {
        let mut h = hub.lock().unwrap();
        h.clients.push((Vec::new(), tx.clone()));
        tx.clone()
    };
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            // Nothing to send means `cut_off`: dropping `writer` ends the connection.
            if bytes.is_empty() || writer.write_all(&bytes).await.is_err() {
                return;
            }
        }
    });
    drop(tx);
    let reply = |bytes: Vec<u8>| {
        let _ = index.send(bytes);
    };
    loop {
        let Ok(first) = reader.read_u8().await else { return };
        let Some(length) = read_varint(&mut reader).await else { return };
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        // Still connected? `cut_off` empties the list, and this client must then go too.
        if !hub.lock().unwrap().clients.iter().any(|(_, t)| t.same_channel(&index)) {
            return;
        }
        match first >> 4 {
            1 => reply(packet(0x20, &[0, 0, 0])),
            3 => {
                let qos = (first >> 1) & 3;
                let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
                let topic = String::from_utf8_lossy(&body[2..2 + topic_len]).to_string();
                let mut at = 2 + topic_len;
                if qos > 0 {
                    reply(packet(0x40, &body[at..at + 2]));
                    at += 2;
                }
                let (props, used) = slice_varint(&body[at..]);
                at += used;
                // 0x02 is the message expiry interval, four bytes.
                let expiry = (props >= 5 && body[at] == 0x02).then(|| u32::from_be_bytes([body[at + 1], body[at + 2], body[at + 3], body[at + 4]]));
                let payload = body[at + props..].to_vec();
                hub.lock().unwrap().seen.push(Published { topic, payload, qos, expiry });
            }
            8 => {
                let (props, used) = slice_varint(&body[2..]);
                let mut at = 2 + used + props;
                let mut filters = Vec::new();
                while at < body.len() {
                    let len = usize::from(u16::from_be_bytes([body[at], body[at + 1]]));
                    filters.push(String::from_utf8_lossy(&body[at + 2..at + 2 + len]).to_string());
                    at += 2 + len + 1;
                }
                let mut ack = body[..2].to_vec();
                ack.push(0);
                ack.extend(std::iter::repeat_n(1u8, filters.len()));
                {
                    let mut h = hub.lock().unwrap();
                    if let Some(c) = h.clients.iter_mut().find(|(_, t)| t.same_channel(&index)) {
                        c.0.extend(filters);
                    }
                }
                reply(packet(0x90, &ack));
            }
            12 => reply(vec![0xd0, 0]),
            14 => return,
            _ => {}
        }
    }
}

fn mqtt_config(port: u16) -> MqttConfig {
    MqttConfig {
        host: "127.0.0.1".into(),
        port,
        username: None,
        password: None,
        measurements: vec![Subscription {
            topic: "plant/+/temp".into(),
            device: "PRESS-1".into(),
            metric: "temp.c".into(),
            unit: Some("C".into()),
            field: Some("value".into()),
        }],
        command_topic: Some("plant/{device}/cmd".into()),
        status_topic: Some("plant/{device}/status".into()),
    }
}

#[tokio::test]
async fn mqtt_says_only_what_the_device_said() {
    let broker = Broker::start().await;
    let adapter = Mqtt::start("t", mqtt_config(broker.port));
    until("the status subscription", async || broker.subscribed("plant/+/status")).await;

    let c = command("machine.start_job", json!({"job": 412}));
    assert!(matches!(adapter.dispatch(&c, &device("PRESS-1")).await, Ok(Dispatched::Accepted { .. })));
    until("the command on the wire", async || !broker.sent_to("plant/PRESS-1/cmd").is_empty()).await;
    let sent = broker.sent_to("plant/PRESS-1/cmd").remove(0);
    let body: Value = serde_json::from_slice(&sent.payload).unwrap();
    assert_eq!(body["command"], json!(c.id));
    assert_eq!(body["payload"]["job"], json!(412));
    assert_eq!(sent.qos, 1, "at least once, because the command id makes a repeat harmless");
    assert!(sent.expiry.is_some_and(|s| s > 0 && s <= 600), "a broker must not deliver it after its deadline: {:?}", sent.expiry);

    // QoS says what the broker did. Only the device's own status moves the command.
    assert_eq!(adapter.observe(&c, None).await.unwrap(), Observation::Waiting);
    broker.device_says("plant/PRESS-1/status", json!({"command": c.id, "state": "accepted"}).to_string());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(adapter.observe(&c, None).await.unwrap(), Observation::Waiting, "accepted is not running");
    broker.device_says("plant/PRESS-1/status", json!({"command": c.id, "state": "running"}).to_string());
    until("running", async || adapter.observe(&c, None).await.unwrap() == Observation::Running).await;
    broker.device_says("plant/PRESS-1/status", json!({"command": Uuid::now_v7(), "state": "completed"}).to_string());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(adapter.observe(&c, None).await.unwrap(), Observation::Running, "another command's status is not this one's");
    broker.device_says("plant/PRESS-1/status", json!({"command": c.id, "state": "completed"}).to_string());
    until("completed", async || matches!(adapter.observe(&c, None).await.unwrap(), Observation::Completed(_))).await;

    // Measurements: what can be read is kept, and what cannot is not stored as something else.
    broker.device_says("plant/PRESS-1/temp", json!({"value": 21.5}).to_string());
    broker.device_says("plant/PRESS-1/temp", "not a number");
    let mut read = Vec::new();
    until("the reading", async || {
        read.extend(adapter.read().await.unwrap());
        !read.is_empty()
    })
    .await;
    assert_eq!(read.len(), 1);
    assert!(matches!(&read[0], Reading::Value { metric, value, .. } if metric == "temp.c" && *value == 21.5), "{read:?}");

    // The broker goes: nothing is claimed sent, and it comes back and subscribes again.
    broker.cut_off();
    until("the loss to be noticed", async || adapter.health() != "connected").await;
    let late = command("machine.start_job", json!({"job": 413}));
    assert!(matches!(adapter.dispatch(&late, &device("PRESS-1")).await, Err(Failure::NotSent(_))), "with no broker nothing can have gone");
    until("the link to return", async || broker.subscribed("plant/+/status")).await;
    until("connected again", async || adapter.health() == "connected").await;
    assert!(matches!(adapter.dispatch(&late, &device("PRESS-1")).await, Ok(Dispatched::Accepted { .. })));
    broker.device_says("plant/PRESS-1/status", json!({"command": late.id, "state": "failed", "why": "jam"}).to_string());
    until("the failure", async || matches!(adapter.observe(&late, None).await.unwrap(), Observation::Failed(_))).await;
}

#[tokio::test]
async fn a_read_only_mqtt_device_takes_no_commands() {
    let broker = Broker::start().await;
    let adapter = Mqtt::start("r", MqttConfig { command_topic: None, status_topic: None, ..mqtt_config(broker.port) });
    let c = command("machine.start_job", json!({}));
    assert!(matches!(adapter.dispatch(&c, &device("PRESS-1")).await, Ok(Dispatched::Rejected(_))));
}

fn vda_config(port: u16, version: &str) -> VdaConfig {
    VdaConfig { host: "127.0.0.1".into(), port, interface: "uagv".into(), version: version.into(), manufacturer: "acme".into(), username: None, password: None }
}

#[tokio::test]
async fn vda5050_follows_the_vehicle() {
    let broker = Broker::start().await;
    let adapter = Vda::start("v", vda_config(broker.port, "2.0.0"), vec!["AMR-1".into()]);
    until("the state subscription", async || broker.subscribed("uagv/v2/acme/AMR-1/connection")).await;
    let order = || json!({"nodes": [{"nodeId": "a"}], "edges": []});
    let c = command("warehouse.move", order());
    assert!(matches!(adapter.dispatch(&c, &device("AMR-1")).await, Err(Failure::NotSent(_))), "a vehicle not known to be online gets nothing");

    broker.device_says("uagv/v2/acme/AMR-1/connection", json!({"connectionState": "ONLINE"}).to_string());
    until("online", async || adapter.health().starts_with("1 of 1")).await;
    assert!(
        matches!(adapter.dispatch(&command("warehouse.move", json!({})), &device("AMR-1")).await, Ok(Dispatched::Rejected(_))),
        "an order needs nodes and edges"
    );
    assert!(matches!(adapter.dispatch(&c, &device("AMR-1")).await, Ok(Dispatched::Accepted { .. })));
    until("the order on the wire", async || !broker.sent_to("uagv/v2/acme/AMR-1/order").is_empty()).await;
    let sent: Value = serde_json::from_slice(&broker.sent_to("uagv/v2/acme/AMR-1/order")[0].payload).unwrap();
    assert_eq!(
        (sent["orderId"].as_str(), sent["version"].as_str(), sent["serialNumber"].as_str()),
        (Some(c.id.to_string().as_str()), Some("2.0.0"), Some("AMR-1"))
    );

    let id = c.id.to_string();
    let state = |header: i64, nodes: Value, driving: bool| {
        json!({"headerId": header, "orderId": id, "nodeStates": nodes, "edgeStates": [], "driving": driving, "actionStates": [], "errors": [], "batteryState": {"batteryCharge": 81.5}}).to_string()
    };
    broker.device_says("uagv/v2/acme/AMR-1/state", state(10, json!([{"nodeId": "a"}]), true));
    until("running", async || adapter.observe(&c, None).await.unwrap() == Observation::Running).await;
    broker.device_says("uagv/v2/acme/AMR-1/state", state(5, json!([]), false));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(adapter.observe(&c, None).await.unwrap(), Observation::Running, "a late message does not replace a newer one");
    let read = adapter.read().await.unwrap();
    assert!(read.iter().any(|r| matches!(r, Reading::Value { metric, value, .. } if metric == "battery.pct" && *value == 81.5)), "{read:?}");
    broker.device_says("uagv/v2/acme/AMR-1/state", state(11, json!([]), false));
    until("finished", async || matches!(adapter.observe(&c, None).await.unwrap(), Observation::Completed(_))).await;

    let c2 = command("warehouse.move", order());
    adapter.dispatch(&c2, &device("AMR-1")).await.unwrap();
    adapter.cancel(&c2, None).await.unwrap();
    until("the cancel on the wire", async || !broker.sent_to("uagv/v2/acme/AMR-1/instantActions").is_empty()).await;
    let cancel: Value = serde_json::from_slice(&broker.sent_to("uagv/v2/acme/AMR-1/instantActions")[0].payload).unwrap();
    assert_eq!(cancel["actions"][0]["actionType"], json!("cancelOrder"));
    assert_eq!(adapter.observe(&c2, None).await.unwrap(), Observation::Waiting, "asking is not stopping");
    let fatal = json!({"headerId": 12, "orderId": c2.id.to_string(), "nodeStates": [{"nodeId": "a"}], "edgeStates": [], "driving": false, "actionStates": [], "errors": [{"errorLevel": "FATAL", "errorType": "x"}]});
    broker.device_says("uagv/v2/acme/AMR-1/state", fatal.to_string());
    until("the failure", async || matches!(adapter.observe(&c2, None).await.unwrap(), Observation::Failed(_))).await;
}

#[tokio::test]
async fn a_vda5050_version_we_did_not_write_for_is_refused() {
    let broker = Broker::start().await;
    let adapter = Vda::start("o", vda_config(broker.port, "3.0.0"), vec!["AMR-1".into()]);
    let c = command("warehouse.move", json!({"nodes": [], "edges": []}));
    assert!(matches!(adapter.dispatch(&c, &device("AMR-1")).await, Ok(Dispatched::Rejected(why)) if why.contains("3.0.0")));
}

// ───── just enough of a Modbus TCP controller ─────

#[derive(Default)]
struct Plc {
    registers: HashMap<u16, u16>,
    writes: Vec<(u16, u16)>,
    /// A write to this register reaches it and is never answered.
    silent_on: Option<u16>,
}

async fn controller(plc: Arc<Mutex<Plc>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let plc = plc.clone();
            tokio::spawn(async move {
                let mut header = [0u8; 7];
                while socket.read_exact(&mut header).await.is_ok() {
                    let length = usize::from(u16::from_be_bytes([header[4], header[5]])) - 1;
                    let mut pdu = vec![0; length];
                    if socket.read_exact(&mut pdu).await.is_err() {
                        return;
                    }
                    let register = u16::from_be_bytes([pdu[1], pdu[2]]);
                    let word = u16::from_be_bytes([pdu[3], pdu[4]]);
                    let answer = match pdu[0] {
                        3 => {
                            let mut a = vec![3, (word * 2) as u8];
                            for r in register..register + word {
                                a.extend(plc.lock().unwrap().registers.get(&r).copied().unwrap_or(0).to_be_bytes());
                            }
                            a
                        }
                        6 => {
                            let mut p = plc.lock().unwrap();
                            p.registers.insert(register, word);
                            p.writes.push((register, word));
                            if p.silent_on == Some(register) {
                                return;
                            }
                            pdu.clone()
                        }
                        function => vec![function | 0x80, 1],
                    };
                    let mut out = header[..4].to_vec();
                    out.extend(((answer.len() + 1) as u16).to_be_bytes());
                    out.push(header[6]);
                    out.extend(answer);
                    if socket.write_all(&out).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    address
}

fn modbus_config(address: std::net::SocketAddr) -> ModbusConfig {
    ModbusConfig {
        address,
        unit: 1,
        registers: vec![Register { device: "PRESS-1".into(), metric: "count".into(), address: 200, scale: 0.5, unit: None }],
        operations: HashMap::from([(
            "machine.start_job".to_string(),
            Handshake { job_register: 100, start_register: 101, status_register: 102, running: 1, done: 2, fault: 3, idle: 0 },
        )]),
    }
}

#[tokio::test]
async fn modbus_writes_only_the_handshake_it_was_given() {
    let plc: Arc<Mutex<Plc>> = Arc::default();
    let adapter = Modbus::new(modbus_config(controller(plc.clone()).await));
    let c = command("machine.start_job", json!({"job": 412}));
    assert!(matches!(adapter.dispatch(&c, &device("PRESS-1")).await, Ok(Dispatched::Accepted { .. })));
    assert_eq!(plc.lock().unwrap().writes, vec![(100, 412), (101, 1)], "the job first: nothing has happened until the start register");

    assert_eq!(adapter.observe(&c, None).await.unwrap(), Observation::Waiting);
    plc.lock().unwrap().registers.insert(102, 1);
    assert_eq!(adapter.observe(&c, None).await.unwrap(), Observation::Running);
    plc.lock().unwrap().registers.insert(102, 2);
    assert!(matches!(adapter.observe(&c, None).await.unwrap(), Observation::Completed(_)));

    plc.lock().unwrap().registers.insert(200, 10);
    let read = adapter.read().await.unwrap();
    assert!(matches!(&read[0], Reading::Value { metric, value, .. } if metric == "count" && *value == 5.0), "{read:?}");

    let writes = plc.lock().unwrap().writes.len();
    assert!(
        matches!(adapter.dispatch(&command("door.force", json!({"job": 1})), &device("PRESS-1")).await, Ok(Dispatched::Rejected(_))),
        "only what the site mapped"
    );
    assert!(
        matches!(adapter.dispatch(&command("machine.start_job", json!({"job": 70000})), &device("PRESS-1")).await, Ok(Dispatched::Rejected(_))),
        "a job number that does not fit"
    );
    assert!(matches!(adapter.dispatch(&command("machine.start_job", json!({})), &device("PRESS-1")).await, Ok(Dispatched::Rejected(_))));
    assert_eq!(plc.lock().unwrap().writes.len(), writes, "and none of those wrote anything");
    assert!(adapter.cancel(&c, None).await.is_err(), "a stop over Modbus would look like a stop without being one");
}

#[tokio::test]
async fn an_unanswered_start_is_not_known_to_have_failed() {
    let plc: Arc<Mutex<Plc>> = Arc::default();
    plc.lock().unwrap().silent_on = Some(101);
    let adapter = Modbus::new(modbus_config(controller(plc.clone()).await));
    let c = command("machine.start_job", json!({"job": 412}));
    let answer = adapter.dispatch(&c, &device("PRESS-1")).await;
    assert!(matches!(answer, Err(Failure::Unknown(_))), "it may have started, and nothing here can tell: {answer:?}");

    // Before the start register nothing can have happened, so that much is known.
    plc.lock().unwrap().silent_on = Some(100);
    let adapter = Modbus::new(modbus_config(controller(plc.clone()).await));
    let answer = adapter.dispatch(&c, &device("PRESS-1")).await;
    assert!(matches!(answer, Err(Failure::NotSent(_))), "{answer:?}");

    let nobody = Modbus::new(modbus_config("127.0.0.1:9".parse().unwrap()));
    assert!(matches!(nobody.dispatch(&c, &device("PRESS-1")).await, Err(Failure::NotSent(_))), "no connection, nothing sent");
}

// ───── any of them by a definition ─────

fn defined(definition: &str, code: &str, connect: Option<String>) -> Defined {
    let d = Defined::new(Limits::default(), json!({"pw": "s3cret"}));
    assert_eq!(apply(&d, definition, &[(code, connect)]), Vec::<String>::new());
    d
}

fn apply(d: &Defined, definition: &str, devices: &[(&str, Option<String>)]) -> Vec<String> {
    let definitions = BTreeMap::from([("it".to_string(), definition.to_string())]);
    let links: Vec<DeviceLink> = devices
        .iter()
        .map(|(code, connect)| DeviceLink {
            code: code.to_string(),
            definition: "it".into(),
            connect: connect.clone(),
            settings: json!({"unit": 3}),
            concurrency: 1,
        })
        .collect();
    d.apply(&definitions, &links)
}

async fn reading(d: &Defined, want: impl Fn(&Reading) -> bool) -> Reading {
    let mut got = None;
    until("a reading", async || {
        got = d.read().await.unwrap().into_iter().find(|r| want(r));
        got.is_some()
    })
    .await;
    got.unwrap()
}

fn is_metric(r: &Reading, name: &str, want: f64) -> bool {
    matches!(r, Reading::Value { metric, value, .. } if metric == name && (*value - want).abs() < 1e-9)
}

#[tokio::test]
async fn a_scale_on_a_tcp_line() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = Arc::new(Mutex::new(0));
    let count = served.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            *count.lock().unwrap() += 1;
            tokio::spawn(async move {
                let (r, mut w) = socket.into_split();
                let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(r));
                while let Ok(Some(line)) = lines.next_line().await {
                    let answer = match line.trim() {
                        "W" => "12.5 kg\r\n",
                        "T" => "OK\r\n",
                        "HANG" => return,
                        _ => "ERR what\r\n",
                    };
                    if w.write_all(answer.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let d = defined(
        r#"
name: scale
frame: {line: "\r\n"}
poll:
  - {every: 50ms, send: "W\r\n"}
read:
  - {match: '^(?P<kg>-?[0-9.]+) kg$', metric: weight.kg, group: kg}
commands:
  scale.tare:
    send: "T\r\n"
    status: {match: '^(?P<r>OK|ERR)', group: r, done: OK, failed: ERR}
  scale.nonsense:
    send: "Q\r\n"
    status: {match: '^(?P<r>OK|ERR)', group: r, done: OK, failed: ERR}
  scale.hang:
    send: "HANG\r\n"
    once: true
"#,
        "SCALE-1",
        Some(format!("tcp://{address}")),
    );
    assert!(d.reaches("SCALE-1"));
    reading(&d, |r| is_metric(r, "weight.kg", 12.5)).await;

    let c = command("scale.tare", json!({}));
    assert!(matches!(d.dispatch(&c, &device("SCALE-1")).await, Ok(Dispatched::Accepted { .. })));
    until("the tare", async || matches!(d.observe(&c, None).await.unwrap(), Observation::Completed(_))).await;
    let bad = command("scale.nonsense", json!({}));
    d.dispatch(&bad, &device("SCALE-1")).await.unwrap();
    until("the refusal", async || matches!(d.observe(&bad, None).await.unwrap(), Observation::Failed(_))).await;

    let unknown = command("scale.dance", json!({}));
    assert!(
        matches!(d.dispatch(&unknown, &device("SCALE-1")).await, Ok(Dispatched::Rejected(why)) if why.contains("scale.dance")),
        "only what its definition says"
    );

    // `once` proves the send and says that is all it proves.
    let once = command("scale.hang", json!({}));
    d.dispatch(&once, &device("SCALE-1")).await.unwrap();
    assert!(matches!(d.observe(&once, None).await.unwrap(), Observation::Completed(e) if e["proved"].as_str().unwrap().contains("nothing beyond")));

    // That closed the connection: it comes back by itself, and readings with it.
    d.read().await.unwrap();
    reading(&d, |r| is_metric(r, "weight.kg", 12.5)).await;
    assert!(*served.lock().unwrap() >= 2, "reconnected");
    let health = d.links();
    assert_eq!((health.len(), health[0].definition.as_str()), (1, "scale"));
    assert!(health[0].reconnects >= 1, "{health:?}");
}

#[tokio::test]
async fn a_datagram_each_way() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        while let Ok((_, from)) = socket.recv_from(&mut buf).await {
            let _ = socket.send_to(&[0x5a, 0x01, 0x2c], from).await;
        }
    });
    let d = defined(
        "name: dgram\npoll:\n  - {every: 50ms, hex: '01 02'}\nread:\n  - {when: {at: 0, is: 5a}, metric: level, at: 1, type: u16}\n",
        "TANK-1",
        Some(format!("udp://{address}")),
    );
    reading(&d, |r| is_metric(r, "level", 300.0)).await;
}

#[tokio::test]
async fn a_robot_over_rest_as_its_own_mission_api() {
    use axum::Json;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{delete, get, post};
    #[derive(Clone, Default)]
    struct Fleet(Arc<Mutex<HashMap<String, String>>>);
    let app = axum::Router::new()
        .route("/status", get(|| async { Json(json!({"battery_percentage": 81.25, "state_text": "Executing", "firmware": "3.4"})) }))
        .route(
            "/missions",
            post(|State(f): State<Fleet>, Json(b): Json<Value>| async move {
                if b["to"] == json!("NOWHERE") {
                    return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error": "no such place"})));
                }
                if b["to"] == json!("BROKEN") {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
                }
                f.0.lock().unwrap().insert("m-7".into(), "Pending".into());
                (StatusCode::CREATED, Json(json!({"id": "m-7", "state": "Pending"})))
            }),
        )
        .route(
            "/missions/{id}",
            get(|State(f): State<Fleet>, Path(id): Path<String>| async move {
                let state = f.0.lock().unwrap().get(&id).cloned();
                Json(json!({"id": id, "state": state}))
            }),
        )
        .route(
            "/queue/{id}",
            delete(|State(f): State<Fleet>, Path(id): Path<String>| async move {
                f.0.lock().unwrap().insert(id, "Aborted".into());
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/advance/{state}",
            post(|State(f): State<Fleet>, Path(state): Path<String>| async move {
                f.0.lock().unwrap().insert("m-7".into(), state);
                StatusCode::NO_CONTENT
            }),
        )
        .with_state(Fleet::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    let d = defined(
        r#"
name: rest-robot
headers: {Authorization: "Bearer {secret.pw}"}
poll:
  - {every: 50ms, get: /status}
read:
  - topic: /status
    values:
      - {metric: battery.pct, json: $.battery_percentage, unit: "%"}
      - {state: true, json: $.state_text, map: {Executing: Running, Ready: Idle}}
      - {attribute: firmware, json: $.firmware}
commands:
  robot.go:
    post: /missions
    send: {to: "{payload.to}", by: "{device}"}
    id: $.id
    status:
      poll: {get: "/missions/{id}"}
      json: $.state
      running: [Pending, Executing]
      done: Done
      failed: Error
      aborted: Aborted
    cancel: {delete: "/queue/{id}"}
"#,
        "AMR-1",
        Some(format!("http://{address}")),
    );
    let got = reading(&d, |r| is_metric(r, "battery.pct", 81.25)).await;
    assert!(matches!(got, Reading::Value { ref unit, .. } if unit.as_deref() == Some("%")));
    reading(&d, |r| matches!(r, Reading::State { state, .. } if state == "Running")).await;
    reading(&d, |r| matches!(r, Reading::Attribute { key, .. } if key == "firmware")).await;

    let no = command("robot.go", json!({"to": "NOWHERE"}));
    assert!(matches!(d.dispatch(&no, &device("AMR-1")).await, Ok(Dispatched::Rejected(why)) if why.contains("422")), "it said no: nothing happened");
    let broke = command("robot.go", json!({"to": "BROKEN"}));
    assert!(matches!(d.dispatch(&broke, &device("AMR-1")).await, Err(Failure::Unknown(_))), "it broke while reading it: it may have happened");
    let missing = command("robot.go", json!({}));
    assert!(
        matches!(d.dispatch(&missing, &device("AMR-1")).await, Ok(Dispatched::Rejected(why)) if why.contains("payload.to")),
        "half a command does not leave"
    );

    let c = command("robot.go", json!({"to": "A-1"}));
    let Ok(Dispatched::Accepted { external }) = d.dispatch(&c, &device("AMR-1")).await else { panic!("accepted") };
    assert_eq!(external.as_deref(), Some("m-7"), "its own name for the work");
    assert_eq!(d.observe(&c, external.as_deref()).await.unwrap(), Observation::Running);
    let http = reqwest::Client::new();
    http.post(format!("http://{address}/advance/Done")).send().await.unwrap();
    assert!(matches!(d.observe(&c, external.as_deref()).await.unwrap(), Observation::Completed(e) if e["id"] == json!("m-7")), "the evidence is what it said");

    // A restart: the journal has the name, and this link has never heard of the command.
    let again = defined_like(&d, &address).await;
    http.post(format!("http://{address}/advance/Executing")).send().await.unwrap();
    let c2 = command("robot.go", json!({"to": "A-1"}));
    assert_eq!(again.observe(&c2, Some("m-7")).await.unwrap(), Observation::Running);
    again.cancel(&c2, Some("m-7")).await.unwrap();
    assert!(matches!(again.observe(&c2, Some("m-7")).await.unwrap(), Observation::Aborted(_)));
}

async fn defined_like(_: &Defined, address: &std::net::SocketAddr) -> Defined {
    defined(
        r#"
name: rest-robot
commands:
  robot.go:
    post: /missions
    send: {to: "{payload.to}"}
    id: $.id
    status:
      poll: {get: "/missions/{id}"}
      json: $.state
      running: [Pending, Executing]
      done: Done
      aborted: Aborted
    cancel: {delete: "/queue/{id}"}
"#,
        "AMR-1",
        Some(format!("http://{address}")),
    )
}

#[tokio::test]
async fn many_devices_on_one_broker_and_a_stranger_is_noticed() {
    let broker = Broker::start().await;
    let d = Defined::new(Limits::default(), json!({}));
    let definition = r#"
name: sensors
subscribe: ["plant/+/telemetry", "plant/+/ack"]
read:
  - topic: plant/+/telemetry
    device: "{topic.1}"
    values:
      - {metric: temp.c, json: $.temp, deadband: 1.0}
      - {state: true, json: $.mode, map: {1: Running, 0: Idle}}
commands:
  valve.open:
    topic: "plant/{device}/cmd"
    send: {op: open, id: "{command}"}
    status: {topic: "plant/{device}/ack", id: $.id, json: $.state, running: started, done: opened, failed: stuck}
"#;
    let address = format!("mqtt://127.0.0.1:{}", broker.port);
    let problems = apply(&d, definition, &[("V-1", Some(address.clone())), ("V-2", Some(address.clone()))]);
    assert_eq!(problems, Vec::<String>::new());
    assert_eq!(d.links().len(), 1, "two devices, one connection");
    until("the subscription", async || broker.subscribed("plant/+/telemetry")).await;
    until("the link", async || d.links()[0].up).await;

    broker.device_says("plant/V-1/telemetry", json!({"temp": 20.0, "mode": 1}).to_string());
    broker.device_says("plant/V-2/telemetry", json!({"temp": 30.0, "mode": 0}).to_string());
    broker.device_says("plant/V-9/telemetry", json!({"temp": 40.0}).to_string());
    let mut got: Vec<Reading> = Vec::new();
    until("both devices", async || {
        got.extend(d.read().await.unwrap());
        got.len() >= 4
    })
    .await;
    assert!(got.iter().any(|r| matches!(r, Reading::Value { device, value, .. } if device == "V-1" && *value == 20.0)), "{got:?}");
    assert!(got.iter().any(|r| matches!(r, Reading::Value { device, value, .. } if device == "V-2" && *value == 30.0)), "{got:?}");
    assert!(!got.iter().any(|r| matches!(r, Reading::Value { device, .. } if device == "V-9")), "nobody claimed that one");
    assert_eq!(d.strangers(), vec!["V-9".to_string()], "but it was noticed");

    // A number inside its deadband is not news.
    broker.device_says("plant/V-1/telemetry", json!({"temp": 20.4}).to_string());
    broker.device_says("plant/V-1/telemetry", json!({"temp": 25.0}).to_string());
    let moved = reading(&d, |r| matches!(r, Reading::Value { .. })).await;
    assert!(is_metric(&moved, "temp.c", 25.0), "{moved:?}");

    let c = command("valve.open", json!({}));
    d.dispatch(&c, &device("V-2")).await.unwrap();
    until("the command", async || !broker.sent_to("plant/V-2/cmd").is_empty()).await;
    let sent = broker.sent_to("plant/V-2/cmd").remove(0);
    assert!(sent.expiry.is_some(), "a broker must not deliver it after its deadline");
    assert_eq!(serde_json::from_slice::<Value>(&sent.payload).unwrap()["id"], json!(c.id.to_string()));
    broker.device_says("plant/V-2/ack", json!({"id": Uuid::now_v7(), "state": "opened"}).to_string());
    broker.device_says("plant/V-2/ack", json!({"id": c.id, "state": "started"}).to_string());
    until("started", async || d.observe(&c, None).await.unwrap() == Observation::Running).await;
    broker.device_says("plant/V-2/ack", json!({"id": c.id, "state": "opened"}).to_string());
    until("opened", async || matches!(d.observe(&c, None).await.unwrap(), Observation::Completed(_))).await;
    broker.device_says("plant/V-2/ack", json!({"id": c.id, "state": "started"}).to_string());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(matches!(d.observe(&c, None).await.unwrap(), Observation::Completed(_)), "a late word does not reopen what ended");
}

#[tokio::test]
async fn a_plc_by_registers_alone() {
    let plc: Arc<Mutex<Plc>> = Arc::default();
    let address = controller(plc.clone()).await;
    plc.lock().unwrap().registers.extend([(100, (-125i16) as u16), (101, 0x41ac), (102, 0x0000), (103, 7)]);
    let d = defined(
        r#"
name: press
poll:
  - {every: 50ms, read: holding, at: 100, count: 4}
read:
  - topic: holding/100
    values:
      - {metric: temp.c, reg: 0, type: i16, scale: 0.1}
      - {metric: pressure, reg: 1, type: f32}
      - {metric: count, reg: 3}
commands:
  machine.start_job:
    write:
      - {at: 200, value: "{payload.job}"}
      - {at: 201, value: 1}
    status:
      poll: {read: holding, at: 202, count: 1}
      at: 0
      type: u16
      running: 1
      done: 2
      failed: 3
"#,
        "PRESS-1",
        Some(format!("modbus+tcp://{address}?unit=1")),
    );
    reading(&d, |r| is_metric(r, "temp.c", -12.5)).await;
    reading(&d, |r| is_metric(r, "pressure", 21.5)).await;
    reading(&d, |r| is_metric(r, "count", 7.0)).await;
    let c = command("machine.start_job", json!({"job": 412}));
    assert!(matches!(d.dispatch(&c, &device("PRESS-1")).await, Ok(Dispatched::Accepted { .. })));
    assert_eq!(plc.lock().unwrap().writes, vec![(200, 412), (201, 1)], "the job first, and only those");
    assert_eq!(d.observe(&c, None).await.unwrap(), Observation::Waiting);
    plc.lock().unwrap().registers.insert(202, 1);
    assert_eq!(d.observe(&c, None).await.unwrap(), Observation::Running);
    plc.lock().unwrap().registers.insert(202, 2);
    assert!(matches!(d.observe(&c, None).await.unwrap(), Observation::Completed(_)));
    let too_big = command("machine.start_job", json!({"job": 70000}));
    assert!(matches!(d.dispatch(&too_big, &device("PRESS-1")).await, Ok(Dispatched::Rejected(why)) if why.contains("fit a register")));

    plc.lock().unwrap().silent_on = Some(201);
    let lost = command("machine.start_job", json!({"job": 9}));
    assert!(matches!(d.dispatch(&lost, &device("PRESS-1")).await, Err(Failure::Unknown(_))), "the start went unanswered: it may have started");
}

#[tokio::test]
async fn a_web_socket_as_rosbridge_speaks_it() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Frame;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let said: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = said.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                while let Some(Ok(Frame::Text(t))) = ws.next().await {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap();
                    log.lock().unwrap().push(v.clone());
                    let answer = match v["op"].as_str() {
                        Some("subscribe") => json!({"op": "publish", "topic": "/battery_state", "msg": {"percentage": 0.5}}),
                        Some("send_action_goal") => json!({"op": "action_result", "id": v["id"], "values": {"status": 4}, "result": true}),
                        _ => continue,
                    };
                    if ws.send(Frame::Text(answer.to_string().into())).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let d = defined(
        r#"
name: ros2
on_connect:
  - send: {op: subscribe, topic: /battery_state, type: sensor_msgs/msg/BatteryState}
read:
  - when: {json: $.topic, is: /battery_state}
    metric: battery.pct
    json: $.msg.percentage
    scale: 100
commands:
  robot.navigate:
    send: {op: send_action_goal, id: "{command}", action: /navigate_to_pose, action_type: nav2_msgs/action/NavigateToPose, args: {pose: "{payload.pose}"}}
    status: {id: $.id, json: $.values.status, running: 2, done: 4, aborted: 5, failed: 6}
"#,
        "AMR-1",
        Some(format!("ws://{address}")),
    );
    reading(&d, |r| is_metric(r, "battery.pct", 50.0)).await;
    let c = command("robot.navigate", json!({"pose": {"x": 1.5}}));
    d.dispatch(&c, &device("AMR-1")).await.unwrap();
    until("the goal's end", async || matches!(d.observe(&c, None).await.unwrap(), Observation::Completed(_))).await;
    let goal = said.lock().unwrap().iter().find(|v| v["op"] == json!("send_action_goal")).cloned().unwrap();
    assert_eq!(goal["args"]["pose"]["x"], json!(1.5), "a value keeps its shape");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_serial_port() {
    // A pseudo-terminal: a real tty, with this test at the other end of the cable.
    let (master, slave) = unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0 && libc::grantpt(master) == 0 && libc::unlockpt(master) == 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_string_lossy().to_string();
        (master, name)
    };
    let mut cable = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(master) };
    let d = defined(
        "name: meter\nframe: {line: \"\\n\"}\nread:\n  - {match: 'V=(?P<v>[0-9.]+)', metric: volts, group: v}\ncommands:\n  meter.reset:\n    send: \"RST\\n\"\n    once: true\n",
        "METER-1",
        Some(format!("serial://{slave}?baud=9600")),
    );
    until("the port", async || d.links()[0].up).await;
    use std::io::{Read, Write};
    cable.write_all(b"V=230.5\n").unwrap();
    reading(&d, |r| is_metric(r, "volts", 230.5)).await;
    d.dispatch(&command("meter.reset", json!({})), &device("METER-1")).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::task::block_in_place(|| cable.read(&mut buf).unwrap());
    assert_eq!(&buf[..n], b"RST\n");
}

#[tokio::test]
async fn an_external_driver_in_any_language() {
    let dir = std::env::temp_dir().join(format!("ferrum-drivers-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("driver.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
echo '{"metric":"flow","value":7.5,"device":"PUMP-1","unit":"l/s"}'
echo '{"attribute":"model","value":"X200","device":"PUMP-1"}'
while read -r line; do
  id=$(echo "$line" | sed -n 's/.*"command":"\([^"]*\)".*/\1/p')
  case "$line" in
    *'"op":"dispatch"'*) echo "{\"command\":\"$id\",\"status\":\"running\"}"; echo "{\"command\":\"$id\",\"status\":\"done\",\"detail\":\"ok\"}" ;;
  esac
done
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let d = Defined::new(Limits { drivers_dir: Some(dir.clone()), ..Default::default() }, json!({}));
    assert_eq!(apply(&d, "name: pump\nconnect: process://driver.sh\n", &[("PUMP-1", None)]), Vec::<String>::new());
    reading(&d, |r| is_metric(r, "flow", 7.5)).await;
    let c = command("pump.start", json!({"rate": 3}));
    assert!(matches!(d.dispatch(&c, &device("PUMP-1")).await, Ok(Dispatched::Accepted { .. })));
    until("the driver's answer", async || matches!(d.observe(&c, None).await.unwrap(), Observation::Completed(_))).await;

    let elsewhere = Defined::new(Limits { drivers_dir: Some(dir.clone()), ..Default::default() }, json!({}));
    let problems = apply(&elsewhere, "name: pump\nconnect: process://../../bin/sh\n", &[("PUMP-1", None)]);
    assert_eq!(problems.len(), 1, "a definition cannot run what it likes: {problems:?}");
    assert!(!elsewhere.reaches("PUMP-1"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_change_touches_only_what_changed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let opened = Arc::new(Mutex::new(0));
    let count = opened.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            *count.lock().unwrap() += 1;
            held.push(socket);
        }
    });
    let d = Defined::new(Limits::default(), json!({}));
    let text = "name: a\nframe: {line: \"\\n\"}\nread:\n  - {metric: x}\n";
    let connect = Some(format!("tcp://{address}"));
    assert_eq!(apply(&d, text, &[("A", connect.clone())]), Vec::<String>::new());
    until("the connection", async || *opened.lock().unwrap() == 1).await;
    assert_eq!(apply(&d, text, &[("A", connect.clone()), ("B", connect.clone())]), Vec::<String>::new());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(*opened.lock().unwrap(), 1, "a device joining leaves the connection alone");
    assert!(d.reaches("B"));

    // One bad definition is reported, and what was running stays running.
    let mixed = BTreeMap::from([("it".to_string(), text.to_string()), ("bad".to_string(), "name: x\nconnect: foo://y\n".to_string())]);
    let links = vec![
        DeviceLink { code: "A".into(), definition: "it".into(), connect: connect.clone(), settings: Value::Null, concurrency: 1 },
        DeviceLink { code: "C".into(), definition: "bad".into(), connect: None, settings: Value::Null, concurrency: 1 },
        DeviceLink { code: "D".into(), definition: "missing".into(), connect: None, settings: Value::Null, concurrency: 1 },
    ];
    let problems = d.apply(&mixed, &links);
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(d.reaches("A") && !d.reaches("B") && !d.reaches("C"));
    assert_eq!(*opened.lock().unwrap(), 1);

    assert_eq!(d.apply(&BTreeMap::new(), &[]), Vec::<String>::new());
    assert!(d.links().is_empty() && !d.reaches("A"));
}
