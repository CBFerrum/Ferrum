//! The adapters on the wire: real sockets, real packets, and each protocol's own way of going wrong.
//! The broker and the controller here are a few dozen lines that speak just enough of MQTT 5 and
//! Modbus TCP, so what is exercised is the client we ship. They are stand-ins: passing here says the
//! adapter speaks the protocol, not that any vendor's device agrees (docs/compatibility.md).

use chrono::{Duration, Utc};
use ferrum_ops_contract::Command;
use ferrum_ops_contract::sync::SiteDevice;
use ferrum_site::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use ferrum_site::adapters::modbus::{Handshake, Modbus, ModbusConfig, Register};
use ferrum_site::adapters::mqtt::{Mqtt, MqttConfig, Subscription};
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
                            let value = plc.lock().unwrap().registers.get(&register).copied().unwrap_or(0);
                            let mut a = vec![3, 2];
                            a.extend(value.to_be_bytes());
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
