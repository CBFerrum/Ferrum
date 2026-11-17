//! AT-MSG-001, 002 and 005 as written, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): crashes around the commit and around publication, the same event delivered
//! at the same moment and after a restart, and nothing but what is committed to go on. Then scenario F8:
//! a replica killed holding a batch, and one that stops, leave nothing waiting long.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result, Trace};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// Delivered everything, and counts what it does exactly once: a row and a counter in one transaction.
#[derive(Default)]
struct Consumer {
    delivered: AtomicUsize,
}

#[async_trait]
impl EventSink for Consumer {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != "paid" {
            return Ok(());
        }
        self.delivered.fetch_add(1, Ordering::SeqCst);
        effect(k, ev.tenant_id, ev.id).await
    }
}

/// Holds `held.{n}` until `open` reaches n, and notes what it began and what it finished (its topics).
#[derive(Default)]
struct Held {
    open: AtomicUsize,
    began: std::sync::Mutex<Vec<String>>,
    done: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl EventSink for Held {
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        self.began.lock().unwrap().push(ev.topic.clone());
        if let Some(n) = ev.topic.strip_prefix("held.").and_then(|n| n.parse::<usize>().ok()) {
            while self.open.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        self.done.lock().unwrap().push(ev.topic.clone());
        Ok(())
    }
}

impl Held {
    fn began(&self, topic: &str) -> bool {
        self.began.lock().unwrap().iter().any(|t| t == topic)
    }
    fn done(&self, topic: &str) -> bool {
        self.done.lock().unwrap().iter().any(|t| t == topic)
    }
    async fn begins(&self, topic: &str) {
        for _ in 0..400 {
            if self.began(topic) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{topic} was never begun");
    }
}

async fn effect(k: &Kernel, tenant: Uuid, event: i64) -> Result<()> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    if outbox::first_time(&mut tx, tenant, "ledger", event).await? {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("insert into effect (tenant_id, event) values ('{tenant}', {event})"))).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn count(k: &Kernel, sql: &'static str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

async fn paid(k: &Kernel, tenant: Uuid) -> i64 {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, "paid", None, None, json!({})).await.unwrap();
    tx.commit().await.unwrap();
    count(k, "select max(id) from outbox").await
}

#[test]
fn every_committed_event_arrives_and_counts_once() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = || Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let consumer = Arc::new(Consumer::default());
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(consumer.clone());
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
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
    sqlx::raw_sql(
        "create table effect (tenant_id uuid not null, event bigint not null); alter table effect enable row level security; \
         create policy anyone on effect using (true) with check (true); grant all on effect to public",
    )
    .execute(&k.db.pool)
    .await
    .unwrap();
    drain(&k).await;

