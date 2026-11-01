//! The protocols that needed more than a definition: equipment that calls in, a bus shared by several
//! units, a check on every frame, OPC UA and an MQTT broker. Real sockets, a real serial line, and
//! for OPC UA and MQTT someone else's implementation at the far end, which is why those two are
//! worth more than a stand-in (`docs/compatibility.md`). No vendor's device was involved.

use chrono::{Duration, Utc};
use ferrum_ops_contract::Command;
use ferrum_ops_contract::edge::DeviceLink;
use ferrum_ops_contract::sync::SiteDevice;
use ferrum_site::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use ferrum_site::adapters::defined::Defined;
use ferrum_site::adapters::transport::Limits;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration as Wait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
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
    SiteDevice { id: Uuid::nil(), code: code.into(), kind: "Machine".into(), lifecycle: "Active".into(), operations: vec![], limits: BTreeMap::new() }
}

async fn until(what: &str, mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..400 {
        if done().await {
            return;
        }
        tokio::time::sleep(Wait::from_millis(25)).await;
    }
    panic!("gave up waiting for {what}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start(limits: Limits, definition: &str, devices: &[(&str, Value)]) -> Defined {
    let d = Defined::new(limits, json!({"token": "s3cret"}));
    let definitions = BTreeMap::from([("it".to_string(), definition.to_string())]);
    let links: Vec<DeviceLink> = devices
        .iter()
        .map(|(code, settings)| DeviceLink { code: code.to_string(), definition: "it".into(), connect: None, settings: settings.clone(), concurrency: 1 })
        .collect();
    assert_eq!(d.apply(&definitions, &links), Vec::<String>::new());
    d
}

/// Keeps what arrives, since reading takes it all and a test looks for one thing at a time.
#[derive(Default)]
struct Seen(std::sync::Mutex<Vec<Reading>>);

impl Seen {
    async fn reading(&self, d: &Defined, want: impl Fn(&Reading) -> bool) -> Reading {
        let mut got = None;
        until("a reading", async || {
            let more = d.read().await.unwrap();
            let mut seen = self.0.lock().unwrap();
            seen.extend(more);
            got = seen.iter().position(&want).map(|i| seen.remove(i));
            got.is_some()
        })
        .await;
        got.unwrap()
    }
}

fn metric(r: &Reading, name: &str, want: f64) -> bool {
    matches!(r, Reading::Value { metric, value, .. } if metric == name && (*value - want).abs() < 1e-9)
}

fn of(r: &Reading, code: &str) -> bool {
    matches!(r, Reading::Value { device, .. } if device == code)
}

#[tokio::test]
async fn equipment_that_calls_in_is_answered_on_its_own_connection() {
    let seen = Seen::default();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let definition = format!(
        r#"
name: tracker
connect: listen://{address}
frame: {{line: "\n"}}
read:
  - {{match: '^(?P<id>[A-Z0-9-]+) t=(?P<t>[0-9.]+)$', device: "{{group.id}}", metric: temp, group: t}}
commands:
  tracker.ping:
    send: "PING {{device}}\n"
    once: true
"#
    );
    let d = start(Limits { listen: vec![address.clone()], ..Default::default() }, &definition, &[("T-1", Value::Null), ("T-2", Value::Null)]);
    until("the port", async || d.links()[0].up).await;

    let c = command("tracker.ping", json!({}));
    assert!(
        matches!(d.dispatch(&c, &device("T-1")).await, Err(Failure::NotSent(why)) if why.contains("has not called in")),
        "nobody to say it to, so it did not go"
    );

    let mut one = TcpStream::connect(&address).await.unwrap();
    let mut two = TcpStream::connect(&address).await.unwrap();
    one.write_all(b"T-1 t=20.5\n").await.unwrap();
    two.write_all(b"T-2 t=30.5\n").await.unwrap();
    seen.reading(&d, |r| of(r, "T-1") && metric(r, "temp", 20.5)).await;
    seen.reading(&d, |r| of(r, "T-2") && metric(r, "temp", 30.5)).await;

    d.dispatch(&command("tracker.ping", json!({})), &device("T-2")).await.expect("sent");
    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Wait::from_secs(2), two.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"PING T-2\n", "it reached the one it was for");
    assert!(tokio::time::timeout(Wait::from_millis(100), one.read(&mut buf)).await.is_err(), "and nobody else");
    d.dispatch(&command("tracker.ping", json!({})), &device("T-1")).await.expect("sent");
    let n = tokio::time::timeout(Wait::from_secs(2), one.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"PING T-1\n", "and so did the other");

    two.write_all(b"T-9 t=1.0\n").await.unwrap();
    until("the stranger", async || d.strangers().contains(&"T-9".to_string())).await;

    drop(two);
    tokio::time::sleep(Wait::from_millis(100)).await;
    let gone = d.dispatch(&command("tracker.ping", json!({})), &device("T-2")).await;
    assert!(matches!(gone, Err(Failure::NotSent(_))), "a caller that hung up is not there to hear it: {gone:?}");
}

