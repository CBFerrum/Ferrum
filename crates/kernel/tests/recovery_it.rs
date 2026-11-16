//! What happens when a database comes back from a backup, and when two regions each think they write,
//! against real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; these are reset first, and
//! `FERRUM_RESTORE_TEST_DATABASE_URL` and `FERRUM_WITNESS_TEST_DATABASE_URL` as well). The backup is a real
//! `pg_dump`, restored with a real `pg_restore`. Losing the witness is a real connection being cut. A planned move and the
//! restore marker run on a database with a vault (crates/kernel/src/vault): its secrets read on the copy as they did.

use async_trait::async_trait;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::recovery::{Authority, Restored};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, New, Opener, Rule, Scope, Vault, Zeroizing};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, EventSink, HookRegistry, Kernel, KernelSettings, Result, Tx};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use uuid::Uuid;

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

#[derive(Default)]
struct Sent {
    count: AtomicUsize,
}

#[async_trait]
impl EventSink for Sent {
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic == "pay" {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// An app with something of its own in hand.
#[derive(Default)]
struct App {
    told: AtomicUsize,
}

#[async_trait]
impl Restored for App {
    async fn set_aside(&self, _: &mut Tx, _: Option<Uuid>) -> Result<u64> {
        self.told.fetch_add(1, Ordering::SeqCst);
        Ok(0)
    }
}

/// Object storage, in memory.
#[derive(Default)]
struct MemFiles(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

#[async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

async fn database(url: &str, reset: bool) -> Db {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    if reset {
        // The vault's schema too: a restore brings its own, and would meet this one.
        sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
            .execute(&db.pool)
            .await
            .expect("reset");
    }
    db
}

async fn kernel(db: Db, sent: Arc<Sent>, app: Arc<App>) -> Arc<Kernel> {
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(sent);
    hooks.add_restored(app);
    Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot")
}

/// [`kernel`] with its vault opened as a server opens it, from `address`'s password (Argon2id kept cheap here), and an
/// app's handle on it.
async fn vaulted(db: Db, address: &str, sent: Arc<Sent>, app: Arc<App>) -> (Arc<Kernel>, Vault) {
    let opener = Opener {
        database_url: address.to_string(),
        file: PathBuf::from("/nonexistent/root.key"),
        transit: None,
        command: None,
        kdf: Kdf { m_kib: 64, t: 1, p: 1 },
    };
    vault::open(&db, opener, false).await.expect("the vault opens");
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(sent);
    hooks.add_restored(app);
    let mut crm = None;
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    (k, crm.unwrap())
}

/// A secret its owner may read, as it is kept.
fn shown(s: &str) -> (New, Rule) {
    (New::Value(Zeroizing::new(s.as_bytes().to_vec())), Rule { reveal: true, ..Default::default() })
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

async fn pay(k: &Kernel, tenant: Uuid) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, "pay", None, None, json!({"amount": 100})).await.unwrap();
    tx.commit().await.unwrap();
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

/// As in docs/operations.md: row-level security stops anyone but a superuser reading everything.
fn superuser(address: &str) -> String {
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    format!("{base}/{}", address.rsplit('/').next().unwrap())
}

/// A database's own address, as a move's, a sandbox's or an extraction's source must be: where the suites run through a
/// pooler (scripts/local_ci.sh managed), the direct host stands in.
fn direct(url: &str) -> String {
    match std::env::var("FERRUM_TEST_DATABASE_DIRECT_HOST") {
        Ok(host) => ferrum_kernel::db::with_host(url, &host).expect("a direct address"),
        Err(_) => url.to_string(),
    }
}

fn backup(address: &str) -> PathBuf {
    let dump = std::env::temp_dir().join(format!("ferrum-restore-{}.dump", Uuid::now_v7()));
    let done = Command::new("pg_dump").args(["-Fc", "-f", dump.to_str().unwrap(), &superuser(address)]).status().expect("pg_dump");
    assert!(done.success());
    dump
}

async fn restore(dump: &Path, address: &str) {
    let db = database(address, true).await;
    db.pool.close().await;
    // A schema of the extensions an administrator made there (as on a managed service, scripts/managed_pg.sh) is in
    // the dump too (the vault's is gone with the reset above): the database is emptied of it first, as an
    // administrator restoring a whole database would.
    let root = database(&superuser(address), false).await;
    sqlx::query("drop schema if exists extensions cascade").execute(&root.pool).await.unwrap();
    root.pool.close().await;
    let done = Command::new("pg_restore").args(["--exit-on-error", "-d", &superuser(address), dump.to_str().unwrap()]).status().expect("pg_restore");
    assert!(done.success());
    let _ = std::fs::remove_file(dump);
}

#[test]
fn restores_and_regions() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async {
        tokio::spawn(a_restored_database_does_not_act_on_its_past()).await.unwrap();
        tokio::spawn(one_workspace_goes_back_alone()).await.unwrap();
        tokio::spawn(only_one_region_writes()).await.unwrap();
        tokio::spawn(a_planned_move_carries_what_was_waiting()).await.unwrap();
        tokio::spawn(a_mark_out_of_time_or_unfenced_is_a_restore()).await.unwrap();
        tokio::spawn(the_fence_falls_between_transactions()).await.unwrap();
        tokio::spawn(what_a_move_can_break()).await.unwrap();
        tokio::spawn(the_marker_notices_a_restore_the_identity_misses()).await.unwrap();
        tokio::spawn(a_new_timeline_holds_as_the_setting_says_now()).await.unwrap();
    });
}

/// D7, failovers: a new timeline under a running server is noticed within a minute, and held or not as
/// `hold_on_timeline` says at that moment (it applies at once).
async fn a_new_timeline_holds_as_the_setting_says_now() {
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let k = kernel(database(&live, true).await, Arc::new(Sent::default()), Arc::new(App::default())).await;
    let recorded = async || -> i32 { sqlx::query_scalar("select timeline from platform_incarnation").fetch_one(&k.db.pool).await.unwrap() };
    let actual: i32 = sqlx::query_scalar("select timeline_id from pg_control_checkpoint()").fetch_one(&k.db.pool).await.unwrap();
    let failover = async || sqlx::query("update platform_incarnation set timeline = timeline + 7").execute(&k.db.pool).await.unwrap();
    // A failover to a synchronous standby: the operator said a new timeline loses nothing.
    k.set_live(KernelSettings { hold_on_timeline: false, ..k.settings.clone() });
    failover().await;
    assert_eq!(k.look_again().await.unwrap(), ferrum_kernel::recovery::MarkerSaid::NoStore);
    assert!(!k.restore_held().await.unwrap(), "not held");
    assert_eq!(recorded().await, actual, "the timeline as it is now, recorded");
    // Changed back, it applies at once: the next look holds.
    k.set_live(KernelSettings { hold_on_timeline: true, ..k.settings.clone() });
    failover().await;
    k.look_again().await.unwrap();
    assert!(k.restore_held().await.unwrap(), "held as after a restore");
    let because: String = sqlx::query_scalar("select held_because from platform_incarnation").fetch_one(&k.db.pool).await.unwrap();
    assert!(because.contains("new timeline"), "{because}");
}

/// Object storage that can be cut off, as an outage does.
#[derive(Default)]
struct Flaky {
    files: MemFiles,
    down: AtomicBool,
}

#[async_trait]
impl ferrum_kernel::files::FileStore for Flaky {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        if self.down.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("object storage does not answer".into()));
        }
        self.files.put(key, data).await
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        if self.down.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("object storage does not answer".into()));
        }
        self.files.get(key).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.files.delete(key).await
    }
}

