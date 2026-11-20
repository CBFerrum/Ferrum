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
use crate::vault::Vault;
use crate::{Doc, Kernel};
pub use ferrum_logic::capability::snake;
use ferrum_logic::capability::{Capability, Effect, Idempotency, Limits, Operation, Profile, UnknownOutcome, object_schema};
use ferrum_logic::refusal::{Code, Refusal};
use ferrum_meta::{EffEntity, EntityKind};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The environment a workspace's own runs use, until deployments name others (W4).
pub const PRODUCTION: &str = "production";
/// The most a `list` capability returns.
pub const LIST_LIMIT: u32 = 100;
const RETENTION_DAYS: u32 = 400;

/// The most one page of the catalog holds.
pub const CATALOG_PAGE: usize = 100;

/// What [`Kernel::catalog_page`] looks for; every part given must hold.
#[derive(Debug, Clone, Default)]
pub struct CatalogQuery {
    /// Words, each to occur in the id, label, description or entity label (any case).
    pub q: Option<String>,
    /// The package the id starts with (`crm`).
    pub app: Option<String>,
    pub entity: Option<String>,
    pub effect: Option<Effect>,
    /// Only those that change something (or only those that do not).
    pub changes: Option<bool>,
    pub limit: usize,
    /// The previous page's next cursor.
    pub cursor: Option<String>,
}

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
    /// A repeat of an action whose result has fields given once: that they were, and what to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_once: Option<String>,
}

fn refused(code: Code, message: impl Into<String>) -> Error {
    Refusal::new(code, message).into()
}

