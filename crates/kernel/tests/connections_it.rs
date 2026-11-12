//! Connections (crates/kernel/src/connections, BD-29 to BD-33) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`;
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
const CLIENT: &str = "cs-Kk55-oauth-client-secret";
const REFRESH: &str = "RT-1-Zz8-refresh";

/// The stand-in provider's state: whether the refresh token was revoked, how many access tokens it gave, the one it
/// takes now.
static PROVIDER: Mutex<(bool, usize, String)> = Mutex::new((false, 0, String::new()));

/// The stand-in's pull routes: how often `/shaky` was asked (it answers "later" twice), whether `/down` is down.
static SHAKY: AtomicUsize = AtomicUsize::new(0);
/// How often `/busy` was asked (it answers 429 with a Retry-After once).
static BUSY: AtomicUsize = AtomicUsize::new(0);
static DOWN: Mutex<bool> = Mutex::new(true);

fn issue() -> String {
    let mut p = PROVIDER.lock().unwrap();
    p.1 += 1;
    p.2 = format!("AT-{}-Xy9-access", p.1);
    p.2.clone()
}

const PACKAGE: &str = "package: depot\nversion: 0.1.0\ndepends: [core]\npersonal_data:\n  - {entity: Parcel, match: label, erase: [tracking]}\n";
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
                    "/v1/orders" => match path.split_once("cursor=").map(|c| c.1.to_string()).as_deref() {
                        None | Some("") => {
                            (200, String::new(), json!({"data": [{"id": "O-1", "title": "First"}, {"id": "O-2", "title": "Second"}], "next": "c2"}).to_string())
                        }
                        Some("c2") => (200, String::new(), json!({"data": [{"id": "O-3", "title": "Third"}], "next": null}).to_string()),
                        Some(_) => (400, String::new(), "{}".into()),
                    },
                    "/v1/oauth/token" => {
                        let form: BTreeMap<String, String> = reqwest::Url::parse(&format!("http://f/?{}", text.split("\r\n\r\n").nth(1).unwrap_or_default()))
                            .unwrap()
                            .query_pairs()
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect();
                        let refused = (400, String::new(), json!({"error": "invalid_grant"}).to_string());
                        match form.get("grant_type").map(String::as_str) {
                            Some("authorization_code") if form.get("code").map(String::as_str) == Some("CODE-1") => {
                                use base64::Engine as _;
                                let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"email": "pia@mail.test"}).to_string());
                                let at = issue();
                                (
                                    200,
                                    String::new(),
                                    json!({"access_token": at, "refresh_token": REFRESH, "expires_in": 3600, "id_token": format!("h.{claims}.s")}).to_string(),
                                )
                            }
                            Some("refresh_token") if !PROVIDER.lock().unwrap().0 && form.get("refresh_token").map(String::as_str) == Some(REFRESH) => {
                                (200, String::new(), json!({"access_token": issue(), "expires_in": 3600}).to_string())
                            }
                            _ => refused,
                        }
                    }
                    "/v1/oauth/revoke" => {
                        PROVIDER.lock().unwrap().0 = true;
                        (200, String::new(), "{}".into())
                    }
                    "/v1/me" => {
                        let now = PROVIDER.lock().unwrap().2.clone();
                        if text.contains(&format!("authorization: Bearer {now}\r\n")) && !now.is_empty() {
                            (200, String::new(), json!({"email": "pia@mail.test"}).to_string())
                        } else {
                            (401, String::new(), json!({"error": "invalid_token"}).to_string())
                        }
                    }
                    "/v1/shaky" => match SHAKY.fetch_add(1, Ordering::SeqCst) {
                        0 | 1 => (503, String::new(), json!({"error": "busy"}).to_string()),
                        _ => (200, String::new(), json!({"data": [{"id": "SH-1", "title": "Shaky"}], "next": "s2"}).to_string()),
                    },
                    "/v1/down" if *DOWN.lock().unwrap() => (503, String::new(), "{}".into()),
                    "/v1/down" => (200, String::new(), json!({"data": []}).to_string()),
                    "/v1/broken" => (422, String::new(), json!({"error": "no such feed"}).to_string()),
                    "/v1/since" => (200, String::new(), json!({"data": [{"id": "S-1", "title": "Since"}]}).to_string()),
                    "/v1/posts" => (201, "location: /v1/posts/P-77\r\nx-post-id: P-77\r\n".into(), json!({"counts": {"likes": 3, "shares": 4}}).to_string()),
                    "/v1/busy" => match BUSY.fetch_add(1, Ordering::SeqCst) {
                        0 => (429, "retry-after: 120\r\n".into(), json!({"error": "slow down"}).to_string()),
                        _ => (200, String::new(), "{}".into()),
                    },
                    "/v1/feed" if path.contains("page=2") => (200, String::new(), json!({"data": [{"id": "L-2", "title": "Two"}]}).to_string()),
                    "/v1/feed" => (
                        200,
                        "link: </v1/feed?page=2>; rel=\"next\", </v1/feed?page=9>; rel=\"last\"\r\n".into(),
                        json!({"data": [{"id": "L-1", "title": "One"}]}).to_string(),
                    ),
                    "/v1/astray" => (200, "link: <http://10.0.0.1/v1/feed?page=2>; rel=\"next\"\r\n".into(), json!({"data": []}).to_string()),
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

