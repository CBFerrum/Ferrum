//! The kernel's own stores of secrets, kept in the vault (docs/operations.md, "The vault"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, the public schema and the vault's reset first).
//!
//! Every store a workspace fills through the kernel (secret fields of two apps and one an app adds to another's entity,
//! a second factor, a connection's secrets, a channel's, a webhook's signing secret, the push key) reads on two replicas
//! from the vault alone: its own table holds the vault's mark, and nothing is plain. One app cannot read another's secret
//! field, and the platform reads none of an app's. Every level of key rotates and the values still read. What a store no
//! longer holds goes from the vault, with its row or in the hourly upkeep. A sandbox treats each kind by its rule
//! (fields, connections and channels blank; webhooks a secret of their own; second factors the same), and a move there
//! and back (`FERRUM_RESTORE_TEST_DATABASE_URL` the other installation) reads every value at each end. After each
//! scenario no table holds a value in the clear.

use ferrum_kernel::connections::{ConnectionInput, Description};
use ferrum_kernel::messages::{ChannelDesc, ChannelInput, Inbound};
use ferrum_kernel::sandbox::Copy;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, Opener, Scope, Vault};
use ferrum_kernel::workspace_move::{MoveSource, Run};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

/// core, app crm's package (a lead with an API key) and app hr's (a bank with a PIN, and a token it adds to crm's lead).
fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-vault-stores-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for p in ["crm/entities", "hr/entities"] {
        std::fs::create_dir_all(dir.join(p)).unwrap();
    }
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("crm/package.yaml"), "package: crm\nversion: 0.1.0\nlabel: CRM\ndepends: [core]\n").unwrap();
    std::fs::write(
        dir.join("crm/entities/lead.yaml"),
        "- name: Lead\n  label: Lead\n  kind: master\n  title_field: title\n  fields:\n    - {name: title, type: data, required: true}\n    - {name: api_key, type: secret}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("hr/package.yaml"),
        "package: hr\nversion: 0.1.0\nlabel: HR\ndepends: [core, crm]\nfield_prefix: hr_\noverlays:\n  - entity: Lead\n    ops:\n      - {op: add_field, field: {name: hr_token, type: secret}}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("hr/entities/bank.yaml"),
        "- name: Bank\n  label: Bank\n  kind: master\n  title_field: title\n  fields:\n    - {name: title, type: data, required: true}\n    - {name: pin, type: secret}\n",
    )
    .unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

/// A replica: its kernel and the handles its registry gave out (the server's, crm's, hr's).
struct Replica {
    k: Arc<Kernel>,
    platform: Vault,
    crm: Vault,
    hr: Vault,
}

fn other_url() -> String {
    std::env::var("FERRUM_RESTORE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_restore_test".into())
}

/// A replica of the installation on `url`, its vault opened (`open`) or left closed.
async fn replica_on(url: &str, open: bool) -> Replica {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    if open {
        let opener = Opener {
            database_url: url.to_string(),
            file: std::env::temp_dir().join("ferrum-vault-stores-absent.key"),
            kdf: Kdf { m_kib: 64, t: 1, p: 1 },
            ..Default::default()
        };
        vault::open(&db, opener, false).await.expect("open");
    }
    let mut hooks = HookRegistry::default();
    let platform = hooks.platform_vault();
    let (mut crm, mut hr) = (None, None);
    hooks.app("crm", &[], |h| crm = Some(h.vault()));
    hooks.app("hr", &[], |h| hr = Some(h.vault()));
    hooks.own("crm", &["crm".into()]).own("hr", &["hr".into()]);
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    Replica { k, platform, crm: crm.unwrap(), hr: hr.unwrap() }
}

/// A replica of the test installation, its vault opened (`open`) or left closed.
async fn replica(open: bool) -> Replica {
    replica_on(&url(), open).await
}

async fn reset_on(url: &str) {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 2, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.pool.close().await;
}

async fn reset() {
    reset_on(&url()).await;
}

