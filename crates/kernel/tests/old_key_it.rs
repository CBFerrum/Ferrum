//! The old master key's last uses (crates/kernel/src/vault/keyed.rs; docs/operations.md, "Retiring the old key"),
//! against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, the public schema and the vault reset first).
//!
//! A new installation with no old key: everything the platform keys, or seals for a while, works with the vault's keys
//! (an opt-out link made and honoured, an erased person's opt-out, a sign-in's state and verifier, a public address of a
//! file, a receipt's digest, a message waiting with a secret value, a plugin's secret setting), and no table holds one
//! of those values in the clear; each still works after the platform's key rotates. An installation upgraded with each
//! made by the previous build's paths (the vault closed, the old key set): the key is adopted into the vault once by two
//! replicas starting together; while it is set, what this build makes is what the previous build checks; and each
//! thing made before still works on two replicas once the key is gone from the environment. With the vault closed,
//! each is refused, saying so, and nothing falls back to the old key once it is retired.

use ferrum_kernel::connections::{self, Call, ConnectionInput};
use ferrum_kernel::files::FileStore;
use ferrum_kernel::messages::{self, ChannelInput, Message, Recipient};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, Opener, Scope, Vault};
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const PACKAGE: &str = r#"package: notes
version: 0.1.0
depends: [core]
messages:
  - key: notes.promo
    category: promotional
    languages:
      en:
        text: {text: "Sale! {{ opt_out }}"}
  - key: notes.code
    values: [code]
    languages:
      en:
        text: {text: "Your code is {{ values.code }}"}
"#;
const ENTITIES: &str = r#"
- name: Note
  label: Note
  kind: master
  fields:
    - {name: title, type: data}
  actions:
    - name: redeem
      label: Redeem
      params:
        - {name: pin, label: PIN, secret: true}
"#;

/// What must never be found in the clear.
const PIN: &str = "Pin-9931-Secret";
const CODE: &str = "Code-5512-Secret";
const SETTING: &str = "Plug-7720-Secret";

/// What the stand-in service was sent: (path, body).
static SENT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// Stored files' bytes, for every replica.
static FILES: Mutex<Option<HashMap<String, Vec<u8>>>> = Mutex::new(None);

struct Memory;

#[async_trait::async_trait]
impl FileStore for Memory {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        FILES.lock().unwrap().get_or_insert_default().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        FILES.lock().unwrap().get_or_insert_default().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        FILES.lock().unwrap().get_or_insert_default().remove(key);
        Ok(())
    }
}

/// A provider's token address and a service that takes a file's public address: each request kept.
async fn stand_in() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
                let (head, body) = loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len || n == 0 {
                            break (text[..end].to_string(), text[end + 4..].to_string());
                        }
                    } else if n == 0 {
                        break (text, String::new());
                    }
                };
                let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                SENT.lock().unwrap().push((path.clone(), body));
                let answer =
                    if path.ends_with("/token") { json!({"access_token": "AT-1", "token_type": "Bearer", "expires_in": 3600}) } else { json!({"id": "M-1"}) };
                let answer = answer.to_string();
                let reply =
                    format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}", answer.len());
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

