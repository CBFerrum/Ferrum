//! Flows: logic a person draws, run at the gateway so that it keeps working with the link down.
//! Messages leave sources, pass through logic and reach actions, as in Node-RED or a ThingsBoard
//! rule chain, with three differences that matter on a plant floor.
//!
//! * There is no code. Every node is one of the kinds below, so a flow can be read, reviewed and
//!   simulated, and nothing in one can do what its nodes do not say.
//! * It cannot loop. A flow is checked to have no cycles, and each message's work is bounded, so a
//!   bad one costs a little time and cannot hang a gateway.
//! * It cannot move anything. The strongest thing a flow can do is ask for a command, which goes
//!   through the same ledger as a person's: commissioned operations, authority, approval or an
//!   automation envelope. Safety functions do not belong here (`docs/edge-flows.md`).
//!
//! Pure: this checks and evaluates. The same code simulates a flow on the platform and runs it at
//! the site, so what a reviewer saw is what runs.

use crate::driver::{at_path, parse_duration, render_json};
use chrono::{DateTime, TimeDelta, Utc};
use ferrum_calc::formula::Formula;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

pub const MAX_NODES: usize = 500;
pub const MAX_EDGES: usize = 2_000;
/// Steps one message may cost. No cycle can exist, so this bounds fan-out, not a loop.
const MAX_STEPS: usize = 5_000;
/// Devices a node keeps memory for.
const MAX_KEYS: usize = 5_000;
/// What a window or a delay holds per device.
const MAX_HELD: usize = 10_000;
/// The longest stretch of time a simulation will walk through, second by second.
pub const MAX_SIMULATED_SECS: i64 = 6 * 3600;

fn period<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let v = Value::deserialize(d)?;
    let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    parse_duration(&text).ok_or_else(|| serde::de::Error::custom(format!("{text} is not a duration: try 500ms, 5s or 2m")))
}

fn some_period<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    period(d).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Comparison {
    #[serde(rename = ">")]
    Above,
    #[serde(rename = ">=")]
    AtLeast,
    #[serde(rename = "<")]
    Below,
    #[serde(rename = "<=")]
    AtMost,
    #[serde(rename = "==")]
    Is,
    #[serde(rename = "!=")]
    IsNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stat {
    Avg,
    Min,
    Max,
    Sum,
    Count,
    /// The newest less the oldest.
    Delta,
}

/// What a node is. Sources first, then logic, then actions.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Kind {
    /// A number a device reported. `device` is a code, `PRESS-*`, or empty for any.
    Metric {
        #[serde(default)]
        device: String,
        metric: String,
    },
    /// A device's state changing.
    State {
        #[serde(default)]
        device: String,
    },
    Event {
        #[serde(default)]
        device: String,
        name: String,
    },
    Attribute {
        #[serde(default)]
        device: String,
        key: String,
    },
    /// Fires on its own, with the time as its value.
    Timer {
        #[serde(deserialize_with = "period")]
        every: Duration,
    },

    /// Sends the message out of `yes` or `no`.
    Compare {
        op: Comparison,
        value: Value,
    },
    Range {
        min: f64,
        max: f64,
    },
    /// Only what is different from last time, or by at least `by`.
    Changed {
        #[serde(default)]
        by: Option<f64>,
    },
    /// Passes one message once `in` has held this long with nothing arriving at `reset`.
    Hold {
        #[serde(rename = "for", deserialize_with = "period")]
        hold: Duration,
    },
    /// Fires when nothing has arrived for this long.
    Silence {
        #[serde(rename = "for", deserialize_with = "period")]
        quiet: Duration,
    },
    /// At most one message this often.
    Limit {
        #[serde(deserialize_with = "period")]
        every: Duration,
    },
    Delay {
        #[serde(rename = "for", deserialize_with = "period")]
        wait: Duration,
    },
    /// Passes the count when `of` messages have arrived within this time.
    Count {
        of: usize,
        #[serde(deserialize_with = "period")]
        within: Duration,
    },
    /// Replaces a number with a statistic of the last stretch of them.
    Window {
        #[serde(deserialize_with = "period")]
        over: Duration,
        stat: Stat,
    },
    /// `[value] * 1.8 + 32`. The fields of a value that has them are there by name too.
    Math {
        formula: String,
    },
    /// Waits for every wired input, then passes them together, by input name, or what `formula`
    /// makes of them. Values older than `fresh` do not count: a stale reading is not a reading.
    Combine {
        #[serde(default)]
        formula: Option<String>,
        #[serde(default, deserialize_with = "some_period")]
        fresh: Option<Duration>,
        /// Across devices, instead of for each device on its own.
        #[serde(default)]
        across: bool,
    },
    /// `yes` when every wired input was last true, `no` when that stops being so. It says each
    /// once. `any: true` asks for one instead of all.
    All {
        #[serde(default)]
        any: bool,
        #[serde(default, deserialize_with = "some_period")]
        fresh: Option<Duration>,
        #[serde(default)]
        across: bool,
    },
    /// Remembers: `set` turns it on and `reset` off. Says `yes` or `no` when that changes.
    Latch,
    /// What a raw value means: `{0: Idle, 1: Running}`.
    Map {
        map: BTreeMap<String, Value>,
        #[serde(default)]
        other: Option<Value>,
    },
    /// Sends the message out of the port named by its value, or `other`.
    Switch {
        cases: Vec<String>,
    },
    /// Replaces the value. `{value}` and `{device}` work inside.
    Set {
        value: Value,
    },
    /// Takes one thing out of a value: `$.a.b`.
    Field {
        path: String,
    },

    /// Raises at `in`, clears at `clear`, once each, for each device.
    Alarm {
        name: String,
        #[serde(default = "warning")]
        severity: String,
        #[serde(default)]
        device: Option<String>,
    },
    RecordEvent {
        name: String,
    },
    RecordMetric {
        name: String,
        #[serde(default)]
        unit: Option<String>,
    },
    RecordAttribute {
        key: String,
    },
    /// Tells the people who look after this. `{value}` and `{device}` work inside.
    Notify {
        title: String,
        #[serde(default = "a_minute", deserialize_with = "period")]
        every: Duration,
    },
    /// Hands the message to one of the site's exports.
    Export {
        to: String,
    },
    /// Asks for a command. It goes through the ledger, and goes nowhere until someone or something
    /// entitled has allowed it. At most once per `every`, however often this is reached.
    Command {
        operation: String,
        #[serde(default)]
        device: Option<String>,
        #[serde(default)]
        payload: Value,
        #[serde(default = "a_minute", deserialize_with = "period")]
        every: Duration,
        #[serde(default = "five_minutes", deserialize_with = "period")]
        expires: Duration,
    },
}

fn warning() -> String {
    "Warning".into()
}

fn a_minute() -> Duration {
    Duration::from_secs(60)
}

fn five_minutes() -> Duration {
    Duration::from_secs(300)
}

impl Kind {
    fn is_source(&self) -> bool {
        matches!(self, Kind::Metric { .. } | Kind::State { .. } | Kind::Event { .. } | Kind::Attribute { .. } | Kind::Timer { .. })
    }

