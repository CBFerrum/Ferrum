//! Connection pool, tenant-scoped transactions and retry on transient conflicts.

use crate::error::{Error, Result};
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPoolOptions};
use sqlx::query::Query;
use sqlx::{AssertSqlSafe, Postgres, Transaction};
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;

pub type Tx = Transaction<'static, Postgres>;

/// Connections in the side pool (see [`Db::side`]); count them in the database's connection budget.
pub const SIDE_CONNECTIONS: u32 = 4;

/// What the work in progress belongs to. Every transaction opened inside [`Trace::scope`] carries
/// it, and the events written there record it (the columns of `outbox` default to these settings),
/// so a chain of events can be followed from the request that started it without any caller
/// passing anything along.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    /// The request or job that started the chain.
    pub correlation: String,
    /// The event this work answers.
    pub causation: Option<i64>,
    /// The caller's W3C trace context, passed on as it came.
    pub traceparent: Option<String>,
    /// Events' priority; unset means normal ([`crate::outbox::PRIORITY_BULK`] for bulk work).
    pub priority: Option<i16>,
    /// How far down a chain of events this work is: 0 for a request or job, one more than the event's
    /// own for work that answers an event, whatever handler does it (FLC-HOOK-008).
    pub depth: i16,
}

tokio::task_local! {
    static TRACE: Trace;
}

impl Trace {
    /// Runs `f` under this trace (tasks it spawns start without one).
    pub async fn scope<F: Future>(self, f: F) -> F::Output {
        TRACE.scope(self, f).await
    }

    /// `tokio::spawn`, keeping the trace: a spawned task starts without one, so what it writes would
    /// name no request.
    pub fn spawn<F>(work: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        tokio::spawn(TRACE.try_with(Trace::clone).unwrap_or_default().scope(work))
    }

    /// The operation the current work belongs to (the request or job that started it), if any: what a
    /// person is shown to quote, and what an operator looks everything up by (FEO-079).
    pub fn operation() -> Option<String> {
        TRACE.try_with(|t| t.correlation.clone()).ok().filter(|c| !c.is_empty())
    }

    /// The current trace, with what it sets off at `priority`.
    pub fn at_priority(priority: i16) -> Trace {
        Trace { priority: Some(priority), ..TRACE.try_with(Trace::clone).unwrap_or_default() }
    }
}

/// `settings` (their parameters start at `$5`) with the current [`Trace`] set beside them.
fn carrying(settings: &str) -> Query<'static, Postgres, PgArguments> {
    let t = TRACE.try_with(Trace::clone).unwrap_or_default();
    // The depth is a number, written in place, so the settings' parameters still start at $5.
    sqlx::query(AssertSqlSafe(format!(
        "select set_config('ferrum.correlation', $1, true), set_config('ferrum.causation', $2, true), \
         set_config('ferrum.traceparent', $3, true), set_config('ferrum.priority', $4, true), \
         set_config('ferrum.depth', '{}', true), {settings}",
        t.depth
    )))
    .bind(t.correlation)
    .bind(t.causation.map(|c| c.to_string()).unwrap_or_default())
    .bind(t.traceparent.unwrap_or_default())
    .bind(t.priority.map(|p| p.to_string()).unwrap_or_default())
}

#[derive(Debug, Clone)]
pub struct DbConfig {
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
    /// Applied per transaction; protects the database from runaway queries.
    pub statement_timeout_ms: u64,
    /// Report/list queries may run longer than writes.
    pub report_timeout_ms: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        DbConfig {
            url: "postgres://ferrum:ferrum@localhost:5432/ferrum_dev".into(),
            max_connections: 20,
            min_connections: 1,
            // Bursts on one hot row (e.g. a popular item) queue behind its lock while holding a
            // connection; give them time before shedding load with 503 + Retry-After.
            acquire_timeout: Duration::from_secs(20),
            statement_timeout_ms: 15_000,
            report_timeout_ms: 60_000,
        }
    }
}

#[derive(Clone)]
pub struct Db {
    pub pool: sqlx::PgPool,
    /// A few connections for short autonomous writes made while a request's own transaction is
    /// open (non-gapless numbering). Taking them from `pool` could deadlock the pool under load:
    /// every connection held by a transaction that waits for a second one.
    pub side: sqlx::PgPool,
    pub config: DbConfig,
    /// This region does not hold the right to write (`recovery`): every transaction is read-only, so
    /// nothing has to remember to check.
    fenced: Arc<AtomicBool>,
}

impl Db {
    pub fn fence(&self, fenced: bool) {
        if self.fenced.swap(fenced, Ordering::SeqCst) != fenced {
            tracing::warn!(fenced, "this region's right to write changed");
        }
    }

