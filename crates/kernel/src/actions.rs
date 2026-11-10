//! Actions a workspace sets up by configuration (docs/api.md, "Configured actions"; docs/builder/decisions.md
//! BD-11): an action whose `effects` say what it does, so it needs no handler. Whoever runs it is checked as for
//! any action (the permission it names or a rule naming it, its record's scope, its document status and
//! condition), then against its `roles` and `refuse_if`; its effects then run in order in the action's one
//! transaction, as the system on the runner's behalf, each through the save pipeline every write uses.
//!
//! What keeps that from being an escape hatch: effects are automation actions (no code, no SQL), each save they
//! make runs every computed field, check, Logic rule, unique key and workflow rule of the record it writes, an
//! effect writes only the workspace's own fields or fields a person may type (never the platform's columns, a
//! package's read-only or computed fields, a secret or a field group), and no effect runs another action.

use crate::automation::{self, AutomationAction};
use crate::ctx::{Ctx, PermSet};
use crate::error::{Error, Result};
use crate::{Doc, Kernel};
use ferrum_meta::{ActionDef, EffEntity, FieldType, Schema, Scope};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// What is wrong with `schema`'s configured actions in workspace `tenant`, one line each, naming the entity, action
/// and effect. An `invoke` is checked against the catalog `schema` gives, as the system sees it.
pub fn problems(k: &Kernel, tenant: Uuid, schema: &Schema) -> Vec<String> {
    let system = std::cell::OnceCell::new();
    let system = || system.get_or_init(|| Ctx::system(tenant, Arc::new(schema.clone())));
    let mut out = Vec::new();
    for ent in schema.entities.values() {
        for a in ent.def.actions.iter().filter(|a| !a.effects.is_empty()) {
            for (i, raw) in a.effects.iter().enumerate() {
                if let Err(m) = check_effect(k, &system, schema, ent, raw) {
                    out.push(format!("{}: action {}, effect {}: {m}", ent.def.name, a.name, i + 1));
                }
            }
        }
    }
    out
}

fn check_effect<'a>(k: &Kernel, system: &dyn Fn() -> &'a Ctx, schema: &Schema, ent: &EffEntity, raw: &Value) -> std::result::Result<(), String> {
    let effect: AutomationAction = serde_json::from_value(raw.clone()).map_err(|e| e.to_string())?;
    let said = |e: Error| match e {
        Error::Validation(fs) => fs.into_iter().map(|f| f.message).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    };
    automation::check_action(&effect).map_err(said)?;
    automation::check_targets(schema, &ent.def.name, std::slice::from_ref(&effect)).map_err(said)?;
    let (target, values) = match &effect {
        AutomationAction::SetFields { values } => (Some(ent), values),
        AutomationAction::CreateDocument { entity, values } => (schema.entity(entity).map(|e| &**e), values),
        AutomationAction::UpdateLinked { link, values } => (ent.field(link).and_then(|f| f.to.as_deref()).and_then(|t| schema.entity(t)).map(|e| &**e), values),
        AutomationAction::Invoke { capability, input, .. } => return automation::check_invoke(k, system(), capability, input).map_err(said),
        AutomationAction::Notify { .. } | AutomationAction::Email { .. } => return Ok(()),
    };
    let Some(target) = target else { return Ok(()) };
    for name in values.keys() {
        writable(target, name)?;
    }
    Ok(())
}

/// Whether an effect, running as the system, may write `field` of `ent`: the workspace's own fields (any field
/// of a custom entity, an `x_` field of a packaged one; the fields a Logic derivation may fill) and fields a
/// person may type; never the platform's columns, a computed or rolled-up field, a secret, a package's read-only
/// field or a field in a permission group.
fn writable(ent: &EffEntity, field: &str) -> std::result::Result<(), String> {
    let label = &ent.def.label;
    if ferrum_meta::RESERVED_FIELDS.contains(&field) {
        return Err(format!("{field} is the platform's: no effect writes it"));
    }
    let Some(f) = ent.field(field) else { return Err(format!("{label} has no field {field}")) };
    if f.computed.is_some() || f.rollup.is_some() {
        return Err(format!("{label}.{field} is worked out, not written"));
    }
    if f.ftype == FieldType::Secret || f.ftype == FieldType::Table {
        return Err(format!("{label}.{field} is not written by an effect"));
    }
    let own = ent.custom || field.starts_with("x_");
    if !own && (f.read_only || f.read_only_if.is_some() || f.perm_group.is_some()) {
        return Err(format!("{label}.{field} is its package's to keep: an effect writes the workspace's own fields and those a person may type"));
    }
    Ok(())
}

/// Runs configured action `def` of `ent` on `doc` (record `id`, locked, its permission, scope, document status
/// and condition already checked as `ctx`): its roles and refusals, then its effects in order in `tx`, as the
/// system on the runner's behalf, and the audit entry naming the runner. Answers `{status?, made?}`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    ent: &Arc<EffEntity>,
    def: &ActionDef,
    id: Uuid,
    doc: &Doc,
    params: &Value,
) -> Result<Value> {
    let params = match params {
        Value::Null => json!({}),
        Value::Object(_) => params.clone(),
        _ => return Err(Error::invalid(format!("'{}' takes its parameters as an object", def.label))),
    };
    let user = ctx.user_value();
    let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
    for (expr, message) in ent.exprs.action_refusals.get(&def.name).into_iter().flatten() {
        let holds = expr.eval_with(&Scope { doc: &view, old: None, parent: None, user: &user }, &[("params", &params)]).map_err(Error::invalid)?;
        if holds == Value::Bool(true) {
            return Err(Error::Forbidden(message.clone()));
        }
    }
    // The system, acting for the runner: every right, read-only fields included, while the records it writes say
    // who ran it.
    let system = Ctx {
        is_system: true,
        is_admin: true,
        egress: None,
        scopes: BTreeMap::new(),
        perms: Arc::new(PermSet { admin: true, entities: BTreeMap::new() }),
        ..ctx.clone()
    };
    let effects: Vec<AutomationAction> = serde_json::from_value(Value::Array(def.effects.clone())).map_err(|e| Error::invalid(e.to_string()))?;
    // An `invoke` without a key of its own does its step once per action, record version (as pressed) and place.
    let origin = format!("action:{}.{}:{id}:v{}:", ent.def.name, def.name, doc.get("version").unwrap_or(&Value::Null));
    let mut made = Vec::new();
    let mut results = Vec::new();
    for (i, effect) in effects.into_iter().enumerate() {
        let creates = matches!(effect, AutomationAction::CreateDocument { .. });
        // Each effect sees the record as the ones before it left it, and what each before it gave as `results`.
        let now = k.get_tx(&system, tx, &ent.def.name, id, false).await?;
        let view = ent.cel_view(&Value::Object(now), &ctx.schema);
        let done = Value::Array(results.clone());
        let extra = [("params", &params), ("results", &done)];
        let out = automation::execute(k, &system, tx, &ent.def.name, Some(id), &view, &user, &extra, &format!("{origin}{i}"), effect).await?;
        if creates {
            made.push(out.clone());
        }
        results.push(out);
    }
    crate::audit::record(tx, ctx, &ent.def.name, id, &format!("action:{}", def.name), None, params).await?;
    let after = k.get_tx(&system, tx, &ent.def.name, id, false).await?;
    let mut answer = serde_json::Map::new();
    if let Some(s) = after.get("status").filter(|s| s.is_string()) {
        answer.insert("status".into(), s.clone());
    }
    if !made.is_empty() {
        answer.insert("made".into(), Value::Array(made));
    }
    Ok(Value::Object(answer))
}
