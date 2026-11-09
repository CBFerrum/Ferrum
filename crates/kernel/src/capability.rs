//! The business capability catalog, and the one gate every process, simulation, API call and agent goes
//! through to use one (docs/logic, FLC-CAP-001 to 003).
//!
//! The catalog is read from what a workspace has installed, not typed in again: each entity's record
//! operations, the actions its package declares (with their contracts), its workflow's transitions, and the
//! platform's own (a notification, an e-mail). A caller discovers only what it may use.
//!
//! Invoking runs the owning app's operation through the kernel path a person's request takes, so every
//! hook, rule, row scope, field permission and audit entry applies. A capability that changes something
//! needs a logical effect key, and its receipt commits in the same transaction as the change: the same key
//! again returns what was done, and the same key with other input is refused (`input_changed`). Who is
//! asking, and for which workspace, comes from the caller's context, never from the input.

use crate::ctx::{Access, Action, Ctx};
use crate::db::{Tx, with_retry};
use crate::error::{Error, Result};
use crate::{Doc, Kernel};
use ferrum_logic::capability::{Capability, Effect, Idempotency, Limits, Operation, Profile, UnknownOutcome, object_schema, snake};
use ferrum_logic::refusal::{Code, Refusal};
use ferrum_meta::{EffEntity, EntityKind};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The environment a workspace's own runs use, until deployments name others (W4).
pub const PRODUCTION: &str = "production";
/// The most a `list` capability returns.
pub const LIST_LIMIT: u32 = 100;
const RETENTION_DAYS: u32 = 400;

/// One use of a capability.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub capability: String,
    /// The version the caller was built against; another installed version is refused, not adopted.
    pub version: Option<String>,
    pub input: Value,
    /// The logical effect's key: the same for every attempt at one effect, never an attempt's own.
    pub effect_key: Option<String>,
    pub environment: String,
}

impl Invocation {
    pub fn new(capability: impl Into<String>, input: Value) -> Invocation {
        Invocation { capability: capability.into(), version: None, input, effect_key: None, environment: PRODUCTION.into() }
    }

    pub fn key(mut self, key: impl Into<String>) -> Invocation {
        self.effect_key = Some(key.into());
        self
    }

    pub fn version(mut self, version: impl Into<String>) -> Invocation {
        self.version = Some(version.into());
        self
    }
}

/// What came of it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Receipt {
    pub capability: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_key: Option<String>,
    /// The key had been used before with the same input: this is what was done then.
    pub repeated: bool,
    pub result: Value,
}

fn refused(code: Code, message: impl Into<String>) -> Error {
    Refusal::new(code, message).into()
}

/// The version an entity's capabilities carry: its package's, or for a workspace's own entity, the
/// workspace schema's revision.
fn version_of(k: &Kernel, ctx: &Ctx, ent: &EffEntity) -> String {
    match k.registry.catalog().package(&ent.package) {
        Some(p) => p.manifest.version.clone(),
        None => format!("schema-{}", ctx.schema.version),
    }
}

/// Whether the caller holds a capability's permissions.
pub fn allowed(ctx: &Ctx, cap: &Capability) -> bool {
    allowed_as(ctx, cap, false)
}

/// The same, for a run acting as its solution's own account when `granted`: an administrator's capability
/// (mail) is then allowed because a second administrator reviewed the release with it in its lock, never
/// because the account is an administrator (FLC-CAP-005). Every other permission still comes from the
/// account's role.
fn allowed_as(ctx: &Ctx, cap: &Capability, granted: bool) -> bool {
    cap.permissions.iter().all(|p| match p.split_once(':') {
        Some(("role", role)) => ctx.is_admin || ctx.has_role(role),
        Some((entity, action)) => Action::parse(action).is_some_and(|a| ctx.perms.access(entity, a) != Access::None),
        None => p == "admin" && (ctx.is_admin || granted),
    })
}

/// A record's id, naming its kind (FLC-COMP-002).
fn id_prop(entity: &str) -> Value {
    json!({"type": "string", "format": "uuid", "x-ferrum-link": entity, "description": "The record's id"})
}

