//! A device described instead of programmed. Most protocols are the same few things said differently:
//! connect somewhere, cut what arrives into messages, find values in them, send something when asked,
//! and watch for how it ended. A [`Definition`] says those five things as data, so supporting a new
//! device is a few lines of YAML, reviewed and versioned like any record, and the platform can check
//! one and try it on a sample with no device attached. What cannot be said this way (a vendor's SDK,
//! a protocol with a session of its own) runs as a process of its own and speaks JSON lines
//! (`docs/edge-definitions.md`).
//!
//! ```yaml
//! name: acme-scale
//! connect: tcp://10.0.0.7:9100
//! frame: {line: "\r\n"}
//! poll:
//!   - {every: 5s, send: "W\r\n"}
//! read:
//!   - {match: '^(?P<kg>-?[0-9.]+) kg$', metric: weight.kg, group: kg}
//! commands:
//!   scale.tare:
//!     send: "T\r\n"
//!     status: {match: '^(?P<r>OK|ERR)', group: r, done: OK, failed: ERR}
//! ```
//!
//! Pure: this parses, checks and decodes. A site does the talking. A command must say how it ends,
//! because a write that nothing confirms is not work done: `once: true` is the explicit admission
//! that this device can prove nothing more than the send.

use regex::Regex;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;

/// The schemes a site can open. Anything else is an error when the definition is saved, not when it
/// reaches a gateway at three in the morning.
pub const SCHEMES: [&str; 12] = ["tcp", "udp", "serial", "http", "https", "ws", "wss", "mqtt", "mqtts", "modbus+tcp", "process", "opcua"];

/// The longest message a site will assemble. A stream that never frames costs this much, not memory.
pub const MAX_FRAME: usize = 1 << 20;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    /// `tcp://host:port`, `serial:///dev/ttyUSB0?baud=9600`, and so on. A device may give its own,
    /// which is how one definition serves a hundred devices.
    #[serde(default)]
    pub connect: Option<String>,
    #[serde(default, deserialize_with = "frame")]
    pub frame: Option<Frame>,
    /// Sent with every HTTP request, or at connection on a WebSocket.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Topics to hold open.
    #[serde(default)]
    pub subscribe: Vec<String>,
    /// Said once on each new connection: a login, a subscription.
    #[serde(default)]
    pub on_connect: Vec<Step>,
    #[serde(default)]
    pub poll: Vec<Poll>,
    #[serde(default)]
    pub read: Vec<ReadRule>,
    #[serde(default)]
    pub commands: BTreeMap<String, CommandDef>,
}

/// How a stream of bytes becomes messages. Datagrams, MQTT and HTTP are messages already.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    /// Ends with this text.
    Line(String),
    /// Exactly this long.
    Fixed(usize),
    /// Between these two markers, both removed.
    Between(String, String),
    /// A length field somewhere in the header.
    Length(LengthField),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LengthField {
    #[serde(default)]
    pub at: usize,
    /// One, two or four bytes.
    pub size: usize,
    #[serde(default)]
    pub little: bool,
    /// Added to the field's value to get the whole message's length: the header, a checksum.
    #[serde(default)]
    pub add: i64,
}