async fn reset() {
    let db = Db::connect(DbConfig { url: url(), max_connections: 2, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.pool.close().await;
}

fn catalog() -> Catalog {
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/note.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Catalog::from_packages(vec![core, ferrum_meta::catalog::load_package_files(&files).expect("package")]).unwrap()
}

/// A replica: its kernel and the platform's handle its registry gave the server.
struct Replica {
    k: Arc<Kernel>,
    platform: Vault,
}

/// A replica with the old key `key` (none: not in its environment) and the vault opened (`open`) or not, as the
/// previous build has it.
async fn replica(key: Option<&SecretKey>, open: bool) -> Replica {
    let db = Db::connect(DbConfig { url: url(), max_connections: 8, ..Default::default() }).await.expect("connect");
    if open {
        let opener = Opener {
            database_url: url(),
            file: std::env::temp_dir().join("ferrum-old-key-absent.key"),
            kdf: Kdf { m_kib: 64, t: 1, p: 1 },
            ..Default::default()
        };
        vault::open(&db, opener, false).await.expect("open");
    }
    let mut hooks = HookRegistry::default();
    let platform = hooks.platform_vault();
    let settings = KernelSettings {
        secret_key: key.cloned(),
        public_url: Some("https://ferrum.test".into()),
        connection_hosts: vec!["127.0.0.1".into()],
        ..Default::default()
    };
    let k = Kernel::boot(db, catalog(), hooks, settings, None).await.expect("boot");
    k.set_file_store(Arc::new(Memory));
    Replica { k, platform }
}

/// What one workspace has: its administrator, its connection (signs in to an account; hands files out by address).
struct Place {
    tenant: Uuid,
    admin: Ctx,
    conn: Uuid,
}

async fn workspace(k: &Kernel, slug: &str, port: u16) -> Place {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    k.update_tenant_settings(&admin, json!({"messages": {"calling_code": "+49"}}).as_object().unwrap()).await.unwrap();
    let channel = ChannelInput {
        kind: "text".into(),
        name: "Test".into(),
        description: serde_json::from_value(json!({"test": true})).unwrap(),
        secrets: None,
        enabled: true,
    };
    k.save_message_channel(&admin, None, &channel).await.unwrap();
    let base = format!("http://127.0.0.1:{port}/v1");
    let desc = json!({"base": base, "auth": {"type": "oauth", "provider": {"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/token"), "client_id": "ferrum-test"}},
        "operations": {"media": {"method": "POST", "path": "/media", "body": "{\"url\": {{ public_files.photo | json }}}", "public_files": {"photo": {"file": "{{ inputs.photo }}"}}}}});
    let input: ConnectionInput = serde_json::from_value(json!({"name": "net", "label": "Net", "description": desc, "enabled": true})).unwrap();
    let conn = k.save_connection_def(&admin, None, &input).await.unwrap();
    Place { tenant, admin, conn }
}

fn promo(to: &str) -> Message {
    Message {
        template: "notes.promo".into(),
        to: vec![Recipient { phone: Some(to.into()), ..Default::default() }],
        channels: vec!["text".into()],
        ..Default::default()
    }
}

/// A promotional message's opt-out link, as rendered for `to`; its token.
async fn opt_out_link(k: &Kernel, p: &Place, to: &str) -> Result<String> {
    let mut tx = k.db.tenant_tx(p.tenant).await?;
    let r = messages::render_for(k, &mut tx, &p.admin, &promo(to), "text").await;
    tx.commit().await?;
    Ok(r?.opt_out.expect("an opt-out link").rsplit("token=").next().unwrap().to_string())
}

async fn opted_out(k: &Kernel, p: &Place, to: &str) -> bool {
    let mut tx = k.db.tenant_tx(p.tenant).await.unwrap();
    let out = k.opted_out(&mut tx, p.tenant, "text", to).await.unwrap();
    tx.commit().await.unwrap();
    out
}

/// A message with a secret value, queued (not delivered).
async fn waiting(k: &Kernel, p: &Place, to: &str) -> Result<Uuid> {
    let m = Message {
        template: "notes.code".into(),
        to: vec![Recipient { phone: Some(to.into()), ..Default::default() }],
        channels: vec!["text".into()],
        values: Map::from_iter([("code".to_string(), json!(CODE))]),
        secret: vec!["code".into()],
        ..Default::default()
    };
    let mut tx = k.db.tenant_tx(p.tenant).await?;
    let sent = messages::send(k, &mut tx, &p.admin, &m).await;
    tx.commit().await?;
    Ok(sent?[0].id)
}

/// Runs every job due; what the test channel was handed for `to`.
async fn delivered(k: &Kernel, p: &Place, to: &str) -> Vec<String> {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    while k.run_jobs_once(4).await.unwrap() > 0 {}
    messages::test_deliveries(p.tenant).into_iter().filter(|d| d.0 == to).map(|d| d.1["text"].as_str().unwrap_or_default().to_string()).collect()
}

/// A sign-in started from `host`: its state.
async fn sign_in(k: &Kernel, p: &Place, session: Uuid) -> Result<String> {
    let at = k.connect_start_on(&p.admin, p.conn, session, Some("erp.acme.test")).await?;
    let state = reqwest::Url::parse(&at).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    Ok(state)
}

/// A public address of a stored file, as the service was handed it: (id, signature).
async fn address(k: &Kernel, p: &Place, key: &str) -> Result<(Uuid, String)> {
    let file = k.store_file(&p.admin, None, "photo.jpg", "image/jpeg", b"JPEG-0123").await?["id"].as_str().unwrap().to_string();
    let call = Call { connection: "net".into(), operation: "media".into(), inputs: json!({"photo": file}), key: Some(key.into()), ..Default::default() };
    connections::call(k, &p.admin, None, &call).await?;
    let body = SENT.lock().unwrap().iter().rev().find(|s| s.0.ends_with("/media")).unwrap().1.clone();
    let handed = serde_json::from_str::<Value>(&body).unwrap()["url"].as_str().unwrap().to_string();
    let (at, sig) = handed.split_once("?s=").unwrap();
    Ok((at.rsplit('/').next().unwrap().parse().unwrap(), sig.to_string()))
}

async fn digest(k: &Kernel, p: &Place) -> Result<String> {
    Ok(k.digestible_params(&p.admin, "Note", "redeem", &json!({"pin": PIN})).await?["pin"].as_str().unwrap().to_string())
}

/// Every table of the database, the vault's too, searched for each of `plain` as text in any column: a dump's view.
async fn nowhere_in_the_clear(k: &Kernel, plain: &[&str]) {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "select table_schema, table_name, column_name from information_schema.columns c join information_schema.tables t using (table_schema, table_name) \
         where table_schema in ('public', 'vault') and t.table_type = 'BASE TABLE' and data_type in ('text', 'jsonb', 'json', 'bytea', 'character varying', 'ARRAY', 'USER-DEFINED')",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let patterns: Vec<String> = plain.iter().map(|p| format!("%{p}%")).collect();
    for (schema, table, column) in columns {
        let q = format!("select string_agg(\"{column}\"::text, ' | ') from \"{schema}\".\"{table}\" where \"{column}\"::text like any($1)");
        let found: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(q)).bind(&patterns).fetch_one(&mut *tx).await.unwrap();
        assert!(found.is_none(), "a value is in the clear in {schema}.{table}.{column}");
    }
    tx.commit().await.unwrap();
}