/// Pulling: a cursor kept between runs, pages bounded per run, each item mapped as an inbound call is.
async fn pulling(k: &Kernel, admin: &Ctx, base: &str, seen: &Seen) {
    let tenant = admin.tenant;
    let alert = admin.schema.entity("Alert").unwrap().table.clone();
    let map = json!({"entity": "Alert", "by": {"external_id": "item.id"}, "set": {"title": "item.title", "state": "'pulled'"}});
    let d = json!({"base": base, "operations": {"orders": {"path": "/orders", "query": {"cursor": "{{ cursor or '' }}"}}, "since": {"path": "/since", "query": {"since": "{{ inputs.cursor }}"}}},
        "pulls": [{"name": "orders", "operation": "orders", "every_minutes": 15, "items_at": "/data", "next_at": "/next", "max_pages": 1, "map": map},
                  {"name": "recent", "operation": "since", "every_minutes": 60, "items_at": "/data", "cursor": "time", "map": map}]});
    let id = k.save_connection_def(admin, None, &input("orders", d.clone(), None)).await.unwrap();
    let pulled = |n: i64| {
        let sql = format!("select count(*) from {alert} where tenant_id = $1 and state = 'pulled'");
        async move { assert_eq!(count(k, tenant, &sql).await, n) }
    };
    assert_eq!(k.sweep_pulls_at(chrono::Utc::now()).await.unwrap(), 2, "both due at once when new");
    assert_eq!(k.sweep_pulls_at(chrono::Utc::now()).await.unwrap(), 0, "and not queued twice");
    drain(k).await;
    pulled(3).await; // orders' first page (one page a run) and recent's one item
    let pulls = k.connection_pulls(admin, id).await.unwrap();
    let orders = pulls.iter().find(|p| p["name"] == "orders").unwrap();
    assert_eq!(orders["cursor"], "c2", "the token its answer gave, kept for the next run");
    let recent = pulls.iter().find(|p| p["name"] == "recent").unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(recent["cursor"].as_str().unwrap()).is_ok(), "the moment the run began: {recent}");
    k.pull_now(admin, id, "orders").await.unwrap();
    drain(k).await;
    pulled(4).await;
    assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("GET /v1/orders?cursor=c2 ")));
    let since = recent["cursor"].as_str().unwrap().to_string();
    k.pull_now(admin, id, "recent").await.unwrap();
    drain(k).await;
    let asked = seen.lock().unwrap().iter().rev().find(|s| s.starts_with("GET /v1/since")).cloned().unwrap();
    assert!(asked.contains(&since.replace(':', "%3A").replace('+', "%2B")), "the last run's start is the next one's since: {asked}");
    pulled(4).await; // S-1 again: changed, not made twice
    // A pull reads: one that changes something is refused.
    let mut bad = d.clone();
    bad["operations"]["orders"]["method"] = json!("POST");
    assert!(k.save_connection_def(admin, Some(id), &input("orders", bad, None)).await.unwrap_err().to_string().contains("a pull only reads"));

    // A pull answered "later" is tried again from where it stood, before the next run, without a second job.
    let ops = json!({"shaky": {"path": "/shaky"}, "down": {"path": "/down"}, "broken": {"path": "/broken"}});
    let pull = |name: &str| json!({"name": name, "operation": name, "every_minutes": 15, "items_at": "/data", "next_at": "/next", "max_pages": 1, "map": map});
    let feeds = json!({"base": base, "operations": ops, "pulls": [pull("shaky"), pull("down"), pull("broken")]});
    let feeds = k.save_connection_def(admin, None, &input("feeds", feeds, None)).await.unwrap();
    let state = |name: &'static str| async move { k.connection_pulls(admin, feeds).await.unwrap().into_iter().find(|p| p["name"] == name).unwrap() };
    let job_of = |name: &'static str| {
        let sql = format!(
            "select (attempts * 10 + case status when 'queued' then 1 when 'done' then 2 else 3 end)::bigint from job where tenant_id = $1 and dedupe_key = '{feeds}:{name}' order by created_at desc limit 1"
        );
        async move { count(k, tenant, &sql).await }
    };
    let told = |name: &str| {
        let sql = format!("select count(*) from notification where tenant_id = $1 and title = 'Pulling {name} from feeds failed'");
        async move { count(k, tenant, &sql).await }
    };
    assert_eq!(k.sweep_pulls_at(chrono::Utc::now()).await.unwrap(), 3);
    while k.run_jobs_once(4).await.unwrap() > 0 {}
    assert_eq!(job_of("shaky").await, 11, "one attempt, queued again with backoff");
    assert!(state("shaky").await["cursor"].is_null(), "the cursor did not move");
    assert_eq!(told("shaky").await, 0, "not told of a run still being tried");
    assert_eq!(k.sweep_pulls_at(chrono::Utc::now() + chrono::Duration::minutes(2)).await.unwrap(), 0, "the next run is not due, nor queued over the retry");
    drain(k).await;
    assert_eq!(job_of("shaky").await, 32, "done on the third attempt");
    let shaky = state("shaky").await;
    assert_eq!((shaky["cursor"].as_str(), shaky["last_error"].is_null()), (Some("s2"), true), "{shaky}");
    assert_eq!(SHAKY.load(Ordering::SeqCst), 3);
    // Always "later": a bounded number of attempts, then recorded and told once per streak.
    assert_eq!(job_of("down").await, 42, "four attempts, then the run ends");
    assert!(state("down").await["last_error"].is_string());
    assert_eq!(told("down").await, 1);
    // A permanent refusal: not tried again, recorded, told once over two failed runs.
    assert_eq!(job_of("broken").await, 12, "one attempt");
    assert!(state("broken").await["last_error"].as_str().is_some_and(|e| e.contains("422")));
    k.pull_now(admin, feeds, "broken").await.unwrap();
    k.pull_now(admin, feeds, "down").await.unwrap();
    drain(k).await;
    assert_eq!((told("broken").await, told("down").await), (1, 1), "once per streak");
    // A run that works ends the streak; the next failure tells again.
    *DOWN.lock().unwrap() = false;
    k.pull_now(admin, feeds, "down").await.unwrap();
    drain(k).await;
    assert!(state("down").await["last_error"].is_null());
    *DOWN.lock().unwrap() = true;
    k.pull_now(admin, feeds, "down").await.unwrap();
    drain(k).await;
    assert_eq!(told("down").await, 2, "a new streak");
}

