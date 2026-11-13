//! Connections, part three (BD-53 onwards), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset first) and a
//! stand-in service on 127.0.0.1 shaped like the networks' documented answers (nothing here is a real network): steps
//! that read earlier answers and resume after the last finished one, never sending one twice.

use async_trait::async_trait;
use ferrum_kernel::connections::{self, Call, ConnectionInput};
use ferrum_kernel::files::FileStore;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const SECRET: &str = "sk-Vv31-steps-secret";

const PACKAGE: &str = "package: social\nversion: 0.1.0\ndepends: [core]\n";
const ENTITIES: &str = r#"
- name: Post
  label: Post
  kind: master
  fields:
    - {name: title, type: data}
    - {name: external_id, type: data, read_only: true}
"#;

/// What the stand-in was asked, raw, and what it keeps between requests.
#[derive(Default)]
struct Service {
    seen: Vec<String>,
    /// How often each path was asked.
    asked: BTreeMap<String, usize>,
    /// Paths that fail once each with the status given, or hang (0) past the connection's time limit.
    fail_once: BTreeMap<String, u16>,
}

static SERVICE: Mutex<Option<Service>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Service) -> T) -> T {
    f(SERVICE.lock().unwrap().get_or_insert_with(Service::default))
}

fn asked(path: &str) -> usize {
    with(|s| s.asked.get(path).copied().unwrap_or(0))
}

fn fail_once(path: &str, status: u16) {
    with(|s| s.fail_once.insert(path.to_string(), status));
}

fn last(prefix: &str) -> String {
    with(|s| s.seen.iter().rev().find(|r| r.starts_with(prefix)).cloned().unwrap_or_default())
}

fn body_of(raw: &str) -> Value {
    serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or_default()).unwrap_or(Value::Null)
}

/// What the stand-in answers: `(status, extra headers, body)`.
fn route(method: &str, path: &str, raw: &str, port: u16) -> (u16, String, String) {
    let bare = path.split('?').next().unwrap_or_default();
    let body = body_of(raw);
    match (method, bare) {
        // Shaped like an upload that is registered first, its bytes put to the address given, then the post.
        ("POST", "/v1/uploads:register") => {
            let n = asked(bare);
            (
                200,
                String::new(),
                json!({"value": {"uploadUrl": format!("http://127.0.0.1:{port}/v1/upload/{n}"), "asset": format!("urn:asset:{n}"),
                                                  "echo": raw.lines().find(|l| l.starts_with("authorization")).unwrap_or_default()}})
                .to_string(),
            )
        }
        ("POST", "/v1/uploads:elsewhere") => {
            (200, String::new(), json!({"value": {"uploadUrl": "https://uploads.elsewhere.example/put", "asset": "urn:asset:x"}}).to_string())
        }
        ("PUT", p) if p.starts_with("/v1/upload/") => (201, String::new(), String::new()),
        ("POST", "/v1/posts") | ("POST", "/v1/posts-keyed") => {
            let id = format!("urn:post:{}", asked("/v1/posts") + asked("/v1/posts-keyed"));
            (201, format!("x-restli-id: {id}\r\n"), json!({"text": body["text"], "asset": body["asset"]}).to_string())
        }
        ("GET", "/v1/posts/by-asset") => {
            let wanted = path.split_once("asset=").map(|a| a.1.replace("%3A", ":")).unwrap_or_default();
            let found = with(|s| s.seen.iter().any(|r| r.starts_with("POST /v1/posts ") && body_of(r)["asset"] == json!(wanted)));
            (200, String::new(), json!({"elements": if found { json!([{"id": "urn:post:found"}]) } else { json!([]) }}).to_string())
        }
        ("POST", "/v1/slow-step") => (200, String::new(), json!({"id": "S-1"}).to_string()),
        _ => (404, String::new(), json!({"error": "no such route"}).to_string()),
    }
}

