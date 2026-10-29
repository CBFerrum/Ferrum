//! Extension-point contracts: the subset of JSON Schema an app uses to say what contributions to
//! its extension points look like (docs/plugins.md, "Extension points"), and the check of one
//! contribution against it. Keywords outside the subset are refused, not ignored, so a contract
//! never seems to promise a check it does not make.
//
// ponytail: a JSON Schema subset (type, description, properties, required, additionalProperties:
// false, items, enum, maxLength, maxItems); move to the jsonschema crate if authors need more.

use serde_json::Value;

const KEYWORDS: &[&str] = &["type", "description", "properties", "required", "additionalProperties", "items", "enum", "maxLength", "maxItems"];
const TYPES: &[&str] = &["object", "array", "string", "number", "integer", "boolean"];
const MAX_DEPTH: usize = 8;

fn at(path: &str) -> &str {
    if path.is_empty() { "(top)" } else { path.trim_start_matches('.') }
}

/// Checks that `schema` uses the supported keywords, each correctly.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    check(schema, "", 0)
}

fn check(schema: &Value, path: &str, depth: usize) -> Result<(), String> {
    let Some(obj) = schema.as_object() else { return Err(format!("{}: a schema is a mapping", at(path))) };
    if depth > MAX_DEPTH {
        return Err(format!("{}: nested deeper than {MAX_DEPTH} levels", at(path)));
    }
    for (key, v) in obj {
        let ok = match key.as_str() {
            "type" => v.as_str().is_some_and(|t| TYPES.contains(&t)),
            "description" => v.is_string(),
            "properties" => match v.as_object() {
                Some(props) => {
                    for (name, sub) in props {
                        check(sub, &format!("{path}.{name}"), depth + 1)?;
                    }
                    true
                }
                None => false,
            },
            "required" => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            "additionalProperties" => v == &Value::Bool(false),
            "items" => {
                check(v, &format!("{path}[]"), depth + 1)?;
                true
            }
            "enum" => v.as_array().is_some_and(|a| !a.is_empty()),
            "maxLength" | "maxItems" => v.as_u64().is_some(),
            other => return Err(format!("{}: '{other}' is not supported (use {})", at(path), KEYWORDS.join(", "))),
        };
        if !ok {
            let want = match key.as_str() {
                "type" => format!("one of {}", TYPES.join(", ")),
                "properties" => "a mapping of field → schema".into(),
                "required" => "a list of field names".into(),
                "additionalProperties" => "false (or left out)".into(),
                "enum" => "a non-empty list".into(),
                "maxLength" | "maxItems" => "a whole number".into(),
                _ => "text".into(),
            };
            return Err(format!("{}: {key} must be {want}", at(path)));
        }
    }
    Ok(())
}

/// Checks `value` against a schema that passed [`check_schema`].
pub fn conforms(value: &Value, schema: &Value) -> Result<(), String> {
    conform(value, schema, "")
}

fn conform(v: &Value, schema: &Value, path: &str) -> Result<(), String> {
    let Some(s) = schema.as_object() else { return Ok(()) };
    if let Some(t) = s.get("type").and_then(Value::as_str) {
        let ok = match t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "number" => v.is_number(),
            "integer" => v.is_i64() || v.is_u64(),
            _ => v.is_boolean(),
        };
        if !ok {
            return Err(format!("{} must be {}", at(path), if t == "array" || t == "integer" || t == "object" { format!("an {t}") } else { format!("a {t}") }));
        }
    }
    if let Some(options) = s.get("enum").and_then(Value::as_array)
        && !options.contains(v)
    {
        let list: Vec<String> = options.iter().map(Value::to_string).collect();
        return Err(format!("{} must be one of {}", at(path), list.join(", ")));
    }
    if let (Some(max), Some(text)) = (s.get("maxLength").and_then(Value::as_u64), v.as_str())
        && text.chars().count() as u64 > max
    {
        return Err(format!("{} is longer than {max} characters", at(path)));
    }
    if let (Some(max), Some(list)) = (s.get("maxItems").and_then(Value::as_u64), v.as_array())
        && list.len() as u64 > max
    {
        return Err(format!("{} has more than {max} items", at(path)));
    }
    if let Some(obj) = v.as_object() {
        for name in s.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !obj.contains_key(name) {
                return Err(format!("{} needs {name}", at(path)));
            }
        }
        let props = s.get("properties").and_then(Value::as_object);
        let closed = s.get("additionalProperties") == Some(&Value::Bool(false));
        for (key, val) in obj {
            match props.and_then(|p| p.get(key)) {
                Some(sub) => conform(val, sub, &format!("{path}.{key}"))?,
                None if closed => return Err(format!("{}: unknown field {key}", at(path))),
                None => {}
            }
        }
    }
    if let (Some(items), Some(list)) = (s.get("items"), v.as_array()) {
        for (i, item) in list.iter().enumerate() {
            conform(item, items, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn editor() -> Value {
        json!({
            "type": "object",
            "required": ["kind", "types"],
            "additionalProperties": false,
            "properties": {
                "kind": {"enum": ["wopi", "bridge"]},
                "types": {"type": "array", "items": {"type": "string", "maxLength": 10}, "maxItems": 3},
                "discovery": {"type": "string"},
                "weight": {"type": "integer"}
            }
        })
    }

    #[test]
    fn schemas_use_the_subset_only() {
        check_schema(&editor()).unwrap();
        let e = check_schema(&json!({"type": "object", "properties": {"a": {"pattern": "x"}}})).unwrap_err();
        assert!(e.contains("a: 'pattern' is not supported"), "{e}");
        assert!(check_schema(&json!({"type": "date"})).unwrap_err().contains("type must be one of"));
        assert!(check_schema(&json!({"additionalProperties": true})).unwrap_err().contains("false"));
        assert!(check_schema(&json!("string")).is_err());
        let mut deep = json!({"type": "string"});
        for _ in 0..10 {
            deep = json!({"type": "array", "items": deep});
        }
        assert!(check_schema(&deep).unwrap_err().contains("deeper"));
    }

    #[test]
    fn contributions_are_checked() {
        let s = editor();
        conforms(&json!({"kind": "wopi", "types": ["docx", "xlsx"], "discovery": "https://x"}), &s).unwrap();
        let cases = [
            (json!({"types": []}), "needs kind"),
            (json!({"kind": "other", "types": []}), "kind must be one of \"wopi\", \"bridge\""),
            (json!({"kind": "wopi", "types": "docx"}), "types must be an array"),
            (json!({"kind": "wopi", "types": ["a-very-long-type"]}), "types[0] is longer than 10"),
            (json!({"kind": "wopi", "types": ["a", "b", "c", "d"]}), "more than 3 items"),
            (json!({"kind": "wopi", "types": [], "weight": 1.5}), "weight must be an integer"),
            (json!({"kind": "wopi", "types": [], "colour": "red"}), "unknown field colour"),
            (json!(["wopi"]), "(top) must be an object"),
        ];
        for (value, want) in cases {
            let e = conforms(&value, &s).unwrap_err();
            assert!(e.contains(want), "{value}: {e}");
        }
    }
}
