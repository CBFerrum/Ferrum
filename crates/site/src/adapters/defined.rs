//! Equipment reached by a definition (`ferrum_ops_contract::driver`): one engine, any protocol it
//! can describe. Each distinct address is one [`Link`], which stays connected, polls, and turns what
//! arrives into readings. Several devices can share a link, as they share a broker.
//!
//! The set of links changes while the site runs, because the platform can send new definitions, so
//! [`Defined::apply`] reconciles it: what is unchanged keeps its connection.
//!
//! A definition promises no more than an adapter may: a command is accepted once it has gone, ends
//! only when its status says so, and a write that was not answered leaves it unknown.

use crate::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use crate::adapters::transport::{self, Limits, Message, Out, Transport};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_ops_contract::driver::{self, Compiled, Decoded, Ending, Step, render, render_json};
use ferrum_ops_contract::edge::DeviceLink;
use ferrum_ops_contract::telemetry::Class;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

/// Readings waiting for the next read. A chatty device costs samples, never memory.
const MAX_WAITING: usize = 10_000;
/// Device codes seen on a shared connection that this site does not know, for a person to claim.
const MAX_STRANGERS: usize = 200;

struct Inflight {
    operation: String,
    device: String,
    external: Option<String>,
    ending: Ending,
    evidence: Option<Value>,
}

#[derive(Default)]
struct Shared {
    readings: VecDeque<Reading>,
    inflight: HashMap<Uuid, Inflight>,
    /// The last number reported per device and metric, for deadbands.
    reported: HashMap<(String, String), f64>,
    strangers: BTreeMap<String, ()>,
    last_error: Option<String>,
}

/// What a link has done, for the console and for `/metrics`.
#[derive(Default)]
pub struct Counters {
    pub messages: AtomicU64,
    pub readings: AtomicU64,
    pub dropped: AtomicU64,
    pub reconnects: AtomicU64,
}

pub struct Link {
    compiled: Arc<Compiled>,
    address: String,
    transport: Arc<dyn Transport>,
    devices: RwLock<Vec<DeviceLink>>,
    secrets: Value,
    shared: Mutex<Shared>,
    up: AtomicBool,
    counters: Counters,
    stop: watch::Sender<bool>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn class_of(name: Option<&str>) -> Class {
    match name {
        Some("Alarm") => Class::Alarm,
        Some("State") => Class::State,
        _ => Class::Sample,
    }
}

/// What a step becomes on the wire. Anything left unfilled refuses it here, before it can go.
fn outgoing(step: &Step, vars: &Value, expires: Option<u32>) -> Result<Out, String> {
    if let Some((method, path)) = step.method() {
        let body = step.send.as_ref().map(|b| body_of(b, vars)).transpose()?;
        // A body written as data is JSON. One written as text says what it is in the definition's headers.
        let headers = step.send.as_ref().filter(|b| !b.is_string()).map(|_| ("content-type".to_string(), "application/json".to_string())).into_iter().collect();
        return Ok(Out::Http { method: method.to_string(), path: render(path, vars)?, body, headers });
    }
    if let Some(table) = &step.read {
        return Ok(Out::Read { table: table.clone(), at: step.at.unwrap_or(0), count: step.count.unwrap_or(1) });
    }
    if !step.write.is_empty() {
        let mut values = Vec::new();
        for w in &step.write {
            let v = render_json(&w.value, vars)?;
            let word = v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).filter(|n| *n <= u64::from(u16::MAX));
            values.push((w.at, word.ok_or_else(|| format!("{v} does not fit a register"))? as u16));
        }
        return Ok(Out::Write(values));
    }
    let body = match (&step.send, &step.hex) {
        (Some(b), _) => body_of(b, vars)?,
        (None, Some(h)) => hex::decode(render(h, vars)?.replace(' ', "")).map_err(|e| e.to_string())?,
        (None, None) => Vec::new(),
    };
    Ok(match &step.topic {
        Some(topic) => Out::Publish { topic: render(topic, vars)?, body, expiry: expires },
        None => Out::Bytes(body),
    })
}