async fn workspace(k: &Kernel, slug: &str) -> (Uuid, Ctx) {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["core".into(), "crm".into(), "hr".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    (tenant, ctx(k, slug, tenant).await)
}

/// The first step of signing in (the password): its session, whether or not a second step follows.
async fn signed(k: &Kernel, slug: &str) -> ferrum_kernel::auth::Session {
    match k.sign_in(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap() {
        ferrum_kernel::auth::SignIn::Session(s) | ferrum_kernel::auth::SignIn::SecondFactor(s) | ferrum_kernel::auth::SignIn::SetupRequired(s) => s,
    }
}

async fn ctx(k: &Kernel, slug: &str, tenant: Uuid) -> Ctx {
    let s = signed(k, slug).await;
    k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap()
}

/// Runs `sql` with every row seen: the system's and the vault's (as the vault's module, or a dump, sees them).
async fn sql<T: Send + Unpin + for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>>(k: &Kernel, q: &str, binds: &[Uuid]) -> Vec<T> {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let mut query = sqlx::query_as::<_, T>(sqlx::AssertSqlSafe(q.to_string()));
    for b in binds {
        query = query.bind(*b);
    }
    let rows = query.fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rows
}

async fn exec(k: &Kernel, q: &str, binds: &[&(dyn ToString + Sync)]) {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await.unwrap();
    let mut query = sqlx::query(sqlx::AssertSqlSafe(q.to_string()));
    for b in binds {
        query = query.bind(b.to_string());
    }
    query.execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Every table of the database, the vault's too, searched for each of `plain` as text in any column: a dump's view.
async fn nowhere_in_the_clear(k: &Kernel, plain: &[&str]) {
    let columns: Vec<(String, String, String)> = sql(
        k,
        "select table_schema, table_name, column_name from information_schema.columns c join information_schema.tables t using (table_schema, table_name) \
         where table_schema in ('public', 'vault') and t.table_type = 'BASE TABLE' and data_type in ('text', 'jsonb', 'json', 'bytea', 'character varying', 'ARRAY', 'USER-DEFINED')",
        &[],
    )
    .await;
    let patterns: Vec<String> = plain.iter().map(|p| format!("%{p}%")).collect();
    for (schema, table, column) in columns {
        let q = format!("select string_agg(\"{column}\"::text, ' | ') from \"{schema}\".\"{table}\" where \"{column}\"::text like any($1)");
        let mut tx = k.db.pool.begin().await.unwrap();
        sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
        let found: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(q)).bind(&patterns).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert!(found.is_none(), "a value is in the clear in {schema}.{table}.{column}");
    }
}

/// What the test knows of each value, to read it back.
struct Fixture {
    tenant: Uuid,
    lead: Uuid,
    bank: Uuid,
    user: Uuid,
    totp: String,
    conn: Uuid,
    channel: Uuid,
    hook: Uuid,
    vapid_public: String,
}

const PLAIN: &[&str] = &["crm-api-key-1", "hr-token-1", "hr-pin-1", "conn-secret-1", "chan-secret-1", "whsec-1"];

/// A channel taking calls signed with its secret `inbound`.
fn channel_input(secret: &str) -> ChannelInput {
    ChannelInput {
        kind: "text".into(),
        name: "sms".into(),
        description: ChannelDesc {
            test: true,
            inbound: Some(Inbound { auth: "token".into(), header: Some("x-token".into()), secret: "inbound".into(), ..Default::default() }),
            ..Default::default()
        },
        secrets: Some(BTreeMap::from([("inbound".to_string(), secret.to_string())])),
        enabled: true,
    }
}

/// A connection with one secret.
fn connection_input(secret: &str) -> ConnectionInput {
    ConnectionInput {
        name: "acme".into(),
        label: "Acme".into(),
        description: Description { test: true, base: "https://api.acme.test".into(), ..Default::default() },
        secrets: Some(BTreeMap::from([("api_key".to_string(), secret.to_string())])),
        enabled: true,
    }
}

/// A webhook of `tenant` signing with `secret`, kept as the admin route keeps one.
async fn webhook(k: &Kernel, tenant: Uuid, secret: &str) -> Uuid {
    let hook = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let stored = k.keep_webhook_secret(&mut tx, tenant, hook, secret).await.unwrap();
    sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics) values ($1, $2, 'h', 'https://hooks.test/x', $3, '{*}')")
        .bind(tenant)
        .bind(hook)
        .bind(&stored)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    hook
}

/// Every store filled through the kernel, on `r`.
async fn populate(r: &Replica) -> Fixture {
    let k = &r.k;
    let (tenant, admin) = workspace(k, "kept").await;
    let lead = k.insert(&admin, "Lead", obj(json!({"title": "L1", "api_key": "crm-api-key-1", "hr_token": "hr-token-1"}))).await.unwrap();
    let lead: Uuid = lead["id"].as_str().unwrap().parse().unwrap();
    let bank = k.insert(&admin, "Bank", obj(json!({"title": "B1", "pin": "hr-pin-1"}))).await.unwrap();
    let bank: Uuid = bank["id"].as_str().unwrap().parse().unwrap();
    let begun = k.begin_two_factor(&admin).await.unwrap();
    let totp = begun["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    k.enable_two_factor(&admin, &ferrum_kernel::twofactor::code(&totp, now).unwrap()).await.unwrap();
    let conn = k.save_connection_def(&admin, None, &connection_input("conn-secret-1")).await.unwrap();
    let channel = k.save_message_channel(&admin, None, &channel_input("chan-secret-1")).await.unwrap();
    let hook = webhook(k, tenant, "whsec-1").await;
    let vapid = k.vapid().await.unwrap().expect("a push key, kept in the vault");
    Fixture { tenant, lead, bank, user: admin.user, totp, conn, channel, hook, vapid_public: vapid.public.clone() }
}

/// Reads every value the kernel lets a test read, as the code that uses each reads it.
async fn reads_every_value(r: &Replica, f: &Fixture, slug: &str) {
    let k = &r.k;
    let mut tx = k.db.tenant_tx(f.tenant).await.unwrap();
    assert_eq!(r.crm.field(k, &mut tx, f.tenant, "Lead", f.lead, "api_key").await.unwrap().as_deref().map(String::as_str), Some("crm-api-key-1"));
    assert_eq!(r.hr.field(k, &mut tx, f.tenant, "Lead", f.lead, "hr_token").await.unwrap().as_deref().map(String::as_str), Some("hr-token-1"));
    assert_eq!(r.hr.field(k, &mut tx, f.tenant, "Bank", f.bank, "pin").await.unwrap().as_deref().map(String::as_str), Some("hr-pin-1"));
    tx.commit().await.unwrap();
    // The second factor signs in (each check with a code not used yet: a code is taken once).
    exec(k, "update app_user set totp_last_step = null where tenant_id = $1::uuid and id = $2::uuid", &[&f.tenant, &f.user]).await;
    let s = signed(k, slug).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    if let Err(e) = k.second_factor(f.tenant, f.user, s.token_version, &ferrum_kernel::twofactor::code(&f.totp, now).unwrap()).await {
        panic!("the second factor did not open: {e}");
    }
    // The channel's inbound secret checks a call.
    let token = |t: &str| BTreeMap::from([("x-token".to_string(), t.to_string())]);
    let ok = k.message_inbound(f.channel, &token("chan-secret-1"), &BTreeMap::new(), b"{}").await;
    assert!(ok.is_ok(), "the channel's secret opened: {ok:?}");
    assert!(k.message_inbound(f.channel, &token("wrong"), &BTreeMap::new(), b"{}").await.is_err());
    // The connection's secrets are there for its calls (named, never shown).
    let admin = ctx(k, slug, f.tenant).await;
    let defs = k.connection_defs(&admin).await.unwrap();
    assert_eq!(defs.iter().find(|d| d["id"] == json!(f.conn)).unwrap()["secrets"], json!(["api_key"]));
    // The push key is the one made.
    assert_eq!(k.vapid().await.unwrap().unwrap().public, f.vapid_public);
}

/// The vault's secrets of the kernel's stores, each with its version.
async fn kept(k: &Kernel) -> Vec<(String, String, i32)> {
    sql(k, "select owner, name, version from vault.secret where name ~ '^(field|webhook|totp|connector|conn|conn-token|channel|sso|saml-sp|saml-enc|approval|push)[.]' order by 1, 2", &[]).await
}

#[test]
fn the_kernels_stores_are_kept_in_the_vault() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    reset().await;

    // ── every store, filled through the kernel, read on two replicas from the vault alone ──
    let (a, b) = (replica(true).await, replica(true).await);
    let f = populate(&a).await;
    reads_every_value(&a, &f, "kept").await;
    reads_every_value(&b, &f, "kept").await;
    let marks: Vec<(String,)> = sql(
        &a.k,
        "select sealed from record_secret union all select totp_secret from app_user where totp_secret is not null union all select sealed from message_channel \
         union all select sealed from conn where sealed is not null union all select secret from webhook union all select sealed from server_key",
        &[],
    )
    .await;
    assert!(marks.iter().all(|(m,)| m == "vault" || m == "sealed:vault"), "only the vault's mark: {marks:?}");
    assert_eq!(marks.len(), 8, "three fields, a second factor, a channel, a connection, a webhook and the push key");
    let held = kept(&a.k).await;
    let owners: Vec<&str> = held.iter().filter(|(_, n, _)| n.starts_with("field.")).map(|(o, _, _)| o.as_str()).collect();
    assert_eq!(owners, ["crm", "hr", "hr"], "a field is its declaring app's, the one hr adds to crm's lead included");
    nowhere_in_the_clear(&a.k, PLAIN).await;
    // A change on one replica is read on the other.
    let admin = ctx(&a.k, "kept", f.tenant).await;
    a.k.update(&admin, "Lead", f.lead, obj(json!({"api_key": "crm-api-key-2"})), None).await.unwrap();
    let mut tx = b.k.db.tenant_tx(f.tenant).await.unwrap();
    assert_eq!(b.crm.field(&b.k, &mut tx, f.tenant, "Lead", f.lead, "api_key").await.unwrap().as_deref().map(String::as_str), Some("crm-api-key-2"));
    tx.commit().await.unwrap();
    a.k.update(&admin, "Lead", f.lead, obj(json!({"api_key": "crm-api-key-1"})), None).await.unwrap();

    // ── owners: an app reads its own fields, never another's; the platform reads none of an app's ──
    let mut tx = a.k.db.tenant_tx(f.tenant).await.unwrap();
    for (who, entity, record, field) in
        [(&a.hr, "Lead", f.lead, "api_key"), (&a.crm, "Lead", f.lead, "hr_token"), (&a.crm, "Bank", f.bank, "pin"), (&a.platform, "Lead", f.lead, "api_key")]
    {
        let e = who.field(&a.k, &mut tx, f.tenant, entity, record, field).await.unwrap_err();
        assert!(matches!(e, Error::Forbidden(_)), "{} reads {entity}.{field}: {e}", who.owner());
    }
    // Refused the same whether it is set or not.
    let e = a.hr.field(&a.k, &mut tx, f.tenant, "Lead", Uuid::now_v7(), "api_key").await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)));
    tx.commit().await.unwrap();
    // A handle of another registry (another server's) opens nothing here.
    let mut tx = a.k.db.tenant_tx(f.tenant).await.unwrap();
    assert!(matches!(b.crm.field(&a.k, &mut tx, f.tenant, "Lead", f.lead, "api_key").await, Err(Error::Forbidden(_))));
    tx.commit().await.unwrap();

    // ── rotation at each level: the values still read, on both replicas ──
    let w = Scope::Workspace(f.tenant);
    a.k.rotate_workspace_key(w).await.unwrap();
    a.k.rotate_owner_key(w, "platform").await.unwrap();
    a.k.rotate_owner_key(w, "crm").await.unwrap();
    assert_eq!(a.k.vault_reseal(w, "platform").await.unwrap(), 0);
    assert_eq!(a.k.vault_reseal(w, "crm").await.unwrap(), 0);
    vault::rotate_installation_key(&a.k.db, None).await.unwrap();
    // Replica b hears of it as it reads (no listener runs here): it opens the vault again, and reads on.
    reads_every_value(&a, &f, "kept").await;
    reads_every_value(&b, &f, "kept").await;
    nowhere_in_the_clear(&a.k, PLAIN).await;

    // ── what a store no longer holds goes from the vault: with its row, or (a row gone without it) in the upkeep ──
    a.k.delete(&admin, "Bank", f.bank, None).await.unwrap();
    a.k.delete_message_channel(&admin, f.channel).await.unwrap();
    exec(&a.k, "delete from webhook where id = $1::uuid", &[&f.hook]).await;
    let left = kept(&a.k).await;
    assert!(left.iter().all(|(_, n, _)| !n.starts_with("channel.")), "{left:?}");
    assert_eq!(left.iter().filter(|(_, n, _)| n.starts_with("field.")).count(), 2, "the lead's two stay: {left:?}");
    assert!(left.iter().any(|(_, n, _)| n.starts_with("webhook.")), "a row deleted on its own leaves its value until the upkeep");
    a.k.maintenance().await.unwrap();
    let left = kept(&a.k).await;
    assert!(!left.iter().any(|(_, n, _)| n.starts_with("webhook.")), "{left:?}");
    assert_eq!(left.len(), 5, "the lead's two fields, the second factor, the connection's secrets and the push key: {left:?}");

    moves_and_sandboxes().await;
    keys_are_never_made_in_a_callers_transaction().await;
    reset().await;
    reset_on(&other_url()).await;
}

