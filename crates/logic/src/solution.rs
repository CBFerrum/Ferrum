//! A solution's source, as a customer, the builder or an agent writes it (docs/logic/decisions.md L-04): its
//! rules on records, decision tables, processes and tests, with the capabilities it asks for. JSON is the
//! canonical form; layout is kept apart from meaning.
//!
//! Reading one is strict: duplicate keys (which two parsers read differently), unknown keys (a typo, or a
//! setting that would be silently ignored), oversized or deeply nested documents are refused before
//! anything else looks at it. There is no way to refer to a remote schema.

use crate::decision;
use crate::refusal::{Code, Refusal};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

pub const SCHEMA_VERSION: &str = "ferrum.solution/1";
pub const MAX_BYTES: usize = 1 << 20;
const MAX_DEPTH: usize = 64;

/// JSON text read as a value, refusing a key repeated in one object.
pub fn strict_json(text: &str) -> Result<Value, Refusal> {
    if text.len() > MAX_BYTES {
        return Err(Refusal::new(Code::SchemaInvalid, format!("the source is {} bytes; at most {MAX_BYTES}", text.len())));
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let v =
        de::DeserializeSeed::deserialize(Strict(0), &mut de).map_err(|e| Refusal::new(Code::SchemaInvalid, format!("the source is not valid JSON: {e}")))?;
    de.end().map_err(|e| Refusal::new(Code::SchemaInvalid, format!("the source has more after its end: {e}")))?;
    Ok(v)
}

/// A value at a depth, read strictly.
struct Strict(usize);

impl<'de> de::DeserializeSeed<'de> for Strict {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v).map(Value::Number).ok_or_else(|| E::custom("a number that is not finite"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if self.0 >= MAX_DEPTH {
            return Err(de::Error::custom(format!("nested deeper than {MAX_DEPTH} levels")));
        }
        let mut out = vec![];
        while let Some(v) = seq.next_element_seed(Strict(self.0 + 1))? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        if self.0 >= MAX_DEPTH {
            return Err(de::Error::custom(format!("nested deeper than {MAX_DEPTH} levels")));
        }
        let mut out = Map::new();
        while let Some(k) = map.next_key::<String>()? {
            if out.contains_key(&k) {
                return Err(de::Error::custom(format!("the key \"{k}\" appears twice in one object")));
            }
            let v = map.next_value_seed(Strict(self.0 + 1))?;
            out.insert(k, v);
        }
        Ok(Value::Object(out))
    }
}

/// A capability asked for, at an exact version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapRef {
    pub id: String,
    pub version: String,
}

/// A rule on a record, run in its transaction. `derive` fills the customer's own fields; `validate` refuses
/// a change with a message. Neither reaches outside the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub entity: String,
    pub phase: Phase,
    /// When the rule applies (yes/no over `doc`, `old` and `params`); always, when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// For `derive`: field → expression.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub writes: BTreeMap<String, String>,
    /// For `validate`: what must hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// For `validate`: what a person reads when it does not hold. Plain text: nothing from the record is
    /// put into it, so a refusal cannot show a field its reader may not see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// For `validate`: the field the message is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// For `validate`: keep evidence of each refusal, apart from the change it refused (FLC-HOOK-007).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audit_refusals: bool,
    /// Linked records the rule reads, as `lookups.{name}.{field}`: named, few and bounded, read with the
    /// rights of whoever saves before the rule runs (FLC-HOOK-004).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lookups: Vec<Lookup>,
}

/// A linked record a rule reads: the record its `link` field points to, and only these fields of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lookup {
    pub name: String,
    pub link: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Derive,
    Validate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// An event from the catalog (`procurement.material_request.submitted`).
    pub event: String,
    /// What makes two triggers one request: an expression over `trigger`.
    pub business_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub outcome: Value,
    pub target: String,
}

/// One step of a process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    Query {
        id: String,
        label: String,
        capability: CapRef,
        input: BTreeMap<String, String>,
        next: String,
        on_error: String,
    },
    Action {
        id: String,
        label: String,
        capability: CapRef,
        input: BTreeMap<String, String>,
        /// How the effect's key is made. Only `stable_activation`: the same for every attempt at one
        /// activation of this step, never an attempt's own.
        #[serde(default = "stable", skip_serializing_if = "is_stable")]
        effect_key: String,
        next: String,
        on_error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_unknown: Option<String>,
    },
    Decision {
        id: String,
        label: String,
        decision: String,
        routes: Vec<Route>,
        on_error: String,
    },
    HumanTask {
        id: String,
        label: String,
        /// Who may decide: a role.
        role: String,
        /// The requester may not decide their own request, whatever roles they hold.
        #[serde(default = "yes")]
        exclude_requester: bool,
        /// What the decision is bound to: if it changes, the decision lapses.
        material: String,
        /// What the person deciding fills in, which later steps read as `{task}.output.{field}`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        form: Vec<FormField>,
        /// Seconds, or an ISO 8601 duration (`P2D`, `PT4H`).
        timeout: Value,
        next: String,
        on_reject: String,
        on_timeout: String,
    },
    WaitEvent {
        id: String,
        label: String,
        event: String,
        /// Which event is this process's: an expression over the process's scope.
        correlation: String,
        timeout: Value,
        next: String,
        on_timeout: String,
    },
    End {
        id: String,
        label: String,
        outcome: EndOutcome,
    },
    Suspend {
        id: String,
        label: String,
        reason: String,
    },
}