    fn is_action(&self) -> bool {
        matches!(
            self,
            Kind::Alarm { .. }
                | Kind::RecordEvent { .. }
                | Kind::RecordMetric { .. }
                | Kind::RecordAttribute { .. }
                | Kind::Notify { .. }
                | Kind::Export { .. }
                | Kind::Command { .. }
        )
    }

    /// What it can send out of.
    fn ports(&self) -> Vec<String> {
        match self {
            Kind::Compare { .. } | Kind::Range { .. } | Kind::All { .. } | Kind::Latch => vec!["yes".into(), "no".into()],
            Kind::Switch { cases } => cases.iter().cloned().chain(["other".to_string()]).collect(),
            k if k.is_action() => vec![],
            _ => vec!["out".into()],
        }
    }

    /// What it takes, or `None` for inputs named by whoever wires them.
    fn takes(&self) -> Option<&'static [&'static str]> {
        match self {
            Kind::Combine { .. } | Kind::All { .. } => None,
            Kind::Hold { .. } => Some(&["in", "reset"]),
            Kind::Latch => Some(&["set", "reset"]),
            Kind::Alarm { .. } => Some(&["in", "clear"]),
            k if k.is_source() => Some(&[]),
            _ => Some(&["in"]),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: String,
    pub kind: Kind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    from: String,
    #[serde(default = "out")]
    port: String,
    to: String,
    #[serde(default = "into")]
    input: String,
}

fn out() -> String {
    "out".into()
}

fn into() -> String {
    "in".into()
}

/// One thing a device did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub device: String,
    #[serde(default)]
    pub metric: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub attribute: Option<String>,
    #[serde(default)]
    pub value: Value,
}

/// What a flow did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    Alarm { node: String, name: String, severity: String, device: Option<String>, raised: bool, value: Option<f64> },
    Event { device: Option<String>, name: String, value: Value },
    Metric { device: Option<String>, name: String, value: f64, unit: Option<String> },
    Attribute { device: Option<String>, key: String, value: Value },
    Notify { device: Option<String>, title: String },
    Export { to: String, body: Value },
    Command { node: String, device: Option<String>, operation: String, payload: Value, expires_secs: i64 },
}

/// A message passing through one node, for a simulation to show.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trace {
    pub node: String,
    pub device: Option<String>,
    pub value: Value,
    /// Where it left by. Empty if it stopped here.
    pub ports: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Output {
    pub effects: Vec<Effect>,
    pub trace: Vec<Trace>,
    /// Why something did not happen, for the person who drew it.
    pub problems: Vec<String>,
}

/// What a node remembers for one device.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Memory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    on: bool,
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    held: VecDeque<(DateTime<Utc>, Value)>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    inputs: BTreeMap<String, (DateTime<Utc>, Value)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
}

/// Everything a running flow remembers, by node and device. It survives a restart in the journal,
/// so a hold half way through keeps counting and a raised alarm is not raised twice.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    nodes: BTreeMap<String, BTreeMap<String, Memory>>,
}

#[derive(Debug)]
struct Wire {
    port: String,
    to: usize,
    input: String,
}

/// A flow that passed [`parse`].
#[derive(Debug)]
pub struct Compiled {
    pub nodes: Vec<Node>,
    pub digest: String,
    wires: Vec<Vec<Wire>>,
    /// By node, the inputs something is wired to.
    wired: Vec<BTreeSet<String>>,
    formulas: Vec<Option<Formula>>,
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A value as a name. Readings are floating point, and 1.0 from a device is the 1 of a person's map.
fn word(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1e15).map_or_else(|| n.to_string(), |f| (f as i64).to_string()),
        other => other.to_string(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty() && s != "false",
        _ => true,
    }
}

fn compares(op: Comparison, left: &Value, right: &Value) -> Option<bool> {
    if let (Some(a), Some(b)) = (number(left), number(right)) {
        return Some(match op {
            Comparison::Above => a > b,
            Comparison::AtLeast => a >= b,
            Comparison::Below => a < b,
            Comparison::AtMost => a <= b,
            Comparison::Is => a == b,
            Comparison::IsNot => a != b,
        });
    }
    match op {
        Comparison::Is => Some(word(left) == word(right)),
        Comparison::IsNot => Some(word(left) != word(right)),
        // Text has no order worth acting on, so this says nothing rather than something wrong.
        _ => None,
    }
}

fn device_matches(pattern: &str, device: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => device.starts_with(prefix),
        None => pattern.is_empty() || pattern == device,
    }
}

fn span(d: Duration) -> TimeDelta {
    TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX)
}