/// Answers beyond a JSON body (BD-46): headers and links in a result, Retry-After for a queued call, a page asked at
/// the address its Link named (and never off the connection's hosts), pointers that read null, required values,
/// expressions with the connection's name.
async fn answers(k: &Kernel, admin: &Ctx, base: &str, seen: &Seen) {
    let tenant = admin.tenant;
    let alert = admin.schema.entity("Alert").unwrap().table.clone();
    let map = json!({"entity": "Alert", "by": {"external_id": {"at": "/id", "required": true}}, "set": {"title": "/title", "state": "'linked'"}});
    let page = |name: &str| json!({"name": name, "operation": name, "every_minutes": 15, "items_at": "/data", "next_at": {"link": "next"}, "map": map});
    let d = json!({"base": base, "operations": {
        "post": {"path": "/posts", "method": "POST", "body": "{}", "result": {
            "id": {"header": "x-post-id"}, "where": {"header": "Location"}, "total": {"expr": "answer.counts.likes + answer.counts.shares"},
            "via": {"expr": "connection.name + ':' + inputs.ref"}, "missing": "/nope"}},
        "post_strict": {"path": "/posts", "method": "POST", "body": "{}", "result": {"id": "/id", "must": {"at": "/nope", "required": true}}},
        "post_secret": {"path": "/posts", "method": "POST", "body": "{}", "result": {"k": {"expr": "secret.api_key"}}},
        "busy": {"path": "/busy", "method": "POST", "body": "{}"},
        "feed": {"path": "/feed"}, "astray": {"path": "/astray"}},
        "pulls": [page("feed"), page("astray")]});
    k.save_connection_def(admin, None, &input("social", d, Some(json!({"api_key": SECRET})))).await.unwrap();
    let r = connections::call(k, admin, None, &call("social", "post", json!({"ref": "R-1"}), Some("p-1"))).await.unwrap();
    assert_eq!(
        r,
        json!({"id": "P-77", "where": "/v1/posts/P-77", "total": 7, "via": "social:R-1", "missing": null}),
        "headers, sums and the connection's name"
    );
    let e = connections::call(k, admin, None, &call("social", "post_strict", json!({}), Some("p-2"))).await.unwrap_err();
    assert!(matches!(&e, Error::Invalid(m) if m.ends_with("the answer has no must")), "{e}");
    let e = connections::call(k, admin, None, &call("social", "post_secret", json!({}), Some("p-3"))).await.unwrap_err();
    assert!(!e.to_string().contains(SECRET), "an answer's expression does not read the secrets: {e}");

    // A 429 with Retry-After: the queued call comes back when the service said, not on the backoff.
    let busy = json!({"name": "x_busy", "label": "Busy", "effects": [{"type": "call", "connection": "social", "operation": "busy", "queue": true}]});
    let add: NewCustomization = serde_json::from_value(json!({"entity": "Parcel", "op": {"op": "add_action", "action": busy}})).unwrap();
    k.batch_customizations(admin, vec![add], false).await.map_err(|e| e.1).unwrap();
    let admin = &k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    let parcel: Uuid = k.insert(admin, "Parcel", obj(json!({"label": "B-1"}))).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    k.run_action(admin, "Parcel", parcel, "x_busy", json!({})).await.unwrap();
    while k.run_jobs_once(4).await.unwrap() > 0 {}
    let waits = count(
        k,
        tenant,
        &format!(
            "select extract(epoch from run_at - now())::bigint from job where tenant_id = $1 and kind = '{}' and status = 'queued'",
            connections::CALL_KIND
        ),
    )
    .await;
    assert!((100..=120).contains(&waits), "asked again after the service's 120 s: {waits}");
    drain(k).await;
    assert_eq!(BUSY.load(Ordering::SeqCst), 2);

    // Paging by the Link header: the next page asked at the address it named; one naming another host is refused.
    let id: Uuid = k.connection_defs(admin).await.unwrap().iter().find(|c| c["name"] == "social").unwrap()["id"].as_str().unwrap().parse().unwrap();
    k.pull_now(admin, id, "feed").await.unwrap();
    drain(k).await;
    assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("GET /v1/feed?page=2 ")), "the page the link named");
    assert_eq!(count(k, tenant, &format!("select count(*) from {alert} where tenant_id = $1 and state = 'linked' and external_id in ('L-1', 'L-2')")).await, 2);
    k.pull_now(admin, id, "astray").await.unwrap();
    drain(k).await;
    let astray = k.connection_pulls(admin, id).await.unwrap().into_iter().find(|p| p["name"] == "astray").unwrap();
    assert!(astray["last_error"].as_str().is_some_and(|e| e.contains("10.0.0.1 is not one of the hosts")), "{astray}");

    // A mapping's pointer reads null where the key is not there; `required` fails the item, saying which.
    let hook = json!({"test": true, "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "token"}, "map": {"entity": "Alert", "items_at": "/items",
        "by": {"external_id": {"at": "/id", "required": true}}, "set": {"state": "/state", "title": {"expr": "connection.name + ' ' + string(item.likes + item.shares)"}}}}});
    let hook = k.save_connection_def(admin, None, &input("posts_in", hook, Some(json!({"token": TOKEN})))).await.unwrap();
    let h = headers(&[("X-Token", TOKEN.into())]);
    k.connection_inbound(hook, &h, json!({"items": [{"id": "N-1", "likes": 2, "shares": 5}]}).to_string().as_bytes()).await.unwrap();
    k.connection_inbound(hook, &h, json!({"items": [{"id": "N-2", "likes": 1, "shares": 1}, {"likes": 0, "shares": 0}]}).to_string().as_bytes()).await.unwrap();
    drain(k).await;
    assert_eq!(
        count(k, tenant, &format!("select count(*) from {alert} where tenant_id = $1 and external_id = 'N-1' and title = 'posts_in 7' and state is null"))
            .await,
        1
    );
    let failed = k.connection_inbound_log(admin, hook, 10).await.unwrap().into_iter().find(|r| r["state"] == "failed").unwrap();
    assert!(failed["error"].as_str().unwrap().contains("item 2: external_id is required"), "{failed}");
    assert_eq!(
        count(k, tenant, &format!("select count(*) from {alert} where tenant_id = $1 and external_id = 'N-2'")).await,
        0,
        "the payload is taken whole or not at all"
    );
}

