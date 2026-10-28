//! In-app notifications. Each one is stored for the user's bell and announced on the live channel
//! (delivered at commit) so an open app shows it at once.

use crate::db::Tx;
use crate::error::Result;
use serde_json::json;
use uuid::Uuid;

/// NOTIFY channel carrying live updates (record changes and notifications) to every replica.
pub const LIVE_CHANNEL: &str = "ferrum_live";

/// Stores a notification for `user` and announces it; returns its id.
pub async fn send(tx: &mut Tx, tenant: Uuid, user: Uuid, title: &str, body: Option<&str>, entity: Option<&str>, record: Option<Uuid>) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("insert into notification (tenant_id, id, user_id, title, body, entity, record_id) values ($1, $2, $3, $4, $5, $6, $7)")
        .bind(tenant)
        .bind(id)
        .bind(user)
        .bind(title)
        .bind(body)
        .bind(entity)
        .bind(record)
        .execute(&mut **tx)
        .await?;
    // NOTIFY payloads are capped near 8 kB: announce the id, the client fetches the rest.
    let title: String = title.chars().take(200).collect();
    let payload = json!({"tenant": tenant, "user": user, "notification": {"id": id, "title": title, "entity": entity, "record_id": record}});
    sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(payload.to_string()).execute(&mut **tx).await?;
    Ok(id)
}

/// Pushes a small message to `user`'s open apps at commit (`{"push": {"kind", "data"}}` on the live
/// socket) — for features with their own access rules, like chat: the package decides who may know.
/// Keep `data` to ids and short text (NOTIFY payloads are capped near 8 kB); clients fetch the rest.
pub async fn push(tx: &mut Tx, tenant: Uuid, user: Uuid, kind: &str, data: serde_json::Value) -> Result<()> {
    let payload = json!({"tenant": tenant, "user": user, "push": {"kind": kind, "data": data}}).to_string();
    if payload.len() > 7_000 {
        return Err(crate::error::Error::invalid("live push payload larger than 7 kB"));
    }
    sqlx::query("select pg_notify($1, $2)").bind(LIVE_CHANNEL).bind(payload).execute(&mut **tx).await?;
    Ok(())
}
