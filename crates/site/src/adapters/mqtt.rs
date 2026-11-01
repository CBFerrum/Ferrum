//! MQTT 5: measurements from topics, and asynchronous commands on topics of their own. QoS says what
//! the broker did, never what the equipment did, so a command is accepted only by a reply on its
//! status topic, and commands carry an expiry so a broker cannot deliver a stale one later.

use crate::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_ops_contract::telemetry::Class;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn port() -> u16 {
    1883
}

#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Measurements: each topic filter, with the device and metric it carries.
    #[serde(default)]
    pub measurements: Vec<Subscription>,
    /// Where commands go; `{device}` is replaced. Without it this adapter takes no commands.
    #[serde(default)]
    pub command_topic: Option<String>,
    /// Where a device answers `{"command": id, "state": "accepted|running|completed|failed|aborted|rejected"}`.
    #[serde(default)]
    pub status_topic: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Subscription {
    pub topic: String,
    pub device: String,
    pub metric: String,
    #[serde(default)]
    pub unit: Option<String>,
    /// Where the number is in a JSON payload. Empty means the payload is the number.
    #[serde(default)]
    pub field: Option<String>,
}

#[derive(Default)]
struct Shared {
    connected: bool,
    readings: Vec<Reading>,
    statuses: HashMap<String, Value>,
}

pub struct Mqtt {
    cfg: MqttConfig,
    client: AsyncClient,
    shared: Arc<Mutex<Shared>>,
}

/// A topic filter against a topic, by MQTT's rules for `+` and `#`.
pub fn matches(filter: &str, topic: &str) -> bool {
    let (mut f, mut t) = (filter.split('/'), topic.split('/'));
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// The number in a payload, by `field`, or nothing when it is not one. A value that cannot be read
/// is dropped here rather than stored as something it is not.
pub fn number(payload: &[u8], field: Option<&str>) -> Option<f64> {
    let text = std::str::from_utf8(payload).ok()?;
    match field {
        None => text.trim().parse().ok(),
        Some(f) => serde_json::from_str::<Value>(text).ok()?.get(f)?.as_f64(),
    }
}

impl Mqtt {
    pub fn start(name: &str, cfg: MqttConfig) -> Mqtt {
        let mut options = MqttOptions::new(format!("ferrum-site-{name}"), cfg.host.clone(), cfg.port);
        options.set_keep_alive(Duration::from_secs(30));
        if let (Some(u), Some(p)) = (&cfg.username, &cfg.password) {
            options.set_credentials(u.clone(), p.clone());
        }
        let (client, mut events) = AsyncClient::new(options, 256);
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let (state, subscriber, config) = (shared.clone(), client.clone(), cfg.clone());
        tokio::spawn(async move {
            loop {
                match events.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        state.lock().unwrap_or_else(|e| e.into_inner()).connected = true;
                        for s in &config.measurements {
                            let _ = subscriber.subscribe(s.topic.clone(), QoS::AtLeastOnce).await;
                        }
                        if let Some(t) = &config.status_topic {
                            let _ = subscriber.subscribe(t.replace("{device}", "+"), QoS::AtLeastOnce).await;
                        }
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let topic = String::from_utf8_lossy(&p.topic).to_string();
                        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                        for sub in config.measurements.iter().filter(|m| matches(&m.topic, &topic)) {
                            if let Some(value) = number(&p.payload, sub.field.as_deref()) {
                                s.readings.push(Reading::Value {
                                    device: sub.device.clone(),
                                    metric: sub.metric.clone(),
                                    value,
                                    unit: sub.unit.clone(),
                                    class: Class::Sample,
                                    source_time: None,
                                });
                            }
                        }
                        // Bounded: a chatty broker costs samples, never memory.
                        let over = s.readings.len().saturating_sub(10_000);
                        s.readings.drain(..over);
                        if let Ok(v) = serde_json::from_slice::<Value>(&p.payload)
                            && let Some(command) = v["command"].as_str()
                        {
                            s.statuses.insert(command.to_string(), v.clone());
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        state.lock().unwrap_or_else(|e| e.into_inner()).connected = false;
                        tracing::debug!("mqtt: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
        Mqtt { cfg, client, shared }
    }

    fn connected(&self) -> bool {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).connected
    }

    fn status(&self, id: &str) -> Option<Value> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).statuses.get(id).cloned()
    }
}

#[async_trait]
impl Adapter for Mqtt {
    async fn dispatch(&self, c: &Command, device: &SiteDevice) -> Result<Dispatched, Failure> {
        let Some(topic) = &self.cfg.command_topic else { return Ok(Dispatched::Rejected("this adapter takes no commands".into())) };
        if !self.connected() {
            return Err(Failure::NotSent("not connected to the broker".into()));
        }
        let left = (c.expires_at - Utc::now()).num_seconds().max(1) as u32;
        let properties = PublishProperties { message_expiry_interval: Some(left), ..Default::default() };
        let body = json!({"command": c.id, "operation": c.operation, "payload": c.payload, "expires_at": c.expires_at}).to_string();
        let topic = topic.replace("{device}", &device.code);
        match self.client.publish_with_properties(topic, QoS::AtLeastOnce, false, body, properties).await {
            Ok(()) => Ok(Dispatched::Accepted { external: Some(c.id.to_string()) }),
            // The client's queue took it or did not; once it may have, nothing here can tell.
            Err(e) => Err(Failure::Unknown(e.to_string())),
        }
    }

    async fn observe(&self, c: &Command, _external: Option<&str>) -> Result<Observation, Failure> {
        let Some(status) = self.status(&c.id.to_string()) else { return Ok(Observation::Waiting) };
        Ok(match status["state"].as_str() {
            Some("accepted") => Observation::Waiting,
            Some("running") => Observation::Running,
            Some("completed") => Observation::Completed(status),
            Some("aborted") => Observation::Aborted(status),
            Some("failed" | "rejected") => Observation::Failed(status),
            _ => Observation::Waiting,
        })
    }

    async fn cancel(&self, c: &Command, _external: Option<&str>) -> Result<(), Failure> {
        let Some(topic) = &self.cfg.command_topic else { return Ok(()) };
        let body = json!({"cancel": c.id}).to_string();
        self.client.publish(topic.replace("{device}", "all"), QoS::AtLeastOnce, false, body).await.map_err(|e| Failure::NotSent(e.to_string()))
    }

    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        Ok(std::mem::take(&mut self.shared.lock().unwrap_or_else(|e| e.into_inner()).readings))
    }

    fn health(&self) -> String {
        if self.connected() { "connected".into() } else { "no broker".into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_filters_follow_mqtt() {
        assert!(matches("a/b", "a/b"));
        assert!(matches("a/+/c", "a/x/c"));
        assert!(matches("a/#", "a/x/y"));
        assert!(!matches("a/+", "a/x/y"));
        assert!(!matches("a/b", "a/b/c"));
        assert!(!matches("a/b/c", "a/b"));
    }

    #[test]
    fn a_value_that_cannot_be_read_is_not_stored() {
        assert_eq!(number(b"21.5", None), Some(21.5));
        assert_eq!(number(br#"{"t": 3}"#, Some("t")), Some(3.0));
        assert_eq!(number(b"warm", None), None);
        assert_eq!(number(br#"{"t": "warm"}"#, Some("t")), None);
    }
}