fn query(url: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// A person authorises an account: PKCE, state bound to them and their session, used once; tokens sealed, refreshed
/// ahead and when a service refuses one; a revoked sign-in marks the connection and tells its owner once; disconnecting
/// revokes. What must never be kept: the states and verifiers it made.
async fn signing_in(k: &Kernel, admin: &Ctx, base: &str, seen: &Seen) -> Vec<String> {
    let tenant = admin.tenant;
    let person = |email: &'static str| async move {
        let id = k.create_user(admin, email, email, None, &[], false).await.unwrap();
        let mut c = k.load_ctx(tenant, id, None, "it".into()).await.unwrap();
        c.roles.push("Mailer".into());
        c
    };
    let (pia, bo) = (person("pia@depot.test").await, person("bo@depot.test").await);
    let provider = json!({"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/oauth/token"), "revoke_url": format!("{base}/oauth/revoke"),
        "scopes": ["mail.read", "offline"], "client_id": "ferrum-test", "client_secret": "client", "account_at": "/id_token/email"});
    let d =
        json!({"base": base, "auth": {"type": "oauth", "owner": "person", "provider": provider}, "roles": ["Mailer"], "operations": {"me": {"path": "/me"}}});
    let id = k.save_connection_def(admin, None, &input("mailbox", d, Some(json!({"client": CLIENT})))).await.unwrap();
    let (s1, s2) = (Uuid::now_v7(), Uuid::now_v7());
    let me = || call("mailbox", "me", json!({}), None);
    let mut made = Vec::new();
    let start = |who: Ctx, session: Uuid| async move {
        let url = k.connect_start(&who, id, session).await.unwrap();
        let q = query(&url);
        (q["state"].clone(), q)
    };
    let (state, q) = start(pia.clone(), s1).await;
    assert_eq!(q["redirect_uri"], "https://ferrum.example.test/api/connections/callback", "the platform's own address, exactly");
    assert_eq!((q["code_challenge_method"].as_str(), q["scope"].as_str(), q["response_type"].as_str()), ("S256", "mail.read offline", "code"));
    made.push(state.clone());
    let refused = |r: ferrum_kernel::Result<Uuid>| matches!(r, Err(Error::Unauthorized(_)));
    assert!(refused(k.connect_finish(&pia, s1, "a-tampered-state", Some("CODE-1"), None).await), "a state it did not make");
    assert!(refused(k.connect_finish(&pia, s2, &state, Some("CODE-1"), None).await), "another browser session");
    assert!(refused(k.connect_finish(&pia, s1, &state, Some("CODE-1"), None).await), "a state is used once, even by a wrong try");
    let (state, _) = start(pia.clone(), s1).await;
    made.push(state.clone());
    assert!(refused(k.connect_finish(&bo, s1, &state, Some("CODE-1"), None).await), "another person");
    let (state, q) = start(pia.clone(), s1).await;
    made.push(state.clone());
    assert!(k.connect_finish(&pia, s1, &state, None, Some("access_denied<script>")).await.unwrap_err().to_string().ends_with("access_deniedscript"));
    let (state, q2) = start(pia.clone(), s1).await;
    made.push(state.clone());
    assert_eq!(k.connect_finish(&pia, s1, &state, Some("CODE-1"), None).await.unwrap(), id);
    let exchanged = seen.lock().unwrap().iter().rev().find(|s| s.starts_with("POST /v1/oauth/token")).cloned().unwrap();
    let form = query(&format!("http://f/?{}", exchanged.split("\r\n\r\n").nth(1).unwrap()));
    use base64::Engine as _;
    use sha2::Digest as _;
    let verifier = form["code_verifier"].clone();
    assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes())), q2["code_challenge"], "PKCE S256");
    assert_ne!(q["code_challenge"], q2["code_challenge"]);
    assert_eq!((form["redirect_uri"].as_str(), form["client_secret"].as_str()), ("https://ferrum.example.test/api/connections/callback", CLIENT));
    made.push(verifier);
    let mine = k.personal_connections(&pia).await.unwrap();
    assert_eq!((mine[0]["status"].as_str(), mine[0]["account"].as_str()), (Some("connected"), Some("pia@mail.test")), "{mine:?}");

    // Calls go with the caller's own account; another person has none.
    assert_eq!(connections::call(k, &pia, None, &me()).await.unwrap(), json!({"email": "pia@mail.test"}));
    assert!(connections::call(k, &bo, None, &me()).await.unwrap_err().to_string().contains("not connected"));
    let expire = |at: &'static str| async move {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("update conn_token set expires_at = now() + interval '{at}' where tenant_id = $1")))
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    };
    // About to expire: refreshed before the call.
    expire("-1 minute").await;
    connections::call(k, &pia, None, &me()).await.unwrap();
    assert_eq!(PROVIDER.lock().unwrap().1, 2);
    // A service that refuses the token: refreshed once, the call made again.
    PROVIDER.lock().unwrap().2 = "taken-back".into();
    connections::call(k, &pia, None, &me()).await.unwrap();
    assert_eq!(PROVIDER.lock().unwrap().1, 3);
    // Ahead of its expiry, by the job.
    expire("5 minutes").await;
    assert_eq!(k.sweep_tokens_at(chrono::Utc::now()).await.unwrap(), 1);
    drain(k).await;
    assert_eq!(PROVIDER.lock().unwrap().1, 4);

    // Revoked at the provider: "needs reconnecting", its owner told once.
    PROVIDER.lock().unwrap().0 = true;
    expire("-1 minute").await;
    let told = "select count(*) from notification where tenant_id = $1 and title = 'Reconnect mailbox'";
    for _ in 0..2 {
        assert!(connections::call(k, &pia, None, &me()).await.unwrap_err().to_string().contains("needs reconnecting"));
        assert_eq!(count(k, tenant, told).await, 1);
    }
    assert_eq!(k.personal_connections(&pia).await.unwrap()[0]["status"], "needs_reconnecting");
    // Connected again, then disconnected: revoked at the provider, gone here.
    PROVIDER.lock().unwrap().0 = false;
    let (state, _) = start(pia.clone(), s1).await;
    made.push(state.clone());
    k.connect_finish(&pia, s1, &state, Some("CODE-1"), None).await.unwrap();
    connections::call(k, &pia, None, &me()).await.unwrap();
    assert!(k.connect_remove(&pia, id).await.unwrap(), "the provider confirmed");
    let revoked = seen.lock().unwrap().iter().rev().find(|s| s.starts_with("POST /v1/oauth/revoke")).cloned().unwrap();
    assert!(revoked.contains(&format!("token={REFRESH}")) && revoked.contains("token_type_hint=refresh_token"));
    assert!(connections::call(k, &pia, None, &me()).await.unwrap_err().to_string().contains("not connected"));

    // The workspace's account is an administrator's to connect.
    let org = json!({"base": base, "auth": {"type": "oauth", "provider": {"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/oauth/token"), "client_id": "ferrum-org"}},
                     "operations": {"me": {"path": "/me"}}});
    let org = k.save_connection_def(admin, None, &input("org", org, None)).await.unwrap();
    assert!(matches!(k.connect_start(&pia, org, s1).await.unwrap_err(), Error::Forbidden(_)));
    let url = k.connect_start(admin, org, s1).await.unwrap();
    made.push(query(&url)["state"].clone());
    let bad = json!({"auth": {"type": "oauth", "provider": {"authorize_url": "javascript:alert(1)", "token_url": "https://x.test/t", "client_id": "c"}}, "test": true});
    assert!(k.save_connection_def(admin, None, &input("bad", bad, None)).await.unwrap_err().to_string().contains("authorize_url"));
    made
}