async fn service() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
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
                let raw = String::from_utf8_lossy(&buf).to_string();
                let mut words = raw.split_whitespace();
                let (method, path) = (words.next().unwrap_or_default().to_string(), words.next().unwrap_or("/").to_string());
                let bare = path.split('?').next().unwrap_or_default().to_string();
                let failing = with(|s| {
                    s.seen.push(raw.clone());
                    *s.asked.entry(bare.clone()).or_default() += 1;
                    s.fail_once.remove(&bare)
                });
                if bare == "/v1/slow-step" {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
                let (status, extra, body) = match failing {
                    Some(0) => {
                        // Received, and its answer lost: the connection gives up waiting.
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        return;
                    }
                    Some(st) => (st, "retry-after: 7\r\n".to_string(), json!({"error": "not now"}).to_string()),
                    None => route(&method, &path, &raw, port),
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

/// Files kept in memory; how often a whole file was read, and each range.
#[derive(Default)]
struct Memory {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    whole: Mutex<usize>,
}

#[async_trait]
impl FileStore for Memory {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.files.lock().unwrap().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        *self.whole.lock().unwrap() += 1;
        self.files.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.files.lock().unwrap().remove(key);
        Ok(())
    }
}

fn input(name: &str, desc: Value, secrets: Option<Value>) -> ConnectionInput {
    serde_json::from_value(json!({"name": name, "label": name, "description": desc, "secrets": secrets, "enabled": true})).unwrap()
}

fn call(conn: &str, op: &str, inputs: Value, key: Option<&str>) -> Call {
    Call { connection: conn.into(), operation: op.into(), inputs, key: key.map(Into::into), ..Default::default() }
}

async fn count(k: &Kernel, tenant: Uuid, sql: &str) -> i64 {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let n = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// The tables any of `secrets` rests in.
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

/// P1: an operation of steps, each reading earlier answers, keyed `<key>:<step>`, resumed after the last finished one.
async fn steps(k: &Arc<Kernel>, admin: &Ctx, base: &str, store: &Memory) {
    let photo = k.store_file(admin, None, "photo.png", "image/png", b"PNGDATA").await.unwrap()["id"].as_str().unwrap().to_string();
    let _ = store;
    let ops = json!({
        "register": {"method": "POST", "path": "/uploads:register", "body": "{\"owner\": {{ inputs.owner | json }}}",
                     "result": {"upload_url": "/value/uploadUrl", "asset": "/value/asset", "echo": "/value/echo"}},
        "register_elsewhere": {"method": "POST", "path": "/uploads:elsewhere", "body": "{}", "result": {"upload_url": "/value/uploadUrl", "asset": "/value/asset"}},
        "put_bytes": {"method": "PUT", "url": "{{ inputs.url }}", "body": {"file": "{{ inputs.file }}"}},
        "create_post": {"method": "POST", "path": "/posts", "body": "{\"text\": {{ inputs.text | json }}, \"asset\": {{ inputs.asset | json }}}",
                        "result": {"id": {"header": "x-restli-id"}}},
        "create_post_keyed": {"method": "POST", "path": "/posts-keyed", "body": "{\"text\": {{ inputs.text | json }}, \"asset\": {{ inputs.asset | json }}}",
                              "result": {"id": {"header": "x-restli-id"}}, "idempotency_header": "Idempotency-Key"},
        "find_post": {"path": "/posts/by-asset", "query": {"asset": "{{ inputs.asset }}"}, "result": {"id": "/elements/0/id"}},
        "slow": {"method": "POST", "path": "/slow-step", "body": "{}", "result": {"id": "/id"}},
        "publish": {"inputs": ["owner", "text", "file"], "result": {"id": "/post/id", "asset": "/register/asset"}, "steps": [
            {"name": "register", "operation": "register", "inputs": {"owner": "inputs.owner"}},
            {"name": "upload", "operation": "put_bytes", "inputs": {"url": "steps.register.upload_url", "file": "inputs.file"}},
            {"name": "post", "operation": "create_post", "inputs": {"text": "inputs.text", "asset": "steps.register.asset"}}]},
        "publish_confirmed": {"result": {"id": "/post/id"}, "steps": [
            {"name": "register", "operation": "register", "inputs": {"owner": "inputs.owner"}},
            {"name": "post", "operation": "create_post", "inputs": {"text": "inputs.text", "asset": "steps.register.asset"},
             "confirm": {"operation": "find_post", "inputs": {"asset": "sent.asset"}, "proves": "result.id"}}]},
        "publish_keyed": {"result": {"id": "/post/id"}, "steps": [
            {"name": "post", "operation": "create_post_keyed", "inputs": {"text": "inputs.text", "asset": "'none'"}}]},
        "publish_elsewhere": {"steps": [
            {"name": "register", "operation": "register_elsewhere"},
            {"name": "upload", "operation": "put_bytes", "inputs": {"url": "steps.register.upload_url", "file": "inputs.file"}}]},
        "slow_once": {"steps": [{"name": "only", "operation": "slow"}]}
    });
    let desc = json!({"base": base, "auth": {"type": "bearer", "secret": "token"}, "timeout_ms": 1000, "operations": ops});
    k.save_connection_def(admin, None, &input("net", desc, Some(json!({"token": SECRET})))).await.unwrap();
    let publish = |key: &str, text: &str| call("net", "publish", json!({"owner": "urn:person:1", "text": text, "file": photo}), Some(key));

    // The post is "not now" once: the register and the upload are not made again.
    fail_once("/v1/posts", 503);
    let e = connections::call(k, admin, None, &publish("p-1", "Hello")).await.unwrap_err();
    assert!(matches!(e, Error::RetryAfter(_, 7) | Error::Unavailable(_)), "{e:?}");
    assert_eq!((asked("/v1/uploads:register"), asked("/v1/upload/1"), asked("/v1/posts")), (1, 1, 1));
    let put = last("PUT /v1/upload/1");
    assert!(put.ends_with("PNGDATA") && put.contains(&format!("authorization: Bearer {SECRET}")), "the bytes, put to the address the answer gave: {put}");
    let r = connections::call(k, admin, None, &publish("p-1", "Hello")).await.unwrap();
    assert_eq!(r, json!({"id": "urn:post:2", "asset": "urn:asset:1"}));
    assert_eq!((asked("/v1/uploads:register"), asked("/v1/upload/1"), asked("/v1/posts")), (1, 1, 2), "resumed after the last finished step");
    assert_eq!(body_of(&last("POST /v1/posts"))["asset"], "urn:asset:1", "a later step read an earlier answer");
    // Done: the same key gives the same answer and calls nobody; other inputs under it are refused.
    assert_eq!(connections::call(k, admin, None, &publish("p-1", "Hello")).await.unwrap(), r);
    assert_eq!(asked("/v1/posts"), 2);
    assert!(matches!(connections::call(k, admin, None, &publish("p-1", "Other")).await.unwrap_err(), Error::Conflict(_)));
    // Each step under `<key>:<step>`, its answer kept with the call, the token masked there too.
    let kept =
        count(k, admin.tenant, "select count(*) from conn_call where tenant_id = $1 and key in ('p-1:register', 'p-1:upload', 'p-1:post') and outcome = 'ok'")
            .await;
    assert_eq!(kept, 3);
    assert!(rests(k, &[SECRET]).await.is_empty(), "the token rests nowhere: {:?}", rests(k, &[SECRET]).await);

    // The post's answer is lost: it may have been made, so it is not sent again.
    fail_once("/v1/posts", 0);
    assert!(matches!(connections::call(k, admin, None, &publish("p-2", "Lost")).await.unwrap_err(), Error::Unavailable(_)));
    let before = asked("/v1/posts");
    let e = connections::call(k, admin, None, &publish("p-2", "Lost")).await.unwrap_err();
    assert!(e.to_string().contains("may have been sent"), "{e}");
    assert_eq!(asked("/v1/posts"), before, "never sent blindly again");
    // With a read-back check: asked whether it happened, and taken from there.
    fail_once("/v1/posts", 0);
    let confirmed = |key: &str| call("net", "publish_confirmed", json!({"owner": "o", "text": "Checked"}), Some(key));
    assert!(connections::call(k, admin, None, &confirmed("p-3")).await.is_err());
    let before = asked("/v1/posts");
    assert_eq!(connections::call(k, admin, None, &confirmed("p-3")).await.unwrap(), json!({"id": "urn:post:found"}));
    assert_eq!((asked("/v1/posts"), asked("/v1/posts/by-asset")), (before, 1), "proved by reading back, not sent again");
    // With an idempotency key: sent again, under the step's own key, which the service takes once.
    fail_once("/v1/posts-keyed", 0);
    let keyed = || call("net", "publish_keyed", json!({"text": "Keyed"}), Some("p-4"));
    assert!(connections::call(k, admin, None, &keyed()).await.is_err());
    connections::call(k, admin, None, &keyed()).await.unwrap();
    assert!(last("POST /v1/posts-keyed").contains("idempotency-key: p-4:post"), "{}", last("POST /v1/posts-keyed"));

    // An address an answer gives is reached only within the connection's hosts.
    let e = connections::call(k, admin, None, &call("net", "publish_elsewhere", json!({"file": photo}), Some("p-5"))).await.unwrap_err();
    assert!(e.to_string().contains("not one of the hosts"), "{e}");

    // Two workers on one key: one sends.
    let (a, b) = (k.clone(), k.clone());
    let (c1, c2) = (admin.clone(), admin.clone());
    let one = tokio::spawn(async move { connections::call(&a, &c1, None, &call("net", "slow_once", json!({}), Some("p-6"))).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let two = tokio::spawn(async move { connections::call(&b, &c2, None, &call("net", "slow_once", json!({}), Some("p-6"))).await });
    let (one, two) = (one.await.unwrap(), two.await.unwrap());
    assert!(one.is_ok(), "{one:?}");
    assert!(matches!(two, Err(Error::Unavailable(_) | Error::RetryAfter(..))), "the other is told to come back: {two:?}");
    assert_eq!(asked("/v1/slow-step"), 1);
    assert_eq!(connections::call(k, admin, None, &call("net", "slow_once", json!({}), Some("p-6"))).await.unwrap(), json!({"only": {"id": "S-1"}}));

    // Steps are checked when saved.
    let bad = |steps: Value| json!({"base": base, "operations": {"a": {"path": "/a"}, "b": {"steps": steps}}});
    for (steps, says) in [
        (json!([{"name": "x", "operation": "nothing"}]), "no operation nothing"),
        (json!([{"name": "x", "operation": "b"}]), "itself has steps"),
        (json!([{"name": "x", "operation": "a"}, {"name": "x", "operation": "a"}]), "two steps"),
        (json!([{"name": "x", "operation": "a", "inputs": {"v": "steps.("}}]), "steps.("),
    ] {
        let e = k.save_connection_def(admin, None, &input("bad", bad(steps), None)).await.unwrap_err().to_string();
        assert!(e.contains(says), "{says}: {e}");
    }
}

#[test]
fn connections_run_steps_safely() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn boot() -> (Arc<Kernel>, Ctx) {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 16, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/post.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let social = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let settings = KernelSettings {
        secret_key: Some(SecretKey::new([7u8; 32])),
        connection_hosts: vec!["127.0.0.1".into()],
        public_url: Some("https://ferrum.example.test".into()),
        ..Default::default()
    };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, social]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "social".into(),
            name: "social".into(),
            admin_email: "admin@social.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["social".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("social", "admin@social.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    (k, admin)
}

async fn scenario() {
    let (k, admin) = boot().await;
    let store = Arc::new(Memory::default());
    k.set_file_store(store.clone());
    let port = service().await;
    let base = format!("http://127.0.0.1:{port}/v1");
    steps(&k, &admin, &base, &store).await;
}
