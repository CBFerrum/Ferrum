//! The order one workspace's events are handled in, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): a batch is handled most urgent first, then oldest first, as the claim chose it.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result, Trace};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Notes the topics of the `order.*` events it is handed, in the order it is handed them.
#[derive(Default)]
struct Seen(Mutex<Vec<String>>);

#[async_trait]
impl EventSink for Seen {
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic.starts_with("order.") {
            self.0.lock().unwrap().push(ev.topic.clone());
        }
        Ok(())
    }
}

#[test]
fn a_batch_is_handled_most_urgent_first_then_oldest_first() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let seen = Arc::new(Seen::default());
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(seen.clone());
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
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
    while k.dispatch_outbox_once().await.unwrap() > 0 {}

    // Written oldest first: an import's row, a person's change, then a sign-in code; then two more at each end.
    for (topic, priority) in [
        ("order.bulk.1", outbox::PRIORITY_BULK),
        ("order.normal.1", 1),
        ("order.urgent.1", outbox::PRIORITY_URGENT),
        ("order.bulk.2", outbox::PRIORITY_BULK),
        ("order.urgent.2", outbox::PRIORITY_URGENT),
    ] {
        let k = k.clone();
        Trace { priority: Some(priority), ..Default::default() }
            .scope(async move {
                let mut tx = k.db.tenant_tx(tenant).await.unwrap();
                outbox::enqueue(&mut tx, tenant, topic, None, None, json!({})).await.unwrap();
                tx.commit().await.unwrap();
            })
            .await;
    }
    assert_eq!(k.dispatch_outbox_once().await.unwrap(), 5, "one batch takes them all");
    assert_eq!(
        *seen.0.lock().unwrap(),
        ["order.urgent.1", "order.urgent.2", "order.normal.1", "order.bulk.1", "order.bulk.2"],
        "the codes go first, then the person's change, then the import, each oldest first"
    );
}