/// Reads and checks a flow, `{nodes: [...], edges: [...]}`, as JSON or YAML. Every error names the
/// node, so a person can fix it from the message. A draft can be saved with faults: this is what
/// stands between a draft and a review.
pub fn parse(text: &str) -> Result<Compiled, String> {
    let body: Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    let raw = body["nodes"].as_array().ok_or("a flow needs nodes")?;
    if raw.len() > MAX_NODES {
        return Err(format!("a flow has at most {MAX_NODES} nodes"));
    }
    let mut nodes = Vec::with_capacity(raw.len());
    for (i, n) in raw.iter().enumerate() {
        let mut fields = n.as_object().cloned().ok_or_else(|| format!("node {i} is not one"))?;
        let id = fields.remove("id").and_then(|v| v.as_str().map(str::to_string)).filter(|s| !s.is_empty()).ok_or_else(|| format!("node {i} has no id"))?;
        // Where it sits on the canvas, and what a person called it, mean nothing here.
        for layout in ["x", "y", "label"] {
            fields.remove(layout);
        }
        let kind: Kind = serde_json::from_value(Value::Object(fields)).map_err(|e| format!("{id}: {e}"))?;
        if nodes.iter().any(|m: &Node| m.id == id) {
            return Err(format!("{id} is there twice"));
        }
        nodes.push(Node { id, kind });
    }
    let edges: Vec<Edge> = match body.get("edges") {
        Some(e) if !e.is_null() => serde_json::from_value(e.clone()).map_err(|e| format!("edges: {e}"))?,
        _ => Vec::new(),
    };
    if edges.len() > MAX_EDGES {
        return Err(format!("a flow has at most {MAX_EDGES} wires"));
    }
    let index = |id: &str| nodes.iter().position(|n| n.id == id);
    let mut wires: Vec<Vec<Wire>> = nodes.iter().map(|_| Vec::new()).collect();
    let mut wired: Vec<BTreeSet<String>> = nodes.iter().map(|_| BTreeSet::new()).collect();
    for e in &edges {
        let from = index(&e.from).ok_or_else(|| format!("a wire leaves {}, which is not here", e.from))?;
        let to = index(&e.to).ok_or_else(|| format!("a wire reaches {}, which is not here", e.to))?;
        if !nodes[from].kind.ports().contains(&e.port) {
            return Err(format!("{} has no way out called {}", e.from, e.port));
        }
        if nodes[to].kind.takes().is_some_and(|t| !t.contains(&e.input.as_str())) {
            return Err(format!("{} takes nothing at {}", e.to, e.input));
        }
        wires[from].push(Wire { port: e.port.clone(), to, input: e.input.clone() });
        wired[to].insert(e.input.clone());
    }
    // Kahn's: what cannot be ordered is on a cycle, which is exactly what a flow may not have.
    let mut waiting: Vec<usize> = wired.iter().map(|_| 0).collect();
    for out in &wires {
        for w in out {
            waiting[w.to] += 1;
        }
    }
    let mut ready: Vec<usize> = (0..nodes.len()).filter(|i| waiting[*i] == 0).collect();
    let mut ordered = 0;
    while let Some(i) = ready.pop() {
        ordered += 1;
        for w in &wires[i] {
            waiting[w.to] -= 1;
            if waiting[w.to] == 0 {
                ready.push(w.to);
            }
        }
    }
    if ordered != nodes.len() {
        let on: Vec<&str> = nodes.iter().zip(&waiting).filter(|(_, w)| **w > 0).map(|(n, _)| n.id.as_str()).collect();
        return Err(format!("a flow cannot go round in a circle, and these do: {}", on.join(", ")));
    }
    let mut reached: Vec<bool> = nodes.iter().map(|n| n.kind.is_source()).collect();
    let mut leads: Vec<usize> = (0..nodes.len()).filter(|i| reached[*i]).collect();
    while let Some(i) = leads.pop() {
        for w in &wires[i] {
            if !reached[w.to] {
                reached[w.to] = true;
                leads.push(w.to);
            }
        }
    }
    let mut formulas = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        if !reached[i] {
            return Err(format!("{}: nothing reaches it", n.id));
        }
        if wires[i].is_empty() && !n.kind.is_action() {
            return Err(format!("{}: nothing comes of it: wire it to something", n.id));
        }
        let missing = |input: &str| !wired[i].contains(input);
        match &n.kind {
            Kind::Hold { .. } if missing("reset") => return Err(format!("{}: say what resets it, or it holds for ever", n.id)),
            Kind::Latch if missing("reset") || missing("set") => return Err(format!("{}: wire both set and reset", n.id)),
            Kind::Alarm { .. } if missing("clear") => return Err(format!("{}: say what clears it, or it stays raised for ever", n.id)),
            Kind::Alarm { severity, .. } if !["Info", "Warning", "Critical"].contains(&severity.as_str()) => {
                return Err(format!("{}: severity is Info, Warning or Critical", n.id));
            }
            Kind::Range { min, max } if min > max => return Err(format!("{}: {min} is above {max}", n.id)),
            Kind::Count { of: 0, .. } => return Err(format!("{}: count at least one", n.id)),
            Kind::Command { operation, .. } if operation.is_empty() => return Err(format!("{}: say which operation", n.id)),
            Kind::Switch { cases } if cases.is_empty() => return Err(format!("{}: give it some cases", n.id)),
            _ => {}
        }
        let text = match &n.kind {
            Kind::Math { formula } => Some(formula),
            Kind::Combine { formula, .. } => formula.as_ref(),
            _ => None,
        };
        formulas.push(text.map(|t| Formula::parse(t, &|name| Ok(name.to_string()))).transpose().map_err(|e| format!("{}: {e}", n.id))?);
    }
    if !nodes.iter().any(|n| n.kind.is_action()) {
        return Err("this flow does nothing: add an action".into());
    }
    Ok(Compiled { nodes, digest: hex::encode(Sha256::digest(text.as_bytes())), wires, wired, formulas })
}

struct Run<'a> {
    state: &'a mut State,
    at: DateTime<Utc>,
    out: Output,
}

impl Run<'_> {
    fn memory(&mut self, node: &str, key: &str) -> Option<&mut Memory> {
        let keys = self.state.nodes.entry(node.to_string()).or_default();
        if !keys.contains_key(key) && keys.len() >= MAX_KEYS {
            self.out.problems.push(format!("{node} is keeping track of too many devices: {key} was left out"));
            return None;
        }
        Some(keys.entry(key.to_string()).or_default())
    }
}

/// A message's way through a node: out of which ports, with what.
type Leaving = Vec<(String, Value)>;

fn out_of(port: &str, value: Value) -> Leaving {
    vec![(port.to_string(), value)]
}

fn statistic(stat: Stat, numbers: &[f64]) -> Option<f64> {
    let first = *numbers.first()?;
    let sum: f64 = numbers.iter().sum();
    Some(match stat {
        Stat::Avg => sum / numbers.len() as f64,
        Stat::Min => numbers.iter().copied().fold(first, f64::min),
        Stat::Max => numbers.iter().copied().fold(first, f64::max),
        Stat::Sum => sum,
        Stat::Count => numbers.len() as f64,
        Stat::Delta => numbers[numbers.len() - 1] - first,
    })
}

fn calculate(formula: &Formula, row: &Map<String, Value>) -> Option<Value> {
    formula.eval(row).and_then(|d| d.to_f64()).map(|n| json!(n))
}

fn row_of(value: &Value) -> Map<String, Value> {
    let mut row = value.as_object().cloned().unwrap_or_default();
    row.insert("value".into(), value.clone());
    row
}

fn fill(template: &Value, value: &Value, device: Option<&str>) -> Result<Value, String> {
    render_json(template, &json!({"value": value, "device": device}))
}

