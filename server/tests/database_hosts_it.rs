//! A database host from the settings table is given `DATABASE_URL`'s password (D7), so the server uses one only where that
//! cannot draw the password to a machine of someone else's: `DATABASE_URL`'s own host (another port is fine), or a
//! connection that checks the server's certificate and its name against an authority given here. Otherwise the host is not
//! used and never connected to; the log says so once and the doctor reports it. A host from the environment is used as
//! before. Against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset), with the server binary started as an operator
//! starts it: each host is a port on this machine that counts the connections made to it. Last, on an empty database, the
//! binary's start sets itself up once, and its log says what that did; and a replica's set-up leaves the apps' upkeep to
//! the upkeep runner.

mod common;

use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::config::Config;
use loco_rs::environment::Environment;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

fn config() -> Config {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, url());
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    config
}

/// A port on this machine that counts every connection made to it, and answers none.
fn counting() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let n = Arc::new(AtomicUsize::new(0));
    let seen = n.clone();
    std::thread::spawn(move || {
        for c in listener.incoming() {
            if c.is_ok() {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (port, n)
}

/// Another name for this machine than the one `DATABASE_URL` uses.
fn other_name() -> &'static str {
    let host = reqwest::Url::parse(&url()).unwrap().host_str().unwrap_or_default().to_string();
    if host == "127.0.0.1" { "localhost" } else { "127.0.0.1" }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn ferrum(args: &[&str], env: &[(&str, String)], log: &std::path::Path) -> std::process::Child {
    let out = std::fs::File::create(log).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_ferrum"))
        .args(["-e", "development"])
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("the server binary")
}

/// Waits up to five minutes (an empty database is migrated first) for the server on `port` to answer `/api/ready`.
async fn ready(port: u16, child: &mut std::process::Child, log: &std::path::Path) {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap();
    for _ in 0..600 {
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

/// Waits up to two minutes for `child` to end by itself (a start that fails), and says whether it did.
async fn ends(child: &mut std::process::Child) -> bool {
    for _ in 0..240 {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let _ = child.kill();
    false
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

/// Writes the installation's row of `key` by hand, as an operator's tool would; `None` takes it out.
async fn set(st: &AppState, key: &str, value: Option<&str>) {
    let mut tx = st.kernel.db.system_tx().await.unwrap();
    match value {
        Some(value) => sqlx::query(
            "insert into setting (scope, scope_name, key, value, updated_by) values ('installation', '', $1, to_jsonb($2::text), 'test') \
             on conflict (scope, scope_name, key) do update set value = excluded.value, version = nextval('setting_version')",
        )
        .bind(key)
        .bind(value),
        None => sqlx::query("delete from setting where scope = 'installation' and key = $1").bind(key),
    }
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[test]
fn a_stored_host_never_draws_the_password_elsewhere() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let dir = std::env::temp_dir().join(format!("ferrum-hosts-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pool = sqlx::PgPool::connect(&url()).await.expect("connect");
    common::reset(&pool).await;
    pool.close().await;
    let ctx = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config()).build()).await.expect("set-up");
    let st = ctx.shared_store.get::<AppState>().expect("state");
    let port = free_port();
    for (key, value) in [
        ("port", port.to_string()),
        ("binding", "127.0.0.1".to_string()),
        ("public_url", format!("http://127.0.0.1:{port}")),
        // Where the set-up above keeps its files (development and tests keep them in a directory).
        ("files_dir", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(&st.settings.files_dir).to_string_lossy().into_owned()),
        ("packages_dir", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages").to_string_lossy().into_owned()),
        ("log_level", "warn".to_string()),
    ] {
        set(&st, key, Some(&value)).await;
    }
    let secrets = vec![("DATABASE_URL", url()), ("JWT_SECRET", "aG9zdHMtaXQtand0LXNlY3JldC0wMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZjAxMjM=".to_string())];

    // Another host, from the table, with no certificate checked: not used, never connected to; said in the log and by the
    // doctor. The server starts on DATABASE_URL alone, as without the setting.
    let (elsewhere, reached) = counting();
    set(&st, "database_direct_host", Some(&format!("{}:{elsewhere}", other_name()))).await;
    set(&st, "database_read_host", Some(&format!("{}:{elsewhere}", other_name()))).await;
    let log = dir.join("refused.log");
    let mut server = ferrum(&["start", "--no-banner"], &secrets, &log);
    ready(port, &mut server, &log).await;
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(said.contains("the setting database_direct_host from the database is not used"), "{said}");
    assert!(said.contains("the setting database_read_host from the database is not used"), "{said}");
    let mut doctor = ferrum(&["task", "doctor"], &secrets, &dir.join("doctor.log"));
    assert!(ends(&mut doctor).await, "the doctor ends");
    let report = std::fs::read_to_string(dir.join("doctor.log")).unwrap();
    assert!(report.contains("database.direct_host") && report.contains("database.read_host") && report.contains("is not used"), "{report}");
    stop(&mut server);
    assert_eq!(reached.load(Ordering::SeqCst), 0, "a refused host never receives a connection");

    // DATABASE_URL's own host on another port, from the table: used (the start tries it, and fails on a port that is no
    // database).
    let (beside, tried) = counting();
    let own = reqwest::Url::parse(&url()).unwrap().host_str().unwrap().to_string();
    set(&st, "database_read_host", None).await;
    set(&st, "database_direct_host", Some(&format!("{own}:{beside}"))).await;
    let mut server = ferrum(&["start", "--no-banner"], &secrets, &dir.join("same-host.log"));
    assert!(ends(&mut server).await, "a direct address that is no database stops the start");
    assert!(tried.load(Ordering::SeqCst) > 0, "the same host is used");

    // Any host, from the environment: used as before.
    let (named, used) = counting();
    set(&st, "database_direct_host", None).await;
    let mut env = secrets.clone();
    env.push(("FERRUM_DATABASE_DIRECT_HOST", format!("{}:{named}", other_name())));
    let mut server = ferrum(&["start", "--no-banner"], &env, &dir.join("environment.log"));
    assert!(ends(&mut server).await, "a direct address that is no database stops the start");
    assert!(used.load(Ordering::SeqCst) > 0, "the environment's host is used");

    st.kernel.db.close().await;

    // `ferrum start` sets itself up once, and its log says what that did: on an empty database, the schema it made.
    // (Loco's command line sets up twice, the first time before there is a log: what a start did went unsaid, and each
    // replica kept two of everything the set-up makes.)
    let pool = sqlx::PgPool::connect(&url()).await.expect("connect");
    common::reset(&pool).await;
    pool.close().await;
    let (port, log) = (free_port(), dir.join("once.log"));
    let out = std::fs::File::create(&log).unwrap();
    let mut server = std::process::Command::new(env!("CARGO_BIN_EXE_ferrum"))
        .args(["-e", "development", "start", "--no-banner"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .envs(secrets.iter().map(|(k, v)| (*k, v.clone())))
        .envs([
            ("PORT", port.to_string()),
            ("BINDING", "127.0.0.1".to_string()),
            ("FERRUM_FILES_DIR", dir.join("files").to_string_lossy().into_owned()),
            ("LOG_LEVEL", "info".to_string()),
        ])
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("the server binary");
    ready(port, &mut server, &log).await;
    stop(&mut server);
    let said = std::fs::read_to_string(&log).unwrap();
    assert_eq!(said.matches("pretty backtraces are enabled").count(), 1, "one set-up (development's configuration says this once in each): {said}");
    assert_eq!(said.matches("schema synced").count(), 1, "what the start did is in its log: {said}");

    // A replica's set-up does no app's upkeep: that is the runner's, workspace by workspace (crate::packages::spawn). A
    // second replica starting on this database leaves ERP's (here: next year's ledger partition, gone) as it found it; a
    // workspace's ERP upkeep makes it again (ERP's tests/erp_it.rs).
    let one = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config()).build()).await.expect("a replica");
    let one = one.shared_store.get::<AppState>().expect("state");
    let pool = sqlx::PgPool::connect(&url()).await.expect("connect");
    let next: String = sqlx::query_scalar("select format('journal_line_y%s', extract(year from now())::int + 1)").fetch_one(&pool).await.unwrap();
    assert!(table(&pool, &next).await, "ERP's ledger has next year's partition");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("drop table {next}"))).execute(&pool).await.unwrap();
    let two = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config()).build()).await.expect("another replica");
    two.shared_store.get::<AppState>().expect("state").kernel.db.close().await;
    assert!(!table(&pool, &next).await, "a replica's start ran ERP's upkeep itself, outside the runner");
    pool.close().await;
    one.kernel.db.close().await;

    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether the table `name` is there.
async fn table(pool: &sqlx::PgPool, name: &str) -> bool {
    sqlx::query_scalar("select to_regclass($1) is not null").bind(name).fetch_one(pool).await.unwrap()
}