/// A replaced link must stop. Its tasks keep it alive, so left to itself it would go on polling, and
/// hold a listening port or a serial line so that its replacement could never open it.
#[tokio::test]
async fn a_new_version_takes_over_the_port_from_the_old_one() {
    let seen = Seen::default();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let version = |metric: &str| {
        format!(
            "name: tracker\nconnect: listen://{address}\nframe: {{line: \"\\n\"}}\nread:\n  - {{match: '^t=(?P<t>[0-9.]+)$', metric: {metric}, group: t}}\n"
        )
    };
    let d = Defined::new(Limits { listen: vec![address.clone()], ..Default::default() }, json!({}));
    let links = [DeviceLink { code: "T-1".into(), definition: "it".into(), connect: None, settings: Value::Null, concurrency: 1 }];
    let first = BTreeMap::from([("it".to_string(), version("old"))]);
    assert_eq!(d.apply(&first, &links), Vec::<String>::new());
    until("the port", async || d.links()[0].up).await;
    let mut caller = TcpStream::connect(&address).await.unwrap();
    caller.write_all(b"t=1.5\n").await.unwrap();
    seen.reading(&d, |r| metric(r, "old", 1.5)).await;

    let second = BTreeMap::from([("it".to_string(), version("new"))]);
    assert_eq!(d.apply(&second, &links), Vec::<String>::new());
    let mut buf = [0u8; 8];
    let closed = tokio::time::timeout(Wait::from_secs(2), caller.read(&mut buf)).await.expect("the old link let its caller go");
    assert_eq!(closed.unwrap_or(0), 0);
    assert_eq!(d.links().len(), 1);
    until("the new link to have the port", async || d.links()[0].up).await;
    let mut caller = TcpStream::connect(&address).await.unwrap();
    caller.write_all(b"t=2.5\n").await.unwrap();
    seen.reading(&d, |r| metric(r, "new", 2.5)).await;
}

#[tokio::test]
async fn datagrams_from_anyone_and_answers_back() {
    let seen = Seen::default();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let definition = format!(
        "name: beacon\nconnect: listen+udp://{address}\nread:\n  - {{match: '^(?P<id>B[0-9]) (?P<v>[0-9.]+)$', device: \"{{group.id}}\", metric: level, group: v}}\ncommands:\n  beacon.ack:\n    send: ACK\n    once: true\n"
    );
    let d = start(Limits { listen: vec![address.clone()], ..Default::default() }, &definition, &[("B1", Value::Null)]);
    until("the port", async || d.links()[0].up).await;
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.send_to(b"B1 4.5", &address).await.unwrap();
    seen.reading(&d, |r| metric(r, "level", 4.5)).await;
    d.dispatch(&command("beacon.ack", json!({})), &device("B1")).await.expect("sent");
    let mut buf = [0u8; 8];
    let (n, _) = tokio::time::timeout(Wait::from_secs(2), socket.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"ACK");
}