fn version_prop() -> Value {
    json!({"type": "integer", "description": "The version it was read at; a change since then is refused"})
}

fn param_schema(ptype: &str, to: Option<&str>, options: &[String]) -> Value {
    match ptype {
        "int" => json!({"type": "integer"}),
        "date" => json!({"type": "string", "format": "date"}),
        "select" => json!({"type": "string", "enum": options}),
        "link" => match to {
            Some(to) => json!({"type": "string", "format": "uuid", "x-ferrum-link": to, "description": format!("Id of a {to} record")}),
            None => json!({"type": "string", "format": "uuid", "description": "Id of a linked record"}),
        },
        _ => json!({"type": "string"}),
    }
}

#[allow(clippy::too_many_arguments)]
fn capability(
    id: String,
    version: &str,
    owner: &str,
    entity: Option<&str>,
    operation: Operation,
    label: String,
    effect: Effect,
    permissions: Vec<String>,
    input: Value,
    output: Value,
    completion: &str,
) -> Capability {
    let (idempotency, unknown_outcome, retention_days) = match effect {
        Effect::Pure => (Idempotency::Pure, UnknownOutcome::NotApplicable, 0),
        Effect::Query => (Idempotency::ReadOnly, UnknownOutcome::NotApplicable, 0),
        // The receipt commits with the change, so whether it happened can always be looked up.
        _ => (Idempotency::NativeReceipt, UnknownOutcome::Reconcile, RETENTION_DAYS),
    };
    let profiles = match effect {
        Effect::Query => vec![Profile::Process, Profile::Analysis],
        _ => vec![Profile::Process],
    };
    Capability {
        id,
        version: version.to_string(),
        digest: String::new(),
        owner: owner.to_string(),
        entity: entity.map(str::to_string),
        operation,
        label,
        description: None,
        effect,
        profiles,
        permissions,
        input,
        output,
        idempotency,
        retention_days,
        unknown_outcome,
        completion: completion.to_string(),
        compensate_with: None,
        limits: Limits::default(),
        deprecated: None,
    }
}