impl Frame {
    /// The next whole message in `buf`, taken out of it. What cannot be a message is thrown away
    /// rather than held for ever, so a stream that went wrong finds its feet again.
    pub fn take(&self, buf: &mut Vec<u8>) -> Option<Vec<u8>> {
        match self {
            Frame::Line(end) => {
                let at = find(buf, end.as_bytes())?;
                let mut message: Vec<u8> = buf.drain(..at + end.len()).collect();
                message.truncate(at);
                Some(message)
            }
            Frame::Fixed(size) => (*size > 0 && buf.len() >= *size).then(|| buf.drain(..*size).collect()),
            Frame::Between(open, close) => {
                let Some(start) = find(buf, open.as_bytes()) else {
                    // Keep what may be the beginning of a marker, and nothing else.
                    buf.drain(..buf.len().saturating_sub(open.len().saturating_sub(1)));
                    return None;
                };
                buf.drain(..start);
                let end = find(&buf[open.len()..], close.as_bytes())? + open.len();
                let message = buf[open.len()..end].to_vec();
                buf.drain(..end + close.len());
                Some(message)
            }
            Frame::Length(f) => {
                if buf.len() < f.at + f.size {
                    return None;
                }
                let raw = &buf[f.at..f.at + f.size];
                let value = raw.iter().enumerate().fold(0u64, |n, (i, b)| {
                    let shift = if f.little { i } else { f.size - 1 - i };
                    n | (u64::from(*b) << (8 * shift))
                });
                let length = i64::try_from(value).ok()?.saturating_add(f.add);
                if length <= 0 || length as usize > MAX_FRAME {
                    buf.drain(..1);
                    return None;
                }
                (buf.len() >= length as usize).then(|| buf.drain(..length as usize).collect())
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Something to say to the equipment. Which fields apply depends on the transport, and [`parse`]
/// says so rather than letting one be ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Text, with `{placeholders}`, or anything else, sent as JSON.
    #[serde(default)]
    pub send: Option<Value>,
    /// Bytes, as hexadecimal.
    #[serde(default)]
    pub hex: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub get: Option<String>,
    #[serde(default)]
    pub post: Option<String>,
    #[serde(default)]
    pub put: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub delete: Option<String>,
    /// Modbus: `holding`, `input`, `coil` or `discrete`.
    #[serde(default)]
    pub read: Option<String>,
    #[serde(default)]
    pub at: Option<u16>,
    #[serde(default)]
    pub count: Option<u16>,
    /// Modbus: registers to write, in order. Nothing else can be written.
    #[serde(default)]
    pub write: Vec<RegisterWrite>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterWrite {
    pub at: u16,
    /// A number, or a `{placeholder}`.
    pub value: Value,
}

impl Step {
    pub fn method(&self) -> Option<(&'static str, &str)> {
        [("GET", &self.get), ("POST", &self.post), ("PUT", &self.put), ("PATCH", &self.patch), ("DELETE", &self.delete)]
            .into_iter()
            .find_map(|(m, p)| p.as_deref().map(|p| (m, p)))
    }

    fn is_empty(&self) -> bool {
        self.send.is_none() && self.hex.is_none() && self.method().is_none() && self.read.is_none() && self.write.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Poll {
    #[serde(deserialize_with = "duration")]
    pub every: Duration,
    #[serde(flatten)]
    pub step: Step,
}

/// What a field is, in a binary message or in Modbus registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
    /// `len` bytes of text.
    Text,
    /// `len` bytes, as hexadecimal.
    Hex,
}

impl Kind {
    fn size(self) -> usize {
        match self {
            Kind::U8 | Kind::I8 => 1,
            Kind::U16 | Kind::I16 => 2,
            Kind::U32 | Kind::I32 | Kind::F32 => 4,
            Kind::U64 | Kind::I64 | Kind::F64 => 8,
            Kind::Text | Kind::Hex => 0,
        }
    }
}

/// How the bytes of a number are laid out. PLCs split words in every possible way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// Most significant first: `ABCD`.
    #[default]
    Be,
    /// Least significant first: `DCBA`.
    Le,
    /// Words swapped: `CDAB`.
    Cdab,
    /// Bytes swapped in each word: `BADC`.
    Badc,
}

/// One value found in a message, and what it means.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// A number to record.
    #[serde(default)]
    pub metric: Option<String>,
    /// The equipment's state: Running, Idle, Stopped, Maintenance or Unknown.
    #[serde(default)]
    pub state: Option<bool>,
    /// A fact about it that changes rarely: a firmware version, a position in a map.
    #[serde(default)]
    pub attribute: Option<String>,
    /// Something that happened. The value is what happened.
    #[serde(default)]
    pub event: Option<String>,
    /// `$.a.b[0]`.
    #[serde(default)]
    pub json: Option<String>,
    /// A named group of `match`.
    #[serde(default)]
    pub group: Option<String>,
    /// A byte offset.
    #[serde(default)]
    pub at: Option<usize>,
    /// For Modbus: a register's distance from the start of the read.
    #[serde(default)]
    pub reg: Option<usize>,
    #[serde(default, rename = "type")]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub order: Order,
    /// One bit of what was read, 0 being the lowest.
    #[serde(default)]
    pub bit: Option<u32>,
    #[serde(default)]
    pub len: Option<usize>,
    /// A fixed value: this rule matching is the news.
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub offset: Option<f64>,
    /// Decimal places.
    #[serde(default)]
    pub round: Option<u32>,
    /// What the raw value means: `{0: Idle, 1: Running}`.
    #[serde(default)]
    pub map: BTreeMap<String, Value>,
    #[serde(default)]
    pub unit: Option<String>,
    /// Report a number only when it has moved this far.
    #[serde(default)]
    pub deadband: Option<f64>,
    /// `Sample`, `State` or `Alarm`: what may be shed first when space runs out.
    #[serde(default)]
    pub class: Option<String>,
}

/// When a message counts. Without one, every message that reaches the rule does.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    #[serde(default)]
    pub json: Option<String>,
    #[serde(default)]
    pub at: Option<usize>,
    /// What it must be: for `at`, hexadecimal.
    pub is: Value,
}

#[derive(Debug, Clone)]
pub struct ReadRule {
    /// A topic or path, with MQTT's `+` and `#`.
    pub topic: Option<String>,
    pub pattern: Option<String>,
    pub when: Option<When>,
    /// Whose message this is, for a connection several devices share: `{topic.1}`, `{group.id}`.
    pub device: Option<String>,
    pub fields: Vec<Field>,
}

impl<'de> Deserialize<'de> for ReadRule {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut m = Map::deserialize(d)?;
        let text = |m: &mut Map<String, Value>, k: &str| m.remove(k).and_then(|v| v.as_str().map(str::to_string));
        let topic = text(&mut m, "topic");
        let pattern = text(&mut m, "match");
        let device = text(&mut m, "device");
        let when = m.remove("when").map(serde_json::from_value).transpose().map_err(de::Error::custom)?;
        // One value inline or several under `values`: most rules want one, and that should read so.
        let fields = match m.remove("values") {
            Some(list) => {
                if !m.is_empty() {
                    return Err(de::Error::custom("use `values` or a value's own keys, not both"));
                }
                serde_json::from_value(list).map_err(de::Error::custom)?
            }
            None => vec![serde_json::from_value(Value::Object(m)).map_err(de::Error::custom)?],
        };
        Ok(ReadRule { topic, pattern, when, device, fields })
    }
}

/// How a command is known to have ended.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRule {
    /// Ask, each time someone wants to know. Without it, the answer is watched for in what arrives.
    #[serde(default)]
    pub poll: Option<Step>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default, rename = "match")]
    pub pattern: Option<String>,
    /// Where the answer names its command, when several can be running: its value must be the
    /// command's id or the id the equipment gave it.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub json: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub at: Option<usize>,
    #[serde(default, rename = "type")]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub order: Order,
    #[serde(default, deserialize_with = "one_or_many")]
    pub running: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub done: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub failed: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub aborted: Vec<String>,
}

