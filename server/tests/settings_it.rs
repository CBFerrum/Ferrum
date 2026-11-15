//! Settings in the database (docs/operations.md, "Settings in the database"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset):
//! the server set up as the binary sets it up, once per replica, on one database.

mod common;

use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::config::Config;
use loco_rs::environment::Environment;
use serde_json::json;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

/// The test configuration, on the test database (`application_name` added to its address when given).
fn config(application: Option<&str>) -> Config {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    let url = url();
    config.database.uri = match application {
        Some(a) => format!("{url}{}application_name={a}", if url.contains('?') { '&' } else { '?' }),
        None => url,
    };
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    config
}

/// One replica's set-up, as `ferrum start` makes it.
async fn replica(config: Config) -> AppContext {
    <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("set-up")
}

fn state(ctx: &AppContext) -> AppState {
    ctx.shared_store.get::<AppState>().expect("state")
}

/// Writes a row of the table by hand, as an operator's tool would.
async fn row(st: &AppState, scope: &str, name: &str, key: &str, value: Option<&str>) {
    let mut tx = st.kernel.db.system_tx().await.unwrap();
    sqlx::query(
        "insert into setting (scope, scope_name, key, value, updated_by) values ($1, $2, $3, to_jsonb($4::text), 'test') \
         on conflict (scope, scope_name, key) do update set value = excluded.value, version = nextval('setting_version')",
    )
    .bind(scope)
    .bind(name)
    .bind(key)
    .bind(value)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[test]
fn settings_in_the_database() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    {
        let pool = sqlx::PgPool::connect(&url()).await.expect("connect");
        common::reset(&pool).await;
    }

    // A database this build has not seen holds no settings: the server starts as it always has, and its migrations
    // make the table.
    let first = state(&replica(config(None)).await);
    assert_eq!((first.settings.session_days, first.kernel.settings.host_leaving_days), (14, 14), "the file's defaults");
    let rows: i64 = {
        let mut tx = first.kernel.db.system_tx().await.unwrap();
        let n = sqlx::query_scalar("select count(*) from setting").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    };
    assert_eq!(rows, 0, "made empty");

    // What the table holds, the next replica takes, where the file would have put the variable.
    row(&first, "installation", "", "session_days", Some("9")).await;
    row(&first, "installation", "", "host_leaving_days", Some("3")).await;
    row(&first, "installation", "", "public_url", Some("https://erp.example.com")).await;
    first.kernel.db.close().await;
    let second = replica(config(None)).await;
    let st = state(&second);
    assert_eq!(st.settings.session_days, 9);
    assert_eq!(st.kernel.settings.host_leaving_days, 3, "the kernel's settings are built from the same values");
    assert_eq!(second.config.server.host, "https://erp.example.com", "Loco's own configuration too");
    assert_eq!(st.kernel.settings.public_url.as_deref(), Some("https://erp.example.com"));

    // A secret is never taken from the table, whatever is written there.
    row(&st, "installation", "", "metrics_token", Some("from-the-table")).await;
    st.kernel.db.close().await;
    let third = state(&replica(config(None)).await);
    assert_eq!(third.settings.metrics_token, "", "a secret stays the environment's");
    third.kernel.db.close().await;
}
