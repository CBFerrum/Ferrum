//! The read address (`FERRUM_DATABASE_READ_URL`, docs/operations.md "Read address") against a real standby: a
//! PostgreSQL primary and its streaming standby made by `scripts/replica_pair.sh`, with real lag (the standby's replay
//! paused, then its stream cut) and real failures (a recovery conflict cancelling a read, the standby stopped in the
//! middle of a read, the standby promoted). `read_address_it.rs` checks the same choices with an injected lag.
//!
//! Skipped, saying so, unless `FERRUM_TEST_REPLICA_PRIMARY_URL` and `FERRUM_TEST_REPLICA_URL` name such a pair;
//! `FERRUM_TEST_REPLICA_PAIR` (the pair's directory) lets it stop and start the standby. It ends by promoting the
//! standby, so a pair serves one run:
//!
//! ```text
//! scripts/replica_pair.sh up /tmp/ferrum-pair && . /tmp/ferrum-pair/env
//! cargo test -p ferrum-kernel --test replica_pair_it -- --nocapture
//! scripts/replica_pair.sh down /tmp/ferrum-pair
//! ```

use ferrum_kernel::replica::{CATCH_UP, CHECKED_FOR, Served, noting};
use ferrum_kernel::{Db, DbConfig, Result, Tx};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// What a read saw, and which server answered it (its port, and whether it was replaying).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Answer {
    rows: i64,
    port: i32,
    replaying: bool,
}

/// Counts the workspace's rows after `sleep` seconds, in the read's transaction.
async fn count(tx: &mut Tx, sleep: f64) -> Result<Answer> {
    let (rows, port, replaying): (i64, i32, bool) = sqlx::query_as(
        "select (select count(*) from pair_rows where tenant_id = current_setting('ferrum.tenant_id')::uuid), inet_server_port(), pg_is_in_recovery() \
         from pg_sleep($1)",
    )
    .bind(sleep)
    .fetch_one(&mut **tx)
    .await?;
    Ok(Answer { rows, port, replaying })
}

/// One read as a report makes it: where it went, and how far behind a replica was when it answered.
async fn read(db: &Db, tenant: Uuid) -> (Answer, Option<Duration>) {
    let (out, lag) = noting(async {
        let mut tx = db.read_tx(tenant, 10_000).await?;
        count(&mut tx, 0.0).await
    })
    .await;
    (out.expect("a read"), lag)
}

/// A read whose first attempt takes `long` seconds, run as reports and exports run theirs (once more on the primary
/// when it fails on the replica); how many attempts it took.
async fn long_read(db: &Db, tenant: Uuid, long: f64) -> (Result<Answer>, Option<Duration>, u32) {
    let attempts = AtomicU32::new(0);
    let (out, lag) = noting(db.reading(|| async {
        let first = attempts.fetch_add(1, Ordering::SeqCst) == 0;
        let mut tx = db.read_tx(tenant, 30_000).await?;
        count(&mut tx, if first { long } else { 0.0 }).await
    }))
    .await;
    (out, lag, attempts.load(Ordering::SeqCst))
}

async fn primary_rows(db: &Db, tenant: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from pair_rows where tenant_id = $1").bind(tenant).fetch_one(&db.pool).await.unwrap()
}

async fn insert(db: &Db, tenant: Uuid, from: i32, to: i32) {
    sqlx::query("insert into pair_rows (tenant_id, n) select $1, g from generate_series($2, $3 - 1) g")
        .bind(tenant)
        .bind(from)
        .bind(to)
        .execute(&db.pool)
        .await
        .unwrap();
}