/// What a status said. Anything it has no word for proves nothing, so it is `Waiting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Waiting,
    Running,
    Done,
    Failed,
    Aborted,
}

#[derive(Debug, Clone)]
pub struct CommandDef {
    pub step: Step,
    /// Where the reply gives the equipment's own name for this work.
    pub id: Option<String>,
    pub status: Option<StatusRule>,
    pub cancel: Option<Step>,
    /// Asks the equipment whether it has heard of a command, after a crash. Only for equipment that
    /// keeps its own record: an answer of "no" here makes sending it again safe.
    pub find: Option<StatusRule>,
    /// Nothing more than the send can be proved.
    pub once: bool,
}

impl<'de> Deserialize<'de> for CommandDef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut m = Map::deserialize(d)?;
        let id = m.remove("id").and_then(|v| v.as_str().map(str::to_string));
        let status = m.remove("status").map(serde_json::from_value).transpose().map_err(de::Error::custom)?;
        let cancel = m.remove("cancel").map(serde_json::from_value).transpose().map_err(de::Error::custom)?;
        let find = m.remove("find").map(serde_json::from_value).transpose().map_err(de::Error::custom)?;
        let once = m.remove("once").and_then(|v| v.as_bool()).unwrap_or(false);
        let step = serde_json::from_value(Value::Object(m)).map_err(de::Error::custom)?;
        Ok(CommandDef { step, id, status, cancel, find, once })
    }
}

// By way of JSON, because YAML's own reading of an enum wants a tag, and `{line: "\n"}` reads better.
fn frame<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Frame>, D::Error> {
    Option::<Value>::deserialize(d)?.map(serde_json::from_value).transpose().map_err(de::Error::custom)
}

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let word = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    Ok(match Value::deserialize(d)? {
        Value::Array(a) => a.iter().map(word).collect(),
        v => vec![word(&v)],
    })
}

/// `500ms`, `5s`, `2m`, `1h`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: f64 = number.parse().ok()?;
    let secs = match unit.trim() {
        "ms" => n / 1000.0,
        "" | "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => return None,
    };
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs))
}

fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let v = Value::deserialize(d)?;
    let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    parse_duration(&text).ok_or_else(|| de::Error::custom(format!("{text} is not a duration: try 500ms, 5s or 2m")))
}

/// `$.a.b[0].c`, or a bare `a.b`. A path that does not lead anywhere is `None`: a device leaving a
/// field out is not an error.
pub fn at_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.strip_prefix('$').unwrap_or(path).trim_start_matches('.');
    let mut here = root;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        let (name, indexes) = part.split_once('[').map_or((part, ""), |(n, rest)| (n, rest));
        if !name.is_empty() {
            here = here.get(name)?;
        }
        for index in indexes.split('[').filter(|i| !i.is_empty()) {
            here = here.get(index.trim_end_matches(']').parse::<usize>().ok()?)?;
        }
    }
    Some(here)
}

/// A topic or path against a filter with MQTT's rules: `+` is one level, `#` the rest. The levels
/// that `+` matched come back, for `{topic.1}`.
pub fn topic_matches<'a>(filter: &str, topic: &'a str) -> Option<Vec<&'a str>> {
    let (mut f, mut t) = (filter.split('/'), topic.split('/'));
    let mut wild = Vec::new();
    loop {
        match (f.next(), t.next()) {
            (Some("#"), level) => {
                wild.extend(level);
                wild.extend(t);
                return Some(wild);
            }
            (Some("+"), Some(level)) => wild.push(level),
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return Some(wild),
            _ => return None,
        }
    }
}

/// Fills `{a.b}` from `vars`. A placeholder with nothing behind it is an error: half a command must
/// not reach a machine. `{{` is a brace.
pub fn render(template: &str, vars: &Value) -> Result<String, String> {
    let (mut out, mut rest) = (String::new(), template);
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        if let Some(after) = rest.strip_prefix("{{") {
            out.push('{');
            rest = after;
            continue;
        }
        let Some(close) = rest.find('}') else {
            out.push_str(rest);
            return Ok(out);
        };
        let key = &rest[1..close];
        // JSON in a template is left alone: only a name can be a placeholder.
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            out.push('{');
            rest = &rest[1..];
            continue;
        }
        let value = at_path(vars, key).filter(|v| !v.is_null()).ok_or_else(|| format!("nothing to put in {{{key}}}"))?;
        match value {
            Value::String(s) => out.push_str(s),
            other => out.push_str(&other.to_string()),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out.replace("}}", "}"))
}

/// As [`render`], through JSON. A string that is one placeholder keeps its value's type, so a job
/// number stays a number.
pub fn render_json(template: &Value, vars: &Value) -> Result<Value, String> {
    Ok(match template {
        Value::String(s) => match s.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            Some(key) if !key.contains(['{', '}']) && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) => {
                at_path(vars, key).filter(|v| !v.is_null()).cloned().ok_or_else(|| format!("nothing to put in {{{key}}}"))?
            }
            _ => Value::String(render(s, vars)?),
        },
        Value::Array(a) => Value::Array(a.iter().map(|v| render_json(v, vars)).collect::<Result<_, _>>()?),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| Ok((k.clone(), render_json(v, vars)?))).collect::<Result<_, String>>()?),
        other => other.clone(),
    })
}

