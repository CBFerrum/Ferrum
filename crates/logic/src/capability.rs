//! What a business capability promises (FLC-CAP-001): an app's action, query or effect, with its input and
//! output, the effect it has, the rights it needs, how a repeat is recognised and what "done" means.
//!
//! The platform and the installed apps supply these. A customer's solution only names one by id and
//! version, so it cannot declare a dangerous action pure, and declaring one grants nothing (FLC-SEC-001).

use crate::canonical;
use crate::refusal::{Code, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The schema a capability's digest is taken under.
pub const SCHEMA: &str = "ferrum.capability/1";
/// The schema an invocation's input digest is taken under.
pub const INPUT_SCHEMA: &str = "ferrum.invocation/1";

/// What running a capability does to the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Computes from its input, nothing else.
    Pure,
    /// Reads, with the caller's rights.
    Query,
    /// Changes records through their owning app, in one transaction with its receipt.
    TransactionalAction,
    /// Reaches another system: committed as an intent first, sent after, possibly with an unknown outcome.
    ExternalEffect,
    /// Asks for a physical effect, which only the Operations ledger decides.
    OperationalIntent,
    /// Waits for a person's decision.
    HumanTask,
}

impl Effect {
    /// Whether it changes something, so an invocation must carry a logical effect key.
    pub fn changes(self) -> bool {
        !matches!(self, Effect::Pure | Effect::Query)
    }
}

/// Where a capability may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    /// Inside a record's transaction (rules).
    Transaction,
    /// In a durable process.
    Process,
    /// At an Edge gateway.
    Edge,
    /// In analysis.
    Analysis,
}

/// How a repeat of the same request is recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    Pure,
    ReadOnly,
    /// A receipt commits with the change, under the effect key.
    NativeReceipt,
    /// The provider deduplicates on the key, within its own horizon.
    ProviderKey,
    /// Nothing deduplicates: a repeat is found out by asking afterwards.
    ReconcileOnly,
}

/// What happens when it cannot be known whether it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownOutcome {
    NotApplicable,
    /// The capability can look the outcome up.
    Reconcile,
    /// A person settles it, with evidence.
    Manual,
}

/// The operation the gate carries out, on the entity the capability names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "name", rename_all = "snake_case")]
pub enum Operation {
    Read,
    List,
    Create,
    Update,
    Delete,
    Submit,
    Cancel,
    /// An action the entity declares.
    Action(String),
    /// A transition of the entity's workflow.
    Transition(String),
    /// An in-app notification.
    Notify,
    /// An e-mail, queued with the outbox.
    Mail,
    /// An operation of one of the workspace's connections (the kernel's `connections`).
    Call,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub timeout_ms: u64,
    pub max_attempts: u32,
    pub max_input_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { timeout_ms: 30_000, max_attempts: 3, max_input_bytes: 1 << 20 }
    }
}

/// One capability, as discovered (the same shape for the builder, the API and agents).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    /// `{package}.{entity}.{operation}` (`procurement.material_request.make_purchase_order`), or
    /// `ferrum.{what}` for the platform's own.
    pub id: String,
    /// The owning package's version. A process binds to it; another version is another capability.
    pub version: String,
    /// `sha256:` of everything here but this field: what was reviewed is what runs.
    pub digest: String,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    pub operation: Operation,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub effect: Effect,
    pub profiles: Vec<Profile>,
    /// What the caller must be allowed, as `Entity:action` (`MaterialRequest:submit`) or `role:Name`.
    pub permissions: Vec<String>,
    /// JSON Schema of the input.
    pub input: Value,
    /// JSON Schema of the result.
    pub output: Value,
    pub idempotency: Idempotency,
    /// How long a receipt is kept for a repeat to find.
    pub retention_days: u32,
    pub unknown_outcome: UnknownOutcome,
    /// What it means when it reports done: accepted is not completed.
    pub completion: String,
    /// The capability that undoes this one, where one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensate_with: Option<String>,
    pub limits: Limits,
    /// That it is on its way out, and what replaces it and until when, as its owner says (FLC-CAP-008). A release
    /// bound to it keeps it; checking a new one says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
}

