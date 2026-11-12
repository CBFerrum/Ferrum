//! Connections (crates/kernel/src/connections, BD-28 to BD-32) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`;
//! the database is reset first) and a stand-in service on 127.0.0.1, the one host the operator names. Every guard is
//! tried: addresses that are not public, schemes, redirects, time and size limits, retries only for "later", one call
//! per key, who may use a connection, and secrets resting nowhere, with the debug capture on.

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::connections::inbound::InboundHandler;
use ferrum_kernel::connections::{self, Call, ConnectionInput};
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const SECRET: &str = "sk-Zq81-connection-secret";
const SIGNING: &str = "whsec-Pm42-signing-secret";
const INBOUND: &str = "whsec-In77-inbound-secret";
const TOKEN: &str = "tok-Qq19-shared-inbound";

const PACKAGE: &str = "package: depot\nversion: 0.1.0\ndepends: [core]\n";
const ENTITIES: &str = r#"
- name: Parcel
  label: Parcel
  kind: master
  fields:
    - {name: label, type: data}
    - {name: weight, type: int}
    - {name: tracking, type: data}
- name: Alert
  label: Alert
  kind: master
  fields:
    - {name: external_id, type: data}
    - {name: title, type: data, required: true}
    - {name: state, type: data}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

/// What the stand-in service was sent: each request, raw.
type Seen = Arc<Mutex<Vec<String>>>;