fn number_at(bytes: &[u8], at: usize, kind: Kind, order: Order) -> Option<Value> {
    if matches!(kind, Kind::Text | Kind::Hex) {
        return None;
    }
    let size = kind.size();
    let raw = bytes.get(at..at + size)?;
    // Everything is arranged into most significant first, then read one way.
    let mut b: Vec<u8> = raw.to_vec();
    match order {
        Order::Be => {}
        Order::Le => b.reverse(),
        Order::Cdab => b.chunks_mut(2).rev().collect::<Vec<_>>().concat().clone_into(&mut b),
        Order::Badc => b.chunks_mut(2).for_each(|c| c.reverse()),
    }
    let wide = b.iter().fold(0u64, |n, x| (n << 8) | u64::from(*x));
    Some(match kind {
        Kind::U8 | Kind::U16 | Kind::U32 | Kind::U64 => json!(wide),
        Kind::I8 => json!(wide as u8 as i8),
        Kind::I16 => json!(wide as u16 as i16),
        Kind::I32 => json!(wide as u32 as i32),
        Kind::I64 => json!(wide as i64),
        Kind::F32 => json!(f32::from_bits(wide as u32)),
        Kind::F64 => json!(f64::from_bits(wide)),
        Kind::Text | Kind::Hex => return None,
    })
}

/// What a message said.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    Metric { device: Option<String>, metric: String, value: f64, unit: Option<String>, class: Option<String>, deadband: Option<f64> },
    State { device: Option<String>, state: String },
    Attribute { device: Option<String>, key: String, value: Value },
    Event { device: Option<String>, name: String, value: Value },
}

struct CompiledRead {
    pattern: Option<Regex>,
}

/// A definition that passed [`parse`], with its patterns ready.
pub struct Compiled {
    pub definition: Definition,
    pub digest: String,
    reads: Vec<CompiledRead>,
    statuses: BTreeMap<String, Option<Regex>>,
}

fn word(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A message, seen in whichever ways it can be, worked out once and only if a rule asks.
struct Seen<'a> {
    topic: &'a str,
    body: &'a [u8],
    text: Option<&'a str>,
    parsed: std::cell::OnceCell<Option<Value>>,
}

impl<'a> Seen<'a> {
    fn new(topic: &'a str, body: &'a [u8]) -> Seen<'a> {
        Seen { topic, body, text: std::str::from_utf8(body).ok(), parsed: std::cell::OnceCell::new() }
    }

    fn json(&self) -> Option<&Value> {
        self.parsed.get_or_init(|| serde_json::from_slice(self.body).ok()).as_ref()
    }

    fn holds(&self, when: &When) -> bool {
        match (&when.json, when.at) {
            (Some(path), _) => self.json().and_then(|j| at_path(j, path)).is_some_and(|v| word(v) == word(&when.is)),
            (None, Some(at)) => {
                let want = hex::decode(word(&when.is).replace(' ', "")).unwrap_or_default();
                !want.is_empty() && self.body.get(at..at + want.len()) == Some(&want[..])
            }
            (None, None) => self.text.is_some_and(|t| t.trim() == word(&when.is)),
        }
    }
}

fn extract(f: &Field, seen: &Seen<'_>, groups: Option<&regex::Captures<'_>>) -> Option<Value> {
    if let Some(v) = &f.value {
        return Some(v.clone());
    }
    if let Some(path) = &f.json {
        return seen.json().and_then(|j| at_path(j, path)).cloned();
    }
    if let Some(name) = &f.group {
        return groups.and_then(|c| c.name(name)).map(|m| json!(m.as_str()));
    }
    if let Some(at) = f.at.or(f.reg.map(|r| r * 2)) {
        let kind = f.kind.unwrap_or(if f.reg.is_some() { Kind::U16 } else { Kind::U8 });
        return match kind {
            Kind::Text => {
                let raw = seen.body.get(at..at + f.len.unwrap_or(0))?;
                Some(json!(String::from_utf8_lossy(raw).trim_end_matches('\0')))
            }
            Kind::Hex => Some(json!(hex::encode(seen.body.get(at..at + f.len.unwrap_or(0))?))),
            _ => number_at(seen.body, at, kind, f.order),
        };
    }
    seen.text.map(|t| json!(t.trim()))
}

fn as_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn shape(f: &Field, raw: Value) -> Option<Value> {
    if let Some(bit) = f.bit {
        return Some(json!((as_number(&raw)? as u64 >> bit) & 1));
    }
    if let Some(mapped) = f.map.get(&word(&raw)) {
        return Some(mapped.clone());
    }
    if f.scale.is_none() && f.offset.is_none() && f.round.is_none() {
        return Some(raw);
    }
    let n = as_number(&raw)? * f.scale.unwrap_or(1.0) + f.offset.unwrap_or(0.0);
    Some(json!(match f.round {
        Some(places) => {
            let by = 10f64.powi(places as i32);
            (n * by).round() / by
        }
        None => n,
    }))
}