/// Every capability an entity offers, before anyone's rights are applied. A ledger offers none: its
/// engine posts it, and the API reads it through reports, so a capability reaches no further (FLC-CAP-002).
fn entity_capabilities(k: &Kernel, ctx: &Ctx, ent: &EffEntity) -> Vec<Capability> {
    if matches!(ent.def.kind, EntityKind::Ledger | EntityKind::Child) {
        return vec![];
    }
    let name = ent.def.name.as_str();
    let label = ent.def.label.as_str();
    let version = version_of(k, ctx, ent);
    let owner = ent.package.as_str();
    let prefix = format!("{owner}.{}", snake(name));
    let record = crate::describe::entity_schema(ctx, ent, 0);
    // What comes back is the record as stored: what was written, and its id and version, so that a process
    // can name it and see whether it changed since.
    let stored = {
        let mut s = record.clone();
        if let Some(props) = s.get_mut("properties").and_then(Value::as_object_mut) {
            props.insert("id".into(), id_prop(name));
            props.insert("version".into(), json!({"type": "integer", "description": "The record's version"}));
            if ent.def.submittable {
                props.insert("docstatus".into(), json!({"type": "integer", "description": "0 draft, 1 submitted, 2 cancelled"}));
            }
        }
        s
    };
    let perm = |a: &str| format!("{name}:{a}");
    let by_id = |extra: &[(&str, Value)]| {
        let mut props = Map::from_iter([("id".to_string(), id_prop(name))]);
        props.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
        object_schema(props, vec!["id".into()])
    };
    let cap = |op: &str, operation: Operation, l: String, effect: Effect, permission: &str, input: Value, output: Value, completion: &str| {
        capability(format!("{prefix}.{op}"), &version, owner, Some(name), operation, l, effect, vec![perm(permission)], input, output, completion)
    };
    let mut out = vec![
        cap("read", Operation::Read, format!("Read a {label}"), Effect::Query, "read", by_id(&[]), stored.clone(), "The record as the caller may see it"),
        cap(
            "list",
            Operation::List,
            format!("Find {label} records"),
            Effect::Query,
            "read",
            object_schema(
                Map::from_iter([
                    ("filters".into(), json!({"type": "array", "description": "[[field, op, value], ...] as the list API takes them"})),
                    ("sort".into(), json!({"type": "string"})),
                    ("limit".into(), json!({"type": "integer", "minimum": 1, "maximum": LIST_LIMIT})),
                ]),
                vec![],
            ),
            json!({"type": "array", "items": stored}),
            "The records the caller may see, at most the limit",
        ),
    ];
    out.push(cap(
        "create",
        Operation::Create,
        format!("Create a {label}"),
        Effect::TransactionalAction,
        "create",
        object_schema(Map::from_iter([("values".into(), record.clone()), ("submit".into(), json!({"type": "boolean"}))]), vec!["values".into()]),
        stored.clone(),
        "The record exists, committed with this receipt",
    ));
    out.push(cap(
        "update",
        Operation::Update,
        format!("Change a {label}"),
        Effect::TransactionalAction,
        "write",
        by_id(&[("values", record.clone()), ("expected_version", version_prop())]),
        stored.clone(),
        "The change is committed with this receipt",
    ));
    out.push(cap(
        "delete",
        Operation::Delete,
        format!("Delete a {label}"),
        Effect::TransactionalAction,
        "delete",
        by_id(&[("expected_version", version_prop())]),
        json!({"type": "object"}),
        "The record is gone, committed with this receipt",
    ));
    if ent.def.submittable {
        out.push(cap(
            "submit",
            Operation::Submit,
            format!("Submit a {label}"),
            Effect::TransactionalAction,
            "submit",
            by_id(&[("expected_version", version_prop())]),
            stored.clone(),
            "Submitted, with every posting its app makes, committed with this receipt",
        ));
        out.push(cap(
            "cancel",
            Operation::Cancel,
            format!("Cancel a {label}"),
            Effect::TransactionalAction,
            "cancel",
            by_id(&[("expected_version", version_prop())]),
            stored.clone(),
            "Cancelled, with its postings reversed by its app, committed with this receipt",
        ));
    }
    for a in &ent.def.actions {
        // An action meant only for code stays out, unless its app says what it promises.
        if a.hidden && a.contract.is_none() {
            continue;
        }
        let contract = a.contract.clone().unwrap_or_default();
        let effect = contract.effect.unwrap_or(if a.read_only { Effect::Query } else { Effect::TransactionalAction });
        let mut params = Map::new();
        let mut required = vec![];
        for p in &a.params {
            let mut s = param_schema(&p.ptype, p.to.as_deref(), &p.options);
            s["description"] = json!(p.description.clone().unwrap_or_else(|| p.label.clone()));
            params.insert(p.name.clone(), s);
            if p.required {
                required.push(p.name.clone());
            }
        }
        let input = by_id(&[("params", object_schema(params, required))]);
        let completion = contract
            .completion
            .clone()
            .unwrap_or_else(|| if effect.changes() { format!("{} is done, committed with this receipt", a.label) } else { format!("What {} reads", a.label) });
        let mut c = capability(
            format!("{prefix}.{}", a.name),
            &version,
            owner,
            Some(name),
            Operation::Action(a.name.clone()),
            a.label.clone(),
            effect,
            vec![perm(&a.permission)],
            input,
            contract.returns.clone().unwrap_or_else(|| json!({"type": "object"})),
            &completion,
        );
        if let Some(i) = contract.idempotency {
            c.idempotency = i;
        }
        c.deprecated = contract.deprecated.clone();
        if let Some(u) = contract.unknown_outcome {
            c.unknown_outcome = u;
        }
        if let Some(days) = contract.retention_days {
            c.retention_days = days;
        }
        c.compensate_with = contract.compensate_with.as_ref().map(|undo| format!("{prefix}.{undo}"));
        out.push(c);
    }
    if let Some(wf) = &ent.workflow {
        let mut seen = std::collections::BTreeSet::new();
        for t in &wf.def.transitions {
            if !seen.insert(t.action.clone()) {
                continue;
            }
            let mut permissions = vec![perm("read")];
            // Any one of the transition's roles may take it; the transition itself checks which.
            if t.roles.len() == 1 {
                permissions.push(format!("role:{}", t.roles[0]));
            }
            out.push(capability(
                format!("{prefix}.transition.{}", t.action),
                &version,
                owner,
                Some(name),
                Operation::Transition(t.action.clone()),
                format!("{} ({label})", t.action),
                Effect::TransactionalAction,
                permissions,
                by_id(&[("expected_version", version_prop())]),
                stored.clone(),
                &format!("The record has moved to {}, committed with this receipt", t.to),
            ));
        }
    }
    out
}