#[tokio::test]
async fn a_fleet_manager_posts_to_a_site_with_no_platform_in_sight() {
    let seen = Seen::default();
    let port = free_port();
    let address = format!("127.0.0.1:{port}");
    let definition = format!(
        "name: fleet\nconnect: webhook://{address}?token={{secret.token}}\nread:\n  - {{topic: /events/+, device: \"{{topic.1}}\", values: [{{metric: battery, json: $.battery}}, {{event: arrived, json: $.at}}]}}\n"
    );
    let d = start(Limits { listen: vec![address.clone()], ..Default::default() }, &definition, &[("AGV-1", Value::Null)]);
    until("the port", async || d.links()[0].up).await;
    let url = format!("http://{address}/events/AGV-1");
    let http = reqwest::Client::new();
    let body = json!({"battery": 71, "at": "dock-4"}).to_string();
    assert_eq!(http.post(&url).body(body.clone()).send().await.unwrap().status(), 401, "not without the token");
    assert_eq!(http.post(&url).bearer_auth("wrong").body(body.clone()).send().await.unwrap().status(), 401);
    assert_eq!(http.post(&url).bearer_auth("s3cret").body(body).send().await.unwrap().status(), 202);
    seen.reading(&d, |r| metric(r, "battery", 71.0)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn several_units_share_one_modbus_serial_line() {
    let seen = Seen::default();
    use std::io::{Read, Write};
    let (master, slave) = unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0 && libc::grantpt(master) == 0 && libc::unlockpt(master) == 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(master)).to_string_lossy().to_string();
        (master, name)
    };
    let mut cable = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(master) };
    // Raw, or the terminal would echo and translate what crosses it.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        libc::tcgetattr(master, &mut tio);
        libc::cfmakeraw(&mut tio);
        libc::tcsetattr(master, libc::TCSANOW, &tio);
    }
    let crc = |b: &[u8]| b.iter().fold(0xffffu16, |c, x| (0..8).fold(c ^ u16::from(*x), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xa001 } else { c >> 1 }));
    // Each unit answers with a register holding its own address times ten. Unit 9 is not on the bus.
    std::thread::spawn(move || {
        let mut heard = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let Ok(n) = cable.read(&mut buf) else { return };
            heard.extend_from_slice(&buf[..n]);
            while heard.len() >= 8 {
                let frame: Vec<u8> = heard.drain(..8).collect();
                let unit = frame[0];
                if unit == 9 {
                    continue;
                }
                let mut reply = vec![unit, 3, 2, 0, unit * 10];
                let check = crc(&reply);
                reply.extend(check.to_le_bytes());
                if cable.write_all(&reply).is_err() {
                    return;
                }
            }
        }
    });
    let definition = format!(
        "name: meters\nconnect: modbus+rtu://{slave}?baud=19200&parity=E\npoll:\n  - {{every: 100ms, read: holding, at: 0, count: 1, unit: \"{{settings.unit}}\"}}\nread:\n  - {{metric: power, reg: 0}}\n"
    );
    let d = start(Limits::default(), &definition, &[("M3", json!({"unit": 3})), ("M4", json!({"unit": 4}))]);
    seen.reading(&d, |r| of(r, "M3") && metric(r, "power", 30.0)).await;
    seen.reading(&d, |r| of(r, "M4") && metric(r, "power", 40.0)).await;
}

#[tokio::test]
async fn a_binary_protocol_is_checked_both_ways() {
    let seen = Seen::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let crc = |b: &[u8]| b.iter().fold(0xffffu16, |c, x| (0..8).fold(c ^ u16::from(*x), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xa001 } else { c >> 1 }));
    let (got_tx, mut got) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let frame = |value: u8, damage: bool| {
            let mut f = vec![0xaa, 0x55, 2, value, 0];
            let check = crc(&f);
            f.extend(check.to_le_bytes());
            if damage {
                f[3] ^= 1;
            }
            f
        };
        let mut wire = vec![0x00, 0x13];
        wire.extend(frame(7, true));
        wire.extend(frame(9, false));
        socket.write_all(&wire).await.unwrap();
        let mut buf = [0u8; 16];
        let n = socket.read(&mut buf).await.unwrap();
        got_tx.send(buf[..n].to_vec()).await.unwrap();
        tokio::time::sleep(Wait::from_secs(5)).await;
    });
    let definition = format!(
        "name: drive\nconnect: tcp://{address}\nchecksum: {{kind: crc16-modbus}}\nframe: {{length: {{start: 'AA 55', at: 2, size: 1, add: 5}}}}\nread:\n  - {{metric: speed, at: 3}}\ncommands:\n  drive.stop:\n    hex: 01 02\n    once: true\n"
    );
    let d = start(Limits::default(), &definition, &[("D1", Value::Null)]);
    until("the link", async || d.links()[0].up).await;
    seen.reading(&d, |r| metric(r, "speed", 9.0)).await;
    assert_eq!(d.links()[0].dropped, 1, "the damaged frame was not a reading");
    assert!(d.links()[0].last_error.as_deref().is_some_and(|e| e.contains("check")));
    d.dispatch(&command("drive.stop", json!({})), &device("D1")).await.expect("sent");
    let sent = tokio::time::timeout(Wait::from_secs(2), got.recv()).await.unwrap().unwrap();
    assert_eq!(sent[..2], [1, 2]);
    assert_eq!(sent[2..], crc(&[1, 2]).to_le_bytes(), "it went out with its check");
}