async fn marker_in_database(k: &Kernel) -> Option<i64> {
    sqlx::query_scalar("select restore_marker from platform_incarnation").fetch_one(&k.db.pool).await.unwrap()
}

async fn marker_in_store(k: &Kernel, files: &Flaky) -> Option<i64> {
    let (system, oid): (i64, sqlx::postgres::types::Oid) =
        sqlx::query_as("select (select system_identifier from pg_control_system()), (select oid from pg_database where datname = current_database())")
            .fetch_one(&k.db.pool)
            .await
            .unwrap();
    let key = format!("{}{system}-{}.json", ferrum_kernel::recovery::MARKER_PREFIX, oid.0);
    let body = files.files.0.lock().unwrap().get(&key).cloned()?;
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["counter"].as_i64()
}

/// The marker is due again (it moves on at most every 30 seconds), as a minute passing would make it.
async fn due(k: &Kernel) {
    sqlx::query("update platform_incarnation set restore_marker_at = now() - interval '1 minute'").execute(&k.db.pool).await.unwrap();
}

/// D7: a snapshot restore, a clone or a branch keeps the database's identity, so only the marker in object storage tells
/// that it went back in time.
async fn the_marker_notices_a_restore_the_identity_misses() {
    use ferrum_kernel::recovery::MarkerSaid;
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let (k, crm) = vaulted(database(&live, true).await, &live, sent.clone(), app.clone()).await;
    let (value, rule) = shown("L-before-the-snapshot");
    crm.set(&k, Scope::Installation, "license", value, rule).await.unwrap();
    // No object store (development): nothing to compare with, nothing held, nothing waits.
    assert_eq!(k.watch_restore_marker().await.unwrap(), MarkerSaid::NoStore);
    assert!(!k.holding().await.unwrap());

    // A first install, on a store that holds this identity's marker already (a database dropped and made again in
    // place): born with the marker, it adopts the store's counter and moves on from there.
    let files = Arc::new(Flaky::default());
    k.set_file_store(files.clone());
    assert_eq!(marker_in_database(&k).await, Some(0), "born with it");
    let (system, oid): (i64, sqlx::postgres::types::Oid) =
        sqlx::query_as("select (select system_identifier from pg_control_system()), (select oid from pg_database where datname = current_database())")
            .fetch_one(&k.db.pool)
            .await
            .unwrap();
    let key = format!("{}{system}-{}.json", ferrum_kernel::recovery::MARKER_PREFIX, oid.0);
    files.files.0.lock().unwrap().insert(key, serde_json::to_vec(&json!({"counter": 50})).unwrap());
    assert_eq!(k.watch_restore_marker().await.unwrap(), MarkerSaid::Level(51));
    assert_eq!((marker_in_database(&k).await, marker_in_store(&k, &files).await), (Some(51), Some(51)), "the database first, then the store");
    assert_eq!(k.restore_marker().await.unwrap(), MarkerSaid::Level(51), "at most every 30 seconds");

    // Two replicas looking at once, again and again: never held, the counter only moves forward, and the store is never
    // ahead of the database.
    let other = kernel(database(&live, false).await, Arc::new(Sent::default()), Arc::new(App::default())).await;
    other.set_file_store(files.clone());
    let mut last = 51;
    for _ in 0..8 {
        due(&k).await;
        let (a, b) = tokio::join!(k.restore_marker(), other.restore_marker());
        for said in [a.unwrap(), b.unwrap()] {
            assert!(matches!(said, MarkerSaid::Level(_)), "{said:?}");
        }
        let (db, store) = (marker_in_database(&k).await.unwrap(), marker_in_store(&k, &files).await.unwrap());
        assert!(db > last && store <= db, "{last} then {db}, the store at {store}");
        last = db;
    }
    assert!(!k.restore_held().await.unwrap() && !k.holding().await.unwrap());

    // A payment is waiting when the snapshot is taken; the marker moves on; the database is taken back to the snapshot
    // with its identity and timeline unchanged, under a server that starts on it afresh.
    let admin = workspace(&k, "acme").await;
    drain(&k).await;
    pay(&k, admin.tenant).await;
    let snapshot = marker_in_database(&k).await.unwrap();
    due(&k).await;
    assert_eq!(k.restore_marker().await.unwrap(), MarkerSaid::Level(snapshot + 1));
    sqlx::query("update platform_incarnation set restore_marker = $1").bind(snapshot).execute(&k.db.pool).await.unwrap();
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let (restored, crm) = vaulted(database(&live, false).await, &live, sent.clone(), app.clone()).await;
    assert!(!restored.restore_held().await.unwrap(), "its identity says nothing");
    restored.set_file_store(files.clone());
    let said = restored.watch_restore_marker().await.unwrap();
    assert!(matches!(&said, MarkerSaid::Held(why) if why.contains(&format!("object storage's is {}", snapshot + 1))), "{said:?}");
    assert!(restored.restore_held().await.unwrap() && restored.holding().await.unwrap());
    assert_eq!(app.told.load(Ordering::SeqCst), 1, "what apps had in hand is set aside, as after any restore");
    drain(&restored).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 0, "the payment does not go out again by itself");
    let license = crm.reveal(&restored, Scope::Installation, "license").await.expect("held, its vault opens all the same");
    assert_eq!(&**license, b"L-before-the-snapshot");
    // Noticed once; a held database does not move the marker on, so a stale copy cannot push a live one's past it.
    due(&restored).await;
    assert_eq!(restored.restore_marker().await.unwrap(), MarkerSaid::Level(snapshot + 1));
    assert_eq!(app.told.load(Ordering::SeqCst), 1);
    assert_eq!(marker_in_store(&k, &files).await, Some(snapshot + 1));
    restored.release_hold(&admin, "checked the payments with the bank").await.unwrap();
    due(&restored).await;
    assert_eq!(restored.restore_marker().await.unwrap(), MarkerSaid::Level(snapshot + 2), "released, it moves on");

    // The store is briefly unreachable: a healthy installation is not held. A replica that starts meanwhile sends
    // nothing until a look reaches the store, and then carries on by itself.
    files.down.store(true, Ordering::SeqCst);
    let starting = kernel(database(&live, false).await, Arc::new(Sent::default()), Arc::new(App::default())).await;
    starting.set_file_store(files.clone());
    assert!(matches!(starting.watch_restore_marker().await.unwrap(), MarkerSaid::StoreDown(_)));
    assert!(starting.holding().await.unwrap() && !starting.restore_held().await.unwrap(), "it waits; nothing is held");
    assert!(!restored.holding().await.unwrap(), "a replica that compared already carries on");
    files.down.store(false, Ordering::SeqCst);
    assert!(matches!(starting.restore_marker().await.unwrap(), MarkerSaid::Level(_)));
    assert!(!starting.holding().await.unwrap());

    // The database swapped for an older one under a running server while the store is down: what the server saw itself
    // is enough.
    files.down.store(true, Ordering::SeqCst);
    let now = marker_in_database(&starting).await.unwrap();
    sqlx::query("update platform_incarnation set restore_marker = $1").bind(now - 3).execute(&k.db.pool).await.unwrap();
    let said = starting.restore_marker().await.unwrap();
    assert!(matches!(&said, MarkerSaid::Held(why) if why.contains(&format!("saw {now}"))), "{said:?}");
    files.down.store(false, Ordering::SeqCst);
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    starting.release_hold(&admin, "the swap was ours").await.unwrap();

    // A planned move fences the database: the marker does not move there, so the copy that carries on is level with the
    // store, not behind it.
    let before = marker_in_database(&k).await;
    let mark = k.plan_move("to another cluster", DAY, Duration::ZERO).await.expect("planned");
    due(&k).await;
    assert_eq!(k.restore_marker().await.unwrap(), MarkerSaid::Fenced);
    assert_eq!(marker_in_database(&k).await, before, "nothing moved");
    k.cancel_move("rehearsed only").await.unwrap();
    let _ = mark;
}

