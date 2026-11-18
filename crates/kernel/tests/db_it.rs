//! The connection pools (crates/kernel/src/db.rs), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; nothing in it is touched).

use ferrum_kernel::{Db, DbConfig};

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
