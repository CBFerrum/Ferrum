//! Connection pool, tenant-scoped transactions and retry on transient conflicts.

use crate::error::{Error, Result};
use sqlx::postgres::{PgArguments, PgConnectOptions, PgListener, PgNotification, PgPoolOptions};
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

/// The server's LISTEN loops, each holding one connection of [`Db::direct`] for as long as it runs: metadata and users
/// (kernel), plugin sockets, live updates and co-editing (server).
pub const LISTENERS: u32 = 5;

/// What [`Db::direct`] holds besides the listeners and apps' upkeep: one session lock at a time (app revocations; at
/// start the migrations, the schema sync and the upgrade steps, one after another). Its own pool, when the direct address
/// differs from the main one, opens at most `LISTENERS + DIRECT_LOCKS + upkeep at once` ([`DbConfig::direct_connections`]).
pub const DIRECT_LOCKS: u32 = 1;

/// Session settings every kernel transaction sets for itself (the `options` a direct connection also starts with): a
/// connection pooler in transaction mode gives each transaction whichever server connection is free, so nothing set on
/// one session reaches the next transaction.
const LOCK_TIMEOUT: &str = "10s";
const IDLE_IN_TRANSACTION: &str = "60s";

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
    /// The zone of the request behind this work, when the zone narrows staff (crate::zones, level 2):
    /// set once the request's context is built ([`Trace::mark_zone`]), and written on the jobs and
    /// events it queues (`ferrum.zone`), so work done later for it is done in that zone.
    pub zone: Arc<std::sync::OnceLock<String>>,
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

    /// Records the zone the current request's context was built in (the first one holds).
    pub fn mark_zone(zone: &str) {
        let _ = TRACE.try_with(|t| t.zone.set(zone.to_string()));
    }

    /// The zone of the current work (none: inside).
    pub fn zone() -> Option<String> {
        TRACE.try_with(|t| t.zone.get().cloned()).ok().flatten()
    }

    /// A zone for a trace of work queued in one ([`Trace::zone`]).
    pub fn in_zone(zone: Option<String>) -> Arc<std::sync::OnceLock<String>> {
        Arc::new(zone.map(std::sync::OnceLock::from).unwrap_or_default())
    }

    /// The current trace, with what it sets off at `priority`.
    pub fn at_priority(priority: i16) -> Trace {
        Trace { priority: Some(priority), ..TRACE.try_with(Trace::clone).unwrap_or_default() }
    }
}

