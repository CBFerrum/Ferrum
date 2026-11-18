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

/// The fewest connections in the side pool (see [`Db::side`]); count them in the database's connection budget.
pub const SIDE_CONNECTIONS: u32 = 4;

/// The side pool's connections for a main pool of `main`: an eighth of it, and never fewer than [`SIDE_CONNECTIONS`]
/// (4 with the default pool of 30). The vault's verbs and short side writes (numbering) take one each, outside the
/// caller's transaction, so the side pool grows with the requests that can ask for one at once.
pub fn side_connections(main: u32) -> u32 {
    main.div_ceil(8).max(SIDE_CONNECTIONS)
}

/// How many connections of a main pool of `main` the event dispatcher and the webhook sender hold at most, together: a
/// quarter of it, at least one (`Kernel::background`). However much is queued, requests keep the rest.
pub fn background_connections(main: u32) -> u32 {
    (main / 4).max(1)
}

/// The server's LISTEN loops, each holding one connection of [`Db::direct`] for as long as it runs: metadata and users
/// (kernel), plugin sockets, live updates and co-editing (server).
pub const LISTENERS: u32 = 5;

/// What [`Db::direct`] holds besides the listeners: one session lock at a time (app stores' revocations; at start the
/// migrations, the schema sync and the copy set-up, one after the other). Its pool opens at most `LISTENERS +
/// DIRECT_LOCKS` ([`DbConfig::direct_connections`]). The locks taken on a connection detached from it (the two-version
/// window's, a bucket's set-up, the old files directory's rescue and move, a workspace's move) are outside that count.
pub const DIRECT_LOCKS: u32 = 1;

/// The session locks a server replica takes on a connection detached from [`Db::direct`], so outside every pool's
/// maximum, at most at once: at start the two-version window's (crate::window, then its start hold, one after the
/// other); while it runs the bucket's set-up (until it has worked once) and the old files directory's rescue (hourly,
/// while that directory is read) (server/src/store.rs). A task that moves a workspace or the files holds one more, in a
/// process of its own.
pub const DETACHED_LOCKS: u32 = 2;

/// How many workspaces' app upkeep a replica runs at once unless `FERRUM_UPKEEP_CONCURRENCY` says otherwise
/// ([`DbConfig::upkeep`]).
pub const UPKEEP_AT_ONCE: u32 = 4;

/// The most rows one statement of a purge deletes ([`Db::in_batches`]).
pub const PURGE_BATCH: u64 = 5000;

