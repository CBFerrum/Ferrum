//! `GET /api/work`: Home's My work from the platform (docs/plan/apps-ui.md §11.8): what is assigned
//! to the caller, records waiting for their approval, and mentions they have not read. Apps add
//! theirs through `shell.work` providers, which Home asks itself. A source that fails is left out
//! (`partial`), never the whole answer.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::app_state;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error, Kernel};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde_json::{Value, json};
use uuid::Uuid;

/// The most of each kind Home is sent.
const MOST: usize = 50;

fn open(entity: &str, id: &str) -> String {
    format!("/open/{entity}/{id}")
}

fn cut(s: &str, n: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= n { line } else { format!("{}…", line.chars().take(n - 1).collect::<String>()) }
}

async fn mentions(k: &Kernel, ctx: &Ctx) -> ferrum_kernel::Result<Vec<Value>> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    type Row = (Uuid, String, Option<String>, Option<String>, Option<Uuid>, chrono::DateTime<chrono::Utc>);
    let rows: Vec<Row> = sqlx::query_as(
        "select id, title, body, entity, record_id, created_at from notification \
         where tenant_id = $1 and user_id = $2 and kind = 'mention' and read_at is null order by created_at desc limit $3",
    )
    .bind(ctx.tenant)
    .bind(ctx.user)
    .bind(MOST as i64)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(id, title, body, entity, record, at)| {
            let url = match (entity, record) {
                (Some(e), Some(r)) => Some(open(&e, &r.to_string())),
                _ => None,
            };
            json!({"kind": "mention", "id": id, "notification": id, "title": title, "subtitle": body.as_deref().map(|b| cut(b, 140)), "url": url, "at": at})
        })
        .collect())
}

async fn work(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(work_of(&app_state(&app)?.kernel, &ctx).await).into_response())
}

/// What `GET /api/work` answers (callable without the HTTP layer).
pub async fn work_of(k: &Kernel, ctx: &Ctx) -> Value {
    let ctx = ctx.clone();
    let (assigned, approvals, mentioned, requests) =
        tokio::join!(k.my_assignments(&ctx, None), k.my_approvals(&ctx, MOST), mentions(k, &ctx), k.my_approval_requests(&ctx, MOST as i64));
    let mut partial = false;
    let mut ok = |r: Result<Vec<Value>, Error>, what: &str| {
        r.unwrap_or_else(|e| {
            tracing::warn!(source = what, "my work: {e}");
            partial = true;
            Vec::new()
        })
    };
    let assigned: Vec<Value> = ok(assigned, "assignments")
        .into_iter()
        .take(MOST)
        .filter_map(|a| {
            let (entity, record) = (a["entity"].as_str()?, a["record_id"].as_str()?);
            let label = a["entity_label"].as_str().unwrap_or(entity);
            let subtitle = match a["note"].as_str().filter(|n| !n.trim().is_empty()) {
                Some(note) => format!("{label} · {}", cut(note, 100)),
                None => label.to_string(),
            };
            Some(json!({
                "kind": "assignment", "id": a["id"], "assignment": a["id"], "title": a["title"], "subtitle": subtitle, "url": open(entity, record),
                "due": a["due_date"], "priority": a["priority"], "at": a["created_at"],
            }))
        })
        .collect();
    let approvals: Vec<Value> = ok(approvals, "approvals")
        .into_iter()
        .filter_map(|a| {
            let (entity, id) = (a["entity"].as_str()?, a["id"].as_str()?);
            Some(json!({
                "kind": "approval", "id": format!("{entity}/{id}"), "title": a["title"],
                "subtitle": format!("{} · {}", a["entity_label"].as_str().unwrap_or(entity), a["state"].as_str().unwrap_or_default()),
                "url": open(entity, id), "actions": a["actions"], "at": a["updated_at"],
            }))
        })
        .collect();
    // Approval requests (BD-19) awaiting the caller's decision, after the transitions they may take.
    let requests = requests.map(|r| r["to_decide"].as_array().cloned().unwrap_or_default());
    let approvals: Vec<Value> = approvals
        .into_iter()
        .chain(ok(requests, "approval requests").into_iter().filter_map(|a| {
            let (entity, id) = (a["entity"].as_str()?, a["record"].as_str()?);
            Some(json!({
                "kind": "approval_request", "id": a["request"], "title": a["title"],
                "subtitle": format!("{} · {}", a["entity_label"].as_str().unwrap_or(entity), a["action"].as_str().unwrap_or_default()),
                "url": open(entity, id), "at": a["requested_at"],
            }))
        }))
        .collect();
    let mentioned = ok(mentioned, "mentions");
    json!({
        "groups": [
            {"id": "assigned", "title": "Assigned to you", "items": assigned},
            {"id": "approvals", "title": "Waiting for your approval", "items": approvals},
            {"id": "mentions", "title": "Mentions", "items": mentioned},
        ],
        "partial": partial,
    })
}