/// A field of a person's task form. What a person is asked is part of the solution's meaning and digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormField {
    pub name: String,
    pub label: String,
    /// As a parameter's: `text`, `decimal`, `int`, `bool`, `date`, `money:INR`, `quantity:kg`.
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub required: bool,
    /// For a choice of text, the values allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

fn stable() -> String {
    "stable_activation".into()
}

fn is_stable(s: &String) -> bool {
    s == "stable_activation"
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndOutcome {
    Completed,
    Refused,
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Query { id, .. }
            | Node::Action { id, .. }
            | Node::Decision { id, .. }
            | Node::HumanTask { id, .. }
            | Node::WaitEvent { id, .. }
            | Node::End { id, .. }
            | Node::Suspend { id, .. } => id,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Node::Query { label, .. }
            | Node::Action { label, .. }
            | Node::Decision { label, .. }
            | Node::HumanTask { label, .. }
            | Node::WaitEvent { label, .. }
            | Node::End { label, .. }
            | Node::Suspend { label, .. } => label,
        }
    }

    /// Every route out, by name.
    pub fn routes(&self) -> Vec<(&'static str, &str)> {
        match self {
            Node::Query { next, on_error, .. } => vec![("next", next), ("on_error", on_error)],
            Node::Action { next, on_error, on_unknown, .. } => {
                let mut r = vec![("next", next.as_str()), ("on_error", on_error.as_str())];
                if let Some(u) = on_unknown {
                    r.push(("on_unknown", u));
                }
                r
            }
            Node::Decision { routes, on_error, .. } => {
                let mut r: Vec<(&'static str, &str)> = routes.iter().map(|x| ("route", x.target.as_str())).collect();
                r.push(("on_error", on_error));
                r
            }
            Node::HumanTask { next, on_reject, on_timeout, .. } => vec![("next", next), ("on_reject", on_reject), ("on_timeout", on_timeout)],
            Node::WaitEvent { next, on_timeout, .. } => vec![("next", next), ("on_timeout", on_timeout)],
            Node::End { .. } | Node::Suspend { .. } => vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub id: String,
    pub label: String,
    pub trigger: Trigger,
    pub entry: String,
    /// The most steps one run may take.
    pub max_steps: u32,
    pub nodes: Vec<Node>,
}

/// A test the solution carries: the inputs, and what is expected of them, worked out by a person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Test {
    pub id: String,
    /// The decision it tests.
    pub decision: String,
    /// The decision's scope (`snapshot`, `parameters`) as JSON.
    pub given: BTreeMap<String, Value>,
    /// The outcome expected, or `"unknown"` when it must not be decided.
    pub expect: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    /// Where the builder draws things: outside every digest.
    #[serde(default)]
    pub layout: Map<String, Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

/// A parameter: a value the solution's expressions read as `parameters.{name}`, typed by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    #[serde(rename = "type")]
    pub ty: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Solution {
    pub schema_version: String,
    pub kind: String,
    pub id: String,
    pub version: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The capabilities it uses, at exact versions. Asking grants nothing.
    #[serde(default)]
    pub requires: Vec<CapRef>,
    #[serde(default)]
    pub parameters: BTreeMap<String, Parameter>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub decisions: Vec<decision::Table>,
    #[serde(default)]
    pub processes: Vec<Process>,
    #[serde(default)]
    pub tests: Vec<Test>,
    #[serde(default)]
    pub ui: Ui,
}

impl Solution {
    /// Reads JSON text strictly.
    pub fn parse(text: &str) -> Result<Solution, Refusal> {
        let v = strict_json(text)?;
        Solution::from_value(v)
    }

    pub fn from_value(v: Value) -> Result<Solution, Refusal> {
        let s: Solution = serde_json::from_value(v).map_err(|e| Refusal::new(Code::SchemaInvalid, format!("the source does not fit {SCHEMA_VERSION}: {e}")))?;
        if s.schema_version != SCHEMA_VERSION || s.kind != "Solution" {
            return Err(Refusal::new(Code::UnsupportedProfile, format!("this reads {SCHEMA_VERSION} solutions, not {} {}", s.schema_version, s.kind)));
        }
        Ok(s)
    }

    /// What its meaning is, for its digest: everything but where the builder draws things.
    pub fn semantic(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(ui) = v.get_mut("ui").and_then(Value::as_object_mut) {
            ui.remove("layout");
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_twice_is_refused() {
        let e = strict_json(r#"{"a": 1, "a": 2}"#).unwrap_err();
        assert!(e.message.contains("twice"), "{e}");
        assert!(strict_json(r#"{"a": {"b": 1, "b": 1}}"#).is_err(), "at any depth");
        assert!(strict_json(r#"{"a": 1, "b": 2}"#).is_ok());
    }

    #[test]
    fn size_and_depth_are_bounded() {
        assert!(strict_json(&format!("{}1{}", "[".repeat(70), "]".repeat(70))).is_err());
        assert!(strict_json(&" ".repeat(MAX_BYTES + 1)).is_err());
        assert!(strict_json("NaN").is_err(), "not a number is not JSON");
        assert!(strict_json("{} {}").is_err(), "one document");
    }
}
