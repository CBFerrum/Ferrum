//! Plugin schedules: every replica polls, `FOR UPDATE SKIP LOCKED` hands each due schedule to
//! exactly one of them, and the next run is set before running so a crash skips one run rather
//! than repeating it.

use crate::host::{Host, Scope};
use crate::{PluginHost, reply_error};
use ferrum_kernel::{Kernel, Result};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

const POLL: Duration = Duration::from_secs(15);

/// Claims and runs the schedules that are due. Returns how many ran.
pub async fn run_due_once(host: &Arc<PluginHost>, k: &Kernel) -> Result<usize> {
    let mut tx = k.db.system_tx().await?;
    let due: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "select s.tenant_id, s.plugin_id, s.name from plugin_schedule s \
         join plugin p on p.tenant_id = s.tenant_id and p.id = s.plugin_id \
         where p.enabled and s.next_run_at <= now() order by s.next_run_at limit 20 for update of s skip locked",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (tenant, plugin, name) in &due {
        sqlx::query(
            "update plugin_schedule set next_run_at = now() + make_interval(secs => every_secs), last_run_at = now() \
             where tenant_id = $1 and plugin_id = $2 and name = $3",
        )
        .bind(tenant)
        .bind(plugin)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    for (tenant, plugin, name) in &due {
        let outcome = async {
            let plugins = host.plugins(&k.db, *tenant).await?;
            let Some(p) = plugins.iter().find(|p| p.id == *plugin) else { return Ok(()) };
            let ctx = host.service_ctx(k, *tenant, p).await?;
            let reply = {
                let mut api = Host { host, plugin: p, scope: Scope::Own { k, ctx }, limit_to_manifest: false };
                host.invoke(&k.db, *tenant, p, json!({"kind": "schedule", "name": name}), &mut api).await?
            };
            match reply_error(&reply) {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
        .await;
        let error = outcome.err().map(|e: ferrum_kernel::Error| e.to_string());
        if let Some(e) = &error {
            tracing::warn!(%tenant, %plugin, schedule = %name, "plugin schedule failed: {e}");
        }
        let mut tx = k.db.system_tx().await?;
        sqlx::query("update plugin_schedule set last_error = $4 where tenant_id = $1 and plugin_id = $2 and name = $3")
            .bind(tenant)
            .bind(plugin)
            .bind(name)
            .bind(&error)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(due.len())
}

pub fn spawn_scheduler(host: Arc<PluginHost>, k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_due_once(&host, &k).await {
                tracing::warn!("plugin scheduler: {e}");
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(POLL) => {}
            }
        }
    })
}