/// `GET /api/approvals`: the caller's approvals, `to_decide` (theirs, or of whom they stand in for) and
/// `awaiting_others` (their own requests still pending) (BD-19).
async fn approvals(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    Ok(Json(app_state(&app)?.kernel.my_approval_requests(&ctx, MOST as i64).await?).into_response())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AwayQuery {
    /// Whose (an administrator's question about someone else); the caller's by default.
    user: Option<Uuid>,
    /// `DELETE`: only the period starting that day.
    starts_on: Option<chrono::NaiveDate>,
}

/// `GET /api/away`: the caller's away periods today and to come, each with its stand-in (FEO-034); `?user=` for an
/// administrator. Set with `POST /api/away` (and, as before, Studio's `POST /api/app/studio/away`).
async fn away(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<AwayQuery>) -> ApiResult<Response> {
    Ok(Json(away_of(&app_state(&app)?.kernel, &ctx, q.user).await?).into_response())
}

/// What `GET /api/away` answers (callable without the HTTP layer).
pub async fn away_of(k: &Kernel, ctx: &Ctx, user: Option<Uuid>) -> ApiResult<Value> {
    Ok(json!(k.away_of(ctx, user.unwrap_or(ctx.user)).await?))
}

/// `POST /api/away {starts_on, ends_on, substitute (an id or an e-mail), reason, user?}`: who stands in for the caller
/// (or, said by an administrator, for `user`) between two days.
async fn set_away(Auth(ctx): Auth, State(app): State<AppContext>, Json(b): Json<Value>) -> ApiResult<Response> {
    Ok(Json(away_set(&app_state(&app)?.kernel, &ctx, &b).await?).into_response())
}

/// What `POST /api/away` answers (callable without the HTTP layer).
pub async fn away_set(k: &Kernel, ctx: &Ctx, body: &Value) -> ApiResult<Value> {
    k.set_away_given(ctx, body).await?;
    Ok(json!({}))
}

/// `DELETE /api/away`: takes them back, all of them or `?starts_on=` the one; `{"cleared": n}`.
async fn clear_away(Auth(ctx): Auth, State(app): State<AppContext>, Query(q): Query<AwayQuery>) -> ApiResult<Response> {
    Ok(Json(away_cleared(&app_state(&app)?.kernel, &ctx, q.user, q.starts_on).await?).into_response())
}

/// What `DELETE /api/away` answers (callable without the HTTP layer).
pub async fn away_cleared(k: &Kernel, ctx: &Ctx, user: Option<Uuid>, starts_on: Option<chrono::NaiveDate>) -> ApiResult<Value> {
    Ok(json!({"cleared": k.clear_away(ctx, user.unwrap_or(ctx.user), starts_on).await?}))
}

pub fn routes() -> Routes {
    use axum::routing::get;
    Routes::new().add("/api/work", get(work)).add("/api/approvals", get(approvals)).add("/api/away", get(away).post(set_away).delete(clear_away))
}