/// The platform's own capabilities.
fn platform_capabilities() -> Vec<Capability> {
    let version = env!("CARGO_PKG_VERSION");
    let notify = capability(
        "ferrum.notify".into(),
        version,
        "ferrum",
        None,
        Operation::Notify,
        "Notify a person in the app".into(),
        Effect::TransactionalAction,
        vec![],
        object_schema(
            Map::from_iter([
                ("user".into(), json!({"type": "string", "format": "uuid", "description": "An active person of this workspace"})),
                ("title".into(), json!({"type": "string", "maxLength": 300})),
                ("body".into(), json!({"type": "string", "maxLength": 4000})),
                ("entity".into(), json!({"type": "string"})),
                ("record".into(), json!({"type": "string", "format": "uuid"})),
            ]),
            vec!["user".into(), "title".into()],
        ),
        json!({"type": "object"}),
        "The notification is in their list, committed with this receipt",
    );
    let mut mail = capability(
        "ferrum.mail".into(),
        version,
        "ferrum",
        None,
        Operation::Mail,
        "Send an e-mail".into(),
        Effect::ExternalEffect,
        // Mail leaves the workspace: an administrator's, until data-flow policy decides finer (W2).
        vec!["admin".into()],
        object_schema(
            Map::from_iter([
                ("to".into(), json!({"type": "string", "format": "email"})),
                ("subject".into(), json!({"type": "string", "maxLength": 250})),
                ("body".into(), json!({"type": "string"})),
            ]),
            vec!["to".into(), "subject".into(), "body".into()],
        ),
        json!({"type": "object"}),
        "Queued in the outbox with this receipt; the mail server's delivery is not confirmed",
    );
    // SMTP has no idempotency of its own: the outbox delivers at least once, and a person settles doubt.
    mail.unknown_outcome = UnknownOutcome::Manual;
    // Marketing by e-mail: the same, not sent to someone who withdrew their consent to marketing, asked as
    // it goes (FEO-015). Its own capability, so `ferrum.mail`'s digest, and the releases reviewed with
    // it, stay as they were.
    let mut marketing = mail.clone();
    marketing.id = "ferrum.mail.marketing".into();
    marketing.label = "Send a marketing e-mail".into();
    marketing.completion =
        "Queued in the outbox with this receipt, unless the person withdrew their consent to marketing; the mail server's delivery is not confirmed".into();
    vec![notify, mail, marketing]
}