    // Crash before the commit: nothing happened, so nothing is said to have.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, "paid", None, None, json!({})).await.unwrap();
    drop(tx);
    drain(&k).await;
    assert_eq!(consumer.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(count(&k, "select count(*) from outbox where topic = 'paid'").await, 0);

    // Crash after the commit, with nothing running: it is all still there.
    paid(&k, tenant).await;
    assert_eq!(count(&k, "select count(*) from outbox where topic = 'paid' and processed_at is null").await, 1);
    drain(&k).await;
    assert_eq!(consumer.delivered.load(Ordering::SeqCst), 1);
    assert_eq!(count(&k, "select count(*) from effect").await, 1);

    // Crash after publication, before saying so: it goes out again, and the effect does not happen again.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set processed_at = null, available_at = now() where topic = 'paid'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert_eq!(consumer.delivered.load(Ordering::SeqCst), 2, "delivered twice");
    assert_eq!(count(&k, "select count(*) from effect").await, 1, "done once");
    assert_eq!(count(&k, "select sum(duplicates)::bigint from event_receipt").await, 1, "and the repeat is on record");

    // AT-MSG-002: the same event at the same moment, many times over.
    let event = paid(&k, tenant).await;
    let at_once: Vec<_> = (0..8)
        .map(|_| {
            let k = k.clone();
            tokio::spawn(async move { effect(&k, tenant, event).await })
        })
        .collect();
    for task in at_once {
        task.await.unwrap().unwrap();
    }
    assert_eq!(count(&k, "select count(*) from effect").await, 2);
    assert_eq!(count(&k, "select sum(duplicates)::bigint from event_receipt").await, 8);

    // A first attempt that rolled back did nothing, so the next one is the first.
    let event = event + 1000;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(outbox::first_time(&mut tx, tenant, "ledger", event).await.unwrap());
    tx.rollback().await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(outbox::first_time(&mut tx, tenant, "ledger", event).await.unwrap());
    tx.rollback().await.unwrap();

    // AT-MSG-005: there is no message to lose. Writers commit, say nothing, and a dispatcher started
    // afterwards finds it all in the table.
    drain(&k).await;
    let before = consumer.delivered.load(Ordering::SeqCst);
    for _ in 0..5 {
        paid(&k, tenant).await;
    }
    let (stop, stopping) = watch::channel(false);
    let dispatcher = k.spawn_outbox_dispatcher(stopping);
    for _ in 0..200 {
        if consumer.delivered.load(Ordering::SeqCst) >= before + 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.send(true).unwrap();
    dispatcher.await.unwrap();
    assert_eq!(consumer.delivered.load(Ordering::SeqCst), before + 5);
    assert_eq!(count(&k, "select count(*) from effect").await, 7);

    // ───────────── scenario F8: a replica killed holding a batch ─────────────
    // Two more replicas of the server, whose sink holds `held.*` events as long as it is told.
    let held = Arc::new(Held::default());
    let replica = || async {
        let db = Db::connect(DbConfig { url: url.clone(), max_connections: 8, ..Default::default() }).await.expect("connect");
        let mut hooks = HookRegistry::default();
        hooks.add_event_sink(held.clone());
        Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot")
    };
    let (a, b) = (replica().await, replica().await);
    let queue = |topic: &'static str, priority: i16| {
        let a = a.clone();
        Trace { priority: Some(priority), ..Default::default() }.scope(async move {
            let mut tx = a.db.tenant_tx(tenant).await.unwrap();
            outbox::enqueue(&mut tx, tenant, topic, None, None, json!({})).await.unwrap();
            tx.commit().await.unwrap();
        })
    };
    // A sign-in code waits in the same batch behind an event whose handler is at work when its replica is killed.
    queue("held.1", 1).await;
    queue("code", outbox::PRIORITY_URGENT).await;
    let (_alive, running) = watch::channel(false);
    let killed = a.spawn_outbox_dispatcher(running);
    held.begins("held.1").await;
    killed.abort();
    let _ = killed.await;
    // Another replica takes the code once the claim on what the dead one had not begun runs out.
    let since = Instant::now();
    while !held.done("code") && since.elapsed() < Duration::from_secs(60) {
        b.dispatch_outbox_once().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(held.done("code"), "the code was sent within a minute of its replica dying, not after the batch's ten-minute lease");
    let mut tx = k.db.system_tx().await.unwrap();
    let left: f64 =
        sqlx::query_scalar("select extract(epoch from available_at - now())::float8 from outbox where topic = 'held.1'").fetch_one(&mut *tx).await.unwrap();
    sqlx::query("update outbox set processed_at = now() where topic = 'held.1'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(left < 150.0, "the event it was at holds no more than its handlers' time: {left} s left");

    // ───────────── a replica that stops hands back what it has not begun ─────────────
    queue("held.2", 1).await;
    queue("held.3", 1).await;
    let (stop, stopping) = watch::channel(false);
    let dispatcher = a.spawn_outbox_dispatcher(stopping);
    held.begins("held.2").await;
    stop.send(true).unwrap();
    held.open.store(2, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(20), dispatcher).await.expect("it stops once what it had begun is done").unwrap();
    assert!(held.done("held.2") && !held.began("held.3"), "it finished what it had begun, and began nothing more");
    let mut tx = k.db.system_tx().await.unwrap();
    let back: bool =
        sqlx::query_scalar("select processed_at is null and available_at <= now() from outbox where topic = 'held.3'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(back, "what it had not begun is free for another replica at once");
    held.open.store(3, Ordering::SeqCst);
    assert_eq!(b.dispatch_outbox_once().await.unwrap(), 1, "and another takes it");
    assert!(held.done("held.3"));
}
