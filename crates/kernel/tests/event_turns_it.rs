//! One workspace's slow event sink and everyone else's events, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`;
//! the database is reset first): while a sink of one workspace is held at an outside call, the replica's dispatcher
//! goes on handling the other workspaces' events, and the held workspace's own events still come one at a time.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// Holds `slow.{n}` until `open` reaches n, as a sink waiting on a provider would, and notes what it began and finished.
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
        if let Some(n) = ev.topic.strip_prefix("slow.").and_then(|n| n.parse::<usize>().ok()) {
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
    /// Waits up to 20 s for `topic` to be done; whether it was.
    async fn finishes(&self, topic: &str) -> bool {
        for _ in 0..400 {
            if self.done(topic) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

#[test]
fn a_held_sink_of_one_workspace_does_not_hold_the_others() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let held = Arc::new(Held::default());
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(held.clone());
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let mut workspaces = vec![];
    for slug in ["slowco", "quickco"] {
        workspaces.push(
            k.create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
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
    let (slow, quick) = (workspaces[0], workspaces[1]);
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let queue = |tenant: Uuid, topic: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            outbox::enqueue(&mut tx, tenant, topic, None, None, json!({})).await.unwrap();
            tx.commit().await.unwrap();
        }
    };

    // One workspace's sink is held at an outside call, with more of its events behind it.
    queue(slow, "slow.1").await;
    queue(slow, "slow.2").await;
    let (stop, stopping) = watch::channel(false);
    let dispatcher = k.spawn_outbox_dispatcher(stopping);
    for _ in 0..400 {
        if held.began("slow.1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(held.began("slow.1"));

    // Another workspace's event, written after that batch was claimed, is handled while the sink is still held.
    let since = Instant::now();
    queue(quick, "quick").await;
    let handled = held.finishes("quick").await;
    eprintln!("the other workspace's event waited {} ms (handled: {handled})", since.elapsed().as_millis());
    assert!(handled, "the other workspace's event was handled while the held sink waited");
    assert!(!held.done("slow.1") && !held.began("slow.2"), "the held workspace's events still come one at a time, in order");

    // Let go: the held workspace's events follow in order, and nothing is left.
    held.open.store(2, Ordering::SeqCst);
    assert!(held.finishes("slow.2").await);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(20), dispatcher).await.expect("it stops").unwrap();
    let order: Vec<String> = held.done.lock().unwrap().iter().filter(|t| t.starts_with("slow.")).cloned().collect();
    assert_eq!(order, ["slow.1", "slow.2"]);
    let mut tx = k.db.system_tx().await.unwrap();
    let left: i64 = sqlx::query_scalar("select count(*) from outbox where processed_at is null").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(left, 0, "everything was handled");
}