/// A value as a digest sees it: a fraction becomes its decimal text, because canonical JSON holds no
/// fractions and a decimal sent as `2.5` or `"2.5"` is one value.
pub fn digestible(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.as_i64().is_none() => Value::String(n.to_string()),
        Value::Array(items) => Value::Array(items.iter().map(digestible).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), digestible(v))).collect()),
        other => other.clone(),
    }
}

impl Capability {
    /// The digest of everything but `digest` itself.
    pub fn compute_digest(&self) -> Result<String, Refusal> {
        let mut body = serde_json::to_value(self).map_err(|e| Refusal::new(Code::SchemaInvalid, e.to_string()))?;
        if let Some(map) = body.as_object_mut() {
            map.remove("digest");
        }
        canonical::digest(SCHEMA, &digestible(&body)).map_err(|e| Refusal::new(Code::SchemaInvalid, e.to_string()))
    }

    /// Sets `digest` from the rest.
    pub fn sealed(mut self) -> Result<Capability, Refusal> {
        self.digest = self.compute_digest()?;
        Ok(self)
    }

    /// The capability that undoes this one, when its app declares one: an action of the same entity.
    pub fn compensation(&self) -> Option<String> {
        let action = self.compensate_with.as_ref()?;
        Some(match &self.entity {
            Some(e) => format!("{}.{}.{action}", self.owner, snake(e)),
            None => format!("{}.{action}", self.owner),
        })
    }

    /// Whether the input's top-level fields are ones the capability declares.
    pub fn declares(&self, field: &str) -> bool {
        self.input.get("properties").and_then(Value::as_object).is_some_and(|p| p.contains_key(field))
    }

    /// Checks an input before it runs: its size, that it is an object, and that it does not try to say
    /// who is asking or for which workspace (FLC-CAP-003). Trusted context comes from the caller's
    /// authentication; a field naming it is refused unless the capability's own input declares it as data.
    pub fn check_input(&self, input: &Value) -> Result<(), Refusal> {
        let size = serde_json::to_vec(input).map(|b| b.len() as u64).unwrap_or(u64::MAX);
        if size > self.limits.max_input_bytes {
            return Err(Refusal::new(Code::SchemaInvalid, format!("the input is {size} bytes; {} takes at most {}", self.id, self.limits.max_input_bytes)));
        }
        let Some(map) = input.as_object() else {
            return Err(Refusal::new(Code::SchemaInvalid, format!("{} takes an object", self.id)));
        };
        if let Some(field) = map.keys().find(|k| CONTEXT_FIELDS.contains(&k.as_str()) && !self.declares(k)) {
            return Err(Refusal::new(
                Code::PermissionDenied,
                format!("{field} cannot be given to {}: who is asking, and for which workspace, comes from the caller's sign-in", self.id),
            )
            .at(format!("/{field}")));
        }
        Ok(())
    }

    /// The digest an effect key is bound to: this capability and this exact input.
    pub fn input_digest(&self, input: &Value) -> Result<String, Refusal> {
        let body = serde_json::json!({"capability": self.id, "input": digestible(input)});
        canonical::digest(INPUT_SCHEMA, &body).map_err(|e| Refusal::new(Code::SchemaInvalid, e.to_string()).at("/input"))
    }
}

/// Fields that would name trusted context if they were believed.
pub const CONTEXT_FIELDS: &[&str] = &["tenant", "tenant_id", "workspace", "roles", "role", "is_admin", "is_system", "permissions", "user_id", "acting_as"];

