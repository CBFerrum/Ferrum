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

use crate::automation::{self, AutomationAction, Step};
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
        // A configured action's effects, and a workflow transition's (BD-19), are checked alike.
        let actions = ent.def.actions.iter().map(|a| (format!("action {}", a.name), &a.effects));
        let transitions = ent.workflow.iter().flat_map(|w| &w.def.transitions).map(|t| (format!("transition {} from {}", t.action, t.from), &t.effects));
        for (what, effects) in actions.chain(transitions).filter(|(_, e)| !e.is_empty()) {
            let mut steps = Vec::new();
            for (i, raw) in effects.iter().enumerate() {
                match serde_json::from_value::<Step>(raw.clone()) {
                    Ok(s) => match check_effect(k, &system, schema, ent, &s) {
                        Ok(()) => steps.push(s),
                        Err(m) => out.push(format!("{}: {what}, effect {}: {m}", ent.def.name, i + 1)),
                    },
                    Err(e) => out.push(format!("{}: {what}, effect {}: {e}", ent.def.name, i + 1)),
                }
            }
            // What one effect reads from another (`copy_lines` from an earlier `find`) is checked across them.
            if steps.len() == effects.len()
                && let Err(Error::Validation(fs)) = automation::check_targets(schema, &ent.def.name, &steps)
            {
                for f in fs {
                    out.push(format!("{}: {what}, effect {}: {}", ent.def.name, f.row.map_or(0, |r| r.1) + 1, f.message));
                }
            }
        }
    }
    out
}

/// The message and document templates `schema`'s configured actions and transitions name that are not there, one
/// line each as [`problems`] says them.
pub(crate) async fn template_problems(tx: &mut crate::db::Tx, tenant: Uuid, schema: &Schema) -> Result<Vec<String>> {
    let known = automation::known_templates(tx, tenant, schema).await?;
    let mut out = Vec::new();
    for ent in schema.entities.values() {
        let actions = ent.def.actions.iter().map(|a| (format!("action {}", a.name), &a.effects));
        let transitions = ent.workflow.iter().flat_map(|w| &w.def.transitions).map(|t| (format!("transition {} from {}", t.action, t.from), &t.effects));
        for (what, effects) in actions.chain(transitions) {
            for (i, raw) in effects.iter().enumerate() {
                if let Some(m) = serde_json::from_value::<Step>(raw.clone()).ok().and_then(|s| automation::unknown_template(&s, &known)) {
                    out.push(format!("{}: {what}, effect {}: {m}", ent.def.name, i + 1));
                }
            }
        }
    }
    Ok(out)
}

