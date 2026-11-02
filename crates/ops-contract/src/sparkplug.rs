//! Sparkplug B payloads, read and written by hand: the format is small and fixed, only its metrics
//! matter here, and a protobuf toolchain would be a lot to carry for this. Reading gives JSON, so a
//! definition takes values from it with the same paths as anything else.
//!
//! A metric may arrive with only an alias, which means something only since that node's last birth
//! certificate, so [`Aliases`] is kept per link and is why decoding has a memory at all.

use serde_json::{Value, json};
use std::collections::HashMap;

/// Datatypes from the specification, by the numbers on the wire.
const NAMES: [(&str, u64); 12] = [
    ("int8", 1),
    ("int16", 2),
    ("int32", 3),
    ("int64", 4),
    ("uint8", 5),
    ("uint16", 6),
    ("uint32", 7),
    ("uint64", 8),
    ("float", 9),
    ("double", 10),
    ("boolean", 11),
    ("string", 12),
];

enum Wire<'a> {
    Number(u64),
    Eight([u8; 8]),
    Four([u8; 4]),
    Bytes(&'a [u8]),
}

fn varint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The next field of a message, or `None` at its end or where it stops making sense.
fn field<'a>(b: &'a [u8], at: &mut usize) -> Option<(u64, Wire<'a>)> {
    let key = varint(b, at)?;
    let wire = match key & 7 {
        0 => Wire::Number(varint(b, at)?),
        1 => {
            let raw = b.get(*at..*at + 8)?;
            *at += 8;
            Wire::Eight(raw.try_into().ok()?)
        }
        2 => {
            let len = usize::try_from(varint(b, at)?).ok()?;
            let raw = b.get(*at..at.checked_add(len)?)?;
            *at += len;
            Wire::Bytes(raw)
        }
        5 => {
            let raw = b.get(*at..*at + 4)?;
            *at += 4;
            Wire::Four(raw.try_into().ok()?)
        }
        _ => return None,
    };
    Some((key >> 3, wire))
}

fn metric(b: &[u8]) -> Option<Value> {
    let (mut name, mut alias, mut time, mut datatype, mut null, mut value) = (None, None, None, 0, false, None);
    let mut at = 0;
    while at < b.len() {
        match field(b, &mut at)? {
            (1, Wire::Bytes(s)) => name = Some(String::from_utf8_lossy(s).to_string()),
            (2, Wire::Number(n)) => alias = Some(n),
            (3, Wire::Number(n)) => time = Some(n),
            (4, Wire::Number(n)) => datatype = n,
            (7, Wire::Number(n)) => null = n != 0,
            (10, Wire::Number(n)) => value = Some(Number::Int(n)),
            (11, Wire::Number(n)) => value = Some(Number::Long(n)),
            (12, Wire::Four(raw)) => value = Some(Number::Value(json!(f32::from_le_bytes(raw)))),
            (13, Wire::Eight(raw)) => value = Some(Number::Value(json!(f64::from_le_bytes(raw)))),
            (14, Wire::Number(n)) => value = Some(Number::Value(json!(n != 0))),
            (15, Wire::Bytes(s)) => value = Some(Number::Value(json!(String::from_utf8_lossy(s)))),
            (16, Wire::Bytes(s)) => value = Some(Number::Value(json!(hex::encode(s)))),
            _ => {}
        }
    }
    let value = if null { Value::Null } else { value.map_or(Value::Null, |n| n.of(datatype)) };
    Some(json!({"name": name, "alias": alias, "timestamp": time, "datatype": datatype, "value": value}))
}

enum Number {
    Int(u64),
    Long(u64),
    Value(Value),
}

impl Number {
    /// Signed values travel in unsigned fields, two's complement, and only the datatype says so.
    fn of(self, datatype: u64) -> Value {
        match (self, datatype) {
            (Number::Int(n), 1) => json!(n as u8 as i8),
            (Number::Int(n), 2) => json!(n as u16 as i16),
            (Number::Int(n), 3) => json!(n as u32 as i32),
            (Number::Long(n), 4) => json!(n as i64),
            (Number::Int(n) | Number::Long(n), _) => json!(n),
            (Number::Value(v), _) => v,
        }
    }
}

