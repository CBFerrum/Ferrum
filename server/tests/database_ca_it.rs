//! An outside database's certificate authority (D1: `/run/ferrum/db-ca.pem`), through the whole start of the server
//! binary. The binary is a debug build, so the test points the lookup at a folder of its own (`FERRUM_TEST_RUN_DIR`,
//! which a release build never reads). With the authority that signed the database's certificate, and an address that
//! asks for the certificate and its name to be checked, it starts: every connection it makes (Loco's, the kernel's
//! pools, the vault's) had to trust that file, since the same address without it is refused. With another authority
//! it refuses to start, naming the file; with none, and an address as before, it starts as before. A direct host kept in
//! the settings table under another name than `DATABASE_URL`'s is used with the authority (its certificate and name are
//! checked) and never connected to without it. Against a PostgreSQL with TLS and an authority of its own
//! (scripts/managed_pg.sh: `FERRUM_TEST_TLS_URL`, `FERRUM_TEST_TLS_CA`, `FERRUM_TEST_TLS_OTHER_CA`); skipped, saying so,
//! without one.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// `ferrum start` as a developer runs it, given only the database's address, the secrets, its port and the folder the
/// authority is looked for in; its output to `log`.
fn start(url: &str, run_dir: &std::path::Path, port: u16, files: &std::path::Path, log: &std::path::Path) -> std::process::Child {
    let out = std::fs::File::create(log).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_ferrum"))
        .args(["-e", "development", "start", "--no-banner"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .envs([
            ("DATABASE_URL", url.to_string()),
            ("FERRUM_TEST_RUN_DIR", run_dir.to_string_lossy().into_owned()),
            ("PORT", port.to_string()),
            ("BINDING", "127.0.0.1".to_string()),
            ("FERRUM_FILES_DIR", files.to_string_lossy().into_owned()),
            ("LOG_LEVEL", "warn".to_string()),
            ("JWT_SECRET", "Y2EtaXQtand0LXNlY3JldC0wMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY=".to_string()),
        ])
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("the server binary")
}

/// Waits up to five minutes: `Ok` once the server answers `/api/ready`, `Err` with its log once it has stopped.
async fn ready(port: u16, child: &mut std::process::Child, log: &std::path::Path) -> Result<(), String> {
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap();
    for _ in 0..600 {
        if let Ok(Some(_)) = child.try_wait() {
            return Err(std::fs::read_to_string(log).unwrap_or_default());
        }
        if client.get(format!("http://127.0.0.1:{port}/api/ready")).send().await.is_ok_and(|r| r.status().is_success()) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("the server neither answered nor stopped: {}", std::fs::read_to_string(log).unwrap_or_default());
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

/// Writes the installation's `database_direct_host` by hand, as an operator's tool would; `None` takes it out.
async fn direct_host(url: &str, host: Option<&str>) {
    let pool = sqlx::PgPool::connect(url).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await.unwrap();
    match host {
        Some(host) => sqlx::query(
            "insert into setting (scope, scope_name, key, value, updated_by) values ('installation', '', 'database_direct_host', to_jsonb($1::text), 'test') \
             on conflict (scope, scope_name, key) do update set value = excluded.value, version = nextval('setting_version')",
        )
        .bind(host),
        None => sqlx::query("delete from setting where scope = 'installation' and key = 'database_direct_host'"),
    }
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    pool.close().await;
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

#[test]
fn the_start_trusts_the_databases_authority() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (Some(url), Some(ca), Some(other)) = (get("FERRUM_TEST_TLS_URL"), get("FERRUM_TEST_TLS_CA"), get("FERRUM_TEST_TLS_OTHER_CA")) else {
        return eprintln!("skipped: set FERRUM_TEST_TLS_URL, FERRUM_TEST_TLS_CA and FERRUM_TEST_TLS_OTHER_CA (scripts/managed_pg.sh)");
    };
    let checked = format!("{url}{}sslmode=verify-full", if url.contains('?') { '&' } else { '?' });
    let dir = std::env::temp_dir().join(format!("ferrum-ca-it-{}", std::process::id()));
    let (right, wrong, absent, files) = (dir.join("right"), dir.join("wrong"), dir.join("absent"), dir.join("files"));
    for d in [&right, &wrong, &absent, &files] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::copy(&ca, right.join("db-ca.pem")).unwrap();
    std::fs::copy(&other, wrong.join("db-ca.pem")).unwrap();
    let run = |name: &str, url: &str, run_dir: &std::path::Path| {
        let (port, log) = (free_port(), dir.join(format!("{name}.log")));
        (start(url, run_dir, port, &files, &log), port, log)
    };

    // Without the file, an address that asks for the certificate and its name to be checked is refused: the program's
    // own authorities do not know this database's.
    let (mut server, port, log) = run("absent-checked", &checked, &absent);
    ready(port, &mut server, &log).await.expect_err("it starts with nothing to check the certificate against");

    // Present and right: it starts, with that same address. Every connection it made trusted the file.
    let (mut server, port, log) = run("right", &checked, &right);
    ready(port, &mut server, &log).await.unwrap_or_else(|log| panic!("it did not start with its authority: {log}"));
    stop(&mut server);

    // A direct host in the settings table, by another name than DATABASE_URL's (here a port that counts connections and
    // is no database). With the authority, the address checks the certificate and its name: the host is used, and the
    // start stops on it. Without it, the host is never connected to, the log says so, and the server starts.
    let (elsewhere, reached) = counting();
    direct_host(&url, Some(&format!("localhost:{elsewhere}"))).await;
    let (mut server, port, log) = run("stored-checked", &url, &right);
    ready(port, &mut server, &log).await.expect_err("a direct host that is no database stops the start");
    assert!(reached.load(Ordering::SeqCst) > 0, "with the authority, the stored host is used");
    let tried = reached.load(Ordering::SeqCst);
    let (mut server, port, log) = run("stored-unchecked", &url, &absent);
    ready(port, &mut server, &log).await.unwrap_or_else(|log| panic!("it did not start on DATABASE_URL alone: {log}"));
    stop(&mut server);
    assert_eq!(reached.load(Ordering::SeqCst), tried, "without it, the stored host is never connected to");
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(said.contains("the setting database_direct_host from the database is not used"), "{said}");
    direct_host(&url, None).await;

    // Present and wrong: it refuses to start, naming the cause and the file.
    let (mut server, port, log) = run("wrong", &url, &wrong);
    let said = ready(port, &mut server, &log).await.expect_err("it starts with an authority that did not sign the certificate");
    let file = wrong.join("db-ca.pem");
    assert!(said.contains("does not verify against the certificate authority in") && said.contains(&*file.to_string_lossy()), "{said}");

    // Absent, with the address as before: it starts as before.
    let (mut server, port, log) = run("absent", &url, &absent);
    ready(port, &mut server, &log).await.unwrap_or_else(|log| panic!("it did not start without an authority: {log}"));
    stop(&mut server);
    let _ = std::fs::remove_dir_all(&dir);
}
