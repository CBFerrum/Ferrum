//! One workspace moving from one installation to another and back, against two real PostgreSQL databases
//! (`FERRUM_TEST_DATABASE_URL` and `FERRUM_RESTORE_TEST_DATABASE_URL`, both reset first), each with its own vault and its
//! own files in a directory of its own: the workspace's vault goes with it through a transfer made at the source. A crash
//! between two steps is a run told to stop there.

use async_trait::async_trait;
use ferrum_kernel::auth::SignIn;
use ferrum_kernel::jobs::{Job, JobHandler, NewJob};
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::shell::HostRoute;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::workspace_move::{MoveSource, Run, Stop};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, EventSink, HookRegistry, Kernel, KernelSettings, Result, audit};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

/// Which workspace each payment and each job went out for, on either installation.
#[derive(Default)]
struct Seen(Mutex<Vec<(&'static str, Uuid)>>);

impl Seen {
    fn count(&self, what: &str, tenant: Uuid) -> usize {
        self.0.lock().unwrap().iter().filter(|(w, t)| *w == what && *t == tenant).count()
    }
}

struct Sink(Arc<Seen>);

#[async_trait]
impl EventSink for Sink {
    async fn on_event(&self, _: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic == "pay" {
            self.0.0.lock().unwrap().push(("pay", ev.tenant_id));
        }
        Ok(())
    }
}

struct Counted(Arc<Seen>);

#[async_trait]
impl JobHandler for Counted {
    fn kind(&self) -> &'static str {
        "test.counted"
    }
    fn title(&self) -> &'static str {
        "Counted"
    }
    async fn run(&self, _: &Kernel, job: &Job) -> Result<Value> {
        self.0.0.lock().unwrap().push(("job", job.tenant_id));
        Ok(json!({}))
    }
}

/// Files in a directory, as a single host keeps them.
struct DirFiles(PathBuf);

#[async_trait]
impl ferrum_kernel::files::FileStore for DirFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.0.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).map_err(|e| Error::Unavailable(e.to_string()))
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(|_| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        let _ = std::fs::remove_file(self.0.join(key));
        Ok(())
    }
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

/// The vault of the database at `url`, opened as a server opens it.
async fn open_vault(db: &Db, url: &str) {
    let opener = ferrum_kernel::vault::Opener {
        database_url: url.to_string(),
        file: "/nonexistent/root.key".into(),
        kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
        ..Default::default()
    };
    ferrum_kernel::vault::open(db, opener, false).await.expect("the vault");
}

/// The key of a transfer of workspace `tenant`'s vault, made at `k` (the source of a move).
async fn transfer(k: &Kernel, tenant: Uuid) -> SecretKey {
    SecretKey::from_base64(&ferrum_kernel::vault::transfer_out(k, tenant).await.unwrap()).unwrap()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn installation(url: &str, seen: &Arc<Seen>, files: &Path) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    open_vault(&db, url).await;
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(Arc::new(Sink(seen.clone())));
    hooks.add_job_handler(Arc::new(Counted(seen.clone())));
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(DirFiles(files.to_path_buf())));
    k.ensure_directory().await.unwrap();
    k
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["contacts".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

async fn pay(k: &Kernel, tenant: Uuid, topic: &str) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, topic, None, None, json!({"amount": 100})).await.unwrap();
    tx.commit().await.unwrap();
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

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

async fn as_superuser(address: &str, sql: &str) {
    let db = Db::connect(DbConfig { url: superuser(address), max_connections: 1, ..Default::default() }).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).execute(&db.pool).await.unwrap();
    db.pool.close().await;
}

async fn sql<T: Send + Unpin + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>>(k: &Kernel, q: &str, tenant: Uuid) -> Vec<T> {
    let mut tx = k.db.system_tx().await.unwrap();
    let rows = sqlx::query_as(sqlx::AssertSqlSafe(q.to_string())).bind(tenant).fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rows
}

