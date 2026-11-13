//! What a description's templates and step expressions compute (BD-56): digests and signatures, encodings, a sortable
//! record key made from the call's key and clock, lengths in the unit a service counts, the links, mentions and tags
//! in a text with their offsets, and formats of the clock. Each is a minijinja filter; none names a service.

use base64::Engine as _;
use minijinja::{Environment, Error as JError, ErrorKind, State, Value as JValue};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use unicode_segmentation::UnicodeSegmentation;

fn bad(m: impl Into<String>) -> JError {
    JError::new(ErrorKind::InvalidOperation, m.into())
}

/// The base32 alphabet that sorts as its values do.
const SORTABLE: &[u8] = b"234567abcdefghijklmnopqrstuvwxyz";

/// A 13-character id that sorts by time: the moment in microseconds (53 bits) and 10 bits of `seed`.
pub(crate) fn sortable_id(micros: u64, seed: &str) -> String {
    let d = Sha256::digest(seed.as_bytes());
    let clock = u64::from(u16::from_be_bytes([d[0], d[1]])) & 0x3ff;
    let v = ((micros & ((1 << 53) - 1)) << 10) | clock;
    (0..13).rev().map(|i| SORTABLE[((v >> (i * 5)) & 31) as usize] as char).collect()
}

/// Where `byte` (an offset into `text`) falls, counted in `unit`.
fn offset(text: &str, byte: usize, unit: &str) -> Result<usize, JError> {
    let prefix = &text[..byte];
    Ok(match unit {
        "bytes" => byte,
        "chars" => prefix.chars().count(),
        "utf16" => prefix.encode_utf16().count(),
        "graphemes" => prefix.graphemes(true).count(),
        u => return Err(bad(format!("{u} is not a unit: bytes, chars, utf16 or graphemes"))),
    })
}

