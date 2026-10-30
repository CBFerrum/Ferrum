//! One-time steps that move existing workspaces onto a release's layout (docs/plan/apps-ui.md §13),
//! run at start-up on one replica. Each records, per workspace, that it ran and what it installed,
//! so `ferrum task upgrade_back step:<name>` can take that out again before going back to the
//! release before it.

use ferrum_kernel::{Kernel, Result, Tx, registry::Registry};
use uuid::Uuid;

/// ERP's packages: its app installs them together.
pub const ERP_PACKAGES: &[&str] = &["accounting", "inventory", "buying", "selling", "manufacturing"];

/// `core` split into `core`, `parties` and `products`, its accounting, stock and buying records
/// and fields went to those packages. A workspace with any of ERP's packages gets all of them, so
/// it keeps every record and field it had; one without keeps its customers, suppliers and items
/// (the groups and price lists every workspace was seeded with don't count).
pub const CORE_SPLIT: &str = "core-split";

/// Apps whose couplings with ERP moved into connector packages: (step, [(connector package, the
/// packages a workspace had it with)]). Such a workspace gets the connector, so it keeps them.
/// * `crm-erp`: quotations' taxes, sales orders made from quotations (CRM 0.2.0);
/// * `helpdesk-erp`: a ticket's sales invoice, a maintenance contract's sales order (Helpdesk 0.2.0);
/// * `hr-erp`: expense claims and advances, payroll's accounts and postings (HR 0.2.0).
pub type Connector = (&'static str, &'static [(&'static str, &'static [&'static str])]);
pub const CONNECTORS: &[Connector] = &[
    ("crm-erp", &[("crm_erp", &["crm", "selling"])]),
    ("helpdesk-erp", &[("helpdesk_erp", &["helpdesk", "selling"])]),
    ("hr-erp", &[("hr_erp", &["hr", "accounting"]), ("payroll_erp", &["payroll", "accounting"])]),
];

/// Session advisory lock: one replica runs the steps; the others wait for it.
const LOCK: i64 = 0x6665_7272_756d_0002;

pub async fn run(k: &Kernel) -> Result<()> {
    let mut conn = k.db.pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)").bind(LOCK).execute(&mut *conn).await?;
    let out = async {
        core_split(k).await?;
        for (step, connectors) in CONNECTORS {
            connector_step(k, step, connectors).await?;
        }
        Ok(())
    }
    .await;
    let _ = sqlx::query("select pg_advisory_unlock($1)").bind(LOCK).execute(&mut *conn).await;
    out
}

/// Workspaces `step` has not run for.
async fn pending(k: &Kernel, step: &str) -> Result<Vec<Uuid>> {
    let mut tx = k.db.system_tx().await?;
    let tenants = sqlx::query_scalar("select t.id from tenant t where not exists (select 1 from upgrade_step s where s.tenant_id = t.id and s.step = $1)")
        .bind(step)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(tenants)
}

/// Installs `want` (with what it depends on) and records that `step` ran, with what it installed.
async fn finish(k: &Kernel, tenant: Uuid, step: &str, installed: &[String], want: &[String]) -> Result<bool> {
    let closure = k.registry.catalog().closure(want).map_err(|e| ferrum_kernel::Error::internal(e.to_string()))?;
    let new: Vec<String> = closure.into_iter().filter(|p| !installed.contains(p)).collect();
    if !new.is_empty() {
        k.install_packages(tenant, want).await?;
        tracing::info!(%tenant, packages = ?new, "{step}: workspace moved onto this release's packages");
    }
    let mut tx = k.db.system_tenant_tx(tenant).await?;
    sqlx::query("insert into upgrade_step (tenant_id, step, installed) values ($1, $2, $3) on conflict do nothing")
        .bind(tenant)
        .bind(step)
        .bind(&new)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(!new.is_empty())
}

async fn connector_step(k: &Kernel, step: &str, connectors: &[(&str, &[&str])]) -> Result<()> {
    // Without the app compiled in, its workspaces wait for a server that has it.
    if connectors.iter().any(|(c, _)| k.registry.catalog().package(c).is_none()) {
        return Ok(());
    }
    for tenant in pending(k, step).await? {
        let mut tx = k.db.system_tenant_tx(tenant).await?;
        let installed: Vec<String> = sqlx::query_scalar("select name from installed_package where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let want: Vec<String> =
            connectors.iter().filter(|(_, with)| with.iter().all(|p| installed.iter().any(|i| i == p))).map(|(c, _)| c.to_string()).collect();
        finish(k, tenant, step, &installed, &want).await?;
    }
    Ok(())
}

async fn core_split(k: &Kernel) -> Result<()> {
    let mut moved = 0;
    for tenant in pending(k, CORE_SPLIT).await? {
        let mut tx = k.db.system_tenant_tx(tenant).await?;
        let installed: Vec<String> = sqlx::query_scalar("select name from installed_package where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        let want: Vec<String> = if ERP_PACKAGES.iter().any(|p| installed.iter().any(|i| i == p)) {
            ERP_PACKAGES.iter().map(|p| p.to_string()).collect()
        } else {
            let mut w = Vec::new();
            if has_records(&mut tx, tenant, &["Customer", "Supplier"]).await? {
                w.push("parties".to_string());
            }
            if has_records(&mut tx, tenant, &["Item", "ItemPrice"]).await? {
                w.push("products".to_string());
            }
            w
        };
        tx.commit().await?;
        if finish(k, tenant, CORE_SPLIT, &installed, &want).await? {
            moved += 1;
        }
    }
    if moved > 0 {
        tracing::info!(workspaces = moved, "{CORE_SPLIT}: done");
    }
    Ok(())
}

async fn has_records(tx: &mut Tx, tenant: Uuid, entities: &[&str]) -> Result<bool> {
    for e in entities {
        // The table name is an entity's, quoted.
        let q = sqlx::AssertSqlSafe(format!("select exists (select 1 from {} where tenant_id = $1)", ferrum_kernel::ddl::q(&ferrum_meta::table_for(e))));
        if sqlx::query_scalar::<_, bool>(q).bind(tenant).fetch_one(&mut **tx).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Takes out what `step` installed in every workspace and forgets that it ran: for going back to
/// the release before it (which does not know those packages). The data stays.
pub async fn back(k: &Kernel, step: &str) -> Result<usize> {
    let mut tx = k.db.system_tx().await?;
    let rows: Vec<(Uuid, Vec<String>)> = sqlx::query_as("select tenant_id, installed from upgrade_step where step = $1").bind(step).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    for (tenant, installed) in &rows {
        let mut tx = k.db.system_tenant_tx(*tenant).await?;
        sqlx::query("delete from installed_package where tenant_id = $1 and name = any($2)").bind(tenant).bind(installed).execute(&mut *tx).await?;
        sqlx::query("delete from upgrade_step where tenant_id = $1 and step = $2").bind(tenant).bind(step).execute(&mut *tx).await?;
        Registry::bump_version(&mut tx, *tenant).await?;
        tx.commit().await?;
        k.registry.invalidate(*tenant);
    }
    Ok(rows.len())
}
