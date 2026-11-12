//! Secret parameters, answers given once and secret message values, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). An app's action takes a card's PIN as a secret
//! parameter and gives a new code once; it mails and texts both as secret values; a configured action passes a
//! secret parameter on to a `message` effect; a one-time code goes out. After each round every table is searched
//! for the PIN and the codes: the audit log, the receipts, the outbox and the delivery log, and everything else.

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::messages::{self, ChannelInput, Message, Recipient};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PIN: &str = "Pq-2468-z";
const CODE: &str = "CARD-7777-XYZW";

const PACKAGE: &str = r#"package: vault
version: 0.1.0
depends: [core]
messages:
  - key: vault.pin
    values: [pin, code]
    languages:
      en:
        email: {subject: "Your card {{ values.code }}", body: "<p>PIN {{ values.pin }}</p>"}
        text: {text: "PIN {{ values.pin }} for {{ values.code }}"}
        inapp: {title: "PIN {{ values.pin }}"}
  - key: vault.code
    values: [code]
    languages:
      en:
        text: {text: "Your code is {{ values.code }}"}
"#;
const ENTITIES: &str = r#"
- name: Card
  label: Card
  kind: master
  fields:
    - {name: label, type: data}
  actions:
    - name: redeem
      label: Redeem
      params:
        - {name: pin, label: PIN, secret: true}
        - {name: note, label: Note}
      contract:
        returns: {type: object, properties: {code: {type: string}, ok: {type: boolean}}}
        answer_once: {fields: [code], then: "Make a new code with New Code."}
"#;

static MAILED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Captured;

#[async_trait]
impl ferrum_kernel::Mailer for Captured {
    async fn send(&self, _to: &str, subject: &str, html: &str) -> Result<()> {
        MAILED.lock().unwrap().push(format!("{subject} {html}"));
        Ok(())
    }
}

/// The app: a right PIN gives a new code, mailed and texted to the holder with the PIN; a wrong one is refused
/// saying what it was given, as careless code does.
struct Vault;

#[async_trait]
impl DocHook for Vault {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, _doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "redeem" {
            return Ok(None);
        }
        let pin = params["pin"].as_str().unwrap_or_default();
        if pin != PIN {
            return Err(Error::invalid(format!("the PIN {pin} is not this card's")));
        }
        let m = Message {
            template: "vault.pin".into(),
            to: vec![Recipient {
                user: Some(hc.ctx.user),
                email: Some("holder@example.com".into()),
                phone: Some("+491701234567".into()),
                ..Default::default()
            }],
            channels: vec!["inapp".into(), "email".into(), "text".into()],
            all: true,
            values: Map::from_iter([("pin".to_string(), json!(pin)), ("code".to_string(), json!(CODE))]),
            secret: vec!["pin".into(), "code".into()],
            ..Default::default()
        };
        let sent = messages::send(hc.kernel, hc.tx, hc.ctx, &m).await?;
        Ok(Some(json!({"code": CODE, "ok": true, "sent": sent})))
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn drain(k: &Kernel) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    while k.run_jobs_once(4).await.unwrap() > 0 {}
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

/// The tables any of `secrets` rests in, and how many rows.
async fn rests(k: &Kernel, secrets: &[&str]) -> Vec<String> {
    let like: Vec<String> = secrets.iter().map(|s| format!("%{s}%")).collect();
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("select table_name::text from information_schema.tables where table_schema = 'public' and table_type = 'BASE TABLE' order by 1")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let mut out = Vec::new();
    for table in tables {
        let hits: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from \"{table}\" x where x::text ilike any($1)")))
            .bind(&like)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        if hits > 0 {
            out.push(format!("{table}: {hits}"));
        }
    }
    out
}