/// A broker that is not ours, so that what this shows is that the client speaks MQTT 5 to someone
/// else's idea of it. It is here for the test and is not shipped: as of 0.20 it panics when a message
/// with properties reaches a 3.1.1 subscriber, and that takes its listener down with it. A site runs
/// Mosquitto (`deploy/site/mosquitto.conf`), as its own process.
fn broker() -> u16 {
    use rumqttd::{Broker, Config, ConnectionSettings, RouterConfig, ServerSettings, Strategy};
    let port = free_port();
    let connections = ConnectionSettings {
        connection_timeout_ms: 60_000,
        max_payload_size: 1 << 20,
        max_inflight_count: 200,
        auth: None,
        external_auth: None,
        dynamic_filters: true,
    };
    let listener = ServerSettings { name: "v5".into(), listen: ([127, 0, 0, 1], port).into(), tls: None, next_connection_delay_ms: 1, connections };
    let router = RouterConfig {
        max_connections: 64,
        max_outgoing_packet_count: 200,
        max_segment_size: 1 << 20,
        max_segment_count: 4,
        custom_segment: None,
        initialized_filters: None,
        shared_subscriptions_strategy: Strategy::default(),
    };
    let config = Config {
        id: 0,
        router,
        v4: None,
        v5: Some([("v5".to_string(), listener)].into()),
        ws: None,
        cluster: None,
        console: None,
        bridge: None,
        prometheus: None,
        metrics: None,
    };
    std::thread::spawn(move || drop(Broker::new(config).start()));
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sparkplug_node_and_a_robot_on_a_broker() {
    use rumqttc::v5::mqttbytes::QoS;
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::{AsyncClient, Event, MqttOptions};
    let seen = Seen::default();
    let port = broker();
    let definition = format!(
        r#"
name: node
connect: mqtt://127.0.0.1:{port}
subscribe: ["spBv1.0/plant/+/n1/+", "bot/+/state"]
read:
  - {{topic: "spBv1.0/plant/+/n1/+", format: sparkplug, device: "{{topic.2}}", each: $.metrics, metric: "{{each.name}}", json: $.value}}
  - {{topic: bot/+/state, device: "{{topic.1}}", metric: battery, json: $.battery}}
commands:
  machine.set:
    topic: spBv1.0/plant/DCMD/n1/{{device}}
    format: sparkplug
    send: {{timestamp: "{{now_ms}}", metrics: [{{name: Setpoint, value: "{{payload.value}}", type: double}}]}}
    once: true
"#
    );
    let d = start(Limits::default(), &definition, &[("M1", Value::Null), ("BOT-1", Value::Null)]);
    until("the broker", async || d.links()[0].up).await;

    let (node, mut events) = AsyncClient::new(MqttOptions::new("edge-node", "127.0.0.1", port), 16);
    node.subscribe("spBv1.0/plant/DCMD/n1/#", QoS::AtLeastOnce).await.unwrap();
    let (cmd_tx, mut cmd) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    tokio::spawn(async move {
        loop {
            match events.poll().await {
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    let _ = cmd_tx.send(p.payload.to_vec()).await;
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Wait::from_millis(50)).await,
            }
        }
    });
    tokio::time::sleep(Wait::from_millis(300)).await;
    let payload = ferrum_ops_contract::sparkplug::encode(&json!({"metrics": [{"name": "Motor/Speed rpm", "value": 1500.5}]})).unwrap();
    node.publish("spBv1.0/plant/DDATA/n1/M1", QoS::AtLeastOnce, false, payload).await.unwrap();
    node.publish("bot/BOT-1/state", QoS::AtLeastOnce, false, json!({"battery": 64}).to_string()).await.unwrap();
    seen.reading(&d, |r| of(r, "M1") && metric(r, "Motor/Speed_rpm", 1500.5)).await;
    seen.reading(&d, |r| of(r, "BOT-1") && metric(r, "battery", 64.0)).await;

    d.dispatch(&command("machine.set", json!({"value": 12.5})), &device("M1")).await.expect("sent");
    let raw = tokio::time::timeout(Wait::from_secs(5), cmd.recv()).await.unwrap().unwrap();
    let said = ferrum_ops_contract::sparkplug::decode(&raw).unwrap();
    assert_eq!(said["metrics"][0]["name"], "Setpoint");
    assert_eq!(said["metrics"][0]["value"], 12.5);
    assert!(said["timestamp"].as_u64().is_some_and(|t| t > 1_700_000_000_000), "the specification wants one: {said}");
}