fn body_of(template: &Value, vars: &Value) -> Result<Vec<u8>, String> {
    Ok(match render_json(template, vars)? {
        Value::String(s) => s.into_bytes(),
        other => other.to_string().into_bytes(),
    })
}

impl Link {
    fn start(compiled: Arc<Compiled>, address: String, devices: Vec<DeviceLink>, secrets: Value, limits: &Limits) -> Result<Arc<Link>, String> {
        let d = &compiled.definition;
        let vars = json!({"secret": secrets, "device": devices.first().map(|x| x.code.clone()), "settings": devices.first().map(|x| x.settings.clone())});
        let mut headers = Vec::new();
        for (k, v) in &d.headers {
            headers.push((k.clone(), render(v, &vars)?));
        }
        let address = render(&address, &vars)?;
        let mut topics = d.subscribe.clone();
        for c in d.commands.values() {
            topics.extend(c.status.as_ref().and_then(|s| s.topic.as_ref()).map(|t| t.replace("{device}", "+")));
        }
        topics.sort();
        topics.dedup();
        let transport = transport::open(&address, d.frame.clone(), &topics, &headers, limits)?;
        let (stop, _) = watch::channel(false);
        let link = Arc::new(Link {
            compiled,
            address,
            transport,
            devices: RwLock::new(devices),
            secrets,
            shared: Mutex::default(),
            up: AtomicBool::new(false),
            counters: Counters::default(),
            stop,
        });
        link.clone().spawn();
        Ok(link)
    }