#[test]
fn secrets_come_to_rest_nowhere() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/card.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let vault = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let mut hooks = HookRegistry::default();
    hooks.register("Card", Arc::new(Vault));
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), webhooks_allow_private: true, ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, vault]).unwrap(), hooks, settings, Some(Arc::new(Captured))).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "vault".into(),
            name: "vault".into(),
            admin_email: "admin@vault.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["vault".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("vault", "admin@vault.test", "Correct-Horse-9").await.unwrap();
    let admin: Ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let test_channel = ChannelInput {
        kind: "text".into(),
        name: "Test".into(),
        description: serde_json::from_value(json!({"test": true})).unwrap(),
        secrets: None,
        enabled: true,
    };
    k.save_message_channel(&admin, None, &test_channel).await.unwrap();
    let card = k.insert(&admin, "Card", obj(json!({"label": "Gift"}))).await.unwrap();
    let id: Uuid = card["id"].as_str().unwrap().parse().unwrap();
    let nowhere = Vec::<String>::new();

    // ── an action's secret parameter: the handler has it; the audit log, an error and a message do not keep it ──
    let out = k.run_action(&admin, "Card", id, "redeem", json!({"pin": PIN, "note": "at the till"})).await.unwrap();
    assert_eq!(out["code"], CODE, "the caller gets the code");
    let sends: Vec<(String, String, Option<String>)> = out["sent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["channel"].as_str().unwrap().into(), s["state"].as_str().unwrap().into(), s["reason"].as_str().map(Into::into)))
        .collect();
    assert!(sends.contains(&("inapp".into(), "skipped".into(), Some("a secret value is not left in the app".into()))), "{sends:?}");
    assert!(sends.iter().any(|s| s.0 == "email" && s.1 == "queued") && sends.iter().any(|s| s.0 == "text" && s.1 == "queued"), "{sends:?}");
    let e = k.run_action(&admin, "Card", id, "redeem", json!({"pin": "Wrong-1357-q"})).await.unwrap_err();
    assert_eq!(e.to_string(), "the PIN [redacted] is not this card's", "an error never says a secret parameter back");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let audited: Value = sqlx::query_scalar("select diff from audit_event where tenant_id = $1 and action = 'action:redeem' order by at desc limit 1")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(audited, json!({"pin": "[redacted]", "note": "at the till"}), "the audit log says a PIN was given, not which");
    assert_eq!(rests(&k, &[PIN, CODE, "Wrong-1357-q"]).await, nowhere, "while waiting, the message is sealed");
    drain(&k).await;
    let texted = messages::test_deliveries(tenant).into_iter().map(|d| d.1["text"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
    assert!(texted.contains(&format!("PIN {PIN} for {CODE}")), "the text went out with both: {texted:?}");
    assert!(MAILED.lock().unwrap().contains(&format!("Your card {CODE} <p>PIN {PIN}</p>")), "and the mail");
    assert_eq!(rests(&k, &[PIN, CODE]).await, nowhere, "handed over, the outbox and the log hold a placeholder");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let logged: Vec<Value> =
        sqlx::query_scalar("select content from message_send where tenant_id = $1 and channel = 'text'").bind(tenant).fetch_all(&mut *tx).await.unwrap();
    let mailed: Value =
        sqlx::query_scalar("select payload from outbox where tenant_id = $1 and topic = 'ferrum.mail'").bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(logged[0]["message"]["text"], "PIN •••••• for ••••••");
    assert_eq!((mailed["subject"].clone(), mailed["html"].clone(), mailed.get("sealed")), (json!("Your card ••••••"), json!("<p>PIN ••••••</p>"), None));

    // ── under a receipt: the digest is keyed, the code is not kept, a repeat is told ──
    let params = json!({"pin": PIN});
    let target = format!("POST Card/{id}/actions/redeem");
    let digested = k.digestible_params(&admin, "Card", "redeem", &params).unwrap();
    assert_ne!(digested["pin"], json!(PIN));
    let digest = ferrum_kernel::receipt::digest(&target, None, &digested);
    assert_ne!(digest, ferrum_kernel::receipt::digest(&target, None, &params), "not the digest anyone could make by trying PINs");
    let (first, again) = k.run_action_idempotent(&admin, "Card", id, "redeem", params.clone(), "redeem-key-0001", &digest).await.unwrap();
    assert_eq!((first["code"].clone(), again), (json!(CODE), false));
    let (second, again) = k.run_action_idempotent(&admin, "Card", id, "redeem", params.clone(), "redeem-key-0001", &digest).await.unwrap();
    assert!(again && second.get("code").is_none() && second["ok"] == json!(true), "{second}");
    assert!(second["answered_once"].as_str().unwrap().ends_with("Make a new code with New Code."), "{second}");
    // Through the capability gate: the same.
    let inv = || Invocation::new("vault.card.redeem", json!({"id": id, "params": {"pin": PIN}})).key("cap-redeem-1");
    let r = k.invoke(&admin, inv()).await.unwrap();
    assert_eq!((r.result["code"].clone(), r.answered_once.clone()), (json!(CODE), None));
    let r = k.invoke(&admin, inv()).await.unwrap();
    assert!(
        r.repeated && r.result.get("code").is_none() && r.answered_once.as_deref().is_some_and(|m| m.contains("code were given in that first answer only")),
        "{r:?}"
    );
    let cap = k.capability(&admin, "vault.card.redeem").unwrap();
    assert_eq!(cap.input["properties"]["params"]["properties"]["pin"]["writeOnly"], json!(true));
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let kept: String = sqlx::query_scalar("select input_digest from logic_effect where tenant_id = $1 and effect_key = 'cap-redeem-1'")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_ne!(kept, cap.input_digest(&inv().input).unwrap(), "the receipt's digest is keyed");
    let e = k.invoke(&admin, Invocation::new("vault.card.redeem", json!({"id": id, "params": {"pin": "Pq-0000-z"}})).key("cap-redeem-1")).await.unwrap_err();
    assert_eq!(e.code(), "input_changed", "another PIN under the same key is still told apart");
    drain(&k).await;
    assert_eq!(rests(&k, &[PIN, CODE, "Pq-0000-z"]).await, nowhere);

    // ── the `message` effect: a configured action passes a secret parameter on as a secret value ──
    let add = |effects: Value| -> NewCustomization {
        serde_json::from_value(json!({"entity": "Card", "op": {"op": "add_action", "action": {"name": "x_tell", "label": "Tell", "params": [{"name": "pin", "label": "PIN", "secret": true}],
            "effects": effects}}}))
        .unwrap()
    };
    let tell = |secret: Value| json!([{"type": "message", "template": "vault.pin", "to": "expr:{'email': 'other@example.com'}", "values": {"pin": "params.pin", "code": "'NO-CODE'"}, "secret": secret}]);
    let e = k.batch_customizations(&admin, vec![add(tell(json!(["pan"])))], false).await.unwrap_err().1;
    assert!(e.to_string().contains("secret names pan, which is not one of its values"), "{e}");
    k.batch_customizations(&admin, vec![add(tell(json!(["pin"])))], false).await.map_err(|e| e.1).unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    k.run_action(&admin, "Card", id, "x_tell", json!({"pin": "Zz-9753-k"})).await.unwrap();
    assert_eq!(rests(&k, &["Zz-9753-k"]).await, nowhere, "sealed while it waits");
    drain(&k).await;
    assert!(MAILED.lock().unwrap().iter().any(|m| m.contains("PIN Zz-9753-k")));
    assert_eq!(rests(&k, &["Zz-9753-k"]).await, nowhere, "and a placeholder once sent");

    // ── a one-time code: the same mechanism ──
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.send_code(&mut tx, &admin, "vault.guest", "vault.code", &Recipient { phone: Some("+491709876543".into()), ..Default::default() }, "text").await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let pending: Value =
        sqlx::query_scalar("select pending from message_send where tenant_id = $1 and template = 'vault.code'").bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(pending["sealed"].is_string() && pending["message"].is_null() && pending.get("mask").is_none(), "{pending}");
    drain(&k).await;
    let delivered = messages::test_deliveries(tenant).into_iter().rev().find(|d| d.0 == "+491709876543").unwrap();
    assert_eq!(delivered.1["text"].as_str().unwrap().len(), "Your code is 123456".len());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let (pending, content): (Option<Value>, Value) =
        sqlx::query_as("select pending, content from message_send where tenant_id = $1 and template = 'vault.code'")
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((pending, content["message"]["text"].clone()), (None, json!("Your code is ••••••")));
}
