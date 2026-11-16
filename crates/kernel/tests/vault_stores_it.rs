//! The kernel's own stores of sealed values behind the vault (docs/plan/stateless.md, D5 second half), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`, the public schema and the vault's reset first).
//!
//! An installation upgraded with every store populated by the previous build's code paths (the vault closed: what the
//! old key alone sealed) reads every value before, during and after the move into the vault, on two replicas, with the
//! move left half done and finished by both at once, each value moved once; the previous build's view (the old stores
//! opened with the old key, as its code opens them) stays current, a change it makes is read from then on, and the move
//! takes it again. With no old key nothing is written to the old stores but the vault's mark, and nothing is plain.
//! One app cannot read another's secret field (a field one app adds to another's entity is the adder's), and the
//! platform reads none of an app's. Every level of key rotates and the values still read. A sandbox treats each kind as
//! before (fields, connections and channels blank; webhooks a secret of their own; second factors the same), and a move
//! there and back (`FERRUM_RESTORE_TEST_DATABASE_URL` the other installation) reads every value at each end. After each
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

/// A replica of the installation on `url`, with the old key `key` (none: the vault only) and `retired` keys, and the
/// vault opened (`open`) or left closed, as the previous build has it.
async fn replica_on(url: &str, key: Option<SecretKey>, retired: Vec<SecretKey>, open: bool) -> Replica {
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
    let settings = KernelSettings { old_key: ferrum_kernel::OldKey::configured(key, retired), ..Default::default() };
    let k = Kernel::boot(db, catalog(), hooks, settings, None).await.expect("boot");
    Replica { k, platform, crm: crm.unwrap(), hr: hr.unwrap() }
}

/// A replica with the old key `key` (none: the vault only), and the vault opened (`open`) or left closed, as the
/// previous build has it.
async fn replica(key: Option<SecretKey>, open: bool) -> Replica {
    replica_on(&url(), key, vec![], open).await
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
    webhooks: [Uuid; 2],
    sso: Uuid,
    saml_enc: Uuid,
    request: Uuid,
    vapid_public: String,
}

const PLAIN: &[&str] = &[
    "crm-api-key-1",
    "hr-token-1",
    "hr-pin-1",
    "conn-secret-1",
    "chan-secret-1",
    "whsec-plain-1",
    "whsec-sealed-1",
    "tok-access-1",
    "sso-secret-1",
    "saml-sp-private-1",
    "saml-enc-private-1",
    "approval-pin-1",
    "connector-secret-1",
];

