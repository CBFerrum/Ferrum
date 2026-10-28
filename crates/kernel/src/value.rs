//! Converting untrusted JSON input into canonical field values.
//!
//! Canonical forms: decimals as normalized strings (never floats), dates `YYYY-MM-DD`,
//! datetimes RFC 3339 UTC, links lower-case UUID strings, checks booleans, ints `i64`.

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use ferrum_meta::{FieldDef, FieldType};
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
use std::sync::LazyLock;
use uuid::Uuid;

const DATA_MAX: usize = 1_000;
const TEXT_MAX: usize = 1_000_000;
const JSON_MAX: usize = 1_000_000;

static EMAIL: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[^@\s]{1,64}@[^@\s]+\.[^@\s]+$").unwrap());
static PHONE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[+0-9 ()\-./]{3,40}$").unwrap());
static COLOR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$").unwrap());

fn decimal_bound() -> Decimal {
    Decimal::from_str("9999999999999999999").unwrap()
}

pub fn parse_decimal(v: &Value) -> Result<Decimal, String> {
    let d = match v {
        Value::Number(n) => {
            let s = n.to_string();
            Decimal::from_str(&s).or_else(|_| Decimal::from_scientific(&s)).map_err(|_| format!("{s} is not a valid number"))?
        }
        Value::String(s) => {
            let t = s.trim().replace(',', "");
            Decimal::from_str(&t).or_else(|_| Decimal::from_scientific(&t)).map_err(|_| format!("'{s}' is not a valid number"))?
        }
        _ => return Err("expected a number".into()),
    };
    if d.abs() > decimal_bound() {
        return Err("number is too large".into());
    }
    Ok(d.round_dp_with_strategy(9, rust_decimal::RoundingStrategy::MidpointAwayFromZero).normalize())
}

pub fn decimal_value(d: Decimal) -> Value {
    Value::String(d.normalize().to_string())
}

/// Reads a canonical decimal field from a document (missing/null → 0).
pub fn dec(doc: &serde_json::Map<String, Value>, key: &str) -> Decimal {
    doc.get(key).and_then(|v| parse_decimal(v).ok()).unwrap_or(Decimal::ZERO)
}

pub fn parse_date(s: &str) -> Result<NaiveDate, String> {
    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| format!("'{s}' is not a date (YYYY-MM-DD)"))?;
    if !(1900..=9999).contains(&chrono::Datelike::year(&d)) {
        return Err("date out of range".into());
    }
    Ok(d)
}

pub fn parse_datetime(s: &str) -> Result<DateTime<Utc>, String> {
    let t = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
        return Ok(dt.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, fmt) {
            return Ok(n.and_utc());
        }
    }
    Err(format!("'{s}' is not a date-time (RFC 3339)"))
}

pub fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// Normalizes one input value for `f`. `table` fields are handled by the caller.
pub fn normalize(f: &FieldDef, v: &Value) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    if let Value::String(s) = v
        && s.trim().is_empty()
        && !matches!(f.ftype, FieldType::Data | FieldType::Text | FieldType::Markdown | FieldType::Code)
    {
        return Ok(Value::Null);
    }
    let as_text = |v: &Value| -> Result<String, String> {
        match v {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err("expected text".into()),
        }
    };
    let out = match f.ftype {
        FieldType::Data | FieldType::Email | FieldType::Phone | FieldType::Url | FieldType::Attach | FieldType::Color | FieldType::Select => {
            // Surrounding whitespace is never meaningful in single-line values; trim before any check.
            let s = as_text(v)?.trim().to_string();
            let max = f.max_length.map(|m| m as usize).unwrap_or(DATA_MAX);
            if s.chars().count() > max {
                return Err(format!("longer than {max} characters"));
            }
            if s.chars().any(|c| c.is_control() && c != '\t') {
                return Err("contains control characters".into());
            }
            match f.ftype {
                FieldType::Email if !EMAIL.is_match(&s) => return Err("not a valid email address".into()),
                FieldType::Phone if !PHONE.is_match(&s) => return Err("not a valid phone number".into()),
                FieldType::Color if !COLOR.is_match(&s) => return Err("not a hex color".into()),
                FieldType::Url if !(s.starts_with("https://") || s.starts_with("http://")) => return Err("must start with http:// or https://".into()),
                FieldType::Attach if !(s.starts_with("/files/") || s.starts_with("https://") || s.starts_with("http://")) => {
                    return Err("must be a /files/ path or URL".into());
                }
                FieldType::Select if !f.options.iter().any(|o| o == &s) => return Err(format!("must be one of: {}", f.options.join(", "))),
                FieldType::Email => Value::String(s.to_lowercase()),
                _ => Value::String(s),
            }
        }
        FieldType::Text | FieldType::Markdown | FieldType::Code => {
            let s = as_text(v)?;
            let max = f.max_length.map(|m| m as usize).unwrap_or(TEXT_MAX);
            if s.chars().count() > max {
                return Err(format!("longer than {max} characters"));
            }
            Value::String(s)
        }
        FieldType::Int => {
            let i = match v {
                Value::Number(n) => n.as_i64().ok_or("expected a whole number")?,
                Value::String(s) => s.trim().parse::<i64>().map_err(|_| format!("'{s}' is not a whole number"))?,
                Value::Bool(b) => *b as i64,
                _ => return Err("expected a whole number".into()),
            };
            check_bounds(f, Decimal::from(i))?;
            Value::from(i)
        }
        FieldType::Decimal | FieldType::Currency | FieldType::Percent => {
            let d = parse_decimal(v)?;
            check_bounds(f, d)?;
            decimal_value(d)
        }
        FieldType::Date => {
            let s = as_text(v)?;
            Value::String(parse_date(&s)?.format("%Y-%m-%d").to_string())
        }
        FieldType::Datetime => {
            let s = as_text(v)?;
            Value::String(parse_datetime(&s)?.to_rfc3339_opts(SecondsFormat::Millis, true))
        }
        FieldType::Check => Value::Bool(match v {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_i64() == Some(1),
            Value::String(s) => matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"),
            _ => return Err("expected true/false".into()),
        }),
        FieldType::Link => {
            let s = as_text(v)?;
            Value::String(Uuid::parse_str(s.trim()).map_err(|_| format!("'{s}' is not a record id"))?.to_string())
        }
        FieldType::Json => {
            if serde_json::to_vec(v).map(|b| b.len()).unwrap_or(usize::MAX) > JSON_MAX {
                return Err("JSON value is too large".into());
            }
            v.clone()
        }
        FieldType::Table => return Err("table values must be lists of rows".into()),
    };
    Ok(out)
}