async fn adopted_rows(k: &Kernel) -> i64 {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let n = sqlx::query_scalar("select count(*) from vault.secret where owner = 'platform' and name like 'legacy.key.%'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn stored(k: &Kernel, sql: &str, id: Uuid) -> String {
    let mut tx = k.db.system_tx().await.unwrap();
    let v: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(id).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    v
}

fn closed(r: Result<impl std::fmt::Debug>) -> bool {
    matches!(&r, Err(Error::Unavailable(m)) if m.contains("needs the vault"))
}

#[test]
fn the_old_key_goes() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let port = stand_in().await;
    fresh(port).await;
    upgraded(port).await;
}

/// A new installation: no old key anywhere.
async fn fresh(port: u16) {
    reset().await;
    // ── the vault closed: each is refused, saying so ──
    let shut = replica(None, false).await;
    let p = workspace(&shut.k, "shut", port).await;
    assert!(closed(opt_out_link(&shut.k, &p, "+4915199990").await), "an opt-out link");
    let forged = format!("{}.{}", base64_of(&format!("{}|text|+4915199990", p.tenant)), "0".repeat(64));
    assert!(closed(shut.k.opt_out_by_link(&forged).await), "checking one");
    assert!(closed(waiting(&shut.k, &p, "+4915199991").await), "a message with a secret value");
    assert!(closed(sign_in(&shut.k, &p, Uuid::now_v7()).await), "a sign-in");
    assert!(closed(digest(&shut.k, &p).await), "a receipt");
    let kept = shut.platform.keep_plugin_setting(&shut.k, p.tenant, "gateway", "api_key", &json!(SETTING)).await;
    let said = format!("{kept:?}");
    assert!(closed(kept), "a plugin's setting: {said}");
    shut.k.set_message_opt_out(&p.admin, "+491515555550", "text", true).await.unwrap();
    let zoe = ferrum_kernel::privacy::Person { email: "zoe@example.com".into(), phone: Some("+491515555550".into()), ..Default::default() };
    assert!(closed(shut.k.erase_personal_data_of(&p.admin, &zoe, "request 1").await), "an erased opt-out");
    let file: Uuid = shut.k.store_file(&p.admin, None, "x.jpg", "image/jpeg", b"x").await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let mut tx = shut.k.db.system_tx().await.unwrap();
    let addr = Uuid::now_v7();
    sqlx::query("insert into conn_file_address (tenant_id, id, conn_id, operation, name, file_id, expires_at) values ($1, $2, $3, 'media', 'photo', $4, now() + interval '1 hour')")
        .bind(p.tenant)
        .bind(addr)
        .bind(p.conn)
        .bind(file)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(closed(shut.k.pulled_file(addr, &"0".repeat(64)).await), "a public address");
    drop(shut);

    // ── the vault open: each works with the vault's keys ──
    reset().await;
    let (a, b) = (replica(None, true).await, replica(None, true).await);
    assert!(a.k.settings.secret_key.is_some(), "apps that still read the key are given one of the platform's");
    assert_eq!(a.k.settings.secret_key.as_ref().map(|k| &k.id), b.k.settings.secret_key.as_ref().map(|k| &k.id), "the same on every replica");
    let p = workspace(&a.k, "fresh", port).await;
    // An opt-out link, made on one replica and honoured on the other.
    let token = opt_out_link(&a.k, &p, "+4915199990").await.unwrap();
    assert!(b.k.opt_out_by_link(&format!("{token}0")).await.is_err(), "a changed link does nothing");
    assert_eq!(b.k.opt_out_by_link(&token).await.unwrap(), "text");
    assert!(opted_out(&a.k, &p, "+4915199990").await);
    let (floors, _) = a.k.floors().await.unwrap();
    assert!(floors.iter().any(|f| f.feature == vault::OLD_KEY_FLOOR.feature), "builds that check with the old key only are refused from then on");
    // An erased person's opt-out: kept under the workspace's key, and still holds.
    a.k.set_message_opt_out(&p.admin, "+491515555550", "text", true).await.unwrap();
    a.k.erase_personal_data_of(&p.admin, &zoe, "request 1").await.unwrap();
    assert!(opted_out(&b.k, &p, "+491515555550").await, "an erased opt-out holds");
    let listed = a.k.message_opt_outs(&p.admin, None, None, 50, 0).await.unwrap();
    assert!(listed["opt_outs"].as_array().unwrap().iter().any(|r| r["erased"].as_str().is_some_and(|m| m.starts_with("erased:v"))), "{listed}");
    // A sign-in: its state names where it started, checked on the other replica; its verifier opens there.
    let session = Uuid::now_v7();
    let state = sign_in(&a.k, &p, session).await.unwrap();
    assert_eq!(b.k.connection_state_host(&state).await, Some((p.tenant, "erp.acme.test".to_string())));
    let verifier = stored(&a.k, "select verifier from conn_signin where tenant_id is not null and conn_id = $1", p.conn).await;
    assert!(verifier.starts_with("fv1."), "sealed under the platform's key: {verifier}");
    assert_eq!(b.k.connect_finish(&p.admin, session, &state, Some("CODE-1"), None).await.unwrap(), p.conn);
    // A public address of a file, pulled on the other replica.
    let (id, sig) = address(&a.k, &p, "k-1").await.unwrap();
    assert_eq!(b.k.pulled_file(id, &sig).await.unwrap().tenant, p.tenant);
    assert!(b.k.pulled_file(id, &"0".repeat(64)).await.is_err(), "another signature does nothing");
    // A receipt's digest: the same on every replica, and not the value.
    let d = digest(&a.k, &p).await.unwrap();
    assert_eq!((d.len(), d == digest(&b.k, &p).await.unwrap()), (64, true));
    // A message with a secret value waits sealed, and is delivered by the other replica.
    let sent = waiting(&a.k, &p, "+4915112345").await.unwrap();
    let pending = stored(&a.k, "select pending->>'sealed' from message_send where id = $1", sent).await;
    assert!(pending.starts_with("fv1."), "{pending}");
    // A plugin's secret setting, kept under the plugin's key, opened by the other replica.
    let kept = a.platform.keep_plugin_setting(&a.k, p.tenant, "gateway", "api_key", &json!(SETTING)).await.unwrap();
    assert!(kept.get("$vault").is_some() && kept.get("$sealed").is_none() && !kept.to_string().contains(SETTING), "{kept}");
    assert_eq!(b.platform.plugin_setting(&b.k, p.tenant, "gateway", "api_key", &kept).await.unwrap(), Some(json!(SETTING)));
    assert!(matches!(a.platform.plugin("gateway").unwrap().keep_plugin_setting(&a.k, p.tenant, "gateway", "x", &json!(1)).await, Err(Error::Forbidden(_))));
    nowhere_in_the_clear(&a.k, &[PIN, CODE, SETTING, "1515555550"]).await;
    assert!(delivered(&b.k, &p, "+4915112345").await.contains(&format!("Your code is {CODE}")));

    // ── the platform's key rotated: what was made before still works; what is made after uses the new version ──
    let s2 = Uuid::now_v7();
    let state2 = sign_in(&a.k, &p, s2).await.unwrap();
    let sent2 = waiting(&a.k, &p, "+4915112346").await.unwrap();
    let w = Scope::Workspace(p.tenant);
    a.k.rotate_workspace_key(w).await.unwrap();
    assert_eq!(a.k.rotate_owner_key(w, "platform").await.unwrap(), 2);
    a.k.rotate_owner_key(w, "plugin:gateway").await.unwrap();
    a.k.rotate_owner_key(Scope::Installation, "platform").await.unwrap();
    vault::rotate_installation_key(&a.k.db, None).await.unwrap();
    let c = replica(None, true).await;
    assert_eq!(c.k.opt_out_by_link(&token).await.unwrap(), "text", "a link from before");
    assert!(opted_out(&c.k, &p, "+491515555550").await, "an erased opt-out from before");
    assert_eq!(c.k.pulled_file(id, &sig).await.unwrap().tenant, p.tenant, "an address from before");
    assert_eq!(c.k.connection_state_host(&state2).await, Some((p.tenant, "erp.acme.test".to_string())), "a sign-in from before");
    assert_eq!(c.k.connect_finish(&p.admin, s2, &state2, Some("CODE-1"), None).await.unwrap(), p.conn);
    assert_eq!(c.platform.plugin_setting(&c.k, p.tenant, "gateway", "api_key", &kept).await.unwrap(), Some(json!(SETTING)), "a setting from before");
    assert!(stored(&a.k, "select pending->>'sealed' from message_send where id = $1", sent2).await.starts_with("fv1.1."));
    assert!(delivered(&c.k, &p, "+4915112346").await.contains(&format!("Your code is {CODE}")), "a message waiting from before");
    let token2 = opt_out_link(&c.k, &p, "+4915199980").await.unwrap();
    assert_eq!(a.k.opt_out_by_link(&token2).await.unwrap(), "text", "a link made with the new version, checked where the old one was current");
    assert_ne!(digest(&c.k, &p).await.unwrap(), d, "a receipt's digest is made with the current version only");
    let sent3 = waiting(&c.k, &p, "+4915112347").await.unwrap();
    assert!(stored(&c.k, "select pending->>'sealed' from message_send where id = $1", sent3).await.starts_with("fv1.2."));
    nowhere_in_the_clear(&c.k, &[PIN, CODE, SETTING, "1515555550"]).await;
}

