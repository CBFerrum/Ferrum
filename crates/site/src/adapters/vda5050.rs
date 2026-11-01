//! VDA 5050, as master control talking to compatible vehicles over MQTT: one order per mission,
//! followed through the vehicle's `state` topic. It speaks the versions in `SUPPORTED` and refuses
//! others, because "supports VDA 5050" says nothing without a version. Tried on a simulator only.
//!
//! A vehicle's state says what it did, not that the right load reached the right place: a mission
//! completed here is not stock moved. That takes a scan at the station.

use crate::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_ops_contract::telemetry::Class;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::Packet;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The major.minor versions this adapter was written against.
pub const SUPPORTED: [&str; 2] = ["2.0", "2.1"];

fn port() -> u16 {
    1883
}

fn interface() -> String {
    "uagv".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct VdaConfig {
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    #[serde(default = "interface")]
    pub interface: String,
    /// `2.0.0` or `2.1.0`.
    pub version: String,
    pub manufacturer: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

/// `{interface}/v{major}/{manufacturer}/{serial}/{topic}`, as the specification sets out.
pub fn topic(cfg: &VdaConfig, serial: &str, name: &str) -> String {
    let major = cfg.version.split('.').next().unwrap_or("2");
    format!("{}/v{major}/{}/{serial}/{name}", cfg.interface, cfg.manufacturer)
}

fn supported(version: &str) -> bool {
    SUPPORTED.iter().any(|v| version.starts_with(&format!("{v}.")))
}

/// How a vehicle's state stands against one order.
pub fn judge(state: &Value, order: &str) -> Observation {
    if state["orderId"].as_str() != Some(order) {
        return Observation::Waiting;
    }
    let fatal: Vec<&Value> = state["errors"].as_array().map(|e| e.iter().filter(|x| x["errorLevel"] == json!("FATAL")).collect()).unwrap_or_default();
    if !fatal.is_empty() {
        return Observation::Failed(json!({"errors": fatal, "lastNodeId": state["lastNodeId"]}));
    }
    let empty = |k: &str| state[k].as_array().is_none_or(Vec::is_empty);
    let actions_done = state["actionStates"].as_array().is_none_or(|a| a.iter().all(|x| matches!(x["actionStatus"].as_str(), Some("FINISHED" | "FAILED"))));
    if empty("nodeStates") && empty("edgeStates") && state["driving"] != json!(true) && actions_done {
        let failed: Vec<&Value> =
            state["actionStates"].as_array().map(|a| a.iter().filter(|x| x["actionStatus"] == json!("FAILED")).collect()).unwrap_or_default();
        if failed.is_empty() {
            return Observation::Completed(json!({"lastNodeId": state["lastNodeId"], "agvPosition": state["agvPosition"], "timestamp": state["timestamp"]}));
        }
        return Observation::Failed(json!({"failed_actions": failed, "lastNodeId": state["lastNodeId"]}));
    }
    Observation::Running
}

#[derive(Default)]
struct Shared {
    connected: bool,
    /// The newest state of each vehicle, with the header id it arrived under.
    states: HashMap<String, (i64, Value)>,
    online: HashMap<String, String>,
}

pub struct Vda {
    cfg: VdaConfig,
    client: AsyncClient,
    shared: Arc<Mutex<Shared>>,
    header: AtomicI64,
    serials: Vec<String>,
}

fn serial_of<'a>(topic: &'a str, cfg: &VdaConfig) -> Option<(&'a str, &'a str)> {
    let prefix = format!("{}/v{}/{}/", cfg.interface, cfg.version.split('.').next().unwrap_or("2"), cfg.manufacturer);
    topic.strip_prefix(&prefix)?.split_once('/')
}

impl Vda {
    pub fn start(name: &str, cfg: VdaConfig, serials: Vec<String>) -> Vda {
        let mut options = MqttOptions::new(format!("ferrum-site-{name}"), cfg.host.clone(), cfg.port);
        options.set_keep_alive(Duration::from_secs(30));
        if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
            options.set_credentials(u.clone(), p.clone());
        }
        let (client, mut events) = AsyncClient::new(options, 256);
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let (state, subscriber, config, wanted) = (shared.clone(), client.clone(), cfg.clone(), serials.clone());
        tokio::spawn(async move {
            loop {
                match events.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        state.lock().unwrap_or_else(|e| e.into_inner()).connected = true;
                        for serial in &wanted {
                            for name in ["state", "connection"] {
                                let _ = subscriber.subscribe(topic(&config, serial, name), QoS::AtLeastOnce).await;
                            }
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let at = String::from_utf8_lossy(&p.topic).to_string();
                        let Some((serial, name)) = serial_of(&at, &config) else { continue };
                        let Ok(v) = serde_json::from_slice::<Value>(&p.payload) else { continue };
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        match name {
                            "state" => {
                                // A message that arrives late must not replace a newer one. A vehicle
                                // that restarted counts from zero again: its first state resets this.
                                let id = v["headerId"].as_i64().unwrap_or(0);
                                let current = s.states.get(serial).map(|e| e.0).unwrap_or(i64::MIN);
                                if id > current || id < 2 {
                                    s.states.insert(serial.to_string(), (id, v));
                                }
                            }
                            "connection" => {
                                if let Some(c) = v["connectionState"].as_str() {
                                    s.online.insert(serial.to_string(), c.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        state.lock().unwrap_or_else(|e| e.into_inner()).connected = false;
                        tracing::debug!("vda5050: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
        Vda { cfg, client, shared, header: AtomicI64::new(0), serials }
    }

    fn header_id(&self) -> i64 {
        self.header.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn state_of(&self, serial: &str) -> Option<Value> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).states.get(serial).map(|e| e.1.clone())
    }

    fn online(&self, serial: &str) -> bool {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).online.get(serial).is_some_and(|c| c == "ONLINE")
    }
}

#[async_trait]
impl Adapter for Vda {
    async fn dispatch(&self, c: &Command, device: &SiteDevice) -> Result<Dispatched, Failure> {
        if !supported(&self.cfg.version) {
            return Ok(Dispatched::Rejected(format!("VDA 5050 {} is not a version this adapter was written for", self.cfg.version)));
        }
        let (Some(nodes), Some(edges)) = (c.payload["nodes"].as_array(), c.payload["edges"].as_array()) else {
            return Ok(Dispatched::Rejected("a VDA 5050 order needs nodes and edges".into()));
        };
        if !self.online(&device.code) {
            return Err(Failure::NotSent(format!("{} is not online", device.code)));
        }
        let order = json!({
            "headerId": self.header_id(), "timestamp": Utc::now().to_rfc3339(), "version": self.cfg.version,
            "manufacturer": self.cfg.manufacturer, "serialNumber": device.code,
            "orderId": c.id.to_string(), "orderUpdateId": 0, "nodes": nodes, "edges": edges,
        });
        match self.client.publish(topic(&self.cfg, &device.code, "order"), QoS::AtLeastOnce, false, order.to_string()).await {
            Ok(()) => Ok(Dispatched::Accepted { external: Some(c.id.to_string()) }),
            Err(e) => Err(Failure::Unknown(e.to_string())),
        }
    }

    async fn observe(&self, c: &Command, _external: Option<&str>) -> Result<Observation, Failure> {
        let id = c.id.to_string();
        // The device is found by the order it is working on, so a command's target can be a fleet.
        let states: Vec<Value> = self.serials.iter().filter_map(|s| self.state_of(s)).collect();
        let Some(state) = states.iter().find(|s| s["orderId"].as_str() == Some(&id)) else {
            let connected = self.shared.lock().unwrap_or_else(|e| e.into_inner()).connected;
            return if connected { Ok(Observation::Waiting) } else { Err(Failure::Unknown("no broker".into())) };
        };
        Ok(judge(state, &id))
    }

    async fn cancel(&self, c: &Command, _external: Option<&str>) -> Result<(), Failure> {
        let actions = json!({
            "headerId": self.header_id(), "timestamp": Utc::now().to_rfc3339(), "version": self.cfg.version, "manufacturer": self.cfg.manufacturer,
            "actions": [{"actionType": "cancelOrder", "actionId": c.id.to_string(), "blockingType": "HARD"}],
        });
        for serial in &self.serials {
            self.client
                .publish(topic(&self.cfg, serial, "instantActions"), QoS::AtLeastOnce, false, actions.to_string())
                .await
                .map_err(|e| Failure::NotSent(e.to_string()))?;
        }
        Ok(())
    }

    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        let mut out = Vec::new();
        for serial in &self.serials {
            let Some(s) = self.state_of(serial) else { continue };
            if let Some(b) = s["batteryState"]["batteryCharge"].as_f64() {
                out.push(Reading::Value {
                    device: serial.clone(),
                    metric: "battery.pct".into(),
                    value: b,
                    unit: Some("%".into()),
                    class: Class::Sample,
                    source_time: None,
                });
            }
            let fault = s["errors"].as_array().is_some_and(|e| e.iter().any(|x| x["errorLevel"] == json!("FATAL")));
            let state = match (fault, s["driving"].as_bool(), s["paused"].as_bool()) {
                (true, ..) => "Stopped",
                (_, _, Some(true)) => "Idle",
                (_, Some(true), _) => "Running",
                _ => "Idle",
            };
            out.push(Reading::State { device: serial.clone(), state: state.into(), reason: fault.then(|| "fatal error on the vehicle".to_string()) });
        }
        Ok(out)
    }

    fn health(&self) -> String {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        if !s.connected {
            return "no broker".into();
        }
        format!("{} of {} vehicles online", s.online.values().filter(|c| *c == "ONLINE").count(), self.serials.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VdaConfig {
        VdaConfig {
            host: "x".into(),
            port: 1883,
            interface: "uagv".into(),
            version: "2.1.0".into(),
            manufacturer: "acme".into(),
            username: None,
            password: None,
        }
    }

    #[test]
    fn topics_follow_the_specification() {
        assert_eq!(topic(&cfg(), "AMR-1", "order"), "uagv/v2/acme/AMR-1/order");
        assert_eq!(serial_of("uagv/v2/acme/AMR-1/state", &cfg()), Some(("AMR-1", "state")));
        assert_eq!(serial_of("uagv/v1/acme/AMR-1/state", &cfg()), None, "another major version is another protocol");
    }

    #[test]
    fn a_version_is_named_exactly() {
        assert!(supported("2.0.0") && supported("2.1.0"));
        assert!(!supported("3.0.0"), "published, and not what this was written against");
        assert!(!supported("1.1.0"));
    }

    #[test]
    fn an_order_ends_only_when_the_vehicle_says_so() {
        let running = json!({"orderId": "o", "nodeStates": [{"nodeId": "b"}], "edgeStates": [], "driving": true});
        assert_eq!(judge(&running, "o"), Observation::Running);
        assert_eq!(judge(&running, "other"), Observation::Waiting, "another order says nothing about this one");
        let stopped_at_last_node = json!({"orderId": "o", "nodeStates": [], "edgeStates": [], "driving": true});
        assert_eq!(judge(&stopped_at_last_node, "o"), Observation::Running, "still moving");
        let pending_action = json!({"orderId": "o", "nodeStates": [], "edgeStates": [], "driving": false, "actionStates": [{"actionStatus": "RUNNING"}]});
        assert_eq!(judge(&pending_action, "o"), Observation::Running, "it has not finished what it was asked to do there");
        let done =
            json!({"orderId": "o", "nodeStates": [], "edgeStates": [], "driving": false, "actionStates": [{"actionStatus": "FINISHED"}], "lastNodeId": "b"});
        assert!(matches!(judge(&done, "o"), Observation::Completed(_)));
    }

    #[test]
    fn a_fatal_error_or_a_failed_action_is_a_failure() {
        let fatal = json!({"orderId": "o", "nodeStates": [{"nodeId": "b"}], "errors": [{"errorLevel": "FATAL", "errorType": "lost"}]});
        assert!(matches!(judge(&fatal, "o"), Observation::Failed(_)));
        let warned = json!({"orderId": "o", "nodeStates": [{"nodeId": "b"}], "errors": [{"errorLevel": "WARNING"}]});
        assert_eq!(judge(&warned, "o"), Observation::Running, "a warning is not an ending");
        let failed_pick = json!({"orderId": "o", "nodeStates": [], "edgeStates": [], "driving": false, "actionStates": [{"actionStatus": "FAILED"}]});
        assert!(matches!(judge(&failed_pick, "o"), Observation::Failed(_)), "it arrived and did not pick");
    }
}