/// Session settings every kernel transaction sets for itself (the `options` a direct connection also starts with): a
/// connection pooler in transaction mode gives each transaction whichever server connection is free, so nothing set on
/// one session reaches the next transaction.
const LOCK_TIMEOUT: &str = "10s";
/// The longest a transaction waits between two statements before PostgreSQL ends its session: what a transaction waits
/// for outside the database (a PDF render, crate::pdf) gives up well within it.
pub(crate) const IDLE_IN_TRANSACTION: Duration = Duration::from_secs(60);

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
    /// The workspaces this work has served so far (a request's context, a workspace's transaction), shared with the
    /// tasks it spawns: what the kernel's outbound client asks about (crate::sandbox).
    pub serving: Arc<std::sync::Mutex<Vec<Uuid>>>,
    /// The zone of the request behind this work, when the zone narrows staff (crate::zones, level 2):
    /// set once the request's context is built ([`Trace::mark_zone`]), and written on the jobs and
    /// events it queues (`ferrum.zone`), so work done later for it is done in that zone.
    pub zone: Arc<std::sync::OnceLock<String>>,
    /// Whether the request behind this work had proved who its person is (crate::controls::Proof::mark): written on
    /// the jobs it queues (`ferrum.step_up`), so a job does later only what the request could.
    pub step_up: Arc<std::sync::Mutex<&'static str>>,
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

    /// Notes that the work in hand serves `tenant` (nothing outside a trace).
    pub fn serving(tenant: Uuid) {
        let _ = TRACE.try_with(|t| {
            if let Ok(mut w) = t.serving.lock()
                && !w.contains(&tenant)
            {
                w.push(tenant);
            }
        });
    }

    /// The workspaces the work in hand has served; `None` when it has served none (start-up, operator tasks). When it
    /// cannot tell (a poisoned lock), the nil id, which no workspace has, so every check refuses.
    pub(crate) fn served() -> Option<Vec<Uuid>> {
        TRACE.try_with(|t| t.serving.lock().map(|w| w.clone()).unwrap_or_else(|_| vec![Uuid::nil()])).ok().filter(|w| !w.is_empty())
    }

    /// Records the zone the current request's context was built in (the first one holds).
    pub fn mark_zone(zone: &str) {
        let _ = TRACE.try_with(|t| t.zone.set(zone.to_string()));
    }

    /// Records whether the current request had stepped up (crate::controls::Proof::mark); the last one holds.
    pub fn mark_step_up(mark: &'static str) {
        let _ = TRACE.try_with(|t| {
            if let Ok(mut m) = t.step_up.lock() {
                *m = mark;
            }
        });
    }

    /// A step-up mark for a trace of work queued with one.
    pub fn with_step_up(mark: Option<&str>) -> Arc<std::sync::Mutex<&'static str>> {
        Arc::new(std::sync::Mutex::new(match mark {
            Some("no") => "no",
            Some("machine") => "machine",
            _ => "",
        }))
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
    // One of three fixed words (Trace::mark_step_up); a poisoned lock writes `no`, so queued work is held to it.
    let step_up = t.step_up.lock().map(|m| *m).unwrap_or("no");
    let idle = IDLE_IN_TRANSACTION.as_secs();
    sqlx::query(AssertSqlSafe(format!(
        "select set_config('ferrum.correlation', $1, true), set_config('ferrum.causation', $2, true), \
         set_config('ferrum.traceparent', $3, true), set_config('ferrum.priority', $4, true), \
         set_config('ferrum.depth', '{}', true), set_config('ferrum.zone', '{zone}', true), set_config('ferrum.step_up', '{step_up}', true), set_config('lock_timeout', '{LOCK_TIMEOUT}', true), \
         set_config('idle_in_transaction_session_timeout', '{idle}s', true), {settings}, {} as moving, {} as frozen",
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
    /// `None`: `FERRUM_TEST_DATABASE_DIRECT_URL` when a test sets it, else `url` at `FERRUM_TEST_DATABASE_DIRECT_HOST`
    /// when a test sets that, else `url` itself (the server always says).
    pub direct_url: Option<String>,
    /// The most connections the direct pool opens: [`LISTENERS`] + [`DIRECT_LOCKS`].
    pub direct_connections: u32,
    /// How many workspaces' app upkeep the replica runs at once (`FERRUM_UPKEEP_CONCURRENCY`, [`UPKEEP_AT_ONCE`]): each
    /// holds a connection of the main pool while it runs, which the doctor counts ([`crate::doctor::background`]).
    pub upkeep: u32,
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
            direct_connections: LISTENERS + DIRECT_LOCKS,
            upkeep: UPKEEP_AT_ONCE,
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
    /// Connections that keep one session (LISTEN, session advisory locks, migrations, the schema sync): a small pool of
    /// its own ([`DbConfig::direct_connections`]), at `FERRUM_DATABASE_DIRECT_URL` when that names another address than
    /// `DATABASE_URL` (a pooler's), else at `DATABASE_URL` itself, so that what holds a session never takes a request's
    /// connection. Each pool's connections say which it is (`application_name`: `ferrum`, `ferrum-side`, `ferrum-direct`,
    /// `ferrum-read`).
    pub direct: sqlx::PgPool,
    /// The read address's pool and what it last found ([`Db::read_tx`]), when one is set.
    pub read: Option<Arc<crate::replica::Reads>>,
    pub config: DbConfig,
    /// This region does not hold the right to write (`recovery`): every transaction is read-only, so
    /// nothing has to remember to check.
    fenced: Arc<AtomicBool>,
    /// Transactions ask `ferrum_moving()` ([`Db::ask_moves`]).
    moves: Arc<AtomicBool>,
    /// Tenant transactions ask `ferrum_workspace_frozen()` ([`Db::ask_moves`]).
    frozen: Arc<AtomicBool>,
    /// The server is starting: a planned move's fence does not apply yet, so that a fenced database's server starts (and
    /// a task runs on it) and then serves read-only. Off unless the server says so.
    starting: Arc<AtomicBool>,
    pooled: bool,
    /// This database's vault as this process holds it (crate::vault): closed until [`crate::vault::open`] opens it.
    pub(crate) vault: Arc<crate::vault::Root>,
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
        let idle = format!("{}s", IDLE_IN_TRANSACTION.as_secs());
        // The TCP keepalives are per-session server settings: a replica that vanishes leaves its session locks and LISTEN
        // backends until PostgreSQL notices the dead peer, 30 + 3 x 10 s here (the OS default is two hours). A server that
        // refuses them fails the start, and its own error names the setting.
        let session = |o: PgConnectOptions| {
            o.options([
                ("timezone", "UTC"),
                ("lock_timeout", LOCK_TIMEOUT),
                ("idle_in_transaction_session_timeout", idle.as_str()),
                ("tcp_keepalives_idle", "30"),
                ("tcp_keepalives_interval", "10"),
                ("tcp_keepalives_count", "3"),
            ])
        };
        // Tests' own direct addresses, read by a debug build only (a release build reads no test's variable).
        let test = |name: &str| if cfg!(debug_assertions) { std::env::var(name).ok() } else { None };
        let direct_url = config
            .direct_url
            .clone()
            .or_else(|| test("FERRUM_TEST_DATABASE_DIRECT_URL"))
            // Suites on several databases through a pooler (scripts/local_ci.sh managed): each one's own, by host.
            .or_else(|| test("FERRUM_TEST_DATABASE_DIRECT_HOST").and_then(|h| with_host(&config.url, &h).ok()))
            .map(|u| u.trim().to_string())
            .filter(|u| !u.is_empty() && *u != config.url);
        let main = url(&config.url, "database url")?;
        // The direct side: the direct address when it differs, else the main one, with the session settings. Always a pool of
        // its own, so that the LISTEN loops and the session locks never hold a connection a request could use.
        let direct_options = match &direct_url {
            Some(d) => url(d, "direct database address (FERRUM_DATABASE_DIRECT_URL or FERRUM_DATABASE_DIRECT_HOST)")?,
            None => main.clone(),
        };
        let direct_options = session(direct_options).application_name("ferrum-direct");
        // `extra_float_digits` is a startup parameter PgBouncer refuses too; PostgreSQL 12 and later print floats exactly
        // without it.
        let main = if direct_url.is_some() { main.extra_float_digits(None) } else { session(main) };
        // Each pool names itself to PostgreSQL (`pg_stat_activity.application_name`), so that its connections can be counted.
        let side = PgPoolOptions::new()
            .max_connections(side_connections(config.max_connections))
            .min_connections(0)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(60)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .connect_lazy_with(main.clone().application_name("ferrum-side"));
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .min_connections(config.min_connections)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(300)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .test_before_acquire(true)
            .connect_with(main)
            .await
            .map_err(pooler_refused)?;
        let direct = PgPoolOptions::new()
            .max_connections(config.direct_connections.max(LISTENERS + DIRECT_LOCKS))
            .min_connections(0)
            .acquire_timeout(config.acquire_timeout)
            .idle_timeout(Some(Duration::from_secs(60)))
            .max_lifetime(Some(Duration::from_secs(1800)))
            .test_before_acquire(true);
        let direct = if direct_url.is_some() {
            // Connected now, so a start with a direct address that does not answer fails at once, saying which.
            direct.connect_with(direct_options).await.map_err(|e| {
                Error::Internal(format!("the direct database address (FERRUM_DATABASE_DIRECT_URL or FERRUM_DATABASE_DIRECT_HOST) does not answer: {e}"))
            })?
        } else {
            // The main address: the main pool has just answered, and no start is slowed by connections not yet needed.
            direct.connect_lazy_with(direct_options)
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
                    .connect_lazy_with(
                        url(r, "read database address (FERRUM_DATABASE_READ_URL or FERRUM_DATABASE_READ_HOST)")?
                            .extra_float_digits(None)
                            .application_name("ferrum-read"),
                    ),
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
            vault: Arc::default(),
        })
    }

    /// Whether `direct` is at another address than the main pool: the main pool then goes through a pooler.
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

    /// Transactions ask `ferrum_moving()` and `ferrum_workspace_frozen()` from here on: Kernel::boot says so once the
    /// schema is settled, before which an empty database has neither.
    pub fn ask_moves(&self) {
        self.moves.store(true, Ordering::SeqCst);
        self.frozen.store(true, Ordering::SeqCst);
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
        crate::sandbox::serving(tenant);
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
                        Ok(OnReplica::Caught(tx)) => {
                            r.served(Served::Replica);
                            crate::replica::note(lag);
                            return Ok(tx);
                        }
                        // Behind its own lag (a standby whose stream from the primary is cut has replayed all it got,
                        // so its lag reads 0): the reads after this one go to the primary at once, until the next look.
                        Ok(OnReplica::Behind) => {
                            r.seen(Health::Lagging);
                            Served::Behind
                        }
                        // Promoted since the last look: never read from as a replica.
                        Ok(OnReplica::NotReplica) => {
                            r.seen(Health::NotReplica);
                            Served::NotReplica
                        }
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

    /// A transaction of the vault's own module (crate::vault): the vault's tables are seen in it and in nothing else
    /// (`ferrum.vault`, transaction-local as every setting here). On the side pool: the vault is often asked inside a
    /// request's own transaction. `tenant`: the workspace whose trail it writes; none: the installation's scope, which
    /// writes as the system does.
    pub(crate) async fn vault_tx(&self, tenant: Option<Uuid>) -> Result<Tx> {
        let mut tx = self.side.begin().await?;
        let row = match tenant {
            Some(t) => carrying(
                "set_config('ferrum.vault', 'on', true), set_config('ferrum.tenant_id', $5, true), set_config('statement_timeout', $6, true)",
                self.moves(),
                false,
            )
            .bind(t.to_string()),
            None => carrying(
                "set_config('ferrum.vault', 'on', true), set_config('ferrum.system', 'on', true), set_config('statement_timeout', $5, true)",
                self.moves(),
                false,
            ),
        }
        .bind(self.config.statement_timeout_ms.to_string())
        .fetch_one(&mut *tx)
        .await?;
        self.stay_fenced(&mut tx, row).await?;
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

    /// Runs `batch` in a system transaction of its own, again and again, until it deletes fewer than [`PURGE_BATCH`] rows:
    /// however far behind a purge has fallen, no statement takes more than a batch, so none meets the statement timeout.
    /// `batch` deletes at most [`PURGE_BATCH`] rows and says how many it did.
    pub(crate) async fn in_batches(&self, mut batch: impl for<'a> FnMut(&'a mut Tx) -> futures::future::BoxFuture<'a, Result<u64>>) -> Result<u64> {
        let mut done = 0;
        loop {
            let mut tx = self.system_tx().await?;
            let n = batch(&mut tx).await?;
            tx.commit().await?;
            done += n;
            if n < PURGE_BATCH {
                return Ok(done);
            }
        }
    }

    /// Deletes from each table the rows its filter selects, in batches ([`Db::in_batches`]). A table that fails is passed
    /// over and the next one is still purged; the error names each that failed. The filters are the platform's own SQL
    /// (numbers written in, nothing anyone typed), and rows are taken by `ctid`, so no table here may be partitioned.
    pub(crate) async fn purge(&self, purges: &[(&str, String)]) -> Result<u64> {
        let (mut done, mut failed) = (0, Vec::new());
        for (table, filter) in purges {
            let sql = format!("delete from {table} where ctid = any(array(select ctid from {table} where {filter} limit {PURGE_BATCH}))");
            let deleted = self
                .in_batches(|tx| {
                    let sql = sql.clone();
                    Box::pin(async move { Ok(sqlx::query(AssertSqlSafe(sql)).execute(&mut **tx).await?.rows_affected()) })
                })
                .await;
            match deleted {
                Ok(n) => done += n,
                Err(e) => failed.push(format!("{table}: {e}")),
            }
        }
        if failed.is_empty() { Ok(done) } else { Err(Error::Internal(format!("purging {}", failed.join("; ")))) }
    }

    /// System transaction pinned to one tenant: RLS still allows everything, but code that relies on
    /// `ferrum_tenant()` (defaults, policies in package SQL) sees the right tenant.
    pub async fn system_tenant_tx(&self, tenant: Uuid) -> Result<Tx> {
        crate::sandbox::serving(tenant);
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

/// What [`on_replica`] found.
enum OnReplica {
    /// A read-only tenant transaction on the read address, which has replayed what the primary had committed.
    Caught(Tx),
    /// It had not replayed that far within [`crate::replica::CATCH_UP`].
    Behind,
    /// It replays nothing: promoted since the last look at it (or another database).
    NotReplica,
}

/// A read-only tenant transaction on the read address, once it has replayed up to `at`.
async fn on_replica(r: &crate::replica::Reads, tenant: Uuid, timeout_ms: u64, at: &str) -> Result<OnReplica> {
    let mut tx = r.pool.begin().await?;
    // A replica takes no changes anyway.
    as_tenant(&mut tx, tenant, timeout_ms, false, false).await?;
    sqlx::query("set transaction read only").execute(&mut *tx).await?;
    let started = std::time::Instant::now();
    loop {
        // Asked in the transaction that reads, whatever the last look said: a standby promoted since is no replica, and
        // its own position would pass for caught up. A primary standing in for a replica (tests) is caught up with itself.
        let (replaying, caught): (bool, bool) = sqlx::query_as(
            "select pg_is_in_recovery(), \
             coalesce((case when pg_is_in_recovery() then pg_last_wal_replay_lsn() else pg_current_wal_lsn() end) >= $1::pg_lsn, false)",
        )
        .bind(at)
        .fetch_one(&mut *tx)
        .await?;
        if !replaying && !r.standing_in() {
            return Ok(OnReplica::NotReplica);
        }
        if caught {
            return Ok(OnReplica::Caught(tx));
        }
        if started.elapsed() >= crate::replica::CATCH_UP {
            return Ok(OnReplica::Behind);
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

/// A connection pooler in transaction mode refuses the settings a session starts with, which an address given as
/// PostgreSQL's own sends (DATABASE_URL without a direct address; a move's, a sandbox's or an extraction's source,
/// which needs one session there): said plainly.
fn pooler_refused(e: sqlx::Error) -> Error {
    if e.to_string().contains("unsupported startup parameter") {
        return Error::Invalid(format!(
            "this address is a connection pooler, which refuses a session's settings ({e}): give PostgreSQL's own address here; \
             a server takes the pooler's in DATABASE_URL and PostgreSQL's in FERRUM_DATABASE_DIRECT_HOST"
        ));
    }
    e.into()
}

/// Where an outside database's certificate authority is read from when it is there (docs/plan/stateless.md, D1).
pub const DB_CA: &str = "/run/ferrum/db-ca.pem";

/// The outside database's certificate authority, when its file is there: [`DB_CA`] ([`run_file`]).
pub fn database_ca() -> Option<String> {
    run_file(DB_CA).map(|p| p.to_string_lossy().into_owned())
}

/// Where a file of the boot contract's is read: its fixed path under `/run/ferrum` (docs/plan/stateless.md, D1). A debug
/// build (tests, a developer's machine, the rehearsals) looks in `FERRUM_TEST_RUN_DIR` instead when that is set, for the
/// file of the same name; a release build never reads that variable.
pub fn run_path(fixed: &str) -> std::path::PathBuf {
    let elsewhere = if cfg!(debug_assertions) { std::env::var("FERRUM_TEST_RUN_DIR").ok().filter(|d| !d.trim().is_empty()) } else { None };
    match (elsewhere, std::path::Path::new(fixed).file_name()) {
        (Some(dir), Some(name)) => std::path::Path::new(&dir).join(name),
        _ => std::path::PathBuf::from(fixed),
    }
}

/// A file of the boot contract's ([`run_path`]), when it is there.
pub fn run_file(fixed: &str) -> Option<std::path::PathBuf> {
    let path = run_path(fixed);
    path.is_file().then_some(path)
}

/// `url` trusting the database's certificate authority when its file is there ([`database_ca`], [`with_ca`]); as it is
/// otherwise.
pub fn trusted(url: &str) -> Result<String> {
    match database_ca() {
        Some(ca) => with_ca(url, &ca),
        None => Ok(url.trim().to_string()),
    }
}

fn address(url: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(url.trim()).map_err(|e| Error::Invalid(format!("the database's address is not one: {e}")))
}

/// `url` (`DATABASE_URL`) at another host: the direct or read address made from a host setting (`host`, `host:port`,
/// `[v6]` or `[v6]:port`; without a port, PostgreSQL's own 5432), keeping the address's role, password, database and
/// TLS options (D7). A `host`, `hostaddr` or `port` in its query would win over the host, so they go.
pub fn with_host(url: &str, host: &str) -> Result<String> {
    let h = host.trim();
    let bad = || Error::Invalid(format!("{h:?} is not a host, or a host and port (db.example.com, db.example.com:5432, [2001:db8::1]:5432)"));
    let (name, port) = match h.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && (!name.contains(':') || name.starts_with('[')) => {
            (name, Some(port.parse::<u16>().map_err(|_| bad())?))
        }
        _ => (h, None),
    };
    let plain = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    let v6 = name.len() > 2 && name.starts_with('[') && name.ends_with(']') && name[1..name.len() - 1].bytes().all(|b| b.is_ascii_hexdigit() || b == b':');
    if !plain && !v6 {
        return Err(bad());
    }
    let mut u = address(url)?;
    u.set_host(Some(name)).map_err(|_| bad())?;
    u.set_port(port).map_err(|_| bad())?;
    let kept: Vec<(String, String)> = u.query_pairs().into_owned().filter(|(k, _)| !matches!(k.as_str(), "host" | "hostaddr" | "port")).collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    Ok(u.to_string())
}

/// D7's hosts given in the database's address itself (`DATABASE_URL`): `ferrum_direct_host` and `ferrum_read_host`, each
/// `host` or `host:port` as the settings `database_direct_host` and `database_read_host` take them. With them the address
/// is all a replica needs, and they are as trusted as the address, which the operator gave. They are no connection
/// parameters: [`address_hosts`] takes them out before anything connects.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AddressHosts {
    #[serde(default)]
    pub direct_host: String,
    #[serde(default)]
    pub read_host: String,
}

/// `url` without the parameters [`AddressHosts`] names, and their values; `url` as it is when it has none (or is no
/// address at all: whatever reads it next says so). A value that is not a host, or a host and port, is refused.
pub fn address_hosts(url: &str) -> Result<(String, AddressHosts)> {
    let mut hosts = AddressHosts::default();
    let Ok(mut u) = reqwest::Url::parse(url.trim()) else { return Ok((url.to_string(), hosts)) };
    let pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
    if !pairs.iter().any(|(k, _)| k == "ferrum_direct_host" || k == "ferrum_read_host") {
        return Ok((url.to_string(), hosts));
    }
    let mut kept = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "ferrum_direct_host" => hosts.direct_host = v.trim().to_string(),
            "ferrum_read_host" => hosts.read_host = v.trim().to_string(),
            _ => kept.push((k, v)),
        }
    }
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    let stripped = u.to_string();
    for (name, host) in [("ferrum_direct_host", &hosts.direct_host), ("ferrum_read_host", &hosts.read_host)] {
        if !host.is_empty() {
            with_host(&stripped, host).map_err(|e| Error::Invalid(format!("DATABASE_URL's {name}: {e}")))?;
        }
    }
    Ok((stripped, hosts))
}

/// Whether a host taken from the settings table may be given `url`'s password (D7): only `url`'s own host (another port is
/// fine: several services put their pooler and their direct port on one name), or any host when the connection checks
/// the server's certificate and its name against an authority given here (`sslmode=verify-full` with `sslrootcert`:
/// the mounted one, which [`with_ca`] names, or the address's own). The authorities built into the program are not
/// enough (nor `sslrootcert=system`, libpq's word for them): anyone can have a certificate from one for a name of their own. A host from the environment is not asked.
/// Why not, in plain words, otherwise.
pub fn may_send_password(url: &str, host: &str) -> std::result::Result<(), String> {
    let u = address(url).map_err(|e| e.to_string())?;
    let h = host.trim();
    let name = match h.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) && (!name.contains(':') || name.starts_with('[')) => name,
        _ => h,
    };
    if u.host_str().is_some_and(|own| own.eq_ignore_ascii_case(name)) {
        return Ok(());
    }
    let query: Vec<(String, String)> = u.query_pairs().into_owned().collect();
    let says = |k: &str| query.iter().find(|(q, _)| q == k).map(|(_, v)| v.to_ascii_lowercase());
    if says("sslmode").as_deref() == Some("verify-full") && says("sslrootcert").is_some_and(|f| !f.is_empty() && f != "system") {
        return Ok(());
    }
    Err(format!(
        "{h} is not DATABASE_URL's host, and the connection to it would not check the server's certificate and name against an authority \
         given here, so it would be given the database's password: set it in the environment, or mount the database's certificate \
         authority at {DB_CA} (or give sslmode=verify-full and sslrootcert in DATABASE_URL)"
    ))
}

