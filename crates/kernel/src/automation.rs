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
}

fn check_action(a: &AutomationAction) -> Result<()> {
    if let AutomationAction::Notify { to, .. } = a {
        let ok = to == "owner"
            || to.strip_prefix("role:").is_some_and(|r| !r.trim().is_empty())
            || to.strip_prefix("user:").is_some_and(|u| u.parse::<Uuid>().is_ok())
            || to.strip_prefix("field:").is_some_and(|f| !f.trim().is_empty());
        if !ok {
            return Err(Error::invalid("notify.to must be owner, role:<name>, user:<id> or field:<name>"));
        }
    }
    let exprs: Vec<&String> = match a {
        AutomationAction::SetFields { values } | AutomationAction::CreateDocument { values, .. } => values.values().collect(),
        AutomationAction::Notify { title, body, .. } => std::iter::once(title).chain(body.iter()).collect(),
        AutomationAction::Email { to, subject, body, template, .. } => {
            if let Some(t) = template {
                mail_template(t)?;
            }
            vec![to, subject, body]
        }
    };
    for e in exprs {
        Expr::compile(e).map_err(|m| Error::invalid(format!("expression '{e}': {m}")))?;
    }
    Ok(())
}

/// The HTML a mail action sends: its template when it has one, otherwise its body as escaped text. The one
/// place it is made, so that a preview and a send never differ. A refusal is a field error on `template` or `body`.
pub fn render_mail(body: &str, template: Option<&str>, doc: &Value, user: &Value) -> Result<String> {
    match template {
        Some(t) => mail_template(t)?
            .get_template("mail")
            .and_then(|m| m.render(minijinja::context! { doc => minijinja::Value::from_serialize(doc), user => minijinja::Value::from_serialize(user) }))
            .map_err(|e| Error::field("template", format!("e-mail template failed: {e}"))),
        None => Ok(crate::outbox::text_html(&as_text(eval(body, doc, user).map_err(|e| Error::field("body", e.to_string()))?))),
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
    Ok((render_mail(body.unwrap_or("''"), template, &doc, &ctx.user_value())?, id))
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

fn eval(src: &str, doc: &Value, user: &Value) -> Result<Value> {
    let expr = compiled(src)?;
    expr.eval(&Scope { doc, old: None, parent: None, user }).map_err(|e| Error::invalid(format!("'{src}': {e}")))
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
    let mut first_error = None;
    for a in autos {
        // Whom it would run as, worked out before its transaction holds anything.
        let who = runner(k, ev.tenant_id, &schema, a.created_by, format!("automation-{}-{}", a.id, ev.id)).await;
        let res = run_one(k, ev, entity, &a, &who, depth, ev.record_id, &cel_doc, true).await;
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
    run_one(k, ev, entity, &a, &who, depth, Some(record), &cel_doc, false).await.map(|_| ())
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
    cel_doc: &Value,
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
    let pass = match &a.condition {
        Some(c) if !c.trim().is_empty() => matches!(eval(c, cel_doc, &user)?, Value::Bool(true)),
        _ => true,
    };
    let status = if pass {
        for action in validate_actions(&a.actions)? {
            execute(k, &ctx, &mut tx, entity, record, cel_doc, &user, action).await?;
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

#[allow(clippy::too_many_arguments)]
async fn execute(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    record: Option<Uuid>,
    doc: &Value,
    user: &Value,
    action: AutomationAction,
) -> Result<()> {
    match action {
        AutomationAction::SetFields { values } => {
            let id = record.ok_or_else(|| Error::invalid("set_fields needs a record"))?;
            let mut patch = Doc::new();
            for (field, expr) in values {
                patch.insert(field, eval(&expr, doc, user)?);
            }
            k.update_tx(ctx, tx, entity, id, patch, None).await?;
        }
        AutomationAction::CreateDocument { entity: target, values } => {
            let mut input = Doc::new();
            for (field, expr) in values {
                input.insert(field, eval(&expr, doc, user)?);
            }
            k.insert_tx(ctx, tx, &target, input).await?;
        }
        AutomationAction::Notify { to, title, body } => {
            let title = as_text(eval(&title, doc, user)?);
            let body = body.map(|b| eval(&b, doc, user).map(as_text)).transpose()?;
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
            let to = as_text(eval(&to, doc, user)?);
            let subject = as_text(eval(&subject, doc, user)?);
            // The body is text: nothing a record holds becomes markup in a mail sent in the workspace's name. A
            // template's own markup is the administrator's; what it prints from the record is escaped.
            let body = render_mail(&body, template.as_deref(), doc, user)?;
            // Never sent from here: a later action could still roll the run back (FLC-HOOK-002).
            k.queue_mail_for(tx, ctx.tenant, &to, &subject, &body, &[], purpose).await?;
        }
    }
    Ok(())
}

/// Validates and stores an automation definition.
pub async fn save(k: &Kernel, ctx: &Ctx, input: &Map<String, Value>) -> Result<Uuid> {
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
        Expr::compile(c).map_err(|m| Error::field("condition", m))?;
    }
    let actions = input.get("actions").cloned().unwrap_or(Value::Null);
    validate_actions(&actions)?;
    let id = input.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or_else(Uuid::now_v7);
    let active = input.get("is_active").and_then(Value::as_bool).unwrap_or(true);
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    if active {
        // Never both: while the solution imported from it runs in its place, it stays off. Its row is locked first,
        // as an activation handing it over locks it, so the two cannot cross (FLC-MIG-003).
        sqlx::query("select 1 from automation where tenant_id = $1 and id = $2 for update").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
        let solution = crate::authoring::solution_of(id);
        let running: Option<i32> = sqlx::query_scalar("select 1 from logic_activation where tenant_id = $1 and solution = $2")
            .bind(ctx.tenant)
            .bind(&solution)
            .fetch_optional(&mut *tx)
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
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
            assert_eq!(eval(src, &serde_json::json!({"title": "A"}), &Value::Null).unwrap(), "A (compiled once)");
        }
        assert_eq!(COMPILES.load(Ordering::Relaxed) - before, 1);
    }
}
