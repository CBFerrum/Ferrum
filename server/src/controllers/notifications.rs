//! The person's notifications (docs/plan/apps-ui.md §12.8; crates/kernel/src/notify.rs): the
//! latest, marking them read, the kinds they may choose for (the platform's and the apps'
//! `shell.notification`, as `{app}.{id}`) and how each reaches them. Own notifications only.

use crate::error::ApiResult;
use crate::extract::Auth;
use crate::state::{AppState, app_state};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error, notify};
use loco_rs::app::AppContext;
use loco_rs::controller::Routes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Deserialize)]
struct ListParams {
    /// How many of the latest (at most 100, the default; 0 for the count alone).
    limit: Option<i64>,
}

/// `GET /api/notifications?limit=`: the latest and how many are unread.
async fn list(Auth(ctx): Auth, State(app): State<AppContext>, Query(p): Query<ListParams>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(n) - 'tenant_id' from notification n where n.tenant_id = $1 and n.user_id = $2 order by n.created_at desc limit $3",
    )
    .bind(ctx.tenant)
    .bind(ctx.user)
    .bind(p.limit.unwrap_or(100).clamp(0, 100))
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::from)?;
    let unread: i64 = sqlx::query_scalar("select count(*) from notification where tenant_id = $1 and user_id = $2 and read_at is null")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(Json(json!({"notifications": rows, "unread": unread})).into_response())
}

async fn mark_read(Auth(ctx): Auth, State(app): State<AppContext>, Path(id): Path<Uuid>) -> ApiResult<Response> {
    read(&app, &ctx, Some(id)).await
}

async fn mark_all_read(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    read(&app, &ctx, None).await
}

async fn read(app: &AppContext, ctx: &Ctx, id: Option<Uuid>) -> ApiResult<Response> {
    let st = app_state(app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("update notification set read_at = now() where tenant_id = $1 and user_id = $2 and ($3::uuid is null or id = $3) and read_at is null")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The kinds a person may choose for: (kind, label, app's name, default channel). Apps' kinds only
/// for the apps they may use.
async fn kinds(st: &AppState, ctx: &Ctx) -> ferrum_kernel::Result<Vec<(String, String, Option<String>, &'static str)>> {
    let mut out: Vec<_> = notify::KINDS.iter().map(|(k, l)| (k.to_string(), l.to_string(), None, "inapp")).collect();
    let kind = st.shell.kind(ctx.tenant, ctx.user).await?;
    for a in st.apps.shell_apps(ctx.tenant).await? {
        if !a.items("shell.app").any(|i| crate::shell::may_use(ctx, &kind, i)) {
            continue;
        }
        for item in a.items("shell.notification") {
            let (Some(id), Some(label)) = (item.get("id").and_then(Value::as_str), item.get("label").and_then(Value::as_str)) else { continue };
            let listed: Vec<&str> = match item.get("default").and_then(Value::as_array) {
                Some(l) => l.iter().filter_map(Value::as_str).collect(),
                None => vec!["inapp"],
            };
            out.push((format!("{}.{id}", a.id), label.to_string(), Some(a.manifest.name.clone()), notify::declared_default(&listed)));
        }
    }
    Ok(out)
}

/// `GET /api/notifications/types`: each kind with its default and the person's choice, the
/// channels, and whether this server sends email at all.
async fn types(Auth(ctx): Auth, State(app): State<AppContext>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    let chosen: HashMap<String, String> = sqlx::query_as("select kind, channel from notification_pref where tenant_id = $1 and user_id = $2")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::from)?
        .into_iter()
        .collect();
    tx.commit().await.map_err(Error::from)?;
    let types: Vec<Value> = kinds(&st, &ctx)
        .await?
        .into_iter()
        .map(|(kind, label, app, default)| {
            let channel = chosen.get(&kind).map(String::as_str).unwrap_or(default);
            json!({"kind": kind, "label": label, "app": app, "default": default, "channel": channel})
        })
        .collect();
    Ok(Json(json!({"types": types, "channels": notify::CHANNELS, "email": st.kernel.mailer.is_some()})).into_response())
}

#[derive(Deserialize)]
struct Choice {
    kind: String,
    /// `None`: back to the default.
    channel: Option<String>,
}

/// `PUT /api/notifications/preferences` `{kind, channel}`: how one kind reaches the person (`null`:
/// its default).
async fn choose(Auth(ctx): Auth, State(app): State<AppContext>, Json(c): Json<Choice>) -> ApiResult<Response> {
    let st = app_state(&app)?;
    if !kinds(&st, &ctx).await?.iter().any(|k| k.0 == c.kind) {
        return Err(Error::field("kind", "is not a kind of notification you may choose for").into());
    }
    if let Some(ch) = c.channel.as_deref() {
        if !notify::CHANNELS.contains(&ch) {
            return Err(Error::field("channel", format!("is one of {}", notify::CHANNELS.join(", "))).into());
        }
        if matches!(ch, "email" | "digest") && st.kernel.mailer.is_none() {
            return Err(Error::field("channel", "this server sends no email").into());
        }
    }
    let mut tx = st.kernel.db.tenant_tx(ctx.tenant).await?;
    match c.channel {
        Some(ch) => sqlx::query(
            "insert into notification_pref (tenant_id, user_id, kind, channel) values ($1, $2, $3, $4) \
             on conflict (tenant_id, user_id, kind) do update set channel = excluded.channel, updated_at = now()",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(&c.kind)
        .bind(ch),
        None => sqlx::query("delete from notification_pref where tenant_id = $1 and user_id = $2 and kind = $3").bind(ctx.tenant).bind(ctx.user).bind(&c.kind),
    }
    .execute(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub fn routes() -> Routes {
    use axum::routing::{get, post, put};
    Routes::new()
        .add("/api/notifications", get(list))
        .add("/api/notifications/read-all", post(mark_all_read))
        .add("/api/notifications/types", get(types))
        .add("/api/notifications/preferences", put(choose))
        .add("/api/notifications/{id}/read", post(mark_read))
}