/// `url` trusting the certificate authority at `ca` (D1): the address's own `sslrootcert`, when it names one, wins and
/// nothing changes; otherwise `ca` becomes it, and the TLS mode is raised so that the certificate is checked: none or
/// `prefer` becomes `verify-full`, `require` becomes `verify-ca` (as libpq treats `require` with a root certificate).
/// A mode that turns TLS off with a certificate authority there is refused: one of the two is a mistake.
pub fn with_ca(url: &str, ca: &str) -> Result<String> {
    let mut u = address(url)?;
    let pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
    if pairs.iter().any(|(k, _)| k == "sslrootcert") {
        return Ok(url.trim().to_string());
    }
    let mode = pairs.iter().find(|(k, _)| k == "sslmode").map(|(_, v)| v.to_ascii_lowercase());
    let mode = match mode.as_deref() {
        None | Some("prefer") => "verify-full",
        Some("require") => "verify-ca",
        Some("verify-ca") => "verify-ca",
        Some("verify-full") => "verify-full",
        Some(other) => {
            return Err(Error::Invalid(format!(
                "a certificate authority is at {ca}, and the database's address says sslmode={other}, which checks no certificate: \
                 remove the file, or ask for TLS in the address (sslmode=verify-full)"
            )));
        }
    };
    let rest = pairs.into_iter().filter(|(k, _)| k != "sslmode");
    u.query_pairs_mut().clear().extend_pairs(rest).append_pair("sslmode", mode).append_pair("sslrootcert", ca);
    Ok(u.to_string())
}