/// AT-DR-002.
async fn a_restored_database_does_not_act_on_its_past() {
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let restored = url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test");
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let k = kernel(database(&live, true).await, sent.clone(), app.clone()).await;
    let admin = workspace(&k, "acme").await;
    drain(&k).await;
    assert!(!k.holding().await.unwrap(), "a database starting out is not a restore");

    // A payment goes out, and another is waiting when the backup is taken.
    pay(&k, admin.tenant).await;
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 1);
    pay(&k, admin.tenant).await;
    let dump = backup(&live);
    // It goes out after the backup, as far as the world is concerned. The backup does not know.
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 2);

    // The database is lost, and comes back from that backup.
    restore(&dump, &restored).await;
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let k = kernel(database(&restored, false).await, sent.clone(), app.clone()).await;
    assert!(k.holding().await.unwrap(), "this is not the database this server knew");
    assert_eq!(app.told.load(Ordering::SeqCst), 1, "and what apps had in hand is theirs to set aside");

    // The payment that was waiting does not go out again by itself.
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 0);
    assert!(k.run_jobs_once(4).await.unwrap() == 0);
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    let waiting = k.dead_events(&admin, 10).await.unwrap();
    assert_eq!(waiting.len(), 1, "it is where someone can see it: {waiting:?}");
    assert!(waiting[0]["last_error"].as_str().unwrap().contains("restored"), "{waiting:?}");

    // Starting again changes nothing, and new work waits too.
    let k = kernel(database(&restored, false).await, sent.clone(), app.clone()).await;
    assert_eq!(app.told.load(Ordering::SeqCst), 1, "noticed once");
    assert!(k.holding().await.unwrap());
    pay(&k, admin.tenant).await;
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 0);

    // A person looks, and says so.
    let plain = k.create_user(&admin, "clerk@acme.test", "Clerk", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let plain = k.load_ctx(admin.tenant, plain, None, "it".into()).await.unwrap();
    assert!(matches!(k.release_hold(&plain, "looks fine").await, Err(Error::Forbidden(_))));
    assert!(k.release_hold(&admin, " ").await.is_err(), "it takes saying what was checked");
    k.release_hold(&admin, "reconciled against the payment provider").await.expect("released");
    assert!(!k.holding().await.unwrap());
    assert!(matches!(k.release_hold(&admin, "again").await, Err(Error::Conflict(_))));
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 1, "what came after goes out, and what was set aside stays set aside");
    assert_eq!(k.dead_events(&admin, 10).await.unwrap().len(), 1);
}