/// A secret field written inside a request's open transaction, on a workspace whose vault has no key yet, then the
/// vault asked on another connection before that transaction commits: nothing waits (the uncommitted insert of the key
/// used to hold the other connection until the lock timeout), on two replicas at once, and the workspace ends with one
/// workspace key and one owner key.
async fn keys_are_never_made_in_a_callers_transaction() {
    reset().await;
    let first = replica(true).await;
    let (tenant, _) = workspace(&first.k, "keyless").await;
    // No key at all in the workspace, as before anything of it was sealed.
    let mut tx = first.k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    for q in ["delete from vault.secret where scope = $1", "delete from vault.key where scope = $1 and owner <> ''", "delete from vault.key where scope = $1"] {
        sqlx::query(q).bind(tenant).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let (a, b) = (replica(true).await, replica(true).await);
    let ws = Scope::Workspace(tenant);
    let write_then_ask = |r: Replica, c: Ctx, title: &'static str| async move {
        let mut tx = r.k.db.tenant_tx(tenant).await.unwrap();
        r.k.insert_tx(&c, &mut tx, "Lead", obj(json!({"title": title, "api_key": "Kl-1717-in-a-request"}))).await.unwrap();
        let asked = std::time::Instant::now();
        let derived = tokio::time::timeout(std::time::Duration::from_secs(5), r.crm.derive(&r.k, ws, "keys-while-open")).await;
        let waited = asked.elapsed();
        tx.commit().await.unwrap();
        (derived.is_ok_and(|d| d.is_ok()), waited)
    };
    let (ca, cb) = (ctx(&a.k, "keyless", tenant).await, ctx(&b.k, "keyless", tenant).await);
    let ((done_a, waited_a), (done_b, waited_b)) = tokio::join!(write_then_ask(a, ca, "A"), write_then_ask(b, cb, "B"));
    assert!(done_a && done_b, "the vault answered on another connection before the commit ({waited_a:?}, {waited_b:?})");
    assert!(waited_a.as_secs() < 3 && waited_b.as_secs() < 3, "and nothing waited: {waited_a:?}, {waited_b:?}");
    let keys: Vec<(String, i64)> = sql(&first.k, "select owner, count(*) from vault.key where scope = $1 group by owner order by owner", &[tenant]).await;
    assert_eq!(keys, vec![(String::new(), 1), ("crm".to_string(), 1)], "one workspace key and one owner key");
}

