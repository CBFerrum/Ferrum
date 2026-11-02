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

use crate::sparkplug;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::time::Duration;

/// The schemes a site can open. Anything else is an error when the definition is saved, not when it
/// reaches a gateway at three in the morning.
pub const SCHEMES: [&str; 16] =
    ["tcp", "udp", "serial", "http", "https", "ws", "wss", "mqtt", "mqtts", "modbus+tcp", "modbus+rtu", "process", "opcua", "listen", "listen+udp", "webhook"];

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
    /// Topics to hold open, or OPC UA nodes to watch.
    #[serde(default)]
    pub subscribe: Vec<String>,
    /// Added to what is sent as bytes, and checked on what arrives: what fails is dropped.
    #[serde(default)]
    pub checksum: Option<Checksum>,
    /// Certificates for a plant's own authority, by name in the site's `tls` directory.
    #[serde(default)]
    pub tls: Option<Tls>,
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

/// What a message is written in, when that is not JSON or plain bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Json,
    /// Every element with no children, flat: `[{tag, parent, text, attrs, of}]`. `of` holds the
    /// attributes of the elements around it, by their names.
    Xml,
    /// A Sparkplug B payload: `{timestamp, seq, metrics: [{name, alias, datatype, value}]}`.
    Sparkplug,
}

/// How a frame is checked. The check sits at its end, and covers it from `from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checksum {
    pub kind: ChecksumKind,
    #[serde(default)]
    pub from: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChecksumKind {
    /// Two bytes, low first.
    Crc16Modbus,
    /// CCITT-FALSE: two bytes, high first.
    Crc16Ccitt,
    /// Four bytes, high first.
    Crc32,
    Sum8,
    Xor8,
    /// The two's complement of the sum, as Modbus ASCII and many meters use.
    Lrc,
}

impl Checksum {
    fn over(&self, bytes: &[u8]) -> Vec<u8> {
        let bytes = bytes.get(self.from..).unwrap_or_default();
        match self.kind {
            ChecksumKind::Crc16Modbus => {
                let crc = bytes.iter().fold(0xffffu16, |crc, b| (0..8).fold(crc ^ u16::from(*b), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xa001 } else { c >> 1 }));
                crc.to_le_bytes().to_vec()
            }
            ChecksumKind::Crc16Ccitt => {
                let crc = bytes
                    .iter()
                    .fold(0xffffu16, |crc, b| (0..8).fold(crc ^ (u16::from(*b) << 8), |c, _| if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 }));
                crc.to_be_bytes().to_vec()
            }
            ChecksumKind::Crc32 => {
                let crc = bytes.iter().fold(!0u32, |crc, b| (0..8).fold(crc ^ u32::from(*b), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 }));
                (!crc).to_be_bytes().to_vec()
            }
            ChecksumKind::Sum8 => vec![bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b))],
            ChecksumKind::Xor8 => vec![bytes.iter().fold(0u8, |a, b| a ^ b)],
            ChecksumKind::Lrc => vec![bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)).wrapping_neg()],
        }
    }

    /// `bytes` with its check on the end.
    pub fn seal(&self, mut bytes: Vec<u8>) -> Vec<u8> {
        let check = self.over(&bytes);
        bytes.extend(check);
        bytes
    }

    /// The message without its check, if the check holds.
    pub fn open<'a>(&self, bytes: &'a [u8]) -> Option<&'a [u8]> {
        let size = self.over(&[]).len();
        let (body, check) = bytes.split_at_checked(bytes.len().checked_sub(size)?)?;
        (self.over(body) == check).then_some(body)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// The authority to trust, instead of the public ones.
    #[serde(default)]
    pub ca: Option<String>,
    /// This gateway's own certificate and key, for a server that asks.
    #[serde(default)]
    pub cert: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
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
    /// What every message starts with, in hexadecimal, for a stream to find its place by.
    #[serde(default)]
    pub start: Option<String>,
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
                if let Some(marker) = f.start.as_deref().and_then(|m| hex::decode(m.replace(' ', "")).ok()) {
                    match find(buf, &marker) {
                        Some(at) => drop(buf.drain(..at)),
                        None => {
                            buf.drain(..buf.len().saturating_sub(marker.len().saturating_sub(1)));
                            return None;
                        }
                    }
                }
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
    /// MQTT: the broker keeps it for whoever subscribes next.
    #[serde(default)]
    pub retain: bool,
    /// How `send` is written, when not as JSON or text.
    #[serde(default)]
    pub format: Option<Format>,
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
    /// Modbus: the unit on a shared bus, when it is not the connection's. A number or `{settings.unit}`.
    #[serde(default)]
    pub unit: Option<Value>,
    /// OPC UA: a node to read, or to write `send` to.
    #[serde(default)]
    pub node: Option<String>,
    /// OPC UA: a method of `node` to call, with `send` as its arguments.
    #[serde(default)]
    pub call: Option<String>,
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
        self.send.is_none() && self.hex.is_none() && self.method().is_none() && self.read.is_none() && self.write.is_empty() && self.node.is_none()
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
    /// Report a number at most this often, whatever rate it arrives at.
    #[serde(default, deserialize_with = "maybe_duration")]
    pub every: Option<Duration>,
    /// Where the message says when this was true, for equipment that keeps its own time: a path to
    /// RFC 3339, or to seconds or milliseconds since 1970.
    #[serde(default)]
    pub time: Option<String>,
    /// `Sample`, `State` or `Alarm`: what may be shed first when space runs out.
    #[serde(default)]
    pub class: Option<String>,
}

