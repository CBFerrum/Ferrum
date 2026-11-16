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
    the_binary().await;
}

/// A free port on this machine.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// `ferrum start` in production, given only `env` (the database's address and the secrets), from the server's
/// directory (Loco reads `config/` there); its output to `log`.
fn start(env: &[(&str, String)], log: &std::path::Path) -> std::process::Child {
    let out = std::fs::File::create(log).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_ferrum"))
        .args(["-e", "production", "start", "--no-banner"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("the server binary")
}

/// Waits up to three minutes for the server on `port` to answer `/api/ready`.
async fn ready(port: u16, child: &mut std::process::Child, log: &std::path::Path) {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap();
    for _ in 0..360 {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the server stopped ({status}): {}", std::fs::read_to_string(log).unwrap_or_default());
        }
        if client.get(format!("http://127.0.0.1:{port}/api/ready")).send().await.is_ok_and(|r| r.status().is_success()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("the server did not answer on {port}: {}", std::fs::read_to_string(log).unwrap_or_default());
}

fn stop(child: &mut std::process::Child) {
    let _ = std::process::Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
    for _ in 0..100 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The binary, as an operator runs it: it starts in production with only the database's address and the secrets, and
/// takes everything else from the table (its port, where its files and packages are, that one server needs no object
/// store, its pool's connection budget); the environment still wins where it sets a variable; and two started
/// together copy what their environment sets into the table once.
async fn the_binary() {
    let dir = std::env::temp_dir().join(format!("ferrum-settings-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let st = state(&replica(config(None)).await);
    let port = free_port();
    for (key, value) in [
        ("port", port.to_string()),
        ("binding", "127.0.0.1".to_string()),
        ("public_url", format!("http://127.0.0.1:{port}")),
        ("require_object_storage", "false".to_string()),
        // Where the set-up above keeps its files: outside development and tests, a server without a bucket starts only
        // where its database's files are (docs/operations.md, "Object storage").
        ("files_dir", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(&st.settings.files_dir).to_string_lossy().into_owned()),
        ("packages_dir", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages").to_string_lossy().into_owned()),
        ("log_level", "warn".to_string()),
    ] {
        row(&st, "installation", "", key, Some(&value)).await;
    }
    // Its pool's own budget: a small one, above the floor the replica's own loops need (POOL_FLOOR).
    row(&st, "pool", "default", "db_max_connections", Some("12")).await;
    st.kernel.db.close().await;
    let secrets = vec![
        ("DATABASE_URL", url()),
        ("JWT_SECRET", "c2V0dGluZ3MtaXQtand0LXNlY3JldC0wMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZg==".to_string()),
        ("FERRUM_SECRET_KEY", "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=".to_string()),
    ];

    // Only the database's address and the secrets: everything else from the table.
    let log = dir.join("only-the-address.log");
    let mut only = start(&secrets, &log);
    ready(port, &mut only, &log).await;
    let version: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/api/version")).await.unwrap().json().await.unwrap();
    assert!(version["packages"].as_array().is_some_and(|p| !p.is_empty()), "serving the packages the table points at: {version}");
    stop(&mut only);

    // A task, given the same: it changes the table.
    let task = std::process::Command::new(env!("CARGO_BIN_EXE_ferrum"))
        .args(["-e", "production", "task", "set", "key:FERRUM_SESSION_DAYS", "value:6"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .envs(secrets.iter().map(|(k, v)| (*k, v.as_str())))
        .output()
        .expect("the task");
    assert!(task.status.success(), "{}", String::from_utf8_lossy(&task.stdout));
    let check = state(&replica(config(None)).await);
    let mut tx = check.kernel.db.system_tx().await.unwrap();
    let (value, by): (serde_json::Value, String) =
        sqlx::query_as("select value, updated_by from setting where scope = 'installation' and key = 'session_days'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((value, by.as_str()), (json!("6"), "task"));
    check.kernel.db.close().await;

    // Two at once, each with a port of its own in its environment (over the table's) and a setting the table lacks.
    let (p1, p2) = (free_port(), free_port());
    let with = |p: u16| {
        let mut env = secrets.clone();
        env.push(("PORT", p.to_string()));
        env.push(("FERRUM_ASSISTANT_MODEL", "a-model-of-the-environment".to_string()));
        env
    };
    let (log1, log2) = (dir.join("first.log"), dir.join("second.log"));
    let (mut one, mut two) = (start(&with(p1), &log1), start(&with(p2), &log2));
    ready(p1, &mut one, &log1).await;
    ready(p2, &mut two, &log2).await;
    let check = state(&replica(config(None)).await);
    let mut written = 0;
    for _ in 0..60 {
        let mut tx = check.kernel.db.system_tx().await.unwrap();
        let n: i64 = sqlx::query_scalar("select count(*) from setting_change where key = 'assistant_model' and updated_by = 'environment'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        written = n;
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let mut tx = check.kernel.db.system_tx().await.unwrap();
    let rows: Vec<(serde_json::Value, String)> =
        sqlx::query_as("select value, updated_by from setting where key = 'assistant_model'").fetch_all(&mut *tx).await.unwrap();
    let changes: i64 = sqlx::query_scalar("select count(*) from setting_change where key = 'assistant_model'").fetch_one(&mut *tx).await.unwrap();
    let replicas: i64 = sqlx::query_scalar("select count(*) from replica where pool = 'default'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(written, 1, "copied by one of the two");
    assert_eq!((rows, changes), (vec![(json!("a-model-of-the-environment"), "environment".to_string())], 1), "once, however many start together");
    assert_eq!(replicas, 2, "each keeps its row");
    stop(&mut one);
    stop(&mut two);
    check.kernel.db.close().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Changing settings as the tasks do: refused by type, as a secret, as someone else's, unknown, out of scope, or as the
/// start would refuse it, and then nothing is written; else recorded with the value before and after, announced, and
/// taken by the other replica. A whole document, with what it leaves out unset.
async fn changes(a: &AppState, b: &AppState) {
    use ferrum_server::settings::{Change, change};
    let one = |name: &str, value: Option<&str>| vec![Change { name: name.into(), pool: None, value: value.map(str::to_string) }];
    let said = |r: ferrum_kernel::Result<Vec<ferrum_server::settings::Done>>| r.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(said(change(a, &one("session_days", Some("soon")), "task").await).contains("whole number"));
    assert!(said(change(a, &one("FERRUM_METRICS_TOKEN", Some("x")), "task").await).contains("is a secret"));
    assert!(said(change(a, &one("kms_ca_file", Some("/etc/ca.pem")), "task").await).contains("by the key-management client"));
    assert!(matches!(change(a, &one("nonsense", Some("1")), "task").await, Err(ferrum_kernel::Error::NotFound(_))));
    assert!(said(change(a, &one("db_max_connections", Some("8")), "task").await).contains("each pool's own"));
    let zone = said(change(a, &one("zone_default", Some("nowhere")), "task").await);
    assert!(zone.contains("FERRUM_ZONES does not name"), "as the start would refuse it: {zone}");
    let both = [Change { name: "proxy_hops".into(), pool: None, value: Some("3".into()) }, one("session_days", Some("x")).remove(0)];
    assert!(change(a, &both, "task").await.is_err());
    let count = |st: &AppState| {
        let db = st.kernel.db.clone();
        async move {
            let mut tx = db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar("select count(*) from setting_change where updated_by = 'task'").fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            n
        }
    };
    assert_eq!(count(a).await, 0, "refused: nothing written, not even the good one beside it");

    let done = change(a, &one("FERRUM_PROXY_HOPS", Some("2")), "task").await.unwrap();
    assert_eq!((done[0].key.as_str(), done[0].old.clone(), done[0].new.clone()), ("proxy_hops", None, Some("2".to_string())));
    until("set: the other replica takes it", || b.live_settings.now().settings.proxy_hops == 2).await;
    assert!(change(a, &one("proxy_hops", Some("2")), "task").await.unwrap()[0].version.is_none(), "so already: nothing written");
    change(a, &one("proxy_hops", None), "task").await.unwrap();
    until("unset: back to the default", || b.live_settings.now().settings.proxy_hops == 1).await;
    let mut tx = a.kernel.db.system_tx().await.unwrap();
    let record: Vec<(Option<serde_json::Value>, Option<serde_json::Value>, String)> =
        sqlx::query_as("select old_value, new_value, updated_by from setting_change where key = 'proxy_hops' order by id").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(record, [(None, Some(json!("2")), "task".to_string()), (Some(json!("2")), None, "task".to_string())], "each change, before and after");
    let pool = change(a, &[Change { name: "db_max_connections".into(), pool: Some("web".into()), value: Some("8".into()) }], "task").await.unwrap();
    assert_eq!(pool[0].pool.as_deref(), Some("web"));
    // An app's own setting, which its manifest declares: kept in the table as <app>.<key>, as the platform's are.
    let drive = change(a, &one("FERRUM_OFFICE_URL", Some("http://office.test")), "task").await.unwrap();
    assert_eq!((drive[0].key.as_str(), drive[0].new.as_deref()), ("drive.office_url", Some("http://office.test")));

    // A document: what it names, and with `others: unset` the installation's settings it leaves out.
    let doc: ferrum_server::settings::Document =
        serde_yaml::from_str("installation:\n  FERRUM_PDF_URL: http://pdf:3000\n  session_days: 4\nothers: unset\n").unwrap();
    let stored = ferrum_server::settings::read_with(&a.kernel.db.pool).await.unwrap();
    let changes = doc.changes(&stored).unwrap();
    assert!(changes.iter().any(|c| c.name == "user_requests_per_minute" && c.value.is_none()), "left out: unset");
    assert!(!changes.iter().any(|c| c.pool.is_some()), "a pool the document does not name is left alone");
    change(a, &changes, "document").await.unwrap();
    until("the document reaches the other replica", || {
        let now = b.live_settings.now();
        now.settings.user_requests_per_minute == 3000 && now.settings.session_days == 4 && now.settings.pdf_url == "http://pdf:3000"
    })
    .await;
    assert_eq!(b.kernel.live().pdf_url.as_deref(), Some("http://pdf:3000"), "the kernel's too");
}

/// The operator's routes: refused without the operator's token, the same with a wrong one; the listing asked as a
/// browser asks (compressed); a change and its refusal.
async fn routes() {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;
    let mut with_token = config(None);
    with_token.settings.as_mut().unwrap()["operator_token"] = json!("an-operator-token-of-enough-length");
    let ctx = replica(with_token).await;
    let routes = <App as Hooks>::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    let app = <App as Hooks>::after_routes(routes, &ctx).await.expect("layers");
    let call = |method: Method, path: &str, token: Option<&str>, body: Option<serde_json::Value>, gzip: bool| {
        let mut req = Request::builder().method(method).uri(path).header(header::HOST, "localhost");
        if let Some(t) = token {
            req = req.header("x-ferrum-operator-token", t);
        }
        if gzip {
            req = req.header(header::ACCEPT_ENCODING, "gzip, br");
        }
        let req = match body {
            Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let (status, encoded) = (res.status(), res.headers().contains_key(header::CONTENT_ENCODING));
            let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
            (status, encoded, bytes)
        }
    };
    let token = Some("an-operator-token-of-enough-length");
    let (none, _, none_said) = call(Method::GET, "/api/operator/settings", None, None, false).await;
    let (wrong, _, wrong_said) = call(Method::GET, "/api/operator/settings", Some("a-guess"), None, false).await;
    assert_eq!((none, wrong), (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN));
    let said = |b: &[u8]| {
        let mut v: serde_json::Value = serde_json::from_slice(b).unwrap();
        v.as_object_mut().unwrap().remove("operation");
        v
    };
    assert_eq!(said(&none_said), said(&wrong_said), "a refusal says the same, token or none");
    let (status, encoded, _) = call(Method::GET, "/api/operator/settings", token, None, true).await;
    assert!(status == StatusCode::OK && encoded, "answered, compressed for a browser as in production: {status}");
    let (status, _, body) = call(Method::GET, "/api/operator/settings", token, None, false).await;
    assert_eq!(status, StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let get = |key: &str| listed["settings"].as_array().unwrap().iter().find(|s| s["key"] == key).cloned().unwrap();
    assert_eq!((get("session_days")["source"].clone(), get("session_days")["value"].clone()), (json!("database"), json!("4")));
    assert_eq!(get("jwt_secret")["value"], json!(null), "a secret's value is never shown");
    assert_eq!(get("db_max_connections")["stored"]["web"], json!("8"), "a pool's own, by pool");
    let (status, _, body) = call(Method::PUT, "/api/operator/settings/session_days", token, Some(json!({"value": 5})), false).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let done: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!((done["changed"][0]["old"].clone(), done["changed"][0]["new"].clone()), (json!("4"), json!("5")));
    let (status, _, _) = call(Method::PUT, "/api/operator/settings/jwt_secret", token, Some(json!({"value": "x"})), false).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a secret is never accepted");
    let (status, _, _) = call(Method::PUT, "/api/operator/settings/session_days", None, Some(json!({"value": 6})), false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = call(Method::DELETE, "/api/operator/settings/session_days", token, None, false).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    state(&ctx).kernel.db.close().await;
}

/// The doctor: what the environment decides, and the replicas still running an older value of a setting that applies
/// at start, until they start again.
async fn doctor(a: &AppState) {
    use ferrum_kernel::doctor::Level;
    let found = |all: &[ferrum_kernel::doctor::Finding], check: &str| all.iter().find(|f| f.check == check).cloned().expect(check);
    let all = ferrum_server::doctor::database_settings_findings(a).await;
    let replicas = found(&all, "settings.replicas");
    assert!(replicas.level == Level::Warning && replicas.detail.starts_with("2 replicas"), "{replicas:?}");
    assert!(replicas.detail.contains("default_workspace") && !replicas.detail.contains("ui_source"), "changed since they started: {replicas:?}");
    assert_eq!(found(&all, "settings.environment").level, Level::Ok, "this test's environment sets none");
    let start = [ferrum_server::settings::Change { name: "ui_source".into(), pool: None, value: Some("http://web:8080/_releases/".into()) }];
    ferrum_server::settings::change(a, &start, "task").await.unwrap();
    let replicas = found(&ferrum_server::doctor::database_settings_findings(a).await, "settings.replicas");
    assert!(replicas.level == Level::Warning && replicas.detail.contains("ui_source"), "both run the older value until they start again: {replicas:?}");
    assert!(replicas.detail.contains(&a.live_settings.replica.to_string()));
    let unset = [ferrum_server::settings::Change { name: "ui_source".into(), pool: None, value: None }];
    ferrum_server::settings::change(a, &unset, "task").await.unwrap();
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
    // The rows were written one by one: the compare may take the first before the notice comes.
    until("the change reaches the other replica", || b.live_settings.now().snapshot.get("default_workspace").1 == Some("acme")).await;
    assert_eq!(b.live_settings.now().settings.user_requests_per_minute, 600);
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

    changes(&a, &b).await;
    routes().await;
    doctor(&a).await;

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
    // A zone FERRUM_ZONES does not name would keep the pool's next replica from starting: unset again.
    row(st, "pool", "web", "zone", None).await;
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
    assert_eq!(
        written,
        ["drive.office_url", "pdf_url"],
        "once, by one of them: session_days is the table's already, a secret is never kept there, and an app's own setting is, as the platform's are"
    );
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
    assert_eq!(
        rows,
        [
            ("drive.office_url".to_string(), json!("http://office"), "environment".to_string()),
            ("pdf_url".to_string(), json!("http://pdf:3000"), "environment".to_string())
        ]
    );
    assert_eq!(changes, [("pdf_url".to_string(), None, json!("http://pdf:3000"), "environment".to_string())], "recorded once");
}
