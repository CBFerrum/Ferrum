//! The connection pools (crates/kernel/src/db.rs), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; nothing in it is touched).

use ferrum_kernel::db::LISTENERS;
use ferrum_kernel::{Db, DbConfig};
use sqlx::Connection;

/// One test at a time: each counts the sessions of this database by their names.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

async fn shown(pool: &sqlx::PgPool, name: &str) -> String {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("show {name}"))).fetch_one(pool).await.unwrap()
}

/// A replica that vanishes (a lost node) leaves its session advisory locks and LISTEN backends until PostgreSQL notices
/// the dead peer; with no keepalive settings that is the operating system's two hours. Every session that holds such a
/// thing carries the three keepalive settings, so it is about a minute (30 s + 3 x 10 s).
///
/// What is asserted is what the session says it runs with, not a cut connection: a dead peer shows only when its
/// keepalive probes go unanswered, and on one machine the peer's kernel (a proxy's socket, even a stopped process)
/// answers them, so a test proxy cannot make PostgreSQL see silence. The proof that the lock goes is the settings
/// plus PostgreSQL's documented behaviour (and a deployment's run, `deploy/kind`).
#[tokio::test]
async fn sessions_carry_tcp_keepalives() {
    let _one = ONE_AT_A_TIME.lock().await;
    let want = [("tcp_keepalives_idle", "30"), ("tcp_keepalives_interval", "10"), ("tcp_keepalives_count", "3")];
    // No pooler: the main pool is the direct pool (locks and LISTEN go through it).
    let db = Db::connect(DbConfig { url: url(), max_connections: 4, ..Default::default() }).await.unwrap();
    assert!(!db.pooled());
    for (pool, what) in [(&db.direct, "direct"), (&db.pool, "main"), (&db.side, "side")] {
        for (name, value) in want {
            assert_eq!(shown(pool, name).await, value, "{what} pool, {name}");
        }
    }
    // A pooler in front of the main pool: it takes no session settings; the direct pool, the one that holds locks and
    // listeners, does.
    let u = url();
    let main = format!("{u}{}application_name=main", if u.contains('?') { '&' } else { '?' });
    let db = Db::connect(DbConfig { url: main, direct_url: Some(u), max_connections: 4, ..Default::default() }).await.unwrap();
    assert!(db.pooled());
    for (name, value) in want {
        assert_eq!(shown(&db.direct, name).await, value, "direct pool, {name}");
    }
}

/// What listens and the session locks hold their connections for as long as they run. Without a pooler the direct
/// address is the main one, and they used to sit in the request pool (5 of its 30 for ever, and the apps' upkeep locks
/// beside). They now have a pool of their own, whatever the address, and each pool says its name to PostgreSQL. The
/// server's five listeners (metadata and settings, users and the vault, plugin sockets, live updates, co-editing) each
/// held a connection of it; they now share one: the figures are the request pool's and the direct pool's connections in
/// use on an idle replica with its five subscribers on their seven channels.
#[tokio::test]
async fn direct_is_own_pool() {
    let _one = ONE_AT_A_TIME.lock().await;
    let db = Db::connect(DbConfig { url: url(), max_connections: 10, ..Default::default() }).await.unwrap();
    assert!(!db.pooled(), "the same address: no pooler");
    let channels: [&[&'static str]; 5] =
        [&["db_it_meta", "db_it_settings"], &["db_it_users", "db_it_vault"], &["db_it_sockets"], &["db_it_live"], &["db_it_coedit"]];
    let mut listeners = Vec::new();
    for c in channels {
        listeners.push(db.subscribe(c).await);
    }
    let in_use = |p: &sqlx::PgPool| p.size() as usize - p.num_idle();
    let (request, direct) = (in_use(&db.pool), in_use(&db.direct));
    eprintln!("request pool connections in use with 5 subscribers on 7 channels: {request}; direct pool: {direct}");
    assert_eq!(request, 0, "the listener holds none of the request pool's connections");
    assert_eq!(direct, 1, "one connection of the direct pool listens for all of them");
    assert_eq!(LISTENERS, 1);
    // What PostgreSQL sees, by name (a connection of its own: no pool of the server's is touched).
    let mut conn = sqlx::PgConnection::connect(&url()).await.unwrap();
    let seen: Vec<(String, i64)> = sqlx::query_as(
        "select application_name, count(*) from pg_stat_activity where datname = current_database() and application_name like 'ferrum%' \
         group by 1 order by 1",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    let count = |name: &str| seen.iter().find(|(n, _)| n == name).map_or(0, |(_, c)| *c);
    assert_eq!(count("ferrum-direct"), 1, "{seen:?}");
    assert!(count("ferrum") <= 2, "at most two request connections on an idle replica: {seen:?}");
    drop(listeners);
    db.close().await;
}