    fn spawn(self: Arc<Self>) {
        let (tx, mut rx) = mpsc::channel::<Message>(1024);
        let link = self.clone();
        let mut stopped = self.stop.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => return,
                    next = rx.recv() => match next {
                        Some(m) => link.heard(&m, None),
                        None => return,
                    },
                }
            }
        });
        let link = self.clone();
        let mut stopped = self.stop.subscribe();
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                let (ready_tx, mut ready_rx) = mpsc::channel::<()>(1);
                let run = link.transport.run(tx.clone(), ready_tx);
                let greeter = link.clone();
                let hello = async move {
                    if ready_rx.recv().await.is_some() {
                        greeter.up.store(true, Ordering::Relaxed);
                        greeter.greet().await;
                    }
                    std::future::pending::<()>().await;
                };
                let ended = tokio::select! {
                    _ = stopped.changed() => return,
                    r = run => r,
                    () = hello => Ok(()),
                };
                let was_up = link.up.swap(false, Ordering::Relaxed);
                if let Err(why) = ended {
                    locked(&link.shared).last_error = Some(why);
                }
                link.counters.reconnects.fetch_add(1, Ordering::Relaxed);
                failures = if was_up { 1 } else { failures.saturating_add(1) };
                let wait = Duration::from_millis(250u64.saturating_mul(1 << failures.min(7)));
                tokio::select! {
                    _ = stopped.changed() => return,
                    () = tokio::time::sleep(wait) => {}
                }
            }
        });
        for (i, poll) in self.compiled.definition.poll.iter().enumerate() {
            let (link, every) = (self.clone(), poll.every);
            let mut stopped = self.stop.subscribe();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = stopped.changed() => return,
                        _ = tick.tick() => {}
                    }
                    if link.up.load(Ordering::Relaxed) {
                        link.poll(i).await;
                    }
                }
            });
        }
    }

    async fn greet(&self) {
        let vars = self.vars(self.devices.read().unwrap_or_else(|e| e.into_inner()).first(), None, None);
        for step in &self.compiled.definition.on_connect {
            match outgoing(step, &vars, None) {
                Ok(out) => {
                    if let Ok(Some(reply)) = self.transport.send(out).await {
                        self.heard(&reply, None);
                    }
                }
                Err(e) => locked(&self.shared).last_error = Some(e),
            }
        }
    }

    /// A poll is said once for each device: a shared connection can ask after each of them, and the
    /// answer is theirs unless it says otherwise.
    async fn poll(&self, i: usize) {
        let devices: Vec<DeviceLink> = self.devices.read().unwrap_or_else(|e| e.into_inner()).clone();
        let step = &self.compiled.definition.poll[i].step;
        for device in &devices {
            let vars = self.vars(Some(device), None, None);
            let out = match outgoing(step, &vars, None) {
                Ok(o) => o,
                Err(e) => {
                    locked(&self.shared).last_error = Some(e);
                    continue;
                }
            };
            match self.transport.send(out).await {
                Ok(Some(reply)) if reply.status.is_none_or(|s| s < 400) => self.heard(&reply, Some(&device.code)),
                Ok(Some(reply)) => locked(&self.shared).last_error = Some(format!("{} answered {}", reply.topic, reply.status.unwrap_or(0))),
                Ok(None) => {}
                Err(Failure::NotSent(e) | Failure::Unknown(e)) => locked(&self.shared).last_error = Some(e),
            }
        }
    }

    fn vars(&self, device: Option<&DeviceLink>, command: Option<&Command>, external: Option<&str>) -> Value {
        json!({
            "device": device.map(|d| d.code.clone()),
            "settings": device.map(|d| d.settings.clone()).unwrap_or(Value::Null),
            "command": command.map(|c| c.id.to_string()),
            "operation": command.map(|c| c.operation.clone()),
            "payload": command.map(|c| c.payload.clone()).unwrap_or(Value::Null),
            "id": external,
            "secret": self.secrets,
        })
    }

    /// Takes in a message: what it says about the equipment, and what it says about commands.
    fn heard(&self, m: &Message, owner: Option<&str>) {
        self.counters.messages.fetch_add(1, Ordering::Relaxed);
        let decoded = if self.native() { native_decode(&m.body) } else { self.compiled.decode(&m.topic, &m.body) };
        let known: Vec<String> = self.devices.read().unwrap_or_else(|e| e.into_inner()).iter().map(|d| d.code.clone()).collect();
        let sole = owner.map(str::to_string).or_else(|| (known.len() == 1).then(|| known[0].clone()));
        let mut s = locked(&self.shared);
        for d in decoded {
            let (device, reading) = match d {
                Decoded::Metric { device, metric, value, unit, class, deadband } => {
                    let Some(code) = device.or_else(|| sole.clone()) else { continue };
                    if let Some(band) = deadband
                        && let Some(last) = s.reported.get(&(code.clone(), metric.clone()))
                        && (value - last).abs() < band
                    {
                        continue;
                    }
                    s.reported.insert((code.clone(), metric.clone()), value);
                    (code.clone(), Reading::Value { device: code, metric, value, unit, class: class_of(class.as_deref()), source_time: None })
                }
                Decoded::State { device, state } => {
                    let Some(code) = device.or_else(|| sole.clone()) else { continue };
                    (code.clone(), Reading::State { device: code, state, reason: None })
                }
                Decoded::Attribute { device, key, value } => {
                    let Some(code) = device.or_else(|| sole.clone()) else { continue };
                    (code.clone(), Reading::Attribute { device: code, key, value })
                }
                Decoded::Event { device, name, value } => {
                    let Some(code) = device.or_else(|| sole.clone()) else { continue };
                    (code.clone(), Reading::Event { device: code, name, value })
                }
            };
            if !known.contains(&device) {
                if s.strangers.len() < MAX_STRANGERS {
                    s.strangers.insert(device, ());
                }
                continue;
            }
            if s.readings.len() >= MAX_WAITING {
                s.readings.pop_front();
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
            s.readings.push_back(reading);
            self.counters.readings.fetch_add(1, Ordering::Relaxed);
        }
        // Whatever arrives may be the answer to something running.
        let ids: Vec<Uuid> = s.inflight.keys().copied().collect();
        for id in ids {
            let Some(f) = s.inflight.get_mut(&id) else { continue };
            let text = id.to_string();
            let names: Vec<&str> = std::iter::once(text.as_str()).chain(f.external.as_deref()).collect();
            let said = if self.native() { native_status(&m.body, &names) } else { self.compiled.status(&f.operation, &f.device, &m.topic, &m.body, &names) };
            if let Some(ending) = said {
                merge(f, ending, &m.body);
            }
        }
    }

    /// A process that defines nothing itself speaks the driver protocol.
    fn native(&self) -> bool {
        self.address.starts_with("process://") && self.compiled.definition.read.is_empty()
    }

    fn settings_of(&self, code: &str) -> Option<DeviceLink> {
        self.devices.read().unwrap_or_else(|e| e.into_inner()).iter().find(|d| d.code == code).cloned()
    }

    async fn dispatch(&self, c: &Command, device: &SiteDevice) -> Result<Dispatched, Failure> {
        let link = self.settings_of(&device.code);
        let vars = self.vars(link.as_ref(), Some(c), None);
        let expires = (c.expires_at - Utc::now()).num_seconds().max(1) as u32;
        let (out, id_path) = if self.native() {
            let line = json!({"op": "dispatch", "command": c.id, "operation": c.operation, "device": device.code, "payload": c.payload, "settings": vars["settings"], "expires_at": c.expires_at});
            (Out::Bytes(format!("{line}\n").into_bytes()), None)
        } else {
            let Some(def) = self.compiled.command(&c.operation) else {
                return Ok(Dispatched::Rejected(format!("{} has no {}", self.compiled.definition.name, c.operation)));
            };
            match outgoing(&def.step, &vars, Some(expires)) {
                Ok(o) => (o, def.id.clone()),
                Err(why) => return Ok(Dispatched::Rejected(why)),
            }
        };
        locked(&self.shared)
            .inflight
            .insert(c.id, Inflight { operation: c.operation.clone(), device: device.code.clone(), external: None, ending: Ending::Waiting, evidence: None });
        let sent = self.transport.send(out).await;
        let reply = match sent {
            Ok(r) => r,
            Err(Failure::NotSent(e)) => {
                locked(&self.shared).inflight.remove(&c.id);
                return Err(Failure::NotSent(e));
            }
            Err(e) => return Err(e),
        };
        if let Some(status) = reply.as_ref().and_then(|r| r.status) {
            // The far end read it and said no, or broke while reading it, which leaves it open.
            if (400..500).contains(&status) {
                locked(&self.shared).inflight.remove(&c.id);
                return Ok(Dispatched::Rejected(format!(
                    "it answered {status}: {}",
                    String::from_utf8_lossy(&reply.as_ref().map(|r| r.body.clone()).unwrap_or_default()).chars().take(200).collect::<String>()
                )));
            }
            if status >= 500 {
                return Err(Failure::Unknown(format!("it answered {status}")));
            }
        }
        let external = reply.as_ref().and_then(|r| {
            let path = id_path.as_ref()?;
            let body: Value = serde_json::from_slice(&r.body).ok()?;
            driver::at_path(&body, path).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
        });
        if id_path.is_some() && external.is_none() {
            return Err(Failure::Unknown("it took the command and did not say what it called it".into()));
        }
        if let Some(f) = locked(&self.shared).inflight.get_mut(&c.id) {
            f.external.clone_from(&external);
        }
        if let Some(reply) = &reply {
            self.heard(reply, Some(&device.code));
        }
        Ok(Dispatched::Accepted { external: external.or_else(|| Some(c.id.to_string())) })
    }

    async fn observe(&self, c: &Command, external: Option<&str>) -> Result<Observation, Failure> {
        // After a restart the journal knows the command and this does not yet.
        {
            let mut s = locked(&self.shared);
            s.inflight.entry(c.id).or_insert_with(|| Inflight {
                operation: c.operation.clone(),
                device: String::new(),
                external: external.filter(|e| *e != c.id.to_string()).map(str::to_string),
                ending: Ending::Waiting,
                evidence: None,
            });
        }
        if !self.native() {
            let Some(def) = self.compiled.command(&c.operation) else { return Ok(Observation::NotFound) };
            if def.once {
                return Ok(Observation::Completed(json!({"sent": true, "proved": "nothing beyond the send: this device gives no answer"})));
            }
            if let Some(step) = def.status.as_ref().and_then(|s| s.poll.as_ref()) {
                let (name, device) = {
                    let s = locked(&self.shared);
                    let f = s.inflight.get(&c.id);
                    (f.and_then(|f| f.external.clone()).or_else(|| external.map(str::to_string)), f.map(|f| f.device.clone()).unwrap_or_default())
                };
                let vars = self.vars(self.settings_of(&device).as_ref(), Some(c), name.as_deref());
                let out = outgoing(step, &vars, None).map_err(Failure::Unknown)?;
                if let Some(reply) = self.transport.send(out).await? {
                    self.heard(&reply, None);
                }
            }
        }
        let s = locked(&self.shared);
        let Some(f) = s.inflight.get(&c.id) else { return Ok(Observation::Waiting) };
        let evidence = f.evidence.clone().unwrap_or(Value::Null);
        Ok(match f.ending {
            Ending::Waiting => Observation::Waiting,
            Ending::Running => Observation::Running,
            Ending::Done => Observation::Completed(evidence),
            Ending::Failed => Observation::Failed(evidence),
            Ending::Aborted => Observation::Aborted(evidence),
        })
    }

    async fn find(&self, c: &Command) -> Result<Option<Observation>, Failure> {
        let Some(rule) = self.compiled.command(&c.operation).and_then(|d| d.find.as_ref()) else { return Ok(None) };
        let Some(step) = &rule.poll else { return Ok(None) };
        let out = outgoing(step, &self.vars(None, Some(c), None), None).map_err(Failure::Unknown)?;
        let Some(reply) = self.transport.send(out).await? else { return Ok(None) };
        if reply.status == Some(404) {
            return Ok(Some(Observation::NotFound));
        }
        let id = c.id.to_string();
        Ok(self.compiled.found(&c.operation, "", &reply.topic, &reply.body, &[id.as_str()]).map(|ending| {
            let evidence = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
            match ending {
                Ending::Waiting => Observation::Waiting,
                Ending::Running => Observation::Running,
                Ending::Done => Observation::Completed(evidence),
                Ending::Failed => Observation::Failed(evidence),
                Ending::Aborted => Observation::Aborted(evidence),
            }
        }))
    }

    async fn cancel(&self, c: &Command, external: Option<&str>) -> Result<(), Failure> {
        if self.native() {
            let line = json!({"op": "cancel", "command": c.id});
            return self.transport.send(Out::Bytes(format!("{line}\n").into_bytes())).await.map(drop);
        }
        let step = self
            .compiled
            .command(&c.operation)
            .and_then(|d| d.cancel.as_ref())
            .ok_or_else(|| Failure::NotSent("this definition has no way to stop it".into()))?;
        let name = locked(&self.shared).inflight.get(&c.id).and_then(|f| f.external.clone()).or_else(|| external.map(str::to_string));
        let out = outgoing(step, &self.vars(None, Some(c), name.as_deref()), None).map_err(Failure::NotSent)?;
        self.transport.send(out).await.map(drop)
    }

    fn take(&self) -> Vec<Reading> {
        locked(&self.shared).readings.drain(..).collect()
    }

    fn health(&self) -> LinkHealth {
        let s = locked(&self.shared);
        LinkHealth {
            address: redact(&self.address),
            definition: self.compiled.definition.name.clone(),
            up: self.up.load(Ordering::Relaxed),
            devices: self.devices.read().unwrap_or_else(|e| e.into_inner()).len(),
            messages: self.counters.messages.load(Ordering::Relaxed),
            readings: self.counters.readings.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            reconnects: self.counters.reconnects.load(Ordering::Relaxed),
            last_error: s.last_error.clone(),
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

fn merge(f: &mut Inflight, ending: Ending, body: &[u8]) {
    // A last word stands: a late "running" does not reopen what ended.
    if matches!(f.ending, Ending::Done | Ending::Failed | Ending::Aborted) {
        return;
    }
    if ending == Ending::Waiting && f.ending == Ending::Running {
        return;
    }
    f.ending = ending;
    if matches!(ending, Ending::Done | Ending::Failed | Ending::Aborted) {
        f.evidence = Some(serde_json::from_slice(body).unwrap_or_else(|_| json!({"said": String::from_utf8_lossy(body)})));
    }
}

/// What a driver says, when it has no definition but the protocol itself.
fn native_decode(line: &[u8]) -> Vec<Decoded> {
    let Ok(v) = serde_json::from_slice::<Value>(line) else { return vec![] };
    let device = v["device"].as_str().map(str::to_string);
    let text = |k: &str| v[k].as_str().map(str::to_string);
    if let (Some(metric), Some(value)) = (text("metric"), v["value"].as_f64()) {
        return vec![Decoded::Metric { device, metric, value, unit: text("unit"), class: text("class"), deadband: None }];
    }
    if let Some(state) = text("state") {
        return vec![Decoded::State { device, state }];
    }
    if let Some(key) = text("attribute") {
        return vec![Decoded::Attribute { device, key, value: v["value"].clone() }];
    }
    if let Some(name) = text("event") {
        return vec![Decoded::Event { device, name, value: v["value"].clone() }];
    }
    vec![]
}

fn native_status(line: &[u8], names: &[&str]) -> Option<Ending> {
    let v: Value = serde_json::from_slice(line).ok()?;
    let id = v["command"].as_str()?;
    if !names.contains(&id) {
        return None;
    }
    Some(match v["status"].as_str()? {
        "running" => Ending::Running,
        "done" => Ending::Done,
        "failed" => Ending::Failed,
        "aborted" => Ending::Aborted,
        _ => Ending::Waiting,
    })
}

/// An address fit to show: no passwords.
fn redact(address: &str) -> String {
    match (address.split_once("://"), address.rsplit_once('@')) {
        (Some((scheme, _)), Some((_, tail))) => format!("{scheme}://…@{tail}"),
        _ => address.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LinkHealth {
    pub address: String,
    pub definition: String,
    pub up: bool,
    pub devices: usize,
    pub messages: u64,
    pub readings: u64,
    pub dropped: u64,
    pub reconnects: u64,
    pub last_error: Option<String>,
}

/// Every link this site holds.
#[derive(Default)]
pub struct Defined {
    links: RwLock<HashMap<(String, String), Arc<Link>>>,
    by_device: RwLock<HashMap<String, Arc<Link>>>,
    pub limits: Limits,
    pub secrets: Value,
}

impl Defined {
    pub fn new(limits: Limits, secrets: Value) -> Defined {
        Defined { limits, secrets, ..Default::default() }
    }

    /// Brings the links to what is wanted. A link whose definition and address have not changed
    /// keeps its connection, and anything running through it. A device that cannot start is said
    /// plainly and the rest carry on: one bad definition must not take a site down.
    pub fn apply(&self, definitions: &BTreeMap<String, String>, wanted: &[DeviceLink]) -> Vec<String> {
        let mut problems = Vec::new();
        let mut compiled: HashMap<&str, Arc<Compiled>> = HashMap::new();
        for (name, text) in definitions {
            match driver::parse(text) {
                Ok(c) => {
                    compiled.insert(name, Arc::new(c));
                }
                Err(e) => problems.push(format!("{name}: {e}")),
            }
        }
        let mut groups: BTreeMap<(String, String), Vec<DeviceLink>> = BTreeMap::new();
        for d in wanted {
            let Some(c) = compiled.get(d.definition.as_str()) else {
                problems.push(format!("{}: no definition called {}", d.code, d.definition));
                continue;
            };
            let Some(address) = d.connect.clone().or_else(|| c.definition.connect.clone()) else {
                problems.push(format!("{}: it says nowhere to connect", d.code));
                continue;
            };
            groups.entry((c.digest.clone(), address)).or_default().push(d.clone());
        }
        let mut links = self.links.write().unwrap_or_else(|e| e.into_inner());
        links.retain(|key, _| groups.contains_key(key));
        let mut by_device = HashMap::new();
        for ((digest, address), devices) in groups {
            let key = (digest, address.clone());
            let link = match links.get(&key) {
                Some(link) => {
                    *link.devices.write().unwrap_or_else(|e| e.into_inner()) = devices.clone();
                    link.clone()
                }
                None => {
                    let Some(c) = devices.first().and_then(|d| compiled.get(d.definition.as_str())) else { continue };
                    match Link::start(c.clone(), address, devices.clone(), self.secrets.clone(), &self.limits) {
                        Ok(link) => {
                            links.insert(key, link.clone());
                            link
                        }
                        Err(e) => {
                            problems.push(format!("{}: {e}", devices.iter().map(|d| d.code.as_str()).collect::<Vec<_>>().join(", ")));
                            continue;
                        }
                    }
                }
            };
            for d in devices {
                by_device.insert(d.code, link.clone());
            }
        }
        *self.by_device.write().unwrap_or_else(|e| e.into_inner()) = by_device;
        problems
    }

    pub fn reaches(&self, code: &str) -> bool {
        self.by_device.read().unwrap_or_else(|e| e.into_inner()).contains_key(code)
    }

    fn link_of(&self, code: &str) -> Option<Arc<Link>> {
        self.by_device.read().unwrap_or_else(|e| e.into_inner()).get(code).cloned()
    }

    fn link_for(&self, c: &Command) -> Option<Arc<Link>> {
        let links = self.by_device.read().unwrap_or_else(|e| e.into_inner());
        // After a restart the command's device is known by the journal, not here: its operation
        // and who holds it in flight say which link it belongs to.
        links
            .values()
            .find(|l| locked(&l.shared).inflight.contains_key(&c.id))
            .cloned()
            .or_else(|| links.values().find(|l| l.compiled.command(&c.operation).is_some() || l.native()).cloned())
    }

    pub fn links(&self) -> Vec<LinkHealth> {
        let mut out: Vec<LinkHealth> = self.links.read().unwrap_or_else(|e| e.into_inner()).values().map(|l| l.health()).collect();
        out.sort_by(|a, b| a.address.cmp(&b.address));
        out
    }

    /// Device codes heard on a shared connection that this site does not know.
    pub fn strangers(&self) -> Vec<String> {
        let links = self.links.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<String> = links.values().flat_map(|l| locked(&l.shared).strangers.keys().cloned().collect::<Vec<_>>()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// How many commands this device may run at once.
    pub fn concurrency(&self, code: &str) -> Option<u32> {
        let link = self.link_of(code)?;
        link.settings_of(code).map(|d| d.concurrency.max(1))
    }
}

#[async_trait]
impl Adapter for Defined {
    async fn dispatch(&self, command: &Command, device: &SiteDevice) -> Result<Dispatched, Failure> {
        match self.link_of(&device.code) {
            Some(link) => link.dispatch(command, device).await,
            None => Ok(Dispatched::Rejected(format!("no definition reaches {}", device.code))),
        }
    }

    async fn observe(&self, command: &Command, external: Option<&str>) -> Result<Observation, Failure> {
        match self.link_for(command) {
            Some(link) => link.observe(command, external).await,
            None => Err(Failure::Unknown("no link holds this any more".into())),
        }
    }

    async fn find(&self, command: &Command) -> Result<Option<Observation>, Failure> {
        match self.link_for(command) {
            Some(link) => link.find(command).await,
            None => Ok(None),
        }
    }

    async fn cancel(&self, command: &Command, external: Option<&str>) -> Result<(), Failure> {
        match self.link_for(command) {
            Some(link) => link.cancel(command, external).await,
            None => Err(Failure::NotSent("no link holds this any more".into())),
        }
    }

    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        let links: Vec<Arc<Link>> = self.links.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        Ok(links.iter().flat_map(|l| l.take()).collect())
    }

    fn health(&self) -> String {
        let links = self.links();
        format!("{} of {} links up", links.iter().filter(|l| l.up).count(), links.len())
    }
}
