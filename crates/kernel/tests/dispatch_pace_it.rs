//! How an idle replica's dispatcher paces its looks, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database
//! is reset first): it looks less and less often while there is nothing to do, an event this process writes wakes it, a
//! wake that comes while it is busy is not lost, what another replica wrote is found within the cap, and a hold set
//! elsewhere stops it at its next look. Counts of looks that found nothing (`Kernel::outbox_empty_looks`), not times,
//! carry the verdicts; the waits only give the counts room.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Notes the `paced.*` events it is handed, and how many empty looks the dispatcher had made by then.
#[derive(Default)]
struct Seen(Mutex<Vec<(String, u64)>>);

#[async_trait]
impl EventSink for Seen {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic.starts_with("paced.") {
            self.0.lock().unwrap().push((ev.topic.clone(), k.outbox_empty_looks()));
        }
        Ok(())
    }
}

impl Seen {
    fn has(&self, topic: &str) -> bool {
        self.at(topic).is_some()
    }
    /// The looks made when it was handled.
    fn at(&self, topic: &str) -> Option<u64> {
        self.0.lock().unwrap().iter().find(|(t, _)| t == topic).map(|(_, n)| *n)
    }
}

#[test]
fn an_idle_dispatcher_rests_and_wakes() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn until(what: impl Fn() -> bool, secs: u64) -> bool {
    let since = Instant::now();
    while since.elapsed() < Duration::from_secs(secs) {
        if what() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    what()
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let catalog = || Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let seen = Arc::new(Seen::default());
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(seen.clone());
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant: uuid::Uuid = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let write = |topic: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            outbox::enqueue(&mut tx, tenant, topic, None, None, json!({})).await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    // As another replica writes it: no wake reaches this process.
    let written_elsewhere = |topic: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            sqlx::query("insert into outbox (tenant_id, topic, payload, build_date) values ($1, $2, '{}', '1970-01-01')")
                .bind(tenant)
                .bind(topic)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    let (stop, stopping) = watch::channel(false);
    let dispatcher = k.spawn_outbox_dispatcher(stopping);

    // ───────────── with nothing to do, it looks less and less often ─────────────
    let mut per_second = vec![];
    for _ in 0..12 {
        let before = k.outbox_empty_looks();
        tokio::time::sleep(Duration::from_secs(1)).await;
        per_second.push(k.outbox_empty_looks() - before);
    }
    eprintln!("looks in each second of an idle dispatcher: {per_second:?}");
    assert!(per_second[0] >= 4, "it looks often at first: {per_second:?}");
    assert!(per_second[8..].iter().all(|n| *n <= 1), "and at most once a second once it has rested: {per_second:?}");
    assert!(per_second[8..].iter().sum::<u64>() <= 2, "about every five seconds: {per_second:?}");

    // ───────────── an event this process writes wakes it ─────────────
    for (n, topic) in ["paced.1", "paced.2", "paced.3"].into_iter().enumerate() {
        // Rested: the next look would be seconds away.
        tokio::time::sleep(Duration::from_millis(3000 + 400 * n as u64)).await;
        // Counted from its commit: a look before it could not see it.
        write(topic).await;
        let (before, since) = (k.outbox_empty_looks(), Instant::now());
        assert!(until(|| seen.has(topic), 10).await, "{topic} handled");
        let (looks, took) = (seen.at(topic).unwrap() - before, since.elapsed());
        eprintln!("{topic}: handled {} ms after its commit, after {looks} look(s)", took.as_millis());
        // Its wake's look took it; the sink is reached a little later, and a look or two may come back empty meanwhile
        // (the workspace is in hand). A rest instead would have meant seconds with no look at all.
        assert!(looks <= 2, "{topic}: {looks} empty looks between its commit and its handling");
        assert!(took < ferrum_kernel::outbox::IDLE_CAP / 2, "{topic}: handled {} ms after its commit, not after a rest", took.as_millis());
    }

    // ───────────── a wake while it is busy is not lost ─────────────
    // Events written as fast as they commit, many of them while the dispatcher is between a look and its rest: each is
    // handled after a few looks, never after a rest.
    for n in 0..30 {
        let topic: &'static str = Box::leak(format!("paced.burst.{n}").into_boxed_str());
        write(topic).await;
        let before = k.outbox_empty_looks();
        assert!(until(|| seen.has(topic), 10).await, "{topic} handled");
        let looks = seen.at(topic).unwrap() - before;
        // None, or one when that look found the workspace still at the event before (its events go one at a time).
        assert!(looks <= 3, "{topic}: {looks} empty looks after its commit");
    }

    // ───────────── what another replica wrote is found within the cap ─────────────
    tokio::time::sleep(Duration::from_secs(12)).await;
    let (before, since) = (k.outbox_empty_looks(), Instant::now());
    written_elsewhere("paced.elsewhere").await;
    assert!(until(|| seen.has("paced.elsewhere"), 10).await, "found");
    let looks = seen.at("paced.elsewhere").unwrap() - before;
    eprintln!("written elsewhere: found {} ms later, after {looks} look(s)", since.elapsed().as_millis());
    assert_eq!(looks, 0, "the next look found it, the cap's wait at most");

    // ───────────── a hold set elsewhere stops it at its next look ─────────────
    // Another replica notices a restore (its identity changed) and holds; this one is resting.
    let db = Db::connect(DbConfig { url, max_connections: 4, ..Default::default() }).await.expect("connect");
    let other = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    assert!(!k.holding().await.unwrap());
    tokio::time::sleep(Duration::from_secs(6)).await;
    sqlx::query("update platform_incarnation set system_identifier = system_identifier + 1").execute(&other.db.pool).await.unwrap();
    other.look_again().await.unwrap();
    assert!(other.restore_held().await.unwrap());
    let since = Instant::now();
    while !k.holding().await.unwrap() && since.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    eprintln!("the hold reached this replica in {} ms", since.elapsed().as_millis());
    assert!(k.holding().await.unwrap(), "this replica heard of the hold");
    written_elsewhere("paced.held").await;
    let before = k.outbox_empty_looks();
    assert!(until(|| k.outbox_empty_looks() > before, 10).await, "it looked");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!seen.has("paced.held"), "nothing went out while held");
    let admin = {
        let session = other.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
        other.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
    };
    other.release_hold(&admin, "checked: nothing went out twice").await.unwrap();
    assert!(until(|| seen.has("paced.held"), 10).await, "released: it goes out");

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), dispatcher).await.expect("it stops").unwrap();
}
