//! Which gateway a device is connected to. A device that calls in goes wherever it is sent, so in a
//! pooled domain the connection decides who may act for it: a record with a generation, moved by the
//! device calling somewhere else and never by a gateway's belief.
//!
//! What a gateway says arrives late and out of order, because it comes from a journal. So an opening
//! older than what is known changes nothing, and a closing only ends the connection it is about: an old
//! gateway noticing that its socket died says nothing about the new one.

use crate::commands;
use crate::devices;
use chrono::{DateTime, Utc};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use serde_json::Value;
use uuid::Uuid;

/// A device connected to `gateway`, at the time the gateway says. Returns the generation, unless what
/// is known is newer than this. What the gateway it left may have been doing becomes unknown.
pub async fn opened(tx: &mut Tx, tenant: Uuid, device: Uuid, gateway: Uuid, site: Uuid, at: DateTime<Utc>) -> Result<Option<i64>> {
    let target = devices::target(tx, tenant, device).await?;
    if target.site != site {
        return Err(Error::Forbidden(format!("{} belongs to another site", target.code)));
    }
    let known: Option<(Uuid, i64, DateTime<Utc>, Option<DateTime<Utc>>)> =
        sqlx::query_as("select gateway_id, generation, opened_at, closed_at from op_connection where tenant_id = $1 and device_id = $2 for update")
            .bind(tenant)
            .bind(device)
            .fetch_optional(&mut **tx)
            .await?;
    match known {
        Some((_, _, since, _)) if at < since => Ok(None),
        Some((owner, generation, _, None)) if owner == gateway => Ok(Some(generation)),
        Some((owner, generation, _, _)) => {
            sqlx::query("update op_connection set gateway_id = $3, generation = $4, opened_at = $5, closed_at = null where tenant_id = $1 and device_id = $2")
                .bind(tenant)
                .bind(device)
                .bind(gateway)
                .bind(generation + 1)
                .bind(at)
                .execute(&mut **tx)
                .await?;
            if owner != gateway {
                commands::mark_moved(tx, tenant, device, owner).await?;
            }
            Ok(Some(generation + 1))
        }
        None => {
            sqlx::query("insert into op_connection (tenant_id, device_id, gateway_id, opened_at) values ($1, $2, $3, $4)")
                .bind(tenant)
                .bind(device)
                .bind(gateway)
                .bind(at)
                .execute(&mut **tx)
                .await?;
            Ok(Some(1))
        }
    }
}

/// A gateway's connection to a device ended. Only ever that one: a newer connection stays.
pub async fn closed(tx: &mut Tx, tenant: Uuid, device: Uuid, gateway: Uuid, at: DateTime<Utc>) -> Result<bool> {
    let ended = sqlx::query("update op_connection set closed_at = $4 where tenant_id = $1 and device_id = $2 and gateway_id = $3 and closed_at is null and opened_at <= $4")
        .bind(tenant)
        .bind(device)
        .bind(gateway)
        .bind(at)
        .execute(&mut **tx)
        .await?;
    Ok(ended.rows_affected() > 0)
}

/// Everything a gateway held, when it stops being trusted. Whoever calls in next starts a new generation.
pub async fn close_all(tx: &mut Tx, tenant: Uuid, gateway: Uuid) -> Result<()> {
    let held: Vec<Uuid> =
        sqlx::query_scalar("update op_connection set closed_at = now() where tenant_id = $1 and gateway_id = $2 and closed_at is null returning device_id")
            .bind(tenant)
            .bind(gateway)
            .fetch_all(&mut **tx)
            .await?;
    for device in held {
        commands::mark_moved(tx, tenant, device, gateway).await?;
    }
    Ok(())
}

pub async fn list(k: &Kernel, ctx: &Ctx) -> Result<Vec<Value>> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(c) - 'tenant_id' || jsonb_build_object('device_code', d.device_code, 'gateway_code', g.device_code) from op_connection c \
         join e_device d on d.tenant_id = c.tenant_id and d.id = c.device_id join e_device g on g.tenant_id = c.tenant_id and g.id = c.gateway_id \
         where c.tenant_id = $1 order by d.device_code",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}
