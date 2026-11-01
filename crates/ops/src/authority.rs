//! Who may dispatch for a domain. One holder at a time, and every change of holder raises the
//! epoch, so what the old holder still believes is refused. There is no automatic failover: while
//! nobody can say whether the old gateway is alive, handing over is a person's decision, and it
//! fences the old holder first. What it may have started becomes `OutcomeUnknown`.

use crate::commands;
use crate::devices;
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx, audit};
use ferrum_ops_contract::Lifecycle;
use serde_json::{Value, json};
use uuid::Uuid;

/// A gateway takes a domain nobody holds, on its first enrollment. Anything else is a transfer.
pub async fn claim_if_free(tx: &mut Tx, tenant: Uuid, domain: &str, site: Uuid, gateway: Uuid) -> Result<()> {
    sqlx::query(
        "insert into op_authority (tenant_id, domain, site_id, holder, reason) values ($1, $2, $3, $4, 'first enrollment') \
         on conflict (tenant_id, domain) do update set holder = excluded.holder, epoch = op_authority.epoch + 1, changed_at = now(), reason = excluded.reason \
         where op_authority.holder is null",
    )
    .bind(tenant)
    .bind(domain)
    .bind(site)
    .bind(gateway)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Hands `domain` to `to` and fences whoever held it.
pub async fn transfer(k: &Kernel, ctx: &Ctx, domain: &str, to: Uuid, reason: &str) -> Result<i64> {
    if reason.trim().is_empty() {
        return Err(Error::field("reason", "say why: this stops whatever the old gateway was doing from being believed"));
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let row: Option<(Uuid, Option<Uuid>, i64)> =
        sqlx::query_as("select site_id, holder, epoch from op_authority where tenant_id = $1 and domain = $2 for update")
            .bind(ctx.tenant)
            .bind(domain)
            .fetch_optional(&mut *tx)
            .await?;
    let (site, old, epoch) = row.ok_or_else(|| Error::NotFound("domain".into()))?;
    let gateway = devices::target(&mut tx, ctx.tenant, to).await?;
    if gateway.kind != "Gateway" {
        return Err(Error::invalid(format!("{} is not a gateway", gateway.code)));
    }
    if gateway.site != site {
        return Err(Error::invalid(format!("{} belongs to another site", gateway.code)));
    }
    if !gateway.lifecycle.may_connect() || gateway.lifecycle == Lifecycle::Quarantined {
        return Err(Error::invalid(format!("{} is {}", gateway.code, gateway.lifecycle.as_str())));
    }
    if old == Some(to) {
        return Ok(epoch);
    }
    let epoch = epoch + 1;
    change(&mut tx, ctx.tenant, domain, Some(to), epoch, ctx, reason).await?;
    commands::mark_in_doubt(&mut tx, ctx.tenant, domain, epoch, "authority changed while this may have been running").await?;
    audit::record(&mut tx, ctx, "Site", site, "ops.authority", None, json!({"domain": domain, "from": old, "to": to, "epoch": epoch, "reason": reason}))
        .await?;
    tx.commit().await?;
    Ok(epoch)
}

async fn change(tx: &mut Tx, tenant: Uuid, domain: &str, holder: Option<Uuid>, epoch: i64, ctx: &Ctx, reason: &str) -> Result<()> {
    sqlx::query("update op_authority set holder = $3, epoch = $4, changed_by = $5, changed_at = now(), reason = $6 where tenant_id = $1 and domain = $2")
        .bind(tenant)
        .bind(domain)
        .bind(holder)
        .bind(epoch)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(reason)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A gateway stops being trusted: whatever it holds is held by nobody, until a person transfers it.
pub async fn fence_holder(tx: &mut Tx, ctx: &Ctx, gateway: Uuid, reason: &str) -> Result<()> {
    let held: Vec<(String, i64)> = sqlx::query_as("select domain, epoch from op_authority where tenant_id = $1 and holder = $2 for update")
        .bind(ctx.tenant)
        .bind(gateway)
        .fetch_all(&mut **tx)
        .await?;
    for (domain, epoch) in held {
        change(tx, ctx.tenant, &domain, None, epoch + 1, ctx, reason).await?;
        commands::mark_in_doubt(tx, ctx.tenant, &domain, epoch + 1, "its gateway stopped being trusted while this may have been running").await?;
    }
    Ok(())
}

pub async fn list(k: &Kernel, ctx: &Ctx) -> Result<Vec<Value>> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select to_jsonb(a) - 'tenant_id' || jsonb_build_object('holder_name', d.device_name, 'last_seen_at', g.last_seen_at) from op_authority a \
         left join e_device d on d.tenant_id = a.tenant_id and d.id = a.holder \
         left join op_gateway g on g.tenant_id = a.tenant_id and g.device_id = a.holder where a.tenant_id = $1 order by a.domain",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}
