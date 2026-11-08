//! No-code automations: `event → condition (CEL) → actions`, run after commit from the outbox.
//! Each (automation, event) runs at most once: the run record commits with the action's effects.
//! An automation runs as the administrator who saved it, with their rights as they are when it runs:
//! once they are no longer an active administrator it does not run, and its runs say why, until an
//! administrator saves it again. One saved by no one (an app's) runs as the system.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::outbox::Event;
use crate::{Doc, Kernel};
use ferrum_meta::{Expr, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const MAX_DEPTH: u8 = 3;

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
    /// once per event. All fields are CEL expressions.
    Email { to: String, subject: String, body: String },
}

pub fn validate_actions(actions: &Value) -> Result<Vec<AutomationAction>> {
    let list: Vec<AutomationAction> = serde_json::from_value(actions.clone()).map_err(|e| Error::invalid(format!("actions: {e}")))?;
    if list.is_empty() || list.len() > 20 {
        return Err(Error::invalid("an automation needs 1 to 20 actions"));
    }
    for a in &list {
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
            AutomationAction::Email { to, subject, body } => vec![to, subject, body],
        };
        for e in exprs {
            Expr::compile(e).map_err(|m| Error::invalid(format!("expression '{e}': {m}")))?;
        }
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
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    let autos: Vec<Automation> = sqlx::query_as(
        "select id, condition, actions, created_by from automation where tenant_id = $1 and entity = $2 and event = $3 and is_active order by created_at",
    )
    .bind(ev.tenant_id)
    .bind(entity)
    .bind(event)
    .fetch_all(&mut *tx)
    .await?;
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
        let mut ctx = match runner(k, ev.tenant_id, &schema, a.created_by, format!("automation-{}-{}", a.id, ev.id)).await {
            Ok(ctx) => ctx,
            Err(why) => {
                // Recorded, so it is not tried again for this event, and an administrator can see why.
                let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
                sqlx::query(
                    "insert into automation_run (tenant_id, automation_id, outbox_id, status, error) values ($1, $2, $3, 'failed', $4) on conflict do nothing",
                )
                .bind(ev.tenant_id)
                .bind(a.id)
                .bind(ev.id)
                .bind(&why)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                tracing::warn!(automation = %a.id, event = ev.id, "automation not run: {why}");
                continue;
            }
        };
        ctx.depth = depth + 1;
        let user = ctx.user_value();
        let res: Result<&str> = async {
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            let done: Option<i32> = sqlx::query_scalar("select 1 from automation_run where tenant_id = $1 and automation_id = $2 and outbox_id = $3")
                .bind(ev.tenant_id)
                .bind(a.id)
                .bind(ev.id)
                .fetch_optional(&mut *tx)
                .await?;
            if done.is_some() {
                return Ok("already");
            }
            let pass = match &a.condition {
                Some(c) if !c.trim().is_empty() => matches!(eval(c, &cel_doc, &user)?, Value::Bool(true)),
                _ => true,
            };
            let status = if pass {
                for action in validate_actions(&a.actions)? {
                    execute(k, &ctx, &mut tx, entity, ev.record_id, &cel_doc, &user, action).await?;
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
        .await;
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
        AutomationAction::Email { to, subject, body } => {
            let to = as_text(eval(&to, doc, user)?);
            let subject = as_text(eval(&subject, doc, user)?);
            let body = as_text(eval(&body, doc, user)?);
            // Never sent from here: a later action could still roll the run back (FLC-HOOK-002).
            k.queue_mail(tx, ctx.tenant, &to, &subject, &body).await?;
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
    if !["created", "updated", "submitted", "cancelled", "deleted"].contains(&event.as_str()) {
        return Err(Error::field("event", "must be created, updated, submitted, cancelled or deleted"));
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
    sqlx::query(
        "insert into automation (tenant_id, id, name, entity, event, condition, actions, is_active, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict (tenant_id, id) do update set name = excluded.name, \
         entity = excluded.entity, event = excluded.event, condition = excluded.condition, actions = excluded.actions, \
         is_active = excluded.is_active, created_by = excluded.created_by, updated_at = now()",
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