/// When a message counts. Without one, every message that reaches the rule does. Under `each`, it
/// is asked of each element.
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
    /// Whose message this is, for a connection several devices share: `{topic.1}`, `{group.id}`,
    /// `{each.x}`.
    pub device: Option<String>,
    pub format: Format,
    /// A list or map in the message: the rule is applied to each element, its paths start there,
    /// and a value's name can come from it: `metric: "{each.name}"`. A map's elements are
    /// `{key, value}`. This is what makes a device that reports hundreds of values one line.
    pub each: Option<String>,
    pub fields: Vec<Field>,
}

impl<'de> Deserialize<'de> for ReadRule {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut m = Map::deserialize(d)?;
        let text = |m: &mut Map<String, Value>, k: &str| m.remove(k).and_then(|v| v.as_str().map(str::to_string));
        let topic = text(&mut m, "topic");
        let pattern = text(&mut m, "match");
        let device = text(&mut m, "device");
        let each = text(&mut m, "each");
        let format = m.remove("format").map(serde_json::from_value).transpose().map_err(de::Error::custom)?.unwrap_or_default();
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
        Ok(ReadRule { topic, pattern, when, device, format, each, fields })
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
    #[serde(default)]
    pub format: Format,
    /// Other things that may say how it ended, for a protocol with no one field for it: the first
    /// that says it has ended settles it.
    #[serde(default)]
    pub also: Vec<StatusRule>,
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

fn maybe_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    duration(d).map(Some)
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
    Metric {
        device: Option<String>,
        metric: String,
        value: f64,
        unit: Option<String>,
        class: Option<String>,
        deadband: Option<f64>,
        every: Option<Duration>,
        at: Option<DateTime<Utc>>,
    },
    State {
        device: Option<String>,
        state: String,
    },
    Attribute {
        device: Option<String>,
        key: String,
        value: Value,
    },
    Event {
        device: Option<String>,
        name: String,
        value: Value,
    },
}

struct CompiledRead {
    pattern: Option<Regex>,
}

/// A definition that passed [`parse`], with its patterns ready.
pub struct Compiled {
    pub definition: Definition,
    pub digest: String,
    reads: Vec<CompiledRead>,
    /// By operation: the patterns of a status and of each of its `also`, in order.
    statuses: BTreeMap<String, Vec<Option<Regex>>>,
}

/// What a decoder remembers between messages on one link.
pub type Memory = crate::sparkplug::Aliases;

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
    parsed: OnceCell<Option<Value>>,
    xml: OnceCell<Option<Value>>,
    sparkplug: OnceCell<Option<Value>>,
    /// Which of those the rule at hand reads.
    format: Format,
}

