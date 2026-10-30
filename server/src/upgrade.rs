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

/// Session advisory lock: one replica runs the steps; the others wait for it.
const LOCK: i64 = 0x6665_7272_756d_0002;

pub async fn run(k: &Kernel) -> Result<()> {
    let mut conn = k.db.pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)").bind(LOCK).execute(&mut *conn).await?;
    let out = core_split(k).await;
    let _ = sqlx::query("select pg_advisory_unlock($1)").bind(LOCK).execute(&mut *conn).await;
    out
}

async fn core_split(k: &Kernel) -> Result<()> {
    let mut tx = k.db.system_tx().await?;
    let tenants: Vec<Uuid> =
        sqlx::query_scalar("select t.id from tenant t where not exists (select 1 from upgrade_step s where s.tenant_id = t.id and s.step = $1)")
            .bind(CORE_SPLIT)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let mut moved = 0;
    for tenant in tenants {
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
        let closure = k.registry.catalog().closure(&want).map_err(|e| ferrum_kernel::Error::internal(e.to_string()))?;
        let new: Vec<String> = closure.into_iter().filter(|p| !installed.contains(p)).collect();
        if !new.is_empty() {
            k.install_packages(tenant, &want).await?;
            moved += 1;
            tracing::info!(%tenant, packages = ?new, "workspace moved onto the split core");
        }
        let mut tx = k.db.system_tenant_tx(tenant).await?;
        sqlx::query("insert into upgrade_step (tenant_id, step, installed) values ($1, $2, $3) on conflict do nothing")
            .bind(tenant)
            .bind(CORE_SPLIT)
            .bind(&new)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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
