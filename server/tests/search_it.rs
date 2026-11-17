//! The bar's search against a real PostgreSQL (`FERRUM_SEARCH_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_search_test`; the database is reset). Its queries are the
//! request's: one the client gave up on (the bar drops it at the next keystroke) starts no further query, and
//! a slow one answers by its deadline with what finished, marked partial; either way, what it had started is
//! stopped by PostgreSQL at the search's own budget. Every searched entity here is a
//! custom one, kept in `custom_record`, so that holding that table makes each of their queries wait where the
//! test can count them (`pg_locks`).

mod common;

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn a_search_is_its_requests_and_answers_by_its_deadline() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_SEARCH_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_search_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 20, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    // Six times as many entities as a search asks at once.
    let kinds: Vec<ferrum_kernel::customize::NewCustomization> = (0..36)
        .map(|i| {
            let name = format!("XKind{i:02}");
            serde_json::from_value(json!({"entity": name, "op": {"op": "define_entity", "entity": {"name": name, "label": name, "kind": "master",
                "title_field": "title", "fields": [{"name": "title", "type": "data", "in_list": true}]}}}))
            .unwrap()
        })
        .collect();
    k.batch_customizations(&admin, kinds, false).await.map_err(|(_, e)| e.to_string()).unwrap();
    let ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();

    let watcher = sqlx::PgPool::connect(&url).await.expect("connect");
    let hold = async || {
        let mut tx = watcher.begin().await.unwrap();
        sqlx::query("lock table custom_record in access exclusive mode").execute(&mut *tx).await.unwrap();
        tx
    };
    // The search's queries started and waiting for the table the test holds.
    let waiting = async || -> i64 {
        sqlx::query_scalar(
            "select count(*) from pg_locks where not granted and relation = 'custom_record'::regclass \
             and database = (select oid from pg_database where datname = current_database())",
        )
        .fetch_one(&watcher)
        .await
        .unwrap()
    };

    // ───── a search the client gave up on starts no further query ─────
    let held = hold().await;
    let search = tokio::spawn({
        let (k, ctx) = (k.clone(), ctx.clone());
        async move { ferrum_server::search::search(&k, &ctx, "zz", 10).await }
    });
    let started = Instant::now();
    while waiting().await < 6 {
        assert!(started.elapsed() < Duration::from_secs(10), "the search's first queries wait for the table");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // What the server does with a request whose client went away: it drops the handler.
    search.abort();
    assert!(search.await.is_err_and(|e| e.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let when_dropped = waiting().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let since = waiting().await;
    assert!(since <= when_dropped, "no query starts once the request is gone: {when_dropped} were waiting when it was dropped, {since} a second later");
    assert_eq!(since, 0, "what it had started is stopped by PostgreSQL within its own budget, not left holding its connection");
    held.commit().await.unwrap();
    // What it had started ends with its statement, and leaves no transaction open.
    let started = Instant::now();
    let open = async || -> i64 {
        sqlx::query_scalar(
            "select count(*) from pg_stat_activity where datname = current_database() and application_name = 'ferrum' and xact_start is not null",
        )
        .fetch_one(&watcher)
        .await
        .unwrap()
    };
    while open().await > 0 {
        assert!(started.elapsed() < Duration::from_secs(5), "a transaction of the dropped search is still open");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ───── a slow search answers by its deadline, with what finished ─────
    let held = hold().await;
    let started = Instant::now();
    let found = ferrum_server::search::search(&k, &ctx, "zz", 10).await.unwrap();
    let took = started.elapsed();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let left = waiting().await;
    held.commit().await.unwrap();
    assert_eq!(found["partial"], json!(true), "{found}");
    assert_eq!(left, 0, "a second after a partial answer, none of its queries still waits");
    assert!(took < Duration::from_millis(1300), "a search with every table held answers by its deadline, not after {took:?}");
}
