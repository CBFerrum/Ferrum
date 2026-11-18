//! Apps' upkeep as the kernel sees it (the runner is server/src/packages.rs; the contract is docs/app-contract.md,
//! *Upkeep*): how many transactions one run of an app's `maintenance` opened, when the app says it next needs the
//! workspace ([`next_in`]), and how it says it needs it now ([`wake`]).

use crate::{Result, Tx};
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::futures::TaskLocalFuture;
use uuid::Uuid;

/// The wait after a run that opened no transaction: it did nothing, and nothing changes that until something wakes it
/// ([`wake`], a catalog or a workspace's metadata changing: [`wake_idle`]).
pub const IDLE: Duration = Duration::from_secs(86_400);

/// The longest wait an app may ask for with [`next_in`].
pub const LONGEST: Duration = Duration::from_secs(3_600);

/// What one run of an app's upkeep did, as far as the kernel saw it.
#[derive(Default)]
pub struct Watch {
    opened: AtomicU32,
    next: Mutex<Option<Duration>>,
}

impl Watch {
    /// The transactions the run opened through the kernel (every one the kernel makes counts: `Db`'s constructors).
    pub fn opened(&self) -> u32 {
        self.opened.load(Ordering::Relaxed)
    }

    /// The shortest wait the run asked for with [`next_in`], if it asked.
    pub fn next(&self) -> Option<Duration> {
        *self.next.lock().unwrap_or_else(|e| e.into_inner())
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

/// Called from inside an app's `maintenance`: this workspace needs it again in `d`, not after the usual minute. The
/// runner keeps it between that minute and [`LONGEST`]; of several calls the shortest wins; a run that fails or times
/// out is due again as usual whatever it said. Say it only for what the app also [`wake`]s: whatever a person does that
/// needs the upkeep sooner must wake it.
pub fn next_in(d: Duration) {
    let _ = WATCH.try_with(|w| {
        let mut next = w.next.lock().unwrap_or_else(|e| e.into_inner());
        *next = Some(next.map_or(d, |n| n.min(d)));
    });
}

/// Brings `app`'s upkeep of `tenant` forward to now, in the caller's transaction, so that it commits or rolls back with
/// the write that needs it. A run in flight is left to end: its row is due again the moment it is recorded.
///
/// An app passes its own id. Any id is accepted: waking can only make a workspace's upkeep run sooner, once, which is
/// what it did every minute before an app could ask for less; it skips, delays and changes nothing, and the row is the
/// workspace's own (a tenant transaction reaches no other's).
pub async fn wake(tx: &mut Tx, tenant: Uuid, app: &str) -> Result<()> {
    sqlx::query("update upkeep set woken = held, due_at = case when held then due_at else least(due_at, now()) end where tenant_id = $1 and app = $2")
        .bind(tenant)
        .bind(app)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Rows waiting out a day because their last run did nothing ([`IDLE`]) are due again: in `tenant`, or everywhere. For
/// what may change what an app's upkeep has to do: a new release of an app (the catalog), a workspace's metadata.
pub(crate) async fn wake_idle(tx: &mut Tx, tenant: Option<Uuid>) -> Result<()> {
    // A row recorded idle has `due_at` exactly `ran_at` + IDLE: the record sets both from one `now()`.
    let idle = "not held and due_at = ran_at + make_interval(secs => $1) and due_at > now()";
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