fn check_effect<'a>(k: &Kernel, system: &dyn Fn() -> &'a Ctx, schema: &Schema, ent: &EffEntity, step: &Step) -> std::result::Result<(), String> {
    let said = |e: Error| match e {
        Error::Validation(fs) => fs.into_iter().map(|f| f.message).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    };
    automation::check_step(step).map_err(said)?;
    // Each effect alone; one reading an earlier one's result (`copy_lines` from a `find`) is checked with the others.
    if !matches!(step.action, AutomationAction::CopyLines { result: Some(_), .. }) {
        automation::check_targets(schema, &ent.def.name, std::slice::from_ref(step)).map_err(said)?;
    }
    if let AutomationAction::ForEach { effects, .. } = &step.action {
        return effects
            .iter()
            .enumerate()
            .try_for_each(|(j, e)| check_effect(k, system, schema, ent, e).map_err(|m| format!("for_each, effect {}: {m}", j + 1)));
    }
    let both: BTreeMap<String, String>;
    let (target, values) = match &step.action {
        // What it makes or changes is written as the system: fields an effect may write.
        AutomationAction::Find { entity, by, create, update } => {
            both = by.iter().filter(|_| create.is_some()).chain(create.iter().chain(update.iter()).flatten()).map(|(k, v)| (k.clone(), v.clone())).collect();
            (schema.entity(entity).map(|e| &**e), &both)
        }
        AutomationAction::CopyLines { .. } | AutomationAction::ForEach { .. } => return Ok(()),
        AutomationAction::SetFields { values } => (Some(ent), values),
        AutomationAction::CreateDocument { entity, values } => (schema.entity(entity).map(|e| &**e), values),
        AutomationAction::UpdateLinked { link, values } => (ent.field(link).and_then(|f| f.to.as_deref()).and_then(|t| schema.entity(t)).map(|e| &**e), values),
        AutomationAction::Invoke { capability, input, .. } => return automation::check_invoke(k, system(), capability, input).map_err(said),
        // What it names is the workspace's configuration, asked when it runs.
        AutomationAction::Call { .. } => return Ok(()),
        // Their table and line fields are checked with the targets above.
        AutomationAction::Notify { .. }
        | AutomationAction::Email { .. }
        | AutomationAction::Message { .. }
        | AutomationAction::Link { .. }
        | AutomationAction::Generate { .. }
        | AutomationAction::ReportDocuments { .. }
        | AutomationAction::AppendLine { .. }
        | AutomationAction::SetLine { .. } => {
            return Ok(());
        }
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
pub(crate) fn writable(ent: &EffEntity, field: &str) -> std::result::Result<(), String> {
    writable_as(ent, field, false)
}

/// [`writable`], a table field included when `lines` (an effect on one line of it, `append_line` or `set_line`).
pub(crate) fn writable_as(ent: &EffEntity, field: &str, lines: bool) -> std::result::Result<(), String> {
    let label = &ent.def.label;
    if ferrum_meta::RESERVED_FIELDS.contains(&field) {
        return Err(format!("{field} is the platform's: no effect writes it"));
    }
    let Some(f) = ent.field(field) else { return Err(format!("{label} has no field {field}")) };
    if f.computed.is_some() || f.rollup.is_some() || ent.target_keeping(field).is_some() {
        return Err(format!("{label}.{field} is worked out, not written"));
    }
    if lines && f.ftype != FieldType::Table {
        return Err(format!("{label}.{field} is not a table of lines"));
    }
    if f.ftype == FieldType::Secret || (f.ftype == FieldType::Table && !lines) {
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
    refuse_if(k, ctx, tx, ent, def, doc, &params).await?;
    // An `invoke` without a key of its own does its step once per action, record version (as pressed) and place.
    let origin = format!("action:{}.{}:{id}:v{}:", ent.def.name, def.name, doc.get("version").unwrap_or(&Value::Null));
    let made = run_effects(k, ctx, tx, ent, id, &def.effects, &params, &origin).await?;
    crate::audit::record(tx, ctx, &ent.def.name, id, &format!("action:{}", def.name), None, def.redacted(&params)).await?;
    let after = k.get_tx(&system(ctx), tx, &ent.def.name, id, false).await?;
    let mut answer = serde_json::Map::new();
    if let Some(s) = after.get("status").filter(|s| s.is_string()) {
        answer.insert("status".into(), s.clone());
    }
    if !made.is_empty() {
        answer.insert("made".into(), Value::Array(made));
    }
    Ok(Value::Object(answer))
}

/// Refuses action `def` on `doc` with `params` as `ctx` where one of its `refuse_if` holds, in its own words.
pub(crate) async fn refuse_if(k: &Kernel, ctx: &Ctx, tx: &mut crate::db::Tx, ent: &Arc<EffEntity>, def: &ActionDef, doc: &Doc, params: &Value) -> Result<()> {
    let refusals = ent.exprs.action_refusals.get(&def.name).map(Vec::as_slice).unwrap_or_default();
    if refusals.is_empty() {
        return Ok(());
    }
    let params = if params.is_null() { &json!({}) } else { params };
    let user = ctx.user_value();
    let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
    let links = crate::links::load(k, ctx, tx, ent, &view, &crate::links::used(refusals.iter().map(|(e, _)| e))).await?;
    for (expr, message) in refusals {
        let holds =
            expr.eval_with(&Scope { doc: &view, old: None, parent: None, user: &user }, &[("params", params), ("links", &links)]).map_err(Error::invalid)?;
        if holds == Value::Bool(true) {
            return Err(Error::Forbidden(crate::print::message(message, || crate::doc::told(ctx, ent, doc.clone(), Some(k)))));
        }
    }
    Ok(())
}

/// The system, acting for `ctx`'s person: every right, read-only fields included, while the records it writes say
/// who it acts for.
pub(crate) fn system(ctx: &Ctx) -> Ctx {
    Ctx {
        is_system: true,
        is_admin: true,
        egress: None,
        scopes: BTreeMap::new(),
        perms: Arc::new(PermSet { admin: true, entities: BTreeMap::new() }),
        ..ctx.clone()
    }
}

/// Runs `effects` (a configured action's or a transition's) on record `id` of `ent` in order in `tx`, as the system
/// on `ctx`'s behalf, each seeing `params`, what the ones before gave as `results`, and the record as they left it.
/// `origin` keys an `invoke` without a key of its own. The records they made.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_effects(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    ent: &Arc<EffEntity>,
    id: Uuid,
    effects: &[Value],
    params: &Value,
    origin: &str,
) -> Result<Vec<Value>> {
    let system = system(ctx);
    let effects: Vec<Step> = serde_json::from_value(Value::Array(effects.to_vec())).map_err(|e| Error::invalid(e.to_string()))?;
    let mut made = Vec::new();
    let mut results = Vec::new();
    for (i, effect) in effects.into_iter().enumerate() {
        let creates = matches!(effect.action, AutomationAction::CreateDocument { .. });
        // Each effect sees the record as the ones before it left it, and what each before it gave as `results`.
        let now = Value::Object(k.get_tx(&system, tx, &ent.def.name, id, false).await?);
        let view = ent.cel_view(&now, &ctx.schema);
        let done = Value::Array(results.clone());
        let links = crate::links::load(k, ctx, tx, ent, &view, &automation::links_used(&effect)?).await?;
        // The settings it reads, as the workspace has them now (BD-17).
        let user = crate::settings::with(k, ctx, tx, automation::settings_used(&effect)?).await?.user_value();
        let extra = [("params", params), ("results", &done), ("links", &links)];
        let what = effect.action.clone();
        let out = automation::run_step(k, &system, tx, &ent.def.name, Some(id), (&now, &view), &user, &extra, &format!("{origin}{i}"), effect)
            .await
            .map_err(|e| automation::at_effect(i, &what, e))?;
        if creates && !out.is_null() {
            made.push(out.clone());
        }
        results.push(out);
    }
    Ok(made)
}