impl Compiled {
    fn step(&self, i: usize, input: &str, device: Option<&str>, value: Value, run: &mut Run<'_>) -> Leaving {
        let node = &self.nodes[i];
        let id = node.id.as_str();
        let at = run.at;
        let key = device.unwrap_or_default();
        match &node.kind {
            Kind::Metric { .. } | Kind::State { .. } | Kind::Event { .. } | Kind::Attribute { .. } | Kind::Timer { .. } => out_of("out", value),
            Kind::Compare { op, value: right } => match compares(*op, &value, right) {
                Some(true) => out_of("yes", value),
                Some(false) => out_of("no", value),
                None => vec![],
            },
            Kind::Range { min, max } => match number(&value) {
                Some(n) if n >= *min && n <= *max => out_of("yes", value),
                Some(_) => out_of("no", value),
                None => vec![],
            },
            Kind::Changed { by } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                let moved = match (&m.last, by, number(&value)) {
                    (None, _, _) => true,
                    (Some(last), Some(by), Some(n)) => number(last).is_none_or(|l| (n - l).abs() >= *by),
                    (Some(last), _, _) => *last != value,
                };
                if !moved {
                    return vec![];
                }
                m.last = Some(value.clone());
                out_of("out", value)
            }
            Kind::Hold { hold } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                if input == "reset" {
                    (m.since, m.on, m.last) = (None, false, None);
                    return vec![];
                }
                m.since.get_or_insert(at);
                m.last = Some(value);
                m.device = device.map(str::to_string);
                Self::due(m, span(*hold), at)
            }
            Kind::Silence { .. } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                (m.since, m.on, m.last) = (Some(at), false, Some(value));
                m.device = device.map(str::to_string);
                vec![]
            }
            Kind::Limit { every } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                if m.since.is_some_and(|last| at - last < span(*every)) {
                    return vec![];
                }
                m.since = Some(at);
                out_of("out", value)
            }
            Kind::Delay { wait } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                if m.held.len() >= MAX_HELD {
                    m.held.pop_front();
                    run.out.problems.push(format!("{id} is holding too much: its oldest message was dropped"));
                    return vec![];
                }
                m.held.push_back((at + span(*wait), value));
                m.device = device.map(str::to_string);
                vec![]
            }
            Kind::Count { of, within } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                let from = at - span(*within);
                while m.held.front().is_some_and(|(t, _)| *t < from) {
                    m.held.pop_front();
                }
                if m.held.len() < MAX_HELD {
                    m.held.push_back((at, Value::Null));
                }
                if m.held.len() < *of {
                    return vec![];
                }
                m.held.clear();
                out_of("out", json!(of))
            }
            Kind::Window { over, stat } => {
                let Some(n) = number(&value) else { return vec![] };
                let Some(m) = run.memory(id, key) else { return vec![] };
                let from = at - span(*over);
                while m.held.front().is_some_and(|(t, _)| *t < from) || m.held.len() >= MAX_HELD {
                    m.held.pop_front();
                }
                m.held.push_back((at, json!(n)));
                let numbers: Vec<f64> = m.held.iter().filter_map(|(_, v)| v.as_f64()).collect();
                statistic(*stat, &numbers).map_or(vec![], |s| out_of("out", json!(s)))
            }
            Kind::Math { .. } => self.formulas[i].as_ref().and_then(|f| calculate(f, &row_of(&value))).map_or(vec![], |v| out_of("out", v)),
            Kind::Combine { fresh, across, .. } => {
                let key = if *across { "" } else { key };
                let Some(m) = run.memory(id, key) else { return vec![] };
                m.inputs.insert(input.to_string(), (at, value));
                let values: Map<String, Value> =
                    m.inputs.iter().filter(|(_, (when, _))| fresh.is_none_or(|f| at - *when <= span(f))).map(|(k, (_, v))| (k.clone(), v.clone())).collect();
                if !self.wired[i].iter().all(|w| values.contains_key(w)) {
                    return vec![];
                }
                match &self.formulas[i] {
                    Some(f) => calculate(f, &values).map_or(vec![], |v| out_of("out", v)),
                    None => out_of("out", Value::Object(values)),
                }
            }
            Kind::All { any, fresh, across } => {
                let key = if *across { "" } else { key };
                let Some(m) = run.memory(id, key) else { return vec![] };
                m.inputs.insert(input.to_string(), (at, value));
                let holds = |name: &String| m.inputs.get(name).is_some_and(|(when, v)| truthy(v) && fresh.is_none_or(|f| at - *when <= span(f)));
                let now = if *any { self.wired[i].iter().any(holds) } else { self.wired[i].iter().all(holds) };
                let first = m.last.is_none();
                if !first && m.on == now {
                    return vec![];
                }
                (m.on, m.last) = (now, Some(json!(now)));
                out_of(if now { "yes" } else { "no" }, json!(now))
            }
            Kind::Latch => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                let now = input == "set";
                if m.on == now {
                    return vec![];
                }
                m.on = now;
                out_of(if now { "yes" } else { "no" }, value)
            }
            Kind::Map { map, other } => match map.get(&word(&value)).or(other.as_ref()) {
                Some(v) => out_of("out", v.clone()),
                None => vec![],
            },
            Kind::Switch { cases } => {
                let said = word(&value);
                out_of(if cases.contains(&said) { &said } else { "other" }, value)
            }
            Kind::Set { value: template } => match fill(template, &value, device) {
                Ok(v) => out_of("out", v),
                Err(why) => {
                    run.out.problems.push(format!("{id}: {why}"));
                    vec![]
                }
            },
            Kind::Field { path } => at_path(&value, path).cloned().map_or(vec![], |v| out_of("out", v)),
            Kind::Alarm { name, severity, device: fixed } => {
                let device = fixed.as_deref().or(device);
                let Some(m) = run.memory(id, device.unwrap_or_default()) else { return vec![] };
                let raised = input != "clear";
                if m.on == raised {
                    return vec![];
                }
                m.on = raised;
                let effect = Effect::Alarm {
                    node: id.to_string(),
                    name: name.clone(),
                    severity: severity.clone(),
                    device: device.map(str::to_string),
                    raised,
                    value: number(&value),
                };
                run.out.effects.push(effect);
                vec![]
            }
            Kind::RecordEvent { name } => {
                run.out.effects.push(Effect::Event { device: device.map(str::to_string), name: name.clone(), value });
                vec![]
            }
            Kind::RecordMetric { name, unit } => {
                if let Some(n) = number(&value) {
                    run.out.effects.push(Effect::Metric { device: device.map(str::to_string), name: name.clone(), value: n, unit: unit.clone() });
                }
                vec![]
            }
            Kind::RecordAttribute { key } => {
                run.out.effects.push(Effect::Attribute { device: device.map(str::to_string), key: key.clone(), value });
                vec![]
            }
            Kind::Notify { title, every } => {
                let Some(m) = run.memory(id, key) else { return vec![] };
                if m.since.is_some_and(|last| at - last < span(*every)) {
                    return vec![];
                }
                m.since = Some(at);
                match fill(&json!(title), &value, device) {
                    Ok(title) => run.out.effects.push(Effect::Notify { device: device.map(str::to_string), title: word(&title) }),
                    Err(why) => run.out.problems.push(format!("{id}: {why}")),
                }
                vec![]
            }
            Kind::Export { to } => {
                run.out.effects.push(Effect::Export { to: to.clone(), body: json!({"device": device, "value": value, "at": at}) });
                vec![]
            }
            Kind::Command { operation, device: fixed, payload, every, expires } => {
                let device = fixed.as_deref().or(device);
                let Some(m) = run.memory(id, device.unwrap_or_default()) else { return vec![] };
                if m.since.is_some_and(|last| at - last < span(*every)) {
                    return vec![];
                }
                m.since = Some(at);
                match fill(payload, &value, device) {
                    Ok(payload) => run.out.effects.push(Effect::Command {
                        node: id.to_string(),
                        device: device.map(str::to_string),
                        operation: operation.clone(),
                        payload,
                        expires_secs: span(*expires).num_seconds(),
                    }),
                    // Half a command must not go anywhere.
                    Err(why) => run.out.problems.push(format!("{id}: {why}")),
                }
                vec![]
            }
        }
    }

    /// A hold that has lasted: says so once.
    fn due(m: &mut Memory, hold: TimeDelta, at: DateTime<Utc>) -> Leaving {
        match (m.since, m.on) {
            (Some(since), false) if at - since >= hold => {
                m.on = true;
                out_of("out", m.last.clone().unwrap_or(Value::Null))
            }
            _ => vec![],
        }
    }

    /// Follows messages until they have all stopped. A flow has no cycles, so they do, and the
    /// limit is for one so wide that a message would cost too much.
    fn follow(&self, start: Vec<(usize, String, Option<String>, Value)>, run: &mut Run<'_>) {
        let mut queue: VecDeque<_> = start.into();
        let mut steps = 0;
        while let Some((i, input, device, value)) = queue.pop_front() {
            steps += 1;
            if steps > MAX_STEPS {
                run.out.problems.push(format!("one message took more than {MAX_STEPS} steps, and the rest of its work was dropped"));
                return;
            }
            let leaving = self.step(i, &input, device.as_deref(), value.clone(), run);
            self.send_on(i, device, value, leaving, &mut queue, run);
        }
    }

    fn send_on(
        &self,
        i: usize,
        device: Option<String>,
        value: Value,
        leaving: Leaving,
        queue: &mut VecDeque<(usize, String, Option<String>, Value)>,
        run: &mut Run<'_>,
    ) {
        run.out.trace.push(Trace { node: self.nodes[i].id.clone(), device: device.clone(), value, ports: leaving.iter().map(|(p, _)| p.clone()).collect() });
        for (port, v) in leaving {
            for w in self.wires[i].iter().filter(|w| w.port == port) {
                queue.push_back((w.to, w.input.clone(), device.clone(), v.clone()));
            }
        }
    }

    /// Takes in one thing a device did.
    pub fn input(&self, state: &mut State, input: &Input, at: DateTime<Utc>) -> Output {
        let mut run = Run { state, at, out: Output::default() };
        let matched = |n: &Node| match &n.kind {
            Kind::Metric { device, metric } => input.metric.as_ref() == Some(metric) && device_matches(device, &input.device),
            Kind::State { device } => input.state.is_some() && device_matches(device, &input.device),
            Kind::Event { device, name } => input.event.as_ref() == Some(name) && device_matches(device, &input.device),
            Kind::Attribute { device, key } => input.attribute.as_ref() == Some(key) && device_matches(device, &input.device),
            _ => false,
        };
        let value = input.state.as_ref().map_or_else(|| input.value.clone(), |s| json!(s));
        let start =
            self.nodes.iter().enumerate().filter(|(_, n)| matched(n)).map(|(i, _)| (i, "in".to_string(), Some(input.device.clone()), value.clone())).collect();
        self.follow(start, &mut run);
        run.out
    }

    /// Whatever the clock alone can cause: timers, holds that have lasted, silences and delays.
    pub fn tick(&self, state: &mut State, at: DateTime<Utc>) -> Output {
        let mut run = Run { state, at, out: Output::default() };
        let mut queue = VecDeque::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let id = n.id.as_str();
            match &n.kind {
                Kind::Timer { every } => {
                    let Some(m) = run.memory(id, "") else { continue };
                    let due = m.since.is_none_or(|last| at - last >= span(*every));
                    if due {
                        m.since = Some(at);
                        self.send_on(i, None, json!(at), out_of("out", json!(at)), &mut queue, &mut run);
                    }
                }
                Kind::Hold { hold } => {
                    for (device, leaving) in Self::each_key(run.state, id, |m| Self::due(m, span(*hold), at)) {
                        self.send_on(i, device, Value::Null, leaving, &mut queue, &mut run);
                    }
                }
                Kind::Silence { quiet } => {
                    let fire = |m: &mut Memory| match (m.since, m.on) {
                        (Some(since), false) if at - since >= span(*quiet) => {
                            m.on = true;
                            out_of("out", m.last.clone().unwrap_or(Value::Null))
                        }
                        _ => vec![],
                    };
                    for (device, leaving) in Self::each_key(run.state, id, fire) {
                        self.send_on(i, device, Value::Null, leaving, &mut queue, &mut run);
                    }
                }
                Kind::Delay { .. } => {
                    let release = |m: &mut Memory| {
                        let mut leaving = Vec::new();
                        while m.held.front().is_some_and(|(when, _)| *when <= at) {
                            if let Some((_, v)) = m.held.pop_front() {
                                leaving.push(("out".to_string(), v));
                            }
                        }
                        leaving
                    };
                    for (device, leaving) in Self::each_key(run.state, id, release) {
                        self.send_on(i, device, Value::Null, leaving, &mut queue, &mut run);
                    }
                }
                _ => {}
            }
        }
        let start: Vec<_> = queue.into();
        self.follow(start, &mut run);
        run.out
    }

    fn each_key(state: &mut State, node: &str, mut f: impl FnMut(&mut Memory) -> Leaving) -> Vec<(Option<String>, Leaving)> {
        let Some(keys) = state.nodes.get_mut(node) else { return vec![] };
        keys.values_mut()
            .filter_map(|m| {
                let leaving = f(m);
                (!leaving.is_empty()).then(|| (m.device.clone(), leaving))
            })
            .collect()
    }
}

