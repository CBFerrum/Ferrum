//! No-code automations: `event → condition (CEL) → actions`, run after commit from the outbox.
//! Each (automation, event) runs at most once: the run record commits with the action's effects.
//! The event is a record's (created, updated, submitted, cancelled, deleted) or, for an automation with a
//! schedule, a moment of a record coming due (`scheduled`, schedule.rs).
//! An automation runs as the administrator who saved it, with their rights as they are when it runs:
//! once they are no longer an active administrator it does not run, and its runs say why, until an
//! administrator saves it again. One saved by no one (an app's) runs as the system.

use crate::ctx::Ctx;
use crate::error::{Error, FieldError, Result};
use crate::outbox::Event;
use crate::{Doc, Kernel};
use ferrum_meta::{Expr, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const MAX_DEPTH: u8 = 3;
/// The largest mail template an automation keeps, as large as a saved view or preferences may be.
pub const MAX_TEMPLATE: usize = 64 * 1024;

/// A mail template, compiled in the print formats' engine: refused when too large or when it does not compile.
fn mail_template(src: &str) -> Result<minijinja::Environment<'_>> {
    if src.len() > MAX_TEMPLATE {
        return Err(Error::field("template", "an e-mail template is larger than 64 KB"));
    }
    let mut env = crate::print::environment();
    env.add_template("mail", src).map_err(|e| Error::field("template", format!("e-mail template: {e}")))?;
    Ok(env)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationAction {
    /// Set fields on the triggering record (draft documents and masters only).
    SetFields { values: BTreeMap<String, String> },
    /// Create a new record; each value is a CEL expression over `doc` (the triggering record).
    CreateDocument { entity: String, values: BTreeMap<String, String> },
    /// Set fields of the record a link field of the triggering record names, saved through the pipeline with that
    /// record's checks and rules; each value is a CEL expression over `doc` and `linked` (the record written, as it
    /// was). An empty link does nothing; a link to a record that is not there is an error.
    UpdateLinked { link: String, values: BTreeMap<String, String> },
    /// In-app notification. `to`: `owner`, `role:<name>`, `user:<uuid>`, or `field:<name>` — the
    /// user whose email is in that field of the record (an assignee, an employee).
    Notify {
        to: String,
        title: String,
        #[serde(default)]
        body: Option<String>,
    },
    /// E-mail, queued with the outbox in the run's transaction: it goes out only if the run commits,
    /// once per event. `to`, `subject` and `body` are CEL expressions; what `body` gives is text, escaped, its line
    /// breaks kept. A `template` (MiniJinja, the print formats' engine) makes formatted mail in its place: its own
    /// markup stays, every value it prints is escaped, and it sees the record as `doc` and the runner as `user`. Marketing (`purpose`) is not sent to someone who
    /// withdrew their consent, asked again as it goes (FEO-015); a transactional notice does not ask.
    Email {
        to: String,
        subject: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<String>,
        #[serde(default)]
        purpose: crate::retention::Purpose,
    },
    /// Another app's step through the capability gate, in the run's transaction (BD-13): `capability` a catalog id,
    /// `input` its inputs as CEL expressions, `key` the effect key as an expression. Without `key`, the run's own:
    /// the same run on the same record version does it once. It gives the receipt's result.
    Invoke {
        capability: String,
        #[serde(default)]
        input: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Adds one line at the end of `table`, a table field of the record, through the record's ordinary save; each
    /// value an expression. Every other line keeps its id and values.
    AppendLine { table: String, values: BTreeMap<String, String> },
    /// Changes the one line of `table` for which `where` holds (an expression over `row`, the line, besides what the
    /// values see), only the fields named; no line or more than one is refused. Through the record's ordinary save.
    SetLine {
        table: String,
        #[serde(rename = "where")]
        when: String,
        values: BTreeMap<String, String>,
    },
}

/// The expressions an action holds.
fn sources(a: &AutomationAction) -> Vec<&String> {
    match a {
        AutomationAction::SetFields { values }
        | AutomationAction::CreateDocument { values, .. }
        | AutomationAction::UpdateLinked { values, .. }
        | AutomationAction::AppendLine { values, .. } => values.values().collect(),
        AutomationAction::SetLine { when, values, .. } => std::iter::once(when).chain(values.values()).collect(),
        AutomationAction::Notify { title, body, .. } => std::iter::once(title).chain(body.iter()).collect(),
        AutomationAction::Email { to, subject, body, .. } => vec![to, subject, body],
        AutomationAction::Invoke { input, key, .. } => input.values().chain(key.iter()).collect(),
    }
}

/// The link fields an action's expressions read as `links.<field>` (BD-14).
pub(crate) fn links_used(a: &AutomationAction) -> Result<std::collections::BTreeSet<String>> {
    let exprs = sources(a).into_iter().map(|s| compiled(s)).collect::<Result<Vec<_>>>()?;
    Ok(crate::links::used(&exprs))
}

pub(crate) fn check_action(a: &AutomationAction) -> Result<()> {
    if let AutomationAction::Notify { to, .. } = a {
        let ok = to == "owner"
            || to.strip_prefix("role:").is_some_and(|r| !r.trim().is_empty())
            || to.strip_prefix("user:").is_some_and(|u| u.parse::<Uuid>().is_ok())
            || to.strip_prefix("field:").is_some_and(|f| !f.trim().is_empty());
        if !ok {
            return Err(Error::invalid("notify.to must be owner, role:<name>, user:<id> or field:<name>"));
        }
    }
    if let AutomationAction::Email { template: Some(t), .. } = a {
        mail_template(t)?;
    }
    for e in sources(a) {
        Expr::compile(e).map_err(|m| Error::invalid(format!("expression '{e}': {m}")))?;
    }
    Ok(())
}

/// The HTML a mail action sends: its template when it has one, otherwise its body as escaped text. The one
/// place it is made, so that a preview and a send never differ. A refusal is a field error on `template` or `body`.
/// `extra` are more variables, as for the mail's other expressions (a configured action's `params`).
pub fn render_mail(body: &str, template: Option<&str>, doc: &Value, user: &Value, extra: &[(&str, &Value)]) -> Result<String> {
    match template {
        Some(t) => {
            let mut vars: BTreeMap<&str, &Value> = extra.iter().copied().collect();
            vars.extend([("doc", doc), ("user", user)]);
            mail_template(t)?
                .get_template("mail")
                .and_then(|m| m.render(minijinja::Value::from_serialize(&vars)))
                .map_err(|e| Error::field("template", format!("e-mail template failed: {e}")))
        }
        None => Ok(crate::outbox::text_html(&as_text(eval_with(body, doc, user, extra).map_err(|e| Error::field("body", e.to_string()))?))),
    }
}

/// A mail as it would be sent, against one record of `entity`: the one named (read with the caller's rights), else
/// the one most recently changed that they may read, else an empty record. Sends and writes nothing.
pub async fn preview_mail(
    k: &Kernel,
    ctx: &Ctx,
    entity: &str,
    body: Option<&str>,
    template: Option<&str>,
    record: Option<Uuid>,
) -> Result<(String, Option<Uuid>)> {
    if !ctx.is_admin {
        return Err(Error::Forbidden("administrator access required".into()));
    }
    let ent = ctx.schema.entity(entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
    let found = match record {
        Some(id) => Some(k.get(ctx, entity, id, false).await?),
        None => {
            let q = crate::query::ListQuery { sort: Some("updated_at desc".into()), limit: Some(1), ..Default::default() };
            k.list(ctx, entity, &q).await?.rows.into_iter().next()
        }
    };
    let id = found.as_ref().and_then(|d| d.get("id")).and_then(Value::as_str).and_then(|s| s.parse().ok());
    let doc = ent.cel_view(&Value::Object(found.unwrap_or_default()), &ctx.schema);
    Ok((render_mail(body.unwrap_or("''"), template, &doc, &ctx.user_value(), &[])?, id))
}

pub fn validate_actions(actions: &Value) -> Result<Vec<AutomationAction>> {
    let list: Vec<AutomationAction> = serde_json::from_value(actions.clone()).map_err(|e| Error::invalid(format!("actions: {e}")))?;
    if list.is_empty() || list.len() > 20 {
        return Err(Error::invalid("an automation needs 1 to 20 actions"));
    }
    for (i, a) in list.iter().enumerate() {
        // A refusal is a field error on `actions`, in the row of the action it is about.
        check_action(a).map_err(|e| match e {
            Error::Validation(fs) => Error::Validation(fs.into_iter().map(|f| FieldError::new("actions", f.message).in_row("actions", i)).collect()),
            other => Error::Validation(vec![FieldError::new("actions", other.to_string()).in_row("actions", i)]),
        })?;
    }
    Ok(list)
}

/// Compiled expressions by their source: an automation's are compiled once, not at every run. Bounded:
/// emptied when it grows past what any workspace's automations need.
static COMPILED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Expr>>> = std::sync::LazyLock::new(Default::default);
const COMPILED_MAX: usize = 4096;
/// How many expressions were compiled, for the test that they are not compiled again.
static COMPILES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn compiled(src: &str) -> Result<Expr> {
    if let Some(e) = COMPILED.lock().unwrap_or_else(|p| p.into_inner()).get(src) {
        return Ok(e.clone());
    }
    let expr = Expr::compile(src).map_err(Error::invalid)?;
    COMPILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut cache = COMPILED.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= COMPILED_MAX {
        cache.clear();
    }
    cache.insert(src.to_string(), expr.clone());
    Ok(expr)
}

fn eval(src: &str, doc: &Value, old: Option<&Value>, user: &Value, extra: &[(&str, &Value)]) -> Result<Value> {
    compiled(src)?.eval_with(&Scope { doc, old, parent: None, user }, extra).map_err(|e| Error::invalid(format!("'{src}': {e}")))
}

/// [`eval`] with more variables: a configured action's `params`, the `linked` record an effect writes.
fn eval_with(src: &str, doc: &Value, user: &Value, extra: &[(&str, &Value)]) -> Result<Value> {
    let expr = compiled(src)?;
    expr.eval_with(&Scope { doc, old: None, parent: None, user }, extra).map_err(|e| Error::invalid(format!("'{src}': {e}")))
}

/// What an automation's (or a configured action's) actions write, checked against the entity they run on when
/// they are saved: `update_linked` names a link field of it, and every field written is a field of the record
/// written. A refusal names the action by its place.
pub(crate) fn check_targets(schema: &ferrum_meta::Schema, entity: &str, actions: &[AutomationAction]) -> Result<()> {
    let ent = schema.entity(entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
    for (i, a) in actions.iter().enumerate() {
        let at = |m: String| Error::Validation(vec![FieldError::new("actions", m).in_row("actions", i)]);
        // What it reads through `links` are link fields of the record it runs on (BD-14).
        if let Some(n) = links_used(a)?.into_iter().find(|n| linked_entity(ent, n).is_none()) {
            return Err(at(format!("links.{n}: {n} is not a link field of {}", ent.def.label)));
        }
        let (target, values) = match a {
            AutomationAction::SetFields { values } => (ent.clone(), values),
            AutomationAction::CreateDocument { entity, values } => {
                (schema.entity(entity).cloned().ok_or_else(|| at(format!("unknown entity {entity}")))?, values)
            }
            AutomationAction::UpdateLinked { link, values } => {
                let to = linked_entity(ent, link).ok_or_else(|| at(format!("{link} is not a link field of {}", ent.def.label)))?;
                (schema.entity(to).cloned().ok_or_else(|| at(format!("{link} links to {to}, which is not installed")))?, values)
            }
            AutomationAction::AppendLine { table, values } | AutomationAction::SetLine { table, values, .. } => {
                crate::actions::writable_as(ent, table, true).map_err(at)?;
                let of = ent.field(table).and_then(|f| f.of.as_deref()).and_then(|c| schema.entity(c)).cloned();
                let of = of.ok_or_else(|| at(format!("the lines of {table} are not installed")))?;
                if let Some(Err(m)) = values.keys().map(|f| crate::actions::writable(&of, f)).find(|r| r.is_err()) {
                    return Err(at(m));
                }
                continue;
            }
            AutomationAction::Notify { .. } | AutomationAction::Email { .. } | AutomationAction::Invoke { .. } => continue,
        };
        // The platform's own keys are ignored when written, as they always were (a configured action refuses them).
        if let Some(f) = values.keys().find(|f| target.field(f).is_none() && !crate::store::SYSTEM_KEYS.contains(&f.as_str())) {
            return Err(at(format!("{} has no field {f}", target.def.label)));
        }
    }
    Ok(())
}

/// A refusal that the capability is not in the catalog, said for whoever set the effect up.
fn gone(e: Error, id: &str) -> Error {
    match e.code() {
        "capability_unavailable" => ferrum_logic::refusal::Refusal::new(
            ferrum_logic::refusal::Code::CapabilityUnavailable,
            format!("{id} is not a capability here: is its app installed?"),
        )
        .into(),
        _ => e,
    }
}

/// An `invoke` checked against the catalog as `ctx` sees it: its capability is there, and its input names are the
/// capability's, the required ones given.
pub(crate) fn check_invoke(k: &Kernel, ctx: &Ctx, id: &str, input: &BTreeMap<String, String>) -> Result<()> {
    let cap = k.capability(ctx, id).map_err(|e| gone(e, id))?;
    // No effect runs another configured action (BD-11), so a button cannot loop through the gate.
    if let ferrum_logic::capability::Operation::Action(name) = &cap.operation
        && cap.entity.as_deref().and_then(|e| ctx.schema.entity(e)).is_some_and(|e| e.def.actions.iter().any(|a| &a.name == name && !a.effects.is_empty()))
    {
        return Err(Error::invalid(format!("{id} is a configured action: an effect does not run another one")));
    }
    let props = cap.input.get("properties").and_then(Value::as_object);
    if let Some(n) = input.keys().find(|n| props.is_none_or(|p| !p.contains_key(n.as_str()))) {
        return Err(Error::invalid(format!("{id} has no input {n}")));
    }
    let required = cap.input.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    if let Some(r) = required.into_iter().find(|r| !input.contains_key(*r)) {
        return Err(Error::invalid(format!("{id} needs its input {r}")));
    }
    Ok(())
}

/// The entity a link field of `ent` points at.
fn linked_entity<'a>(ent: &'a ferrum_meta::EffEntity, link: &str) -> Option<&'a str> {
    ent.field(link).filter(|f| f.ftype == ferrum_meta::FieldType::Link).and_then(|f| f.to.as_deref())
}

fn as_text(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[derive(sqlx::FromRow)]
struct Automation {
    id: Uuid,
    condition: Option<String>,
    actions: Value,
    created_by: Option<Uuid>,
    is_active: bool,
}

/// Whom an automation runs as: the administrator who saved it, read now; or why it may not run.
async fn runner(
    k: &Kernel,
    tenant: Uuid,
    schema: &std::sync::Arc<ferrum_meta::Schema>,
    made_by: Option<Uuid>,
    request_id: String,
) -> std::result::Result<Ctx, String> {
    let Some(user) = made_by else {
        let mut ctx = Ctx::system(tenant, schema.clone());
        ctx.request_id = request_id;
        return Ok(ctx);
    };
    match k.load_ctx(tenant, user, None, request_id).await {
        Ok(ctx) if ctx.is_admin => Ok(ctx),
        Ok(ctx) => Err(format!("{}, who saved it, is no longer an administrator: an administrator saves it again", ctx.email)),
        Err(_) => Err("whoever saved it can no longer sign in here: an administrator saves it again".into()),
    }
}

pub async fn run_for_event(k: &Kernel, ev: &Event) -> Result<()> {
    let Some((entity, event)) = ev.topic.split_once('.') else { return Ok(()) };
    let depth = ev.payload.get("depth").and_then(Value::as_u64).unwrap_or(0) as u8;
    if event == "scheduled" {
        return run_scheduled(k, ev, entity, depth).await;
    }
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    let autos: Vec<Automation> = sqlx::query_as(
        // The active ones, and those off because the solution imported from each runs in its place: their events
        // are handed over to it (FLC-MIG-003).
        "select id, condition, actions, created_by, is_active from automation where tenant_id = $1 and entity = $2 and event = $3 \
           and (is_active or exists (select 1 from logic_activation a where a.tenant_id = automation.tenant_id \
             and a.solution = 'workspace.automation_' || replace(automation.id::text, '-', ''))) order by created_at",
    )
    .bind(ev.tenant_id)
    .bind(entity)
    .bind(event)
    .fetch_all(&mut *tx)
    .await?;
    if depth >= MAX_DEPTH && autos.iter().any(|a| a.is_active) {
        // Not run, and recorded so an administrator sees which automations answer one another's changes.
        for a in autos.iter().filter(|a| a.is_active) {
            sqlx::query(
                "insert into automation_run (tenant_id, automation_id, outbox_id, status, error) values ($1, $2, $3, 'failed', $4) on conflict do nothing",
            )
            .bind(ev.tenant_id)
            .bind(a.id)
            .bind(ev.id)
            .bind(format!(
                "not run: automations made this change after {depth} others in one chain, and stop at {MAX_DEPTH}; one of them answers its own changes"
            ))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::warn!(event = ev.id, topic = %ev.topic, "automations stopped at depth {depth}");
        return Ok(());
    }
    tx.commit().await?;
    if autos.is_empty() {
        return Ok(());
    }
    let schema = k.registry.schema(ev.tenant_id).await?;
    let Some(ent) = schema.entity(entity).cloned() else { return Ok(()) };
    let doc = ev.payload.get("doc").cloned().unwrap_or(Value::Null);
    let cel_doc = ent.cel_view(&doc, &schema);
    // The record before the change: the record now with what the change replaced (`was`) put back.
    let cel_old = ev.payload.get("was").and_then(Value::as_object).map(|was| {
        let mut old = doc.clone();
        for (k, v) in was {
            old[k] = v.clone();
        }
        ent.cel_view(&old, &schema)
    });
    let mut first_error = None;
    for a in autos {
        // Whom it would run as, worked out before its transaction holds anything.
        let who = runner(k, ev.tenant_id, &schema, a.created_by, format!("automation-{}-{}", a.id, ev.id)).await;
        let res = run_one(k, ev, entity, &a, &who, depth, ev.record_id, (&cel_doc, cel_old.as_ref()), true).await;
        if let Err(e) = res {
            tracing::warn!(automation = %a.id, event = ev.id, "automation failed: {e}");
            first_error.get_or_insert(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A moment of a record come due (schedule.rs): only the automation it names, with the record as it is now,
/// read with its saver's rights. Nothing happens when the automation is off or gone, when the record is gone
/// or no longer readable, or when the moment is no longer one of the record's (its date moved, it no longer
/// passes the filter); otherwise it runs as for a record's event, its changes one step further in a chain.
async fn run_scheduled(k: &Kernel, ev: &Event, entity: &str, depth: u8) -> Result<()> {
    let auto = ev.payload.get("automation").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
    let due = ev.payload.get("due_at").and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    let (Some(auto), Some(due), Some(record)) = (auto, due, ev.record_id) else { return Ok(()) };
    let due = due.to_utc();
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    let found: Option<(Uuid, Option<String>, Value, Option<Uuid>, bool, Value)> = sqlx::query_as(
        "select id, condition, actions, created_by, is_active, schedule from automation \
         where tenant_id = $1 and id = $2 and entity = $3 and event = 'scheduled'",
    )
    .bind(ev.tenant_id)
    .bind(auto)
    .bind(entity)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some((id, condition, actions, created_by, is_active, schedule)) = found else { return Ok(()) };
    if !is_active {
        return Ok(());
    }
    let a = Automation { id, condition, actions, created_by, is_active };
    let schema = k.registry.schema(ev.tenant_id).await?;
    let Some(ent) = schema.entity(entity).cloned() else { return Ok(()) };
    let who = runner(k, ev.tenant_id, &schema, a.created_by, format!("automation-{}-{}", a.id, ev.id)).await;
    let mut cel_doc = Value::Null;
    if let Ok(ctx) = &who {
        let doc = match k.get(ctx, entity, record, false).await {
            Ok(doc) => doc,
            Err(Error::NotFound(_) | Error::Forbidden(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let Ok(plan) = crate::schedule::plan(&schema, &ent, &schedule) else { return Ok(()) };
        if !crate::schedule::still_due(k, ev.tenant_id, &schema, &ent, &plan, record, due).await? {
            return Ok(());
        }
        cel_doc = ent.cel_view(&Value::Object(doc), &schema);
    }
    run_one(k, ev, entity, &a, &who, depth, Some(record), (&cel_doc, None), false).await.map(|_| ())
}

/// One automation's run for one event, in its own transaction: nothing if it is off (or the event was handed
/// over to the solution imported from it) or ran for this event already; otherwise its condition, then its
/// actions, recorded with their effects.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    k: &Kernel,
    ev: &Event,
    entity: &str,
    a: &Automation,
    who: &std::result::Result<Ctx, String>,
    depth: u8,
    record: Option<Uuid>,
    (cel_doc, cel_old): (&Value, Option<&Value>),
    hand_over: bool,
) -> Result<&'static str> {
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    // Read again under its lock. An activation handing it over to the solution imported from it waits for
    // this to finish, or went first, and then the event is the solution's: claimed and started here unless
    // the solution's own subscriber claimed it already. Each event is run by one of the two (FLC-MIG-003).
    let active: Option<bool> = sqlx::query_scalar("select is_active from automation where tenant_id = $1 and id = $2 for share")
        .bind(ev.tenant_id)
        .bind(a.id)
        .fetch_optional(&mut *tx)
        .await?;
    if active != Some(true) {
        let handed = hand_over && k.hand_over_automation_event(&mut tx, ev, a.id).await?;
        tx.commit().await?;
        return Ok(if handed { "handed over" } else { "off" });
    }
    let done: Option<i32> = sqlx::query_scalar("select 1 from automation_run where tenant_id = $1 and automation_id = $2 and outbox_id = $3")
        .bind(ev.tenant_id)
        .bind(a.id)
        .bind(ev.id)
        .fetch_optional(&mut *tx)
        .await?;
    if done.is_some() {
        return Ok("already");
    }
    let mut ctx = match who {
        Ok(ctx) => ctx.clone(),
        Err(why) => {
            // Recorded, so it is not tried again for this event, and an administrator can see why.
            sqlx::query(
                "insert into automation_run (tenant_id, automation_id, outbox_id, status, error) values ($1, $2, $3, 'failed', $4) on conflict do nothing",
            )
            .bind(ev.tenant_id)
            .bind(a.id)
            .bind(ev.id)
            .bind(why)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            tracing::warn!(automation = %a.id, event = ev.id, "automation not run: {why}");
            return Ok("not run");
        }
    };
    ctx.depth = depth + 1;
    let user = ctx.user_value();
    // The linked records its condition and actions read, once for the run (BD-14).
    let condition = a.condition.as_deref().filter(|c| !c.trim().is_empty());
    let mut names = condition.map(|c| compiled(c).map(|e| crate::links::used([&e]))).transpose()?.unwrap_or_default();
    for action in validate_actions(&a.actions).into_iter().flatten() {
        names.extend(links_used(&action)?);
    }
    let links = match ctx.schema.entity(entity).cloned() {
        Some(ent) => crate::links::load(k, &ctx, &mut tx, &ent, cel_doc, &names).await?,
        None => Value::Object(Map::new()),
    };
    let pass = match condition {
        Some(c) => matches!(eval(c, cel_doc, cel_old, &user, &[("links", &links)])?, Value::Bool(true)),
        _ => true,
    };
    let status = if pass {
        // An `invoke` without a key of its own does its step once per automation, record version and place.
        let origin = format!("automation:{}:{}:v{}:", a.id, record.map(|r| r.to_string()).unwrap_or_default(), cel_doc.get("version").unwrap_or(&Value::Null));
        let mut results = Vec::new();
        for (i, action) in validate_actions(&a.actions)?.into_iter().enumerate() {
            let done = Value::Array(results.clone());
            results.push(
                execute(k, &ctx, &mut tx, entity, record, cel_doc, &user, &[("results", &done), ("links", &links)], &format!("{origin}{i}"), action).await?,
            );
        }
        "done"
    } else {
        "skipped"
    };
    sqlx::query("insert into automation_run (tenant_id, automation_id, outbox_id, status) values ($1, $2, $3, $4)")
        .bind(ev.tenant_id)
        .bind(a.id)
        .bind(ev.id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(status)
}

/// Runs one action as `ctx` in `tx`, its expressions over `doc` (the record as CEL sees it), `user` and `extra`
/// (a configured action's `params`, earlier effects' `results`). Returns the record it made or wrote,
/// `{entity, id, number}`, an `invoke`'s result (its effect key `key` unless it names one), or null.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    record: Option<Uuid>,
    doc: &Value,
    user: &Value,
    extra: &[(&str, &Value)],
    key: &str,
    action: AutomationAction,
) -> Result<Value> {
    let eval = |src: &str| eval_with(src, doc, user, extra);
    let made = |entity: &str, d: &Doc| serde_json::json!({"entity": entity, "id": d.get("id"), "number": d.get("number")});
    match action {
        AutomationAction::SetFields { values } => {
            let id = record.ok_or_else(|| Error::invalid("set_fields needs a record"))?;
            let mut patch = Doc::new();
            for (field, expr) in values {
                patch.insert(field, eval(&expr)?);
            }
            k.update_tx(ctx, tx, entity, id, patch, None).await?;
        }
        AutomationAction::CreateDocument { entity: target, values } => {
            let mut input = Doc::new();
            for (field, expr) in values {
                input.insert(field, eval(&expr)?);
            }
            let d = k.insert_tx(ctx, tx, &target, input).await?;
            return Ok(made(&target, &d));
        }
        AutomationAction::UpdateLinked { link, values } => {
            let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
            let target = linked_entity(&ent, &link).ok_or_else(|| Error::invalid(format!("update_linked: {link} is not a link field of {}", ent.def.label)))?;
            let tent = ctx.schema.entity(target).cloned().ok_or_else(|| Error::invalid(format!("update_linked: {target} is not installed")))?;
            // An empty link names nothing to write.
            let Some(id) = doc.get(&link).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(Value::Null) };
            // Locked for the rest of the transaction, as the save below locks it.
            let linked = match k.get_tx(ctx, tx, target, id, true).await {
                Ok(d) => d,
                Err(Error::NotFound(_)) => return Err(Error::invalid(format!("{link}: the {} it names does not exist", tent.def.label))),
                Err(e) => return Err(e),
            };
            let seen = tent.cel_view(&Value::Object(linked), &ctx.schema);
            let mut vars = extra.to_vec();
            vars.push(("linked", &seen));
            let mut patch = Doc::new();
            for (field, expr) in values {
                patch.insert(field, eval_with(&expr, doc, user, &vars)?);
            }
            let d = k.update_tx(ctx, tx, target, id, patch, None).await?;
            return Ok(made(target, &d));
        }
        AutomationAction::Notify { to, title, body } => {
            let title = as_text(eval(&title)?);
            let body = body.map(|b| eval(&b).map(as_text)).transpose()?;
            let users: Vec<Uuid> = if to == "owner" {
                doc.get("created_by").and_then(Value::as_str).and_then(|s| s.parse().ok()).into_iter().collect()
            } else if let Some(role) = to.strip_prefix("role:") {
                sqlx::query_scalar("select user_id from user_role where tenant_id = $1 and role = $2 and (expires_at is null or expires_at > now())")
                    .bind(ctx.tenant)
                    .bind(role)
                    .fetch_all(&mut **tx)
                    .await?
            } else if let Some(u) = to.strip_prefix("user:") {
                vec![u.parse().map_err(|_| Error::invalid("notify: bad user id"))?]
            } else if let Some(field) = to.strip_prefix("field:") {
                match doc.get(field).and_then(Value::as_str).map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()) {
                    Some(email) => {
                        sqlx::query_scalar("select id from app_user where tenant_id = $1 and lower(email) = $2 and is_active")
                            .bind(ctx.tenant)
                            .bind(email)
                            .fetch_all(&mut **tx)
                            .await?
                    }
                    None => vec![],
                }
            } else {
                return Err(Error::invalid("notify.to must be owner, role:<name>, user:<id> or field:<name>"));
            };
            for u in users {
                crate::notify::send_kind(tx, ctx.tenant, u, "automation", "inapp", &title, body.as_deref(), Some(entity), record).await?;
            }
        }
        AutomationAction::Email { to, subject, body, template, purpose } => {
            let to = as_text(eval(&to)?);
            let subject = as_text(eval(&subject)?);
            // The body is text: nothing a record holds becomes markup in a mail sent in the workspace's name. A
            // template's own markup is the administrator's; what it prints from the record is escaped.
            let body = render_mail(&body, template.as_deref(), doc, user, extra)?;
            // Never sent from here: a later action could still roll the run back (FLC-HOOK-002).
            k.queue_mail_for(tx, ctx.tenant, &to, &subject, &body, &[], purpose).await?;
        }
        AutomationAction::Invoke { capability, input, key: named } => {
            // Checked again as it runs: the capability's app may have gone since, or its action been configured.
            check_invoke(k, ctx, &capability, &input)?;
            let mut values = Map::new();
            for (name, expr) in input {
                values.insert(name, eval(&expr)?);
            }
            let key = match named {
                Some(src) => as_text(eval(&src)?),
                None => key.to_string(),
            };
            let inv = crate::capability::Invocation::new(capability.as_str(), Value::Object(values)).key(key);
            // Boxed: an app's action reached through the gate is an action, which may run effects of its own.
            return Ok(Box::pin(k.invoke_tx(ctx, tx, &inv)).await?.result);
        }
        AutomationAction::AppendLine { table, values } => {
            let id = record.ok_or_else(|| Error::invalid("append_line needs a record"))?;
            return write_line(k, ctx, tx, entity, id, doc, user, extra, &table, None, values).await;
        }
        AutomationAction::SetLine { table, when, values } => {
            let id = record.ok_or_else(|| Error::invalid("set_line needs a record"))?;
            return write_line(k, ctx, tx, entity, id, doc, user, extra, &table, Some(&when), values).await;
        }
    }
    Ok(Value::Null)
}

/// `append_line` (no `when`) or `set_line` on record `id`: through the record's ordinary save, with a patch that names
/// every stored line by its id alone, so each keeps what it holds, and gives only the one line its named fields.
#[allow(clippy::too_many_arguments)]
async fn write_line(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    id: Uuid,
    doc: &Value,
    user: &Value,
    extra: &[(&str, &Value)],
    table: &str,
    when: Option<&str>,
    values: BTreeMap<String, String>,
) -> Result<Value> {
    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
    let field = ent.field(table).filter(|f| f.ftype == ferrum_meta::FieldType::Table);
    let cent = field.and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).cloned();
    let cent = cent.ok_or_else(|| Error::invalid(format!("{table} is not a table field of {}", ent.def.label)))?;
    let stored = k.get_tx(ctx, tx, entity, id, false).await?;
    let rows = stored.get(table).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut lines: Vec<Value> = rows.iter().map(|r| serde_json::json!({"id": r.get("id")})).collect();
    let (at, row) = match when {
        None => (lines.len(), None),
        Some(w) => {
            let mut hits = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                let row = cent.cel_view(r, &ctx.schema);
                let vars: Vec<(&str, &Value)> = extra.iter().copied().chain([("row", &row)]).collect();
                if eval_with(w, doc, user, &vars)? == Value::Bool(true) {
                    hits.push(i);
                }
            }
            match hits[..] {
                [i] => (i, Some(cent.cel_view(&rows[i], &ctx.schema))),
                [] => {
                    return Err(Error::invalid(format!("set_line: no line of {} has {w}", ent.field(table).and_then(|f| f.label.as_deref()).unwrap_or(table))));
                }
                _ => {
                    let which = hits.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(", ");
                    return Err(Error::invalid(format!("set_line: lines {which} of {table} all have {w}; it changes one line")));
                }
            }
        }
    };
    let vars: Vec<(&str, &Value)> = extra.iter().copied().chain(row.as_ref().map(|r| ("row", r))).collect();
    let mut line = Map::new();
    for (f, src) in values {
        line.insert(f, eval_with(&src, doc, user, &vars)?);
    }
    match lines.get_mut(at) {
        Some(Value::Object(kept)) => kept.extend(line),
        _ => lines.push(Value::Object(line)),
    }
    let d = k.update_tx(ctx, tx, entity, id, Doc::from_iter([(table.to_string(), Value::Array(lines))]), None).await?;
    Ok(serde_json::json!({"entity": entity, "id": d.get("id"), "number": d.get("number")}))
}

/// Validates and stores an automation definition.
pub async fn save(k: &Kernel, ctx: &Ctx, input: &Map<String, Value>) -> Result<Uuid> {
    // What an `invoke` names is checked against the catalog here, where the kernel is at hand.
    // ponytail: a pack's install (save_in) skips this; its invoke is refused when it runs instead.
    if ctx.is_admin
        && let Ok(list) = validate_actions(input.get("actions").unwrap_or(&Value::Null))
    {
        for (i, a) in list.iter().enumerate() {
            if let AutomationAction::Invoke { capability, input, .. } = a {
                check_invoke(k, ctx, capability, input).map_err(|e| Error::Validation(vec![FieldError::new("actions", e.to_string()).in_row("actions", i)]))?;
            }
        }
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let id = save_in(ctx, &mut tx, input).await?;
    tx.commit().await?;
    Ok(id)
}

/// [`save`] in the caller's transaction (a pack's install, pack.rs).
pub(crate) async fn save_in(ctx: &Ctx, tx: &mut crate::db::Tx, input: &Map<String, Value>) -> Result<Uuid> {
    // What it says runs with an administrator's rights (its saver's), so saying it takes one.
    if !ctx.is_admin {
        return Err(Error::Forbidden("administrator access required".into()));
    }
    let get = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    let name = get("name").filter(|n| !n.trim().is_empty()).ok_or_else(|| Error::field("name", "is required"))?;
    let entity = get("entity").ok_or_else(|| Error::field("entity", "is required"))?;
    if ctx.schema.entity(&entity).is_none() {
        return Err(Error::field("entity", "unknown entity"));
    }
    let event = get("event").ok_or_else(|| Error::field("event", "is required"))?;
    if !["created", "updated", "submitted", "cancelled", "deleted", "scheduled"].contains(&event.as_str()) {
        return Err(Error::field("event", "must be created, updated, submitted, cancelled, deleted or scheduled"));
    }
    // A schedule in place of a record event, checked against the entity it is on (schedule.rs).
    let schedule = input.get("schedule").filter(|v| !v.is_null()).cloned();
    match (&schedule, event == "scheduled") {
        (Some(s), true) => {
            let ent = ctx.schema.entity(&entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
            crate::schedule::plan(&ctx.schema, ent, s)?;
        }
        (None, true) => return Err(Error::field("schedule", "a scheduled automation needs a schedule")),
        (Some(_), false) => return Err(Error::field("schedule", "only an automation whose event is scheduled has a schedule")),
        (None, false) => {}
    }
    let condition = get("condition").filter(|c| !c.trim().is_empty());
    if let Some(c) = &condition {
        let compiled = Expr::compile(c).map_err(|m| Error::field("condition", m))?;
        let ent = ctx.schema.entity(&entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
        if let Some(n) = compiled.links().iter().find(|n| linked_entity(ent, n).is_none()) {
            return Err(Error::field("condition", format!("links.{n}: {n} is not a link field of {}", ent.def.label)));
        }
    }
    let actions = input.get("actions").cloned().unwrap_or(Value::Null);
    check_targets(&ctx.schema, &entity, &validate_actions(&actions)?)?;
    let id = input.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or_else(Uuid::now_v7);
    let active = input.get("is_active").and_then(Value::as_bool).unwrap_or(true);
    if active {
        // Never both: while the solution imported from it runs in its place, it stays off. Its row is locked first,
        // as an activation handing it over locks it, so the two cannot cross (FLC-MIG-003).
        sqlx::query("select 1 from automation where tenant_id = $1 and id = $2 for update").bind(ctx.tenant).bind(id).execute(&mut **tx).await?;
        let solution = crate::authoring::solution_of(id);
        let running: Option<i32> = sqlx::query_scalar("select 1 from logic_activation where tenant_id = $1 and solution = $2")
            .bind(ctx.tenant)
            .bind(&solution)
            .fetch_optional(&mut **tx)
            .await?;
        if running.is_some() {
            return Err(Error::Conflict(format!(
                "{solution}, imported from this automation, runs in its place: deactivate it before turning the automation on again"
            )));
        }
    }
    // A schedule starts from now: when it is new, changed (or its entity), or switched on again, no moment
    // already past fires.
    sqlx::query(
        "insert into automation (tenant_id, id, name, entity, event, condition, actions, is_active, created_by, schedule, scanned_until) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, case when $10::jsonb is null then null else now() end) \
         on conflict (tenant_id, id) do update set name = excluded.name, \
         entity = excluded.entity, event = excluded.event, condition = excluded.condition, actions = excluded.actions, \
         is_active = excluded.is_active, created_by = excluded.created_by, updated_at = now(), schedule = excluded.schedule, \
         scanned_until = case when automation.schedule is not distinct from excluded.schedule and automation.entity = excluded.entity \
           and (automation.is_active or not excluded.is_active) then automation.scanned_until else excluded.scanned_until end, \
         scan_to = case when automation.schedule is not distinct from excluded.schedule and automation.entity = excluded.entity \
           and (automation.is_active or not excluded.is_active) then automation.scan_to end, \
         scan_cursor = case when automation.schedule is not distinct from excluded.schedule and automation.entity = excluded.entity \
           and (automation.is_active or not excluded.is_active) then automation.scan_cursor end",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(name)
    .bind(entity)
    .bind(event)
    .bind(condition)
    .bind(actions)
    .bind(active)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .bind(schedule)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn an_expression_is_compiled_once() {
        let src = "doc.title + ' (compiled once)'";
        let before = COMPILES.load(Ordering::Relaxed);
        for _ in 0..3 {
            assert_eq!(eval(src, &serde_json::json!({"title": "A"}), None, &Value::Null, &[]).unwrap(), "A (compiled once)");
        }
        assert_eq!(COMPILES.load(Ordering::Relaxed) - before, 1);
    }
}