impl Kernel {
    /// What this caller may use, sorted by id. Entities it can do nothing with are not even looked at.
    pub fn catalog(&self, ctx: &Ctx) -> Vec<Capability> {
        let actions = [Action::Read, Action::Write, Action::Create, Action::Delete, Action::Submit, Action::Cancel];
        let mut out: Vec<Capability> = platform_capabilities();
        for ent in ctx.schema.entities.values() {
            if actions.iter().any(|a| ctx.perms.access(&ent.def.name, *a) != Access::None) {
                out.extend(entity_capabilities(self, ctx, ent));
            }
        }
        let mut out: Vec<Capability> = out.into_iter().filter(|c| allowed(ctx, c)).filter_map(|c| c.sealed().ok()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// One capability, if this caller may use it. Only its own entity's are worked out.
    pub fn capability(&self, ctx: &Ctx, id: &str) -> Result<Capability> {
        self.capability_as(ctx, id, false)
    }

    /// One capability for a run acting as its solution's own account (`granted`, see [`allowed_as`]).
    pub(crate) fn capability_as(&self, ctx: &Ctx, id: &str, granted: bool) -> Result<Capability> {
        let candidates = if id.starts_with("ferrum.") {
            platform_capabilities()
        } else {
            ctx.schema
                .entities
                .values()
                .filter(|e| id.starts_with(&format!("{}.{}.", e.package, snake(&e.def.name))))
                .flat_map(|e| entity_capabilities(self, ctx, e))
                .collect()
        };
        let found = candidates.into_iter().find(|c| c.id == id).and_then(|c| c.sealed().ok());
        match found {
            Some(c) if allowed_as(ctx, &c, granted) => Ok(c),
            Some(c) => Err(refused(Code::PermissionDenied, format!("you may not use {}: it needs {}", c.id, c.permissions.join(", ")))),
            None => Err(refused(Code::CapabilityUnavailable, format!("{id} is not installed in this workspace"))),
        }
    }

    /// The events processes can start on or wait for: each readable entity's own, and those packages
    /// declare. `topic` is what the outbox carries.
    pub fn events(&self, ctx: &Ctx) -> Vec<Value> {
        let mut out = vec![];
        for ent in ctx.schema.entities.values() {
            if ent.def.kind == EntityKind::Child || ctx.perms.access(&ent.def.name, Action::Read) == Access::None {
                continue;
            }
            let mut kinds = vec!["created", "updated", "deleted"];
            if ent.def.submittable {
                kinds.extend(["submitted", "cancelled"]);
            }
            if ent.workflow.is_some() {
                kinds.push("transitioned");
            }
            for kind in kinds {
                out.push(json!({
                    "id": format!("{}.{}.{kind}", ent.package, snake(&ent.def.name)),
                    "topic": format!("{}.{kind}", ent.def.name),
                    "entity": ent.def.name,
                    "label": format!("{} {kind}", ent.def.label),
                }));
            }
        }
        for name in &ctx.schema.packages {
            let Some(p) = self.registry.catalog().package(name).cloned() else { continue };
            for ev in &p.manifest.events {
                if ev.entity.as_ref().is_some_and(|e| ctx.perms.access(e, Action::Read) == Access::None) {
                    continue;
                }
                out.push(json!({
                    "id": format!("{name}.{}", ev.name),
                    "topic": format!("{name}.{}", ev.name),
                    "entity": ev.entity,
                    "label": ev.label,
                    "description": ev.description,
                    "payload": ev.payload,
                }));
            }
        }
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        out
    }

    /// Runs a capability in its own transaction, again on a serialization failure under the same key.
    pub async fn invoke(&self, ctx: &Ctx, inv: Invocation) -> Result<Receipt> {
        self.invoke_as(ctx, inv, false).await
    }

    /// The same for a run, `granted` when it acts as its solution's own account (see [`allowed_as`]).
    pub(crate) async fn invoke_as(&self, ctx: &Ctx, inv: Invocation, granted: bool) -> Result<Receipt> {
        let cap = self.capability_as(ctx, &inv.capability, granted)?;
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            if !cap.effect.changes() {
                // A read promises not to write; the database holds it to that.
                sqlx::query("set transaction read only").execute(&mut *tx).await?;
            }
            let out = self.invoke_tx_as(ctx, &mut tx, &inv, granted).await?;
            tx.commit().await?;
            Ok(out)
        })
        .await
    }

    /// Runs a capability in the caller's transaction, with its receipt.
    pub async fn invoke_tx(&self, ctx: &Ctx, tx: &mut Tx, inv: &Invocation) -> Result<Receipt> {
        self.invoke_tx_as(ctx, tx, inv, false).await
    }

