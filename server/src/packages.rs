//! Functional packages compiled into the server (cargo features): their hooks, reports, web
//! endpoints and periodic upkeep.

use ferrum_kernel::{HookRegistry, Kernel};
use ferrum_meta::Catalog;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How often package upkeep runs (expiring quotations, SLA clocks, mail polling...).
const EVERY: Duration = Duration::from_secs(60);
/// Session advisory lock: one replica runs upkeep at a time; the others skip that round.
const LOCK: i64 = 0x6665_7272_756d_0001;

macro_rules! packages {
    ($($feature:literal => $krate:ident [$($yaml:literal),+]),* $(,)?) => {
        /// Registers every compiled package's hooks, reports and endpoints.
        pub fn register(hooks: &mut HookRegistry) {
            $(
                #[cfg(feature = $feature)]
                {
                    $krate::register(hooks);
                    for source in $krate::reports() {
                        ferrum_domain::reports::add_source(source);
                    }
                }
            )*
            let _ = hooks;
        }

        /// Runs every compiled package's upkeep once; failures are logged and do not stop the others.
        pub async fn maintenance(k: &Kernel) {
            $(
                #[cfg(feature = $feature)]
                if let Err(e) = $krate::maintenance(k).await {
                    tracing::warn!(package = $feature, "package maintenance failed: {e}");
                }
            )*
            let _ = k;
        }

        /// YAML packages whose business logic is not compiled into this build.
        pub fn missing_logic(catalog: &Catalog) -> Vec<String> {
            let mut out = Vec::new();
            $(
                if !cfg!(feature = $feature) {
                    out.extend([$($yaml),+].into_iter().filter(|p| catalog.packages().any(|x| x.manifest.package == *p)).map(String::from));
                }
            )*
            out
        }
    };
}

packages! {
    "crm" => ferrum_pkg_crm ["crm"],
    "projects" => ferrum_pkg_projects ["projects"],
    "support" => ferrum_pkg_support ["support"],
}

/// Upkeep under the advisory lock, held on one pooled connection for the round.
async fn locked_round(k: &Kernel) -> Result<(), sqlx::Error> {
    let mut conn = k.db.pool.acquire().await?;
    let got: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(LOCK).fetch_one(&mut *conn).await?;
    if !got {
        return Ok(());
    }
    maintenance(k).await;
    sqlx::query("select pg_advisory_unlock($1)").bind(LOCK).execute(&mut *conn).await?;
    Ok(())
}

pub fn spawn(k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = locked_round(&k).await {
                tracing::warn!("package maintenance: {e}");
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(EVERY) => {}
            }
        }
    })
}
