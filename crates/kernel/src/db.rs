//! Connection pool, tenant-scoped transactions and retry on transient conflicts.

use crate::error::{Error, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Postgres, Transaction};
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

pub type Tx = Transaction<'static, Postgres>;

/// Connections in the side pool (see [`Db::side`]); count them in the database's connection budget.
pub const SIDE_CONNECTIONS: u32 = 4;

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
}

impl Db {
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
        Ok(Db { pool, side, config })
    }

    /// Applies the kernel's own SQL migrations (idempotent; safe with several replicas).
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await.map_err(|e| Error::Internal(format!("kernel migration failed: {e}")))
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
        sqlx::query("select set_config('ferrum.tenant_id', $1, true), set_config('statement_timeout', $2, true)")
            .bind(tenant.to_string())
            .bind(self.config.statement_timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// A transaction isolated to one tenant by RLS.
    pub async fn tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        self.tenant_tx_with_timeout(tenant, self.config.statement_timeout_ms).await
    }

    pub async fn tenant_tx_with_timeout(&self, tenant: Uuid, timeout_ms: u64) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("select set_config('ferrum.tenant_id', $1, true), set_config('statement_timeout', $2, true)")
            .bind(tenant.to_string())
            .bind(timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// A transaction that bypasses tenant isolation. Only for cluster jobs (outbox, provisioning, login lookup).
    pub async fn system_tx(&self) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("select set_config('ferrum.system', 'on', true), set_config('statement_timeout', $1, true)")
            .bind(self.config.statement_timeout_ms.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// System transaction pinned to one tenant: RLS still allows everything, but code that relies on
    /// `ferrum_tenant()` (defaults, policies in package SQL) sees the right tenant.
    pub async fn system_tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "select set_config('ferrum.system', 'on', true), set_config('ferrum.tenant_id', $1, true), \
             set_config('statement_timeout', $2, true)",
        )
        .bind(tenant.to_string())
        .bind(self.config.statement_timeout_ms.to_string())
        .execute(&mut *tx)
        .await?;
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
