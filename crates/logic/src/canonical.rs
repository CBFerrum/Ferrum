//! One way to write an intent down (docs/logic/decisions.md L-03; Ferrum docs/platform/decisions.md, D-05).
//!
//! Moved here from Ferrum Cloud (`ferrum-app-cloud`, `contract/src/canonical.rs` at c64e0bc) on 8 November 2026,
//! with the owner's agreement, so that approvals, releases and effect keys across Ferrum share one digest.
//!
//! An approval, an idempotency check and a release all compare digests, so two sides that mean the
//! same thing must produce the same bytes. This is RFC 8785 (JCS) over JSON, narrowed: integers within
//! the range every JSON reader agrees on, and no fractions at all, because formatting a float is where
//! languages differ. A quantity that needs a fraction is a string.
//!
//! Null and absent are different: an intent that says nothing about a field did not say it was empty.
//! Strings are compared exactly, with no Unicode normalization: `é` written two ways is two values.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fmt;

/// The version of these rules, part of every digest.
pub const VERSION: u32 = 1;
/// Beyond this a JavaScript number cannot hold every integer.
const MAX_INTEGER: i64 = (1 << 53) - 1;
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// A fraction, or an integer too large for every reader to agree on: write it as a string.
    Number(String),
    TooDeep,
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Number(n) => write!(f, "{n} cannot be part of an intent: write it as a string"),
            Refused::TooDeep => write!(f, "this intent is nested too deeply"),
        }
    }
}

impl std::error::Error for Refused {}

fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{09}' => out.push_str("\\t"),
            '\u{0a}' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\u{0d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Keys in UTF-16 code unit order, as RFC 8785 says, which is not the order of `str`.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut keys: Vec<(Vec<u16>, &String, &Value)> = map.iter().map(|(k, v)| (k.encode_utf16().collect(), k, v)).collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    keys.into_iter().map(|(_, k, v)| (k, v)).collect()
}

fn write(v: &Value, depth: usize, out: &mut String) -> Result<(), Refused> {
    if depth > MAX_DEPTH {
        return Err(Refused::TooDeep);
    }
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_i64() {
            Some(i) if i.abs() <= MAX_INTEGER => out.push_str(&i.to_string()),
            _ => return Err(Refused::Number(n.to_string())),
        },
        Value::String(s) => string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, depth + 1, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, value)) in sorted(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(k, out);
                out.push(':');
                write(value, depth + 1, out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// The canonical text of a value.
pub fn canonical(v: &Value) -> Result<String, Refused> {
    let mut out = String::new();
    write(v, 0, &mut out)?;
    Ok(out)
}

/// `sha256:…` of a value under a schema. The schema and these rules' version are part of what is
/// hashed, so the same body under another schema, or a later version of these rules, cannot match.
pub fn digest(schema: &str, body: &Value) -> Result<String, Refused> {
    let text = canonical(&json!({"canonical": VERSION, "schema": schema, "body": body}))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(text.as_bytes()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: Value) -> String {
        canonical(&v).unwrap()
    }

    #[test]
    fn the_order_a_client_wrote_does_not_matter() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"y":2,"x":[3,4]}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{ "a" : { "x" : [ 3, 4 ], "y" : 2 } , "b" : 1 }"#).unwrap();
        assert_eq!(text(a.clone()), text(b.clone()));
        assert_eq!(text(a), r#"{"a":{"x":[3,4],"y":2},"b":1}"#);
        assert_ne!(digest("s", &b).unwrap(), digest("s", &json!({"a": {"x": [4, 3], "y": 2}, "b": 1})).unwrap(), "a list's order is its meaning");
    }

    #[test]
    fn keys_sort_as_utf16_not_as_bytes() {
        // U+10000 is 0xD800 0xDC00 in UTF-16, so it comes before U+E000, though its UTF-8 sorts after.
        assert_eq!(text(json!({"\u{e000}": 1, "\u{10000}": 2})), "{\"\u{10000}\":2,\"\u{e000}\":1}");
    }

    #[test]
    fn strings_escape_as_rfc_8785_says() {
        assert_eq!(text(json!("a\"b\\c\n\t\u{1}\u{7f}é")), "\"a\\\"b\\\\c\\n\\t\\u0001\u{7f}é\"");
    }

    #[test]
    fn null_is_not_the_same_as_absent() {
        assert_ne!(digest("s", &json!({"a": null})).unwrap(), digest("s", &json!({})).unwrap());
    }

    #[test]
    fn unicode_is_compared_exactly() {
        assert_ne!(digest("s", &json!("é")).unwrap(), digest("s", &json!("e\u{301}")).unwrap(), "two spellings of one letter stay two values");
    }

    #[test]
    fn a_fraction_or_a_huge_number_is_refused_not_rounded() {
        assert!(matches!(canonical(&json!({"a": 1.5})), Err(Refused::Number(_))));
        assert!(matches!(canonical(&json!(1u64 << 53)), Err(Refused::Number(_))));
        assert!(canonical(&json!((1i64 << 53) - 1)).is_ok());
        assert!(canonical(&json!(-((1i64 << 53) - 1))).is_ok());
    }

    #[test]
    fn the_schema_is_part_of_the_digest() {
        assert_ne!(digest("release/1", &json!({})).unwrap(), digest("release/2", &json!({})).unwrap());
    }

    #[test]
    fn depth_is_bounded() {
        let mut v = json!(1);
        for _ in 0..100 {
            v = json!([v]);
        }
        assert_eq!(canonical(&v), Err(Refused::TooDeep));
    }

    /// Worked out by hand from the canonical text, in another language. The TypeScript SDK checks the
    /// same vector: change this and every stored digest changes.
    #[test]
    fn the_digest_is_stable() {
        let body = json!({"environment": "e1", "artifact": "sha256:ab", "capabilities": ["net:none"], "replicas": 2, "note": null});
        assert_eq!(digest("release-intent/1", &body).unwrap(), "sha256:8f4e4c3c115b5ee1e515e18af61f531c9603fb07c1b972effbd1c2f376da744e");
    }
}

// What cargo-mutants found no test of, each at its exact edge, with the numbers themselves.
#[cfg(test)]
mod at_the_edges {
    use super::*;

    use serde_json::json;

    fn nest(levels: usize, object: bool) -> Value {
        let mut v = Value::Null;
        for _ in 0..levels {
            v = if object { json!({"a": v}) } else { json!([v]) };
        }
        v
    }

    #[test]
    fn sixty_four_levels_and_no_deeper_in_arrays_and_objects_alike() {
        for object in [false, true] {
            assert!(canonical(&nest(64, object)).is_ok(), "object: {object}");
            assert!(matches!(canonical(&nest(65, object)), Err(Refused::TooDeep)), "object: {object}");
        }
        assert_eq!(Refused::TooDeep.to_string(), "this intent is nested too deeply");
    }

    #[test]
    fn a_space_is_a_space_and_below_it_is_escaped() {
        assert_eq!(canonical(&json!(" ")).unwrap(), "\" \"");
        assert_eq!(canonical(&json!("\u{1f}")).unwrap(), "\"\\u001f\"");
    }
}