/// Connects once to `url` (made by [`with_ca`]) and refuses, naming the cause, when the database's certificate does not
/// verify against `ca`. Any other failure is not this check's to say: the start that follows says it.
pub async fn check_certificate(url: &str, ca: &str) -> Result<()> {
    let Ok(options) = url.parse::<PgConnectOptions>() else { return Ok(()) };
    let tried = tokio::time::timeout(Duration::from_secs(10), sqlx::ConnectOptions::connect(&options)).await;
    match tried {
        Ok(Ok(c)) => {
            let _ = sqlx::Connection::close(c).await;
            Ok(())
        }
        Ok(Err(e)) if matches!(e, sqlx::Error::Tls(_)) || e.to_string().to_ascii_lowercase().contains("certificate") => Err(Error::Invalid(format!(
            "the database's certificate does not verify against the certificate authority in {ca} ({e}): check that the file holds \
             the authority that signed it, and that the address names a host the certificate names (sslmode=verify-ca checks the \
             certificate without its name)"
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn the_hosts_in_the_address_are_taken_out_of_it() {
        let (url, hosts) =
            address_hosts("postgres://u:p%40ss@pooler:6432/f?sslmode=require&ferrum_direct_host=db:5432&application_name=pool:web&ferrum_read_host=ro")
                .unwrap();
        assert_eq!(url, "postgres://u:p%40ss@pooler:6432/f?sslmode=require&application_name=pool%3Aweb", "the rest as it was");
        assert_eq!(hosts, AddressHosts { direct_host: "db:5432".into(), read_host: "ro".into() });
        assert_eq!(with_host(&url, &hosts.direct_host).unwrap(), "postgres://u:p%40ss@db:5432/f?sslmode=require&application_name=pool%3Aweb");
        let plain = "postgres://u:p@db/f?application_name=pool:web";
        assert_eq!(address_hosts(plain).unwrap(), (plain.to_string(), AddressHosts::default()), "an address without them is left exactly as it is");
        assert!(address_hosts("postgres://u:p@db/f?ferrum_direct_host=a/b").unwrap_err().to_string().contains("ferrum_direct_host"));
    }

    /// Against a PostgreSQL with TLS whose certificate `FERRUM_TEST_TLS_CA` signed (scripts/managed_pg.sh makes one):
    /// trusted, it connects with the name checked; another authority is refused, saying why. Skipped without it.
    #[tokio::test]
    async fn a_certificate_that_does_not_verify_is_refused_saying_why() {
        let (Ok(url), Ok(ca), Ok(other)) =
            (std::env::var("FERRUM_TEST_TLS_URL"), std::env::var("FERRUM_TEST_TLS_CA"), std::env::var("FERRUM_TEST_TLS_OTHER_CA"))
        else {
            return eprintln!("skipped: set FERRUM_TEST_TLS_URL, FERRUM_TEST_TLS_CA and FERRUM_TEST_TLS_OTHER_CA (scripts/managed_pg.sh)");
        };
        let trusted = with_ca(&url, &ca).unwrap();
        assert!(trusted.contains("sslmode=verify-full"), "{trusted}");
        check_certificate(&trusted, &ca).await.expect("its own authority");
        let c = sqlx::ConnectOptions::connect(&trusted.parse::<PgConnectOptions>().unwrap()).await.expect("connects verified");
        sqlx::Connection::close(c).await.unwrap();
        let e = check_certificate(&with_ca(&url, &other).unwrap(), &other).await.unwrap_err().to_string();
        assert!(e.contains("does not verify") && e.contains(&other), "{e}");
    }

    #[test]
    fn a_host_setting_keeps_the_addresss_role_password_database_and_tls() {
        let url = "postgres://ferrum:p%40ss@pooler.example.com:6543/ferrum?sslmode=require&application_name=pool:web";
        assert_eq!(with_host(url, "db.example.com").unwrap(), "postgres://ferrum:p%40ss@db.example.com/ferrum?sslmode=require&application_name=pool%3Aweb");
        assert_eq!(
            with_host(url, " db.example.com:5433 ").unwrap(),
            "postgres://ferrum:p%40ss@db.example.com:5433/ferrum?sslmode=require&application_name=pool%3Aweb"
        );
        assert_eq!(with_host("postgres://u:p@a:1/d", "[2001:db8::1]:5432").unwrap(), "postgres://u:p@[2001:db8::1]:5432/d");
        assert_eq!(
            with_host("postgres://u:p@a/d?host=b&port=7&sslmode=verify-full", "c").unwrap(),
            "postgres://u:p@c/d?sslmode=verify-full",
            "the query's host would win"
        );
        for bad in ["", "db.example.com:", "db example.com", "2001:db8::1", "db.example.com:99999", "user@db", "db/x"] {
            assert!(matches!(with_host(url, bad), Err(Error::Invalid(_))), "{bad:?}");
        }
        assert!(with_host("not an address", "db").is_err());
    }

    #[test]
    fn a_stored_host_gets_the_password_only_where_it_cannot_be_drawn_elsewhere() {
        let plain = "postgres://u:p@db.example.com:6543/d";
        assert_eq!(may_send_password(plain, "db.example.com:5432"), Ok(()), "the same host, another port");
        assert_eq!(may_send_password(plain, "DB.Example.com"), Ok(()), "names are not case-sensitive");
        assert_eq!(may_send_password("postgres://u:p@[2001:db8::1]:6432/d", "[2001:db8::1]:5432"), Ok(()));
        for other in ["evil.example.com", "evil.example.com:5432", "db.example.com.evil.net", "[2001:db8::2]:5432", "127.0.0.1:5432"] {
            let e = may_send_password(plain, other).unwrap_err();
            assert!(e.contains(other.trim()) && e.contains("password"), "{other}: {e}");
        }
        let checked = "postgres://u:p@db.example.com/d?sslmode=verify-full&sslrootcert=/run/ferrum/db-ca.pem";
        assert_eq!(may_send_password(checked, "replica.example.com:5432"), Ok(()), "verified against an authority given here, name and all");
        assert_eq!(may_send_password(&with_ca(plain, DB_CA).unwrap(), "replica.example.com"), Ok(()), "the mounted one");
        for weaker in [
            "postgres://u:p@db.example.com/d?sslmode=verify-full",
            "postgres://u:p@db.example.com/d?sslmode=verify-full&sslrootcert=system",
            "postgres://u:p@db.example.com/d?sslmode=verify-ca&sslrootcert=/ca.pem",
            "postgres://u:p@db.example.com/d?sslmode=require&sslrootcert=/ca.pem",
        ] {
            assert!(may_send_password(weaker, "replica.example.com").is_err(), "{weaker}: built-in authorities, or no name checked");
        }
        assert!(may_send_password(&with_ca("postgres://u:p@h/d?sslmode=require", DB_CA).unwrap(), "elsewhere").is_err(), "require: no name checked");
    }

    #[test]
    fn a_certificate_authority_is_trusted_and_checked_against() {
        let ca = "/run/ferrum/db-ca.pem";
        let q = |url: &str| with_ca(url, ca).unwrap().split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
        assert_eq!(q("postgres://u:p@h/d"), "sslmode=verify-full&sslrootcert=%2Frun%2Fferrum%2Fdb-ca.pem");
        assert_eq!(q("postgres://u:p@h/d?sslmode=prefer&application_name=x"), "application_name=x&sslmode=verify-full&sslrootcert=%2Frun%2Fferrum%2Fdb-ca.pem");
        assert_eq!(q("postgres://u:p@h/d?sslmode=require"), "sslmode=verify-ca&sslrootcert=%2Frun%2Fferrum%2Fdb-ca.pem", "as libpq does");
        assert_eq!(q("postgres://u:p@h/d?sslmode=verify-ca"), "sslmode=verify-ca&sslrootcert=%2Frun%2Fferrum%2Fdb-ca.pem");
        assert_eq!(
            with_ca("postgres://u:p@h/d?sslrootcert=/etc/mine.pem", ca).unwrap(),
            "postgres://u:p@h/d?sslrootcert=/etc/mine.pem",
            "the address's own wins"
        );
        for off in ["disable", "allow"] {
            let e = with_ca(&format!("postgres://u:p@h/d?sslmode={off}"), ca).unwrap_err().to_string();
            assert!(e.contains(ca) && e.contains(off), "{e}");
        }
        // What sqlx makes of it: the file and the mode.
        let o: sqlx::postgres::PgConnectOptions = with_ca("postgres://u:p@h/d?sslmode=require", ca).unwrap().parse().unwrap();
        assert!(matches!(o.get_ssl_mode(), sqlx::postgres::PgSslMode::VerifyCa));
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