/// A person's data (BD-35): their own sign-in, the calls they made, a call about a record of theirs, a capture and a
/// delivery naming them are in their report; erasure destroys the sign-in's tokens and revokes them at the provider
/// (recorded), and leaves the log's lines without what named them.
async fn erasing(k: &Kernel, admin: &Ctx, seen: &Seen) {
    let tenant = admin.tenant;
    let ivy_id = k.create_user(admin, "ivy@depot.test", "Ivy", None, &[], false).await.unwrap();
    let mut ivy = k.load_ctx(tenant, ivy_id, None, "it".into()).await.unwrap();
    ivy.roles.push("Mailer".into());
    let defs = k.connection_defs(admin).await.unwrap();
    let id_of = |name: &str| -> Uuid { defs.iter().find(|c| c["name"] == name).unwrap()["id"].as_str().unwrap().parse().unwrap() };
    let (mailbox, carrier) = (id_of("mailbox"), id_of("carrier"));
    PROVIDER.lock().unwrap().0 = false;
    let session = Uuid::now_v7();
    let state = query(&k.connect_start(&ivy, mailbox, session).await.unwrap())["state"].clone();
    k.connect_finish(&ivy, session, &state, Some("CODE-1"), None).await.unwrap();
    connections::call(k, &ivy, None, &call("mailbox", "me", json!({}), None)).await.unwrap();
    let parcel: Uuid =
        k.insert(admin, "Parcel", obj(json!({"label": "ivy@depot.test", "tracking": "T-9"}))).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let about = Call { entity: Some("Parcel".into()), record: Some(parcel), ..call("carrier", "find", json!({"q": "x"}), None) };
    connections::call(k, admin, None, &about).await.unwrap();
    connections::call(k, admin, None, &call("carrier", "find", json!({"q": "ivy@depot.test"}), None)).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into conn_inbound (tenant_id, conn_id, delivery, state, payload, error) values ($1, $2, 'd-ivy', 'failed', $3, 'no')")
        .bind(tenant)
        .bind(carrier)
        .bind(json!({"from": "Ivy@Depot.test"}))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let report = k.personal_data(admin, "ivy@depot.test").await.unwrap();
    let held: Vec<&Value> = report["apps"].as_array().unwrap().iter().filter(|a| a["app"] == "connections").map(|a| &a["held"]).collect();
    let kinds = |kind: &str| held.iter().filter(|h| h["kind"] == kind).count();
    assert_eq!((kinds("sign_in"), kinds("inbound")), (1, 1), "{report}");
    assert!(held.iter().any(|h| h["kind"] == "call" && h["made_by_them"] == json!(true)), "the call they made: {report}");
    assert!(held.iter().any(|h| h["record"] == json!(parcel)), "the call about their record: {report}");
    assert!(held.iter().any(|h| h["operation"] == "find" && h["record"].is_null()), "the capture naming them: {report}");
    assert!(!report.to_string().contains(REFRESH), "never a token");

    let revokes = || seen.lock().unwrap().iter().filter(|s| s.starts_with("POST /v1/oauth/revoke")).count();
    let before = revokes();
    let erased = k.erase_personal_data(admin, "ivy@depot.test", "asked to be forgotten").await.unwrap();
    assert_eq!(erased["sign_ins_revoked"], json!([{"connection": mailbox, "revoked": true}]), "{erased}");
    assert_eq!(revokes(), before + 1, "revoked at the provider");
    let left = |sql: &'static str| async move { count(k, tenant, sql).await };
    assert_eq!(
        left("select count(*) from conn_token where tenant_id = $1 and user_id <> '00000000-0000-0000-0000-000000000000'").await,
        0,
        "the tokens are gone"
    );
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let hers: i64 =
        sqlx::query_scalar("select count(*) from conn_call where tenant_id = $1 and user_id = $2").bind(tenant).bind(ivy_id).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(hers, 0, "no call names her as its caller");
    assert_eq!(left("select count(*) from conn_call where tenant_id = $1 and lower(concat(debug::text, error, result::text)) like '%ivy@depot%'").await, 0);
    assert_eq!(
        left(
            "select count(*) from conn_call where tenant_id = $1 and record_id is not null and debug is not null and entity = 'Parcel' and operation = 'find'"
        )
        .await,
        0
    );
    assert_eq!(left("select count(*) from conn_inbound where tenant_id = $1 and payload is null and delivery = 'd-ivy'").await, 1, "the line stays, emptied");
    assert_eq!(left("select count(*) from audit_event where tenant_id = $1 and action = 'revoke_on_erasure'").await, 1, "recorded");
    let again = k.personal_data(admin, "ivy@depot.test").await.unwrap();
    // The record stays theirs (only its declared fields go), so the call about it is still listed, emptied.
    let left: Vec<&Value> = again["apps"].as_array().unwrap().iter().filter(|a| a["app"] == "connections").map(|a| &a["held"]).collect();
    assert!(left.iter().all(|h| h["record"] == json!(parcel) && h["made_by_them"] == json!(false)), "{again}");

    // A restore from before the erasure brings her rows back; the erasure's journal takes them again.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update app_user set email = 'ivy@depot.test' where tenant_id = $1 and id = $2").bind(tenant).bind(ivy_id).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into conn_token (tenant_id, conn_id, user_id, sealed) values ($1, $2, $3, 'restored')")
        .bind(tenant)
        .bind(mailbox)
        .bind(ivy_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into conn_call (tenant_id, id, conn_id, operation, outcome, user_id, debug) values ($1, $2, $3, 'me', 'ok', $4, $5)")
        .bind(tenant)
        .bind(Uuid::now_v7())
        .bind(mailbox)
        .bind(ivy_id)
        .bind(json!({"request": {"url": "https://x.test/?to=ivy%40depot.test"}}))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("update conn_inbound set payload = $3 where tenant_id = $1 and conn_id = $2 and delivery = 'd-ivy'")
        .bind(tenant)
        .bind(carrier)
        .bind(json!({"from": "ivy@depot.test"}))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let store = Arc::new(Memory::default());
    k.set_file_store(store.clone());
    let key = format!("{}{tenant}/1.json", ferrum_kernel::privacy::JOURNAL_PREFIX);
    let entry =
        json!({"tenant": tenant, "erasure": {"account": ivy_id, "records": [{"entity": "Parcel", "id": parcel, "fields": ["tracking"]}], "reason": "r"}});
    ferrum_kernel::files::FileStore::put(&*store, &key, entry.to_string().as_bytes()).await.unwrap();
    k.reapply_erasure(&key).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let back: (i64, i64, i64) = sqlx::query_as(
        "select (select count(*) from conn_token where tenant_id = $1 and user_id = $2), (select count(*) from conn_call where tenant_id = $1 and user_id = $2), \
           (select count(*) from conn_inbound where tenant_id = $1 and payload is not null and delivery = 'd-ivy')",
    )
    .bind(tenant)
    .bind(ivy_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(back, (0, 0, 0), "the restored sign-in, call and delivery are taken again");
}

/// Stored files, in memory.
#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);

#[async_trait]
impl ferrum_kernel::files::FileStore for Memory {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
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
    let settings = KernelSettings {
        secret_key: Some(SecretKey::new([9u8; 32])),
        connection_hosts: vec!["127.0.0.1".into()],
        public_url: Some("https://ferrum.example.test".into()),
        ..Default::default()
    };
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
    answers(&k, &admin, &base, &seen).await;
    pulling(&k, &admin, &base, &seen).await;
    let signed_in = signing_in(&k, &admin, &base, &seen).await;
    erasing(&k, &admin, &seen).await;

    // ── secrets rest nowhere: not the log, the capture, the audit trail, the jobs ──
    let mut never: Vec<&str> = vec![SECRET, SIGNING, INBOUND, TOKEN, CLIENT, REFRESH, "Xy9-access"];
    never.extend(signed_in.iter().map(String::as_str));
    assert_eq!(rests(&k, &never).await, nowhere);
}