/// AT-DR-003.
async fn one_workspace_goes_back_alone() {
    const PASSWORD: &str = "Correct-Horse-9";
    let (live, copy) = (url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"));
    // The copy as an operator gives it: PostgreSQL's own address. Through a pooler it is refused, saying so.
    let source = direct(&copy);
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let k = kernel(database(&live, true).await, sent.clone(), app.clone()).await;
    k.set_file_store(Arc::new(MemFiles::default()));
    let (acme, bravo) = (workspace(&k, "acme").await, workspace(&k, "bravo").await);
    drain(&k).await;
    assert!(k.extract_tenant(&acme, &source, false, "not yet").await.is_err(), "there is nothing there");
    let file = |v: serde_json::Value| -> Uuid { v["id"].as_str().unwrap().parse().unwrap() };
    k.store_file(&acme, None, "kept.txt", "text/plain", b"still here").await.unwrap();
    let gone = file(k.store_file(&acme, None, "gone.txt", "text/plain", b"deleted later").await.unwrap());
    let stays = k.create_user(&acme, "stays@acme.test", "Stays", Some(PASSWORD), &[], false).await.unwrap();
    let leaves = k.create_user(&acme, "leaves@acme.test", "Leaves", Some(PASSWORD), &[], false).await.unwrap();
    k.create_user(&acme, "forget-me@acme.test", "Forget Me", Some(PASSWORD), &[], false).await.unwrap();
    let (_, key) = k.create_api_key(&acme, stays, "a key", None, None).await.unwrap();
    let signed_in = k.login("acme", "stays@acme.test", PASSWORD).await.unwrap();
    pay(&k, acme.tenant).await;
    let dump = backup(&live);
    restore(&dump, &copy).await;

    // Both workspaces carry on after the backup. Acme's payment goes out; Bravo's is still waiting.
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 1);
    k.create_user(&acme, "later@acme.test", "Later", Some(PASSWORD), &[], false).await.unwrap();
    k.create_user(&bravo, "later@bravo.test", "Later", Some(PASSWORD), &[], false).await.unwrap();
    k.set_password(&acme, stays, None, "Another-Horse-7").await.unwrap();
    k.set_user_active(&acme, leaves, false).await.unwrap();
    // Since the backup: a hold, a restriction and a withdrawn consent, which going back does not undo (FEO-078).
    k.place_legal_hold(&acme, Some("subject@acme.test"), None, "Matter 12").await.unwrap();
    k.restrict_processing(&acme, "subject@acme.test", "Disputes how their data is used").await.unwrap();
    k.record_consent(&acme, "subject@acme.test", ferrum_kernel::retention::Purpose::Marketing, false, "Unsubscribed").await.unwrap();
    // A file deleted since, bytes and all, which the copy still names.
    k.remove_file(&acme, gone).await.unwrap();
    // And someone asked to be forgotten: the copy has them as they were.
    let forgotten = k.erase_personal_data(&acme, "forget-me@acme.test", "Asked to be forgotten").await.expect("erased");
    assert_eq!(forgotten["account_erased"], json!(true), "{forgotten}");
    pay(&k, bravo.tenant).await;

    let emails = async |tenant: Uuid| -> Vec<String> {
        let mut tx = k.db.system_tx().await.unwrap();
        let found = sqlx::query_scalar("select email::text from app_user where tenant_id = $1 order by 1").bind(tenant).fetch_all(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        found
    };
    let (acme_now, bravo_now) = (emails(acme.tenant).await, emails(bravo.tenant).await);
    assert!(acme_now.contains(&"later@acme.test".to_string()));

    // It needs no grant (D7): the role holds none, as on a managed service, where only the administrator could give it.
    let grant = async |sql: &'static str| {
        let root = database(&superuser(&live), false).await;
        sqlx::query(sql).execute(&root.pool).await.unwrap();
        root.pool.close().await;
    };
    grant("do $$ begin execute format('revoke set on parameter session_replication_role from %I', (select datdba::regrole::text from pg_database where datname = current_database())); end $$").await;
    let granted: bool = sqlx::query_scalar("select has_parameter_privilege('session_replication_role', 'SET')").fetch_one(&k.db.pool).await.unwrap();
    assert!(!granted);

    // Comparing changes nothing, and it says what would go.
    // A pooler's address for the copy is refused, saying what to give instead (the rehearsal runs through one).
    if std::env::var("FERRUM_TEST_DATABASE_DIRECT_HOST").is_ok() {
        let r = k.extract_tenant(&acme, &copy, false, "through the pooler").await;
        assert!(matches!(&r, Err(Error::Invalid(m)) if m.contains("connection pooler")), "{r:?}");
    }
    let compared = k.extract_tenant(&acme, &source, false, "to see").await.expect("compared");
    let people = compared["tables"].as_array().unwrap().iter().find(|t| t["table"] == json!("app_user")).unwrap().clone();
    assert_eq!((people["now"].clone(), people["in_copy"].clone()), (json!(5), json!(4)), "{compared}");
    assert_eq!(compared["applied"], json!(false));
    // Acme's payment went out after the copy was taken: listed to reconcile, not undone (FEO-078).
    assert!(compared["went_out_since"].as_array().unwrap().contains(&json!({"topic": "pay", "count": 1})), "{compared}");
    assert_eq!(emails(acme.tenant).await, acme_now);

    // What it will not do.
    assert!(k.extract_tenant(&acme, &source, true, " ").await.is_err(), "it takes saying why");
    let clerk = k.load_ctx(acme.tenant, stays, None, "it".into()).await.unwrap();
    assert!(matches!(k.extract_tenant(&clerk, &source, true, "mine").await, Err(Error::Forbidden(_))));
    let r = k.extract_tenant(&acme, &direct(&live), true, "the wrong address").await;
    assert!(matches!(&r, Err(Error::Conflict(m)) if m.contains("this database")), "{r:?}");

    let done = k.extract_tenant(&acme, &source, true, "the customer asked").await.expect("extracted");
    assert_eq!((done["applied"].clone(), done["status"].clone()), (json!(true), json!("active")), "{done}");
    assert!(!emails(acme.tenant).await.contains(&"later@acme.test".to_string()), "acme is as it was");
    assert!(!emails(acme.tenant).await.contains(&"forget-me@acme.test".to_string()), "but what was erased since stays erased, without a task");
    assert_eq!((done["erasures"].clone(), done["erased_again"].clone()), (json!(1), json!(1)), "{done}");
    // The file that came back without its bytes is checked and listed; the one still stored is not (FEO-078).
    assert_eq!(done["files"], json!({"checked": 1, "unchecked": 0, "missing": [{"id": gone, "name": "gone.txt"}]}), "{done}");
    assert_eq!(done["went_out_since"], compared["went_out_since"], "said again as it went back, and kept in the audit trail");
    let mut tx = k.db.system_tx().await.unwrap();
    let kept: (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from legal_hold where tenant_id = $1 and released_at is null), \
                (select count(*) from processing_restriction where tenant_id = $1 and lifted_at is null), \
                (select count(*) from contact_consent where tenant_id = $1 and not granted)",
    )
    .bind(acme.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept, (1, 1, 1), "what the law holds and what people asked for stay as they are now");
    assert_eq!(emails(bravo.tenant).await, bravo_now, "and nobody else went anywhere");

    // Nobody gets in who could not before.
    assert!(k.login("acme", "stays@acme.test", PASSWORD).await.is_err(), "the password from before");
    k.login("acme", "stays@acme.test", "Another-Horse-7").await.expect("the password as it is now");
    assert!(k.login("acme", "leaves@acme.test", PASSWORD).await.is_err(), "someone who left has still left");
    assert!(k.verify_api_key(&key).await.is_err(), "every key has to be issued again");
    assert!(k.load_ctx(acme.tenant, stays, Some(signed_in.token_version), "it".into()).await.is_err(), "and every sign-in");

    // Nothing runs because its record came back, and what Bravo had waiting is not affected.
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 2, "Bravo's went out, and Acme's did not go out twice");
    assert_eq!(app.told.load(Ordering::SeqCst), 1);
    let admin = k.load_ctx(acme.tenant, acme.user, None, "it".into()).await.unwrap();
    let waiting = k.dead_events(&admin, 10).await.unwrap();
    assert_eq!(waiting.len(), 1, "{waiting:?}");

    // What happened since is still known, including this.
    let mut tx = k.db.system_tx().await.unwrap();
    let said: i64 = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and action = 'platform.tenant_restored'")
        .bind(acme.tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("select status from tenant where id = $1").bind(acme.tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((said, status.as_str()), (1, "active"));

    // A reference that would point at nothing stops it, and leaves everything as it was. A support record is
    // kept while what it refers to goes back, so this one is what such a thing looks like.
    let dangling = Uuid::now_v7();
    let root = database(&superuser(&live), false).await;
    let mut tx = root.pool.begin().await.unwrap();
    sqlx::query("set local session_replication_role = replica").execute(&mut *tx).await.unwrap();
    sqlx::query("insert into support_access (tenant_id, grant_id, method, path) values ($1, $2, 'GET', '/')")
        .bind(acme.tenant)
        .bind(dangling)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = emails(acme.tenant).await;
    let r = k.extract_tenant(&acme, &source, true, "again").await;
    assert!(matches!(&r, Err(Error::Conflict(m)) if m.contains("support_access_tenant_id_grant_id_fkey")), "{r:?}");
    assert_eq!(emails(acme.tenant).await, before);
    let mut tx = root.pool.begin().await.unwrap();
    sqlx::query("delete from support_access where grant_id = $1").bind(dangling).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    root.pool.close().await;

    // A copy that does not match what is running goes no further.
    let older = database(&superuser(&copy), false).await;
    sqlx::query("alter table app_user add column left_over text").execute(&older.pool).await.unwrap();
    older.pool.close().await;
    let r = k.extract_tenant(&acme, &source, true, "too late").await;
    assert!(matches!(&r, Err(Error::Conflict(m)) if m.contains("another version") && m.contains("app_user")), "{r:?}");
    grant("do $$ begin execute format('revoke set on parameter session_replication_role from %I', (select datdba::regrole::text from pg_database where datname = current_database())); end $$").await;
}

/// A connection to the witness that can be cut, the way a partition does.
struct Link {
    port: u16,
    cut: Arc<AtomicBool>,
    closing: watch::Sender<bool>,
}

impl Link {
    async fn to(upstream: (String, u16)) -> Link {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cut = Arc::new(AtomicBool::new(false));
        let (closing, closed) = watch::channel(false);
        let refusing = cut.clone();
        tokio::spawn(async move {
            while let Ok((mut near, _)) = listener.accept().await {
                if refusing.load(Ordering::SeqCst) {
                    continue;
                }
                let (upstream, mut closed) = (upstream.clone(), closed.clone());
                closed.borrow_and_update();
                tokio::spawn(async move {
                    let Ok(mut far) = TcpStream::connect(upstream).await else { return };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut near, &mut far) => {}
                        _ = closed.changed() => {}
                    }
                });
            }
        });
        Link { port, cut, closing }
    }

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        let _ = self.closing.send(true);
    }

    fn mend(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }
}

