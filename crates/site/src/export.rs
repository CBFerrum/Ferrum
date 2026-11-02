//! Where else a site's measurements go: a historian on the same network, another broker, someone
//! else's cloud. Each export keeps its own queue in the journal, so one that is down holds up nothing
//! and catches up when it is back, within a budget. Delivery is at least once: a receiver that cares
//! uses `device`, `metric` and `at` as the key.

use crate::adapter::Failure;
use crate::adapters::transport::{self, Limits, Message, Out, Transport};
use ferrum_ops_contract::edge::Export;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// One export's queue, at most.
pub const BUDGET: i64 = 200_000;
/// One delivery, at most.
pub const BATCH: i64 = 500;

/// Exact, or everything under a prefix: `weight.*`.
pub fn wanted(export: &Export, metric: &str) -> bool {
    export.only.is_empty()
        || export.only.iter().any(|o| match o.strip_suffix('*') {
            Some(prefix) => metric.starts_with(prefix),
            None => o == metric,
        })
}

fn signature(secret: &str, body: &[u8]) -> Option<String> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(body);
    Some(format!("sha256={}", hex::encode(mac.finalize().into_bytes())))
}

#[derive(Default)]
pub struct Sender {
    http: reqwest::Client,
    brokers: Mutex<HashMap<String, Arc<dyn Transport>>>,
    limits: Limits,
}

impl Sender {
    /// An export can come from the platform, so where it goes is the site's to limit, like anything
    /// a definition opens.
    pub fn new(limits: Limits) -> Sender {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(5)).build().unwrap_or_default();
        Sender { http, brokers: Mutex::default(), limits }
    }

    /// A broker stays connected between batches. Its connection looks after itself.
    fn broker(&self, to: &str) -> Result<Arc<dyn Transport>, String> {
        let mut brokers = self.brokers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = brokers.get(to) {
            return Ok(b.clone());
        }
        let broker = transport::open(to, transport::Options::default(), &self.limits)?;
        brokers.insert(to.to_string(), broker.clone());
        let runner = broker.clone();
        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::channel::<Message>(1);
            tokio::spawn(async move { while rx.recv().await.is_some() {} });
            loop {
                let (ready, _keep) = mpsc::channel::<()>(1);
                let _ = runner.run(tx.clone(), ready).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Ok(broker)
    }

    /// A release's file.
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let response = self.http.get(url).timeout(Duration::from_secs(600)).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{url} answered {}", response.status()));
        }
        response.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
    }

    /// Delivers a batch of JSON measurements. Anything but a clear yes leaves it queued.
    pub async fn deliver(&self, export: &Export, bodies: &[String], secrets: &Value) -> Result<(), String> {
        if !self.limits.reaches(&export.to) {
            return Err(format!("{} is not in this site's allow list", export.to));
        }
        if export.to.starts_with("mqtt") {
            let broker = self.broker(&export.to)?;
            for body in bodies {
                let m: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                let topic = export
                    .topic
                    .as_deref()
                    .unwrap_or("ferrum/{device}/{metric}")
                    .replace("{device}", m["device"].as_str().unwrap_or("unknown"))
                    .replace("{metric}", m["metric"].as_str().unwrap_or("unknown"));
                broker.send(Out::Publish { topic, body: body.clone().into_bytes(), expiry: None, retain: false }).await.map_err(|e| match e {
                    Failure::NotSent(w) | Failure::Unknown(w) => w,
                })?;
            }
            return Ok(());
        }
        let items: Vec<Value> = bodies.iter().filter_map(|b| serde_json::from_str(b).ok()).collect();
        let body = serde_json::to_vec(&serde_json::json!({"measurements": items})).map_err(|e| e.to_string())?;
        let mut request = self.http.post(&export.to).header("content-type", "application/json");
        if let Some(name) = &export.sign_with {
            let secret = secrets[name].as_str().ok_or_else(|| format!("no secret called {name} at this site"))?;
            request = request.header("x-ferrum-signature", signature(secret, &body).ok_or("could not sign")?);
        }
        let response = request.body(body).send().await.map_err(|e| e.to_string())?;
        if response.status().is_success() { Ok(()) } else { Err(format!("it answered {}", response.status())) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export(only: &[&str]) -> Export {
        Export { name: "e".into(), to: "http://x".into(), topic: None, only: only.iter().map(|o| o.to_string()).collect(), sign_with: None }
    }

    #[test]
    fn only_what_it_asked_for() {
        assert!(wanted(&export(&[]), "anything"));
        assert!(wanted(&export(&["temp.c"]), "temp.c"));
        assert!(!wanted(&export(&["temp.c"]), "temp.f"));
        assert!(wanted(&export(&["weight.*"]), "weight.kg"));
        assert!(!wanted(&export(&["weight.*"]), "height.m"));
    }

    #[test]
    fn a_signature_is_the_standard_one() {
        // RFC 4231, test case 2.
        assert_eq!(signature("Jefe", b"what do ya want for nothing?").unwrap(), "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }
}