    async fn invoke_tx_as(&self, ctx: &Ctx, tx: &mut Tx, inv: &Invocation, granted: bool) -> Result<Receipt> {
        let cap = self.capability_as(ctx, &inv.capability, granted)?;
        if let Some(v) = &inv.version
            && v != &cap.version
        {
            return Err(refused(Code::CapabilityUnavailable, format!("{} is bound to version {v}; this workspace has {}", cap.id, cap.version)));
        }
        cap.check_input(&inv.input)?;
        if !cap.effect.changes() {
            let result = self.carry_out(ctx, tx, &cap, &inv.input).await?;
            return Ok(Receipt { capability: cap.id, version: cap.version, effect_key: None, repeated: false, result });
        }
        let Some(key) = inv.effect_key.as_deref().filter(|k| !k.trim().is_empty()) else {
            return Err(refused(Code::SchemaInvalid, format!("{} changes something, so it needs a logical effect key", cap.id)));
        };
        let digest = cap.input_digest(&inv.input)?;
        let claimed = sqlx::query(
            "insert into logic_effect (tenant_id, environment, capability, effect_key, input_digest, version, capability_digest, actor) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) on conflict do nothing",
        )
        .bind(ctx.tenant)
        .bind(&inv.environment)
        .bind(&cap.id)
        .bind(key)
        .bind(&digest)
        .bind(&cap.version)
        .bind(&cap.digest)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut **tx)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            return self.repeat(ctx, tx, &cap, &inv.environment, key, &digest).await;
        }
        let result = self.carry_out(ctx, tx, &cap, &inv.input).await?;
        let record = result.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        sqlx::query(
            "update logic_effect set result = $5, record_entity = $6, record_id = $7 \
             where tenant_id = $1 and environment = $2 and capability = $3 and effect_key = $4",
        )
        .bind(ctx.tenant)
        .bind(&inv.environment)
        .bind(&cap.id)
        .bind(key)
        .bind(&result)
        .bind(record.and(cap.entity.as_deref()))
        .bind(record)
        .execute(&mut **tx)
        .await?;
        Ok(Receipt { capability: cap.id, version: cap.version, effect_key: Some(key.to_string()), repeated: false, result })
    }

    /// The key was used before: the same input gets what was done, with the caller's own view of it;
    /// other input is refused.
    async fn repeat(&self, ctx: &Ctx, tx: &mut Tx, cap: &Capability, environment: &str, key: &str, digest: &str) -> Result<Receipt> {
        let (was, result, entity, record, actor): (String, Value, Option<String>, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
            "select input_digest, result, record_entity, record_id, actor from logic_effect \
             where tenant_id = $1 and environment = $2 and capability = $3 and effect_key = $4",
        )
        .bind(ctx.tenant)
        .bind(environment)
        .bind(&cap.id)
        .bind(key)
        .fetch_one(&mut **tx)
        .await?;
        if was != digest {
            return Err(refused(Code::InputChanged, format!("the key {key} was used for {} with other input: use a new key for a new request", cap.id)));
        }
        // What someone else did is shown as the caller may see it now, never as its first caller saw it.
        let shown = match (entity, record) {
            (Some(e), Some(id)) => self.get_tx(ctx, tx, &e, id, false).await.map(Value::Object).unwrap_or(Value::Null),
            _ if actor == Some(ctx.user) => result,
            _ => Value::Null,
        };
        Ok(Receipt { capability: cap.id.clone(), version: cap.version.clone(), effect_key: Some(key.to_string()), repeated: true, result: shown })
    }

    /// The owning app's operation, through the kernel path a person's request takes.
    async fn carry_out(&self, ctx: &Ctx, tx: &mut Tx, cap: &Capability, input: &Value) -> Result<Value> {
        let entity = cap.entity.as_deref().unwrap_or_default();
        let id = || -> Result<Uuid> {
            input
                .get("id")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| refused(Code::SchemaInvalid, "id must be a record's id").into_pointer("/id"))
        };
        let version = input.get("expected_version").and_then(Value::as_i64);
        let object = |key: &str| -> Result<Doc> {
            match input.get(key) {
                None => Ok(Doc::new()),
                Some(Value::Object(m)) => Ok(m.clone()),
                Some(_) => Err(refused(Code::SchemaInvalid, format!("{key} must be an object")).into_pointer(&format!("/{key}"))),
            }
        };
        let record = |d: Doc| Value::Object(d);
        Ok(match &cap.operation {
            Operation::Read => record(self.get_tx(ctx, tx, entity, id()?, false).await?),
            Operation::List => {
                let filters = crate::query::parse_filters(input.get("filters").unwrap_or(&Value::Null))?;
                let limit = input.get("limit").and_then(Value::as_u64).map_or(LIST_LIMIT, |n| (n as u32).clamp(1, LIST_LIMIT));
                let sort = input.get("sort").and_then(Value::as_str);
                // `find_tx` is the engines' lookup and masks nothing: each row is masked here as a list masks it.
                let ent = self.entity(ctx, entity)?;
                let rows = self.find_tx(ctx, tx, entity, &filters, sort, limit).await?;
                Value::Array(rows.into_iter().map(|d| Value::Object(self.mask(ctx, &ent, d))).collect())
            }
            Operation::Create => {
                let doc = self.insert_tx(ctx, tx, entity, object("values")?).await?;
                if input.get("submit").and_then(Value::as_bool) == Some(true) {
                    let id = crate::doc::uuid_of(doc.get("id")).ok_or_else(|| Error::internal("a created record has no id"))?;
                    record(self.submit_tx(ctx, tx, entity, id, None).await?)
                } else {
                    record(doc)
                }
            }
            Operation::Update => record(self.update_tx(ctx, tx, entity, id()?, object("values")?, version).await?),
            Operation::Delete => {
                let id = id()?;
                self.delete_tx(ctx, tx, entity, id, version).await?;
                json!({"id": id, "deleted": true})
            }
            Operation::Submit => record(self.submit_tx(ctx, tx, entity, id()?, version).await?),
            Operation::Cancel => record(self.cancel_tx(ctx, tx, entity, id()?, version).await?),
            Operation::Action(name) => self.run_action_tx(ctx, tx, entity, id()?, name, &Value::Object(object("params")?)).await?,
            Operation::Transition(name) => record(self.transition_tx(ctx, tx, entity, id()?, name, version).await?),
            Operation::Notify => {
                let user: Uuid = input
                    .get("user")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| refused(Code::SchemaInvalid, "user must be a person's id").into_pointer("/user"))?;
                let active: Option<bool> = sqlx::query_scalar("select is_active from app_user where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(user)
                    .fetch_optional(&mut **tx)
                    .await?;
                if active != Some(true) {
                    return Err(refused(Code::SchemaInvalid, "user is not an active person of this workspace").into_pointer("/user"));
                }
                let text = |k: &str| input.get(k).and_then(Value::as_str);
                let record_id = text("record").and_then(|s| s.parse::<Uuid>().ok());
                crate::notify::send_kind(tx, ctx.tenant, user, "process", "inapp", text("title").unwrap_or_default(), text("body"), text("entity"), record_id)
                    .await?;
                json!({"notified": user})
            }
            Operation::Mail => {
                let text = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
                let purpose = if cap.id == "ferrum.mail.marketing" { crate::retention::Purpose::Marketing } else { crate::retention::Purpose::Transactional };
                self.queue_mail_for(tx, ctx.tenant, text("to"), text("subject"), text("body"), &[], purpose).await?;
                json!({"queued": true})
            }
        })
    }
}

trait IntoPointer {
    fn into_pointer(self, pointer: &str) -> Error;
}

impl IntoPointer for Error {
    /// The same refusal, saying where in the input it applies.
    fn into_pointer(self, pointer: &str) -> Error {
        match self {
            Error::Refused { code, status, message, .. } => Error::Refused { code, status, message, pointer: Some(pointer.to_string()) },
            other => other,
        }
    }
}