/// The version capabilities of `package` carry: the package's, or for a workspace's own entity (`tenant`), the
/// workspace schema's revision.
fn version_of(k: &Kernel, ctx: &Ctx, package: &str) -> String {
    match k.registry.catalog().package(package) {
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
    // Level 2 of zones: a capability of a package the caller's zone leaves out is not theirs, nor of whatever acts
    // for them, however it was granted (`ferrum.…` is the platform's own, held by its permissions alone).
    if let Some(limit) = &ctx.perms.limit {
        let package = cap.id.split('.').next().unwrap_or_default();
        if cap.entity.as_deref().is_some_and(|e| !limit.sees(e)) || (package != "ferrum" && !limit.sees_package(package)) {
            return false;
        }
    }
    // A rule naming the action lets its holder run it, whatever the action's own permission asks (BD-02, A11 D1), as on
    // the records path (`ctx::action_scope`); the action's roles still apply.
    let named = match (&cap.operation, cap.entity.as_deref()) {
        (Operation::Action(name), Some(entity)) => ctx.perms.action_grants(entity, name).is_some(),
        _ => false,
    };
    cap.permissions.iter().all(|p| match p.split_once(':') {
        Some(("role", role)) => ctx.is_admin || ctx.has_role(role),
        Some((entity, _)) if named && cap.entity.as_deref() == Some(entity) => true,
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

/// The schema of an action's `params`: what the gate takes, as the catalog, MCP and the OpenAPI document show it.
pub fn params_schema(params: &[ferrum_meta::ActionParam]) -> Value {
    let mut props = Map::new();
    for p in params {
        let mut s = param_schema(p);
        s["description"] = json!(p.description.clone().unwrap_or_else(|| p.label.clone()));
        if p.secret {
            s["writeOnly"] = json!(true);
        }
        props.insert(p.name.clone(), s);
    }
    object_schema(props, params.iter().filter(|p| p.required).map(|p| p.name.clone()).collect())
}

fn param_schema(p: &ferrum_meta::ActionParam) -> Value {
    match p.ptype.as_str() {
        "list" => json!({
            "type": "array",
            "maxItems": p.max_items.unwrap_or(ferrum_meta::MAX_LIST_ITEMS).min(ferrum_meta::MAX_LIST_ITEMS),
            "items": p.of.as_deref().map_or_else(|| params_schema(&p.fields), |of| one_schema(of, p.to.as_deref(), &p.options)),
        }),
        "row" => params_schema(&p.fields),
        t => one_schema(t, p.to.as_deref(), &p.options),
    }
}

fn one_schema(ptype: &str, to: Option<&str>, options: &[String]) -> Value {
    match ptype {
        "int" => json!({"type": "integer"}),
        "decimal" => json!({"type": ["number", "string"], "format": "decimal"}),
        "date" => json!({"type": "string", "format": "date"}),
        "check" => json!({"type": "boolean"}),
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
    let version = version_of(k, ctx, &ent.package);
    let owner = ent.package.as_str();
    let prefix = format!("{owner}.{}", snake(name));
    let record = crate::describe::entity_schema(ctx, ent, 0);
    // What comes back is the record as stored, as the caller may read it: what was written, its id and version, so that
    // a process can name it and see whether it changed since; any field may be empty.
    let stored = crate::describe::record_output_schema(ctx, ent, 0);
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
        // One another package's overlay added is that package's operation, its code behind it: its id, owner and version.
        let origin = a.origin.as_deref().unwrap_or(owner);
        let of = |package: &str| format!("{package}.{}", snake(name));
        let version = if origin == owner { version.clone() } else { version_of(k, ctx, origin) };
        let contract = a.contract.clone().unwrap_or_default();
        let effect = contract.effect.unwrap_or(if a.read_only { Effect::Query } else { Effect::TransactionalAction });
        let input = by_id(&[("params", params_schema(&a.params))]);
        let completion = contract
            .completion
            .clone()
            .unwrap_or_else(|| if effect.changes() { format!("{} is done, committed with this receipt", a.label) } else { format!("What {} reads", a.label) });
        let mut c = capability(
            format!("{}.{}", of(origin), a.name),
            &version,
            origin,
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
        // The undo is named where it comes from: the entity's own action, or the one a package added.
        c.compensate_with = contract.compensate_with.as_ref().map(|undo| {
            let from = ent.def.actions.iter().find(|x| &x.name == undo).and_then(|x| x.origin.as_deref()).unwrap_or(owner);
            format!("{}.{undo}", of(from))
        });
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
    // A connection's operation (BD-29): an administrator's, as configuration is; the connection's own roles and apps are
    // asked again as it runs. The effect key goes to the service as the call's key.
    let call = capability(
        "ferrum.call".into(),
        version,
        "ferrum",
        None,
        Operation::Call,
        "Call an operation of a connection".into(),
        Effect::ExternalEffect,
        vec!["admin".into()],
        object_schema(
            Map::from_iter([
                ("connection".into(), json!({"type": "string", "maxLength": 63})),
                ("operation".into(), json!({"type": "string", "maxLength": 63})),
                ("inputs".into(), json!({"type": "object"})),
            ]),
            vec!["connection".into(), "operation".into()],
        ),
        json!({"type": "object"}),
        "The service answered; its mapped result is this receipt's",
    );
    vec![notify, mail, marketing, call]
}

impl Kernel {
    /// What this caller may use, sorted by id. Entities it can do nothing with are not even looked at.
    pub fn catalog(&self, ctx: &Ctx) -> Vec<Capability> {
        let actions = [Action::Read, Action::Write, Action::Create, Action::Delete, Action::Submit, Action::Cancel];
        let mut out: Vec<Capability> = platform_capabilities();
        for ent in ctx.schema.entities.values() {
            // An entity reached only through an action a rule names is listed too: its other operations are filtered out.
            if actions.iter().any(|a| ctx.perms.access(&ent.def.name, *a) != Access::None) || !ctx.perms.named_actions(&ent.def.name).is_empty() {
                out.extend(entity_capabilities(self, ctx, ent));
            }
        }
        let mut out: Vec<Capability> = out.into_iter().filter(|c| allowed(ctx, c)).filter_map(|c| c.sealed().ok()).collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// One page of [`Kernel::catalog`] for a search (`list_capabilities`): sorted by id, the `limit` after `cursor`, and
    /// the cursor of the next page when there is one.
    // ponytail: the catalog is built per call in memory; cache by (tenant, schema version, rights) when measured.
    pub fn catalog_page(&self, ctx: &Ctx, q: &CatalogQuery) -> Result<(Vec<Capability>, Option<String>)> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let after = match &q.cursor {
            Some(c) => Some(
                b64.decode(c)
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .ok_or_else(|| refused(Code::SchemaInvalid, "cursor is not one this server gave").into_pointer("/cursor"))?,
            ),
            None => None,
        };
        let words: Vec<String> = q.q.as_deref().unwrap_or_default().split_whitespace().map(str::to_lowercase).collect();
        let limit = q.limit.clamp(1, CATALOG_PAGE);
        let mut found = self.catalog(ctx).into_iter().filter(|c| {
            let entity_label = c.entity.as_deref().and_then(|e| ctx.schema.entity(e)).map(|e| e.def.label.as_str()).unwrap_or_default();
            let text = format!("{} {} {} {}", c.id, c.label, c.description.as_deref().unwrap_or_default(), entity_label).to_lowercase();
            after.as_deref().is_none_or(|a| c.id.as_str() > a)
                && words.iter().all(|w| text.contains(w))
                && q.app.as_deref().is_none_or(|app| c.id.split('.').next() == Some(app))
                && q.entity.as_deref().is_none_or(|e| c.entity.as_deref() == Some(e))
                && q.effect.is_none_or(|e| c.effect == e)
                && q.changes.is_none_or(|changes| c.effect.changes() == changes)
        });
        let page: Vec<Capability> = found.by_ref().take(limit).collect();
        let next = match (found.next(), page.last()) {
            (Some(_), Some(last)) => Some(b64.encode(&last.id)),
            _ => None,
        };
        Ok((page, next))
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
            // `{package}.{entity}.…`: the package is the entity's, or the one whose overlay added the action.
            let entity = id.split('.').nth(1).unwrap_or_default();
            ctx.schema.entities.values().filter(|e| snake(&e.def.name) == entity).flat_map(|e| entity_capabilities(self, ctx, e)).collect()
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
            // `opened`: a signed link to the record first acted on by its visitor (BD-38), any entity's.
            let mut kinds = vec!["created", "updated", "deleted", "opened"];
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
        self.run_capability(ctx, tx, cap, inv, None).await
    }

    /// App `handle`'s call of another app's operation, for `ctx`'s person, in the caller's transaction `tx`
    /// (docs/app-contract.md, *Using another app's operations*). The handle says which app calls: one this server gave
    /// out. A package of that app installed in the workspace must declare the capability in its `uses` (`forbidden`),
    /// the installed operation be at a version within a range declared (`capability_unavailable`), and an action's
    /// params be those it declares (`invalid`). It runs through the gate as the system on the person's behalf, within
    /// their zone: the person is its actor, the key is the app's own (`{app}:{key}`), and the receipt names the app.
    /// What would wait for an approval is refused: a call does the thing or fails. A refused call leaves nothing of itself
    /// in `tx` (a savepoint around it), and what the caller wrote before it stays: the caller may carry on and commit.
    pub async fn call_tx(&self, handle: &Vault, ctx: &Ctx, tx: &mut Tx, inv: &Invocation) -> Result<Receipt> {
        let app = handle.given(self)?;
        let catalog = self.registry.catalog();
        let ranges: Vec<&str> = ctx
            .schema
            .packages
            .iter()
            .filter(|p| self.hooks.package_owner(p) == Some(app))
            .filter_map(|p| catalog.package(p))
            .flat_map(|p| &p.manifest.uses)
            .filter(|u| u.capability == inv.capability)
            .map(|u| u.version.as_str())
            .collect();
        if ranges.is_empty() {
            return Err(refused(Code::PermissionDenied, format!("{app} does not use {} here: none of its packages installed declares it", inv.capability)));
        }
        // The calling app, named on the context its operation runs with (BD-83); effects inside it name none.
        let system = Ctx { app: Some(app.to_string()), ..crate::actions::system(ctx) };
        let cap = self.capability_as(&system, &inv.capability, false)?;
        let installed = semver::Version::parse(&cap.version).ok();
        let within = |range: &&str| semver::VersionReq::parse(range).is_ok_and(|r| installed.as_ref().is_some_and(|v| r.matches(v)));
        if !ranges.iter().any(within) {
            return Err(refused(
                Code::CapabilityUnavailable,
                format!("{} is at {} here, and {app} was built for {}", cap.id, cap.version, ranges.join(" or ")),
            ));
        }
        if let Operation::Action(name) = &cap.operation
            && let Some(a) = crate::receipt::action_def(ctx, cap.entity.as_deref().unwrap_or_default(), name)
        {
            crate::doc::given_params(&a.name, &a.params, inv.input.get("params").unwrap_or(&Value::Null))?;
        }
        let key = inv.effect_key.as_deref().filter(|k| !k.trim().is_empty()).map(|k| format!("{app}:{k}"));
        // A refusal takes back all the call wrote, its receipt too, and nothing the caller wrote before it: a caller that
        // carries on commits none of it. Savepoints nest, so a call made inside another's operation is undone alone.
        sqlx::query("savepoint app_call").execute(&mut **tx).await?;
        // A query answers as the caller (the person, a records key, an agent): it never reads past their reach. What
        // changes something runs as the system for the person, as approved at install.
        let runner = if cap.effect.changes() { &system } else { ctx };
        let done = match self.run_capability(runner, tx, cap, &Invocation { effect_key: key, ..inv.clone() }, Some(app)).await {
            Ok(done) if done.result.get("_approval").is_some() => {
                Err(refused(Code::PermissionDenied, format!("{} waits for an approval here, and a call from {app} does it or fails", done.capability)))
            }
            done => done,
        };
        // Rolled back to, a savepoint stays: released as well, so that a refusal of the call around this one rolls back
        // to its own savepoint, not to this one.
        let end = if done.is_ok() { "release savepoint app_call" } else { "rollback to savepoint app_call; release savepoint app_call" };
        sqlx::raw_sql(end).execute(&mut **tx).await?;
        done
    }

    /// Capability `cap` run in `tx` as `inv` asks, its receipt naming `caller`, the app whose call it is
    /// ([`Kernel::call_tx`]); none for everyone else.
    async fn run_capability(&self, ctx: &Ctx, tx: &mut Tx, cap: Capability, inv: &Invocation, caller: Option<&str>) -> Result<Receipt> {
        if let Some(v) = &inv.version
            && v != &cap.version
        {
            return Err(refused(Code::CapabilityUnavailable, format!("{} is bound to version {v}; this workspace has {}", cap.id, cap.version)));
        }
        cap.check_input(&inv.input)?;
        if !cap.effect.changes() {
            let result = self.carry_out(ctx, tx, &cap, &inv.input, None).await?;
            return Ok(Receipt { capability: cap.id, version: cap.version, effect_key: None, repeated: false, result, answered_once: None });
        }
        let Some(key) = inv.effect_key.as_deref().filter(|k| !k.trim().is_empty()) else {
            return Err(refused(Code::SchemaInvalid, format!("{} changes something, so it needs a logical effect key", cap.id)));
        };
        // An action's secret parameters are digested only keyed with the workspace's key for receipts, from the vault.
        let action = match &cap.operation {
            Operation::Action(name) => crate::receipt::action_def(ctx, cap.entity.as_deref().unwrap_or_default(), name),
            _ => None,
        };
        let digest = match (action, inv.input.get("params"), inv.input.get("values")) {
            (Some(a), Some(params), _) if a.params.iter().any(|p| p.secret) => {
                let mut input = inv.input.clone();
                input["params"] = self.digestible_params(ctx, cap.entity.as_deref().unwrap_or_default(), &a.name, params).await?;
                cap.input_digest(&input)?
            }
            // A record's secret field values too (TP-S2): a receipt is kept for its retention, 400 days by default.
            (_, _, Some(values)) if matches!(cap.operation, Operation::Create | Operation::Update) => {
                let mut input = inv.input.clone();
                input["values"] = self.digestible_record(ctx, cap.entity.as_deref().unwrap_or_default(), values).await?;
                cap.input_digest(&input)?
            }
            _ => cap.input_digest(&inv.input)?,
        };
        let claimed = sqlx::query(
            "insert into logic_effect (tenant_id, environment, capability, effect_key, input_digest, version, capability_digest, actor, caller, retention_days) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) on conflict do nothing",
        )
        .bind(ctx.tenant)
        .bind(&inv.environment)
        .bind(&cap.id)
        .bind(key)
        .bind(&digest)
        .bind(&cap.version)
        .bind(&cap.digest)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(caller)
        // ponytail: a declared retention past a hundred years is kept a hundred years (the date must stay in range).
        .bind(cap.retention_days.min(36_500) as i32)
        .execute(&mut **tx)
        .await?
        .rows_affected()
            == 1;
        if !claimed {
            return self.repeat(ctx, tx, &cap, &inv.environment, key, &digest, caller).await;
        }
        let result = self.carry_out(&ctx.keyed(key), tx, &cap, &inv.input, Some(key)).await?;
        let id_of = |v: &Value| v.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        let mut record = id_of(&result);
        // The record a repeat re-reads is the answer's own: an action that made another entity's record says so
        // (`{entity, id}`); one answering with its own record, that; anything else is answered as it was kept.
        let entity = match &cap.operation {
            Operation::Action(_) => result
                .get("entity")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| cap.entity.clone().filter(|_| record.is_some() && record == id_of(&inv.input))),
            _ => cap.entity.clone(),
        };
        if entity.is_none() {
            record = None;
        }
        sqlx::query(
            "update logic_effect set result = $5, record_entity = $6, record_id = $7 \
             where tenant_id = $1 and environment = $2 and capability = $3 and effect_key = $4",
        )
        .bind(ctx.tenant)
        .bind(&inv.environment)
        .bind(&cap.id)
        .bind(key)
        .bind(action.map_or_else(|| result.clone(), |a| a.kept_result(&result)))
        .bind(record.and(entity))
        .bind(record)
        .execute(&mut **tx)
        .await?;
        Ok(Receipt { capability: cap.id, version: cap.version, effect_key: Some(key.to_string()), repeated: false, result, answered_once: None })
    }

    /// Receipts past the retention their capability declares (`retention_days`): hourly, from [`Kernel::maintenance`].
    pub(crate) async fn purge_receipts(&self) -> Result<u64> {
        self.db.purge(&[("logic_effect", "kept_until < now() at time zone 'UTC' and not ferrum_workspace_held(tenant_id)".into())]).await
    }

    /// The key was used before: the same input gets what was done, with the caller's own view of it;
    /// other input is refused, and so is a key another caller claimed (an app's keys are its own).
    async fn repeat(&self, ctx: &Ctx, tx: &mut Tx, cap: &Capability, environment: &str, key: &str, digest: &str, caller: Option<&str>) -> Result<Receipt> {
        let (was, result, entity, record, actor, by): (String, Value, Option<String>, Option<Uuid>, Option<Uuid>, Option<String>) = sqlx::query_as(
            "select input_digest, result, record_entity, record_id, actor, caller from logic_effect \
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
        if by.as_deref() != caller {
            return Err(refused(Code::InputChanged, format!("the key {key} was used for {} by another caller: use a new key", cap.id)));
        }
        // What someone else did is shown as the caller may see it now, never as its first caller saw it. An app repeating
        // its own key (checked just above) did it, whoever it acts for now.
        let shown = match (entity, record) {
            (Some(e), Some(id)) => self.get_tx(ctx, tx, &e, id, false).await.map(Value::Object).unwrap_or(Value::Null),
            _ if actor == Some(ctx.user) || caller.is_some() => result,
            _ => Value::Null,
        };
        let answered_once = match &cap.operation {
            Operation::Action(name) => {
                crate::receipt::action_def(ctx, cap.entity.as_deref().unwrap_or_default(), name).and_then(|a| a.answer_once().map(|o| o.told(&a.label)))
            }
            _ => None,
        };
        Ok(Receipt {
            capability: cap.id.clone(),
            version: cap.version.clone(),
            effect_key: Some(key.to_string()),
            repeated: true,
            result: shown,
            answered_once,
        })
    }

    /// The owning app's operation, through the kernel path a person's request takes.
    async fn carry_out(&self, ctx: &Ctx, tx: &mut Tx, cap: &Capability, input: &Value, key: Option<&str>) -> Result<Value> {
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
                let sort = input.get("sort").and_then(Value::as_str).map(str::to_string);
                // A caller's list: the one path, which refuses a filter or sort on a field the caller cannot read.
                let q = crate::query::ListQuery { filters, sort, limit: Some(limit), ..Default::default() };
                let page = self.list_tx(ctx, tx, entity, &q, crate::ctx::Action::Read).await?;
                Value::Array(page.rows.into_iter().map(Value::Object).collect())
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
                self.queue_mail_for(tx, ctx.tenant, text("to"), text("subject"), &crate::outbox::text_html(text("body")), &[], purpose).await?;
                json!({"queued": true})
            }
            Operation::Call => {
                let text = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                let c = crate::connections::Call {
                    connection: text("connection"),
                    operation: text("operation"),
                    inputs: input.get("inputs").cloned().unwrap_or(Value::Null),
                    key: key.map(str::to_string),
                    ..Default::default()
                };
                self.call_connection(ctx, None, &c).await?
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
