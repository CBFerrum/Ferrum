//! FLC-HOOK-008: a chain of events that keeps answering itself across apps is stopped, and kept with a
//! diagnosis, before its work is unbounded; so is one change followed by too many events. Against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; reset first).

use async_trait::async_trait;
use ferrum_kernel::hooks::EventSink;
use ferrum_kernel::jobs::{Job, JobHandler, NewJob};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, Result, automation};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const PACKAGE: &str = "package: chains\nversion: 1.0.0\ndepends: [core]\n";
const ENTITIES: &str = r#"
- {name: Alpha, label: Alpha, kind: master, fields: [{name: n, type: int}]}
- {name: Beta, label: Beta, kind: master, fields: [{name: n, type: int}]}
- {name: Gamma, label: Gamma, kind: master, fields: [{name: n, type: int}]}
- {name: Delta, label: Delta, kind: master, fields: [{name: n, type: int}]}
- {name: Epsilon, label: Epsilon, kind: master, fields: [{name: n, type: int}]}
- {name: Zeta, label: Zeta, kind: master, fields: [{name: n, type: int}]}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn system(k: &Kernel, tenant: Uuid) -> Result<Ctx> {
    Ok(Ctx::system(tenant, k.registry.schema(tenant).await?))
}

/// Two apps, each answering the other's changes: A changes a Beta when an Alpha changes, B an Alpha when a
/// Beta does. Neither is wrong alone.
struct Apps;

#[async_trait]
impl EventSink for Apps {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        let answer = match ev.topic.split('.').next() {
            Some("Alpha") => vec!["Beta"],
            Some("Beta") => vec!["Alpha"],
            // One change answered by three, each answered by three.
            Some("Gamma") => vec!["Gamma"; 3],
            _ => vec![],
        };
        for entity in answer {
            k.insert(&system(k, ev.tenant_id).await?, entity, obj(json!({"n": 1}))).await?;
        }
        if ev.topic == "Delta.created" {
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            ferrum_kernel::jobs::enqueue(
                &mut tx,
                ev.tenant_id,
                None,
                NewJob {
                    kind: "chains.echo".into(),
                    payload: json!({}),
                    priority: 1,
                    max_attempts: 1,
                    dedupe_key: Some(format!("echo-{}", ev.id)),
                    undo_on_cancel: false,
                },
            )
            .await?;
            tx.commit().await?;
        }
        Ok(())
    }
}

/// Work an event handler queued: what it saves is as far down the chain as the handler was.
struct Echo;

#[async_trait]
impl JobHandler for Echo {
    fn kind(&self) -> &'static str {
        "chains.echo"
    }
    fn title(&self) -> &'static str {
        "Echo"
    }
    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        k.insert(&system(k, job.tenant_id).await?, "Epsilon", obj(json!({"n": 1}))).await?;
        Ok(json!({}))
    }
}

async fn drain(k: &Kernel) {
    for _ in 0..60 {
        if k.dispatch_outbox_once().await.unwrap() == 0 {
            return;
        }
    }
    panic!("the outbox never emptied: a chain was not stopped");
}

async fn rows(k: &Kernel, tenant: Uuid, like: &str) -> Vec<(i64, String, i16, bool, Option<String>)> {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let rows = sqlx::query_as("select id, topic, depth, dead, last_error from outbox where tenant_id = $1 and topic like $2 order by id")
        .bind(tenant)
        .bind(like)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rows
}

#[test]
fn event_chains_are_stopped_with_a_diagnosis() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/chains.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(Arc::new(Apps));
    hooks.add_job_handler(Arc::new(Echo));
    let settings = KernelSettings { chain_events: 30, ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "chain".into(),
            name: "chain".into(),
            admin_email: "admin@chain.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["chains".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("chain", "admin@chain.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();

    // A to B to A, across two apps: stopped at the chain's depth, kept dead with what happened.
    k.insert(&admin, "Alpha", obj(json!({"n": 0}))).await.unwrap();
    drain(&k).await;
    let chain: Vec<_> = rows(&k, tenant, "Alpha.%").await.into_iter().chain(rows(&k, tenant, "Beta.%").await).collect();
    let deepest = chain.iter().map(|r| r.2).max().unwrap();
    assert_eq!(deepest, 8, "the chain went exactly as deep as allowed: {chain:?}");
    assert_eq!(chain.len(), 9, "one event per step, the start and eight more");
    let stopped: Vec<_> = chain.iter().filter(|r| r.3).collect();
    assert_eq!(stopped.len(), 1, "the event at the limit was stopped, before any handler ran: {chain:?}");
    let why = stopped[0].4.clone().unwrap();
    assert!(why.contains("8 steps from the change that started its chain"), "{why}");
    assert!(why.contains("Alpha.created → Beta.created → Alpha.created"), "the diagnosis shows the chain: {why}");
    let dead = k.dead_events(&admin, 10).await.unwrap();
    assert!(dead.iter().any(|d| d["last_error"].as_str().is_some_and(|e| e.contains("Something answers its own changes"))), "{dead:?}");

    // Replayed by a person, it starts again from depth 0, and is stopped again only when it is deep again.
    let replayed = stopped[0].0;
    k.replay_event(&admin, replayed).await.unwrap();
    drain(&k).await;
    let again = rows(&k, tenant, "%").await.into_iter().find(|r| r.0 == replayed).unwrap();
    assert!(!again.3 && again.2 == 0, "the replayed event ran, from the start of a chain: {again:?}");

    // One change followed by too many events, however shallow: stopped at the chain's budget.
    k.insert(&admin, "Gamma", obj(json!({"n": 0}))).await.unwrap();
    drain(&k).await;
    let fan = rows(&k, tenant, "Gamma.%").await;
    assert!(fan.iter().all(|r| r.2 <= 4), "no deeper than the budget lets it get: {:?}", fan.iter().map(|r| r.2).max());
    assert!(fan.len() <= 1 + 3 + 9 + 27 + 54, "bounded: {}", fan.len());
    let over: Vec<_> = fan.iter().filter(|r| r.3).collect();
    assert!(!over.is_empty() && over.iter().all(|r| r.4.as_deref().unwrap().contains("events have followed from one change (at most 30)")), "{over:?}");

    // Work an event handler queues is as far down the chain as the handler.
    k.insert(&admin, "Delta", obj(json!({"n": 0}))).await.unwrap();
    drain(&k).await;
    k.run_jobs_once(1).await.unwrap();
    drain(&k).await;
    let echoed = rows(&k, tenant, "Epsilon.%").await;
    assert_eq!(echoed.iter().map(|r| r.2).collect::<Vec<_>>(), vec![1], "a job keeps its chain's depth: {echoed:?}");

    // An automation answering its own changes stops sooner, and says so.
    automation::save(
        &k,
        &admin,
        &obj(json!({"name": "zeta again", "entity": "Zeta", "event": "created",
        "actions": [{"type": "create_document", "entity": "Zeta", "values": {"n": "1"}}]})),
    )
    .await
    .unwrap();
    k.insert(&admin, "Zeta", obj(json!({"n": 0}))).await.unwrap();
    drain(&k).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let why: Vec<String> = sqlx::query_scalar("select error from automation_run where status = 'failed'").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(why.iter().any(|w| w.contains("automations made this change after 3 others")), "{why:?}");
}
