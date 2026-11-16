//! What the server's tests share.

/// Empties a test database: its tables a few dozen to a statement, then the schema with what is
/// left. One `drop schema ... cascade` over every app's tables, with their indexes, policies and
/// guards, needs more locks than Postgres has by default.
pub async fn reset(pool: &sqlx::PgPool) {
    let tables: Vec<String> =
        sqlx::query_scalar("select quote_ident(tablename) from pg_tables where schemaname = 'public'").fetch_all(pool).await.expect("reset: tables");
    for chunk in tables.chunks(50) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("drop table if exists {} cascade", chunk.join(", ")))).execute(pool).await.expect("reset");
    }
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(pool).await.expect("reset");
}

/// Opens the vault of `db` as a server does as it starts, before the kernel boots: with its credential slot (from the
/// address's password; Argon2id kept cheap here), so that the apps registered through `hooks.app` use their own handles
/// as they do in the server.
#[allow(dead_code)]
pub async fn open_vault(db: &ferrum_kernel::Db, url: &str) {
    let opener = ferrum_kernel::vault::Opener {
        database_url: url.to_string(),
        file: "/nonexistent/ferrum/root.key".into(),
        kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
        ..Default::default()
    };
    ferrum_kernel::vault::open(db, opener, false).await.expect("the vault opens");
}
