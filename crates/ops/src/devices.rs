//! Devices: what the ledger needs to know of one, and its lifecycle. A device becomes Provisioned
//! only by enrolling a key, and Commissioned only with a site and allowed operations, so everything
//! it may do was decided by a person before it can do it.

use crate::{authority, identity};
use ferrum_domain::util::sys;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx, audit};
use ferrum_ops_contract::Lifecycle;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Target {
    pub id: Uuid,
    pub code: String,
    pub kind: String,
    pub lifecycle: Lifecycle,
    pub site: Uuid,
    pub domain: String,
    pub config_version: Option<String>,
}

pub(crate) fn lifecycle_of(s: &str) -> Result<Lifecycle> {
    Lifecycle::parse(s).ok_or_else(|| Error::internal(format!("a device in an unknown lifecycle: {s}")))
}

/// A device with its site, or an error saying what is missing.
pub async fn target(tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<Target> {
    let row: Option<(String, String, String, Option<Uuid>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "select d.device_code, d.kind, d.lifecycle, d.site, nullif(d.domain, ''), s.site_code, d.configuration_version \
         from e_device d left join e_site s on s.tenant_id = d.tenant_id and s.id = d.site where d.tenant_id = $1 and d.id = $2",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let (code, kind, lifecycle, site, domain, site_code, config_version) = row.ok_or_else(|| Error::NotFound("device".into()))?;
    let site = site.ok_or_else(|| Error::invalid(format!("{code} belongs to no site")))?;
    let domain = domain.or(site_code).ok_or_else(|| Error::invalid(format!("{code} has no domain")))?;
    Ok(Target { id, code, kind, lifecycle: lifecycle_of(&lifecycle)?, site, domain, config_version })
}

/// How an operation was allowed for a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowed {
    /// A second person approves each one.
    pub needs_approval: bool,
    /// Only the app that owns this work may ask for it.
    pub engine_only: bool,
}

/// Whether `operation` was allowed for this device, and on what terms.
pub async fn operation(tx: &mut Tx, tenant: Uuid, device: Uuid, operation: &str) -> Result<Option<Allowed>> {
    let row: Option<(bool, bool)> = sqlx::query_as(
        "select coalesce(requires_approval, false), coalesce(engine_only, false) from e_device__operations where tenant_id = $1 and parent_id = $2 and operation = $3",
    )
    .bind(tenant)
    .bind(device)
    .bind(operation)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(needs_approval, engine_only)| Allowed { needs_approval, engine_only }))
}

/// Moves a device through its lifecycle. Provisioned is reached only by enrolling.
pub async fn set_lifecycle(k: &Kernel, ctx: &Ctx, id: Uuid, to: Lifecycle, reason: &str) -> Result<Lifecycle> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    set_lifecycle_tx(k, ctx, &mut tx, id, to, reason).await?;
    tx.commit().await?;
    Ok(to)
}

/// [`set_lifecycle`] inside the caller's transaction, so that what moved it commits with it.
pub async fn set_lifecycle_tx(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, to: Lifecycle, reason: &str) -> Result<()> {
    let row: Option<(String, String, Option<Uuid>, Option<Uuid>)> =
        sqlx::query_as("select lifecycle, kind, site, gateway from e_device where tenant_id = $1 and id = $2 for update")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    let (from, kind, site, gateway) = row.ok_or_else(|| Error::NotFound("device".into()))?;
    let from = lifecycle_of(&from)?;
    // Equipment behind a gateway has no key of its own: the gateway speaks for it, so naming the
    // gateway is what provisions it. Anything else proves itself by enrolling.
    if to == Lifecycle::Provisioned && gateway.is_none() {
        return Err(Error::invalid("a device is provisioned by enrolling a key, or by naming the gateway that reaches it"));
    }
    if !from.can_move(to) {
        return Err(Error::invalid(format!("a device that is {} cannot become {}", from.as_str(), to.as_str())));
    }
    if to == Lifecycle::Commissioned {
        if site.is_none() {
            return Err(Error::invalid("a device is commissioned for a site: choose one"));
        }
        let ops: i64 = sqlx::query_scalar("select count(*) from e_device__operations where tenant_id = $1 and parent_id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
        if ops == 0 && kind != "Gateway" && kind != "Sensor" && kind != "Meter" {
            return Err(Error::invalid("list what this device may be asked to do before commissioning it"));
        }
        let waiting = crate::acceptance::waiting(tx, ctx.tenant, id).await?;
        if !waiting.is_empty() {
            return Err(Error::invalid(format!("pass its acceptance on site first: {}", waiting.join(", "))));
        }
    }
    let mut values = Doc::new();
    values.insert("lifecycle".into(), json!(to.as_str()));
    if to == Lifecycle::Commissioned {
        values.insert("commissioned_by".into(), json!(ctx.email));
        values.insert("commissioned_at".into(), json!(chrono::Utc::now().to_rfc3339()));
        values.insert("configuration_version".into(), json!(Uuid::now_v7().to_string()));
    }
    k.set_values_tx(&sys(ctx), tx, "Device", id, values).await?;
    if to == Lifecycle::Retired {
        identity::revoke_keys(tx, ctx.tenant, id).await?;
    }
    if matches!(to, Lifecycle::Quarantined | Lifecycle::Retired) {
        authority::fence_holder(tx, ctx, id, reason).await?;
    }
    audit::record(tx, ctx, "Device", id, "ops.lifecycle", None, json!({"from": from.as_str(), "to": to.as_str(), "reason": reason})).await
}

/// The lifecycle is not a field anyone edits, and what a commissioned device may do changes only
/// with a new configuration version, so a site and the record can always be compared.
pub struct DeviceHook;

fn changed(a: Option<&Value>, b: Option<&Value>) -> bool {
    a.filter(|v| !v.is_null()) != b.filter(|v| !v.is_null())
}

#[async_trait::async_trait]
impl DocHook for DeviceHook {
    fn name(&self) -> &'static str {
        "ops.device"
    }

    async fn validate_change(&self, hc: &mut HookCtx<'_>, old: Option<&Doc>, doc: &Doc) -> Result<()> {
        let Some(old) = old else {
            let tenant = hc.ctx.tenant;
            let over: Option<bool> = sqlx::query_scalar(
                "select (select count(*) from e_device where tenant_id = $1 and lifecycle <> 'Retired') \
                   >= (select (quotas->>'devices')::bigint from tenant where id = $1)",
            )
            .bind(tenant)
            .fetch_one(&mut **hc.tx)
            .await?;
            if over.unwrap_or(false) {
                return Err(Error::Conflict("this workspace has reached its allowance of devices".into()));
            }
            return Ok(());
        };
        let commissioned = matches!(old.get("lifecycle").and_then(Value::as_str), Some("Commissioned" | "Active" | "Maintenance"));
        if commissioned && (changed(old.get("site"), doc.get("site")) || changed(old.get("domain"), doc.get("domain"))) {
            return Err(Error::field("site", "a commissioned device is bound to its site and domain: quarantine it and commission it again"));
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        let Some(old) = old else { return Ok(()) };
        if changed(old.get("operations"), doc.get("operations")) || changed(old.get("metrics"), doc.get("metrics")) {
            let id = doc.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("a device without an id"))?;
            let mut values = Doc::new();
            values.insert("configuration_version".into(), json!(Uuid::now_v7().to_string()));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Device", id, values).await?;
        }
        Ok(())
    }
}
