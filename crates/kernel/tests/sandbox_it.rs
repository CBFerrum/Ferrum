//! A sandbox of a workspace: a copy under a new id that never reaches outside, beside the live workspace (`from:here`)
//! and from another installation, against two real PostgreSQL databases (`FERRUM_TEST_DATABASE_URL` and
//! `FERRUM_RESTORE_TEST_DATABASE_URL`, both reset first; helpers as in workspace_move_it.rs).

use async_trait::async_trait;
use ferrum_kernel::auth::SignIn;
use ferrum_kernel::jobs::{Job, JobHandler, NewJob};
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::sandbox::{Copy, Halt, SANDBOX};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::shell::HostRoute;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::twofactor::code;
use ferrum_kernel::workspace_move::MoveSource;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Db, DbConfig, Error, EventSink, HookRegistry, Kernel, KernelSettings, Mailer, Result, Trace};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// What went out, and for which workspace: events an app handled, mail handed to the mailer.
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

/// A job that calls the receiver through the kernel's client, as an app's job calls a service.
struct CallOut(String);

#[async_trait]
impl JobHandler for CallOut {
    fn kind(&self) -> &'static str {
        "test.call_out"
    }
    fn title(&self) -> &'static str {
        "Calls out"
    }
    async fn run(&self, k: &Kernel, _: &Job) -> Result<Value> {
        let (client, url) = k.vetted_client(&self.0, true, Duration::from_secs(5), "it").await?;
        client.get(url).send().await.map_err(|e| Error::Unavailable(e.to_string()))?;
        Ok(json!({}))
    }
}

/// Mail handed over, by address.
struct Mails(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl Mailer for Mails {
    async fn send(&self, to: &str, _: &str, _: &str) -> Result<()> {
        self.0.lock().unwrap().push(to.to_string());
        Ok(())
    }
}

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

/// A local receiver that counts every connection made to it: what must stay silent for a sandbox.
async fn receiver() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = format!("http://{}/hook", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut s, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0; 65536];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
        }
    });
    (at, hits)
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

async fn installation(url: &str, key: &SecretKey, seen: &Arc<Seen>, mails: &Arc<Mutex<Vec<String>>>, files: &Path, call: &str) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(Arc::new(Sink(seen.clone())));
    hooks.add_job_handler(Arc::new(CallOut(call.to_string())));
    let settings = KernelSettings { secret_key: Some(key.clone()), webhooks_allow_private: true, ..Default::default() };
    let k = Kernel::boot(db, catalog(), hooks, settings, Some(Arc::new(Mails(mails.clone())))).await.expect("boot");
    k.set_file_store(Arc::new(DirFiles(files.to_path_buf())));
    k.ensure_directory().await.unwrap();
    k
}

fn superuser(address: &str) -> String {
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    format!("{base}/{}", address.rsplit('/').next().unwrap())
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

async fn count(k: &Kernel, table: &str, tenant: Uuid) -> i64 {
    sql::<(i64,)>(k, &format!("select count(*) from {table} where tenant_id = $1"), tenant).await[0].0
}

async fn run(k: &Kernel, q: &str, tenant: Uuid) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(q.to_string())).bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Every row a workspace has, table by table, as a digest.
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

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

async fn id_of(k: &Kernel, slug: &str) -> Uuid {
    let mut tx = k.db.system_tx().await.unwrap();
    let id = sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    id
}

async fn queue(k: &Kernel, tenant: Uuid, topic: &str) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, topic, None, None, json!({"amount": 100})).await.unwrap();
    tx.commit().await.unwrap();
}

fn quiet(_: &str) {}

/// The holders of the sandbox floor (crates/kernel/src/window.rs), in order.
async fn floor_holders(k: &Kernel) -> Vec<String> {
    let (floors, _) = k.floors().await.unwrap();
    let mut h: Vec<String> = floors.into_iter().filter(|f| f.feature == ferrum_kernel::sandbox::FLOOR.feature).map(|f| f.holder).collect();
    h.sort();
    h
}