/// A payload as JSON: `{timestamp, seq, metrics: [{name, alias, timestamp, datatype, value}]}`.
/// Datasets and templates are left out, with a null value: nothing here can record them.
pub fn decode(b: &[u8]) -> Option<Value> {
    let (mut time, mut seq, mut metrics) = (None, None, Vec::new());
    let mut at = 0;
    while at < b.len() {
        match field(b, &mut at)? {
            (1, Wire::Number(n)) => time = Some(n),
            (2, Wire::Bytes(raw)) => metrics.push(metric(raw)?),
            (3, Wire::Number(n)) => seq = Some(n),
            _ => {}
        }
    }
    Some(json!({"timestamp": time, "seq": seq, "metrics": metrics}))
}

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn put(out: &mut Vec<u8>, number: u64, wire: u64) {
    put_varint(out, number << 3 | wire);
}

fn put_bytes(out: &mut Vec<u8>, number: u64, bytes: &[u8]) {
    put(out, number, 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A command's payload: `{timestamp, metrics: [{name, value, type}]}`. `type` is `int32`, `double`
/// and so on, and defaults to what the value looks like; a device checks it against its birth.
pub fn encode(payload: &Value) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    if let Some(time) = payload["timestamp"].as_u64() {
        put(&mut out, 1, 0);
        put_varint(&mut out, time);
    }
    for m in payload["metrics"].as_array().ok_or("a Sparkplug message needs metrics")? {
        let name = m["name"].as_str().ok_or("a Sparkplug metric needs a name")?;
        let value = &m["value"];
        let datatype = match m["type"].as_str() {
            Some(t) => NAMES.iter().find(|(n, _)| *n == t).map(|(_, d)| *d).ok_or_else(|| format!("{t} is not a Sparkplug type"))?,
            None if value.is_boolean() => 11,
            None if value.is_string() => 12,
            None if value.is_i64() || value.is_u64() => 4,
            None => 10,
        };
        let mut one = Vec::new();
        put_bytes(&mut one, 1, name.as_bytes());
        put(&mut one, 4, 0);
        put_varint(&mut one, datatype);
        let whole = || value.as_i64().map(|n| n as u64).or(value.as_u64()).or_else(|| value.as_str()?.parse().ok());
        let wrong = || format!("{value} is not a {name} value");
        match datatype {
            1..=3 | 5..=7 => {
                put(&mut one, 10, 0);
                put_varint(&mut one, whole().ok_or_else(wrong)? & 0xffff_ffff);
            }
            4 | 8 => {
                put(&mut one, 11, 0);
                put_varint(&mut one, whole().ok_or_else(wrong)?);
            }
            9 => {
                put(&mut one, 12, 5);
                one.extend_from_slice(&(value.as_f64().ok_or_else(wrong)? as f32).to_le_bytes());
            }
            10 => {
                put(&mut one, 13, 1);
                one.extend_from_slice(&value.as_f64().ok_or_else(wrong)?.to_le_bytes());
            }
            11 => {
                put(&mut one, 14, 0);
                put_varint(&mut one, u64::from(value.as_bool().ok_or_else(wrong)?));
            }
            _ => put_bytes(&mut one, 15, value.as_str().ok_or_else(wrong)?.as_bytes()),
        }
        put_bytes(&mut out, 2, &one);
    }
    Ok(out)
}

/// What each node's aliases stood for at its last birth.
#[derive(Debug, Default)]
pub struct Aliases {
    nodes: HashMap<String, HashMap<u64, String>>,
}

impl Aliases {
    /// Gives each metric the name its alias stands for. A birth teaches them, and a metric still
    /// without a name stays that way, so a rule cannot file it under the wrong one.
    pub fn resolve(&mut self, topic: &str, payload: &mut Value) {
        // spBv1.0/group/MESSAGE/node[/device]. Aliases are unique across a node and its devices.
        let levels: Vec<&str> = topic.split('/').collect();
        let (Some(group), Some(kind), Some(node)) = (levels.get(1), levels.get(2), levels.get(3)) else { return };
        let names = self.nodes.entry(format!("{group}/{node}")).or_default();
        if *kind == "NBIRTH" {
            names.clear();
        }
        let birth = kind.ends_with("BIRTH");
        for m in payload["metrics"].as_array_mut().into_iter().flatten() {
            let Some(metric) = m.as_object_mut() else { continue };
            let Some(alias) = metric.get("alias").and_then(Value::as_u64) else { continue };
            match metric.get("name").and_then(Value::as_str) {
                Some(name) if birth => {
                    names.insert(alias, name.to_string());
                }
                Some(_) => {}
                None => {
                    if let Some(name) = names.get(&alias) {
                        metric.insert("name".into(), json!(name));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(v: &Value) -> &Vec<Value> {
        v["metrics"].as_array().unwrap()
    }

    /// Bytes as another implementation writes them, worked out by hand from the specification's
    /// proto file: timestamp 1, one metric named "t", alias 7, Int32, value -2.
    #[test]
    fn a_payload_is_read_from_the_wire_format() {
        let v = decode(&hex::decode("0801120d0a01741007200350feffffff0f").unwrap()).unwrap();
        assert_eq!(v["timestamp"], 1);
        let m = &metrics(&v)[0];
        assert_eq!((&m["name"], &m["alias"], &m["datatype"], &m["value"]), (&json!("t"), &json!(7), &json!(3), &json!(-2)));
    }

    #[test]
    fn what_is_written_reads_back_with_its_sign_and_type() {
        let sent = json!({"timestamp": 9, "metrics": [
            {"name": "a", "value": -2, "type": "int32"},
            {"name": "b", "value": -3, "type": "int8"},
            {"name": "c", "value": -4},
            {"name": "d", "value": 1.5},
            {"name": "e", "value": 2.5, "type": "float"},
            {"name": "f", "value": true},
            {"name": "g", "value": "go"},
        ]});
        let v = decode(&encode(&sent).unwrap()).unwrap();
        let got: Vec<&Value> = metrics(&v).iter().map(|m| &m["value"]).collect();
        assert_eq!(got, [&json!(-2), &json!(-3), &json!(-4), &json!(1.5), &json!(2.5), &json!(true), &json!("go")]);
        assert!(encode(&json!({"metrics": [{"name": "x", "value": "no", "type": "int32"}]})).is_err());
        assert!(encode(&json!({"metrics": [{"value": 1}]})).is_err());
        assert!(encode(&json!({"metrics": [{"name": "x", "value": 1, "type": "wide"}]})).is_err());
    }

    #[test]
    fn nonsense_is_refused_and_does_not_panic() {
        assert_eq!(decode(&[0x0a, 0xff]), None, "a length past the end");
        assert_eq!(decode(&[0xff; 12]), None, "a number that never ends");
        assert_eq!(decode(&[0x12, 0x02, 0x0f, 0x0f]), None, "a metric that makes no sense");
        for n in 0..=255u8 {
            let _ = decode(&[n, n, n, n, n]);
        }
    }

    #[test]
    fn an_alias_means_what_the_last_birth_said() {
        let mut aliases = Aliases::default();
        let mut birth = json!({"metrics": [{"name": "Temp", "alias": 1}, {"name": "Rpm", "alias": 2}]});
        aliases.resolve("spBv1.0/g/DBIRTH/n/d", &mut birth);
        let mut data = json!({"metrics": [{"alias": 1, "name": null}, {"alias": 9, "name": null}]});
        aliases.resolve("spBv1.0/g/DDATA/n/d", &mut data);
        assert_eq!(data["metrics"][0]["name"], "Temp");
        assert_eq!(data["metrics"][1]["name"], Value::Null, "nothing said what 9 is, so it has no name");

        // A node that restarted may have numbered them differently.
        let mut rebirth = json!({"metrics": [{"name": "Other", "alias": 3}]});
        aliases.resolve("spBv1.0/g/NBIRTH/n", &mut rebirth);
        let mut data = json!({"metrics": [{"alias": 1, "name": null}]});
        aliases.resolve("spBv1.0/g/DDATA/n/d", &mut data);
        assert_eq!(data["metrics"][0]["name"], Value::Null);

        let mut elsewhere = json!({"metrics": [{"alias": 3, "name": null}]});
        aliases.resolve("spBv1.0/g/DDATA/other/d", &mut elsewhere);
        assert_eq!(elsewhere["metrics"][0]["name"], Value::Null, "another node's numbers are its own");
    }
}
