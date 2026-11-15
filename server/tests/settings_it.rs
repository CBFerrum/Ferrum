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

    adopted_once(&third).await;
    pools(&third).await;
    third.kernel.db.close().await;
    live().await;
}

/// Tells every replica that the table changed, as a change of it does.
async fn notify(st: &AppState) {
    let mut tx = st.kernel.db.system_tx().await.unwrap();
    sqlx::query("select pg_notify('ferrum_settings', '')").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Waits up to five seconds for `done`.
async fn until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{what}: not within five seconds");
}

/// Two replicas serving: a change of a setting that applies at once reaches the other without a restart, one whose
/// notice was missed arrives with the compare, a value the start would refuse is not taken, and the per-user and
/// per-address budgets are divided between them.
async fn live() {
    ferrum_server::settings::EVERY_MS.store(300, std::sync::atomic::Ordering::Relaxed);
    let (a_ctx, b_ctx) = (replica(config(None)).await, replica(config(None)).await);
    <App as Hooks>::before_run(&a_ctx).await.expect("serving");
    <App as Hooks>::before_run(&b_ctx).await.expect("serving");
    let (a, b) = (state(&a_ctx), state(&b_ctx));
    until("both replicas counted", || a.live_settings.replicas() == 2 && b.live_settings.replicas() == 2).await;
    assert_eq!(b.user_limiter.max(), 1500, "3000 a minute for the pool: half each");
    assert_eq!(b.address_limiter.max(), 3000);

    row(&a, "installation", "", "user_requests_per_minute", Some("600")).await;
    row(&a, "installation", "", "session_days", Some("2")).await;
    row(&a, "installation", "", "zone_mode", Some("report")).await;
    row(&a, "installation", "", "default_workspace", Some("acme")).await;
    notify(&a).await;
    until("the change reaches the other replica", || b.live_settings.now().settings.user_requests_per_minute == 600).await;
    assert_eq!(b.user_limiter.max(), 300, "and its share of it");
    assert_eq!(b.shell.session_ttl(), std::time::Duration::from_secs(2 * 86_400));
    assert_eq!(b.zones.mode(), ferrum_kernel::zones::Mode::Report);
    assert_eq!(b.live_settings.now().settings.default_workspace, "", "a setting that applies at start waits for the next start");
    assert_eq!(b.settings.default_workspace, "");

    // No notice: the compare finds it.
    row(&a, "installation", "", "jwt_expiration_seconds", Some("120")).await;
    row(&a, "installation", "", "host_leaving_days", Some("5")).await;
    until("a missed notice heals", || b.live_settings.now().jwt_expiration == 120 && b.kernel.live().host_leaving_days == 5).await;
    assert_eq!(b.kernel.settings.host_leaving_days, 14, "the start's stay what they were for whoever reads them");

    // Written by hand past every check: the replicas keep what they have.
    row(&a, "installation", "", "zone_mode", Some("sideways")).await;
    notify(&a).await;
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert_eq!(b.zones.mode(), ferrum_kernel::zones::Mode::Report, "a value the start refuses is not taken");
    assert_eq!(b.live_settings.now().settings.zone_mode, "report");
    row(&a, "installation", "", "zone_mode", None).await;
    notify(&a).await;
    until("unset: back to the default", || b.zones.mode() == ferrum_kernel::zones::Mode::Enforce).await;

    let ids = [a.live_settings.replica, b.live_settings.replica];
    let _ = a.shutdown.send(true);
    let _ = b.shutdown.send(true);
    for st in [&a, &b] {
        let handles: Vec<_> = st.background.lock().await.drain(..).collect();
        futures_util::future::join_all(handles).await;
    }
    let mut tx = a.kernel.db.system_tx().await.unwrap();
    let left: i64 = sqlx::query_scalar("select count(*) from replica where id = any($1)").bind(&ids[..]).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(left, 0, "a replica that stops takes its row with it");
    a.kernel.db.close().await;
    b.kernel.db.close().await;
}

