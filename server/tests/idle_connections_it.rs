//! What an idle replica holds in PostgreSQL: the server binary started in development (a directory for files is allowed there) on a fresh database
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset),
//! left alone for a few seconds, then `pg_stat_activity` grouped by `application_name`. Every connection belongs to a
//! pool that names itself; none is anyone else's (Loco's own pool, which was unnamed, is gone).

mod common;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

#[test]
fn an_idle_replica_holds_only_its_named_pools() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(scenario());
}

async fn scenario() {
    let url = url();
    let observer = format!("{url}{}application_name=idle-observer", if url.contains('?') { '&' } else { '?' });
    let pool = sqlx::PgPool::connect(&observer).await.expect("connect");
    common::reset(&pool).await;
    let dir = std::env::temp_dir().join(format!("ferrum-idle-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("server.log");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = std::fs::File::create(&log).unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_ferrum"))
        .args(["-e", "development", "start", "--no-banner"])
        .current_dir(manifest)
        .env_clear()
        .envs([
            ("DATABASE_URL", url.clone()),
            ("JWT_SECRET", "c2V0dGluZ3MtaXQtand0LXNlY3JldC0wMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZg==".to_string()),
            ("PORT", port.to_string()),
            ("BINDING", "127.0.0.1".to_string()),
            ("PUBLIC_URL", format!("http://127.0.0.1:{port}")),
            ("FERRUM_REQUIRE_OBJECT_STORAGE", "false".to_string()),
            ("FERRUM_FILES_DIR", dir.join("files").to_string_lossy().into_owned()),
            ("FERRUM_PACKAGES_DIR", manifest.join("../packages").to_string_lossy().into_owned()),
            ("LOG_LEVEL", "warn".to_string()),
        ])
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("the server binary");
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().unwrap();
    let mut up = false;
    for _ in 0..360 {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the server stopped ({status}): {}", std::fs::read_to_string(&log).unwrap_or_default());
        }
        if client.get(format!("http://127.0.0.1:{port}/api/ready")).send().await.is_ok_and(|r| r.status().is_success()) {
            up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(up, "the server did not answer: {}", std::fs::read_to_string(&log).unwrap_or_default());
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    let held: Vec<(String, i64)> = sqlx::query_as(
        "select application_name, count(*) from pg_stat_activity where datname = current_database() and application_name <> 'idle-observer' and backend_type = 'client backend' group by 1 order by 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let _ = std::process::Command::new("kill").args(["-TERM", &child.id().to_string()]).status();
    let _ = child.wait();
    println!("idle replica's connections by application_name: {held:?}; total {}", held.iter().map(|(_, n)| n).sum::<i64>());
    let strangers: Vec<_> = held.iter().filter(|(name, _)| !["ferrum", "ferrum-side", "ferrum-direct", "ferrum-read"].contains(&name.as_str())).collect();
    assert!(strangers.is_empty(), "connections of no named pool: {strangers:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
