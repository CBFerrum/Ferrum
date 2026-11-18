//! Live updates (server/src/live.rs) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first). Who is told that a record changed: whoever may open it, and nobody else (an own-record rule, a rule whose
//! owner path goes past a link, no rule at all, a deleted record). What it costs: one read of the record per
//! announcement, whatever the number of sockets with partial access. And a slow read never stops the listener.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::live::LiveHub;
use serde_json::{Value, json};
use serial_test::serial;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::{OnceCell, mpsc};
use uuid::Uuid;

// ───────────── counting statements: every query sqlx runs is an event of target `sqlx::query` ─────────────

static COUNTING: AtomicBool = AtomicBool::new(false);
static STATEMENTS: AtomicUsize = AtomicUsize::new(0);

struct Statements;

impl tracing::Subscriber for Statements {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == "sqlx::query"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        if COUNTING.load(Ordering::SeqCst) {
            STATEMENTS.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The statements `f` runs.
async fn counted(f: impl std::future::Future<Output = ()>) -> usize {
    STATEMENTS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    f.await;
    COUNTING.store(false, Ordering::SeqCst);
    STATEMENTS.load(Ordering::SeqCst)
}

// ───────────── one kernel for the suite, on one runtime ─────────────

const ENTITIES: &str = r#"
- name: Account
  label: Account
  kind: master
  title_field: name
  fields:
    - {name: name, type: data}
    - {name: manager_email, type: email}
- name: Deal
  label: Deal
  kind: master
  title_field: title
  fields:
    - {name: title, type: data}
    - {name: account, type: link, to: Account}
"#;

const PACKAGE: &str = r#"
package: work
version: 0.1.0
label: Work
depends: [core]
roles:
  - name: Own Deals
    permissions:
      - {entity: Deal, read: true, create: true, if_owner: true}
  - name: Account Managers
    permissions:
      - {entity: Deal, read: true, via: account.manager_email}
  - name: Account Readers
    permissions:
      - {entity: Account, read: true}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-live-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("work/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("work/entities/work.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().expect("a runtime"));

fn run(scenario: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Err(e) = RUNTIME.block_on(RUNTIME.spawn(scenario)) {
        std::panic::resume_unwind(e.into_panic());
    }
}

static KERNEL: OnceCell<Arc<Kernel>> = OnceCell::const_new();

async fn kernel() -> Arc<Kernel> {
    KERNEL
        .get_or_init(|| async {
            let _ = tracing::subscriber::set_global_default(Statements);
            let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
            let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
            sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
                .execute(&db.pool)
                .await
                .expect("reset");
            Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
        })
        .await
        .clone()
}

/// A workspace: its administrator; Olga, who may open the deals she made; Max, who may open the deals of the accounts
/// he manages (a rule whose owner path goes past a link); Nina, who may open no deal; a deal of Olga's and a deal of an
/// account Max manages.
struct World {
    k: Arc<Kernel>,
    admin: Ctx,
    olga: Ctx,
    max: Ctx,
    nina: Ctx,
    olgas: Uuid,
    maxs: Uuid,
}

async fn person(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str]) -> Ctx {
    let user = k.create_user(admin, email, email, None, &[], false).await.unwrap();
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    k.set_user_roles_until(admin, user, &roles, &Default::default()).await.expect("roles given");
    k.load_ctx(admin.tenant, user, None, "it".into()).await.unwrap()
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

async fn world(slug: &str) -> World {
    let k = kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["work".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let olga = person(&k, &admin, &format!("olga@{slug}.test"), &["Own Deals"]).await;
    let max = person(&k, &admin, &format!("max@{slug}.test"), &["Account Managers"]).await;
    let nina = person(&k, &admin, &format!("nina@{slug}.test"), &["Account Readers"]).await;
    let olgas = id_of(&k.insert(&olga, "Deal", obj(json!({"title": "Olga's"}))).await.expect("Olga makes a deal"));
    let account = id_of(&k.insert(&admin, "Account", obj(json!({"name": "Acme", "manager_email": format!("max@{slug}.test")}))).await.unwrap());
    let maxs = id_of(&k.insert(&admin, "Deal", obj(json!({"title": "Max's", "account": account}))).await.unwrap());
    World { k, admin, olga, max, nina, olgas, maxs }
}

/// A socket of `ctx` subscribed to deals (`None` when it may not subscribe).
fn socket(hub: &LiveHub, ctx: &Ctx) -> (mpsc::Receiver<String>, bool) {
    let id = Uuid::now_v7();
    let rx = hub.register(id, ctx.clone());
    let (granted, _) = hub.subscribe(id, &["Deal".to_string()], &[]);
    (rx, !granted.is_empty())
}

fn announcement(w: &World, record: Uuid, event: &str) -> String {
    json!({"tenant": w.admin.tenant, "entity": "Deal", "id": record, "event": event, "actor": null}).to_string()
}

/// The records the socket was told about since last asked.
fn told(rx: &mut mpsc::Receiver<String>) -> Vec<Uuid> {
    std::iter::from_fn(|| rx.try_recv().ok()).map(|m| serde_json::from_str::<Value>(&m).unwrap()["id"].as_str().unwrap().parse().unwrap()).collect()
}

#[test]
#[serial]
fn own_record_filtering() {
    run(async {
        let w = world("live-own").await;
        let hub = LiveHub::new();
        let (mut admin, _) = socket(&hub, &w.admin);
        let (mut olga, olga_in) = socket(&hub, &w.olga);
        let (mut max, max_in) = socket(&hub, &w.max);
        let (mut nina, nina_in) = socket(&hub, &w.nina);
        assert!(olga_in && max_in && !nina_in, "Olga and Max may subscribe to deals, Nina may not");

        // Two people with own-record access, whose scopes differ only in who owns: each is told of their own deal
        // only, whichever socket is looked at first.
        let otto = person(&w.k, &w.admin, "otto@live-own.test", &["Own Deals"]).await;
        let ottos = id_of(&w.k.insert(&otto, "Deal", obj(json!({"title": "Otto's"}))).await.expect("Otto makes a deal"));
        let (mut otto_rx, _) = socket(&hub, &otto);
        for (record, maker) in [(w.olgas, "Olga"), (ottos, "Otto")] {
            hub.dispatch(&w.k, &announcement(&w, record, "updated")).await;
            let (to_olga, to_otto) = (told(&mut olga), told(&mut otto_rx));
            let mine = if maker == "Olga" { (vec![record], vec![]) } else { (vec![], vec![record]) };
            assert_eq!((to_olga, to_otto), mine, "only {maker}, who made the deal, is told");
            assert!(told(&mut max).is_empty() && told(&mut nina).is_empty());
            assert_eq!(told(&mut admin), vec![record]);
        }

        // Olga's deal: only she (and the administrator) are told.
        w.k.get(&w.olga, "Deal", w.olgas, false).await.expect("Olga opens her deal");
        assert!(w.k.get(&w.max, "Deal", w.olgas, false).await.is_err(), "Max cannot open Olga's deal");
        hub.dispatch(&w.k, &announcement(&w, w.olgas, "updated")).await;
        assert_eq!(told(&mut olga), vec![w.olgas], "Olga is told of her deal");
        assert_eq!(told(&mut admin), vec![w.olgas], "the administrator is told of every deal");
        assert!(told(&mut max).is_empty(), "Max is not told of a deal he cannot open");
        assert!(told(&mut nina).is_empty(), "Nina is not told of any deal");

        // The deal of Max's account: he is told (through the link), Olga is not.
        hub.dispatch(&w.k, &announcement(&w, w.maxs, "updated")).await;
        assert_eq!(told(&mut max), vec![w.maxs], "Max is told of his account's deal");
        assert!(told(&mut olga).is_empty(), "Olga is not told of a deal she did not make");
        assert!(told(&mut nina).is_empty());
        assert_eq!(told(&mut admin), vec![w.maxs]);

        // Deleted: no socket with partial access is told; the administrator is.
        for id in [w.olgas, w.maxs] {
            w.k.delete(&w.admin, "Deal", id, None).await.expect("deleted");
            hub.dispatch(&w.k, &announcement(&w, id, "deleted")).await;
        }
        assert!(told(&mut olga).is_empty() && told(&mut max).is_empty() && told(&mut nina).is_empty(), "a deleted deal reaches no scoped socket");
        assert_eq!(told(&mut admin), vec![w.olgas, w.maxs]);
    });
}

#[test]
#[serial]
fn scoped_sockets_one_read() {
    run(async {
        let w = world("live-many").await;
        // Olga's scope is tested in memory; Max's goes past a link, which is looked up.
        for (who, ctx, record) in [("Olga", &w.olga, w.olgas), ("Max", &w.max, w.maxs)] {
            let announce = announcement(&w, record, "updated");
            let mut reads = vec![];
            for n in [1usize, 500] {
                let hub = LiveHub::new();
                let mut sockets: Vec<_> = (0..n).map(|_| socket(&hub, ctx).0).collect();
                reads.push(counted(hub.dispatch(&w.k, &announce)).await);
                assert!(sockets.iter_mut().all(|rx| told(rx) == vec![record]), "every one of {who}'s {n} sockets is told");
            }
            println!("statements per announcement ({who}): {} with 1 scoped socket, {} with 500", reads[0], reads[1]);
            assert!(reads[0] > 0, "the statements are counted");
            assert_eq!(reads[0], reads[1], "one read of the record, whatever the number of {who}'s scoped sockets");
        }
    });
}

#[test]
#[serial]
fn listener_not_blocked() {
    run(async {
        let w = world("live-slow").await;
        let hub = LiveHub::new();
        let (_olga, _) = socket(&hub, &w.olga);
        let (stop, shutdown) = tokio::sync::watch::channel(false);
        let listener = hub.spawn_listener(w.k.clone(), shutdown);
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Reading a deal waits on a lock; announcements keep coming.
        let table = w.admin.schema.entity("Deal").unwrap().table.clone();
        let mut lock = w.k.db.pool.begin().await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("lock table \"{table}\" in access exclusive mode"))).execute(&mut *lock).await.unwrap();
        let before = ferrum_server::live::dropped();
        sqlx::query(
            "select pg_notify($1, json_build_object('tenant', $2::uuid, 'entity', 'Deal', 'id', $3::uuid, 'event', 'updated', 'n', i)::text) \
             from generate_series(1, 5000) i",
        )
        .bind(ferrum_kernel::notify::LIVE_CHANNEL)
        .bind(w.admin.tenant)
        .bind(w.olgas)
        .execute(&w.k.db.pool)
        .await
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while ferrum_server::live::dropped() == before && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let dropped = ferrum_server::live::dropped() - before;
        lock.rollback().await.unwrap();
        stop.send(true).unwrap();
        listener.await.unwrap();
        println!("announcements dropped while a read waited: {dropped}");
        assert!(dropped > 0, "the listener kept reading announcements while a read waited, and dropped those past its queue");
    });
}