/// Files in a directory, as a single host keeps them.
struct DirFiles(PathBuf);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for DirFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        let path = self.0.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).map_err(|e| Error::Unavailable(e.to_string()))
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(|_| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        let _ = std::fs::remove_file(self.0.join(key));
        Ok(())
    }
}

fn quiet(_: &str) {}

async fn as_superuser(address: &str, q: &str) {
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    let db = Db::connect(DbConfig { url: format!("{base}/{}", address.rsplit('/').next().unwrap()), max_connections: 1, ..Default::default() }).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(q.to_string())).execute(&db.pool).await.unwrap();
    db.pool.close().await;
}

/// What a workspace that moves holds, to read it back where it is.
struct Mover {
    tenant: Uuid,
    lead: Uuid,
    user: Uuid,
    totp: String,
    channel: Uuid,
    conn: Uuid,
}

async fn reads_where_it_is(r: &Replica, m: &Mover, slug: &str) {
    let k = &r.k;
    let mut tx = k.db.tenant_tx(m.tenant).await.unwrap();
    assert_eq!(r.crm.field(k, &mut tx, m.tenant, "Lead", m.lead, "api_key").await.unwrap().as_deref().map(String::as_str), Some("crm-mv-1"));
    assert_eq!(r.hr.field(k, &mut tx, m.tenant, "Lead", m.lead, "hr_token").await.unwrap().as_deref().map(String::as_str), Some("hr-mv-1"));
    tx.commit().await.unwrap();
    exec(k, "update app_user set totp_last_step = null where tenant_id = $1::uuid and id = $2::uuid", &[&m.tenant, &m.user]).await;
    let s = signed(k, slug).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    k.second_factor(m.tenant, m.user, s.token_version, &ferrum_kernel::twofactor::code(&m.totp, now).unwrap()).await.expect("the second factor");
    let token = BTreeMap::from([("x-token".to_string(), "chan-mv-1".to_string())]);
    assert!(k.message_inbound(m.channel, &token, &BTreeMap::new(), b"{}").await.is_ok(), "the channel's secret");
    let defs = k.connection_defs(&ctx(k, slug, m.tenant).await).await.unwrap();
    assert_eq!(defs.iter().find(|d| d["id"] == json!(m.conn)).unwrap()["secrets"], json!(["api_key"]));
}