/// `MaterialRequest` → `material_request`, `POSInvoice` → `pos_invoice`.
pub fn snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev = i.checked_sub(1).map(|j| chars[j]);
            let next = chars.get(i + 1);
            // A word starts after a lower-case letter or digit, or at the last capital of an acronym.
            let starts = prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(|p| p.is_ascii_uppercase()) && next.is_some_and(|n| n.is_ascii_lowercase()));
            if starts {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// An empty object schema.
pub fn object_schema(properties: Map<String, Value>, required: Vec<String>) -> Value {
    serde_json::json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cap() -> Capability {
        Capability {
            id: "procurement.material_request.make_purchase_order".into(),
            version: "0.5.0".into(),
            digest: String::new(),
            owner: "procurement".into(),
            entity: Some("MaterialRequest".into()),
            operation: Operation::Action("make_purchase_order".into()),
            label: "Create Purchase Order".into(),
            description: None,
            effect: Effect::TransactionalAction,
            profiles: vec![Profile::Process],
            permissions: vec!["MaterialRequest:read".into()],
            input: object_schema(Map::from_iter([("id".into(), json!({"type": "string"})), ("supplier".into(), json!({"type": "string"}))]), vec!["id".into()]),
            output: json!({"type": "object"}),
            idempotency: Idempotency::NativeReceipt,
            retention_days: 365,
            unknown_outcome: UnknownOutcome::Reconcile,
            completion: "The order exists, committed with the receipt".into(),
            compensate_with: None,
            limits: Limits::default(),
            deprecated: None,
        }
    }

    #[test]
    fn the_digest_covers_what_it_does_and_not_itself() {
        let a = cap().sealed().unwrap();
        assert!(a.digest.starts_with("sha256:"));
        assert_eq!(a.compute_digest().unwrap(), a.digest, "sealing twice gives the same digest");
        let mut b = a.clone();
        b.effect = Effect::Pure;
        assert_ne!(b.compute_digest().unwrap(), a.digest, "calling a write pure is another capability");
    }

    #[test]
    fn an_input_cannot_say_who_is_asking() {
        let c = cap().sealed().unwrap();
        assert!(c.check_input(&json!({"id": "x", "supplier": "s"})).is_ok());
        for field in ["tenant_id", "roles", "is_admin"] {
            let refused = c.check_input(&json!({"id": "x", field: "other"})).unwrap_err();
            assert_eq!(refused.code, Code::PermissionDenied, "{field}");
            assert_eq!(refused.pointer.as_deref(), Some(format!("/{field}").as_str()));
        }
        assert_eq!(c.check_input(&json!([1])).unwrap_err().code, Code::SchemaInvalid);
    }

    #[test]
    fn a_field_the_capability_declares_is_data() {
        let mut c = cap();
        c.input["properties"]["role"] = json!({"type": "string"});
        assert!(c.check_input(&json!({"id": "x", "role": "Storekeeper"})).is_ok(), "a job title on a record is not a grant");
    }

    #[test]
    fn the_input_digest_binds_capability_and_exact_input() {
        let c = cap();
        let a = c.input_digest(&json!({"id": "x", "qty": "2.5"})).unwrap();
        assert_eq!(a, c.input_digest(&json!({"qty": 2.5, "id": "x"})).unwrap(), "key order and a decimal's spelling are not meaning");
        assert_ne!(a, c.input_digest(&json!({"id": "x", "qty": "2.6"})).unwrap());
        let mut other = c.clone();
        other.id = "procurement.material_request.make_rfq".into();
        assert_ne!(a, other.input_digest(&json!({"id": "x", "qty": "2.5"})).unwrap(), "the same input to another capability is another request");
    }

    #[test]
    fn an_oversized_input_is_refused() {
        let mut c = cap();
        c.limits.max_input_bytes = 16;
        assert_eq!(c.check_input(&json!({"id": "a long identifier"})).unwrap_err().code, Code::SchemaInvalid);
    }

    #[test]
    fn names_become_ids() {
        assert_eq!(snake("MaterialRequest"), "material_request");
        assert_eq!(snake("Uom"), "uom");
        assert_eq!(snake("POSInvoice"), "pos_invoice");
        assert_eq!(snake("BOMItem2Line"), "bom_item2_line");
    }
}
