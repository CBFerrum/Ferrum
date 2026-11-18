//! A replica killed while one sink of an event is at work, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): the sinks that had finished are called again when another replica takes the event, as at
//! least once allows, and a sink that asks `outbox::first_time` in the transaction of its effect does it once.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// Called first: counts its calls, and writes its effect once (`first_time`) when `guarded`, every time when not.
struct Effect {
    name: &'static str,
    guarded: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl EventSink for Effect {
    fn name(&self) -> &'static str {
        self.name
    }
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != "paid" {
            return Ok(());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        if !self.guarded || outbox::first_time(&mut tx, ev.tenant_id, self.name, ev.id).await? {
            sqlx::query("insert into effect (tenant_id, sink, event) values ($1, $2, $3)")
                .bind(ev.tenant_id)
                .bind(self.name)
                .bind(ev.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Called last: waits at a provider until `open`.
#[derive(Default)]
struct Provider {
    open: AtomicUsize,
    began: AtomicUsize,
}

#[async_trait]
impl EventSink for Provider {
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != "paid" {
            return Ok(());
        }
        self.began.fetch_add(1, Ordering::SeqCst);
        while self.open.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

#[test]
fn a_killed_replica_repeats_the_sinks_that_had_finished() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let catalog = || Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let plain = Arc::new(Effect { name: "plain", guarded: false, calls: AtomicUsize::new(0) });
    let guarded = Arc::new(Effect { name: "guarded", guarded: true, calls: AtomicUsize::new(0) });
    let provider = Arc::new(Provider::default());
    let hooks = || {
        let mut hooks = HookRegistry::default();
        hooks.add_event_sink(plain.clone());
        hooks.add_event_sink(guarded.clone());
        hooks.add_event_sink(provider.clone());
        hooks
    };
    let a = Kernel::boot(db, catalog(), hooks(), KernelSettings::default(), None).await.expect("boot");
    let tenant = a
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
        "create table effect (tenant_id uuid not null, sink text not null, event bigint not null); alter table effect enable row level security; \
         create policy anyone on effect using (true) with check (true); grant all on effect to public",
    )
    .execute(&a.db.pool)
    .await
    .unwrap();
    while a.dispatch_outbox_once().await.unwrap() > 0 {}
    let mut tx = a.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, "paid", None, None, json!({})).await.unwrap();
    tx.commit().await.unwrap();

    // The first two sinks finish; the replica is killed while the third waits at its provider.
    let (_alive, running) = watch::channel(false);
    let killed = a.spawn_outbox_dispatcher(running);
    for _ in 0..400 {
        if provider.began.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(provider.began.load(Ordering::SeqCst), 1);
    killed.abort();
    let _ = killed.await;

    // Its hold on the event runs out (150 s; moved on here), and another replica takes it.
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    let b = Kernel::boot(db, catalog(), hooks(), KernelSettings::default(), None).await.expect("boot");
    let mut tx = b.db.system_tx().await.unwrap();
    sqlx::query("update outbox set available_at = now() where topic = 'paid'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    provider.open.store(1, Ordering::SeqCst);
    assert_eq!(b.dispatch_outbox_once().await.unwrap(), 1);

    let mut tx = b.db.system_tx().await.unwrap();
    let effects: Vec<(String, i64)> = sqlx::query_as("select sink, count(*) from effect group by sink order by sink").fetch_all(&mut *tx).await.unwrap();
    let handled: bool = sqlx::query_scalar("select processed_at is not null from outbox where topic = 'paid'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(handled);
    assert_eq!((plain.calls.load(Ordering::SeqCst), guarded.calls.load(Ordering::SeqCst)), (2, 2), "both finished sinks were called again");
    assert_eq!(effects, [("guarded".to_string(), 1), ("plain".to_string(), 2)], "the guarded one did its work once, the plain one twice");
}