#[cfg(feature = "opcua")]
mod opcua_server {
    use opcua::crypto::SecurityPolicy;
    use opcua::nodes::{MethodBuilder, ObjectBuilder, VariableBuilder};
    use opcua::server::diagnostics::NamespaceMetadata;
    use opcua::server::node_manager::memory::{SimpleNodeManager, simple_node_manager};
    use opcua::server::{ANONYMOUS_USER_TOKEN_ID, ServerBuilder, ServerHandle, ServerUserToken};
    use opcua::types::{DataTypeId, DataValue, MessageSecurityMode, NodeId, ObjectId, StatusCode, Variant};
    use std::sync::{Arc, Mutex};

    pub struct Plc {
        pub handle: ServerHandle,
        pub manager: Arc<SimpleNodeManager>,
        pub ns: u16,
        pub calls: Arc<Mutex<Vec<Vec<Variant>>>>,
    }

    impl Plc {
        pub fn set(&self, name: &str, value: impl Into<Variant>) {
            let value = DataValue::new_now(value.into());
            self.manager.set_value(self.handle.subscriptions(), &NodeId::new(self.ns, name), None, value).unwrap();
        }

        pub fn set_bad(&self, name: &str, value: impl Into<Variant>) {
            let mut value = DataValue::new_now(value.into());
            value.status = Some(StatusCode::BadSensorFailure);
            self.manager.set_value(self.handle.subscriptions(), &NodeId::new(self.ns, name), None, value).unwrap();
        }

        pub fn get(&self, name: &str) -> Option<Variant> {
            let space = self.manager.address_space().read();
            let id = NodeId::new(self.ns, name);
            match space.find_node(&id)? {
                opcua::nodes::NodeType::Variable(v) => {
                    v.value(opcua::types::TimestampsToReturn::Neither, &Default::default(), &opcua::types::DataEncoding::Binary, 0.0).value
                }
                _ => None,
            }
        }
    }