impl<'a> Seen<'a> {
    fn new(topic: &'a str, body: &'a [u8]) -> Seen<'a> {
        Seen {
            topic,
            body,
            text: std::str::from_utf8(body).ok(),
            parsed: OnceCell::new(),
            xml: OnceCell::new(),
            sparkplug: OnceCell::new(),
            format: Format::Json,
        }
    }

    /// One element of a list, seen as a message of its own.
    fn element(&self, value: Value) -> Seen<'a> {
        Seen {
            topic: self.topic,
            body: &[],
            text: None,
            parsed: OnceCell::from(Some(value)),
            xml: OnceCell::new(),
            sparkplug: OnceCell::new(),
            format: Format::Json,
        }
    }

    fn json(&self) -> Option<&Value> {
        match self.format {
            Format::Json => self.parsed.get_or_init(|| serde_json::from_slice(self.body).ok()).as_ref(),
            Format::Xml => self.xml.get_or_init(|| flatten_xml(self.body)).as_ref(),
            Format::Sparkplug => self.sparkplug.get_or_init(|| sparkplug::decode(self.body)).as_ref(),
        }
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

/// See [`Format::Xml`]. An XML document is usually a few levels of wrapping around leaves that all
/// look alike, so a flat list reads better from a definition than a tree, and needs no XPath.
fn flatten_xml(body: &[u8]) -> Option<Value> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(body);
    let (mut open, mut leaves): (Vec<(String, Map<String, Value>, bool)>, Vec<Value>) = (Vec::new(), Vec::new());
    let (mut text, mut buf) = (String::new(), Vec::new());
    let attrs = |e: &quick_xml::events::BytesStart<'_>| -> Map<String, Value> {
        e.attributes()
            .flatten()
            .map(|a| {
                let value = quick_xml::escape::unescape(&a.value).map(|v| v.to_string()).unwrap_or_else(|_| a.value.to_string());
                (a.key.local_name().as_ref().to_string(), json!(value))
            })
            .collect()
    };
    let name = |e: &quick_xml::events::BytesStart<'_>| e.local_name().as_ref().to_string();
    let leaf = |open: &[(String, Map<String, Value>, bool)], tag: String, attrs: Map<String, Value>, text: &str| {
        let of: Map<String, Value> = open.iter().filter(|(_, a, _)| !a.is_empty()).map(|(t, a, _)| (t.clone(), Value::Object(a.clone()))).collect();
        json!({"tag": tag, "parent": open.last().map(|(t, _, _)| t), "text": text.trim(), "attrs": attrs, "of": of})
    };
    loop {
        match reader.read_event_into(&mut buf).ok()? {
            Event::Start(e) => {
                if let Some(parent) = open.last_mut() {
                    parent.2 = true;
                }
                open.push((name(&e), attrs(&e), false));
                text.clear();
            }
            Event::Empty(e) => {
                if let Some(parent) = open.last_mut() {
                    parent.2 = true;
                }
                leaves.push(leaf(&open, name(&e), attrs(&e), ""));
            }
            Event::Text(t) => text.push_str(&t.xml10_content()),
            Event::CData(t) => text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => {
                let raw = format!("&{};", r.xml10_content());
                text.push_str(&quick_xml::escape::unescape(&raw).map(|v| v.to_string()).unwrap_or(raw));
            }
            Event::End(_) => {
                let (tag, attrs, parent) = open.pop()?;
                if !parent {
                    leaves.push(leaf(&open, tag, attrs, &text));
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    (!leaves.is_empty()).then_some(Value::Array(leaves))
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

/// A name a device chose, made into one a metric can have. Sparkplug's have spaces in them.
fn named(template: &str, each: &Value) -> Option<String> {
    let name = render(template, &json!({"each": each})).ok()?;
    let clean: String = name.chars().take(120).map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/') { c } else { '_' }).collect();
    (!clean.is_empty()).then_some(clean)
}

fn device_of(rule: &ReadRule, wild: &[&str], groups: Option<&regex::Captures<'_>>, pattern: Option<&Regex>, each: Option<&Value>) -> Option<String> {
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
    render(template, &json!({"topic": topic, "group": group, "each": each})).ok()
}

/// A time as equipment gives it. Below 10^11 a number is seconds: as milliseconds that is 1973.
fn when_of(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(t) => t.parse().ok(),
        Value::Number(n) => {
            let n = n.as_f64()?;
            DateTime::from_timestamp_millis(if n < 1e11 { n * 1000.0 } else { n } as i64)
        }
        _ => None,
    }
}

/// Everything one element or whole message says, by one rule.
fn say(rule: &ReadRule, seen: &Seen<'_>, device: Option<String>, each: &Value, groups: Option<&regex::Captures<'_>>, out: &mut Vec<Decoded>) {
    for f in &rule.fields {
        let Some(value) = extract(f, seen, groups).and_then(|raw| shape(f, raw)) else { continue };
        let device = device.clone();
        if let Some(metric) = &f.metric {
            if let (Some(value), Some(metric)) = (as_number(&value), named(metric, each)) {
                let at = f.time.as_deref().and_then(|path| at_path(seen.json()?, path)).and_then(when_of);
                out.push(Decoded::Metric { device, metric, value, unit: f.unit.clone(), class: f.class.clone(), deadband: f.deadband, every: f.every, at });
            }
        } else if f.state == Some(true) {
            out.push(Decoded::State { device, state: word(&value) });
        } else if let Some(key) = f.attribute.as_deref().and_then(|k| named(k, each)) {
            out.push(Decoded::Attribute { device, key, value });
        } else if let Some(name) = f.event.as_deref().and_then(|n| named(n, each)) {
            out.push(Decoded::Event { device, name, value });
        }
    }
}

impl Compiled {
    /// Everything a message says, by the rules of this definition. A value that cannot be read is
    /// left out: dropped, and never stored as something it is not.
    pub fn decode(&self, topic: &str, body: &[u8]) -> Vec<Decoded> {
        self.decode_with(topic, body, &mut Memory::default())
    }

    /// As [`Self::decode`], for a link that keeps what earlier messages taught it.
    pub fn decode_with(&self, topic: &str, body: &[u8], memory: &mut Memory) -> Vec<Decoded> {
        let mut seen = Seen::new(topic, body);
        if self.definition.read.iter().any(|r| r.format == Format::Sparkplug)
            && let Some(mut payload) = sparkplug::decode(body)
        {
            memory.resolve(topic, &mut payload);
            seen.sparkplug = OnceCell::from(Some(payload));
        }
        let mut out = Vec::new();
        for (rule, compiled) in self.definition.read.iter().zip(&self.reads) {
            seen.format = rule.format;
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
            if let Some(path) = &rule.each {
                let elements = match seen.json().and_then(|j| at_path(j, path)) {
                    Some(Value::Array(a)) => a.clone(),
                    Some(Value::Object(m)) => m.iter().map(|(k, v)| json!({"key": k, "value": v})).collect(),
                    _ => continue,
                };
                for element in elements {
                    let one = seen.element(element.clone());
                    if rule.when.as_ref().is_none_or(|w| one.holds(w)) {
                        let device = device_of(rule, &wild, groups.as_ref(), compiled.pattern.as_ref(), Some(&element));
                        say(rule, &one, device, &element, groups.as_ref(), &mut out);
                    }
                }
                continue;
            }
            if rule.when.as_ref().is_some_and(|w| !seen.holds(w)) {
                continue;
            }
            let device = device_of(rule, &wild, groups.as_ref(), compiled.pattern.as_ref(), None);
            say(rule, &seen, device, &Value::Null, groups.as_ref(), &mut out);
        }
        out
    }

    /// What this message says about a command, if it says anything. `names` are the ids the command
    /// goes by: ours, and the equipment's.
    pub fn status(&self, operation: &str, device: &str, topic: &str, body: &[u8], names: &[&str]) -> Option<Ending> {
        let rule = self.definition.commands.get(operation)?.status.as_ref()?;
        let patterns = self.statuses.get(operation)?;
        let chain = std::iter::once(rule).chain(&rule.also);
        let said: Vec<Ending> = chain.zip(patterns).filter_map(|(r, p)| self.judge(r, p.as_ref(), device, topic, body, names)).collect();
        said.iter().find(|e| !matches!(e, Ending::Waiting | Ending::Running)).or(said.first()).copied()
    }

    /// What this reply to a `find` says, by the same rules as a status.
    pub fn found(&self, operation: &str, device: &str, topic: &str, body: &[u8], names: &[&str]) -> Option<Ending> {
        let rule = self.definition.commands.get(operation)?.find.as_ref()?;
        let pattern = rule.pattern.as_deref().and_then(|p| Regex::new(p).ok());
        self.judge(rule, pattern.as_ref(), device, topic, body, names)
    }

    fn judge(&self, rule: &StatusRule, pattern: Option<&Regex>, device: &str, topic: &str, body: &[u8], names: &[&str]) -> Option<Ending> {
        let mut seen = Seen::new(topic, body);
        seen.format = rule.format;
        if let Some(filter) = &rule.topic {
            // After a restart a running command's device may not be known here. Where the answer
            // names its command, any device's topic will do. Where it does not, nothing can say
            // whose it is, and this waits rather than believing another device's news.
            let device = if device.is_empty() && rule.id.is_some() { "+" } else { device };
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
    let probe = json!({"command": "c", "device": "d", "id": "i", "now": "t", "now_ms": 1, "seq": 1});
    match render(template, &probe) {
        Ok(_) => Ok(()),
        Err(e) if ["{payload", "{secret", "{settings"].iter().any(|r| e.contains(r)) => Ok(()),
        Err(e) => Err(format!("{what}: {e}. Placeholders are command, device, id, now, now_ms, seq, payload.*, secret.* and settings.*")),
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
    let modbus = matches!(scheme, "modbus+tcp" | "modbus+rtu");
    let opcua = scheme == "opcua";
    let mqtt = matches!(scheme, "mqtt" | "mqtts");
    if step.method().is_some() && !http {
        return Err(format!("{what}: get, post, put, patch and delete are for http, and this connects by {scheme}"));
    }
    if http && (step.send.is_some() || step.hex.is_some()) && step.method().is_none() {
        return Err(format!("{what}: say which method and path to send this with"));
    }
    if (step.read.is_some() || !step.write.is_empty() || step.unit.is_some()) && !modbus {
        return Err(format!("{what}: read, write and unit are for Modbus, and this connects by {scheme}"));
    }
    if (step.node.is_some() || step.call.is_some()) && !opcua {
        return Err(format!("{what}: node and call are for opcua, and this connects by {scheme}"));
    }
    if step.call.is_some() && step.node.is_none() {
        return Err(format!("{what}: call needs the node it belongs to"));
    }
    if opcua && step.node.is_none() && (step.send.is_some() || step.hex.is_some()) {
        return Err(format!("{what}: say which node this is for"));
    }
    if step.retain && !mqtt {
        return Err(format!("{what}: retain is for MQTT, and this connects by {scheme}"));
    }
    if modbus && (step.send.is_some() || step.hex.is_some()) {
        return Err(format!("{what}: Modbus takes read and write"));
    }
    Ok(())
}

fn check_field(what: &str, f: &Field, each: bool) -> Result<(), String> {
    let says = usize::from(f.metric.is_some()) + usize::from(f.state == Some(true)) + usize::from(f.attribute.is_some()) + usize::from(f.event.is_some());
    if says != 1 {
        return Err(format!("{what}: say one of metric, state, attribute or event"));
    }
    for name in [&f.metric, &f.attribute, &f.event].into_iter().flatten() {
        if name.contains("{each.") && !each {
            return Err(format!("{what}: {name} needs each, to say what to take it from"));
        }
    }
    if let Some(m) = &f.metric
        && !m.contains("{each.")
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
    if let Some(s) = scheme_of {
        if d.checksum.is_some() && !matches!(s, "tcp" | "udp" | "serial" | "listen" | "listen+udp") {
            return Err(format!("checksum: {s} has checks of its own"));
        }
        if !d.on_connect.is_empty() && matches!(s, "listen" | "listen+udp" | "webhook") {
            return Err(format!("on_connect: nothing is connected to at {s}: equipment calls in"));
        }
    }
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
            check_field(&what, f, r.each.is_some())?;
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
            if s.done.is_empty() && s.also.iter().all(|a| a.done.is_empty()) {
                return Err(format!("{what}: status needs a value that means done"));
            }
            if let Some(p) = &s.poll {
                check_step(&format!("{what}.status"), p, scheme_of)?;
            }
        }
        if let Some(step) = &c.cancel {
            check_step(&format!("{what}.cancel"), step, scheme_of)?;
        }
        let chain = c.status.iter().flat_map(|s| std::iter::once(s).chain(&s.also));
        let patterns =
            chain.map(|s| s.pattern.as_deref().map(Regex::new).transpose()).collect::<Result<Vec<_>, _>>().map_err(|e| format!("{what}.status: {e}"))?;
        statuses.insert(name.clone(), patterns);
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
    fn each_makes_a_device_with_hundreds_of_values_one_line() {
        let d = def(r#"
name: bridge
connect: mqtt://b:1883
subscribe: ["plant/#"]
read:
  - topic: plant/+/tags
    device: "{topic.1}"
    each: $.tags
    when: {json: $.good, is: true}
    metric: "{each.name}"
    json: $.v
    every: 5s
  - topic: plant/+/info
    device: "{topic.1}"
    each: $
    attribute: "{each.key}"
    json: $.value
"#);
        let body = br#"{"tags":[{"name":"Temp Sensor/1","v":21.5,"good":true},{"name":"bad","v":0,"good":false},{"name":"s","v":"not a number","good":true}]}"#;
        let got = d.decode("plant/PRESS-1/tags", body);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            matches!(&got[0], Decoded::Metric { device: Some(c), metric, value, every, .. } if c == "PRESS-1" && metric == "Temp_Sensor/1" && *value == 21.5 && *every == Some(Duration::from_secs(5))),
            "a name a device chose is made fit, and {got:?}"
        );
        let got = d.decode("plant/PRESS-1/info", br#"{"fw":"1.2","model":"X"}"#);
        assert!(got.contains(&Decoded::Attribute { device: Some("PRESS-1".into()), key: "fw".into(), value: json!("1.2") }), "{got:?}");
        assert_eq!(got.len(), 2, "a map's elements are its keys");
        assert!(d.decode("plant/PRESS-1/tags", b"not json").is_empty());
        assert!(parse("name: x\nread:\n  - {metric: \"{each.name}\", json: $.v}\n").is_err_and(|e| e.contains("needs each")));
    }

    #[test]
    fn sparkplug_is_read_by_name_even_when_it_arrives_by_alias() {
        let d = def(r#"
name: edge-node
connect: mqtts://b:8883
subscribe: ["spBv1.0/plant/+/+/+"]
read:
  - {topic: "spBv1.0/plant/+/+/+", format: sparkplug, device: "{topic.3}", each: $.metrics, metric: "{each.name}", json: $.value}
commands:
  machine.set:
    topic: spBv1.0/plant/DCMD/node/{device}
    format: sparkplug
    send: {timestamp: "{now_ms}", metrics: [{name: Setpoint, value: "{payload.value}", type: double}]}
    once: true
"#);
        let named = crate::sparkplug::encode(&json!({"metrics": [{"name": "Temp", "value": 21.5}]})).unwrap();
        assert!(
            matches!(&d.decode("spBv1.0/plant/DDATA/node/M1", &named)[0], Decoded::Metric { device: Some(c), metric, .. } if c == "M1" && metric == "Temp")
        );

        // A birth with aliases, then data with only the alias. Field 2 of a metric carries it.
        let with_alias = |name: Option<&str>| {
            let mut m = Vec::new();
            if let Some(n) = name {
                m.extend([0x0a, n.len() as u8]);
                m.extend(n.as_bytes());
            }
            m.extend([0x10, 5, 0x20, 10, 0x69]);
            m.extend(7.5f64.to_le_bytes());
            let mut p = vec![0x12, m.len() as u8];
            p.extend(m);
            p
        };
        let mut memory = Memory::default();
        let birth = d.decode_with("spBv1.0/plant/DBIRTH/node/M1", &with_alias(Some("Rpm")), &mut memory);
        assert_eq!(birth.len(), 1);
        let data = d.decode_with("spBv1.0/plant/DDATA/node/M1", &with_alias(None), &mut memory);
        assert!(matches!(&data[0], Decoded::Metric { metric, value, .. } if metric == "Rpm" && *value == 7.5), "{data:?}");
        assert!(d.decode("spBv1.0/plant/DDATA/node/M1", &with_alias(None)).is_empty(), "without its birth it has no name, so it is not filed under one");
    }

    #[test]
    fn xml_is_read_as_a_flat_list() {
        let d = def(r#"
name: mtconnect
connect: http://agent:5000
poll:
  - {every: 1s, get: /current}
read:
  - {format: xml, each: $, when: {json: $.parent, is: Samples}, device: "{each.of.DeviceStream.name}", metric: "{each.attrs.dataItemId}", json: $.text}
  - {format: xml, each: $, when: {json: $.parent, is: Events}, device: "{each.of.DeviceStream.name}", attribute: "{each.attrs.dataItemId}", json: $.text}
  - {format: xml, each: $, when: {json: $.parent, is: Condition}, device: "{each.of.DeviceStream.name}", event: "{each.attrs.type}", json: $.tag}
"#);
        let body = br#"<?xml version="1.0"?><MTConnectStreams xmlns="urn:x"><Streams><DeviceStream name="M1"><ComponentStream component="Axes">
            <Samples><Position dataItemId="x.pos" timestamp="t">10.5</Position><Temperature dataItemId="t1">UNAVAILABLE</Temperature></Samples>
            <Events><Execution dataItemId="exec">ACTIVE &amp; ready</Execution></Events>
            <Condition><Fault type="TEMPERATURE"/><Normal type="SYSTEM"></Normal></Condition>
        </ComponentStream></DeviceStream></Streams></MTConnectStreams>"#;
        let got = d.decode("/current", body);
        assert!(matches!(&got[0], Decoded::Metric { device: Some(c), metric, value, .. } if c == "M1" && metric == "x.pos" && *value == 10.5), "{got:?}");
        assert_eq!(got.iter().filter(|g| matches!(g, Decoded::Metric { .. })).count(), 1, "UNAVAILABLE is not a number, so it is not one");
        assert!(got.contains(&Decoded::Attribute { device: Some("M1".into()), key: "exec".into(), value: json!("ACTIVE & ready") }), "{got:?}");
        assert!(got.contains(&Decoded::Event { device: Some("M1".into()), name: "TEMPERATURE".into(), value: json!("Fault") }), "{got:?}");
        assert!(got.contains(&Decoded::Event { device: Some("M1".into()), name: "SYSTEM".into(), value: json!("Normal") }), "{got:?}");
        assert!(d.decode("/current", b"<not<xml").is_empty());
    }

    #[test]
    fn a_check_is_added_going_out_and_held_to_coming_in() {
        // Known answers, from the standards' own examples.
        let modbus = Checksum { kind: ChecksumKind::Crc16Modbus, from: 0 };
        assert_eq!(modbus.seal(vec![0x01, 0x03, 0x00, 0x00, 0x00, 0x0a]), [0x01, 0x03, 0x00, 0x00, 0x00, 0x0a, 0xc5, 0xcd]);
        let ccitt = Checksum { kind: ChecksumKind::Crc16Ccitt, from: 0 };
        assert_eq!(ccitt.seal(b"123456789".to_vec())[9..], [0x29, 0xb1]);
        let crc32 = Checksum { kind: ChecksumKind::Crc32, from: 0 };
        assert_eq!(crc32.seal(b"123456789".to_vec())[9..], [0xcb, 0xf4, 0x39, 0x26]);
        let lrc = Checksum { kind: ChecksumKind::Lrc, from: 0 };
        assert_eq!(lrc.seal(vec![0x11, 0x03, 0x00, 0x6b, 0x00, 0x03]), [0x11, 0x03, 0x00, 0x6b, 0x00, 0x03, 0x7e]);
        assert_eq!(Checksum { kind: ChecksumKind::Sum8, from: 1 }.seal(vec![0xff, 2, 3]), [0xff, 2, 3, 5], "it covers what follows the header");
        for kind in [ChecksumKind::Crc16Modbus, ChecksumKind::Crc16Ccitt, ChecksumKind::Crc32, ChecksumKind::Sum8, ChecksumKind::Xor8, ChecksumKind::Lrc] {
            let c = Checksum { kind, from: 0 };
            let sealed = c.seal(vec![1, 2, 3]);
            assert_eq!(c.open(&sealed), Some(&[1u8, 2, 3][..]), "{kind:?}");
            let mut damaged = sealed.clone();
            damaged[1] ^= 0x10;
            assert_eq!(c.open(&damaged), None, "{kind:?}");
            assert_eq!(c.open(&[]), None, "{kind:?}: too short to have one");
        }
    }

    #[test]
    fn a_stream_finds_its_place_by_a_marker() {
        let length = Frame::Length(LengthField { start: Some("AA 55".into()), at: 2, size: 1, little: false, add: 3 });
        let mut b = vec![1, 2, 3, 0xaa, 0x55, 2, 7, 8, 0xaa];
        assert_eq!(length.take(&mut b), Some(vec![0xaa, 0x55, 2, 7, 8]));
        assert_eq!(length.take(&mut b), None);
        assert_eq!(b, vec![0xaa], "what may be the start of the next stays");
    }

    #[test]
    fn a_protocol_with_no_one_field_for_it_can_say_how_work_ended_several_ways() {
        let d = def(r#"
name: agv
connect: mqtt://b:1883
commands:
  transport.move:
    topic: agv/{device}/order
    send: {orderId: "{command}"}
    status:
      topic: agv/{device}/state
      id: $.orderId
      json: $.errors[0].errorLevel
      failed: FATAL
      also:
        - {topic: "agv/{device}/state", id: $.orderId, json: $.nodeStates, done: "[]"}
"#);
        let status = |body: &str| d.status("transport.move", "A1", "agv/A1/state", body.as_bytes(), &["c1"]);
        assert_eq!(status(r#"{"orderId":"c1","errors":[],"nodeStates":[{"id":"n"}]}"#), Some(Ending::Waiting));
        assert_eq!(status(r#"{"orderId":"c1","errors":[],"nodeStates":[]}"#), Some(Ending::Done));
        assert_eq!(status(r#"{"orderId":"c1","errors":[{"errorLevel":"FATAL"}],"nodeStates":[{"id":"n"}]}"#), Some(Ending::Failed));
        assert_eq!(status(r#"{"orderId":"other","errors":[],"nodeStates":[]}"#), None, "another order's news is not this one's");

        // After a restart the device is not known here, and the id is enough to go on.
        let body = br#"{"orderId":"c1","errors":[],"nodeStates":[]}"#;
        assert_eq!(d.status("transport.move", "", "agv/A1/state", body, &["c1"]), Some(Ending::Done));
        let unnamed =
            def("name: x\nconnect: mqtt://b:1\ncommands:\n  a.b:\n    topic: t\n    send: go\n    status: {topic: \"m/{device}/s\", json: $.s, done: IDLE}\n");
        assert_eq!(unnamed.status("a.b", "", "m/OTHER/s", br#"{"s":"IDLE"}"#, &["c1"]), None, "with no id it could be anyone's, so it proves nothing");
        assert!(
            parse(
                "name: x\nconnect: mqtt://b:1\ncommands:\n  a:\n    topic: t\n    send: x\n    status: {json: $.s, failed: F, also: [{json: $.t, failed: G}]}\n"
            )
            .is_err_and(|e| e.contains("done"))
        );
    }

    #[test]
    fn what_belongs_to_one_protocol_is_refused_on_another() {
        let on = |connect: &str, step: &str| parse(&format!("name: x\nconnect: {connect}\npoll:\n  - {{every: 1s, {step}}}\n"));
        assert!(on("tcp://h:1", "node: \"ns=2;i=1\"").is_err_and(|e| e.contains("opcua")));
        assert!(on("opcua://h:4840", "node: \"ns=2;i=1\"").is_ok());
        assert!(on("opcua://h:4840", "send: x").is_err_and(|e| e.contains("which node")));
        assert!(on("tcp://h:1", "send: x, unit: 3").is_err_and(|e| e.contains("Modbus")));
        assert!(on("modbus+rtu:///dev/ttyUSB0", "read: holding, unit: \"{settings.unit}\"").is_ok());
        assert!(on("tcp://h:1", "send: x, retain: true").is_err_and(|e| e.contains("MQTT")));
        assert!(parse("name: x\nconnect: opcua://h:1\ncommands:\n  a:\n    call: \"ns=2;i=2\"\n    once: true\n").is_err());
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

        let length = Frame::Length(LengthField { start: None, at: 1, size: 2, little: false, add: 3 });
        let mut b = vec![0xaa, 0, 2, 1, 2, 0xaa];
        assert_eq!(length.take(&mut b), Some(vec![0xaa, 0, 2, 1, 2]));
        assert_eq!(b, vec![0xaa]);

        // A length that cannot be: skip a byte and look again, rather than wait for ever.
        let mut b = vec![0xff, 0xff, 0xff, 0xff, 0xff, 0];
        let wide = Frame::Length(LengthField { start: None, at: 0, size: 4, little: false, add: 0 });
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
        assert_eq!(
            got,
            vec![Decoded::Metric {
                device: None,
                metric: "weight.kg".into(),
                value: 12.5,
                unit: Some("kg".into()),
                class: None,
                deadband: None,
                every: None,
                at: None
            }]
        );
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
        fails("name: x\nconnect: tcp://a:1\npoll:\n  - {every: 1s, read: holding, at: 1}", "are for Modbus");
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
