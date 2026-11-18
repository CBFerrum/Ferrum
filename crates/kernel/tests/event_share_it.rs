//! One sink's share of a replica's turns, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first). A sink held at its provider in seven workspaces at once (a tax portal that is down) runs in at most half of
//! the turns while other workspaces' events wait, so an eighth workspace's events are handled meanwhile; what it was
//! put off for comes back without using an attempt, in order; and with nothing else waiting it uses every turn.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// Waits at its provider on `slow.*` events while `open` is false; notes how many of its calls are held at once.
#[derive(Default)]
struct Blocking {
    open: AtomicBool,
    now: AtomicUsize,
    most: AtomicUsize,
    done: Mutex<Vec<(Uuid, String)>>,
}

#[async_trait]
impl EventSink for Blocking {
    fn name(&self) -> &'static str {
        "blocking"
    }
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if !ev.topic.starts_with("slow.") {
            return Ok(());
        }
        let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(n, Ordering::SeqCst);
        while !self.open.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.now.fetch_sub(1, Ordering::SeqCst);
        self.done.lock().unwrap().push((ev.tenant_id, ev.topic.clone()));
        Ok(())
    }
}

/// Another app's sink: notes the `slow.*` and `quick.*` events it handled.
#[derive(Default)]
struct Other(Mutex<Vec<String>>);

#[async_trait]
impl EventSink for Other {
    fn name(&self) -> &'static str {
        "other"
    }
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic.starts_with("quick.") || ev.topic.starts_with("slow.") {
            self.0.lock().unwrap().push(ev.topic.clone());
        }
        Ok(())
    }
}

impl Other {
    fn has(&self, topic: &str) -> bool {
        self.0.lock().unwrap().iter().any(|t| t == topic)
    }
}

async fn until(what: impl Fn() -> bool, secs: u64) -> bool {
    let since = Instant::now();
    while since.elapsed() < Duration::from_secs(secs) {
        if what() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    what()
}

#[test]
fn a_slow_sink_keeps_to_its_share_of_the_turns() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    // A pool of 16: four turns for events, two of them a sink's share.
    let db = Db::connect(DbConfig { url, max_connections: 16, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let (blocking, other) = (Arc::new(Blocking::default()), Arc::new(Other::default()));
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(blocking.clone());
    hooks.add_event_sink(other.clone());
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let mut workspaces = vec![];
    for n in 0..8 {
        let slug = format!("w{n}");
        workspaces.push(
            k.create_tenant(NewTenant {
                slug: slug.clone(),
                name: slug.clone(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec![],
                settings: None,
            })
            .await
            .unwrap(),
        );
    }
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let queue = |tenant: Uuid, topic: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            outbox::enqueue(&mut tx, tenant, topic, None, None, json!({})).await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    let figures = |sql: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let row: (i64, i64) = sqlx::query_as(sql).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            row
        }
    };

    // ───────────── seven workspaces' events wait at the provider; an eighth's go on ─────────────
    for w in &workspaces[..7] {
        queue(*w, "slow.a").await;
        queue(*w, "slow.b").await;
    }
    queue(workspaces[7], "quick.1").await;
    queue(workspaces[7], "quick.2").await;
    let (stop, stopping) = watch::channel(false);
    let since = Instant::now();
    let dispatcher = k.spawn_outbox_dispatcher(stopping);
    let handled = until(|| other.has("quick.1") && other.has("quick.2"), 20).await;
    eprintln!(
        "the eighth workspace's events: handled {handled} after {} ms, with {} of the slow sink's calls held and none answered",
        since.elapsed().as_millis(),
        blocking.now.load(Ordering::SeqCst)
    );
    assert!(handled, "the eighth workspace's events were handled while the seven waited at the provider");
    assert!(blocking.done.lock().unwrap().is_empty(), "the seven are still held");
    // An event whose slow sink was put off at its share: its other consumer handled it and has its receipt, and it waits,
    // without using an attempt.
    let mut put_off = (0, 0);
    for _ in 0..100 {
        put_off = figures(
            "select count(*), coalesce(max(o.attempts), 0)::int8 from outbox o join event_receipt r on r.event_id = o.id and r.consumer = 'platform.other' \
             where o.topic like 'slow.%' and o.processed_at is null and o.last_error like 'waiting for a turn%'",
        )
        .await;
        if put_off.0 > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("put off for the slow sink: {} events; deferrals counted: {:?}", put_off.0, k.sink_deferrals());
    assert!(put_off.0 > 0, "events were put off for the slow sink alone, the other sink's receipt kept");
    assert_eq!(put_off.1, 0, "being put off is not an attempt");
    assert!(k.sink_deferrals().iter().any(|(name, n)| name == "platform.blocking" && *n > 0), "and counted for /metrics");

    // The provider answers: everything is handled, the slow sink seeing each workspace's events in order, each sink once.
    blocking.open.store(true, Ordering::SeqCst);
    let done = || async {
        figures("select count(*) filter (where processed_at is null), max(attempts)::int8 from outbox where topic like 'slow.%' or topic like 'quick.%'").await
    };
    let since = Instant::now();
    while done().await.0 > 0 && since.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(done().await, (0, 0), "all handled, none of them using an attempt");
    let seen = blocking.done.lock().unwrap().clone();
    for w in &workspaces[..7] {
        let theirs: Vec<&str> = seen.iter().filter(|(t, _)| t == w).map(|(_, topic)| topic.as_str()).collect();
        assert_eq!(theirs, ["slow.a", "slow.b"], "in order, once");
    }
    assert_eq!(other.0.lock().unwrap().len(), 16, "the other sink handled each event once: {:?}", other.0.lock().unwrap());

    // ───────────── with only its own events waiting, it uses every turn ─────────────
    blocking.open.store(false, Ordering::SeqCst);
    blocking.most.store(0, Ordering::SeqCst);
    for w in &workspaces[..7] {
        queue(*w, "slow.c").await;
    }
    let all = until(|| blocking.most.load(Ordering::SeqCst) >= 4, 40).await;
    eprintln!("with nothing else waiting it held {} of the four turns", blocking.most.load(Ordering::SeqCst));
    assert!(all, "with nothing else waiting, the slow sink uses every turn");
    assert_eq!(blocking.most.load(Ordering::SeqCst), 4, "and no more than there are");
    blocking.open.store(true, Ordering::SeqCst);
    let since = Instant::now();
    while done().await.0 > 0 && since.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(done().await.0, 0);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), dispatcher).await.expect("it stops").unwrap();
}