/// Waits until the standby has replayed everything the primary has written.
async fn caught_up(db: &Db, standby: &sqlx::PgPool) {
    let at: String = sqlx::query_scalar("select pg_current_wal_lsn()::text").fetch_one(&db.pool).await.unwrap();
    for _ in 0..200 {
        let done: bool = sqlx::query_scalar("select pg_last_wal_replay_lsn() >= $1::pg_lsn").bind(&at).fetch_one(standby).await.unwrap_or(false);
        if done {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the standby did not replay up to {at}");
}

fn port(url: &str) -> i32 {
    i32::from(url.parse::<sqlx::postgres::PgConnectOptions>().expect("an address").get_port())
}

/// Runs `scripts/replica_pair.sh <what> <dir>`.
fn pair_script(what: &str, dir: &str) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/replica_pair.sh");
    let out = std::process::Command::new("bash").args([script, what, dir]).output().expect("bash");
    assert!(out.status.success(), "{what}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_on_a_real_standby() {
    let get = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (Some(primary), Some(standby)) = (get("FERRUM_TEST_REPLICA_PRIMARY_URL"), get("FERRUM_TEST_REPLICA_URL")) else {
        eprintln!(
            "a real standby: skipped; set FERRUM_TEST_REPLICA_PRIMARY_URL and FERRUM_TEST_REPLICA_URL to a primary and its streaming standby \
             (scripts/replica_pair.sh up <dir>, then . <dir>/env)"
        );
        return;
    };
    let max_lag = Duration::from_secs(3);
    let config = DbConfig {
        url: primary.clone(),
        direct_url: Some(primary.clone()),
        read_url: Some(standby.clone()),
        read_max_lag: max_lag,
        max_connections: 6,
        ..Default::default()
    };
    let db = Db::connect(config).await.expect("the primary");
    let reads = db.read.clone().expect("a read pool");
    let n = |s: Served| reads.counts().into_iter().find(|c| c.0 == s).map_or(0, |c| c.1);
    let (on_primary, on_standby) = (port(&primary), port(&standby));
    // The standby as the test itself reaches it: to watch its replay, pause and resume it, and promote it.
    let reach = || async { sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(&standby).await.expect("the standby") };
    let sb = reach().await;
    let tenant = Uuid::now_v7();
    sqlx::raw_sql(
        "drop table if exists pair_rows; \
         create table pair_rows (tenant_id uuid not null, n int not null, at timestamptz not null default now()) with (autovacuum_enabled = false)",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    // The standby answers, set as on the primary: the workspace, read only, and the same time zone, so that what a
    // report or an export writes out does not depend on which server answered (to_jsonb writes a time in the
    // session's zone).
    insert(&db, tenant, 0, 100).await;
    let (a, lag) = read(&db, tenant).await;
    assert_eq!((a.rows, a.port, a.replaying), (100, on_standby, true), "{a:?}");
    assert!(lag.is_some_and(|l| l <= max_lag), "{lag:?}");
    assert_eq!(n(Served::Replica), 1);
    let shown = |mut tx: Tx| async move {
        let row: (String, String, String, String) = sqlx::query_as(
            "select current_setting('transaction_read_only'), current_setting('ferrum.tenant_id'), current_setting('TimeZone'), \
             (select to_jsonb(at)::text from pair_rows where tenant_id = current_setting('ferrum.tenant_id')::uuid order by n limit 1)",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        row
    };
    let on_replica = shown(db.read_tx(tenant, 1000).await.unwrap()).await;
    let on_the_primary = shown(db.tenant_tx(tenant).await.unwrap()).await;
    assert_eq!(on_replica.0, "on");
    assert_eq!(
        (&on_replica.1, &on_replica.2, &on_replica.3),
        (&on_the_primary.1, &on_the_primary.2, &on_the_primary.3),
        "replica {on_replica:?}, primary {on_the_primary:?}"
    );

    // Whoever has just written sees it: the read waits for the standby to replay it.
    insert(&db, tenant, 100, 101).await;
    let (a, _) = read(&db, tenant).await;
    assert_eq!(a.rows, 101, "{a:?}");

    // Real lag: replay paused. A write the standby has not replayed is never hidden: the read waits for it up to its
    // bound, then reads from the primary (behind); once the standby's lag is past the limit, it goes to the primary at
    // once (lagging).
    sqlx::query("select pg_wal_replay_pause()").execute(&sb).await.unwrap();
    insert(&db, tenant, 101, 102).await;
    let (before, started) = (n(Served::Behind), Instant::now());
    let (a, lag) = read(&db, tenant).await;
    assert_eq!((a.rows, a.port, lag), (102, on_primary, None), "{a:?}");
    assert!(started.elapsed() >= CATCH_UP && started.elapsed() < CATCH_UP + Duration::from_secs(2), "{:?}", started.elapsed());
    assert_eq!(n(Served::Behind), before + 1);
    tokio::time::sleep(max_lag + CHECKED_FOR).await;
    insert(&db, tenant, 102, 103).await;
    let (before, started) = (n(Served::Lagging), Instant::now());
    let (a, lag) = read(&db, tenant).await;
    assert_eq!((a.rows, a.port, lag), (103, on_primary, None), "{a:?}");
    assert!(started.elapsed() < CATCH_UP, "past the limit, nothing is waited for: {:?}", started.elapsed());
    assert_eq!(n(Served::Lagging), before + 1);
    sqlx::query("select pg_wal_replay_resume()").execute(&sb).await.unwrap();
    caught_up(&db, &sb).await;
    tokio::time::sleep(CHECKED_FOR).await;
    let (a, lag) = read(&db, tenant).await;
    assert_eq!((a.rows, a.port), (103, on_standby), "back on the standby once it has caught up");
    assert!(lag.is_some());

    // Real lag the standby cannot see: its stream from the primary cut (the primary's address wrong). It has replayed
    // all it received, so its own lag reads 0 while the primary moves on. A read still never misses a write; and
    // once one has waited its bound in vain, the next reads go to the primary without waiting, until the next look.
    // Only the standby's superuser may change its primary's address: its password is in the pair's directory.
    if let Some(dir) = get("FERRUM_TEST_REPLICA_PAIR") {
        let admin = admin_url(&standby, &dir);
        let su = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&admin).await.expect("the standby's superuser");
        let original: String = sqlx::query_scalar("select setting from pg_settings where name = 'primary_conninfo'").fetch_one(&su).await.unwrap();
        sqlx::query("alter system set primary_conninfo = 'host=127.0.0.1 port=1'").execute(&su).await.unwrap();
        sqlx::query("select pg_reload_conf()").execute(&su).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::time::sleep(CHECKED_FOR).await;
        insert(&db, tenant, 103, 104).await;
        let (behind, lagging, started) = (n(Served::Behind), n(Served::Lagging), Instant::now());
        let (a, _) = read(&db, tenant).await;
        assert_eq!((a.rows, a.port), (104, on_primary), "{a:?}");
        assert!(started.elapsed() >= CATCH_UP, "{:?}", started.elapsed());
        let started = Instant::now();
        for _ in 0..5 {
            let (a, _) = read(&db, tenant).await;
            assert_eq!((a.rows, a.port), (104, on_primary), "{a:?}");
        }
        assert!(started.elapsed() < CATCH_UP, "five reads after one waited in vain waited again: {:?}", started.elapsed());
        assert_eq!((n(Served::Behind), n(Served::Lagging)), (behind + 1, lagging + 5));
        sqlx::query(sqlx::AssertSqlSafe(format!("alter system set primary_conninfo = '{}'", original.replace('\'', "''")))).execute(&su).await.unwrap();
        sqlx::query("select pg_reload_conf()").execute(&su).await.unwrap();
        su.close().await;
        caught_up(&db, &sb).await;
        tokio::time::sleep(CHECKED_FOR).await;
        let (a, _) = read(&db, tenant).await;
        assert_eq!((a.rows, a.port), (104, on_standby), "{a:?}");
    } else {
        eprintln!("the stream cut, the standby stopped: skipped; set FERRUM_TEST_REPLICA_PAIR to the pair's directory");
    }

    // A recovery conflict: a long read on the standby while the primary deletes rows and vacuums them away. The standby
    // cancels the read after max_standby_streaming_delay (scripts/replica_pair.sh sets 1 s); it runs once more, on the
    // primary, and answers what the primary holds.
    let (replica, conflict) = (n(Served::Replica), n(Served::Conflict));
    let vacuum = {
        let pool = db.pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            sqlx::query("delete from pair_rows where tenant_id = $1 and n % 2 = 0").bind(tenant).execute(&pool).await.unwrap();
            sqlx::query("vacuum pair_rows").execute(&pool).await.unwrap();
        })
    };
    let started = Instant::now();
    let (out, lag, attempts) = long_read(&db, tenant, 8.0).await;
    vacuum.await.unwrap();
    let a = out.expect("answered by the primary");
    assert_eq!((a.rows, a.port, lag, attempts), (primary_rows(&db, tenant).await, on_primary, None, 2), "{a:?}");
    assert!(started.elapsed() < Duration::from_secs(6), "cancelled by the standby, not run to its end: {:?}", started.elapsed());
    assert_eq!((n(Served::Replica), n(Served::Conflict)), (replica + 1, conflict + 1), "{:?}", reads.counts());
    caught_up(&db, &sb).await;

    // The standby stopped in the middle of a read: once more on the primary; while it is down, the primary at once.
    if let Some(dir) = get("FERRUM_TEST_REPLICA_PAIR") {
        tokio::time::sleep(CHECKED_FOR).await;
        let (replica, lost) = (n(Served::Replica), n(Served::Lost));
        let stop = {
            let dir = dir.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(700)).await;
                tokio::task::spawn_blocking(move || pair_script("stop-standby", &dir)).await.unwrap();
            })
        };
        let (out, lag, attempts) = long_read(&db, tenant, 8.0).await;
        stop.await.unwrap();
        let a = out.expect("answered by the primary");
        assert_eq!((a.rows, a.port, lag, attempts), (primary_rows(&db, tenant).await, on_primary, None, 2), "{a:?}");
        assert_eq!((n(Served::Replica), n(Served::Lost)), (replica + 1, lost + 1), "{:?}", reads.counts());
        let (down, started) = (n(Served::Down), Instant::now());
        let (a, lag) = read(&db, tenant).await;
        assert_eq!((a.port, lag), (on_primary, None));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(n(Served::Down), down + 1);
        // Reads that come together once the last look is stale: one looks again, and waits for the address to fail; the
        // others go by the last look meanwhile, to the primary at once.
        tokio::time::sleep(CHECKED_FOR).await;
        let together = futures::future::join_all((0..8).map(|_| async {
            let started = Instant::now();
            let (a, _) = read(&db, tenant).await;
            (a.port, started.elapsed())
        }))
        .await;
        assert!(together.iter().all(|(p, _)| *p == on_primary), "{together:?}");
        let waited = together.iter().filter(|(_, t)| *t >= Duration::from_millis(500)).count();
        assert!(waited <= 1, "{waited} of 8 reads waited for the stopped standby: {together:?}");
        let d = dir.clone();
        tokio::task::spawn_blocking(move || pair_script("start-standby", &d)).await.unwrap();
        caught_up(&db, &sb).await;
        tokio::time::sleep(CHECKED_FOR).await;
        let (a, _) = read(&db, tenant).await;
        assert_eq!(a.port, on_standby, "back on the standby once it runs again: {:?}", reads.counts());
    }

    // "Is this the same database?" (D7): the primary under its own address is, by the lock; its standby, which the lock
    // cannot see, is by its identity while it follows; promoted, it is a genuine copy with the same identity (as a managed
    // service's snapshot, clone or branch is), which the identity alone would have refused.
    use ferrum_kernel::recovery::{Same, same_database};
    let other = |u: &str| Db::connect(DbConfig { url: u.to_string(), direct_url: Some(u.to_string()), max_connections: 2, ..Default::default() });
    let (again, follower) = (other(&primary).await.unwrap(), other(&standby).await.unwrap());
    assert_eq!(same_database(&db, &again).await.unwrap(), Some(Same::This));
    assert_eq!(same_database(&db, &follower).await.unwrap(), Some(Same::Following));
    let identity =
        "select (select system_identifier from pg_control_system())::text || '-' || (select oid from pg_database where datname = current_database())";
    let ids: (String, String) =
        (sqlx::query_scalar(identity).fetch_one(&db.pool).await.unwrap(), sqlx::query_scalar(identity).fetch_one(&follower.pool).await.unwrap());

    // Promoted, it is no replica: never read from as one, even before the next look at it, and a write made on the
    // primary since is never missed.
    let sb = reach().await;
    let (a, _) = read(&db, tenant).await;
    assert_eq!(a.port, on_standby);
    let looked = Instant::now();
    sqlx::query("select pg_promote(true, 30)").execute(&sb).await.unwrap();
    let not_replica = n(Served::NotReplica);
    let (a, lag) = read(&db, tenant).await;
    assert!(looked.elapsed() < CHECKED_FOR, "too slow to read within the last look ({:?}): run it again", looked.elapsed());
    assert_eq!((a.port, a.replaying, lag), (on_primary, false, None), "a promoted standby was read from as a replica: {a:?}");
    assert_eq!(n(Served::NotReplica), not_replica + 1);
    let rows = primary_rows(&db, tenant).await;
    insert(&db, tenant, 1000, 1001).await;
    let (a, _) = read(&db, tenant).await;
    assert_eq!((a.rows, a.port), (rows + 1, on_primary), "{a:?}");
    tokio::time::sleep(CHECKED_FOR).await;
    let (a, _) = read(&db, tenant).await;
    assert_eq!(a.port, on_primary);
    assert_eq!(n(Served::NotReplica), not_replica + 3, "{:?}", reads.counts());
    let promoted = other(&standby).await.unwrap();
    assert_eq!(ids.0, sqlx::query_scalar::<_, String>(identity).fetch_one(&promoted.pool).await.unwrap(), "the same identity as before: {ids:?}");
    assert_eq!(ids.0, ids.1);
    assert_eq!(same_database(&db, &promoted).await.unwrap(), None, "a genuine copy, which the identity alone would have refused");
    for d in [again, follower, promoted] {
        d.close().await;
    }
    eprintln!("the standby is promoted: make the pair again (scripts/replica_pair.sh down, then up) before the next run");
}

/// The standby's address as the pair's superuser, whose password `scripts/replica_pair.sh` keeps in the pair's directory.
fn admin_url(standby: &str, dir: &str) -> String {
    let pass = std::fs::read_to_string(format!("{dir}/admin.pass")).expect("the pair's admin.pass");
    format!("postgres://ferrum_pair_admin:{}@127.0.0.1:{}/postgres", pass.trim(), port(standby))
}