    pub fn fenced(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    async fn stay_fenced(&self, tx: &mut Tx) -> Result<()> {
        if self.fenced() {
            sqlx::query("set transaction read only").execute(&mut **tx).await?;
        }
        Ok(())
    }

    pub async fn connect(config: DbConfig) -> Result<Db> {
        let opts = PgConnectOptions::from_str(&config.url)
            .map_err(|e| Error::Invalid(format!("invalid database url: {e}")))?
            .application_name("ferrum")
            .options([("timezone", "UTC"), ("lock_timeout", "10s"), ("idle_in_transaction_session_timeout", "60s")]);
        let side = PgPoolOptions::new()
            .max_connections(SIDE_CONNECTIONS)
            .min_connections(0)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(60)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .connect_lazy_with(opts.clone());
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(300)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .test_before_acquire(true)
            .connect_with(opts)
            .await?;
        Ok(Db { pool, side, config, fenced: Arc::default() })
    }

    /// Applies the kernel's own SQL migrations (idempotent; safe with several replicas).
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await.map_err(|e| Error::Internal(format!("kernel migration failed: {e}")))
    }

    /// Versions of the kernel's SQL migrations this database has applied (none for a database no migration has touched).
    pub async fn applied_migrations(&self) -> Result<Vec<i64>> {
        let there: bool = sqlx::query_scalar("select to_regclass('_sqlx_migrations') is not null").fetch_one(&self.pool).await?;
        if !there {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar("select version from _sqlx_migrations where success order by version").fetch_all(&self.pool).await?)
    }

    /// Versions of this build's SQL migrations the database has not applied.
    pub async fn pending_migrations(&self) -> Result<Vec<i64>> {
        let applied = self.applied_migrations().await?;
        let ours = sqlx::migrate!("./migrations");
        Ok(ours.iter().filter(|m| m.migration_type.is_up_migration() && !applied.contains(&m.version)).map(|m| m.version).collect())
    }

    /// Counts one request for `key` in the current fixed `window`, shared by every replica
    /// (`rate_window`), and returns the window's count so far.
    pub async fn count_hit(&self, key: &str, window: Duration) -> Result<i64> {
        let key: String = key.chars().take(300).collect();
        let n: i32 = sqlx::query_scalar(
            "insert into rate_window (key, window_start, hits) values ($1, to_timestamp(floor(extract(epoch from now()) / $2) * $2), 1) \
             on conflict (key, window_start) do update set hits = rate_window.hits + 1 returning hits",
        )
        .bind(key)
        .bind(window.as_secs().max(1) as f64)
        .fetch_one(&self.pool)
        .await?;
        Ok(n.into())
    }

    /// Forgets rate-limit windows that ended over an hour ago.
    pub async fn sweep_rate_windows(&self) -> Result<u64> {
        Ok(sqlx::query("delete from rate_window where window_start < now() - interval '1 hour'").execute(&self.pool).await?.rows_affected())
    }

    /// A tenant transaction on the side pool, for a short autonomous write while another
    /// transaction of the same request is open.
    pub async fn side_tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        let mut tx = self.side.begin().await?;
        carrying("set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)")
            .bind(tenant.to_string())
            .bind(self.config.statement_timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        self.stay_fenced(&mut tx).await?;
        Ok(tx)
    }

    /// A transaction isolated to one tenant by RLS.
    pub async fn tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        self.tenant_tx_with_timeout(tenant, self.config.statement_timeout_ms).await
    }

    pub async fn tenant_tx_with_timeout(&self, tenant: Uuid, timeout_ms: u64) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        carrying("set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)")
            .bind(tenant.to_string())
            .bind(timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        self.stay_fenced(&mut tx).await?;
        Ok(tx)
    }

    /// A transaction that bypasses tenant isolation. Only for cluster jobs (outbox, provisioning, login lookup).
    pub async fn system_tx(&self) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        carrying("set_config('ferrum.system', 'on', true), set_config('statement_timeout', $5, true)")
            .bind(self.config.statement_timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        self.stay_fenced(&mut tx).await?;
        Ok(tx)
    }

    /// System transaction pinned to one tenant: RLS still allows everything, but code that relies on
    /// `ferrum_tenant()` (defaults, policies in package SQL) sees the right tenant.
    pub async fn system_tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        carrying("set_config('ferrum.system', 'on', true), set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)")
            .bind(tenant.to_string())
            .bind(self.config.statement_timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        self.stay_fenced(&mut tx).await?;
        Ok(tx)
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// Runs `f` again (with jittered exponential backoff) when Postgres reports a serialization
/// failure, deadlock or lock timeout. `f` must be safe to repeat: it must open its own transaction.
pub async fn with_retry<T, F, Fut>(mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    const ATTEMPTS: u32 = 5;
    let mut attempt = 0;
    loop {
        match f().await {
            Err(e) if e.is_retryable() && attempt + 1 < ATTEMPTS => {
                attempt += 1;
                let base = 20u64 << attempt;
                let jitter = rand::random::<u64>() % base;
                tracing::debug!(attempt, error = %e, "retrying after transient conflict");
                tokio::time::sleep(Duration::from_millis(base + jitter)).await;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_spawned_task_keeps_the_trace_it_was_spawned_under() {
        let trace = Trace { correlation: "req-1".into(), ..Default::default() };
        let seen = trace.clone().scope(async { Trace::spawn(async { TRACE.with(|t| t.correlation.clone()) }).await.unwrap() }).await;
        assert_eq!(seen, "req-1");
        let bare = trace.scope(async { tokio::spawn(async { TRACE.try_with(|t| t.correlation.clone()).is_ok() }).await.unwrap() }).await;
        assert!(!bare, "which is why this exists");
    }
}
