//! Durable processes on this server (`crates/process`, docs/logic/processes.md). With `TEMPORAL_ADDRESS` set and
//! the `process` feature built in, releases compile under the Full Process profile, this server starts and signals
//! runs from its outbox, and a replica that runs background work also works them. Without either, solutions with a
//! process are refused (`unsupported_profile`) and everything else runs as before.

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::{HookRegistry, Kernel};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[cfg(feature = "process")]
static SETTINGS: std::sync::OnceLock<ferrum_process::Settings> = std::sync::OnceLock::new();

/// Connects to Temporal when it is configured and registers what starts and signals runs. Whether this server
/// runs processes.
pub async fn configure(hooks: &mut HookRegistry, key: &SecretKey) -> Result<bool, String> {
    let Some(address) = std::env::var("TEMPORAL_ADDRESS").ok().filter(|a| !a.trim().is_empty()) else { return Ok(false) };
    #[cfg(feature = "process")]
    {
        let settings = ferrum_process::Settings {
            address,
            namespace: std::env::var("TEMPORAL_NAMESPACE").unwrap_or_else(|_| "ferrum".into()),
            queue: std::env::var("TEMPORAL_TASK_QUEUE").unwrap_or_else(|_| ferrum_process::QUEUE.into()),
            key: Arc::new(key.clone()),
        };
        let client = ferrum_process::client(&settings).await.map_err(|e| format!("TEMPORAL_ADDRESS: {e}"))?;
        hooks.add_event_sink(Arc::new(ferrum_process::Dispatch::new(client, settings.queue.clone())));
        let _ = SETTINGS.set(settings);
        Ok(true)
    }
    #[cfg(not(feature = "process"))]
    {
        let _ = (hooks, key);
        tracing::warn!(%address, "TEMPORAL_ADDRESS is set, but this build has no process runtime (the `process` feature): processes are refused");
        Ok(false)
    }
}

/// Works this server's processes until shutdown, when it was configured to run them.
pub async fn spawn(k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> Result<Option<JoinHandle<()>>, String> {
    #[cfg(feature = "process")]
    {
        let Some(settings) = SETTINGS.get().cloned() else { return Ok(None) };
        let running = ferrum_process::spawn(k, settings).await.map_err(|e| format!("process worker: {e}"))?;
        Ok(Some(tokio::spawn(async move {
            let _ = shutdown.changed().await;
            running.stop();
            if let Err(e) = running.stopped().await {
                tracing::warn!("the process worker stopped with an error: {e}");
            }
        })))
    }
    #[cfg(not(feature = "process"))]
    {
        let _ = (k, &mut shutdown);
        Ok(None)
    }
}