/// One line of what a simulation is told happened.
#[derive(Debug, Clone, Deserialize)]
pub struct Scripted {
    /// Seconds after the start.
    #[serde(default)]
    pub at: i64,
    #[serde(flatten)]
    pub input: Input,
}

/// What happened at one moment of a simulation.
#[derive(Debug, Serialize)]
pub struct Moment {
    pub at: i64,
    #[serde(flatten)]
    pub output: Output,
}

/// Walks a script through a flow, a second at a time, by the code a gateway runs. Only moments
/// where something happened come back. `until` is how long to keep the clock going after the last
/// line, so that a hold can be seen to fire.
pub fn simulate(flow: &Compiled, script: &[Scripted], until: i64) -> Result<Vec<Moment>, String> {
    let last = script.iter().map(|s| s.at).max().unwrap_or(0).max(until);
    if last > MAX_SIMULATED_SECS || script.iter().any(|s| s.at < 0) {
        return Err(format!("a simulation covers up to {MAX_SIMULATED_SECS} seconds"));
    }
    let start = Utc::now();
    let mut state = State::default();
    let mut moments = Vec::new();
    for second in 0..=last {
        let at = start + TimeDelta::seconds(second);
        let mut merged = flow.tick(&mut state, at);
        for line in script.iter().filter(|s| s.at == second) {
            let more = flow.input(&mut state, &line.input, at);
            merged.effects.extend(more.effects);
            merged.trace.extend(more.trace);
            merged.problems.extend(more.problems);
        }
        if merged != Output::default() {
            moments.push(Moment { at: second, output: merged });
        }
    }
    Ok(moments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(body: Value) -> Compiled {
        parse(&body.to_string()).unwrap_or_else(|e| panic!("{e}"))
    }

    fn metric(device: &str, value: f64) -> Input {
        Input { device: device.into(), metric: Some("temp".into()), value: json!(value), ..Default::default() }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    /// Hot for thirty seconds raises an alarm, and cooling clears it, for each device on its own.
    fn hot() -> Compiled {
        flow(json!({
            "nodes": [
                {"id": "t", "kind": "metric", "metric": "temp", "x": 10, "y": 20, "label": "Temperature"},
                {"id": "c", "kind": "compare", "op": ">", "value": 80},
                {"id": "h", "kind": "hold", "for": "30s"},
                {"id": "a", "kind": "alarm", "name": "Running hot", "severity": "Critical"},
            ],
            "edges": [
                {"from": "t", "to": "c"},
                {"from": "c", "port": "yes", "to": "h"},
                {"from": "c", "port": "no", "to": "h", "input": "reset"},
                {"from": "c", "port": "no", "to": "a", "input": "clear"},
                {"from": "h", "to": "a"},
            ],
        }))
    }

    fn raised(o: &Output) -> Vec<(Option<&str>, bool)> {
        o.effects.iter().filter_map(|e| if let Effect::Alarm { device, raised, .. } = e { Some((device.as_deref(), *raised)) } else { None }).collect()
    }

    #[test]
    fn a_hold_waits_and_a_dip_starts_it_again() {
        let f = hot();
        let mut s = State::default();
        assert!(raised(&f.input(&mut s, &metric("P1", 90.0), at(0))).is_empty());
        assert!(raised(&f.tick(&mut s, at(29))).is_empty(), "not yet");
        assert!(raised(&f.input(&mut s, &metric("P1", 70.0), at(29))).is_empty(), "a dip is not an alarm's clearing when none is raised");
        assert!(raised(&f.input(&mut s, &metric("P1", 90.0), at(30))).is_empty());
        assert!(raised(&f.tick(&mut s, at(59))).is_empty(), "the dip started it again");
        assert_eq!(raised(&f.tick(&mut s, at(60))), [(Some("P1"), true)], "the clock alone can fire it");
        assert!(raised(&f.tick(&mut s, at(61))).is_empty(), "and it says so once");
        assert!(raised(&f.input(&mut s, &metric("P1", 95.0), at(62))).is_empty(), "however long it stays hot");
        assert_eq!(raised(&f.input(&mut s, &metric("P1", 60.0), at(63))), [(Some("P1"), false)]);
        assert!(raised(&f.input(&mut s, &metric("P1", 60.0), at(64))).is_empty(), "and clears once");
    }

    #[test]
    fn each_device_is_watched_on_its_own() {
        let f = hot();
        let mut s = State::default();
        f.input(&mut s, &metric("P1", 90.0), at(0));
        f.input(&mut s, &metric("P2", 90.0), at(20));
        assert_eq!(raised(&f.tick(&mut s, at(30))), [(Some("P1"), true)]);
        assert_eq!(raised(&f.tick(&mut s, at(50))), [(Some("P2"), true)]);
        assert_eq!(raised(&f.input(&mut s, &metric("P1", 10.0), at(51))), [(Some("P1"), false)], "one cooling says nothing of the other");
    }

    #[test]
    fn a_restart_does_not_lose_its_place() {
        let f = hot();
        let mut s = State::default();
        f.input(&mut s, &metric("P1", 90.0), at(0));
        let saved = serde_json::to_string(&s).unwrap();
        let mut back: State = serde_json::from_str(&saved).unwrap();
        assert_eq!(raised(&f.tick(&mut back, at(30))), [(Some("P1"), true)], "the hold kept counting");
        let saved = serde_json::to_string(&back).unwrap();
        let mut again: State = serde_json::from_str(&saved).unwrap();
        assert!(raised(&f.input(&mut again, &metric("P1", 95.0), at(31))).is_empty(), "and a raised alarm is not raised twice");
        assert_eq!(raised(&f.input(&mut again, &metric("P1", 10.0), at(32))), [(Some("P1"), false)]);
    }

    #[test]
    fn a_flow_that_could_not_work_is_refused_and_says_where() {
        let source = json!({"id": "s", "kind": "metric", "metric": "t"});
        let action = json!({"id": "e", "kind": "record_event", "name": "x"});
        let fails = |body: Value, why: &str| {
            let e = parse(&body.to_string()).err().unwrap_or_else(|| panic!("{body} passed"));
            assert!(e.contains(why), "{e}");
        };
        fails(
            json!({"nodes": [source, action, {"id": "a", "kind": "limit", "every": "1s"}, {"id": "b", "kind": "limit", "every": "1s"}],
            "edges": [{"from": "s", "to": "a"}, {"from": "a", "to": "b"}, {"from": "b", "to": "a"}, {"from": "b", "to": "e"}]}),
            "circle",
        );
        fails(
            json!({"nodes": [source, action, {"id": "a", "kind": "limit", "every": "1s"}], "edges": [{"from": "s", "to": "a"}, {"from": "a", "to": "a"}, {"from": "a", "to": "e"}]}),
            "circle",
        );
        fails(
            json!({"nodes": [source, action, {"id": "n", "kind": "limit", "every": "1s"}], "edges": [{"from": "s", "to": "e"}, {"from": "n", "to": "e"}]}),
            "nothing reaches",
        );
        fails(json!({"nodes": [source], "edges": []}), "nothing comes of it");
        fails(json!({"nodes": [source, action], "edges": [{"from": "s", "to": "gone"}]}), "not here");
        fails(json!({"nodes": [source, action], "edges": [{"from": "s", "port": "yes", "to": "e"}]}), "no way out");
        fails(json!({"nodes": [source, action], "edges": [{"from": "s", "to": "e", "input": "side"}]}), "takes nothing");
        fails(json!({"nodes": [source, source, action], "edges": []}), "twice");
        fails(json!({"nodes": [{"id": "x", "kind": "run_script", "code": "rm -rf /"}]}), "x:");
        fails(json!({"nodes": [{"id": "x", "kind": "limit", "every": "soon"}]}), "not a duration");
        fails(
            json!({"nodes": [source, action, {"id": "h", "kind": "hold", "for": "1s"}], "edges": [{"from": "s", "to": "h"}, {"from": "h", "to": "e"}]}),
            "resets",
        );
        fails(json!({"nodes": [source, {"id": "a", "kind": "alarm", "name": "x"}], "edges": [{"from": "s", "to": "a"}]}), "clears");
        fails(
            json!({"nodes": [source, {"id": "m", "kind": "math", "formula": "[value] +"}, action], "edges": [{"from": "s", "to": "m"}, {"from": "m", "to": "e"}]}),
            "m:",
        );
        assert!(parse(&json!({"nodes": [source, action], "edges": [{"from": "s", "to": "e"}]}).to_string()).is_ok());
    }

    #[test]
    fn logic_does_what_it_says() {
        let one = |kind: Value, inputs: &[f64]| {
            let mut node = kind;
            node["id"] = json!("n");
            let f = flow(json!({"nodes": [{"id": "s", "kind": "metric", "metric": "temp"}, node, {"id": "e", "kind": "record_metric", "name": "o"}],
                "edges": [{"from": "s", "to": "n"}, {"from": "n", "to": "e"}]}));
            let mut s = State::default();
            let mut got = Vec::new();
            for (i, v) in inputs.iter().enumerate() {
                for e in f.input(&mut s, &metric("P", *v), at(i as i64 * 10)).effects {
                    if let Effect::Metric { value, .. } = e {
                        got.push(value);
                    }
                }
            }
            got
        };
        assert_eq!(one(json!({"kind": "changed"}), &[1.0, 1.0, 2.0, 2.0]), [1.0, 2.0]);
        assert_eq!(one(json!({"kind": "changed", "by": 1.0}), &[1.0, 1.5, 2.0, 2.4]), [1.0, 2.0], "from what last passed, so a slow creep is still seen");
        assert_eq!(one(json!({"kind": "limit", "every": "25s"}), &[1.0, 2.0, 3.0, 4.0]), [1.0, 4.0]);
        assert_eq!(one(json!({"kind": "count", "of": 3, "within": "25s"}), &[1.0, 1.0, 1.0, 1.0, 1.0, 1.0]), [3.0, 3.0]);
        assert_eq!(one(json!({"kind": "count", "of": 3, "within": "15s"}), &[1.0, 1.0, 1.0, 1.0]), Vec::<f64>::new(), "too far apart");
        assert_eq!(one(json!({"kind": "window", "over": "15s", "stat": "avg"}), &[10.0, 20.0, 60.0]), [10.0, 15.0, 40.0], "the oldest has left by the third");
        assert_eq!(one(json!({"kind": "window", "over": "1m", "stat": "max"}), &[1.0, 3.0, 2.0]), [1.0, 3.0, 3.0]);
        assert_eq!(one(json!({"kind": "window", "over": "1m", "stat": "delta"}), &[5.0, 7.0, 4.0]), [0.0, 2.0, -1.0]);
        assert_eq!(one(json!({"kind": "window", "over": "1m", "stat": "count"}), &[5.0, 7.0]), [1.0, 2.0]);
        assert_eq!(one(json!({"kind": "math", "formula": "round([value] * 1.8 + 32, 1)"}), &[100.0]), [212.0]);
        assert_eq!(one(json!({"kind": "math", "formula": "[value] / 0"}), &[1.0]), Vec::<f64>::new(), "no answer is no message");
        assert_eq!(one(json!({"kind": "map", "map": {"1": 100, "2": 200}}), &[1.0, 2.0, 3.0]), [100.0, 200.0]);
        assert_eq!(one(json!({"kind": "map", "map": {"1": 100}, "other": 0}), &[3.0]), [0.0]);
        assert_eq!(one(json!({"kind": "set", "value": "{value}"}), &[7.0]), [7.0]);
    }

    #[test]
    fn numbers_and_words_compare_as_what_they_are() {
        assert_eq!(compares(Comparison::Above, &json!(9), &json!("8.5")), Some(true));
        assert_eq!(compares(Comparison::Is, &json!("Running"), &json!("Running")), Some(true));
        assert_eq!(compares(Comparison::IsNot, &json!("Running"), &json!("Idle")), Some(true));
        assert_eq!(compares(Comparison::Above, &json!("Running"), &json!(5)), None, "neither way: it says nothing");
    }

    #[test]
    fn inputs_combine_when_all_are_there_and_fresh() {
        let f = flow(json!({
            "nodes": [
                {"id": "v", "kind": "metric", "metric": "volts"},
                {"id": "a", "kind": "metric", "metric": "amps"},
                {"id": "k", "kind": "combine", "formula": "[v] * [a] / 1000", "fresh": "10s"},
                {"id": "e", "kind": "record_metric", "name": "kw", "unit": "kW"},
            ],
            "edges": [{"from": "v", "to": "k", "input": "v"}, {"from": "a", "to": "k", "input": "a"}, {"from": "k", "to": "e"}],
        }));
        let reading = |metric: &str, value: f64| Input { device: "M".into(), metric: Some(metric.into()), value: json!(value), ..Default::default() };
        let mut s = State::default();
        assert!(f.input(&mut s, &reading("volts", 400.0), at(0)).effects.is_empty(), "amps have not been heard");
        let o = f.input(&mut s, &reading("amps", 5.0), at(5));
        assert_eq!(o.effects, [Effect::Metric { device: Some("M".into()), name: "kw".into(), value: 2.0, unit: Some("kW".into()) }]);
        assert!(f.input(&mut s, &reading("amps", 6.0), at(20)).effects.is_empty(), "the volts are stale now");
        assert_eq!(f.input(&mut s, &reading("volts", 400.0), at(21)).effects.len(), 1);
    }

    #[test]
    fn a_gate_says_each_change_once() {
        let f = flow(json!({
            "nodes": [
                {"id": "a", "kind": "metric", "metric": "a"},
                {"id": "b", "kind": "metric", "metric": "b"},
                {"id": "g", "kind": "all"},
                {"id": "y", "kind": "record_event", "name": "both"},
                {"id": "n", "kind": "record_event", "name": "not"},
            ],
            "edges": [{"from": "a", "to": "g", "input": "a"}, {"from": "b", "to": "g", "input": "b"}, {"from": "g", "port": "yes", "to": "y"}, {"from": "g", "port": "no", "to": "n"}],
        }));
        let m = |metric: &str, value: f64| Input { device: "M".into(), metric: Some(metric.into()), value: json!(value), ..Default::default() };
        let named = |o: Output| o.effects.iter().filter_map(|e| if let Effect::Event { name, .. } = e { Some(name.clone()) } else { None }).collect::<Vec<_>>();
        let mut s = State::default();
        assert_eq!(named(f.input(&mut s, &m("a", 1.0), at(0))), ["not"]);
        assert_eq!(named(f.input(&mut s, &m("a", 1.0), at(1))), Vec::<String>::new());
        assert_eq!(named(f.input(&mut s, &m("b", 1.0), at(2))), ["both"]);
        assert_eq!(named(f.input(&mut s, &m("b", 2.0), at(3))), Vec::<String>::new());
        assert_eq!(named(f.input(&mut s, &m("a", 0.0), at(4))), ["not"]);
    }

    #[test]
    fn a_source_listens_only_for_what_it_names() {
        let f = flow(
            json!({"nodes": [{"id": "s", "kind": "metric", "metric": "temp", "device": "PRESS-*"}, {"id": "e", "kind": "record_event", "name": "x"}], "edges": [{"from": "s", "to": "e"}]}),
        );
        let mut s = State::default();
        assert_eq!(f.input(&mut s, &metric("PRESS-1", 1.0), at(0)).effects.len(), 1);
        assert!(f.input(&mut s, &metric("OVEN-1", 1.0), at(0)).effects.is_empty());
        assert!(f.input(&mut s, &Input { device: "PRESS-1".into(), metric: Some("rpm".into()), ..Default::default() }, at(0)).effects.is_empty());
    }

    #[test]
    fn silence_and_a_delay_are_noticed_by_the_clock() {
        let f = flow(json!({
            "nodes": [
                {"id": "s", "kind": "metric", "metric": "temp"},
                {"id": "q", "kind": "silence", "for": "1m"},
                {"id": "d", "kind": "delay", "for": "10s"},
                {"id": "e", "kind": "record_event", "name": "quiet"},
                {"id": "l", "kind": "record_event", "name": "late"},
            ],
            "edges": [{"from": "s", "to": "q"}, {"from": "s", "to": "d"}, {"from": "q", "to": "e"}, {"from": "d", "to": "l"}],
        }));
        let mut s = State::default();
        f.input(&mut s, &metric("M", 1.0), at(0));
        assert!(f.tick(&mut s, at(9)).effects.is_empty());
        let late = f.tick(&mut s, at(10));
        assert_eq!(late.effects, [Effect::Event { device: Some("M".into()), name: "late".into(), value: json!(1.0) }]);
        assert!(f.tick(&mut s, at(59)).effects.is_empty());
        assert_eq!(f.tick(&mut s, at(60)).effects.len(), 1);
        assert!(f.tick(&mut s, at(120)).effects.is_empty(), "once");
        f.input(&mut s, &metric("M", 1.0), at(121));
        assert_eq!(
            f.tick(&mut s, at(181)).effects.iter().filter(|e| matches!(e, Effect::Event { name, .. } if name == "quiet")).count(),
            1,
            "and again after it spoke"
        );
    }

    #[test]
    fn a_timer_fires_on_time_and_its_work_has_no_device() {
        let f = flow(
            json!({"nodes": [{"id": "t", "kind": "timer", "every": "1m"}, {"id": "e", "kind": "record_event", "name": "tick"}], "edges": [{"from": "t", "to": "e"}]}),
        );
        let mut s = State::default();
        assert_eq!(f.tick(&mut s, at(0)).effects.len(), 1);
        assert!(f.tick(&mut s, at(59)).effects.is_empty());
        assert_eq!(f.tick(&mut s, at(60)).effects.len(), 1);
    }

    #[test]
    fn a_command_is_asked_for_rarely_and_only_when_whole() {
        let f = flow(json!({
            "nodes": [
                {"id": "s", "kind": "metric", "metric": "temp"},
                {"id": "c", "kind": "command", "operation": "fan.start", "payload": {"speed": "{value}", "for": "{device}"}, "every": "1m", "expires": "2m"},
            ],
            "edges": [{"from": "s", "to": "c"}],
        }));
        let mut s = State::default();
        let first = f.input(&mut s, &metric("M", 3.0), at(0));
        assert_eq!(
            first.effects,
            [Effect::Command {
                node: "c".into(),
                device: Some("M".into()),
                operation: "fan.start".into(),
                payload: json!({"speed": 3.0, "for": "M"}),
                expires_secs: 120
            }]
        );
        assert!(f.input(&mut s, &metric("M", 4.0), at(30)).effects.is_empty(), "a chatty sensor cannot flood the ledger");
        assert_eq!(f.input(&mut s, &metric("N", 4.0), at(30)).effects.len(), 1, "each device has its own");
        assert_eq!(f.input(&mut s, &metric("M", 5.0), at(60)).effects.len(), 1);

        let broken = flow(
            json!({"nodes": [{"id": "t", "kind": "timer", "every": "1s"}, {"id": "c", "kind": "command", "operation": "x", "device": "M", "payload": {"a": "{nope.b}"}}], "edges": [{"from": "t", "to": "c"}]}),
        );
        let mut s = State::default();
        let o = broken.tick(&mut s, at(0));
        assert!(o.effects.is_empty(), "half a command goes nowhere");
        assert!(o.problems.iter().any(|p| p.contains("nope")), "{:?}", o.problems);
    }

    #[test]
    fn nothing_keeps_more_than_it_should() {
        let f = flow(
            json!({"nodes": [{"id": "s", "kind": "metric", "metric": "temp"}, {"id": "w", "kind": "changed"}, {"id": "e", "kind": "record_event", "name": "x"}], "edges": [{"from": "s", "to": "w"}, {"from": "w", "to": "e"}]}),
        );
        let mut s = State::default();
        let mut refused = 0;
        for i in 0..MAX_KEYS + 10 {
            refused += f.input(&mut s, &metric(&format!("D{i}"), 1.0), at(0)).problems.len();
        }
        assert_eq!(refused, 10, "a device that makes up names costs this much");
        assert_eq!(s.nodes["w"].len(), MAX_KEYS);
    }

    #[test]
    fn a_wide_flow_is_cut_off_and_says_so() {
        // Eight rows of ten: 10^8 paths from one message, in a graph with no cycle.
        let mut nodes = vec![json!({"id": "s", "kind": "metric", "metric": "temp"})];
        let mut edges = Vec::new();
        for row in 0..8 {
            for col in 0..10 {
                nodes.push(json!({"id": format!("n{row}.{col}"), "kind": "set", "value": 1}));
                if row == 0 {
                    edges.push(json!({"from": "s", "to": format!("n0.{col}")}));
                } else {
                    for prev in 0..10 {
                        edges.push(json!({"from": format!("n{}.{prev}", row - 1), "to": format!("n{row}.{col}")}));
                    }
                }
            }
        }
        nodes.push(json!({"id": "e", "kind": "record_event", "name": "x"}));
        for col in 0..10 {
            edges.push(json!({"from": format!("n7.{col}"), "to": "e"}));
        }
        let f = flow(json!({"nodes": nodes, "edges": edges}));
        let o = f.input(&mut State::default(), &metric("M", 1.0), at(0));
        assert!(o.problems.iter().any(|p| p.contains("steps")), "{:?}", o.problems);
    }

    #[test]
    fn a_simulation_shows_what_would_happen_and_when() {
        let script: Vec<Scripted> = serde_json::from_value(json!([
            {"at": 0, "device": "P1", "metric": "temp", "value": 90},
            {"at": 40, "device": "P1", "metric": "temp", "value": 60},
        ]))
        .unwrap();
        let moments = simulate(&hot(), &script, 60).unwrap();
        let alarms: Vec<(i64, bool)> = moments.iter().flat_map(|m| raised(&m.output).into_iter().map(|(_, r)| (m.at, r)).collect::<Vec<_>>()).collect();
        assert_eq!(alarms, [(30, true), (40, false)]);
        let first = moments.iter().find(|m| m.at == 0).unwrap();
        assert_eq!(first.output.trace.iter().map(|t| t.node.as_str()).collect::<Vec<_>>(), ["t", "c", "h"], "and the way each message went");
        assert_eq!(first.output.trace[1].ports, ["yes"]);
        assert!(simulate(&hot(), &script, MAX_SIMULATED_SECS + 1).is_err());
        let negative: Vec<Scripted> = serde_json::from_value(json!([{"at": -1, "device": "P", "metric": "temp"}])).unwrap();
        assert!(simulate(&hot(), &negative, 1).is_err());
    }

    #[test]
    fn yaml_is_as_good_as_json() {
        assert!(
            parse("nodes:\n  - {id: s, kind: state, device: 'P*'}\n  - {id: e, kind: notify, title: '{device} is {value}'}\nedges:\n  - {from: s, to: e}\n")
                .is_ok()
        );
    }
}