fn device_of(rule: &ReadRule, wild: &[&str], groups: Option<&regex::Captures<'_>>, pattern: Option<&Regex>) -> Option<String> {
    let template = rule.device.as_ref()?;
    let mut group = Map::new();
    if let (Some(c), Some(p)) = (groups, pattern) {
        for name in p.capture_names().flatten() {
            if let Some(m) = c.name(name) {
                group.insert(name.to_string(), json!(m.as_str()));
            }
        }
    }
    // `{topic.1}` counts from one, as people do.
    let topic: Map<String, Value> = wild.iter().enumerate().map(|(i, w)| ((i + 1).to_string(), json!(w))).collect();
    render(template, &json!({"topic": topic, "group": group})).ok()
}

impl Compiled {
    /// Everything a message says, by the rules of this definition. A value that cannot be read is
    /// left out: dropped, and never stored as something it is not.
    pub fn decode(&self, topic: &str, body: &[u8]) -> Vec<Decoded> {
        let seen = Seen::new(topic, body);
        let mut out = Vec::new();
        for (rule, compiled) in self.definition.read.iter().zip(&self.reads) {
            let wild = match &rule.topic {
                Some(filter) => match topic_matches(filter, seen.topic) {
                    Some(w) => w,
                    None => continue,
                },
                None => Vec::new(),
            };
            let groups = match &compiled.pattern {
                Some(p) => match seen.text.and_then(|t| p.captures(t)) {
                    Some(c) => Some(c),
                    None => continue,
                },
                None => None,
            };
            if rule.when.as_ref().is_some_and(|w| !seen.holds(w)) {
                continue;
            }
            let device = device_of(rule, &wild, groups.as_ref(), compiled.pattern.as_ref());
            for f in &rule.fields {
                let Some(value) = extract(f, &seen, groups.as_ref()).and_then(|raw| shape(f, raw)) else { continue };
                let device = device.clone();
                if let Some(metric) = &f.metric {
                    if let Some(value) = as_number(&value) {
                        out.push(Decoded::Metric { device, metric: metric.clone(), value, unit: f.unit.clone(), class: f.class.clone(), deadband: f.deadband });
                    }
                } else if f.state == Some(true) {
                    out.push(Decoded::State { device, state: word(&value) });
                } else if let Some(key) = &f.attribute {
                    out.push(Decoded::Attribute { device, key: key.clone(), value });
                } else if let Some(name) = &f.event {
                    out.push(Decoded::Event { device, name: name.clone(), value });
                }
            }
        }
        out
    }

    /// What this message says about a command, if it says anything. `names` are the ids the command
    /// goes by: ours, and the equipment's.
    pub fn status(&self, operation: &str, device: &str, topic: &str, body: &[u8], names: &[&str]) -> Option<Ending> {
        let rule = self.definition.commands.get(operation)?.status.as_ref()?;
        self.judge(rule, self.statuses.get(operation).and_then(Option::as_ref), device, topic, body, names)
    }

    /// What this reply to a `find` says, by the same rules as a status.
    pub fn found(&self, operation: &str, device: &str, topic: &str, body: &[u8], names: &[&str]) -> Option<Ending> {
        let rule = self.definition.commands.get(operation)?.find.as_ref()?;
        let pattern = rule.pattern.as_deref().and_then(|p| Regex::new(p).ok());
        self.judge(rule, pattern.as_ref(), device, topic, body, names)
    }

    fn judge(&self, rule: &StatusRule, pattern: Option<&Regex>, device: &str, topic: &str, body: &[u8], names: &[&str]) -> Option<Ending> {
        let seen = Seen::new(topic, body);
        if let Some(filter) = &rule.topic {
            topic_matches(&filter.replace("{device}", device), topic)?;
        }
        let groups = match pattern {
            Some(p) => Some(p.captures(seen.text?)?),
            None => None,
        };
        if let Some(path) = &rule.id {
            let said = word(at_path(seen.json()?, path)?);
            if !names.iter().any(|n| *n == said) {
                return None;
            }
        }
        let f = Field { json: rule.json.clone(), group: rule.group.clone(), at: rule.at, kind: rule.kind, order: rule.order, ..Default::default() };
        let said = word(&extract(&f, &seen, groups.as_ref())?);
        Some(if rule.done.contains(&said) {
            Ending::Done
        } else if rule.failed.contains(&said) {
            Ending::Failed
        } else if rule.aborted.contains(&said) {
            Ending::Aborted
        } else if rule.running.contains(&said) {
            Ending::Running
        } else {
            Ending::Waiting
        })
    }

    pub fn command(&self, operation: &str) -> Option<&CommandDef> {
        self.definition.commands.get(operation)
    }
}

fn check_template(what: &str, template: &str) -> Result<(), String> {
    // `{payload.x}`, `{command}`, `{device}`, `{id}`, `{secret.x}`, `{settings.x}`: render with
    // everything present, so only an unknown root fails.
    let probe = json!({"command": "c", "device": "d", "id": "i"});
    match render(template, &probe) {
        Ok(_) => Ok(()),
        Err(e) if ["{payload", "{secret", "{settings"].iter().any(|r| e.contains(r)) => Ok(()),
        Err(e) => Err(format!("{what}: {e}. Placeholders are command, device, id, payload.*, secret.* and settings.*")),
    }
}

