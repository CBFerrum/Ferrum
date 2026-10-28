//! Functional packages compiled into the server: their hooks, reports and periodic upkeep.

use ferrum_kernel::{HookRegistry, Kernel};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Registers every package's hooks and reports.
pub fn register(hooks: &mut HookRegistry) {
    ferrum_pkg_crm::register(hooks);
    ferrum_pkg_projects::register(hooks);
    ferrum_pkg_support::register(hooks);
    for source in [ferrum_pkg_crm::reports(), ferrum_pkg_projects::reports(), ferrum_pkg_support::reports()].into_iter().flatten() {
        ferrum_domain::reports::add_source(source);
    }
}

/// Runs every package's upkeep once; failures are logged and do not stop the others.
pub async fn maintenance(k: &Kernel) {
    let results = [
        ("crm", ferrum_pkg_crm::maintenance(k).await),
        ("projects", ferrum_pkg_projects::maintenance(k).await),
        ("support", ferrum_pkg_support::maintenance(k).await),
    ];
    for (name, r) in results {
        if let Err(e) = r {
            tracing::warn!(package = name, "package maintenance failed: {e}");
        }
    }
}

pub fn spawn(k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            maintenance(&k).await;
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(Duration::from_secs(15 * 60)) => {}
            }
        }
    })
}
