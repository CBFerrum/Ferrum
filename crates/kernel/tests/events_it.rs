//! How events travel, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first): one workspace's backlog does not hold up another's, urgent mail goes first, a chain of
//! events can be followed from the request that started it, dead events can be replayed, and a
//! slow webhook endpoint holds no transaction open.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event, PRIORITY_BULK};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, EventSink, HookRegistry, Kernel, KernelSettings, Result, Trace};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// Fails `flaky` events while told to, and answers `chain.start` with `chain.next`.
struct Sink {
    fail: AtomicBool,
    second: AtomicUsize,
}

#[async_trait]
impl EventSink for Sink {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic == "flaky" && self.fail.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("the provider is down".into()));
        }
        // While this one runs, the rest of its batch passes to another worker, as it does when a
        // claim runs out.
        if ev.topic == "lease.first" {
            let mut tx = k.db.system_tx().await?;
            sqlx::query("update outbox set available_at = now() + interval '1 hour' where tenant_id = $1 and topic = 'lease.second'")
                .bind(ev.tenant_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        if ev.topic == "lease.second" {
            self.second.fetch_add(1, Ordering::SeqCst);
        }
        if ev.topic == "chain.start" {
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            outbox::enqueue(&mut tx, ev.tenant_id, "chain.next", None, None, json!({})).await?;
            tx.commit().await?;
        }
        Ok(())
    }
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
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
    .expect("workspace")
}

async fn emit(k: &Kernel, tenant: Uuid, topic: &str, n: usize) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    for _ in 0..n {
        outbox::enqueue(&mut tx, tenant, topic, None, None, json!({})).await.unwrap();
    }
    tx.commit().await.unwrap();
}