static LINK: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"https?://[^\s<>]+").expect("link"));
static MENTION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?:^|[\s(\[])(@[A-Za-z0-9_][A-Za-z0-9_.\-]*(?:@[A-Za-z0-9][A-Za-z0-9.\-]*)?)").expect("mention"));
static TAG: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?:^|[\s(\[])(#[\p{L}\p{N}_]*\p{L}[\p{L}\p{N}_]*)").expect("tag"));

/// The links, mentions and tags in `text`, in order: `{type, text, value, start, end}`, offsets in `unit`.
pub(crate) fn entities(text: &str, unit: &str) -> Result<Vec<serde_json::Value>, JError> {
    let trim = |s: &str| s.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']).len();
    let mut found: Vec<(usize, usize, &str)> = Vec::new();
    for m in LINK.find_iter(text) {
        found.push((m.start(), m.start() + trim(m.as_str()), "link"));
    }
    for (re, kind) in [(&*MENTION, "mention"), (&*TAG, "tag")] {
        for c in re.captures_iter(text) {
            let m = c.get(1).expect("group");
            let end = m.start() + trim(m.as_str());
            // Not inside a link.
            if !found.iter().any(|(s, e, _)| m.start() >= *s && m.start() < *e) && end > m.start() + 1 {
                found.push((m.start(), end, kind));
            }
        }
    }
    found.sort();
    found
        .into_iter()
        .map(|(s, e, kind)| {
            let t = &text[s..e];
            let value = if kind == "link" { t } else { &t[1..] };
            Ok(serde_json::json!({"type": kind, "text": t, "value": value, "start": offset(text, s, unit)?, "end": offset(text, e, unit)?}))
        })
        .collect()
}

/// The moment a template's `now` names.
fn moment(v: &str) -> Result<chrono::DateTime<chrono::Utc>, JError> {
    chrono::DateTime::parse_from_rfc3339(v.trim()).map(|t| t.to_utc()).map_err(|_| bad(format!("{v} is not a moment (RFC 3339)")))
}

/// The deepest an XML answer is read.
const XML_DEPTH: usize = 64;

/// An XML answer in JSON's shape (BD-59): `{<root>: <element>}`; an element is its text when it has neither attributes
/// nor children, else an object of `@attribute`s, children by name (prefixed as written: `dc:creator`) and its text as
/// `#text`. A child that comes more than once, or whose name is in `lists`, is a list. No document type (so no
/// entities of its own) is read.
pub(crate) fn xml_value(raw: &[u8], lists: &[String]) -> Result<serde_json::Value, String> {
    fn name(n: roxmltree::Node, local: &str, ns: Option<&str>) -> String {
        match ns.and_then(|ns| n.lookup_prefix(ns)).filter(|p| !p.is_empty()) {
            Some(p) => format!("{p}:{local}"),
            None => local.to_string(),
        }
    }
    fn element(n: roxmltree::Node, lists: &[String], depth: usize) -> Result<serde_json::Value, String> {
        use serde_json::Value;
        if depth > XML_DEPTH {
            return Err(format!("it is nested deeper than {XML_DEPTH}"));
        }
        let mut obj = serde_json::Map::new();
        for a in n.attributes() {
            obj.insert(format!("@{}", name(n, a.name(), a.namespace())), Value::String(a.value().to_string()));
        }
        let mut text = String::new();
        for c in n.children() {
            if c.is_element() {
                let k = name(c, c.tag_name().name(), c.tag_name().namespace());
                let v = element(c, lists, depth + 1)?;
                match obj.get_mut(&k) {
                    Some(Value::Array(a)) => a.push(v),
                    Some(one) => *one = Value::Array(vec![one.take(), v]),
                    None if lists.contains(&k) => drop(obj.insert(k, Value::Array(vec![v]))),
                    None => drop(obj.insert(k, v)),
                }
            } else if c.is_text() {
                text.push_str(c.text().unwrap_or_default());
            }
        }
        let text = text.trim();
        if obj.is_empty() {
            return Ok(Value::String(text.to_string()));
        }
        if !text.is_empty() {
            obj.insert("#text".into(), Value::String(text.to_string()));
        }
        Ok(Value::Object(obj))
    }
    let text = std::str::from_utf8(raw).map_err(|_| "it is not UTF-8".to_string())?;
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let mut out = serde_json::Map::new();
    out.insert(name(root, root.tag_name().name(), root.tag_name().namespace()), element(root, lists, 0)?);
    Ok(serde_json::Value::Object(out))
}

/// A form-encoded answer (`a=1&b=2`) as an object of texts (a name given twice: the last).
pub(crate) fn form_value(raw: &[u8]) -> serde_json::Value {
    let mut url = reqwest::Url::parse("http://form/").expect("an address");
    url.set_query(Some(String::from_utf8_lossy(raw).trim()));
    serde_json::Value::Object(url.query_pairs().map(|(k, v)| (k.into_owned(), serde_json::Value::String(v.into_owned()))).collect())
}

/// Adds the functions to a template environment.
pub(crate) fn add(env: &mut Environment<'static>) {
    env.add_filter("sha256", |v: String| hex::encode(Sha256::digest(v.as_bytes())));
    env.add_filter("hmac", |v: String, key: String, encoding: Option<String>| -> Result<String, JError> {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key.as_bytes()).map_err(|_| bad("an HMAC key"))?;
        mac.update(v.as_bytes());
        let sig = mac.finalize().into_bytes();
        Ok(match encoding.as_deref().unwrap_or("hex") {
            "hex" => hex::encode(sig),
            "base64" => base64::engine::general_purpose::STANDARD.encode(sig),
            "base64url" => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig),
            e => return Err(bad(format!("{e} is not an encoding: hex, base64 or base64url"))),
        })
    });
    env.add_filter("base64", |v: String| base64::engine::general_purpose::STANDARD.encode(v.as_bytes()));
    env.add_filter("base64url", |v: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.as_bytes()));
    env.add_filter("base32", |v: String| data_encoding::BASE32_NOPAD.encode(v.as_bytes()));
    env.add_filter("urlencode", |v: JValue| super::pct(&if v.is_none() || v.is_undefined() { String::new() } else { v.to_string() }));
    // A sortable id from the call's key and its clock (fixed at the key's first attempt): the same on every retry.
    env.add_filter("tid", |state: &State, key: String| -> Result<String, JError> {
        let now = state.lookup("now").map(|n| n.to_string()).ok_or_else(|| bad("tid needs the clock"))?;
        Ok(sortable_id(moment(&now)?.timestamp_micros().max(0) as u64, &key))
    });
    env.add_filter("length_in", |v: String, unit: Option<String>| -> Result<usize, JError> { offset(&v, v.len(), unit.as_deref().unwrap_or("chars")) });
    env.add_filter("entities", |v: String, unit: Option<String>| -> Result<JValue, JError> {
        Ok(JValue::from_serialize(entities(&v, unit.as_deref().unwrap_or("chars"))?))
    });
    env.add_filter("truncate", |v: String, length: usize, end: Option<String>| -> String {
        let g: Vec<&str> = v.graphemes(true).collect();
        if g.len() <= length {
            return v;
        }
        let end = end.unwrap_or_default();
        let keep = length.saturating_sub(end.graphemes(true).count());
        format!("{}{end}", g[..keep].concat())
    });
    env.add_filter("strftime", |v: String, format: String| -> Result<String, JError> {
        use std::fmt::Write as _;
        let mut out = String::new();
        write!(out, "{}", moment(&v)?.format(&format)).map_err(|_| bad(format!("{format} is not a date format")))?;
        Ok(out)
    });
    // Date arithmetic on a moment (the fixed `now` too, BD-70): `now | add_time('-7d')`, `now | start_of('day')`.
    env.add_filter("add_time", |v: String, by: String| -> Result<String, JError> { Ok(shifted(moment(&v)?, &by)?.to_rfc3339()) });
    env.add_filter("start_of", |v: String, unit: String| -> Result<String, JError> { Ok(bounded(moment(&v)?, &unit, false)?.to_rfc3339()) });
    env.add_filter("end_of", |v: String, unit: String| -> Result<String, JError> { Ok(bounded(moment(&v)?, &unit, true)?.to_rfc3339()) });
}