/// `settings` (their parameters start at `$5`) with the current [`Trace`] and the transaction's lock and idle timeouts
/// set beside them, in one round trip. Its row's `moving` says whether a planned move has fenced the database
/// (`ferrum_moving()`, asked when `moves` is set; see crates/kernel/src/moves.rs), and `frozen` whether the workspace in
/// `$5` is moving to another installation (`ferrum_workspace_frozen`, asked when `frozen` is set: workspace_move.rs).
fn carrying(settings: &str, moves: bool, frozen: bool) -> Query<'static, Postgres, PgArguments> {
    let t = TRACE.try_with(Trace::clone).unwrap_or_default();
    // The depth is a number and the zone a name of lowercase letters, digits and dashes (crate::zones checks them; any
    // other is left out), written in place, so the settings' parameters still start at $5.
    let zone = t.zone.get().filter(|z| z.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')).map_or("", String::as_str);
    sqlx::query(AssertSqlSafe(format!(
        "select set_config('ferrum.correlation', $1, true), set_config('ferrum.causation', $2, true), \
         set_config('ferrum.traceparent', $3, true), set_config('ferrum.priority', $4, true), \
         set_config('ferrum.depth', '{}', true), set_config('ferrum.zone', '{zone}', true), set_config('lock_timeout', '{LOCK_TIMEOUT}', true), \
         set_config('idle_in_transaction_session_timeout', '{IDLE_IN_TRANSACTION}', true), {settings}, {} as moving, {} as frozen",
        t.depth,
        if moves { "ferrum_moving()" } else { "false" },
        if frozen { "ferrum_workspace_frozen($5)" } else { "false" }
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
    /// `FERRUM_DATABASE_DIRECT_URL`: where whatever needs one session for longer than a transaction connects (LISTEN,
    /// session advisory locks, migrations and the schema sync), when `url` is a connection pooler in transaction mode.
    /// `None`: `FERRUM_TEST_DATABASE_DIRECT_URL` when a test sets it, else `url` itself (the server always says).
    pub direct_url: Option<String>,
    /// The most connections the direct side opens when it is a pool of its own:
    /// [`LISTENERS`] + [`DIRECT_LOCKS`] + apps' upkeep at once (`FERRUM_UPKEEP_CONCURRENCY`, default 2).
    pub direct_connections: u32,
    /// `FERRUM_DATABASE_READ_URL`: a replica that reports and exports may read from ([`Db::read_tx`]). `None`: none.
    pub read_url: Option<String>,
    /// `FERRUM_DATABASE_READ_MAX_LAG_SECS`: past this replay lag those reads go to the primary.
    pub read_max_lag: Duration,
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
            direct_url: None,
            direct_connections: LISTENERS + DIRECT_LOCKS + 2,
            read_url: None,
            read_max_lag: Duration::from_secs(10),
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
    /// Connections that keep one session (LISTEN, session advisory locks, migrations, the schema sync): `pool` itself,
    /// unless `FERRUM_DATABASE_DIRECT_URL` names another address than `DATABASE_URL` (a pooler's), and then a small pool
    /// of its own there ([`DbConfig::direct_connections`]).
    pub direct: sqlx::PgPool,
    /// The read address's pool and what it last found ([`Db::read_tx`]), when one is set.
    pub read: Option<Arc<crate::replica::Reads>>,
    pub config: DbConfig,
    /// This region does not hold the right to write (`recovery`): every transaction is read-only, so
    /// nothing has to remember to check.
    fenced: Arc<AtomicBool>,
    /// This database has `ferrum_moving()` (the planned move's migration is applied), so transactions ask it
    /// ([`Db::ask_moves`]).
    moves: Arc<AtomicBool>,
    /// This database has `ferrum_workspace_frozen()`, so tenant transactions ask it ([`Db::ask_moves`]).
    frozen: Arc<AtomicBool>,
    /// The server is starting: a planned move's fence does not apply yet, so that a fenced database's server starts (and
    /// a task runs on it) and then serves read-only. Off unless the server says so.
    starting: Arc<AtomicBool>,
    pooled: bool,
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

    /// Set while a server starts: a planned move's fence is not applied meanwhile (moves.rs).
    pub fn starting(&self, starting: bool) {
        self.starting.store(starting, Ordering::SeqCst);
    }

    async fn stay_fenced(&self, tx: &mut Tx, row: sqlx::postgres::PgRow) -> Result<()> {
        use sqlx::Row;
        let moving: bool = row.try_get("moving")?;
        // A workspace moving to another installation takes no changes either, from anyone (workspace_move.rs).
        let frozen: bool = row.try_get("frozen")?;
        if self.fenced() || frozen || (moving && !self.starting.load(Ordering::SeqCst)) {
            sqlx::query("set transaction read only").execute(&mut **tx).await?;
        }
        Ok(())
    }

    pub async fn connect(config: DbConfig) -> Result<Db> {
        let url = |u: &str, what: &str| {
            PgConnectOptions::from_str(u).map(|o| o.application_name("ferrum")).map_err(|e| Error::Invalid(format!("invalid {what}: {e}")))
        };
        // A direct connection starts with its session settings; one through a pooler must not send them (PgBouncer
        // refuses startup parameters it does not track) and gets them from each transaction instead (`carrying`).
        let session = |o: PgConnectOptions| {
            o.options([("timezone", "UTC"), ("lock_timeout", LOCK_TIMEOUT), ("idle_in_transaction_session_timeout", IDLE_IN_TRANSACTION)])
        };
        let direct_url = config
            .direct_url
            .clone()
            .or_else(|| std::env::var("FERRUM_TEST_DATABASE_DIRECT_URL").ok())
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty() && *u != config.url);
        let main = url(&config.url, "database url")?;
        // `extra_float_digits` is a startup parameter PgBouncer refuses too; PostgreSQL 12 and later print floats exactly
        // without it.
        let main = if direct_url.is_some() { main.extra_float_digits(None) } else { session(main) };
        let side = PgPoolOptions::new()
            .max_connections(SIDE_CONNECTIONS)
            .min_connections(0)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(60)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .connect_lazy_with(main.clone());
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(300)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .test_before_acquire(true)
            .connect_with(main)
            .await?;
        let direct = match &direct_url {
            None => pool.clone(),
            Some(d) => PgPoolOptions::new()
                .max_connections(config.direct_connections.max(LISTENERS + DIRECT_LOCKS))
                .min_connections(0)
                .acquire_timeout(config.acquire_timeout)
                .idle_timeout(Some(Duration::from_secs(60)))
                .max_lifetime(Some(Duration::from_secs(1800)))
                .test_before_acquire(true)
                // Connected now, so a start with a direct address that does not answer fails at once, saying which.
                .connect_with(session(url(d, "direct database url (FERRUM_DATABASE_DIRECT_URL)")?))
                .await
                .map_err(|e| Error::Internal(format!("the direct database address (FERRUM_DATABASE_DIRECT_URL) does not answer: {e}")))?,
        };
        // Connected when first used: a read address that does not answer sends reads to the primary, it stops nothing.
        let read = match config.read_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            None => None,
            Some(r) => Some(Arc::new(crate::replica::Reads::new(
                PgPoolOptions::new()
                    .max_connections(crate::replica::READ_CONNECTIONS)
                    .min_connections(0)
                    .acquire_timeout(crate::replica::CHECKED_FOR)
                    .idle_timeout(Some(Duration::from_secs(60)))
                    .max_lifetime(Some(Duration::from_secs(1800)))
                    .test_before_acquire(true)
                    // Like the main pool through a pooler: each transaction sets what it needs.
                    .connect_lazy_with(url(r, "read database url (FERRUM_DATABASE_READ_URL)")?.extra_float_digits(None)),
                config.read_max_lag,
            ))),
        };
        Ok(Db {
            pooled: direct_url.is_some(),
            pool,
            side,
            direct,
            read,
            config,
            fenced: Arc::default(),
            moves: Arc::default(),
            frozen: Arc::default(),
            starting: Arc::default(),
        })
    }

    /// Whether `direct` is a pool of its own: the main pool then goes through a pooler.
    pub fn pooled(&self) -> bool {
        self.pooled
    }

    /// LISTENs on `channel` over a connection of [`Db::direct`], held until the listener is dropped.
    pub async fn listen(&self, channel: &str) -> Result<PgListener> {
        let mut l = PgListener::connect_with(&self.direct).await?;
        // A lost connection is reported (see `notification`), never quietly replaced.
        l.eager_reconnect(false);
        l.listen(channel).await?;
        Ok(l)
    }

    /// Applies the kernel's own SQL migrations (idempotent; safe with several replicas: sqlx holds a session lock while
    /// it migrates, so on the direct address).
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.direct).await.map_err(|e| Error::Internal(format!("kernel migration failed: {e}")))
    }

    /// Transactions ask `ferrum_moving()` from here on when this database has it (Kernel::boot, once the schema is
    /// settled; a guest's newer database has it too).
    pub async fn ask_moves(&self) -> Result<()> {
        let (there, frozen): (bool, bool) =
            sqlx::query_as("select to_regprocedure('ferrum_moving()') is not null, to_regprocedure('ferrum_workspace_frozen(text)') is not null")
                .fetch_one(&self.pool)
                .await?;
        self.moves.store(there, Ordering::SeqCst);
        self.frozen.store(frozen, Ordering::SeqCst);
        Ok(())
    }

    fn moves(&self) -> bool {
        self.moves.load(Ordering::SeqCst)
    }

    fn frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
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

    /// Versions the database has applied that this build does not have: a newer build migrated it.
    pub async fn unknown_migrations(&self) -> Result<Vec<i64>> {
        let ours = sqlx::migrate!("./migrations");
        Ok(self.applied_migrations().await?.into_iter().filter(|v| !ours.iter().any(|m| m.version == *v)).collect())
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
        let row = carrying("set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)", self.moves(), self.frozen())
            .bind(tenant.to_string())
            .bind(self.config.statement_timeout_ms.to_string())
            .fetch_one(&mut *tx)
            .await?;
        self.stay_fenced(&mut tx, row).await?;
        Ok(tx)
    }

    /// A transaction isolated to one tenant by RLS.
    pub async fn tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        self.tenant_tx_with_timeout(tenant, self.config.statement_timeout_ms).await
    }

    pub async fn tenant_tx_with_timeout(&self, tenant: Uuid, timeout_ms: u64) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        let row = as_tenant(&mut tx, tenant, timeout_ms, self.moves(), self.frozen()).await?;
        self.stay_fenced(&mut tx, row).await?;
        Ok(tx)
    }

    /// Runs a whole read (its transactions opened with [`Db::read_tx`]) and, when it failed after reading from the read
    /// address, once more on the primary ([`crate::replica::once_more_on_primary`]).
    pub async fn reading<T, F, Fut>(&self, read: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut read = read;
        match &self.read {
            Some(r) => crate::replica::once_more_on_primary(r, read).await,
            None => read().await,
        }
    }

    /// A read-only tenant transaction for reading only: reports, and the reading of a workspace's export. On the read
    /// address (`FERRUM_DATABASE_READ_URL`) when it is a replica within its lag limit that has replayed everything the
    /// primary had committed when this began, so that whoever has just written sees it; otherwise on the primary.
    /// Never for a read that a write in the same transaction must see.
    pub async fn read_tx(&self, tenant: Uuid, timeout_ms: u64) -> Result<Tx> {
        use crate::replica::{Health, Served};
        if let Some(r) = self.read.as_ref().filter(|_| !crate::replica::primary_only()) {
            let why = match r.health().await {
                Health::Fresh(lag) => {
                    // Where the primary is now: the replica must have replayed that far.
                    let at: String =
                        sqlx::query_scalar("select (case when pg_is_in_recovery() then pg_last_wal_replay_lsn() else pg_current_wal_lsn() end)::text")
                            .fetch_one(&self.pool)
                            .await?;
                    match on_replica(r, tenant, timeout_ms, &at).await {
                        Ok(Some(tx)) => {
                            r.served(Served::Replica);
                            crate::replica::note(lag);
                            return Ok(tx);
                        }
                        Ok(None) => Served::Behind,
                        Err(e) => {
                            tracing::warn!("the read address (FERRUM_DATABASE_READ_URL) failed, reading from the primary: {e}");
                            r.seen(Health::Down);
                            Served::Down
                        }
                    }
                }
                Health::Lagging => Served::Lagging,
                Health::NotReplica => Served::NotReplica,
                Health::Down => Served::Down,
            };
            r.served(why);
        }
        let mut tx = self.tenant_tx_with_timeout(tenant, timeout_ms).await?;
        sqlx::query("set transaction read only").execute(&mut *tx).await?;
        Ok(tx)
    }

    /// A transaction that bypasses tenant isolation. Only for cluster jobs (outbox, provisioning, login lookup).
    pub async fn system_tx(&self) -> Result<Tx> {
        self.system_tx_asking(self.moves()).await
    }

    /// A system transaction that a planned move's fence leaves writable: only for the move's own steps (moves.rs).
    pub(crate) async fn system_tx_through_move(&self) -> Result<Tx> {
        self.system_tx_asking(false).await
    }

    async fn system_tx_asking(&self, moves: bool) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        let row = carrying("set_config('ferrum.system', 'on', true), set_config('statement_timeout', $5, true)", moves, false)
            .bind(self.config.statement_timeout_ms.to_string())
            .fetch_one(&mut *tx)
            .await?;
        self.stay_fenced(&mut tx, row).await?;
        Ok(tx)
    }

    /// System transaction pinned to one tenant: RLS still allows everything, but code that relies on
    /// `ferrum_tenant()` (defaults, policies in package SQL) sees the right tenant.
    pub async fn system_tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        let mut tx = self.pool.begin().await?;
        let row = carrying(
            "set_config('ferrum.system', 'on', true), set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)",
            self.moves(),
            self.frozen(),
        )
        .bind(tenant.to_string())
        .bind(self.config.statement_timeout_ms.to_string())
        .fetch_one(&mut *tx)
        .await?;
        self.stay_fenced(&mut tx, row).await?;
        Ok(tx)
    }

    /// Closes every pool, waiting for the connections in use to come back.
    pub async fn close(&self) {
        self.pool.close().await;
        self.side.close().await;
        self.direct.close().await;
        if let Some(r) = &self.read {
            r.pool.close().await;
        }
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("select 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// Sets the workspace and the statement timeout on a transaction, as every tenant transaction does.
async fn as_tenant(tx: &mut Tx, tenant: Uuid, timeout_ms: u64, moves: bool, frozen: bool) -> Result<sqlx::postgres::PgRow> {
    Ok(carrying("set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)", moves, frozen)
        .bind(tenant.to_string())
        .bind(timeout_ms.to_string())
        .fetch_one(&mut **tx)
        .await?)
}

/// A read-only tenant transaction on the read address, once it has replayed up to `at` (within
/// [`crate::replica::CATCH_UP`]; `None` past it).
async fn on_replica(r: &crate::replica::Reads, tenant: Uuid, timeout_ms: u64, at: &str) -> Result<Option<Tx>> {
    let mut tx = r.pool.begin().await?;
    // A replica takes no changes anyway.
    as_tenant(&mut tx, tenant, timeout_ms, false, false).await?;
    sqlx::query("set transaction read only").execute(&mut *tx).await?;
    let started = std::time::Instant::now();
    loop {
        // A primary standing in for a replica (tests) is always caught up with itself.
        let caught: bool =
            sqlx::query_scalar("select (case when pg_is_in_recovery() then pg_last_wal_replay_lsn() else pg_current_wal_lsn() end) >= $1::pg_lsn")
                .bind(at)
                .fetch_one(&mut *tx)
                .await?;
        if caught {
            return Ok(Some(tx));
        }
        if started.elapsed() >= crate::replica::CATCH_UP {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The next notification on `l`; an error when its connection was lost, so that the caller recovers what it may have
/// missed. (sqlx's own `recv` would open a new connection and go on as if nothing had been missed.)
pub async fn notification(l: &mut PgListener) -> std::result::Result<PgNotification, sqlx::Error> {
    l.try_recv().await?.ok_or_else(|| sqlx::Error::Io(std::io::ErrorKind::ConnectionAborted.into()))
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