async fn wait_for(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..200 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("gave up waiting until {what}");
}

/// AT-MSG-009 and AT-DR-005: the old region is alive, cut off, and still believes.
async fn only_one_region_writes() {
    let witness = url("FERRUM_WITNESS_TEST_DATABASE_URL", "ferrum_witness_test");
    let options: sqlx::postgres::PgConnectOptions = witness.parse().unwrap();
    let w = sqlx::PgPool::connect_with(options.clone()).await.expect("the witness");
    sqlx::raw_sql("drop table if exists ferrum_authority").execute(&w).await.unwrap();
    let (host, port) = (options.get_host().to_string(), options.get_port());
    // The witness's own address, role and password, through the link.
    let through = |link: &Link| ferrum_kernel::db::with_host(&witness, &format!("127.0.0.1:{}", link.port)).unwrap();
    let ttl = Duration::from_millis(1600);

    let a = kernel(database(&url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), true).await, Arc::default(), Arc::default()).await;
    let b = kernel(database(&url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"), true).await, Arc::default(), Arc::default()).await;
    let (a_admin, b_admin) = (workspace(&a, "one").await, workspace(&b, "two").await);
    let write = async |k: &Kernel, ctx: &Ctx| -> std::result::Result<(), Error> {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        outbox::enqueue(&mut tx, ctx.tenant, "w", None, None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    };

    let (stop, stopping) = watch::channel(false);
    let a_link = Link::to((host.clone(), port)).await;
    let b_link = Link::to((host.clone(), port)).await;
    let authority = |link: &Link, region: &str| {
        let (url, region) = (through(link), region.to_string());
        async move { Authority::connect(&url, "ferrum", &region, ttl).await.unwrap() }
    };
    let a_loop = a.spawn_authority(authority(&a_link, "a").await, stopping.clone());
    {
        let a = a.clone();
        wait_for("the first region writes", move || !a.db.fenced()).await;
    }
    write(&a, &a_admin).await.expect("it holds it");
    let b_loop = b.spawn_authority(authority(&b_link, "b").await, stopping.clone());
    tokio::time::sleep(ttl).await;
    assert!(b.db.fenced(), "someone holds it, so the other region waits");
    assert!(matches!(write(&b, &b_admin).await, Err(Error::Unavailable(_))), "and cannot write, whatever it tries");
    assert!(!a.db.fenced(), "and renewing keeps it");

    // The first region is cut off from the witness, and carries on believing for as long as it may.
    a_link.cut();
    {
        let a = a.clone();
        wait_for("the old region stops itself", move || a.db.fenced()).await;
    }
    let lost_at: bool = sqlx::query_scalar("select expires_at > now() from ferrum_authority").fetch_one(&w).await.unwrap();
    assert!(lost_at, "before its time at the witness ran out, so nobody else could yet have been given it");
    assert!(matches!(write(&a, &a_admin).await, Err(Error::Unavailable(_))));
    {
        let b = b.clone();
        wait_for("the other region takes over", move || !b.db.fenced()).await;
    }
    write(&b, &b_admin).await.expect("one writer again");
    let epoch: i64 = sqlx::query_scalar("select epoch from ferrum_authority").fetch_one(&w).await.unwrap();
    assert_eq!(epoch, 2);

    // The partition heals. The old region does not get to carry on, even once the new one goes away.
    a_link.mend();
    tokio::time::sleep(ttl).await;
    assert!(a.db.fenced(), "someone else holds it");
    b_link.cut();
    {
        let b = b.clone();
        wait_for("the second region stops", move || b.db.fenced()).await;
    }
    tokio::time::sleep(ttl * 2).await;
    assert!(a.db.fenced(), "it could take it back, and its data is behind, so it does not");
    assert!(a.accept_authority(&a_admin, " ").await.is_err());
    a.accept_authority(&a_admin, "rebuilt from the other region's backup").await.expect("a person says so");
    {
        let a = a.clone();
        wait_for("the rebuilt region writes", move || !a.db.fenced()).await;
    }
    write(&a, &a_admin).await.expect("back");
    stop.send(true).unwrap();
    a_loop.await.unwrap();
    b_loop.await.unwrap();
}

const DAY: Duration = Duration::from_secs(24 * 3600);

/// The holders of the planned move's floor (crates/kernel/src/window.rs).
async fn fence_floor(k: &Kernel) -> Vec<String> {
    let (floors, _) = k.floors().await.unwrap();
    floors.into_iter().filter(|f| f.feature == ferrum_kernel::moves::FLOOR.feature).map(|f| f.holder).collect()
}

async fn adopted(k: &Kernel) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n = sqlx::query_scalar("select count(*) from audit_event where action = 'platform.move_adopted'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn try_pay(k: &Kernel, tenant: Uuid) -> Result<()> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    outbox::enqueue(&mut tx, tenant, "pay", None, None, json!({"amount": 100})).await?;
    tx.commit().await?;
    Ok(())
}

/// A planned move: the copy carries on with what was waiting, once; the source takes nothing more.
async fn a_planned_move_carries_what_was_waiting() {
    let (live, copy) = (url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"));
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let (k, crm) = vaulted(database(&live, true).await, &live, sent.clone(), app.clone()).await;
    let admin = workspace(&k, "acme").await;
    for (scope, name, kept) in [(Scope::Workspace(admin.tenant), "smtp.password", "p4ss-moving"), (Scope::Installation, "license", "L-moving")] {
        let (value, rule) = shown(kept);
        crm.set(&k, scope, name, value, rule).await.unwrap();
    }
    drain(&k).await;
    pay(&k, admin.tenant).await;
    assert!(k.plan_move(" ", DAY, Duration::ZERO).await.is_err(), "it takes a reason");
    assert!(k.plan_move("to Kubernetes", Duration::from_secs(60), Duration::ZERO).await.is_err(), "at least an hour");
    assert!(matches!(k.cancel_move("nothing").await, Err(Error::Conflict(_))), "nothing to cancel");
    // Work a newer build wrote waits: an upgrade or a rollback is going on, and an older build ignores the fence.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set build_date = now() + interval '10 years' where topic = 'pay'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let r = k.plan_move("to Kubernetes", DAY, Duration::ZERO).await;
    assert!(matches!(&r, Err(Error::Conflict(m)) if m.contains("newer build")), "{r:?}");
    assert!(k.planned_move().await.unwrap().is_none(), "and nothing was marked");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set build_date = null where topic = 'pay'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(fence_floor(&k).await.is_empty(), "a refused plan holds no floor");
    let mark = k.plan_move("to Kubernetes", DAY, Duration::ZERO).await.expect("planned");
    assert!(mark.fenced_at.is_some());
    // A build that would ignore the fence may no longer start here.
    assert_eq!(fence_floor(&k).await, vec![mark.id.to_string()]);
    assert!(matches!(k.plan_move("again", DAY, Duration::ZERO).await, Err(Error::Conflict(_))), "planned once");

    // The source takes no changes, and sends nothing.
    assert!(matches!(try_pay(&k, admin.tenant).await, Err(Error::Unavailable(_))), "fenced");
    assert!(k.holding().await.unwrap());
    assert!(!k.restore_held().await.unwrap(), "held for the move, not as after a restore");
    assert!(k.move_standing().await.unwrap().is_some_and(|(m, source)| source && m.fenced_at.is_some()), "the source of the move");
    drain(&k).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 0);
    restore(&backup(&live), &copy).await;

    // Two replicas of the copy start at once: the move is adopted once, nothing is set aside, and what was waiting
    // goes out once, from the copy.
    let (moved, told) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let (one, two) = tokio::join!(async { kernel(database(&copy, false).await, moved.clone(), told.clone()).await }, async {
        kernel(database(&copy, false).await, moved.clone(), told.clone()).await
    });
    assert!(!one.holding().await.unwrap() && !two.holding().await.unwrap(), "no hold");
    assert_eq!(told.told.load(Ordering::SeqCst), 0, "nothing set aside");
    assert_eq!(adopted(&one).await, 1, "adopted once");
    assert!(one.planned_move().await.unwrap().is_none(), "the mark is gone from the copy");
    assert!(fence_floor(&one).await.is_empty(), "and so is its floor");
    drain(&one).await;
    drain(&two).await;
    assert_eq!(moved.count.load(Ordering::SeqCst), 1, "what was waiting went out once");
    try_pay(&two, admin.tenant).await.expect("the copy takes changes");
    drain(&two).await;
    assert_eq!(moved.count.load(Ordering::SeqCst), 2);
    let (again, crm) = vaulted(database(&copy, false).await, &copy, moved.clone(), told.clone()).await;
    assert!(!again.holding().await.unwrap() && adopted(&again).await == 1, "starting again changes nothing");
    // The vault came with it, and opens on the copy with the role's password: every secret reads as it did.
    for (scope, name, kept) in [(Scope::Workspace(admin.tenant), "smtp.password", "p4ss-moving"), (Scope::Installation, "license", "L-moving")] {
        assert_eq!(&**crm.reveal(&again, scope, name).await.unwrap(), kept.as_bytes(), "{name}");
    }

    // The source stays fenced when it starts again (it starts, then serves read-only).
    let db = database(&live, false).await;
    db.starting(true);
    let source = kernel(db, sent.clone(), app.clone()).await;
    source.db.starting(false);
    assert!(matches!(try_pay(&source, admin.tenant).await, Err(Error::Unavailable(_))), "still fenced");
    assert!(source.holding().await.unwrap());
    assert!(matches!(source.adopt_move("it is the copy").await, Err(Error::Conflict(_))), "the source is not a copy");
    assert_eq!(app.told.load(Ordering::SeqCst), 0);

    // Cancelled: the source takes changes again, and sends what it held.
    assert_eq!(fence_floor(&source).await, vec![mark.id.to_string()], "the source keeps the floor while fenced");
    source.cancel_move("staying on Compose").await.expect("cancelled");
    assert!(fence_floor(&source).await.is_empty(), "cancelled, the floor goes");
    assert!(!source.holding().await.unwrap());
    try_pay(&source, admin.tenant).await.expect("takes changes");
    drain(&source).await;
    assert_eq!(sent.count.load(Ordering::SeqCst), 2);
}

/// A copy with a mark it may not follow is a restore, held as before: an old backup holding a mark whose time ran out,
/// and a copy taken before the source stopped taking changes.
async fn a_mark_out_of_time_or_unfenced_is_a_restore() {
    let (live, copy) = (url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"));
    let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
    let k = kernel(database(&live, true).await, sent.clone(), app.clone()).await;
    let admin = workspace(&k, "acme").await;
    drain(&k).await;
    pay(&k, admin.tenant).await;
    k.plan_move("to another cluster", DAY, Duration::ZERO).await.expect("planned");
    for (change, why) in [
        ("update platform_move set valid_until = now() - interval '1 hour'", "its time ran out"),
        ("update platform_move set valid_until = now() + interval '1 day', fenced_at = null", "copied before the fence"),
    ] {
        sqlx::query(change).execute(&k.db.pool).await.unwrap();
        restore(&backup(&live), &copy).await;
        let (sent, app) = (Arc::new(Sent::default()), Arc::new(App::default()));
        let c = kernel(database(&copy, false).await, sent.clone(), app.clone()).await;
        assert!(c.holding().await.unwrap(), "{why}: held");
        assert_eq!(app.told.load(Ordering::SeqCst), 1, "{why}: set aside");
        assert!(c.planned_move().await.unwrap().is_none(), "{why}: the mark is gone");
        assert!(fence_floor(&c).await.is_empty(), "{why}: and its floor");
        try_pay(&c, admin.tenant).await.expect("a held copy takes changes, and sends nothing");
        drain(&c).await;
        assert_eq!(sent.count.load(Ordering::SeqCst), 0, "{why}");
    }
}

/// The fence falls between transactions: one in flight finishes, one that starts meanwhile waits and is fenced.
async fn the_fence_falls_between_transactions() {
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let k = kernel(database(&live, true).await, Arc::default(), Arc::default()).await;
    let admin = workspace(&k, "acme").await;
    let mut open = k.db.tenant_tx(admin.tenant).await.unwrap();
    let planning = tokio::spawn({
        let k = k.clone();
        async move { k.plan_move("to Kubernetes", DAY, Duration::ZERO).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!planning.is_finished(), "it waits for the transaction in flight");
    let late = tokio::spawn({
        let k = k.clone();
        async move { try_pay(&k, admin.tenant).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    outbox::enqueue(&mut open, admin.tenant, "pay", None, None, json!({"amount": 1})).await.expect("begun before the fence");
    open.commit().await.expect("and finishes");
    planning.await.unwrap().expect("planned");
    assert!(matches!(late.await.unwrap(), Err(Error::Unavailable(_))), "begun while it waited: fenced");
    k.cancel_move("rehearsed").await.unwrap();
    try_pay(&k, admin.tenant).await.expect("lifted");
}

/// What a move without the source's keys or files breaks, said by count and id, never by value.
async fn what_a_move_can_break() {
    use ferrum_kernel::recovery::Files;
    use ferrum_kernel::secret::SecretKey;
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let (old, new) = (SecretKey::new([1; 32]), SecretKey::new([2; 32]));
    let boot = |db: Db, key: SecretKey, retired: Vec<SecretKey>| async move {
        let settings = KernelSettings { old_key: ferrum_kernel::OldKey::configured(Some(key), retired), ..Default::default() };
        Kernel::boot(db, catalog(), HookRegistry::default(), settings, None).await.expect("boot")
    };
    let k = boot(database(&live, true).await, old.clone(), vec![]).await;
    let files = Arc::new(MemFiles::default());
    k.set_file_store(files.clone());
    let admin = workspace(&k, "acme").await;
    let hook = Uuid::now_v7();
    let plain = "the-signing-secret-itself";
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics) values ($1, $2, 'h', 'https://example.test', $3, '{}')")
        .bind(admin.tenant)
        .bind(hook)
        .bind(k.seal_webhook_secret(admin.tenant, hook, plain).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    let record = Uuid::now_v7();
    sqlx::query("insert into record_secret (tenant_id, entity, record_id, field, sealed, key_id) values ($1, 'Gateway', $2, 'api_key', 'AAAA', $3)")
        .bind(admin.tenant)
        .bind(record)
        .bind(&old.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let kept = k.store_file(&admin, None, "kept.txt", "text/plain", b"here").await.unwrap();
    let lost = k.store_file(&admin, None, "lost.txt", "text/plain", b"not copied").await.unwrap();

    // All fine.
    assert_eq!(k.unopened_secrets().await.unwrap(), (vec![], true));
    let mut missing = Vec::new();
    assert_eq!(k.check_files(None, Files::All, |f| missing.push(f)).await.unwrap(), 2);
    assert!(missing.is_empty());

    // Moved with another key, and without one file: each is said, by where and which, and the value never.
    let k = boot(database(&live, false).await, new.clone(), vec![]).await;
    k.set_file_store(files.clone());
    let key: String = {
        let mut tx = k.db.system_tx().await.unwrap();
        let key = sqlx::query_scalar("select storage_key from file_object where id = $1")
            .bind(lost["id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        key
    };
    files.0.lock().unwrap().remove(&key);
    let (unopened, complete) = k.unopened_secrets().await.unwrap();
    assert!(complete);
    let said = format!("{unopened:?}");
    assert!(!said.contains(plain), "{said}");
    let by: std::collections::BTreeMap<_, _> = unopened.iter().map(|u| (u.what.as_str(), (u.count, u.examples.clone()))).collect();
    assert_eq!(by.get("Gateway.api_key"), Some(&(1, vec![record.to_string()])), "{said}");
    assert_eq!(by.get("webhook.secret"), Some(&(1, vec![hook.to_string()])), "{said}");
    let mut missing = Vec::new();
    assert_eq!(k.check_files(Some(admin.tenant), Files::All, |f| missing.push(f)).await.unwrap(), 2);
    assert_eq!(missing.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [lost["id"].as_str().unwrap()]);
    let mut newest = Vec::new();
    assert_eq!(k.check_files(None, Files::Newest(1), |f| newest.push(f)).await.unwrap(), 1, "only as many as asked");
    assert_eq!(newest.len(), 1, "the newest is the lost one");
    assert!(kept["id"].is_string());

    // With the old key retired beside the new one, everything opens again.
    let k = boot(database(&live, false).await, new, vec![old]).await;
    assert_eq!(k.unopened_secrets().await.unwrap(), (vec![], true));
}