    pub fn start(port: u16, pki: std::path::PathBuf, secure: bool) -> Plc {
        let builder = ServerBuilder::new()
            .application_name("stand-in")
            .application_uri("urn:stand-in")
            .product_uri("urn:stand-in")
            .with_node_manager(simple_node_manager(NamespaceMetadata { namespace_uri: "urn:plc".to_owned(), ..Default::default() }, "plc"))
            .host("127.0.0.1")
            .port(port)
            .pki_dir(pki)
            .create_sample_keypair(secure)
            .trust_client_certs(true)
            .discovery_urls(vec![format!("opc.tcp://127.0.0.1:{port}/")])
            .add_user_token("plant", ServerUserToken::user_pass("ferrum", "s3cret"));
        let builder = if secure {
            builder.add_endpoint("secure", ("/", SecurityPolicy::Basic256Sha256, MessageSecurityMode::SignAndEncrypt, &["plant"] as &[&str]))
        } else {
            builder.add_endpoint("none", ("/", SecurityPolicy::None, MessageSecurityMode::None, &[ANONYMOUS_USER_TOKEN_ID] as &[&str]))
        };
        let (server, handle) = builder.build().unwrap();
        let manager = handle.node_managers().get_of_type::<SimpleNodeManager>().unwrap();
        let ns = handle.get_namespace_index("urn:plc").unwrap();
        let calls: Arc<Mutex<Vec<Vec<Variant>>>> = Arc::default();
        {
            let mut space = manager.address_space().write();
            let press = NodeId::new(ns, "Press");
            ObjectBuilder::new(&press, "Press", "Press").organized_by(ObjectId::ObjectsFolder).insert(&mut *space);
            VariableBuilder::new(&NodeId::new(ns, "Temp"), "Temp", "Temp")
                .data_type(DataTypeId::Double)
                .value(20.0f64)
                .component_of(&press)
                .insert(&mut *space);
            VariableBuilder::new(&NodeId::new(ns, "Job"), "Job", "Job")
                .data_type(DataTypeId::Int16)
                .value(0i16)
                .writable()
                .component_of(&press)
                .insert(&mut *space);
            VariableBuilder::new(&NodeId::new(ns, "Locked"), "Locked", "Locked")
                .data_type(DataTypeId::Int32)
                .value(1i32)
                .component_of(&press)
                .insert(&mut *space);
            MethodBuilder::new(&NodeId::new(ns, "Start"), "Start", "Start")
                .component_of(&press)
                .executable(true)
                .user_executable(true)
                .input_args(&mut *space, &NodeId::new(ns, "StartIn"), &[("job", DataTypeId::Int32).into()])
                .output_args(&mut *space, &NodeId::new(ns, "StartOut"), &[("run", DataTypeId::Int32).into()])
                .insert(&mut *space);
        }
        let seen = calls.clone();
        manager.inner().add_method_callback(NodeId::new(ns, "Start"), move |args| {
            seen.lock().unwrap().push(args.to_vec());
            Ok::<_, StatusCode>(vec![Variant::Int32(7)])
        });
        tokio::spawn(server.run());
        Plc { handle, manager, ns, calls }
    }
}

#[cfg(feature = "opcua")]
#[tokio::test(flavor = "multi_thread")]
async fn a_plc_over_opc_ua_watched_written_and_called() {
    let seen = Seen::default();
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("ferrum-opcua-{}", Uuid::now_v7()));
    let plc = opcua_server::start(port, dir.join("server"), false);
    let ns = plc.ns;
    let definition = format!(
        r#"
name: press
connect: opcua://127.0.0.1:{port}/?interval=100
subscribe: ["ns={ns};s=Temp", "ns={ns};s=Job"]
read:
  - {{topic: "ns={ns};s=Temp", metric: temp, json: $.value, time: $.time}}
  - {{topic: "ns={ns};s=Job", attribute: job, json: $.value}}
poll:
  - {{every: 200ms, node: "ns={ns};s=Temp"}}
commands:
  press.set_job:
    node: "ns={ns};s=Job"
    send: "{{payload.job}}"
    status: {{topic: "ns={ns};s=Job", json: $.value, done: "{{payload.job}}"}}
  press.start:
    node: "ns={ns};s=Press"
    call: "ns={ns};s=Start"
    send: [{{type: int32, value: "{{payload.job}}"}}]
    once: true
  press.bad:
    node: "ns={ns};s=Locked"
    send: "{{payload.job}}"
    once: true
"#
    );
    let d = start(Limits { tls_dir: Some(dir.join("client")), ..Default::default() }, &definition, &[("PRESS-1", Value::Null)]);
    until("the session", async || d.links()[0].up).await;

    plc.set("Temp", 21.5f64);
    let got = seen.reading(&d, |r| metric(r, "temp", 21.5)).await;
    assert!(matches!(got, Reading::Value { source_time: Some(_), .. }), "the server's own time is kept: {got:?}");

    // A value that does not fit is refused here, not rounded and sent.
    let wide = d.dispatch(&command("press.set_job", json!({"job": 70000})), &device("PRESS-1")).await;
    assert!(matches!(wide, Err(Failure::NotSent(ref why)) | Ok(Dispatched::Rejected(ref why)) if why.contains("does not fit")), "{wide:?}");
    let cut = d.dispatch(&command("press.set_job", json!({"job": 1.5})), &device("PRESS-1")).await;
    assert!(matches!(cut, Err(Failure::NotSent(ref why)) if why.contains("does not fit")), "nor is a fraction cut off: {cut:?}");
    assert_eq!(plc.get("Job"), Some(opcua::types::Variant::Int16(0)));

    // What the server itself calls bad is not a reading, whatever number came with it.
    plc.set_bad("Temp", 999.5f64);
    plc.set("Temp", 22.5f64);
    seen.reading(&d, |r| metric(r, "temp", 22.5)).await;
    assert!(!seen.0.lock().unwrap().iter().any(|r| metric(r, "temp", 999.5)), "a failed sensor's number was recorded");

    let c = command("press.start", json!({"job": 42}));
    d.dispatch(&c, &device("PRESS-1")).await.expect("called");
    assert_eq!(plc.calls.lock().unwrap().as_slice(), [vec![opcua::types::Variant::Int32(42)]]);
    assert!(matches!(d.observe(&c, None).await, Ok(Observation::Completed(_))));

    let refused = d.dispatch(&command("press.bad", json!({"job": 1})), &device("PRESS-1")).await;
    assert!(matches!(refused, Err(Failure::NotSent(ref why)) if why.contains("refused")), "the server said no, so nothing changed: {refused:?}");

    // The link goes away with the server, and comes back with it.
    plc.handle.cancel();
    until("the link to fall", async || !d.links()[0].up).await;
    assert!(matches!(d.dispatch(&command("press.start", json!({"job": 1})), &device("PRESS-1")).await, Err(Failure::NotSent(_))));
    let _ = std::fs::remove_dir_all(dir);
}