/// The stand-in service: answers by path.
async fn service(seen: Seen) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let flaky = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let (seen, flaky) = (seen.clone(), flaky.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                seen.lock().unwrap().push(text.clone());
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let auth = text.lines().find_map(|l| l.strip_prefix("authorization: ").or(l.strip_prefix("x-api-key: "))).unwrap_or("").to_string();
                let (status, extra, body) = match path.split('?').next().unwrap() {
                    "/v1/labels" => (201, String::new(), json!({"id": "L-1", "tracking": "TRK-123", "cost": {"amount": 7.5}}).to_string()),
                    "/v1/echo" => (400, String::new(), json!({"error": {"message": format!("the key {auth} is wrong")}}).to_string()),
                    "/v1/slow" => {
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        (200, String::new(), "{}".into())
                    }
                    "/v1/big" => (200, String::new(), format!("\"{}\"", "x".repeat(5000))),
                    "/v1/flaky" => match flaky.fetch_add(1, Ordering::SeqCst) {
                        0 => (503, String::new(), json!({"error": {"message": "busy"}}).to_string()),
                        _ => (200, String::new(), json!({"id": "F-1"}).to_string()),
                    },
                    "/v1/refuse" => (422, String::new(), json!({"error": {"message": "weight missing"}}).to_string()),
                    "/v1/hop" => (307, "location: /v1/ok\r\n".into(), String::new()),
                    "/v1/hop-private" => (302, "location: http://10.0.0.1/v1/ok\r\n".into(), String::new()),
                    "/v1/hop-localhost" => (302, format!("location: http://localhost:{}/v1/ok\r\n", s.local_addr().unwrap().port()), String::new()),
                    "/v1/ok" => (200, String::new(), json!({"ok": true}).to_string()),
                    _ => (404, String::new(), "{}".into()),
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
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

async fn drain(k: &Kernel) {
    for _ in 0..5 {
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        while k.run_jobs_once(4).await.unwrap() > 0 {}
    }
}

fn input(name: &str, desc: Value, secrets: Option<Value>) -> ConnectionInput {
    serde_json::from_value(json!({"name": name, "label": name, "description": desc, "secrets": secrets, "enabled": true})).unwrap()
}

fn call(conn: &str, op: &str, inputs: Value, key: Option<&str>) -> Call {
    Call { connection: conn.into(), operation: op.into(), inputs, key: key.map(Into::into), ..Default::default() }
}

/// The app's own code for what a connection accepts: a parcel per call.
struct Depot;

#[async_trait]
impl InboundHandler for Depot {
    async fn handle(&self, k: &Kernel, tx: &mut ferrum_kernel::db::Tx, ctx: &Ctx, connection: &str, payload: &Value) -> ferrum_kernel::Result<()> {
        let label = format!("{connection}:{}", payload["ref"].as_str().unwrap_or_default());
        k.insert_tx(ctx, tx, "Parcel", obj(json!({"label": label}))).await?;
        Ok(())
    }
}

fn sign(secret: &str, ts: i64, body: &str) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{ts}.{body}").as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn headers(pairs: &[(&str, String)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

async fn count(k: &Kernel, tenant: Uuid, sql: &str) -> i64 {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let n = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// Being called: verified, recorded once, processed once, onto records or by the app.
async fn being_called(k: &Kernel, admin: &Ctx) {
    let tenant = admin.tenant;
    let (alert, parcel) = (admin.schema.entity("Alert").unwrap().table.clone(), admin.schema.entity("Parcel").unwrap().table.clone());
    let alerts = json!({"test": true, "inbound": {
        "verify": {"type": "signature", "header": "x-signature", "secret": "hook", "timestamp_header": "x-timestamp", "prefix": "sha256="},
        "delivery_header": "x-delivery",
        "map": {"entity": "Alert", "items_at": "/alerts", "by": {"external_id": "item.id"}, "set": {"title": "item.summary", "state": "item.state"}}}});
    let id = k.save_connection_def(admin, None, &input("monitor", alerts, Some(json!({"hook": INBOUND})))).await.unwrap();
    let now = chrono::Utc::now().timestamp();
    let post = |delivery: &str, ts: i64, body: &str, secret: &str| {
        let h = headers(&[("X-Signature", sign(secret, ts, body)), ("X-Timestamp", ts.to_string()), ("X-Delivery", delivery.to_string())]);
        let body = body.to_string();
        async move { k.connection_inbound(id, &h, body.as_bytes()).await }
    };
    let one = r#"{"alerts": [{"id": "A-1", "summary": "Disk full", "state": "firing"}, {"id": "A-2", "summary": "CPU hot", "state": "firing"}]}"#;
    assert_eq!(post("d-1", now, one, INBOUND).await.unwrap(), json!({"accepted": true}));
    drain(k).await;
    assert_eq!(count(k, tenant, &format!("select count(*) from {alert} where tenant_id = $1 and state = 'firing'")).await, 2, "an alert per item, no app code");
    let resolved = r#"{"alerts": [{"id": "A-1", "summary": "Disk full", "state": "resolved"}]}"#;
    post("d-2", now, resolved, INBOUND).await.unwrap();
    drain(k).await;
    assert_eq!(count(k, tenant, &format!("select count(*) from {alert} where tenant_id = $1")).await, 2, "found by its key and changed, not made again");
    assert_eq!(count(k, tenant, &format!("select count(*) from {alert} where tenant_id = $1 and external_id = 'A-1' and state = 'resolved'")).await, 1);
    assert_eq!(
        count(k, tenant, "select count(*) from conn_inbound where tenant_id = $1 and state = 'done' and payload is null").await,
        2,
        "processed, its payload dropped"
    );

    // Every refusal is the same answer, and counted: a replay, a wrong signature, a stale one, an unsigned one, one too large, an unknown address.
    let alike = Error::Unauthorized("this call was not accepted".into()).to_string();
    let refusals = [
        post("d-1", now, one, INBOUND).await.unwrap_err(),
        post("d-3", now, one, "whsec-wrong").await.unwrap_err(),
        post("d-4", now - 3600, one, INBOUND).await.unwrap_err(),
        k.connection_inbound(id, &headers(&[("X-Delivery", "d-5".into())]), one.as_bytes()).await.unwrap_err(),
        post("d-6", now, &json!({"alerts": [], "pad": "x".repeat(300 << 10)}).to_string(), INBOUND).await.unwrap_err(),
    ];
    assert!(refusals.iter().all(|e| e.to_string() == alike), "{refusals:?}");
    let unknown_before = ferrum_kernel::connections::inbound::refused_unknown();
    assert_eq!(k.connection_inbound(Uuid::now_v7(), &BTreeMap::new(), b"{}").await.unwrap_err().to_string(), alike);
    assert_eq!(ferrum_kernel::connections::inbound::refused_unknown(), unknown_before + 1);
    let refused = k.connection_defs(admin).await.unwrap().into_iter().find(|c| c["name"] == "monitor").unwrap()["refused"].clone();
    assert_eq!(refused, json!(5));
    // Without a delivery id, what was signed stands for it: the same call again is a replay.
    let h = headers(&[("X-Signature", sign(INBOUND, now, one)), ("X-Timestamp", now.to_string())]);
    k.connection_inbound(id, &h, one.as_bytes()).await.unwrap();
    assert_eq!(k.connection_inbound(id, &h, one.as_bytes()).await.unwrap_err().to_string(), alike);
    // A payload the mapping cannot take is marked failed, saying why, and not tried again.
    let blank = r#"{"alerts": [{"id": "A-9", "state": "firing"}]}"#;
    post("d-7", now, blank, INBOUND).await.unwrap();
    drain(k).await;
    let failed = k.connection_inbound_log(admin, id, 10).await.unwrap().into_iter().find(|r| r["delivery"] == "d-7").unwrap();
    assert!(failed["state"] == "failed" && failed["error"].as_str().unwrap().contains("item 1"), "{failed}");

    // A form, a shared token, and the app that registered for the connection.
    let shop = json!({"test": true, "apps": ["depot"], "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "token"}, "delivery_at": "/order", "handler": "depot"}});
    let shop = k.save_connection_def(admin, None, &input("shop", shop, Some(json!({"token": TOKEN})))).await.unwrap();
    let form = headers(&[("X-Token", TOKEN.into()), ("Content-Type", "application/x-www-form-urlencoded".into())]);
    k.connection_inbound(shop, &form, b"order=O-1&ref=web").await.unwrap();
    assert_eq!(k.connection_inbound(shop, &form, b"order=O-1&ref=web").await.unwrap_err().to_string(), alike, "the same order again");
    let wrong = headers(&[("X-Token", "tok-Qq19-shared-inbounD".into())]);
    assert_eq!(k.connection_inbound(shop, &wrong, b"{}").await.unwrap_err().to_string(), alike);
    drain(k).await;
    assert_eq!(count(k, tenant, &format!("select count(*) from {parcel} where tenant_id = $1 and label = 'shop:web'")).await, 1, "handed to the app, once");
    let other_app =
        json!({"test": true, "apps": ["depot"], "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "token"}, "handler": "crm"}});
    let e = k.save_connection_def(admin, None, &input("crm", other_app, None)).await.unwrap_err();
    assert!(e.to_string().contains("not one of the apps"), "{e}");
}

#[test]
fn connections_call_out_safely() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn boot() -> (Arc<Kernel>, Ctx) {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/parcel.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let depot = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([9u8; 32])), connection_hosts: vec!["127.0.0.1".into()], ..Default::default() };
    let mut hooks = HookRegistry::default();
    hooks.add_inbound_handler("depot", Arc::new(Depot));
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, depot]).unwrap(), hooks, settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "depot".into(),
            name: "depot".into(),
            admin_email: "admin@depot.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["depot".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("depot", "admin@depot.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    (k, admin)
}

async fn scenario() {
    let (k, admin) = boot().await;
    let seen: Seen = Arc::default();
    let port = service(seen.clone()).await;
    let base = format!("http://127.0.0.1:{port}/v1");
    let ops = json!({
        "create_label": {"path": "/labels", "body": "{\"weight\": {{ inputs.weight | json }}, \"ref\": {{ inputs.ref | json }}}",
                         "result": {"tracking": "/tracking", "cost": "/cost/amount"}, "idempotency_header": "Idempotency-Key", "inputs": ["weight", "ref"]},
        "echo": {"path": "/echo", "error_at": "/error/message"},
        "slow": {"path": "/slow"},
        "big": {"path": "/big"},
        "flaky": {"path": "/flaky", "method": "POST", "body": "{}", "error_at": "/error/message"},
        "refuse": {"path": "/refuse", "method": "POST", "body": "{}", "error_at": "/error/message"},
        "hop": {"path": "/hop"},
        "hop_private": {"path": "/hop-private"},
        "hop_localhost": {"path": "/hop-localhost"},
        "find": {"path": "/ok", "query": {"q": "{{ inputs.q }}"}},
        "lookup": {"path": "/ok/{{ inputs.id }}"}
    });
    let desc = json!({"base": base, "auth": {"type": "bearer", "secret": "api_key"}, "headers": {"x-account": "acme"}, "timeout_ms": 1000, "max_bytes": 1000,
                      "roles": ["Shipper"], "apps": ["depot"], "operations": ops});
    let carrier = k.save_connection_def(&admin, None, &input("carrier", desc.clone(), Some(json!({"api_key": SECRET})))).await.unwrap();
    k.debug_connection(&admin, carrier, 10).await.unwrap();
    let nowhere = Vec::<String>::new();

    // ── administrators configure ──
    let mut clerk = admin.clone();
    clerk.is_admin = false;
    clerk.roles = vec![];
    let e = k.save_connection_def(&clerk, None, &input("other", desc.clone(), None)).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "{e}");
    assert!(matches!(k.connection_defs(&clerk).await.unwrap_err(), Error::Forbidden(_)));
    let listed = k.connection_defs(&admin).await.unwrap();
    assert_eq!(listed[0]["secrets"], json!(["api_key"]), "the names of its secrets, never their values: {}", listed[0]);
    assert!(!listed[0].to_string().contains(SECRET));
    let bad = input("broken", json!({"base": "ftp://x.test"}), None);
    assert!(k.save_connection_def(&admin, None, &bad).await.unwrap_err().to_string().contains("https"));

    // ── a call: authenticated, its result mapped, once per key ──
    let label = call("carrier", "create_label", json!({"weight": 3, "ref": "P-1"}), Some("parcel-1"));
    let r = connections::call(&k, &admin, None, &label).await.unwrap();
    assert_eq!(r, json!({"tracking": "TRK-123", "cost": 7.5}));
    let sent = seen.lock().unwrap().last().cloned().unwrap();
    assert!(
        sent.contains(&format!("authorization: Bearer {SECRET}")) && sent.contains("idempotency-key: parcel-1") && sent.contains("x-account: acme"),
        "{sent}"
    );
    assert!(sent.ends_with("{\"weight\": 3, \"ref\": \"P-1\"}"), "{sent}");
    let before = seen.lock().unwrap().len();
    assert_eq!(connections::call(&k, &admin, None, &label).await.unwrap(), r, "the same key gets the first result ...");
    assert_eq!(seen.lock().unwrap().len(), before, "... and calls nobody");
    let other = call("carrier", "create_label", json!({"weight": 4, "ref": "P-1"}), Some("parcel-1"));
    assert!(matches!(connections::call(&k, &admin, None, &other).await.unwrap_err(), Error::Conflict(_)), "the same key with other inputs is refused");
    let e = connections::call(&k, &admin, None, &call("carrier", "create_label", json!({"weight": 1}), None)).await.unwrap_err();
    assert!(e.to_string().contains("needs a key"), "{e}");
    let e = connections::call(&k, &admin, None, &call("carrier", "create_label", json!({"weight": 1, "colour": "red"}), Some("k2"))).await.unwrap_err();
    assert!(e.to_string().contains("no input colour"), "{e}");
    // Values in a path are encoded, in a query too: they cannot change where the call goes.
    connections::call(&k, &admin, None, &call("carrier", "find", json!({"q": "a&b=c"}), None)).await.unwrap();
    assert!(seen.lock().unwrap().last().unwrap().starts_with("GET /v1/ok?q=a%26b%3Dc "));
    let _ = connections::call(&k, &admin, None, &call("carrier", "lookup", json!({"id": "../../admin"}), None)).await;
    assert!(seen.lock().unwrap().last().unwrap().starts_with("GET /v1/ok/..%2F..%2Fadmin "));

    // ── who may use it ──
    let e = connections::call(&k, &clerk, None, &call("carrier", "find", json!({}), None)).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "a person without its role: {e}");
    let mut shipper = clerk.clone();
    shipper.roles = vec!["Shipper".into()];
    connections::call(&k, &shipper, None, &call("carrier", "find", json!({}), None)).await.expect("one holding its role");
    connections::call(&k, &clerk, Some("depot"), &call("carrier", "find", json!({}), None)).await.expect("an app it lists");
    assert!(matches!(connections::call(&k, &clerk, Some("other"), &call("carrier", "find", json!({}), None)).await.unwrap_err(), Error::Forbidden(_)));

    // ── outcomes: a refusal is final, with its words, a secret it echoes masked; "later" is retried ──
    let e = connections::call(&k, &admin, None, &call("carrier", "echo", json!({}), None)).await.unwrap_err();
    assert!(matches!(e, Error::Invalid(_)) && e.to_string().contains("400") && e.to_string().contains("••••••") && !e.to_string().contains(SECRET), "{e}");
    let e = connections::call(&k, &admin, None, &call("carrier", "refuse", json!({}), Some("r1"))).await.unwrap_err();
    assert_eq!(e.to_string(), "422: weight missing");
    let e = connections::call(&k, &admin, None, &call("carrier", "slow", json!({}), None)).await.unwrap_err();
    assert!(matches!(e, Error::Unavailable(_)), "past its time limit, worth trying again: {e}");
    let e = connections::call(&k, &admin, None, &call("carrier", "big", json!({}), None)).await.unwrap_err();
    assert!(e.to_string().contains("larger than 1000 bytes"), "{e}");

    // ── redirects: followed on its own hosts only, each hop checked again ──
    assert_eq!(connections::call(&k, &admin, None, &call("carrier", "hop", json!({}), None)).await.unwrap(), json!({"ok": true}));
    let e = connections::call(&k, &admin, None, &call("carrier", "hop_private", json!({}), None)).await.unwrap_err();
    assert!(e.to_string().contains("10.0.0.1 is not one of the hosts"), "{e}");
    let e = connections::call(&k, &admin, None, &call("carrier", "hop_localhost", json!({}), None)).await.unwrap_err();
    assert!(e.to_string().contains("localhost is not one of the hosts"), "{e}");

    // ── addresses that are not public, and other schemes, are refused, even when the description lists them ──
    for (name, base, hosts, said) in [
        ("loopback", format!("http://localhost:{port}/v1"), json!([]), "https only"),
        ("loopback_tls", format!("https://localhost:{port}/v1"), json!([]), "private, loopback"),
        ("metadata", "https://169.254.169.254/latest".to_string(), json!([]), "private, loopback"),
        ("private", "https://10.1.2.3/".to_string(), json!([]), "private, loopback"),
        ("v6", "https://[::1]/".to_string(), json!([]), "private, loopback"),
        ("listed", format!("https://localhost:{port}/v1"), json!(["localhost"]), "private, loopback"),
    ] {
        let d = json!({"base": base, "allowed_hosts": hosts, "operations": {"get": {"path": "/x"}}});
        k.save_connection_def(&admin, None, &input(name, d, None)).await.unwrap();
        let e = connections::call(&k, &admin, None, &call(name, "get", json!({}), None)).await.unwrap_err();
        assert!(e.to_string().contains(said), "{name}: {e}");
    }

    // ── other ways to authenticate ──
    for (name, auth, header) in [
        ("hdr", json!({"type": "header", "header": "x-api-key", "secret": "api_key", "prefix": "Key "}), format!("x-api-key: Key {SECRET}")),
        ("basic", json!({"type": "basic", "username": "acme", "secret": "api_key"}), "authorization: Basic ".to_string()),
    ] {
        let d = json!({"base": base, "auth": auth, "operations": {"ok": {"path": "/ok"}}});
        k.save_connection_def(&admin, None, &input(name, d, Some(json!({"api_key": SECRET})))).await.unwrap();
        connections::call(&k, &admin, None, &call(name, "ok", json!({}), None)).await.unwrap();
        assert!(seen.lock().unwrap().last().unwrap().contains(&header), "{name}");
    }
    let d = json!({"base": base, "auth": {"type": "signed", "header": "x-signature", "secret": "signing", "timestamp_header": "x-timestamp", "prefix": "sha256="},
                   "operations": {"post": {"path": "/ok", "method": "POST", "body": "{\"a\": 1}", "changes": false}}});
    k.save_connection_def(&admin, None, &input("signed", d, Some(json!({"signing": SIGNING})))).await.unwrap();
    connections::call(&k, &admin, None, &call("signed", "post", json!({}), None)).await.unwrap();
    let sent = seen.lock().unwrap().last().cloned().unwrap();
    let ts = sent.lines().find_map(|l| l.strip_prefix("x-timestamp: ")).unwrap().trim().to_string();
    let sig = sent.lines().find_map(|l| l.strip_prefix("x-signature: sha256=")).unwrap().trim().to_string();
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(SIGNING.as_bytes()).unwrap();
    mac.update(format!("{ts}.{{\"a\": 1}}").as_bytes());
    assert_eq!(sig, hex::encode(mac.finalize().into_bytes()), "HMAC-SHA256 of '<timestamp>.<body>'");
    let d = json!({"base": base, "auth": {"type": "bearer", "secret": "missing"}, "operations": {"ok": {"path": "/ok"}}});
    k.save_connection_def(&admin, None, &input("nokey", d, None)).await.unwrap();
    let e = connections::call(&k, &admin, None, &call("nokey", "ok", json!({}), None)).await.unwrap_err();
    assert!(e.to_string().contains("has no secret missing"), "{e}");

    // ── a test connection answers from its fixtures and calls nobody ──
    let d = json!({"test": true, "operations": {"quote": {"result": {"amount": "/amount"}, "fixture": {"status": 200, "body": {"amount": 12}}}}});
    let fixture = k.save_connection_def(&admin, None, &input("fixture", d, None)).await.unwrap();
    let before = seen.lock().unwrap().len();
    assert_eq!(connections::call(&k, &admin, None, &call("fixture", "quote", json!({}), None)).await.unwrap(), json!({"amount": 12}));
    assert_eq!(k.try_connection(&admin, fixture, "quote", &json!({}), None).await.unwrap()["result"], json!({"amount": 12}));
    assert_eq!(seen.lock().unwrap().len(), before);

    // ── a connection turned off is not called ──
    let mut off = input("fixture", json!({"test": true, "operations": {"quote": {}}}), None);
    off.enabled = false;
    k.save_connection_def(&admin, Some(fixture), &off).await.unwrap();
    assert!(connections::call(&k, &admin, None, &call("fixture", "quote", json!({}), None)).await.unwrap_err().to_string().contains("turned off"));

    // ── the `call` effect: inline, its result for the next effect; queued, retried while the service says "later" ──
    let ship = json!({"name": "x_ship", "label": "Ship", "effects": [
        {"type": "call", "connection": "carrier", "operation": "create_label", "inputs": {"weight": "doc.weight", "ref": "doc.label"}},
        {"type": "set_fields", "values": {"tracking": "results[0].tracking"}},
        {"type": "call", "connection": "carrier", "operation": "flaky", "queue": true}]});
    let add: NewCustomization = serde_json::from_value(json!({"entity": "Parcel", "op": {"op": "add_action", "action": ship}})).unwrap();
    k.batch_customizations(&admin, vec![add], false).await.map_err(|e| e.1).unwrap();
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    let parcel = k.insert(&admin, "Parcel", obj(json!({"label": "P-9", "weight": 2}))).await.unwrap();
    let id: Uuid = parcel["id"].as_str().unwrap().parse().unwrap();
    k.run_action(&admin, "Parcel", id, "x_ship", json!({})).await.unwrap();
    assert_eq!(k.get(&admin, "Parcel", id, false).await.unwrap()["tracking"], "TRK-123");
    let sent = seen.lock().unwrap().iter().rev().find(|s| s.starts_with("POST /v1/labels")).cloned().unwrap();
    assert!(sent.contains(&format!("idempotency-key: action:Parcel.x_ship:{id}:v1:0")), "the run's own key: {sent}");
    drain(&k).await;
    let flaky: Vec<String> = seen.lock().unwrap().iter().filter(|s| s.starts_with("POST /v1/flaky")).cloned().collect();
    assert_eq!(flaky.len(), 2, "503, then again: done");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let job: (String, i32) =
        sqlx::query_as("select status::text, attempts from job where kind = $1").bind(connections::CALL_KIND).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(job, ("done".into(), 2));
    // A refusal is not retried.
    let refuse = json!({"name": "x_refuse", "label": "Refuse", "effects": [{"type": "call", "connection": "carrier", "operation": "refuse", "queue": true}]});
    let add: NewCustomization = serde_json::from_value(json!({"entity": "Parcel", "op": {"op": "add_action", "action": refuse}})).unwrap();
    k.batch_customizations(&admin, vec![add], false).await.map_err(|e| e.1).unwrap();
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    k.run_action(&admin, "Parcel", id, "x_refuse", json!({})).await.unwrap();
    let before = seen.lock().unwrap().iter().filter(|s| s.starts_with("POST /v1/refuse")).count();
    drain(&k).await;
    assert_eq!(seen.lock().unwrap().iter().filter(|s| s.starts_with("POST /v1/refuse")).count(), before + 1, "called once, not retried");

    // ── through the capability gate ──
    let inv =
        Invocation::new("ferrum.call", json!({"connection": "carrier", "operation": "create_label", "inputs": {"weight": 5, "ref": "G-1"}})).key("gate-1");
    let r = k.invoke(&admin, inv.clone()).await.unwrap();
    assert_eq!(r.result["tracking"], "TRK-123");
    let before = seen.lock().unwrap().len();
    assert!(k.invoke(&admin, inv).await.unwrap().repeated);
    assert_eq!(seen.lock().unwrap().len(), before);
    assert!(
        k.invoke(&clerk, Invocation::new("ferrum.call", json!({"connection": "carrier", "operation": "find"}))).await.is_err(),
        "an administrator's capability"
    );

    // ── the log: every call, no bodies; the debug capture redacted ──
    let log = k.connection_log(&admin, carrier, 100).await.unwrap();
    assert!(log.iter().any(|l| l["outcome"] == "ok" && l["entity"] == "Parcel" && l["record_id"] == json!(id)), "the record it was about");
    assert!(log.iter().any(|l| l["outcome"] == "temporary" && l["status"] == 503));
    let captured = log.iter().find(|l| l["operation"] == "create_label" && !l["debug"].is_null()).expect("captured while debug is on");
    assert_eq!(captured["debug"]["request"]["headers"]["authorization"], "••••••");
    assert!(log.iter().all(|l| !l.to_string().contains(SECRET)));

    being_called(&k, &admin).await;

    // ── secrets rest nowhere: not the log, the capture, the audit trail, the jobs ──
    assert_eq!(rests(&k, &[SECRET, SIGNING, INBOUND, TOKEN]).await, nowhere);
}