fn check_bounds(f: &FieldDef, d: Decimal) -> Result<(), String> {
    if let Some(min) = f.min
        && d < min
    {
        return Err(format!("must be ≥ {min}"));
    }
    if let Some(max) = f.max
        && d > max
    {
        return Err(format!("must be ≤ {max}"));
    }
    Ok(())
}

/// Canonical JSON bytes (keys sorted at every level) for hashing.
pub fn canonical_json(v: &Value) -> Vec<u8> {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let sorted: std::collections::BTreeMap<&String, Value> = m.iter().map(|(k, v)| (k, sort(v))).collect();
                let mut out = serde_json::Map::new();
                for (k, v) in sorted {
                    out.insert(k.clone(), v);
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    let mut buf = Vec::new();
    write_canonical(&sort(v), &mut buf);
    buf
}

fn write_canonical(v: &Value, buf: &mut Vec<u8>) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            buf.push(b'{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    buf.push(b',');
                }
                buf.extend(serde_json::to_vec(k).unwrap_or_default());
                buf.push(b':');
                write_canonical(&m[*k], buf);
            }
            buf.push(b'}');
        }
        Value::Array(a) => {
            buf.push(b'[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    buf.push(b',');
                }
                write_canonical(x, buf);
            }
            buf.push(b']');
        }
        other => buf.extend(serde_json::to_vec(other).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(t: &str) -> FieldDef {
        serde_json::from_value(json!({"name": "f", "type": t, "options": ["A", "B"], "min": "0"})).unwrap()
    }

    #[test]
    fn normalizes_values() {
        assert_eq!(normalize(&field("currency"), &json!("1,234.50")).unwrap(), json!("1234.5"));
        assert_eq!(normalize(&field("currency"), &json!(0.30000000000000004)).unwrap(), json!("0.3"));
        assert!(normalize(&field("currency"), &json!(-1)).is_err());
        assert_eq!(normalize(&field("date"), &json!("2026-02-03")).unwrap(), json!("2026-02-03"));
        assert!(normalize(&field("date"), &json!("03/02/2026")).is_err());
        assert_eq!(normalize(&field("check"), &json!(1)).unwrap(), json!(true));
        assert!(normalize(&field("select"), &json!("C")).is_err());
        assert_eq!(normalize(&field("link"), &json!("")).unwrap(), Value::Null);
        assert!(normalize(&field("link"), &json!("abc")).is_err());
        assert_eq!(normalize(&field("email"), &json!(" A@B.co ")).unwrap(), json!("a@b.co"));
        assert!(normalize(&field("int"), &json!(1.5)).is_err());
        assert_eq!(normalize(&field("datetime"), &json!("2026-01-01T10:00:00+05:30")).unwrap(), json!("2026-01-01T04:30:00.000Z"));
    }

    #[test]
    fn canonical_json_is_order_independent() {
        let a = json!({"b": 1, "a": {"y": 2, "x": [1, {"d": 1, "c": 2}]}});
        let b: Value = serde_json::from_str(r#"{"a":{"x":[1,{"c":2,"d":1}],"y":2},"b":1}"#).unwrap();
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }
}