/// `t` moved by `by`: a signed whole number and a unit, `s`, `m`, `h`, `d` or `w` (`-7d`, `90m`, `+1h`).
fn shifted(t: chrono::DateTime<chrono::Utc>, by: &str) -> Result<chrono::DateTime<chrono::Utc>, JError> {
    let by = by.trim();
    let wrong = || bad(format!("{by} is not a duration: a whole number and s, m, h, d or w (-7d, 90m)"));
    let (n, unit) = by.split_at(by.len().checked_sub(1).ok_or_else(wrong)?);
    let n: i64 = n.trim_start_matches('+').parse().map_err(|_| wrong())?;
    let d = match unit {
        "s" => chrono::Duration::try_seconds(n),
        "m" => chrono::Duration::try_minutes(n),
        "h" => chrono::Duration::try_hours(n),
        "d" => chrono::Duration::try_days(n),
        "w" => chrono::Duration::try_weeks(n),
        _ => None,
    }
    .ok_or_else(wrong)?;
    t.checked_add_signed(d).ok_or_else(wrong)
}

/// The start (or the last second) of `t`'s hour, day, week (from Monday) or month, in UTC.
fn bounded(t: chrono::DateTime<chrono::Utc>, unit: &str, end: bool) -> Result<chrono::DateTime<chrono::Utc>, JError> {
    use chrono::{Datelike, Duration, NaiveTime, TimeZone, Timelike};
    let day = |d: chrono::NaiveDate| chrono::Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN));
    let (start, next) = match unit {
        "hour" => {
            let s = t.with_minute(0).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).unwrap_or(t);
            (s, s + Duration::hours(1))
        }
        "day" => (day(t.date_naive()), day(t.date_naive()) + Duration::days(1)),
        "week" => {
            let s = day(t.date_naive()) - Duration::days(i64::from(t.weekday().num_days_from_monday()));
            (s, s + Duration::weeks(1))
        }
        "month" => {
            let first = t.date_naive().with_day(1).unwrap_or(t.date_naive());
            let next = if first.month() == 12 { first.with_year(first.year() + 1).and_then(|d| d.with_month(1)) } else { first.with_month(first.month() + 1) };
            (day(first), day(next.unwrap_or(first)))
        }
        u => return Err(bad(format!("{u} is not hour, day, week or month"))),
    };
    Ok(if end { next - Duration::seconds(1) } else { start })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(t: &str, vars: serde_json::Value) -> String {
        super::super::render(t, &vars).unwrap()
    }

    #[test]
    fn date_arithmetic_on_the_clock() {
        let v = serde_json::json!({"now": "2026-11-13T17:41:34+00:00"});
        assert_eq!(render("{{ now | add_time('-7d') }}", v.clone()), "2026-11-06T17:41:34+00:00");
        assert_eq!(render("{{ now | add_time('+90m') }}", v.clone()), "2026-11-13T19:11:34+00:00");
        assert_eq!(render("{{ now | start_of('day') }} {{ now | end_of('day') }}", v.clone()), "2026-11-13T00:00:00+00:00 2026-11-13T23:59:59+00:00");
        assert_eq!(render("{{ now | start_of('week') }} {{ now | end_of('month') }}", v.clone()), "2026-11-09T00:00:00+00:00 2026-11-30T23:59:59+00:00");
        assert_eq!(render("{{ now | add_time('-1d') | start_of('day') | strftime('%s') }}", v.clone()), "1794441600");
        assert!(super::super::render("{{ now | add_time('7 days') }}", &v).is_err());
        assert!(super::super::render("{{ now | start_of('year') }}", &v).is_err());
    }

    #[test]
    fn functions_compute() {
        assert_eq!(render("{{ 'abc' | sha256 }}", serde_json::json!({})), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // RFC 4231 test case 2.
        assert_eq!(
            render("{{ 'what do ya want for nothing?' | hmac('Jefe') }}", serde_json::json!({})),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(render("{{ 'hi?' | base64 }} {{ 'hi?' | base64url }} {{ 'hi' | base32 }}", serde_json::json!({})), "aGk/ aGk_ NBUQ");
        assert_eq!(render("{{ 'a b/é' | urlencode }}", serde_json::json!({})), "a%20b%2F%C3%A9");
        let t = "👨‍👩‍👧 ok";
        let v = serde_json::json!({"t": t});
        assert_eq!(
            render("{{ t | length_in('graphemes') }} {{ t | length_in('chars') }} {{ t | length_in('utf16') }} {{ t | length_in('bytes') }}", v),
            "4 8 11 21"
        );
        assert_eq!(render("{{ 'abcdef' | truncate(4, '…') }}|{{ 'ab' | truncate(4) }}", serde_json::json!({})), "abc…|ab");
        assert_eq!(render("{{ now | strftime('%d.%m.%Y') }}", serde_json::json!({"now": "2026-11-13T10:00:00Z"})), "13.11.2026");
        assert_eq!(render("{{ 'a,b' | split(',') | join('+') }}{{ x | default('-') }}", serde_json::json!({})), "a+b-");
    }

    #[test]
    fn ids_sort_and_repeat() {
        let a = sortable_id(1_700_000_000_000_000, "k1");
        assert_eq!(a, sortable_id(1_700_000_000_000_000, "k1"), "the same key and clock, the same id");
        assert!(sortable_id(1_700_000_000_000_001, "k0") > a, "later sorts after");
        assert_eq!(a.len(), 13);
        assert!(a.starts_with(|c: char| "234567abcdefghij".contains(c)), "the top bit is 0: {a}");
        assert_eq!(render("{{ key | tid }}", serde_json::json!({"key": "k1", "now": "2023-11-14T22:13:20Z"})), a);
    }

    #[test]
    fn xml_reads_as_json() {
        let v = xml_value(br#"<a x="1"><b>t</b><b>u</b><c>v<d/></c></a>"#, &[]).unwrap();
        assert_eq!(v, serde_json::json!({"a": {"@x": "1", "b": ["t", "u"], "c": {"d": "", "#text": "v"}}}));
        let deep = format!("{}{}", "<a>".repeat(70), "</a>".repeat(70));
        assert!(xml_value(deep.as_bytes(), &[]).unwrap_err().contains("nested deeper"));
        assert_eq!(form_value(b"a=1&b=x+y"), serde_json::json!({"a": "1", "b": "x y"}));
    }

    #[test]
    fn entities_have_offsets_in_each_unit() {
        let t = "é @bo.example.test, #tag1 and https://x.example/p?q=1). #2026 @x@m.example";
        let e = entities(t, "bytes").unwrap();
        let kinds: Vec<&str> = e.iter().map(|x| x["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["mention", "tag", "link", "mention"], "{e:?}");
        assert_eq!(&t[e[0]["start"].as_u64().unwrap() as usize..e[0]["end"].as_u64().unwrap() as usize], "@bo.example.test");
        assert_eq!(e[2]["value"], "https://x.example/p?q=1", "trailing punctuation is not the link's");
        assert_eq!(e[3]["value"], "x@m.example");
        assert_eq!(entities(t, "chars").unwrap()[0]["start"], 2);
        assert_eq!(entities(t, "bytes").unwrap()[0]["start"], 3);
        assert!(entities(t, "words").is_err());
    }
}
