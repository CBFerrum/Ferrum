//! A consumer's claim on an event given back, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is
//! reset first): `outbox::give_back` undoes `outbox::first_time` for exactly that workspace, consumer and event, and never
//! the dispatcher's record that a consumer finished.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, EventSink, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// Claims `paid` events under its own name before its (pretend) outside call, and finishes.
struct Claims;

#[async_trait]
impl EventSink for Claims {
    fn name(&self) -> &'static str {
        "claims"
    }
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic == "paid" {
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            outbox::first_time(&mut tx, ev.tenant_id, "platform.claims", ev.id).await?;
            tx.commit().await?;
        }
        Ok(())
    }
}

/// Fails every `paid` event, so the dispatcher records the sinks that finished.
struct Fails;

#[async_trait]
impl EventSink for Fails {
    fn name(&self) -> &'static str {
        "fails"
    }
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic == "paid" { Err(Error::Unavailable("the provider is down".into())) } else { Ok(()) }
    }
}

#[test]
fn a_claim_is_given_back_and_nothing_else() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(Arc::new(Claims));
    hooks.add_event_sink(Arc::new(Fails));
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let mut workspaces = vec![];
    for slug in ["acme", "other"] {
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
    let (acme, other) = (workspaces[0], workspaces[1]);
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    // Each step in a transaction of its own workspace, committed, as a sink does it.
    let first = |tenant: Uuid, consumer: &'static str, event: i64| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let first = outbox::first_time(&mut tx, tenant, consumer, event).await.unwrap();
            tx.commit().await.unwrap();
            first
        }
    };
    let give_back = |as_of: Uuid, tenant: Uuid, consumer: &'static str, event: i64| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(as_of).await.unwrap();
            let gone = outbox::give_back(&mut tx, tenant, consumer, event).await.unwrap();
            tx.commit().await.unwrap();
            gone
        }
    };

    // Claim, give back, claim again: first again.
    assert!(first(acme, "irp", 1).await);
    assert!(give_back(acme, acme, "irp", 1).await, "the claim was given back");
    assert!(!give_back(acme, acme, "irp", 1).await, "a second time is harmless and says nothing was there");
    assert!(first(acme, "irp", 1).await, "the retry is told it is the first");

    // Another consumer's claim on the event, and the consumer's claim on another event, stay.
    assert!(first(acme, "mtd", 1).await);
    assert!(first(acme, "irp", 2).await);
    assert!(give_back(acme, acme, "irp", 1).await);
    assert!(!first(acme, "mtd", 1).await, "another consumer's claim on the same event stays");
    assert!(!first(acme, "irp", 2).await, "the same consumer's claim on another event stays");

    // Another workspace cannot give back this one's claim, whatever it names.
    assert!(first(acme, "irp", 3).await);
    assert!(!give_back(other, acme, "irp", 3).await, "row-level security keeps it out");
    assert!(!give_back(other, other, "irp", 3).await, "and its own has none");
    assert!(!first(acme, "irp", 3).await, "the claim stays");

    // A consumer the dispatcher recorded as finished is not given back, even one that claimed under its own name.
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    outbox::enqueue(&mut tx, acme, "paid", None, None, json!({})).await.unwrap();
    tx.commit().await.unwrap();
    k.dispatch_outbox_once().await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let (event, failed): (i64, i32) = sqlx::query_as("select id, attempts from outbox where topic = 'paid'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(failed, 1, "the attempt failed for the failing sink, and the finished one is on record");
    assert!(!give_back(acme, acme, "platform.claims", event).await, "a finished consumer is not given back");
    assert!(!first(acme, "platform.claims", event).await, "its record stays");
    assert!(!give_back(acme, acme, "platform.fails", event).await, "and one that never claimed has nothing to give back");
}