/// With security on, a server is believed only once a person has said so, as OPC UA intends: the
/// first connection fails, leaving its certificate where someone can look at it. Slow in a debug
/// build, because each side makes itself an RSA key.
#[cfg(feature = "opcua")]
#[tokio::test(flavor = "multi_thread")]
async fn an_encrypted_session_starts_only_with_a_server_someone_trusted() {
    let seen = Seen::default();
    let port = free_port();
    let dir = std::env::temp_dir().join(format!("ferrum-opcua-{}", Uuid::now_v7()));
    let plc = opcua_server::start(port, dir.join("server"), true);
    let ns = plc.ns;
    let definition = format!(
        "name: press\nconnect: opcua://127.0.0.1:{port}/?security=Basic256Sha256&mode=SignAndEncrypt&user=ferrum&password=s3cret&interval=100\nsubscribe: [\"ns={ns};s=Temp\"]\nread:\n  - {{topic: \"ns={ns};s=Temp\", metric: temp, json: $.value}}\n"
    );
    let store = dir.join("client");
    std::fs::create_dir_all(&store).unwrap();
    let d = start(Limits { tls_dir: Some(store.clone()), ..Default::default() }, &definition, &[("PRESS-1", Value::Null)]);

    let rejected = store.join("opcua/rejected");
    let held = async {
        loop {
            if let Ok(mut files) = std::fs::read_dir(&rejected)
                && let Some(Ok(file)) = files.next()
            {
                return file;
            }
            tokio::time::sleep(Wait::from_millis(100)).await;
        }
    };
    let file = tokio::time::timeout(Wait::from_secs(60), held).await.expect("its certificate is kept for someone to look at");
    assert!(!d.links()[0].up, "nothing is believed yet");
    std::fs::rename(file.path(), store.join("opcua/trusted").join(file.file_name())).unwrap();

    // The link tries again on its own, with its usual patience.
    for _ in 0..1200 {
        if d.links()[0].up {
            break;
        }
        tokio::time::sleep(Wait::from_millis(50)).await;
    }
    assert!(d.links()[0].up, "a person moved it, so now it is trusted: {:?}", d.links()[0].last_error);
    plc.set("Temp", 33.5f64);
    seen.reading(&d, |r| metric(r, "temp", 33.5)).await;
    plc.handle.cancel();
    let _ = std::fs::remove_dir_all(dir);
}