fn check_step(what: &str, step: &Step, scheme: Option<&str>) -> Result<(), String> {
    if let Some(h) = &step.hex {
        hex::decode(h.replace(' ', "")).map_err(|e| format!("{what}: hex: {e}"))?;
    }
    for path in [&step.get, &step.post, &step.put, &step.patch, &step.delete, &step.topic].into_iter().flatten() {
        check_template(what, path)?;
    }
    if let Some(Value::String(s)) = &step.send {
        check_template(what, s)?;
    }
    if let Some(read) = &step.read
        && !["holding", "input", "coil", "discrete"].contains(&read.as_str())
    {
        return Err(format!("{what}: read is holding, input, coil or discrete"));
    }
    let Some(scheme) = scheme else { return Ok(()) };
    let http = matches!(scheme, "http" | "https");
    let modbus = scheme == "modbus+tcp";
    if step.method().is_some() && !http {
        return Err(format!("{what}: get, post, put, patch and delete are for http, and this connects by {scheme}"));
    }
    if http && (step.send.is_some() || step.hex.is_some()) && step.method().is_none() {
        return Err(format!("{what}: say which method and path to send this with"));
    }
    if (step.read.is_some() || !step.write.is_empty()) && !modbus {
        return Err(format!("{what}: read and write are for modbus+tcp, and this connects by {scheme}"));
    }
    if modbus && (step.send.is_some() || step.hex.is_some()) {
        return Err(format!("{what}: Modbus takes read and write"));
    }
    Ok(())
}