/// Every row a workspace has, table by table, as a digest: what "touches none of their rows" compares.
async fn rows_of(k: &Kernel, tenant: Uuid) -> BTreeMap<String, String> {
    let tables: Vec<(String,)> = sql(
        k,
        "select distinct c.table_name::text from information_schema.columns c join pg_class r on r.relname = c.table_name \
         and r.relnamespace = 'public'::regnamespace where c.table_schema = 'public' and c.column_name = 'tenant_id' and not r.relispartition \
         and $1::uuid is not null order by 1",
        tenant,
    )
    .await;
    let mut out = BTreeMap::new();
    for (t,) in tables {
        let q = format!("select coalesce(md5(string_agg(md5(to_jsonb(x)::text), '' order by md5(to_jsonb(x)::text))), '') from \"{t}\" x where tenant_id = $1");
        let (d,): (String,) = sql(k, &q, tenant).await.remove(0);
        out.insert(t, d);
    }
    let (w,): (String,) = sql(k, "select md5(to_jsonb(t)::text) from tenant t where id = $1", tenant).await.remove(0);
    out.insert("tenant".into(), w);
    out
}

/// Every record's audit chain, verified with the existing verifier: the first broken one, if any.
async fn chains(k: &Kernel, tenant: Uuid) -> Option<(String, Uuid)> {
    let records: Vec<(String, Uuid)> = sql(k, "select distinct entity, record_id from audit_event where tenant_id = $1", tenant).await;
    assert!(!records.is_empty());
    let mut tx = k.db.system_tx().await.unwrap();
    for (entity, record) in records {
        if audit::verify_record(&mut tx, tenant, &entity, record).await.unwrap().is_some() {
            return Some((entity, record));
        }
    }
    tx.commit().await.unwrap();
    None
}

/// Today's anchor, made again from the events it covered (up to `upto`), against the one kept.
async fn anchor_holds(k: &Kernel, tenant: Uuid, upto: i64) -> bool {
    let mut tx = k.db.system_tx().await.unwrap();
    let (root, prev): (Vec<u8>, Option<Vec<u8>>) = sqlx::query_as("select root_hash, prev_root from audit_anchor where tenant_id = $1 and day = current_date")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let hashes: Vec<Vec<u8>> =
        sqlx::query_scalar("select hash from audit_event where tenant_id = $1 and at >= current_date and at < current_date + 1 and id <= $2 order by id")
            .bind(tenant)
            .bind(upto)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    let mut h = Sha256::new();
    h.update(prev.as_deref().unwrap_or(&[]));
    for x in &hashes {
        h.update(x);
    }
    h.finalize().to_vec() == root
}

fn quiet(_: &str) {}

#[test]
fn a_workspace_moves() {
    // Its future is deep: polled on the test harness's own thread (2 MiB, as `#[tokio::test]` does) an unoptimized
    // build overflows that stack now and then. A runtime with the server's stack size (server/src/lib.rs) instead.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(a_workspace_moves_to_another_installation_and_back()).await.unwrap() });
}