/// Every store populated by `prev`, the previous build (its vault closed: the old key alone seals).
async fn populate(prev: &Replica, key: &SecretKey) -> Fixture {
    let k = &prev.k;
    let (tenant, admin) = workspace(k, "upgraded").await;
    let lead = k.insert(&admin, "Lead", obj(json!({"title": "L1", "api_key": "crm-api-key-1", "hr_token": "hr-token-1"}))).await.unwrap();
    let lead: Uuid = lead["id"].as_str().unwrap().parse().unwrap();
    let bank = k.insert(&admin, "Bank", obj(json!({"title": "B1", "pin": "hr-pin-1"}))).await.unwrap();
    let bank: Uuid = bank["id"].as_str().unwrap().parse().unwrap();
    let begun = k.begin_two_factor(&admin).await.unwrap();
    let totp = begun["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    k.enable_two_factor(&admin, &ferrum_kernel::twofactor::code(&totp, now).unwrap()).await.unwrap();
    let conn = k
        .save_connection_def(
            &admin,
            None,
            &ConnectionInput {
                name: "acme".into(),
                label: "Acme".into(),
                description: Description { test: true, base: "https://api.acme.test".into(), ..Default::default() },
                secrets: Some(BTreeMap::from([("api_key".to_string(), "conn-secret-1".to_string())])),
                enabled: true,
            },
        )
        .await
        .unwrap();
    let channel = k
        .save_message_channel(
            &admin,
            None,
            &ChannelInput {
                kind: "text".into(),
                name: "sms".into(),
                description: ChannelDesc {
                    test: true,
                    inbound: Some(Inbound { auth: "token".into(), header: Some("x-token".into()), secret: "inbound".into(), ..Default::default() }),
                    ..Default::default()
                },
                secrets: Some(BTreeMap::from([("inbound".to_string(), "chan-secret-1".to_string())])),
                enabled: true,
            },
        )
        .await
        .unwrap();
    let webhooks = [Uuid::now_v7(), Uuid::now_v7()];
    let sealed_hook = k.seal_webhook_secret(tenant, webhooks[1], "whsec-sealed-1").unwrap();
    for (id, secret) in [(webhooks[0], "whsec-plain-1".to_string()), (webhooks[1], sealed_hook)] {
        exec(
            k,
            "insert into webhook (tenant_id, id, name, url, secret, topics) values ($1::uuid, $2::uuid, 'h', 'https://hooks.test/x', $3, '{*}')",
            &[&tenant, &id, &secret],
        )
        .await;
    }
    let (sso, saml_enc, request, connector) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let seal = |label: String, plain: &str| key.seal(&label, plain).unwrap();
    exec(
        k,
        "insert into conn_token (tenant_id, conn_id, user_id, sealed) values ($1::uuid, $2::uuid, '00000000-0000-0000-0000-000000000000', $3)",
        &[&tenant, &conn, &seal(format!("conn_token:{tenant}:{conn}:{}", Uuid::nil()), r#"{"access_token":"tok-access-1","refresh_token":"r","kept":{}}"#)],
    )
    .await;
    exec(
        k,
        "insert into sso_provider (tenant_id, id, label, issuer, client_id, client_secret) values ($1::uuid, $2::uuid, 'IdP', 'https://idp.test', 'c', $3)",
        &[&tenant, &sso, &seal(format!("sso|{tenant}|{sso}"), "sso-secret-1")],
    )
    .await;
    exec(
        k,
        "insert into saml_sp_key (tenant_id, private_key, certificate) values ($1::uuid, $2, 'cert')",
        &[&tenant, &seal(format!("saml-sp|{tenant}"), "saml-sp-private-1")],
    )
    .await;
    exec(
        k,
        "insert into saml_enc_key (tenant_id, id, private_key, certificate) values ($1::uuid, $2::uuid, $3, 'cert')",
        &[&tenant, &saml_enc, &seal(format!("saml-enc|{tenant}|{saml_enc}"), "saml-enc-private-1")],
    )
    .await;
    exec(
        k,
        "insert into approval_request (tenant_id, id, entity, record_id, action, from_state, policy, steps, on_action, sealed) \
         values ($1::uuid, $2::uuid, 'Lead', $3::uuid, 'call', '', '{}', '[]', true, jsonb_build_object('pin', $4::text))",
        &[&tenant, &request, &lead, &seal(format!("{tenant}/approval/{request}/pin"), "\"approval-pin-1\"")],
    )
    .await;
    exec(
        k,
        "insert into connection (tenant_id, id, provider, name, sealed) values ($1::uuid, $2::uuid, 'acme', 'Acme', $3)",
        &[&tenant, &connector, &seal(format!("connection:{tenant}:{connector}"), r#"{"token":"connector-secret-1"}"#)],
    )
    .await;
    let vapid = k.vapid().await.unwrap().expect("a push key");
    Fixture { tenant, lead, bank, user: admin.user, totp, conn, channel, webhooks, sso, saml_enc, request, vapid_public: vapid.public.clone() }
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

/// The previous build's view: each old store's value opens with the old key, as its code opens it.
async fn previous_build_reads(k: &Kernel, key: &SecretKey, f: &Fixture) {
    let t = f.tenant;
    let one = |rows: Vec<(String,)>| rows.into_iter().next().unwrap().0;
    for (entity, id, field, plain) in
        [("Lead", f.lead, "api_key", "crm-api-key-1"), ("Lead", f.lead, "hr_token", "hr-token-1"), ("Bank", f.bank, "pin", "hr-pin-1")]
    {
        let q = format!("select sealed from record_secret where tenant_id = $1 and record_id = $2 and field = '{field}'");
        assert_eq!(key.open(&format!("{t}/{entity}/{id}/{field}"), &one(sql(k, &q, &[t, id]).await)).unwrap(), plain, "{entity}.{field}");
    }
    let hook = one(sql(k, "select secret from webhook where tenant_id = $1 and id = $2", &[t, f.webhooks[1]]).await);
    assert_eq!(key.open(&format!("{t}/webhook/{}", f.webhooks[1]), hook.strip_prefix("sealed:").unwrap()).unwrap(), "whsec-sealed-1");
    let conn = one(sql(k, "select sealed from conn where tenant_id = $1 and id = $2", &[t, f.conn]).await);
    assert!(key.open(&format!("conn:{t}:{}", f.conn), &conn).unwrap().contains("conn-secret-1"));
    let chan = one(sql(k, "select sealed from message_channel where tenant_id = $1 and id = $2", &[t, f.channel]).await);
    assert!(key.open(&format!("message_channel:{t}:{}", f.channel), &chan).unwrap().contains("chan-secret-1"));
    let totp = one(sql(k, "select totp_secret from app_user where tenant_id = $1 and id = $2", &[t, f.user]).await);
    assert_eq!(key.open(&format!("totp|{t}|{}", f.user), &totp).unwrap(), f.totp);
    let sso = one(sql(k, "select client_secret from sso_provider where tenant_id = $1 and id = $2", &[t, f.sso]).await);
    assert_eq!(key.open(&format!("sso|{t}|{}", f.sso), &sso).unwrap(), "sso-secret-1");
    let enc = one(sql(k, "select private_key from saml_enc_key where tenant_id = $1 and id = $2", &[t, f.saml_enc]).await);
    assert_eq!(key.open(&format!("saml-enc|{t}|{}", f.saml_enc), &enc).unwrap(), "saml-enc-private-1");
    let pin = one(sql(k, "select sealed->>'pin' from approval_request where tenant_id = $1 and id = $2", &[t, f.request]).await);
    assert_eq!(key.open(&format!("{t}/approval/{}/pin", f.request), &pin).unwrap(), "\"approval-pin-1\"");
    let push = one(sql(k, "select sealed from server_key where name = 'vapid'", &[]).await);
    assert!(key.open("server_key|vapid", &push).is_ok());
}

/// The vault's secrets of the kernel's stores, each with its version: one value moved once has version 1.
async fn kept(k: &Kernel) -> Vec<(String, String, i32)> {
    sql(k, "select owner, name, version from vault.secret where name ~ '^(field|webhook|totp|connector|conn|conn-token|channel|sso|saml-sp|saml-enc|approval|push)[.]' order by 1, 2", &[]).await
}

#[test]
fn the_old_stores_move_into_the_vault() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let key = SecretKey::new([42u8; 32]);
    reset().await;

    // ── an installation the previous build filled: every store sealed with the old key alone ──
    let prev = replica(Some(key.clone()), false).await;
    let f = populate(&prev, &key).await;
    nowhere_in_the_clear(&prev.k, &PLAIN[..5]).await;
    previous_build_reads(&prev.k, &key, &f).await;

    // ── this build, on two replicas, before the move: every value reads from the old stores ──
    let (a, b) = (replica(Some(key.clone()), true).await, replica(Some(key.clone()), true).await);
    reads_every_value(&a, &f, "upgraded").await;
    let waiting: i64 = a.k.vault_waiting().await.unwrap().iter().map(|(_, n)| n).sum();
    assert_eq!(waiting, 15, "every value waits to move: {:?}", a.k.vault_waiting().await.unwrap());

    // ── the move, held half way (a person holding some rows), then finished by both replicas at once ──
    let mut held = a.k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *held).await.unwrap();
    sqlx::query("select 1 from record_secret where record_id = $1 for update").bind(f.lead).execute(&mut *held).await.unwrap();
    sqlx::query("select 1 from conn where id = $1 for update").bind(f.conn).execute(&mut *held).await.unwrap();
    let half = a.k.vault_move().await.unwrap();
    assert_eq!((half.moved, half.left, half.unopened.len()), (12, 3, 0), "the values someone holds are left: {half:?}");
    reads_every_value(&b, &f, "upgraded").await;
    reads_every_value(&a, &f, "upgraded").await;
    held.rollback().await.unwrap();
    let (x, y) = tokio::join!(a.k.vault_move(), b.k.vault_move());
    let (x, y) = (x.unwrap(), y.unwrap());
    assert_eq!(x.moved + y.moved, 3, "the rest moved once between them: {x:?} {y:?}");
    assert!(a.k.vault_waiting().await.unwrap().is_empty());
    let moved = kept(&a.k).await;
    assert_eq!(moved.len(), 15, "{moved:?}");
    assert!(moved.iter().all(|(_, _, v)| *v == 1), "nothing moved twice: {moved:?}");
    let owners: Vec<&str> = moved.iter().filter(|(_, n, _)| n.starts_with("field.")).map(|(o, _, _)| o.as_str()).collect();
    assert_eq!(owners, ["crm", "hr", "hr"], "a field is its declaring app's, the one hr adds to crm's lead included");
    reads_every_value(&a, &f, "upgraded").await;
    reads_every_value(&b, &f, "upgraded").await;
    previous_build_reads(&a.k, &key, &f).await;
    assert_eq!(a.k.vault_move().await.unwrap().moved, 0, "nothing left to move");
    // The plain webhook secret is sealed in its row now, and in the vault.
    let hook: Vec<(String,)> = sql(&a.k, "select secret from webhook where id = $1", &[f.webhooks[0]]).await;
    assert!(hook[0].0.starts_with("sealed:") && key.open(&format!("{}/webhook/{}", f.tenant, f.webhooks[0]), &hook[0].0[7..]).unwrap() == "whsec-plain-1");
    nowhere_in_the_clear(&a.k, PLAIN).await;

    // ── writes by this build reach both stores; one by the previous build is read from then on, and moved again ──
    let admin = ctx(&a.k, "upgraded", f.tenant).await;
    a.k.update(&admin, "Lead", f.lead, obj(json!({"api_key": "crm-api-key-2"})), None).await.unwrap();
    let mut tx = b.k.db.tenant_tx(f.tenant).await.unwrap();
    assert_eq!(b.crm.field(&b.k, &mut tx, f.tenant, "Lead", f.lead, "api_key").await.unwrap().as_deref().map(String::as_str), Some("crm-api-key-2"));
    tx.commit().await.unwrap();
    let old: Vec<(String,)> = sql(&a.k, "select sealed from record_secret where record_id = $1 and field = 'api_key'", &[f.lead]).await;
    assert_eq!(key.open(&format!("{}/Lead/{}/api_key", f.tenant, f.lead), &old[0].0).unwrap(), "crm-api-key-2", "the previous build reads the new value");
    // The previous build sets it (its own code: the old key alone, nothing in the vault).
    prev.k.update(&ctx(&prev.k, "upgraded", f.tenant).await, "Lead", f.lead, obj(json!({"api_key": "crm-api-key-3"})), None).await.unwrap();
    let mut tx = a.k.db.tenant_tx(f.tenant).await.unwrap();
    assert_eq!(a.crm.field(&a.k, &mut tx, f.tenant, "Lead", f.lead, "api_key").await.unwrap().as_deref().map(String::as_str), Some("crm-api-key-3"));
    tx.commit().await.unwrap();
    assert_eq!(a.k.vault_waiting().await.unwrap(), vec![("record_secret.sealed".to_string(), 1)]);
    assert_eq!(a.k.vault_move().await.unwrap().moved, 1);
    assert!(a.k.vault_waiting().await.unwrap().is_empty());
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
    // The deprecated reader still reads any field in this release, and notes who called it.
    #[allow(deprecated)]
    {
        let mut tx = a.k.db.tenant_tx(f.tenant).await.unwrap();
        assert_eq!(a.k.secret(&mut tx, f.tenant, "Bank", f.bank, "pin").await.unwrap().as_deref(), Some("hr-pin-1"));
        tx.commit().await.unwrap();
    }

    // ── rotation at each level: the values still read, and the old key's own rotation reseals its stores ──
    let w = Scope::Workspace(f.tenant);
    a.k.rotate_workspace_key(w).await.unwrap();
    a.k.rotate_owner_key(w, "platform").await.unwrap();
    a.k.rotate_owner_key(w, "crm").await.unwrap();
    assert_eq!(a.k.vault_reseal(w, "platform").await.unwrap(), 0);
    assert_eq!(a.k.vault_reseal(w, "crm").await.unwrap(), 0);
    vault::rotate_installation_key(&a.k.db, None).await.unwrap();
    // Replica b hears of it as it reads (no listener runs here): it opens the vault again, and reads on.
    reads_every_value(&a, &f, "upgraded").await;
    reads_every_value(&b, &f, "upgraded").await;
    let new_key = SecretKey::new([43u8; 32]);
    let rotating = replica_with(Some(new_key.clone()), vec![key.clone()]).await;
    let resealed = rotating.k.reseal_secrets().await.unwrap();
    assert_eq!(resealed.unopened, 0, "{resealed:?}");
    reads_every_value(&rotating, &f, "upgraded").await;
    previous_build_reads_with(&rotating.k, &new_key, &f).await;
    nowhere_in_the_clear(&a.k, PLAIN).await;

    // ── with no old key: the vault alone; nothing is written to the old stores but its mark, and nothing is plain ──
    reset().await;
    let only = replica(None, true).await;
    let (t, admin) = workspace(&only.k, "vaultonly").await;
    let lead = only.k.insert(&admin, "Lead", obj(json!({"title": "L", "api_key": "crm-api-key-9", "hr_token": "hr-token-9"}))).await.unwrap();
    let lead: Uuid = lead["id"].as_str().unwrap().parse().unwrap();
    let begun = only.k.begin_two_factor(&admin).await.unwrap();
    let totp = begun["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    only.k.enable_two_factor(&admin, &ferrum_kernel::twofactor::code(&totp, now).unwrap()).await.unwrap();
    let channel = only
        .k
        .save_message_channel(
            &admin,
            None,
            &ChannelInput {
                kind: "text".into(),
                name: "sms".into(),
                description: ChannelDesc {
                    test: true,
                    inbound: Some(Inbound { auth: "token".into(), header: Some("x-token".into()), secret: "inbound".into(), ..Default::default() }),
                    ..Default::default()
                },
                secrets: Some(BTreeMap::from([("inbound".to_string(), "chan-secret-9".to_string())])),
                enabled: true,
            },
        )
        .await
        .unwrap();
    only.k
        .save_connection_def(
            &admin,
            None,
            &ConnectionInput {
                name: "acme".into(),
                label: "Acme".into(),
                description: Description { test: true, base: "https://api.acme.test".into(), ..Default::default() },
                secrets: Some(BTreeMap::from([("api_key".to_string(), "conn-secret-9".to_string())])),
                enabled: true,
            },
        )
        .await
        .unwrap();
    let hook = Uuid::now_v7();
    let mut tx = only.k.db.tenant_tx(t).await.unwrap();
    let stored = only.k.keep_webhook_secret(&mut tx, t, hook, "whsec-9").await.unwrap();
    sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics) values ($1, $2, 'h', 'https://hooks.test/x', $3, '{*}')")
        .bind(t)
        .bind(hook)
        .bind(&stored)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored, "sealed:vault", "a webhook's secret is never plain again");
    only.k.vapid().await.unwrap().expect("a push key, kept in the vault");
    let marks: Vec<(String,)> = sql(
        &only.k,
        "select sealed from record_secret union all select totp_secret from app_user where totp_secret is not null union all select sealed from message_channel \
         union all select sealed from conn where sealed is not null union all select secret from webhook union all select sealed from server_key",
        &[],
    )
    .await;
    assert!(marks.iter().all(|(m,)| m == "vault" || m == "sealed:vault"), "only the vault's mark: {marks:?}");
    assert_eq!(marks.len(), 7);
    let mut tx = only.k.db.tenant_tx(t).await.unwrap();
    assert_eq!(only.crm.field(&only.k, &mut tx, t, "Lead", lead, "api_key").await.unwrap().as_deref().map(String::as_str), Some("crm-api-key-9"));
    tx.commit().await.unwrap();
    let token = BTreeMap::from([("x-token".to_string(), "chan-secret-9".to_string())]);
    assert!(only.k.message_inbound(channel, &token, &BTreeMap::new(), b"{}").await.is_ok());
    let s = signed(&only.k, "vaultonly").await;
    let later = (now / 30 + 1) * 30;
    only.k
        .second_factor(t, admin.user, s.token_version, &ferrum_kernel::twofactor::code(&totp, later).unwrap())
        .await
        .expect("the second factor, from the vault");
    assert!(only.k.vault_waiting().await.unwrap().is_empty());
    nowhere_in_the_clear(&only.k, &["crm-api-key-9", "hr-token-9", "chan-secret-9", "conn-secret-9", "whsec-9", &totp]).await;
    // Deleting the record and the channel takes their values from the vault.
    only.k.delete(&admin, "Lead", lead, None).await.unwrap();
    only.k.delete_message_channel(&admin, channel).await.unwrap();
    let left: Vec<(String, String, i32)> = kept(&only.k).await;
    assert!(left.iter().all(|(_, n, _)| !n.starts_with("field.") && !n.starts_with("channel.")), "{left:?}");

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
    let first = replica(None, true).await;
    let (tenant, _) = workspace(&first.k, "keyless").await;
    // No key at all in the workspace, as before anything of it was sealed.
    let mut tx = first.k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    for q in ["delete from vault.secret where scope = $1", "delete from vault.key where scope = $1 and owner <> ''", "delete from vault.key where scope = $1"] {
        sqlx::query(q).bind(tenant).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let (a, b) = (replica(None, true).await, replica(None, true).await);
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

/// A replica with the old key `key` and `retired` being rotated out, the vault open.
async fn replica_with(key: Option<SecretKey>, retired: Vec<SecretKey>) -> Replica {
    replica_on(&url(), key, retired, true).await
}

/// [`previous_build_reads`] with the key the old stores are sealed with now, for the values that did not change.
async fn previous_build_reads_with(k: &Kernel, key: &SecretKey, f: &Fixture) {
    let t = f.tenant;
    let pin: Vec<(String,)> = sql(k, "select sealed from record_secret where tenant_id = $1 and record_id = $2 and field = 'pin'", &[t, f.bank]).await;
    assert_eq!(key.open(&format!("{t}/Bank/{}/pin", f.bank), &pin[0].0).unwrap(), "hr-pin-1");
    let conn: Vec<(String,)> = sql(k, "select sealed from conn where tenant_id = $1 and id = $2", &[t, f.conn]).await;
    assert!(key.open(&format!("conn:{t}:{}", f.conn), &conn[0].0).unwrap().contains("conn-secret-1"));
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
    hook: Uuid,
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

async fn moves_and_sandboxes() {
    let (a_url, b_url) = (url(), other_url());
    reset_on(&a_url).await;
    reset_on(&b_url).await;
    for u in [&a_url, &b_url] {
        as_superuser(u, "grant set on parameter session_replication_role to ferrum").await;
    }
    let dirs = std::env::temp_dir().join(format!("ferrum-vault-stores-files-{}", Uuid::now_v7()));
    let (ka, kb) = (SecretKey::new([11u8; 32]), SecretKey::new([12u8; 32]));
    let a = replica_on(&a_url, Some(ka.clone()), vec![], true).await;
    let b = replica_on(&b_url, Some(kb.clone()), vec![], true).await;
    a.k.set_file_store(Arc::new(DirFiles(dirs.join("a"))));
    b.k.set_file_store(Arc::new(DirFiles(dirs.join("b"))));
    let (t, admin) = workspace(&a.k, "mover").await;
    let lead = a.k.insert(&admin, "Lead", obj(json!({"title": "M", "api_key": "crm-mv-1", "hr_token": "hr-mv-1"}))).await.unwrap();
    let lead: Uuid = lead["id"].as_str().unwrap().parse().unwrap();
    let begun = a.k.begin_two_factor(&admin).await.unwrap();
    let totp = begun["secret"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    a.k.enable_two_factor(&admin, &ferrum_kernel::twofactor::code(&totp, now).unwrap()).await.unwrap();
    let channel =
        a.k.save_message_channel(
            &admin,
            None,
            &ChannelInput {
                kind: "text".into(),
                name: "sms".into(),
                description: ChannelDesc {
                    test: true,
                    inbound: Some(Inbound { auth: "token".into(), header: Some("x-token".into()), secret: "inbound".into(), ..Default::default() }),
                    ..Default::default()
                },
                secrets: Some(BTreeMap::from([("inbound".to_string(), "chan-mv-1".to_string())])),
                enabled: true,
            },
        )
        .await
        .unwrap();
    let conn =
        a.k.save_connection_def(
            &admin,
            None,
            &ConnectionInput {
                name: "acme".into(),
                label: "Acme".into(),
                description: Description { test: true, base: "https://api.acme.test".into(), ..Default::default() },
                secrets: Some(BTreeMap::from([("api_key".to_string(), "conn-mv-1".to_string())])),
                enabled: true,
            },
        )
        .await
        .unwrap();
    let hook = Uuid::now_v7();
    let mut tx = a.k.db.tenant_tx(t).await.unwrap();
    let stored = a.k.keep_webhook_secret(&mut tx, t, hook, "whsec-mv-1").await.unwrap();
    sqlx::query("insert into webhook (tenant_id, id, name, url, secret, topics) values ($1, $2, 'h', 'https://hooks.test/x', $3, '{*}')")
        .bind(t)
        .bind(hook)
        .bind(&stored)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let m = Mover { tenant: t, lead, user: admin.user, totp: totp.clone(), channel, conn, hook };
    reads_where_it_is(&a, &m, "mover").await;
    let plain = ["crm-mv-1", "hr-mv-1", "chan-mv-1", "conn-mv-1", "whsec-mv-1", &totp];

    // ── a sandbox: each kind as before, now through the secrets' rules ──
    let until = chrono::Utc::now().date_naive() + chrono::Duration::days(10);
    let copy = Copy { from: None, until, base_domain: None, not_sealed: vec![], resume: false, halt: None };
    a.k.copy_workspace("mover", "mover-try", &copy, &quiet).await.unwrap();
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
    let hook_there: Vec<(String,)> = sql(&a.k, "select secret from webhook where tenant_id = $1", &[s]).await;
    let theirs = ka.open(&format!("{s}/webhook/{hook}"), hook_there[0].0.strip_prefix("sealed:").unwrap()).unwrap();
    assert_ne!(theirs, "whsec-mv-1", "a webhook's secret of its own");
    nowhere_in_the_clear(&a.k, &plain).await;
    a.k.delete_sandbox("mover-try").await.unwrap();

    // ── moved to the other installation: every value reads there, then moves into its vault ──
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
    let transfer = SecretKey::from_base64(&vault::transfer_out(&a.k, t).await.unwrap()).unwrap();
    b.k.move_workspace("mover", &source(&a_url, vec![ka.clone(), transfer], "a"), Run::Start, &quiet).await.unwrap();
    reads_where_it_is(&b, &m, "mover").await;
    let there = b.k.vault_move().await.unwrap();
    assert!(there.moved >= 5 && b.k.vault_waiting().await.unwrap().is_empty(), "{there:?}");
    reads_where_it_is(&b, &m, "mover").await;
    a.k.finish_move("mover", "checked at the target").await.unwrap();
    nowhere_in_the_clear(&b.k, &plain).await;

    // ── and back ──
    let back = SecretKey::from_base64(&vault::transfer_out(&b.k, t).await.unwrap()).unwrap();
    a.k.move_workspace("mover", &source(&b_url, vec![kb.clone(), back], "b"), Run::Start, &quiet).await.unwrap();
    reads_where_it_is(&a, &m, "mover").await;
    b.k.finish_move("mover", "checked back home").await.unwrap();
    let hooked: Vec<(String,)> = sql(&a.k, "select secret from webhook where id = $1", &[m.hook]).await;
    assert_eq!(ka.open(&format!("{t}/webhook/{}", m.hook), hooked[0].0.strip_prefix("sealed:").unwrap()).unwrap(), "whsec-mv-1");
    nowhere_in_the_clear(&a.k, &plain).await;
    nowhere_in_the_clear(&b.k, &plain).await;
    let _ = std::fs::remove_dir_all(&dirs);
}