fn check_field(what: &str, f: &Field) -> Result<(), String> {
    let says = usize::from(f.metric.is_some()) + usize::from(f.state == Some(true)) + usize::from(f.attribute.is_some()) + usize::from(f.event.is_some());
    if says != 1 {
        return Err(format!("{what}: say one of metric, state, attribute or event"));
    }
    if let Some(m) = &f.metric
        && (m.is_empty() || m.len() > 120 || !m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/')))
    {
        return Err(format!("{what}: {m} is not a metric's name"));
    }
    let sources = usize::from(f.json.is_some())
        + usize::from(f.group.is_some())
        + usize::from(f.at.is_some())
        + usize::from(f.reg.is_some())
        + usize::from(f.value.is_some());
    if sources > 1 {
        return Err(format!("{what}: take it from one of json, group, at, reg or value"));
    }
    if matches!(f.kind, Some(Kind::Text | Kind::Hex)) && f.len.is_none() {
        return Err(format!("{what}: text and hex fields need a len"));
    }
    if f.bit.is_some_and(|b| b > 63) {
        return Err(format!("{what}: there is no bit that high"));
    }
    if let Some(class) = &f.class
        && !["Sample", "State", "Alarm"].contains(&class.as_str())
    {
        return Err(format!("{what}: class is Sample, State or Alarm"));
    }
    Ok(())
}

/// The scheme of a connection address.
pub fn scheme(connect: &str) -> Option<&str> {
    connect.split_once("://").map(|(s, _)| s).filter(|s| SCHEMES.contains(s))
}

/// Reads and checks a definition. Every error names where it is, so a person can fix it from the
/// message, and a definition that passes here cannot fail for its shape on a gateway.
pub fn parse(text: &str) -> Result<Compiled, String> {
    let definition: Definition = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    let d = &definition;
    if d.name.trim().is_empty() {
        return Err("a definition needs a name".into());
    }
    let scheme_of = match &d.connect {
        Some(c) if !c.contains('{') => Some(scheme(c).ok_or_else(|| format!("connect: {c} is not an address this can open. Schemes: {}", SCHEMES.join(", ")))?),
        _ => None,
    };
    for (i, step) in d.on_connect.iter().enumerate() {
        check_step(&format!("on_connect[{i}]"), step, scheme_of)?;
    }
    for (i, p) in d.poll.iter().enumerate() {
        if p.step.is_empty() {
            return Err(format!("poll[{i}]: say what to send"));
        }
        check_step(&format!("poll[{i}]"), &p.step, scheme_of)?;
    }
    let mut reads = Vec::new();
    for (i, r) in d.read.iter().enumerate() {
        let what = format!("read[{i}]");
        let pattern = r.pattern.as_deref().map(Regex::new).transpose().map_err(|e| format!("{what}: {e}"))?;
        if r.fields.is_empty() {
            return Err(format!("{what}: it finds nothing"));
        }
        for f in &r.fields {
            check_field(&what, f)?;
            if let (Some(g), Some(p)) = (&f.group, &pattern)
                && !p.capture_names().flatten().any(|n| n == g)
            {
                return Err(format!("{what}: match has no group called {g}"));
            }
            if f.group.is_some() && pattern.is_none() {
                return Err(format!("{what}: group needs a match"));
            }
        }
        reads.push(CompiledRead { pattern });
    }
    let mut statuses = BTreeMap::new();
    for (name, c) in &d.commands {
        let what = format!("commands.{name}");
        check_step(&what, &c.step, scheme_of)?;
        if c.step.is_empty() {
            return Err(format!("{what}: say what to send"));
        }
        if c.status.is_none() && !c.once {
            return Err(format!("{what}: say how it is known to have ended with status, or set once: true if this device can prove nothing beyond the send"));
        }
        if let Some(s) = &c.status {
            if s.done.is_empty() {
                return Err(format!("{what}: status needs a value that means done"));
            }
            if let Some(p) = &s.poll {
                check_step(&format!("{what}.status"), p, scheme_of)?;
            }
        }
        if let Some(step) = &c.cancel {
            check_step(&format!("{what}.cancel"), step, scheme_of)?;
        }
        let pattern = c.status.as_ref().and_then(|s| s.pattern.as_deref()).map(Regex::new).transpose().map_err(|e| format!("{what}.status: {e}"))?;
        statuses.insert(name.clone(), pattern);
    }
    let digest = hex::encode(Sha256::digest(text.as_bytes()));
    Ok(Compiled { definition, digest, reads, statuses })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(text: &str) -> Compiled {
        parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_stream_is_cut_into_messages_and_finds_its_feet() {
        let mut b = b"12.5 kg\r\n13".to_vec();
        let line = Frame::Line("\r\n".into());
        assert_eq!(line.take(&mut b), Some(b"12.5 kg".to_vec()));
        assert_eq!(line.take(&mut b), None, "half a message waits");
        assert_eq!(b, b"13");

        let mut b = b"junk\x02hello\x03\x02wo".to_vec();
        let between = Frame::Between("\x02".into(), "\x03".into());
        assert_eq!(between.take(&mut b), Some(b"hello".to_vec()));
        assert_eq!(between.take(&mut b), None);
        assert_eq!(b, b"\x02wo");

        let length = Frame::Length(LengthField { at: 1, size: 2, little: false, add: 3 });
        let mut b = vec![0xaa, 0, 2, 1, 2, 0xaa];
        assert_eq!(length.take(&mut b), Some(vec![0xaa, 0, 2, 1, 2]));
        assert_eq!(b, vec![0xaa]);

        // A length that cannot be: skip a byte and look again, rather than wait for ever.
        let mut b = vec![0xff, 0xff, 0xff, 0xff, 0xff, 0];
        let wide = Frame::Length(LengthField { at: 0, size: 4, little: false, add: 0 });
        assert_eq!(wide.take(&mut b), None);
        assert_eq!(b.len(), 5);

        let mut b = vec![1, 2, 3, 4, 5];
        assert_eq!(Frame::Fixed(2).take(&mut b), Some(vec![1, 2]));
    }

    #[test]
    fn the_scale_in_the_module_comment_works() {
        let d = def(r#"
name: acme-scale
connect: tcp://10.0.0.7:9100
frame: {line: "\r\n"}
poll:
  - {every: 5s, send: "W\r\n"}
read:
  - {match: '^(?P<kg>-?[0-9.]+) kg$', metric: weight.kg, group: kg, unit: kg}
commands:
  scale.tare:
    send: "T\r\n"
    status: {match: '^(?P<r>OK|ERR)', group: r, done: OK, failed: ERR}
"#);
        assert_eq!(d.definition.poll[0].every, Duration::from_secs(5));
        let got = d.decode("", b"12.5 kg");
        assert_eq!(got, vec![Decoded::Metric { device: None, metric: "weight.kg".into(), value: 12.5, unit: Some("kg".into()), class: None, deadband: None }]);
        assert!(d.decode("", b"not a weight").is_empty());
        assert_eq!(d.status("scale.tare", "S", "", b"OK", &[]), Some(Ending::Done));
        assert_eq!(d.status("scale.tare", "S", "", b"ERR 4", &[]), Some(Ending::Failed));
        assert_eq!(d.status("scale.tare", "S", "", b"12.5 kg", &[]), None, "a weight says nothing about a tare");
    }

    #[test]
    fn json_on_a_shared_connection_finds_its_device() {
        let d = def(r#"
name: sensors
connect: mqtt://broker:1883
subscribe: ["plant/+/telemetry"]
read:
  - topic: plant/+/telemetry
    device: "{topic.1}"
    values:
      - {metric: temp.c, json: $.env.temp, scale: 0.1, round: 1}
      - {state: true, json: $.mode, map: {1: Running, 0: Idle}}
      - {attribute: firmware, json: $.fw}
      - {metric: rssi, json: $.not.there}
"#);
        let got = d.decode("plant/PRESS-1/telemetry", br#"{"env": {"temp": 215}, "mode": 1, "fw": "1.2.3"}"#);
        assert_eq!(got.len(), 3, "a field a device left out is not an error: {got:?}");
        assert!(matches!(&got[0], Decoded::Metric { device: Some(d), metric, value, .. } if d == "PRESS-1" && metric == "temp.c" && *value == 21.5), "{got:?}");
        assert_eq!(got[1], Decoded::State { device: Some("PRESS-1".into()), state: "Running".into() });
        assert_eq!(got[2], Decoded::Attribute { device: Some("PRESS-1".into()), key: "firmware".into(), value: json!("1.2.3") });
        assert!(d.decode("elsewhere/x", br#"{"env": {"temp": 1}}"#).is_empty());
    }

    #[test]
    fn binary_fields_in_every_order_a_plc_uses() {
        let d = def(r#"
name: frame
frame: {length: {at: 2, size: 2, add: 4}}
read:
  - when: {at: 0, is: 5a5a}
    values:
      - {metric: a, at: 4, type: i16, scale: 0.1}
      - {metric: b, at: 6, type: u16, order: le}
      - {metric: c, at: 8, type: f32, order: cdab}
      - {metric: d, at: 12, type: u8, bit: 3}
      - {attribute: serial, at: 13, type: text, len: 3}
"#);
        let f = 21.5f32.to_be_bytes();
        let mut m = vec![0x5a, 0x5a, 0, 12];
        m.extend((-125i16).to_be_bytes());
        m.extend(0x1234u16.to_le_bytes());
        m.extend([f[2], f[3], f[0], f[1]]);
        m.push(0b0000_1000);
        m.extend(b"X9\0");
        let got = d.decode("", &m);
        let number = |name: &str| {
            got.iter()
                .find_map(|g| {
                    matches!(g, Decoded::Metric { metric, .. } if metric == name).then(|| if let Decoded::Metric { value, .. } = g { *value } else { 0.0 })
                })
                .unwrap()
        };
        assert!((number("a") + 12.5).abs() < 1e-9);
        assert_eq!(number("b"), 4660.0);
        assert_eq!(number("c"), 21.5);
        assert_eq!(number("d"), 1.0);
        assert!(got.iter().any(|g| matches!(g, Decoded::Attribute { key, value, .. } if key == "serial" && value == "X9")), "{got:?}");
        m[0] = 0;
        assert!(d.decode("", &m).is_empty(), "the header decides");
        assert!(d.decode("", &[0x5a, 0x5a]).is_empty(), "a short message reads as nothing, never as zero");
    }

    #[test]
    fn every_word_order_reads_the_same_number() {
        let n = 0x1234_5678u32;
        let b = n.to_be_bytes();
        assert_eq!(number_at(&b, 0, Kind::U32, Order::Be), Some(json!(n)));
        assert_eq!(number_at(&[b[3], b[2], b[1], b[0]], 0, Kind::U32, Order::Le), Some(json!(n)));
        assert_eq!(number_at(&[b[2], b[3], b[0], b[1]], 0, Kind::U32, Order::Cdab), Some(json!(n)));
        assert_eq!(number_at(&[b[1], b[0], b[3], b[2]], 0, Kind::U32, Order::Badc), Some(json!(n)));
    }

    #[test]
    fn a_status_is_read_for_the_right_command_and_anything_else_proves_nothing() {
        let d = def(r#"
name: rest
connect: http://robot.local
commands:
  robot.go:
    post: /missions
    id: $.id
    status:
      poll: {get: "/missions/{id}"}
      id: $.id
      json: $.state
      running: [Pending, Executing]
      done: Done
      failed: [Aborted, Error]
"#);
        let names = ["mission-7"];
        let say = |s: &str| format!(r#"{{"id": "mission-7", "state": "{s}"}}"#).into_bytes();
        assert_eq!(d.status("robot.go", "R", "", &say("Executing"), &names), Some(Ending::Running));
        assert_eq!(d.status("robot.go", "R", "", &say("Done"), &names), Some(Ending::Done));
        assert_eq!(d.status("robot.go", "R", "", &say("Error"), &names), Some(Ending::Failed));
        assert_eq!(d.status("robot.go", "R", "", &say("Paused"), &names), Some(Ending::Waiting), "a word it does not know proves nothing");
        assert_eq!(d.status("robot.go", "R", "", br#"{"id": "other", "state": "Done"}"#, &names), None, "another mission's end is not this one's");
    }

    #[test]
    fn placeholders_fill_or_refuse() {
        let vars = json!({"command": "c1", "device": "AMR-1", "payload": {"job": 412, "to": "A"}, "settings": {"unit": 3}});
        assert_eq!(render("job {payload.job} on {device}", &vars).unwrap(), "job 412 on AMR-1");
        assert_eq!(render("unit {settings.unit}", &vars).unwrap(), "unit 3");
        assert!(render("{payload.missing}", &vars).is_err(), "half a command must not reach a machine");
        assert_eq!(render(r#"{"a": 1}"#, &vars).unwrap(), r#"{"a": 1}"#, "JSON in a template is left alone");
        let body = render_json(&json!({"job": "{payload.job}", "label": "for {device}", "fixed": 3}), &vars).unwrap();
        assert_eq!(body, json!({"job": 412, "label": "for AMR-1", "fixed": 3}), "a number stays a number");
        assert!(render_json(&json!({"x": "{payload.nope}"}), &vars).is_err());
    }

    #[test]
    fn a_mistake_is_found_when_it_is_saved_and_says_where() {
        let fails = |t: &str, why: &str| {
            let e = parse(t).err().unwrap_or_else(|| panic!("{t} passed"));
            assert!(e.contains(why), "{e}");
        };
        fails("name: x\nconnect: foo://bar", "not an address");
        fails("name: x\nread:\n  - {metirc: a}", "metirc");
        fails("name: x\nread:\n  - {json: $.a}", "say one of metric");
        fails("name: x\nread:\n  - {metric: a, match: '('}", "regex");
        fails("name: x\nread:\n  - {match: 'a', metric: b, group: nope}", "no group called nope");
        fails("name: x\nread:\n  - {metric: a, group: nope}", "group needs a match");
        fails("name: x\ncommands:\n  a.b: {send: hi}", "how it is known to have ended");
        fails("name: x\ncommands:\n  a.b: {once: true}", "say what to send");
        fails("name: x\nconnect: tcp://a:1\ncommands:\n  a.b: {get: /x, once: true}", "are for http");
        fails("name: x\nconnect: http://a\ncommands:\n  a.b: {send: hi, once: true}", "which method");
        fails("name: x\nconnect: tcp://a:1\npoll:\n  - {every: never, send: x}", "not a duration");
        fails("name: x\nconnect: tcp://a:1\npoll:\n  - {every: 5s}", "say what to send");
        fails("name: x\ncommands:\n  a.b: {send: 'go {nope}', once: true}", "Placeholders are");
        fails("name: x\nread:\n  - {metric: 'no spaces', json: $.a}", "not a metric");
        fails("name: x\nconnect: modbus+tcp://a:502\npoll:\n  - {every: 1s, send: hi}", "Modbus takes");
        fails("name: x\nconnect: tcp://a:1\npoll:\n  - {every: 1s, read: holding, at: 1}", "are for modbus");
    }

    #[test]
    fn durations_read_as_people_write_them() {
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("soon"), None);
    }
}
