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
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(pool).await.expect("reset");
}
