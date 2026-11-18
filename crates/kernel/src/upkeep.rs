//! Apps' upkeep as the kernel sees it (the runner is server/src/packages.rs; the contract is docs/app-contract.md,
//! *Upkeep*): how many transactions one run of an app's `maintenance` opened.

use crate::{Result, Tx};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::task::futures::TaskLocalFuture;
use uuid::Uuid;

/// The wait after a run that opened no transaction: it did nothing, and nothing changes that until something wakes it
/// (a catalog or a workspace's metadata changing: [`wake_idle`]).
pub const IDLE: Duration = Duration::from_secs(86_400);

/// What one run of an app's upkeep did, as far as the kernel saw it.
#[derive(Default)]
pub struct Watch {
    opened: AtomicU32,
}

impl Watch {
    /// The transactions the run opened through the kernel (every one the kernel makes counts: `Db`'s constructors).
    pub fn opened(&self) -> u32 {
        self.opened.load(Ordering::Relaxed)
    }
}

tokio::task_local! {
    static WATCH: Arc<Watch>;
}

/// Runs `f`, one run of an app's upkeep, under `w`.
pub fn watching<F: Future>(w: Arc<Watch>, f: F) -> TaskLocalFuture<Arc<Watch>, F> {
    WATCH.scope(w, f)
}

/// A kernel transaction is opening (crate::db): counted for the run in hand, if any.
pub(crate) fn opened() {
    let _ = WATCH.try_with(|w| w.opened.fetch_add(1, Ordering::Relaxed));
}

/// Rows waiting out a day because their last run did nothing ([`IDLE`]) are due again: in `tenant`, or everywhere. For
/// what may change what an app's upkeep has to do: a new release of an app (the catalog), a workspace's metadata.
pub(crate) async fn wake_idle(tx: &mut Tx, tenant: Option<Uuid>) -> Result<()> {
    // A row recorded idle has `due_at` exactly `ran_at` + IDLE: the record sets both from one `now()`.
    let idle = "due_at = ran_at + make_interval(secs => $1) and due_at > now()";
    match tenant {
        Some(t) => {
            sqlx::query(sqlx::AssertSqlSafe(format!("update upkeep set due_at = now() where tenant_id = $2 and {idle}")))
                .bind(IDLE.as_secs_f64())
                .bind(t)
                .execute(&mut **tx)
                .await?
        }
        None => sqlx::query(sqlx::AssertSqlSafe(format!("update upkeep set due_at = now() where {idle}"))).bind(IDLE.as_secs_f64()).execute(&mut **tx).await?,
    };
    Ok(())
}