async fn a_workspace_moves_to_another_installation_and_back() {
    let (a_url, b_url) = (url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"));
    let dirs = std::env::temp_dir().join(format!("ferrum-move-{}", Uuid::now_v7()));
    let (dir_a, dir_b) = (dirs.join("a"), dirs.join("b"));
    let seen = Arc::new(Seen::default());
    let a = installation(&a_url, &seen, &dir_a).await;
    let b = installation(&b_url, &seen, &dir_b).await;
    // Without the grant on session_replication_role (D7): moves and copies run as the server's own role.
    for u in [&a_url, &b_url] {
        as_superuser(u, "do $$ begin execute format('revoke set on parameter session_replication_role from %I', (select datdba::regrole::text from pg_database where datname = current_database())); end $$").await;
    }

    // B holds a workspace of its own, with events whose numbers the moving one's would take.
    let bravo = workspace(&b, "bravo").await;
    b.create_user(&bravo, "clerk@bravo.test", "Clerk", Some(PASSWORD), &[], false).await.unwrap();
    for _ in 0..5 {
        pay(&b, bravo.tenant, "noted").await;
    }
    drain(&b).await;

    // Acme, at A: records with lines, a file, secrets in its vault (a webhook's, a second factor), a legal hold, an erasure,
    // an API key, a custom host, a member linked to a Ferrum account, an event handled and one waiting, and a queued job.
    let acme = workspace(&a, "acme").await;
    let t = acme.tenant;
    let contact = a
        .insert(
            &acme,
            "Contact",
            json!({"first_name": "Ann", "emails": [{"email": "ann@buyer.test", "is_primary": true}], "phones": [{"phone": "+15550100"}]})
                .as_object()
                .cloned()
                .unwrap(),
        )
        .await
        .unwrap();
    let contact: Uuid = contact["id"].as_str().unwrap().parse().unwrap();
    let file = a.store_file(&acme, Some(("Contact", contact)), "notes.txt", "text/plain", b"kept with the workspace").await.unwrap();
    let (file_key,): (String,) = sql(&a, "select storage_key from file_object where tenant_id = $1", t).await.remove(0);
    assert!(file["id"].is_string());
    let hook = Uuid::now_v7();
    let mut tx = a.db.system_tx().await.unwrap();
    a.keep_webhook_secret(&mut tx, t, hook, "whsec").await.unwrap();
    sqlx::query("insert into webhook (tenant_id, id, name, url, topics) values ($1, $2, 'books', 'https://books.example/hook', array['never'])")
        .bind(t)
        .bind(hook)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) values ('erp.acme.example', $1, 'custom', 'shell', true, 'x', now())")
        .bind(t)
        .execute(&mut *tx)
        .await
        .unwrap();
    // A name it gave itself under A's base domain then (docs/operations.md, "Addresses of a workspace"), with what it shows.
    sqlx::query(
        "insert into tenant_host (host, tenant_id, kind, target, verify_token, verified_at, base_name, home, apps, label) \
         values ('sales--acme.us.example', $1, 'subdomain', 'shell', '', now(), 'sales', 'crm', array['crm'], 'Sales')",
    )
    .bind(t)
    .execute(&mut *tx)
    .await
    .unwrap();
    // Ann's Ferrum account lives in A's region, eu.
    let ann = Uuid::now_v7();
    sqlx::query("insert into app_user (tenant_id, id, email, full_name, email_verified_at) select id, $1, 'admin@acme.test', 'Ann', now() from tenant where slug = 'accounts'")
        .bind(ann)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("update app_user set identity_id = $2 where tenant_id = $1 and id = $3").bind(t).bind(ann).bind(acme.user).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    a.place_legal_hold(&acme, Some("subject@acme.test"), None, "Matter 12").await.unwrap();
    // Pat's second factor, kept in the vault.
    let pat = a.create_user(&acme, "pat@acme.test", "Pat", Some(PASSWORD), &[], false).await.unwrap();
    let pat_ctx = a.load_ctx(t, pat, None, "it".into()).await.unwrap();
    let totp = a.begin_two_factor(&pat_ctx).await.unwrap()["secret"].as_str().unwrap().to_string();
    a.enable_two_factor(&pat_ctx, &ferrum_kernel::twofactor::code(&totp, now()).unwrap()).await.unwrap();
    a.create_user(&acme, "forget-me@acme.test", "Forget Me", Some(PASSWORD), &[], false).await.unwrap();
    a.erase_personal_data(&acme, "forget-me@acme.test", "Asked to be forgotten").await.unwrap();
    let (_, api_key) = a.create_api_key(&acme, acme.user, "integration", None, None).await.unwrap();
    pay(&a, t, "noted").await;
    drain(&a).await;
    pay(&a, t, "pay").await;
    let mut tx = a.db.tenant_tx(t).await.unwrap();
    ferrum_kernel::jobs::enqueue(&mut tx, t, Some(acme.user), NewJob::new("test.counted", json!({}))).await.unwrap();
    tx.commit().await.unwrap();
    // What sign-in keeps of a person: devices they used, failures counted by the hour; a key pinned to networks.
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("insert into known_device (tenant_id, user_id, device) values ($1, $2, 'laptop')").bind(t).bind(acme.user).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into sign_in_failure (tenant_id, user_id, hour, n) values ($1, $2, date_trunc('hour', now()), 2)")
        .bind(t)
        .bind(acme.user)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("update api_key set networks = array['192.0.2.0/24'] where tenant_id = $1").bind(t).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // A browser signed in an hour ago, which keeps reading while the workspace is frozen.
    let signed = a.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let sid = a.open_session(&signed, Duration::from_secs(7200), "it", "127.0.0.1", None).await.unwrap();
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("update user_session set last_seen_at = now() - interval '1 hour' where id = $1").bind(sid).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // An event from a month B never made (history older than the installation it moves to): the copy makes its month.
    sqlx::query("select ferrum_ensure_month_partitions('audit_event', '2019-03-05 10:00+00', '2019-03-05 10:00+00')").execute(&a.db.pool).await.unwrap();
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("insert into audit_event (tenant_id, at, entity, record_id, action, hash, redacted) values ($1, '2019-03-05 10:00+00', 'Old', $1, 'kept', '\\x00', '[\"diff\"]')")
        .bind(t)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Today's anchor, over the history the move carries.
    audit::anchor_day(&a.db, chrono::Utc::now().date_naive()).await.unwrap();
    let (anchored,): (i64,) = sql(&a, "select max(id) from audit_event where tenant_id = $1", t).await.remove(0);
    let bravo_before = rows_of(&b, bravo.tenant).await;

    // The workspace's vault goes with it through a transfer made at the source, given as the source's keys.
    let keys_a = Mutex::new(vec![transfer(&a, t).await]);
    let from = |stop: Option<Stop>| MoveSource {
        url: direct(&a_url),
        keys: keys_a.lock().unwrap().clone(),
        files: Some(Arc::new(DirFiles(dir_a.clone())) as Arc<dyn ferrum_kernel::files::FileStore>),
        regions: Some(("eu".into(), "us".into())),
        base_domain: None,
        wait: Duration::ZERO,
        not_sealed: vec![],
        stop_after: stop,
    };
    let refused = |r: Result<Value>, says: &str| match r {
        Err(Error::Conflict(m)) | Err(Error::Unavailable(m)) | Err(Error::Invalid(m)) => assert!(m.contains(says), "{m}"),
        other => panic!("expected a refusal saying {says:?}, got {other:?}"),
    };

    // Another build on one side: refused before anything changes.
    let (build,): (String,) = sql(&a, "select value from cluster_state where key = 'schema_build_date' and $1::uuid is not null", t).await.remove(0);
    let set_build = async |v: &str| {
        let mut tx = a.db.system_tx().await.unwrap();
        sqlx::query("update cluster_state set value = $1 where key = 'schema_build_date'").bind(v).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    };
    set_build("2030-01-01T00:00:00Z").await;
    refused(b.move_workspace("acme", &from(None), Run::Start, &quiet).await, "one build on both sides");
    set_build(&build).await;
    refused(b.move_workspace("acme", &from(None), Run::Resume, &quiet).await, "nothing to resume");
    // A value sealed where the move does not know how (an app's own column, say) is found by its form, and refused.
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("update e_contact set notes = $3 where tenant_id = $1 and id = $2")
        .bind(t)
        .bind(contact)
        .bind(SecretKey::new([1; 32]).seal(&format!("{t}/notes/{contact}"), "kept by an app").unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    refused(b.move_workspace("acme", &from(None), Run::Start, &quiet).await, "e_contact.notes");
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("update e_contact set notes = 'a plain note' where tenant_id = $1").bind(t).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // Without the key of its vault's transfer nothing moves.
    let keyless = MoveSource { keys: vec![SecretKey::new([2; 32])], ..from(None) };
    refused(b.move_workspace("acme", &keyless, Run::Start, &quiet).await, "is the key of this workspace's transfer");

    // Stopped once frozen: nothing at A takes changes or runs, reading works; a second move and a second run are refused.
    b.move_workspace("acme", &from(Some(Stop::Frozen)), Run::Start, &quiet).await.expect("frozen");
    // The run has let go of its lock by the time it answers: the next run is never refused as "another run".
    let mut conn = a.db.direct.acquire().await.unwrap();
    let free: bool =
        sqlx::query_scalar("select pg_try_advisory_lock($1, hashtext($2))").bind(0x0FE2_2079_i32).bind(t.to_string()).fetch_one(&mut *conn).await.unwrap();
    assert!(free, "the run that ended still holds its lock");
    sqlx::query("select pg_advisory_unlock($1, hashtext($2))").bind(0x0FE2_2079_i32).bind(t.to_string()).execute(&mut *conn).await.unwrap();
    drop(conn);
    // A build that would ignore the freeze may start on neither side now.
    let r = a.create_user(&acme, "late@acme.test", "Late", Some(PASSWORD), &[], false).await;
    assert!(matches!(&r, Err(Error::Unavailable(m)) if m.contains("this workspace is moving")), "{r:?}");
    a.read_record(&acme, "Contact", contact, false).await.expect("reading works while frozen");
    a.check_session(t, acme.user, sid).await.expect("a signed-in browser's session is checked, not written, while frozen");
    for password in ["wrong", PASSWORD] {
        let r = a.sign_in("acme", "admin@acme.test", password).await;
        assert!(matches!(&r, Err(Error::Unavailable(m)) if m.contains("moving")), "{r:?}");
    }
    assert!(matches!(a.sign_in("acme", "nobody@acme.test", "wrong").await, Err(Error::Unavailable(_))), "the same answer for an address it does not have");
    let failed: Vec<(i32,)> = sql(&a, "select failed_logins from app_user where tenant_id = $1 and email = 'admin@acme.test'", t).await;
    assert_eq!(failed, vec![(0,)], "no failure was counted");
    drain(&a).await;
    assert_eq!(a.run_jobs_once(5).await.unwrap(), 0, "nothing runs for it");
    assert_eq!(seen.count("pay", t), 0, "nothing goes out for it");
    // Stopped with no run working on it: the doctor says so on the source; the status says where it stands.
    let stalled = a.stalled_moves(Duration::ZERO).await.unwrap();
    assert_eq!(stalled.iter().map(|m| (m.0.as_str(), m.1.as_str(), m.2.as_str(), m.4)).collect::<Vec<_>>(), vec![("acme", "source", "leaving", Some(false))]);
    assert_eq!(a.move_status("acme").await.unwrap()["state"], json!("leaving"));
    assert_eq!(b.move_status("acme").await.unwrap()["state"], Value::Null);
    refused(b.move_workspace("acme", &from(None), Run::Start, &quiet).await, "under way");
    // A session lock, so on PostgreSQL's own address: through a pooler in transaction mode it would outlive its client.
    let held = Db::connect(DbConfig { url: direct(&a_url), max_connections: 1, ..Default::default() }).await.unwrap();
    let mut lock = held.pool.acquire().await.unwrap();
    sqlx::query("select pg_advisory_lock($1, hashtext($2))").bind(0x0FE2_2079_i32).bind(t.to_string()).execute(&mut *lock).await.unwrap();
    refused(b.move_workspace("acme", &from(None), Run::Resume, &quiet).await, "another move_workspace run");
    drop(lock);
    held.pool.close().await;
    b.move_workspace("acme", &from(None), Run::Abort, &quiet).await.expect("aborted");
    let undone = a.create_user(&acme, "back@acme.test", "Back", Some(PASSWORD), &[], false).await.expect("A takes changes again");

    // Stopped once copied: the copy at B is frozen too; aborting deletes it.
    b.move_workspace("acme", &from(Some(Stop::Copied)), Run::Start, &quiet).await.expect("copied");
    let at_b = b.load_ctx(t, acme.user, None, "it".into()).await.unwrap();
    assert!(matches!(b.create_user(&at_b, "early@acme.test", "Early", Some(PASSWORD), &[], false).await, Err(Error::Unavailable(_))));
    assert_eq!(b.stalled_moves(Duration::ZERO).await.unwrap().iter().map(|m| (m.2.as_str(), m.4)).collect::<Vec<_>>(), vec![("arriving", None)]);
    // Signing in to a frozen workspace is refused before anything is looked up or written, with the moving answer.
    assert!(matches!(b.sign_in("acme", "admin@acme.test", "wrong").await, Err(Error::Unavailable(m)) if m.contains("moving")));
    b.move_workspace("acme", &from(None), Run::Abort, &quiet).await.expect("aborted");
    assert!(sql::<(i64,)>(&b, "select count(*) from app_user where tenant_id = $1", t).await[0].0 == 0, "the copy is gone");
    assert!(!dir_b.join(&file_key).exists(), "and its files");
    a.set_user_active(&acme, undone, false).await.expect("A takes changes again");

    // An event a clock of 1970 dated: its months to now are more than 600, and the copy is refused, saying so, not a crash.
    sqlx::query("select ferrum_ensure_month_partitions('audit_event', '1970-01-05 10:00+00', '1970-01-05 10:00+00')").execute(&a.db.pool).await.unwrap();
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("insert into audit_event (tenant_id, at, entity, record_id, action, hash, redacted) values ($1, '1970-01-05 10:00+00', 'Clock', $1, 'kept', '\\x00', '[\"diff\"]')")
        .bind(t)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    refused(b.move_workspace("acme", &from(None), Run::Start, &quiet).await, "audit_event would need a partition for every month from 1970-01-05");
    b.move_workspace("acme", &from(None), Run::Abort, &quiet).await.expect("aborted");
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("select set_config('ferrum.copy', 'on', true)").execute(&mut *tx).await.unwrap();
    sqlx::query("delete from audit_event where tenant_id = $1 and entity = 'Clock'").bind(t).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    // A writer the freeze does not stop changes A after the copy: the end refuses, and the next run copies afresh.
    b.move_workspace("acme", &from(Some(Stop::Copied)), Run::Start, &quiet).await.expect("copied");
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("update api_key set name = 'renamed' where tenant_id = $1").bind(t).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    refused(b.move_workspace("acme", &from(None), Run::Resume, &quiet).await, "changed at the source after it was copied");
    b.move_workspace("acme", &from(Some(Stop::Moved)), Run::Resume, &quiet).await.expect("past the point of no return");
    refused(b.move_workspace("acme", &from(None), Run::Abort, &quiet).await, "point of no return");
    b.move_workspace("acme", &from(None), Run::Resume, &quiet).await.expect("arrived");
    b.move_workspace("acme", &from(None), Run::Resume, &quiet).await.expect("again: nothing to do");

    // B's own workspace: not a row touched.
    assert_eq!(rows_of(&b, bravo.tenant).await, bravo_before);

    // Everything is at B, its vault too.
    let at_b = b.load_ctx(t, acme.user, None, "it".into()).await.unwrap();
    let doc = b.read_record(&at_b, "Contact", contact, false).await.unwrap();
    assert_eq!((doc["emails"].as_array().unwrap().len(), doc["phones"].as_array().unwrap().len()), (1, 1), "{doc:?}");
    assert_eq!(std::fs::read(dir_b.join(&file_key)).unwrap(), b"kept with the workspace");
    let kept: Vec<(i64, i64)> =
        sql(&b, "select (select count(*) from legal_hold where tenant_id = $1), (select count(*) from privacy_erasure where tenant_id = $1)", t).await;
    assert_eq!(kept[0], (1, 1), "the legal hold and the erasure's record moved too");
    let (session, _) = b.verify_api_key_from(&api_key, "192.0.2.7").await.expect("the integration's key works at B, from its networks");
    assert!(b.verify_api_key_from(&api_key, "198.51.100.7").await.is_err(), "and from nowhere else");
    let kept: Vec<(i64, i64)> =
        sql(&b, "select (select count(*) from known_device where tenant_id = $1), (select coalesce(sum(n), 0) from sign_in_failure where tenant_id = $1)", t)
            .await;
    assert_eq!(kept, vec![(1, 2)], "known devices and counted failures travel");
    let SignIn::SecondFactor(ticket) = b.sign_in("acme", "pat@acme.test", PASSWORD).await.unwrap() else { panic!("a second step") };
    b.second_factor(t, pat, ticket.token_version, &ferrum_kernel::twofactor::code(&totp, now() + 30).unwrap())
        .await
        .expect("Pat's second factor, from B's vault");
    assert_eq!(session.tenant, t);
    assert_eq!(b.route_host("erp.acme.example").await.unwrap(), HostRoute::Serve { tenant: t, target: "shell".into() });
    assert_eq!(a.route_host("erp.acme.example").await.unwrap(), HostRoute::Moved { tenant: t, to: None }, "the address still points at A");
    assert!(a.moved_away(t).await.unwrap() && !b.moved_away(t).await.unwrap());
    // Ann signs in at B with her account from eu: a visiting account of it.
    let linked: Vec<(Option<String>, Option<Uuid>)> = sql(
        &b,
        "select d.home_region, d.home_sub from app_user u join app_user d on d.id = u.identity_id where u.tenant_id = $1 and u.identity_id is not null",
        t,
    )
    .await;
    assert_eq!(linked, vec![(Some("eu".into()), Some(ann))]);
    assert_eq!(sql::<(i64,)>(&a, "select count(*) from app_user where tenant_id = $1 and identity_id is not null", t).await[0].0, 0, "A's directory lets go");

    // Nothing at A takes changes; at B, the waiting payment and job go out once.
    assert!(matches!(a.create_user(&acme, "after@acme.test", "After", Some(PASSWORD), &[], false).await, Err(Error::Unavailable(_))));
    drain(&a).await;
    assert_eq!(a.run_jobs_once(5).await.unwrap(), 0);
    drain(&b).await;
    drain(&b).await;
    while b.run_jobs_once(5).await.unwrap() > 0 {}
    assert_eq!((seen.count("pay", t), seen.count("job", t)), (1, 1), "each went out once, from B");
    b.create_user(&at_b, "new@acme.test", "New", Some(PASSWORD), &[], false).await.expect("B takes changes");

    // The history came with it: the same audit events, each chain whole, today's anchor as it was made.
    let at_a: Vec<(i64,)> = sql(&a, "select id from audit_event where tenant_id = $1 and action <> 'platform.workspace_moved_away' order by id", t).await;
    let at_b: Vec<(i64,)> = sql(&b, "select id from audit_event where tenant_id = $1 order by id", t).await;
    assert!(at_b.starts_with(&at_a) && at_b.len() > at_a.len(), "audit events keep their numbers, and new ones come after them");
    assert_eq!(chains(&b, t).await, None);
    let old: Vec<(String,)> = sql(&b, "select tableoid::regclass::text from audit_event where tenant_id = $1 and entity = 'Old'", t).await;
    assert_eq!(old, vec![("audit_event_y2019m03".to_string(),)]);
    let resent: Vec<(i64,)> = sql(&b, &format!("select count(*) from audit_event where tenant_id = $1 and xid is not null and id <= {anchored}"), t).await;
    assert_eq!(resent, vec![(0,)], "B's audit sink does not send again what A wrote");
    assert!(anchor_holds(&b, t, anchored).await);
    // Events are numbered anew at B, in the same order.
    let topics = async |k: &Kernel| sql::<(String,)>(k, "select topic from outbox where tenant_id = $1 and topic in ('noted', 'pay') order by id", t).await;
    assert_eq!(topics(&a).await, topics(&b).await);

    // And back to A, once A lets go of what it kept.
    let back = MoveSource {
        url: direct(&b_url),
        keys: vec![transfer(&b, t).await],
        files: Some(Arc::new(DirFiles(dir_b.clone()))),
        regions: Some(("us".into(), "eu".into())),
        base_domain: Some("eu.example".into()),
        wait: Duration::ZERO,
        not_sealed: vec![],
        stop_after: None,
    };
    refused(a.move_workspace("acme", &back, Run::Start, &quiet).await, "finish_move");
    assert!(a.finish_move("acme", " ").await.is_err(), "it takes saying why");
    let finished = a.finish_move("acme", "checked at B").await.expect("finished");
    // Its domain may still point at A: no other workspace there may claim it for a year. Nor take its slug, which named its
    // addresses there and its apps' origins: no other workspace forms them again. It takes them back when it returns.
    let kept: Vec<(String,)> = sql(&a, "select host from host_quarantine where host = 'erp.acme.example' and workspace = $1", t).await;
    assert_eq!(kept, vec![("erp.acme.example".to_string(),)]);
    let taken = a
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Another Acme".into(),
            admin_email: "a@another.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await;
    assert!(taken.is_err(), "its slug stays out of reach at A: {taken:?}");
    assert_eq!(finished["files"], json!(1));
    assert!(!dir_a.join(&file_key).exists());
    // Another replica of A, running while the move (a task, a process of its own) brings the workspace back, which looked
    // at its address before: nothing there then.
    let replica = Db::connect(DbConfig { url: a_url.clone(), max_connections: 4, ..Default::default() }).await.expect("connect");
    open_vault(&replica, &a_url).await;
    let a2 = Kernel::boot(replica, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("a second replica of A");
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let listening = a2.spawn_listeners(stopped);
    assert_eq!(a2.route_host("acme.eu.example").await.unwrap(), HostRoute::Unknown);
    a.move_workspace("acme", &back, Run::Start, &quiet).await.expect("back at A");
    // It answers for the address as soon as the workspace is back, not once its own answer is 20 seconds old.
    let mut route = HostRoute::Unknown;
    for _ in 0..40 {
        route = a2.route_host("acme.eu.example").await.unwrap();
        if route != HostRoute::Unknown {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(route, HostRoute::Serve { tenant: t, target: "shell".into() }, "the other replica kept its answer from before");
    let kept: Vec<(i64,)> = sql(&a, "select count(*) from host_quarantine where host = 'erp.acme.example' and $1::uuid is not null", t).await;
    assert_eq!(kept, vec![(0,)], "back, what A kept of it from everyone else is its own again");
    let _ = stop.send(true);
    for l in listening {
        let _ = l.await;
    }
    assert_eq!(chains(&a, t).await, None, "its history whole at A again");
    assert!(anchor_holds(&a, t, anchored).await);
    let at_a = a.load_ctx(t, acme.user, None, "it".into()).await.unwrap();
    a.read_record(&at_a, "Contact", contact, false).await.unwrap();
    a.create_user(&at_a, "home@acme.test", "Home", Some(PASSWORD), &[], false).await.expect("A takes changes");
    let linked: Vec<(Option<Uuid>,)> = sql(
        &a,
        "select identity_id from app_user where tenant_id = $1 and id = (select id from app_user where tenant_id = $1 and email = 'admin@acme.test')",
        t,
    )
    .await;
    assert_eq!(linked, vec![(Some(ann),)], "Ann's own account again, at home");
    drain(&a).await;
    assert_eq!(seen.count("pay", t), 1, "nothing went out twice");
    assert!(b.moved_away(t).await.unwrap());
    // A has a base domain: the workspace answers under it there, and B's addresses send people to it.
    assert_eq!(a.route_host("acme.eu.example").await.unwrap(), HostRoute::Serve { tenant: t, target: "shell".into() });
    // Its names come with it under A's base, showing what they showed; the copies under the other base cannot reach A, so
    // A lets go of them.
    assert_eq!(a.route_host("sales--acme.eu.example").await.unwrap(), HostRoute::Serve { tenant: t, target: "shell".into() });
    let shown = a.host_shown("sales--acme.eu.example").await.unwrap().expect("a shell address");
    assert_eq!((shown.shape.home.as_deref(), shown.shape.apps, shown.shape.label.as_deref()), (Some("crm"), Some(vec!["crm".to_string()]), Some("Sales")));
    let old: Vec<(bool,)> = sql(&a, "select released_at is not null from tenant_host where host = 'sales--acme.us.example' and tenant_id = $1", t).await;
    assert_eq!(old, vec![(true,)]);
    // (The move tells B's running replicas as it leaves: what they say from then on is what B recorded.)
    let to: Vec<(Option<String>,)> = sql(&b, "select moved_to from workspace_move where workspace = $1", t).await;
    assert_eq!(to, vec![(Some("acme.eu.example".into()),)]);

    // There and back again with a base domain on both sides: each lets go of the names under the other's base, and takes
    // its own back when the workspace returns (a name it let go of is its own to take back), rather than leaving it let go.
    b.finish_move("acme", "checked at A").await.expect("finished at B");
    *keys_a.lock().unwrap() = vec![transfer(&a, t).await];
    let away = MoveSource { base_domain: Some("us.example".into()), ..from(None) };
    b.move_workspace("acme", &away, Run::Start, &quiet).await.expect("away to B again");
    assert_eq!(b.route_host("sales--acme.us.example").await.unwrap(), HostRoute::Serve { tenant: t, target: "shell".into() }, "B's name again");
    let let_go: Vec<(bool,)> = sql(&b, "select released_at is not null from tenant_host where host = 'sales--acme.eu.example' and tenant_id = $1", t).await;
    assert_eq!(let_go, vec![(true,)], "B lets go of the name under A's base");
    a.finish_move("acme", "checked at B again").await.expect("finished at A");
    let back = MoveSource { keys: vec![transfer(&b, t).await], ..back };
    a.move_workspace("acme", &back, Run::Start, &quiet).await.expect("back at A again");
    assert_eq!(a.route_host("sales--acme.eu.example").await.unwrap(), HostRoute::Serve { tenant: t, target: "shell".into() }, "A's name again");
    let shown = a.host_shown("sales--acme.eu.example").await.unwrap().expect("a shell address");
    assert_eq!(shown.shape.label.as_deref(), Some("Sales"), "showing what it showed");

    // Anyone else is refused what A keeps: a workspace moving in with an address A keeps for no one (a deleted workspace's)
    // stops at the copy, with nothing of it here; aborted, it works on at B.
    let mut tx = a.db.system_tx().await.unwrap();
    sqlx::query("insert into host_quarantine (host, until) values ('shop.gone.example', now() + interval '1 day')").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = b.db.system_tx().await.unwrap();
    sqlx::query(
        "insert into tenant_host (host, tenant_id, kind, target, verify_token, verified_at) values ('shop.gone.example', $1, 'custom', 'shell', 'x', now())",
    )
    .bind(bravo.tenant)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    refused(a.move_workspace("bravo", &back, Run::Start, &quiet).await, "shop.gone.example");
    a.move_workspace("bravo", &back, Run::Abort, &quiet).await.expect("aborted");
    assert_eq!(sql::<(i64,)>(&a, "select count(*) from tenant_host where tenant_id = $1", bravo.tenant).await, vec![(0,)]);
    b.create_user(&bravo, "after@bravo.test", "After", Some(PASSWORD), &[], false).await.expect("bravo works on at B");

    for u in [&a_url, &b_url] {
        as_superuser(u, "do $$ begin execute format('revoke set on parameter session_replication_role from %I', (select datdba::regrole::text from pg_database where datname = current_database())); end $$").await;
    }
    let _ = std::fs::remove_dir_all(dirs);
}