/// An installation the previous build ran, with the old key: each thing made by its paths, then this build.
async fn upgraded(port: u16) {
    reset().await;
    let old = SecretKey::new([7u8; 32]);
    // ── the previous build's paths: the vault closed, the key set ──
    let prev = replica(Some(&old), false).await;
    let p = workspace(&prev.k, "acme", port).await;
    let token = opt_out_link(&prev.k, &p, "+4915199990").await.unwrap();
    prev.k.set_message_opt_out(&p.admin, "+491515555550", "text", true).await.unwrap();
    let zoe = ferrum_kernel::privacy::Person { email: "zoe@example.com".into(), phone: Some("+491515555550".into()), ..Default::default() };
    prev.k.erase_personal_data_of(&p.admin, &zoe, "request 1").await.unwrap();
    let session = Uuid::now_v7();
    let state = sign_in(&prev.k, &p, session).await.unwrap();
    // The connection is signed in once, for the file's call; the state above stays open.
    let s0 = Uuid::now_v7();
    let first = sign_in(&prev.k, &p, s0).await.unwrap();
    prev.k.connect_finish(&p.admin, s0, &first, Some("CODE-1"), None).await.unwrap();
    let (id, sig) = address(&prev.k, &p, "k-1").await.unwrap();
    let sent = waiting(&prev.k, &p, "+4915112345").await.unwrap();
    let setting = prev.platform.keep_plugin_setting(&prev.k, p.tenant, "gateway", "api_key", &json!(SETTING)).await.unwrap();
    // What the previous build's code makes: each as it was, under the old key.
    let said = format!("{}|text|+4915199990", p.tenant);
    assert_eq!(token, format!("{}.{}", base64_of(&said), old.fingerprint("message opt-out", &said)), "an opt-out link");
    assert_eq!(sig, old.fingerprint("pulled-file", &id.to_string()), "a file's address");
    assert!(setting.get("$vault").is_none() && old.open(&format!("{}/plugin/gateway/api_key", p.tenant), setting["$sealed"].as_str().unwrap()).is_ok());
    drop(prev);

    // ── upgraded, the key still set: two replicas start together, the key is adopted once ──
    let (a, b) = tokio::join!(replica(Some(&old), true), replica(Some(&old), true));
    assert_eq!(adopted_rows(&a.k).await, 1, "adopted once");
    let events = vault::events(&a.k.db, None, 500).await.unwrap();
    assert_eq!(events.iter().filter(|e| e.action == "adopt" && e.details["by"] == "old key").count(), 1, "and recorded once, never its value");
    let l = a.k.legacy_key(vec![]).await.unwrap();
    assert_eq!(
        (l.configured, l.adopted.clone(), l.not_adopted.is_empty(), l.older_builds_refused, l.retirable),
        (true, vec![old.id.clone()], true, false, false)
    );
    // While older builds may run, what this build makes is what the previous build's paths check.
    let token1 = opt_out_link(&a.k, &p, "+4915199991").await.unwrap();
    let said1 = format!("{}|text|+4915199991", p.tenant);
    assert_eq!(token1, format!("{}.{}", base64_of(&said1), old.fingerprint("message opt-out", &said1)), "the previous build honours it");
    let (id1, sig1) = address(&a.k, &p, "k-2").await.unwrap();
    assert_eq!(sig1, old.fingerprint("pulled-file", &id1.to_string()), "and pulls it");
    let sent1 = waiting(&a.k, &p, "+4915112346").await.unwrap();
    let sealed1 = stored(&a.k, "select pending->>'sealed' from message_send where id = $1", sent1).await;
    assert!(old.open(&format!("{}/message", p.tenant), &sealed1).is_ok(), "and sends it");
    let setting1 = a.platform.keep_plugin_setting(&a.k, p.tenant, "gateway", "api_key", &json!(SETTING)).await.unwrap();
    assert!(setting1.get("$vault").is_some() && old.open(&format!("{}/plugin/gateway/api_key", p.tenant), setting1["$sealed"].as_str().unwrap()).is_ok());
    let s1 = Uuid::now_v7();
    let state1 = sign_in(&a.k, &p, s1).await.unwrap();
    let (nonce, rest) = state1.split_once('.').unwrap();
    let (named, mac) = rest.split_once('.').unwrap();
    assert_eq!(mac, old.fingerprint("connection sign-in state", &format!("{nonce}.{named}")), "and its sign-ins");
    let (floors, _) = a.k.floors().await.unwrap();
    assert!(!floors.iter().any(|f| f.feature == vault::OLD_KEY_FLOOR.feature), "older builds still start");
    // Moved, then retired: older builds refused from now on.
    assert!(matches!(a.k.retire_old_key().await, Err(Error::Conflict(_))), "not before everything moved");
    // Both replicas move at once: a value one holds, the other leaves for its next round.
    let (ma, mb) = tokio::join!(a.k.vault_move(), b.k.vault_move());
    let (ma, mb) = (ma.unwrap(), mb.unwrap());
    assert!(ma.moved + mb.moved > 0, "{ma:?} {mb:?}");
    assert_eq!(a.k.vault_move().await.unwrap().left, 0);
    assert!(a.k.vault_waiting().await.unwrap().is_empty());
    let l = a.k.retire_old_key().await.unwrap();
    assert!(l.older_builds_refused && l.retirable, "{l:?}");

    // ── the key gone from the environment: everything made with it still works, on two replicas ──
    drop((a, b));
    let (c, d) = tokio::join!(replica(None, true), replica(None, true));
    assert_eq!(c.k.settings.secret_key.as_ref().map(|k| k.id.clone()), Some(old.id.clone()), "apps that read the key are given the vault's copy");
    for (r, t) in [(&c, &token), (&d, &token1)] {
        assert_eq!(r.k.opt_out_by_link(t).await.unwrap(), "text", "a link the old key made");
    }
    assert!(opted_out(&d.k, &p, "+491515555550").await, "an erased opt-out the old key made");
    assert_eq!(c.k.connection_state_host(&state).await, Some((p.tenant, "erp.acme.test".to_string())), "a sign-in the old key started");
    assert_eq!(d.k.connect_finish(&p.admin, session, &state, Some("CODE-1"), None).await.unwrap(), p.conn);
    assert_eq!(c.k.connect_finish(&p.admin, s1, &state1, Some("CODE-1"), None).await.unwrap(), p.conn);
    assert_eq!(c.k.pulled_file(id, &sig).await.unwrap().tenant, p.tenant, "an address the old key signed");
    assert_eq!(d.k.pulled_file(id1, &sig1).await.unwrap().tenant, p.tenant);
    for s in [&setting, &setting1] {
        assert_eq!(d.platform.plugin_setting(&d.k, p.tenant, "gateway", "api_key", s).await.unwrap(), Some(json!(SETTING)), "a setting the old key sealed");
    }
    let got = delivered(&c.k, &p, "+4915112345").await;
    assert!(got.contains(&format!("Your code is {CODE}")), "a message the old key sealed: {got:?} ({sent})");
    assert!(delivered(&d.k, &p, "+4915112346").await.contains(&format!("Your code is {CODE}")));
    // What is made now is made with the vault's keys.
    assert!(!opt_out_link(&c.k, &p, "+4915199992").await.unwrap().ends_with(&old.fingerprint("message opt-out", &format!("{}|text|+4915199992", p.tenant))));
    let l = c.k.legacy_key(vec![]).await.unwrap();
    assert_eq!((l.configured, l.adopted.clone(), l.older_builds_refused), (false, vec![old.id.clone()], true));
    assert_eq!(adopted_rows(&c.k).await, 1);
    nowhere_in_the_clear(&c.k, &[PIN, CODE, SETTING, "1515555550"]).await;

    // ── the vault closed once the old key is retired: nothing falls back to it ──
    let shut = replica(Some(&old), false).await;
    assert!(closed(opt_out_link(&shut.k, &p, "+4915199993").await), "an opt-out link");
    assert!(closed(waiting(&shut.k, &p, "+4915112347").await), "a message with a secret value");
}

fn base64_of(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}