fn refused(r: Result<Value>, says: &str) {
    match r {
        Err(Error::Conflict(m)) | Err(Error::Unavailable(m)) | Err(Error::NotFound(m)) => assert!(m.contains(says), "{m}"),
        Err(Error::Validation(f)) => assert!(format!("{f:?}").contains(says), "{f:?}"),
        other => panic!("expected a refusal saying {says:?}, got {other:?}"),
    }
}

#[test]
fn sandboxes() {
    // Its future is large: a runtime with room for it.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(a_sandbox_copies_a_workspace_and_reaches_nothing_outside()).await.unwrap() });
}

async fn a_sandbox_copies_a_workspace_and_reaches_nothing_outside() {
    let (a_url, b_url) = (url("FERRUM_TEST_DATABASE_URL", "ferrum_test"), url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test"));
    let (key_a, key_b) = (SecretKey::new([1; 32]), SecretKey::new([2; 32]));
    let dirs = std::env::temp_dir().join(format!("ferrum-sandbox-{}", Uuid::now_v7()));
    let (dir_a, dir_b) = (dirs.join("a"), dirs.join("b"));
    let (seen, mails) = (Arc::new(Seen::default()), Arc::new(Mutex::new(Vec::new())));
    let (hook_url, hits) = receiver().await;
    let a = installation(&a_url, &key_a, &seen, &mails, &dir_a, &hook_url).await;
    let b = installation(&b_url, &key_b, &seen, &mails, &dir_b, &hook_url).await;
    for u in [&a_url, &b_url] {
        as_superuser(u, "grant set on parameter session_replication_role to ferrum").await;
    }

    // Acme, live at A: records with lines, a file, sealed values of both kinds, a connection with tokens, a webhook, an
    // API key, an enforced second factor and single sign-on, an opt-out, an erasure, a legal hold, waiting events and a
    // queued job.
    let tenant = a
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["contacts".into()],
            settings: None,
        })
        .await
        .unwrap();
    let t = tenant;
    let s = a.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let acme = a.load_ctx(t, s.user, None, "it".into()).await.unwrap();
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
    a.store_file(&acme, Some(("Contact", contact)), "notes.txt", "text/plain", b"kept with the workspace").await.unwrap();
    let (file_key,): (String,) = sql(&a, "select storage_key from file_object where tenant_id = $1", t).await.remove(0);
    // Pat signs in with a second factor; Clerk with a password, where single sign-on does not stop them.
    let pat = a.create_user(&acme, "pat@acme.test", "Pat", Some(PASSWORD), &[], false).await.unwrap();
    a.create_user(&acme, "clerk@acme.test", "Clerk", Some(PASSWORD), &[], false).await.unwrap();
    let s = a.login("acme", "pat@acme.test", PASSWORD).await.unwrap();
    let pat_ctx = a.load_ctx(t, pat, Some(s.token_version), "it".into()).await.unwrap();
    let secret = a.begin_two_factor(&pat_ctx).await.unwrap()["secret"].as_str().unwrap().to_string();
    a.enable_two_factor(&pat_ctx, &code(&secret, now()).unwrap()).await.unwrap();
    let (hook, conn, old_conn, channel, provider) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let mut tx = a.db.system_tx().await.unwrap();
    for q in [
        format!("insert into record_secret (tenant_id, entity, record_id, field, sealed, key_id) values ($1, 'Contact', '{contact}', 'token', '{}', '{}')", key_a.seal(&format!("{t}/Contact/{contact}/token"), "app-secret").unwrap(), key_a.id),
        format!("insert into webhook (tenant_id, id, name, url, secret, topics) values ($1, '{hook}', 'books', '{hook_url}', '{}', array['*'])", a.seal_webhook_secret(t, hook, "whsec").unwrap()),
        format!("insert into connection (tenant_id, id, provider, name, sealed) values ($1, '{old_conn}', 'books', 'Books', '{}')", key_a.seal(&format!("connection:{t}:{old_conn}"), "{\"token\": \"live\"}").unwrap()),
        format!("insert into conn (tenant_id, id, name, label, description, sealed) values ($1, '{conn}', 'pay', 'Payments', '{{}}', '{}')", key_a.seal(&format!("conn:{t}:{conn}"), "{\"key\": \"sk_live\"}").unwrap()),
        format!("insert into conn_token (tenant_id, conn_id, user_id, sealed) values ($1, '{conn}', '{pat}', '{}')", key_a.seal("token", "refresh-me").unwrap()),
        format!("insert into message_channel (tenant_id, id, kind, name, description, sealed) values ($1, '{channel}', 'text', 'Texts', '{{}}', '{}')", key_a.seal(&format!("message_channel:{t}:{channel}"), "{\"auth\": \"live\"}").unwrap()),
        format!("insert into sso_provider (tenant_id, id, label, issuer, client_id, client_secret) values ($1, '{provider}', 'Company', 'https://idp.example', 'ferrum', '{}')", key_a.seal(&format!("sso|{t}|{provider}"), "client-secret").unwrap()),
        "insert into contact_consent (tenant_id, email, purpose, granted, source) values ($1, 'no-mail@buyer.test', 'marketing', false, 'it')".to_string(),
        "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) values ('erp.acme.example', $1, 'custom', 'shell', true, 'x', now())".to_string(),
        "update tenant set settings = settings || '{\"require_sso\": true, \"require_two_factor\": true}' where id = $1".to_string(),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(q)).bind(t).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    a.place_legal_hold(&acme, Some("subject@acme.test"), None, "Matter 12").await.unwrap();
    a.create_user(&acme, "forget-me@acme.test", "Forget Me", Some(PASSWORD), &[], false).await.unwrap();
    a.erase_personal_data(&acme, "forget-me@acme.test", "Asked to be forgotten").await.unwrap();
    let (_, api_key) = a.create_api_key(&acme, acme.user, "integration", None, None).await.unwrap();
    drain(&a).await;
    // The live workspace reaches the receiver: what the sandbox must not.
    while a.deliver_webhooks_once().await.unwrap() > 0 {}
    assert!(hits.load(Ordering::SeqCst) > 0, "the live workspace's webhook is delivered");
    queue(&a, t, "pay").await;
    let mut tx = a.db.tenant_tx(t).await.unwrap();
    ferrum_kernel::jobs::enqueue(&mut tx, t, Some(acme.user), NewJob::new("test.waiting", json!({}))).await.unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(a.sign_in("acme", "clerk@acme.test", PASSWORD).await, Err(Error::Forbidden(_))), "single sign-on is required at the live workspace");
    let before = rows_of(&a, t).await;

    let until = chrono::Utc::now().date_naive() + chrono::Duration::days(30);
    let here = |halt: Option<Halt>, resume: bool| Copy { from: None, until, base_domain: Some("example.test".into()), not_sealed: vec![], resume, halt };

    // Refused rather than guessed.
    refused(a.copy_workspace("acme", "acme-try", &Copy { until: until + chrono::Duration::days(90), ..here(None, false) }, &quiet).await, "90 days");
    refused(a.copy_workspace("acme", "acme", &here(None, false), &quiet).await, "taken");
    refused(a.copy_workspace("nobody", "acme-try", &here(None, false), &quiet).await, "nobody");
    refused(a.copy_workspace("acme", "acme-try", &here(None, true), &quiet).await, "nothing to resume");
    run(&a, "insert into workspace_move (workspace, move_id, side, state, peer) values ($1, gen_random_uuid(), 'source', 'leaving', 'elsewhere')", t).await;
    refused(a.copy_workspace("acme", "acme-try", &here(None, false), &quiet).await, "moving");
    run(&a, "delete from workspace_move where workspace = $1", t).await;
    let same = MoveSource {
        url: a_url.clone(),
        keys: vec![key_a.clone()],
        files: None,
        regions: None,
        base_domain: None,
        wait: Duration::ZERO,
        not_sealed: vec![],
        stop_after: None,
    };
    refused(a.copy_workspace("acme", "acme-try", &Copy { from: Some(same), ..here(None, false) }, &quiet).await, "from:here");

    // A crash before the copy commits leaves nothing; one after it, a marked sandbox that sends nothing, which resume
    // finishes and delete_sandbox removes.
    assert!(a.copy_workspace("acme", "acme-crash", &here(Some(Halt::BeforeCommit), false), &quiet).await.is_err());
    assert!(sql::<(i64,)>(&a, "select count(*) from tenant where slug = 'acme-crash' and $1::uuid is not null", t).await[0].0 == 0, "nothing");
    assert!(floor_holders(&a).await.is_empty(), "and no floor");
    a.copy_workspace("acme", "acme-crash", &here(Some(Halt::BeforeFiles), false), &quiet).await.unwrap();
    let crashed = id_of(&a, "acme-crash").await;
    assert_eq!(floor_holders(&a).await, vec![crashed.to_string()], "a sandbox never exists without its floor");
    assert!(matches!(a.outward(crashed).await, Err(Error::Forbidden(_))), "marked, and sends nothing");
    let crashed_key = file_key.replace(&t.to_string(), &crashed.to_string());
    assert!(!dir_a.join(&crashed_key).exists());
    refused(a.copy_workspace("acme", "acme-crash", &here(None, false), &quiet).await, "resume:yes");
    a.copy_workspace("acme", "acme-crash", &here(None, true), &quiet).await.expect("resumed");
    assert_eq!(std::fs::read(dir_a.join(&crashed_key)).unwrap(), b"kept with the workspace");
    a.delete_sandbox("acme-crash").await.unwrap();
    assert!(floor_holders(&a).await.is_empty(), "deleted, its floor goes");

    // Two sandboxes beside the live workspace, in its own database.
    let said = Arc::new(Mutex::new(Vec::<String>::new()));
    let say = {
        let said = said.clone();
        move |l: &str| said.lock().unwrap().push(l.to_string())
    };
    a.copy_workspace("acme", "acme-try", &here(None, false), &say).await.expect("a sandbox");
    a.copy_workspace("acme", "acme-two", &Copy { base_domain: None, ..here(None, false) }, &quiet).await.expect("a second one");
    let (sb, two) = (id_of(&a, "acme-try").await, id_of(&a, "acme-two").await);
    assert_ne!(sb, two);
    let mut both = vec![sb.to_string(), two.to_string()];
    both.sort();
    assert_eq!(floor_holders(&a).await, both, "each sandbox holds the floor");
    let said = said.lock().unwrap().join("\n");
    assert!(said.contains("single sign-on was required there"), "{said}");

    // And one at B, from A.
    let from_a = MoveSource {
        url: a_url.clone(),
        keys: vec![key_a.clone()],
        files: Some(Arc::new(DirFiles(dir_a.clone()))),
        regions: None,
        base_domain: None,
        wait: Duration::ZERO,
        not_sealed: vec![],
        stop_after: None,
    };
    let build: Vec<(String,)> = sql(&a, "select value from cluster_state where key = 'schema_build_date' and $1::uuid is not null", t).await;
    run(&a, "update cluster_state set value = '2030-01-01T00:00:00Z' where key = 'schema_build_date' and $1::uuid is not null", t).await;
    refused(b.copy_workspace("acme", "acme-b", &Copy { from: Some(from_a.clone_source()), base_domain: None, ..here(None, false) }, &quiet).await, "one build");
    run(&a, &format!("update cluster_state set value = '{}' where key = 'schema_build_date' and $1::uuid is not null", build[0].0), t).await;
    b.copy_workspace("acme", "acme-b", &Copy { from: Some(from_a.clone_source()), base_domain: None, ..here(None, false) }, &quiet).await.expect("from A");
    let at_b = id_of(&b, "acme-b").await;

    // The source: not a row changed, never frozen, and it takes changes.
    assert_eq!(rows_of(&a, t).await, before, "the source is as it was");
    assert_eq!(sql::<(i64,)>(&a, "select count(*) from workspace_move where workspace = $1", t).await[0].0, 0, "never held or frozen");
    a.create_user(&acme, "after@acme.test", "After", Some(PASSWORD), &[], false).await.expect("the live workspace takes changes");
    // Its waiting work goes out from it, as ever.
    drain(&a).await;
    while a.deliver_webhooks_once().await.unwrap() > 0 {}
    assert_eq!(seen.count("pay", t), 1);
    let live_hits = hits.load(Ordering::SeqCst);

    // The sandbox has the records, the lines and the file under its own id, and says what it is a copy of.
    for (k, id) in [(&a, sb), (&a, two), (&b, at_b)] {
        let admin = k.load_ctx(id, acme.user, None, "it".into()).await.unwrap();
        let doc = k.read_record(&admin, "Contact", contact, false).await.unwrap();
        assert_eq!((doc["emails"].as_array().unwrap().len(), doc["phones"].as_array().unwrap().len()), (1, 1));
        assert_eq!(count(k, "privacy_erasure", id).await, 1, "erased people stay erased");
        assert_eq!(count(k, "contact_consent", id).await, count(&a, "contact_consent", t).await, "consent and opt-outs");
        for gone in ["api_key", "legal_hold", "outbox", "job", "record_secret", "conn_token", "user_session", "passkey"] {
            assert_eq!(count(k, gone, id).await, 0, "{gone} is not copied");
        }
        let first: Vec<(String, Value)> = sql(k, "select action, diff from audit_event where tenant_id = $1 order by id limit 1", id).await;
        assert_eq!(first[0].0, "platform.sandbox_created");
        assert_eq!(first[0].1["workspace"], json!(t));
        assert_eq!(first[0].1["single_sign_on_was_required"], json!(true));
        assert_eq!(first[0].1["blanked_columns"], json!([]), "nothing of the kernel's own was taken for a sealed value");
        assert_eq!(count(k, "user_recovery_code", id).await, 10, "recovery codes travel");
        let mark: Vec<(Option<Uuid>, String)> = sql(k, "select sandbox_of, name from tenant where id = $1", id).await;
        assert_eq!(mark[0], (Some(t), "Acme (sandbox)".to_string()));
        let sealed: Vec<(i64, i64, i64, i64)> = sql(
            k,
            "select (select count(*) from connection where tenant_id = $1 and sealed is not null), (select count(*) from conn where tenant_id = $1 and sealed is not null), \
             (select count(*) from message_channel where tenant_id = $1 and sealed is not null), (select count(*) from sso_provider where tenant_id = $1 and client_secret is not null)",
            id,
        )
        .await;
        assert_eq!(sealed[0], (0, 0, 0, 0), "no integration secret is there");
        let renewed: Vec<(i64,)> = sql(
            k,
            &format!(
                "select count(*) from (select id from connection where tenant_id = $1 union all select id from conn where tenant_id = $1 \
                 union all select id from message_channel where tenant_id = $1 union all select id from sso_provider where tenant_id = $1) x \
                 where id in ('{old_conn}', '{conn}', '{channel}', '{provider}')"
            ),
            id,
        )
        .await;
        assert_eq!((renewed[0].0, count(k, "connection", id).await), (0, 1), "the outside world's ids for the live workspace's are not the sandbox's");
        // A person signs in with their password and their second factor; single sign-on is not required there.
        let SignIn::SecondFactor(ticket) = k
            .sign_in(
                if id == at_b {
                    "acme-b"
                } else if id == sb {
                    "acme-try"
                } else {
                    "acme-two"
                },
                "pat@acme.test",
                PASSWORD,
            )
            .await
            .unwrap()
        else {
            panic!("a second step")
        };
        k.second_factor(id, pat, ticket.token_version, &code(&secret, now() + 30).unwrap()).await.expect("their second factor");
    }
    assert!(
        matches!(a.sign_in("acme-try", "clerk@acme.test", PASSWORD).await, Ok(SignIn::SetupRequired(_))),
        "a password signs in where single sign-on was required"
    );
    let sb_key = file_key.replace(&t.to_string(), &sb.to_string());
    assert_eq!(std::fs::read(dir_a.join(&sb_key)).unwrap(), b"kept with the workspace");
    assert_eq!(std::fs::read(dir_b.join(file_key.replace(&t.to_string(), &at_b.to_string()))).unwrap(), b"kept with the workspace");
    assert_eq!(a.route_host("acme-try.example.test").await.unwrap(), HostRoute::Serve { tenant: sb, target: "shell".into() });
    assert_eq!(
        a.route_host("erp.acme.example").await.unwrap(),
        HostRoute::Serve { tenant: t, target: "shell".into() },
        "the live address stays the live workspace's"
    );
    let (session, _) = a.verify_api_key_from(&api_key, "127.0.0.1").await.unwrap();
    assert_eq!(session.tenant, t, "the integration's key is the live workspace's alone");
    assert_eq!(a.sandbox_mark(sb).await.unwrap(), Some(json!({"of": "acme", "until": until})));
    assert_eq!(a.sandbox_mark(t).await.unwrap(), None);

    // Nothing reaches outside: mail, pushes, webhooks, calls through the kernel's client from its jobs and for it.
    let admin = a.load_ctx(sb, acme.user, None, "it".into()).await.unwrap();
    let mut tx = a.db.tenant_tx(sb).await.unwrap();
    a.queue_mail(&mut tx, sb, "ann@buyer.test", "Hello", "<p>Hi</p>").await.unwrap();
    tx.commit().await.unwrap();
    for topic in [ferrum_kernel::webpush::PUSH_TOPIC, ferrum_kernel::notify::MAIL_TOPIC, "pay"] {
        queue(&a, sb, topic).await;
    }
    let mut tx = a.db.tenant_tx(sb).await.unwrap();
    ferrum_kernel::jobs::enqueue(&mut tx, sb, Some(acme.user), NewJob::new("test.call_out", json!({}))).await.unwrap();
    tx.commit().await.unwrap();
    drain(&a).await;
    while a.deliver_webhooks_once().await.unwrap() > 0 {}
    while a.run_jobs_once(5).await.unwrap() > 0 {}
    assert!(!mails.lock().unwrap().iter().any(|m| m == "ann@buyer.test"), "no mail");
    assert_eq!(seen.count("pay", sb), 1, "its own work inside goes on");
    // What it queued for outside is set aside, saying so, where an administrator looks.
    let dead = a.dead_events(&admin, 50).await.unwrap();
    let topics: Vec<&str> = dead.iter().filter(|e| e["last_error"].as_str().is_some_and(|m| m.contains(SANDBOX))).filter_map(|e| e["topic"].as_str()).collect();
    for topic in [outbox::MAIL_TOPIC, ferrum_kernel::webpush::PUSH_TOPIC, ferrum_kernel::notify::MAIL_TOPIC] {
        assert!(topics.contains(&topic), "{topic} set aside: {dead:?}");
    }
    let deliveries = a.dead_webhook_deliveries(&admin, 50).await.unwrap();
    assert!(!deliveries.is_empty() && deliveries.iter().all(|d| d["last_error"].as_str().is_some_and(|m| m.contains(SANDBOX))), "{deliveries:?}");
    // A replay is asked again: still nothing goes.
    let id: Uuid = deliveries[0]["id"].as_str().unwrap().parse().unwrap();
    a.replay_webhook_delivery(&admin, id).await.unwrap();
    while a.deliver_webhooks_once().await.unwrap() > 0 {}
    let failed: Vec<(String, Option<String>)> = sql(&a, "select status, last_error from job where tenant_id = $1 and kind = 'test.call_out'", sb).await;
    assert!(failed[0].1.as_deref().is_some_and(|m| m.contains(SANDBOX)), "the job's call out fails, saying why: {failed:?}");
    assert!(matches!(a.vetted_client_for(sb, &hook_url, true, Duration::from_secs(1), "it").await, Err(Error::Forbidden(_))));
    assert!(matches!(a.sso_begin("acme-try", Uuid::nil(), "https://acme-try.example.test/", "0123456789abcdef").await, Err(Error::NotFound(_))));
    let sso: Vec<(Uuid,)> = sql(&a, "select id from sso_provider where tenant_id = $1", sb).await;
    assert!(
        matches!(a.sso_begin("acme-try", sso[0].0, "https://acme-try.example.test/", "0123456789abcdef").await, Err(Error::Forbidden(m)) if m.contains(SANDBOX))
    );
    // An app's handler serving a person's request calls out with the kernel's client and no workspace named, as
    // Bookings, Comms, Drive, Edge, Live, LMS and Social do: refused for a sandbox's request, also from a task it spawns.
    let handler = |k: Arc<Kernel>, tenant: Uuid, url: String| async move {
        let entry =
            Arc::new(Entry { zones: Arc::new(Default::default()), placed: Placed::Nowhere, address: "127.0.0.1".into(), credential: Credential::Cookie });
        k.request_ctx(tenant, acme.user, None, "req".into(), &entry).await.unwrap();
        let direct = k.vetted_client(&url, true, Duration::from_secs(5), "an app").await.map(|_| ());
        let k2 = k.clone();
        let spawned = Trace::spawn(async move { k2.vetted_client(&url, true, Duration::from_secs(5), "an app").await.map(|_| ()) }).await.unwrap();
        (direct, spawned)
    };
    let request = |tenant: Uuid| Trace { correlation: "req".into(), ..Default::default() }.scope(handler(a.clone(), tenant, hook_url.clone()));
    let (direct, spawned) = request(sb).await;
    for r in [direct, spawned] {
        assert!(matches!(r, Err(Error::Forbidden(ref m)) if m.contains(SANDBOX)), "a sandbox's request: {r:?}");
    }
    let (direct, spawned) = request(t).await;
    assert!(direct.is_ok() && spawned.is_ok(), "the live workspace's request");
    assert!(a.vetted_client(&hook_url, true, Duration::from_secs(1), "a task").await.is_ok(), "no workspace named: as for the installation");
    // In a zone that limits staff to some apps, the sandbox still sends nothing, and says "sandbox": a request's own
    // call, and a job it queued, which is done in that zone.
    a.set_zones(Arc::new(Zones::parse(r#"{"desk": {"staff": ["contacts"]}}"#, "", "", "").unwrap()));
    let in_desk = |tenant: Uuid| {
        let a = a.clone();
        let url = hook_url.clone();
        Trace { correlation: format!("desk-{tenant}"), ..Default::default() }.scope(async move {
            let entry = Arc::new(Entry {
                zones: Arc::new(Zones::parse(r#"{"desk": {"staff": ["contacts"]}}"#, "", "", "").unwrap()),
                placed: Placed::In("desk".into()),
                address: "10.1.2.3".into(),
                credential: Credential::Token,
            });
            let ctx = a.request_ctx(tenant, pat, None, "desk".into(), &entry).await.unwrap();
            let mut tx = a.db.tenant_tx(tenant).await.unwrap();
            ferrum_kernel::jobs::enqueue(&mut tx, tenant, Some(ctx.user), NewJob::new("test.call_out", json!({}))).await.unwrap();
            tx.commit().await.unwrap();
            a.vetted_client(&url, true, Duration::from_secs(1), "an app").await.map(|_| ())
        })
    };
    assert!(matches!(in_desk(sb).await, Err(Error::Forbidden(m)) if m.contains(SANDBOX)), "a sandbox's request in a limited zone");
    assert!(in_desk(t).await.is_ok(), "the live workspace's, in the same zone");
    let zoned: Vec<(Option<String>,)> =
        sql(&a, "select zone from job where tenant_id = $1 and kind = 'test.call_out' order by created_at desc limit 1", sb).await;
    assert_eq!(zoned[0].0.as_deref(), Some("desk"), "the job is the zone's");
    run(&a, "delete from job where tenant_id = $1 and kind = 'test.call_out'", t).await;
    while a.run_jobs_once(5).await.unwrap() > 0 {}
    let failed: Vec<(Option<String>,)> = sql(&a, "select last_error from job where tenant_id = $1 and zone = 'desk'", sb).await;
    assert!(failed[0].0.as_deref().is_some_and(|m| m.contains(SANDBOX)), "the job in the zone fails saying sandbox: {failed:?}");
    a.set_zones(Arc::new(Default::default()));
    // An app's upkeep goes through every workspace in turn (as Social, Bookings, Control and Edge do): a sandbox's
    // turn calls nothing outside, the live workspace's next turn still does.
    let turns = ferrum_kernel::sandbox::upkeep(async {
        let mut out = vec![];
        for tenant in [sb, t] {
            a.db.tenant_tx(tenant).await.unwrap().commit().await.unwrap();
            out.push(a.vetted_client(&hook_url, true, Duration::from_secs(1), "an app's upkeep").await.is_ok());
        }
        out
    })
    .await;
    assert_eq!(turns, vec![false, true]);
    assert_eq!(hits.load(Ordering::SeqCst), live_hits, "the receiver stayed silent");
    assert!(a.outward(t).await.is_ok() && a.outward(Uuid::now_v7()).await.is_err(), "a workspace it cannot find sends nothing");
    // The mark stays: no update turns a sandbox into a live workspace.
    let mut tx = a.db.system_tx().await.unwrap();
    assert!(
        sqlx::query("update tenant set sandbox_of = null, sandbox_from = null, sandbox_until = null where id = $1").bind(sb).execute(&mut *tx).await.is_err()
    );
    tx.rollback().await.unwrap();

    // A sandbox is not copied again; an erasure in the source names the sandboxes that still hold a copy.
    refused(a.copy_workspace("acme-try", "acme-three", &here(None, false), &quiet).await, "sandbox itself");
    let erased = a.erase_personal_data(&acme, "clerk@acme.test", "Asked").await.unwrap();
    let named: Vec<&str> = erased["sandboxes"].as_array().unwrap().iter().filter_map(|s| s["workspace"].as_str()).collect();
    assert_eq!(named, vec!["acme-try", "acme-two"]);
    assert_eq!(a.personal_data(&acme, "pat@acme.test").await.unwrap()["sandboxes"].as_array().unwrap().len(), 2);

    // Deleted: a sandbox, never a live workspace; the source's file stays.
    refused(a.delete_sandbox("acme").await, "live workspace");
    a.delete_sandbox("acme-try").await.unwrap();
    assert_eq!(count(&a, "app_user", sb).await, 0);
    assert_eq!(floor_holders(&a).await, vec![two.to_string()], "the second sandbox keeps the floor");
    assert!(!dir_a.join(&sb_key).exists());
    assert!(dir_a.join(&file_key).exists(), "the live workspace's file is its own");
    assert!(a.expired_sandboxes().await.unwrap().is_empty());
    run(&a, "update tenant set sandbox_until = current_date - 1 where id = $1", two).await;
    assert_eq!(a.expired_sandboxes().await.unwrap().iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), vec!["acme-two"]);
    a.delete_sandbox("acme-two").await.unwrap();
    assert!(floor_holders(&a).await.is_empty(), "the last sandbox deleted, the floor goes");

    for u in [&a_url, &b_url] {
        as_superuser(u, "revoke set on parameter session_replication_role from ferrum").await;
    }
    let _ = std::fs::remove_dir_all(dirs);
}

trait CloneSource {
    fn clone_source(&self) -> MoveSource;
}

impl CloneSource for MoveSource {
    fn clone_source(&self) -> MoveSource {
        MoveSource {
            url: self.url.clone(),
            keys: self.keys.clone(),
            files: self.files.clone(),
            regions: self.regions.clone(),
            base_domain: self.base_domain.clone(),
            wait: self.wait,
            not_sealed: self.not_sealed.clone(),
            stop_after: self.stop_after,
        }
    }
}