async fn pending(k: &Kernel, tenant: Uuid, topic: &str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query_scalar("select count(*) from outbox where tenant_id = $1 and topic = $2 and processed_at is null")
        .bind(tenant)
        .bind(topic)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

#[test]
fn events_travel_fairly_and_can_be_followed() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let sink = Arc::new(Sink { fail: AtomicBool::new(false), second: AtomicUsize::new(0) });
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(sink.clone());
    let settings = KernelSettings { webhooks_allow_private: true, ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    let (loud, quiet) = (workspace(&k, "loud").await, workspace(&k, "quiet").await);
    drain(&k).await;

    // ───────────── a batch that outlives its claim ─────────────
    emit(&k, quiet, "lease.first", 1).await;
    emit(&k, quiet, "lease.second", 1).await;
    k.dispatch_outbox_once().await.unwrap();
    assert_eq!(pending(&k, quiet, "lease.first").await, 0);
    assert_eq!(sink.second.load(Ordering::SeqCst), 0, "what passed to another worker is not handled here too");
    assert_eq!(pending(&k, quiet, "lease.second").await, 1, "and stays theirs to finish");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set available_at = now() where topic = 'lease.second'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert_eq!(sink.second.load(Ordering::SeqCst), 1, "when that claim runs out too, it is handled, once");

    // ───────────── one workspace's backlog does not hold up another's ─────────────
    emit(&k, loud, "backlog", 500).await;
    emit(&k, quiet, "reset", 1).await;
    assert!(k.dispatch_outbox_once().await.unwrap() <= 11, "a workspace gets a share of a batch");
    assert_eq!(pending(&k, quiet, "reset").await, 0, "the quiet workspace's event went in the first batch, behind 500 of the other's");
    assert!(pending(&k, loud, "backlog").await >= 490, "and the backlog waits its turn");

    // ───────────── within a workspace, what someone is waiting for goes first ─────────────
    let mut tx = k.db.tenant_tx(loud).await.unwrap();
    k.queue_mail(&mut tx, loud, "pat@loud.test", "Your sign-in code", "<p>123456</p>").await.unwrap();
    tx.commit().await.unwrap();
    let mailed = |k: Arc<Kernel>| async move {
        let mut tx = k.db.system_tx().await.unwrap();
        let (priority, attempts): (i16, i32) =
            sqlx::query_as("select priority, attempts from outbox where topic = $1").bind(outbox::MAIL_TOPIC).fetch_one(&mut *tx).await.unwrap();
        (priority, attempts)
    };
    assert_eq!(mailed(k.clone()).await.0, outbox::PRIORITY_URGENT);
    k.dispatch_outbox_once().await.unwrap();
    assert_eq!(mailed(k.clone()).await.1, 1, "tried in the next batch, though it is the newest of 490 (no mailer here, so it failed)");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("delete from outbox where topic = $1").bind(outbox::MAIL_TOPIC).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;

    // Bulk work's events wait behind ordinary ones, urgent mail keeps its place in it.
    let bulk = Trace { priority: Some(PRIORITY_BULK), ..Default::default() };
    bulk.scope(emit(&k, quiet, "imported", 12)).await;
    emit(&k, quiet, "ordinary", 1).await;
    k.dispatch_outbox_once().await.unwrap();
    assert_eq!(pending(&k, quiet, "ordinary").await, 0, "ahead of twelve older bulk events");
    assert_eq!(pending(&k, quiet, "imported").await, 3);
    drain(&k).await;

    // ───────────── a chain of events can be followed from the request that started it ─────────────
    let request =
        Trace { correlation: "req-42".into(), traceparent: Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into()), ..Default::default() };
    request.scope(emit(&k, quiet, "chain.start", 1)).await;
    drain(&k).await;
    let mut tx = k.db.system_tx().await.unwrap();
    let chain: Vec<(i64, String, Option<String>, Option<i64>, Option<String>)> =
        sqlx::query_as("select id, topic, correlation_id, causation_id, traceparent from outbox where topic like 'chain.%' order by id")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].2.as_deref(), Some("req-42"), "the request's id");
    assert_eq!(chain[0].3, None, "nothing caused the first one");
    assert_eq!(chain[1].2.as_deref(), Some("req-42"), "and its follower keeps it");
    assert_eq!(chain[1].3, Some(chain[0].0), "naming the event it answers");
    assert_eq!(chain[1].4, chain[0].4, "with the caller's trace context");
    assert!(chain[1].4.is_some());

    // ───────────── a dead event is seen and replayed, by an administrator of its workspace ─────────────
    sink.fail.store(true, Ordering::SeqCst);
    emit(&k, quiet, "flaky", 1).await;
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set attempts = 14 where topic = 'flaky'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.dispatch_outbox_once().await.unwrap();
    let session = k.login("quiet", "admin@quiet.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(quiet, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let dead = k.dead_events(&admin, 10).await.unwrap();
    assert_eq!(dead.len(), 1, "{dead:?}");
    assert_eq!(dead[0]["topic"], json!("flaky"));
    assert!(dead[0]["last_error"].as_str().unwrap().contains("provider is down"));
    assert!(dead[0].get("payload").is_none(), "payloads stay out of the list");
    let id = dead[0]["id"].as_i64().unwrap();

    let other = k.login("loud", "admin@loud.test", "Correct-Horse-9").await.unwrap();
    let other = k.load_ctx(loud, other.user, Some(other.token_version), "it".into()).await.unwrap();
    assert!(k.dead_events(&other, 10).await.unwrap().is_empty(), "another workspace sees none of it");
    assert!(matches!(k.replay_event(&other, id).await, Err(Error::NotFound(_))), "and cannot replay it");
    let clerk = k.create_user(&admin, "clerk@quiet.test", "Clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(quiet, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(k.dead_events(&clerk, 10).await, Err(Error::Forbidden(_))), "administrators only");
    assert!(matches!(k.replay_event(&clerk, id).await, Err(Error::Forbidden(_))));

    sink.fail.store(false, Ordering::SeqCst);
    k.replay_event(&admin, id).await.unwrap();
    assert!(matches!(k.replay_event(&admin, id).await, Err(Error::NotFound(_))), "it is no longer dead");
    drain(&k).await;
    assert_eq!(pending(&k, quiet, "flaky").await, 0, "handled once the provider is back");
    assert!(k.dead_events(&admin, 10).await.unwrap().is_empty());

    // ───────────── a slow webhook endpoint holds no transaction open ─────────────
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(1500)).await;
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
            });
        }
    });
    let mut tx = k.db.tenant_tx(quiet).await.unwrap();
    sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics) values ($1, $2, 'slow', $3, 's3cret', '{hooked}')")
        .bind(quiet)
        .bind(Uuid::now_v7())
        .bind(&endpoint)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    emit(&k, quiet, "hooked", 1).await;
    drain(&k).await;
    let sender = {
        let k = k.clone();
        tokio::spawn(async move { k.deliver_webhooks_once().await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut tx = k.db.system_tx().await.unwrap();
    let delivery: Option<(Uuid, Value)> = sqlx::query_as("select id, to_jsonb(status) from webhook_delivery where topic = 'hooked' for update nowait")
        .fetch_optional(&mut *tx)
        .await
        .expect("the row is not locked while the endpoint answers");
    let (delivery, _) = delivery.expect("claimed, still pending");
    let holding: i64 = sqlx::query_scalar(
        "select count(*) from pg_stat_activity where datname = current_database() and state = 'idle in transaction' and pid <> pg_backend_pid()",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(holding, 0, "no transaction is waiting on the endpoint");
    tx.commit().await.unwrap();
    assert_eq!(k.deliver_webhooks_once().await.unwrap(), 0, "the lease keeps it from being sent twice");
    assert_eq!(sender.await.unwrap().unwrap(), 1);
    let mut tx = k.db.system_tx().await.unwrap();
    let status: String = sqlx::query_scalar("select status from webhook_delivery where id = $1").bind(delivery).fetch_one(&mut *tx).await.unwrap();
    assert_eq!(status, "delivered");
    sqlx::query("update webhook_delivery set status = 'dead' where id = $1").bind(delivery).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.replay_webhook_delivery(&admin, delivery).await.unwrap();
    assert!(matches!(k.replay_webhook_delivery(&admin, delivery).await, Err(Error::NotFound(_))));
    assert!(matches!(k.replay_webhook_delivery(&other, delivery).await, Err(Error::NotFound(_))), "nor across workspaces");
}