/// A replica's address names its pool, and a pool's rows hold its own connection budget, background work and zone.
async fn pools(st: &AppState) {
    row(st, "pool", "web", "db_max_connections", Some("7")).await;
    row(st, "pool", "web", "run_background", Some("true")).await;
    row(st, "pool", "other", "db_max_connections", Some("9")).await;
    row(st, "installation", "", "db_max_connections", Some("11")).await;
    let web = state(&replica(config(Some("pool:web"))).await);
    assert_eq!(web.settings.db_max_connections, 7, "its pool's row");
    assert_eq!(web.kernel.db.pool.options().get_max_connections(), 7, "and its pool is opened that size");
    assert!(web.settings.run_background, "its row over the test file's false");
    web.kernel.db.close().await;
    let plain = state(&replica(config(Some("main"))).await);
    assert_eq!(plain.settings.db_max_connections, 10, "no pool named: the default pool, which has no row; the file's 10");
    assert!(!plain.settings.run_background, "the test file's");
    plain.kernel.db.close().await;
    let refused = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config(Some("pool:Web"))).build()).await;
    let said = refused.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(said.contains("lower-case letters, digits and dashes"), "{said}");
    let written = ferrum_server::settings::adopt(&st.kernel.db, "web", &|n: &str| (n == "FERRUM_ZONE").then(|| "office".to_string())).await.unwrap();
    assert_eq!(written.into_iter().map(|(k, _)| k).collect::<Vec<_>>(), ["zone"]);
    let mut tx = st.kernel.db.system_tx().await.unwrap();
    let at: (String, String) = sqlx::query_as("select scope, scope_name from setting where key = 'zone'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(at, ("pool".to_string(), "web".to_string()), "copied to the pool's own row");
}

/// What a variable says and the table does not hold is written once, however many replicas copy it at once; what the
/// table holds, unset or not, stays.
async fn adopted_once(st: &AppState) {
    let env = |value: &'static str| {
        move |name: &str| match name {
            "FERRUM_PDF_URL" => Some(value.to_string()),
            "FERRUM_SESSION_DAYS" => Some("21".to_string()),
            "FERRUM_ZONE_DEFAULT" => Some(String::new()),
            "FERRUM_METRICS_TOKEN" => Some("a-secret".to_string()),
            "FERRUM_OFFICE_URL" => Some("http://office".to_string()),
            _ => None,
        }
    };
    let (a, b) = (env("http://pdf:3000"), env("http://pdf:3000"));
    let db = &st.kernel.db;
    let (one, two) = tokio::join!(ferrum_server::settings::adopt(db, "default", &a), ferrum_server::settings::adopt(db, "default", &b));
    let mut written: Vec<String> = [one.unwrap(), two.unwrap()].concat().into_iter().map(|(k, _)| k).collect();
    written.sort();
    assert_eq!(written, ["pdf_url"], "once, by one of them: session_days is the table's already, and the rest are not kept there");
    let later = ferrum_server::settings::adopt(db, "default", &env("http://elsewhere")).await.unwrap();
    assert!(later.is_empty(), "the table holds it now: {later:?}");
    row(st, "installation", "", "host_leaving_days", None).await;
    let host = |name: &str| (name == "FERRUM_HOST_LEAVING_DAYS").then(|| "30".to_string());
    assert!(ferrum_server::settings::adopt(db, "default", &host).await.unwrap().is_empty(), "an operator unset it: it stays unset");
    let mut tx = db.system_tx().await.unwrap();
    let rows: Vec<(String, serde_json::Value, String)> =
        sqlx::query_as("select key, value, updated_by from setting where updated_by = 'environment' order by key").fetch_all(&mut *tx).await.unwrap();
    let changes: Vec<(String, Option<serde_json::Value>, serde_json::Value, String)> =
        sqlx::query_as("select key, old_value, new_value, updated_by from setting_change where key = 'pdf_url'").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(rows, [("pdf_url".to_string(), json!("http://pdf:3000"), "environment".to_string())]);
    assert_eq!(changes, [("pdf_url".to_string(), None, json!("http://pdf:3000"), "environment".to_string())], "recorded once");
}