/// The webhook's secret as its vault row holds it (sealed): a move copies it as it is.
async fn webhook_row(k: &Kernel, tenant: Uuid) -> Vec<(Vec<u8>,)> {
    sql(k, "select sealed from vault.secret where scope = $1 and name like 'webhook.%'", &[tenant]).await
}

async fn moves_and_sandboxes() {
    let (a_url, b_url) = (url(), other_url());
    reset_on(&a_url).await;
    reset_on(&b_url).await;
    for u in [&a_url, &b_url] {
        as_superuser(u, "grant set on parameter session_replication_role to ferrum").await;
    }
    let dirs = std::env::temp_dir().join(format!("ferrum-vault-stores-files-{}", Uuid::now_v7()));
    let a = replica_on(&a_url, true).await;
    let b = replica_on(&b_url, true).await;
    a.k.set_file_store(Arc::new(DirFiles(dirs.join("a"))));
    b.k.set_file_store(Arc::new(DirFiles(dirs.join("b"))));
    let (t, admin) = workspace(&a.k, "mover").await;
    let lead = a.k.insert(&admin, "Lead", obj(json!({"title": "M", "api_key": "crm-mv-1", "hr_token": "hr-mv-1"}))).await.unwrap();
    let lead: Uuid = lead["id"].as_str().unwrap().parse().unwrap();
    let begun = a.k.begin_two_factor(&admin).await.unwrap();
    let totp = begun["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    a.k.enable_two_factor(&admin, &ferrum_kernel::twofactor::code(&totp, now).unwrap()).await.unwrap();
    let channel = a.k.save_message_channel(&admin, None, &channel_input("chan-mv-1")).await.unwrap();
    let conn = a.k.save_connection_def(&admin, None, &connection_input("conn-mv-1")).await.unwrap();
    webhook(&a.k, t, "whsec-mv-1").await;
    let m = Mover { tenant: t, lead, user: admin.user, totp: totp.clone(), channel, conn };
    reads_where_it_is(&a, &m, "mover").await;
    let plain = ["crm-mv-1", "hr-mv-1", "chan-mv-1", "conn-mv-1", "whsec-mv-1", &totp];

    // ── a sandbox: each kind through its secret's rule ──
    let until = chrono::Utc::now().date_naive() + chrono::Duration::days(10);
    let copy = Copy { from: None, until, base_domain: None, not_sealed: vec![], resume: false, halt: None };
    let said = std::sync::Mutex::new(Vec::new());
    a.k.copy_workspace("mover", "mover-try", &copy, &|line: &str| said.lock().unwrap().push(line.to_string())).await.unwrap();
    let s: Uuid = sql::<(Uuid,)>(&a.k, "select id from tenant where slug = 'mover-try'", &[]).await[0].0;
    let mut tx = a.k.db.tenant_tx(s).await.unwrap();
    assert_eq!(a.crm.field(&a.k, &mut tx, s, "Lead", lead, "api_key").await.unwrap(), None, "a sandbox has no secret field's value");
    tx.commit().await.unwrap();
    let sandbox_user: Uuid = sql::<(Uuid,)>(&a.k, "select id from app_user where tenant_id = $1 and email = 'admin@mover.test'", &[s]).await[0].0;
    // The code the live workspace took last is taken there too: the sandbox is given a fresh one.
    exec(&a.k, "update app_user set totp_last_step = null where tenant_id = $1::uuid and id = $2::uuid", &[&s, &sandbox_user]).await;
    let st = match a.k.sign_in("mover-try", "admin@mover.test", PASSWORD).await.unwrap() {
        ferrum_kernel::auth::SignIn::SecondFactor(st) => st,
        _ => panic!("the second factor travels to a sandbox"),
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    a.k.second_factor(s, sandbox_user, st.token_version, &ferrum_kernel::twofactor::code(&totp, now).unwrap()).await.expect("the same second factor");
    let blanks: Vec<(Option<String>,)> =
        sql(&a.k, "select sealed from message_channel where tenant_id = $1 union all select sealed from conn where tenant_id = $1", &[s]).await;
    assert!(blanks.len() == 2 && blanks.iter().all(|(v,)| v.is_none()), "channels and connections come blank: {blanks:?}");
    let said = said.lock().unwrap().join("\n");
    assert!(said.contains("1 secrets kept, 1 made anew"), "the second factor kept, the webhook's made anew: {said}");
    assert_eq!(webhook_row(&a.k, s).await.len(), 1, "a webhook's secret of its own");
    nowhere_in_the_clear(&a.k, &plain).await;
    a.k.delete_sandbox("mover-try").await.unwrap();

    // ── moved to the other installation, with its vault: every value reads there ──
    let source = |from: &str, keys: Vec<SecretKey>, files: &str| MoveSource {
        url: from.to_string(),
        keys,
        files: Some(Arc::new(DirFiles(dirs.join(files))) as Arc<dyn ferrum_kernel::files::FileStore>),
        regions: None,
        base_domain: None,
        wait: std::time::Duration::ZERO,
        not_sealed: vec![],
        stop_after: None,
    };
    let webhook_before = webhook_row(&a.k, t).await;
    let transfer = SecretKey::from_base64(&vault::transfer_out(&a.k, t).await.unwrap()).unwrap();
    b.k.move_workspace("mover", &source(&a_url, vec![transfer], "a"), Run::Start, &quiet).await.unwrap();
    reads_where_it_is(&b, &m, "mover").await;
    a.k.finish_move("mover", "checked at the target").await.unwrap();
    nowhere_in_the_clear(&b.k, &plain).await;

    // ── and back ──
    let back = SecretKey::from_base64(&vault::transfer_out(&b.k, t).await.unwrap()).unwrap();
    a.k.move_workspace("mover", &source(&b_url, vec![back], "b"), Run::Start, &quiet).await.unwrap();
    reads_where_it_is(&a, &m, "mover").await;
    b.k.finish_move("mover", "checked back home").await.unwrap();
    assert_eq!(webhook_row(&a.k, t).await, webhook_before, "the webhook's secret came back as it left");
    nowhere_in_the_clear(&a.k, &plain).await;
    nowhere_in_the_clear(&b.k, &plain).await;
    let _ = std::fs::remove_dir_all(&dirs);
}
