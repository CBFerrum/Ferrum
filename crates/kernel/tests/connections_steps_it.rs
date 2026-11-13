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

const PACKAGE: &str = "package: social\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Poster\n    permissions:\n      - {entity: Post, read: true, write: true, create: true}\n";
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
    /// Paths that fail at their n-th ask, as `fail_once` says.
    fail_at: BTreeMap<String, (usize, u16)>,
    /// What each upload session holds.
    sessions: BTreeMap<String, String>,
    /// Requests to `/v1/slow-read` in flight now, and the most at once.
    inflight: (usize, usize),
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

fn fail_at(path: &str, nth: usize, status: u16) {
    with(|s| s.fail_at.insert(path.to_string(), (nth, status)));
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
        // Shaped like a record created under a key of its own, with byte-offset facets.
        ("POST", "/v1/records") => (200, String::new(), json!({"uri": format!("at://did/post/{}", body["rkey"].as_str().unwrap_or_default())}).to_string()),
        ("GET", "/v1/slow-read") | ("GET", "/v1/counted") => (200, String::new(), json!({"ok": true}).to_string()),
        ("GET", "/v1/busy") => (429, "retry-after: 7\r\n".into(), json!({"error": "slow down"}).to_string()),
        ("GET", "/v1/mine") => (200, String::new(), json!({"items": [{"id": "N-1", "title": "Native one"}]}).to_string()),
        ("GET", "/v1/mine2") => (200, String::new(), json!({"items": [{"id": "N-2", "title": "Native two"}]}).to_string()),
        ("POST", "/v1/oauth/token") => {
            (200, String::new(), json!({"access_token": "AT-Pp5-access", "refresh_token": "RT-Pp5", "expires_in": 3600}).to_string())
        }
        ("GET", "/v1/rss") => (
            200,
            String::new(),
            r#"<?xml version="1.0"?><rss version="2.0" xmlns:dc="http://purl.org/dc/elements/1.1/"><channel>
<title>News</title><item><title>First &amp; best</title><link>https://news.example/1</link><dc:creator>Ana</dc:creator></item>
<item><title><![CDATA[Second <b>]]></title><guid isPermaLink="false">g2</guid></item></channel></rss>"#
                .into(),
        ),
        ("GET", "/v1/atom") => (
            200,
            String::new(),
            r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>A</title>
<entry><id>urn:a1</id><title type="text">Atom one</title><link rel="alternate" href="https://news.example/a1"/></entry></feed>"#
                .into(),
        ),
        ("GET", "/v1/ok-false") => (200, String::new(), json!({"ok": false, "error": "channel_not_found"}).to_string()),
        ("GET", "/v1/ok-true") => (200, String::new(), json!({"ok": true, "id": "M-9"}).to_string()),
        ("POST", "/v1/presign") => {
            let evil = body["evil"].as_bool().unwrap_or(false);
            let headers = if evil { json!({"Authorization": "Bearer stolen"}) } else { json!({"x-amz-acl": "private", "x-upload-token": "UT-Hh7-upload"}) };
            (200, String::new(), json!({"url": format!("http://127.0.0.1:{port}/v1/upload/h"), "headers": headers}).to_string())
        }
        ("POST", "/v1/album") => (200, String::new(), json!({"ok": true}).to_string()),
        ("POST", "/v1/graphql") => (200, String::new(), json!({"errors": [{"message": "slow down", "extensions": {"code": "THROTTLED"}}]}).to_string()),
        ("GET", "/v1/to-private") => (302, "location: https://10.0.0.1/feed\r\n".into(), String::new()),
        ("GET", "/v1/to-local") => (302, format!("location: http://localhost:{port}/v1/rss\r\n"), String::new()),
        ("GET", "/v1/dtd") => (200, String::new(), r#"<!DOCTYPE x [<!ENTITY a "b">]><x>&a;</x>"#.into()),
        ("GET", "/v1/form") => (200, String::new(), "a=1&b=x%20y&token=tok-Ww2".into()),
        ("POST", "/v1/oauth/token-key") if body_of(raw).is_null() && raw.contains("client_key=ck-1") && !raw.contains("client_id=") => {
            (200, String::new(), json!({"access_token": "AT-Kk1", "expires_in": 3600, "open_id": "OPEN-Hh2-1", "handle": "ana.example"}).to_string())
        }
        ("POST", "/v1/oauth/token2") => (200, String::new(), json!({"access_token": "AT-Uu8-access", "account": {"id": "U-7"}}).to_string()),
        ("GET", "/v1/users/U-7/posts") => (200, String::new(), json!({"items": [{"id": "P-1"}, {"id": "P-2"}]}).to_string()),
        ("GET", p) if p.starts_with("/v1/posts/") && p.ends_with("/numbers") => {
            (200, String::new(), json!({"likes": if p.contains("P-1") { 5 } else { 9 }}).to_string())
        }
        // Shaped like a sign-in whose token answer is a form, exchanged for a long-lived token, refreshed by a GET.
        ("POST", "/v1/oauth/form-token") => (200, String::new(), "access_token=SHORT-Ss1&user_id=U-9&expires_in=3600".into()),
        ("GET", "/v1/oauth/exchange") if path.contains("fb_exchange_token=SHORT-Ss1") && path.contains("client_secret=cs-Gg4") => {
            (200, String::new(), json!({"access_token": "LONG-Ll1", "expires_in": 5_184_000}).to_string())
        }
        ("GET", "/v1/oauth/refresh_long") if path.contains("access_token=LONG-Ll1") => {
            (200, String::new(), json!({"access_token": "LONG-Ll2", "expires_in": 5_184_000}).to_string())
        }
        ("GET", "/v1/who/U-9") => {
            (200, String::new(), json!({"auth": raw.lines().find_map(|l| l.strip_prefix("authorization: ")).unwrap_or_default()}).to_string())
        }
        // Shaped like a server where each instance registers its client by a request of its own.
        ("POST", "/v1/api/v1/apps") => (200, String::new(), json!({"client_id": "CID-1", "client_secret": "CS-Rr1"}).to_string()),
        ("POST", "/v1/oauth/token3") => (200, String::new(), json!({"access_token": "MAST-Mm1", "expires_in": 1}).to_string()),
        ("GET", "/v1/feed") => (200, String::new(), json!({"items": [{"id": "F-1"}]}).to_string()),
        // Shaped like a media container: made, processed for a while, then published.
        ("POST", "/v1/containers") => (200, String::new(), json!({"id": format!("C-{}-{}", asked(bare), body["kind"].as_str().unwrap_or("ok"))}).to_string()),
        ("GET", p) if p.starts_with("/v1/containers/C-") => {
            let status = match (p.rsplit('-').next().unwrap_or_default(), asked(p)) {
                ("error", _) => "ERROR",
                ("ok", n) if n >= 3 => "FINISHED",
                _ => "IN_PROGRESS",
            };
            (200, String::new(), json!({"id": p.trim_start_matches("/v1/containers/"), "status_code": status}).to_string())
        }
        ("POST", p) if p.starts_with("/v1/publish/") => (200, String::new(), json!({"id": format!("M-{}", p.trim_start_matches("/v1/publish/"))}).to_string()),
        // A thread: each item answers the one before.
        ("POST", "/v1/thread") => (200, String::new(), json!({"id": format!("T-{}", asked(bare)), "reply_to": body["reply_to"]}).to_string()),
        // Shaped like a resumable upload: a session address, chunks with Content-Range, 308 until complete, a probe.
        ("POST", "/v1/resumable") => (200, format!("location: http://127.0.0.1:{port}/v1/session/{}\r\n", asked(bare)), "{}".into()),
        ("PUT", p) if p.starts_with("/v1/session/") => {
            let range = raw.lines().find_map(|l| l.strip_prefix("content-range: bytes ")).unwrap_or_default().to_string();
            let held = with(|s| s.sessions.get(p).cloned().unwrap_or_default());
            let (held, total) = if let Some(total) = range.strip_prefix("*/") {
                (held, total.parse::<usize>().unwrap_or(0))
            } else {
                let (span, total) = range.split_once('/').unwrap_or_default();
                if span.split('-').next().and_then(|o| o.parse::<usize>().ok()) != Some(held.len()) {
                    return (400, String::new(), json!({"error": "wrong offset"}).to_string());
                }
                let held = held + raw.split("\r\n\r\n").nth(1).unwrap_or_default();
                with(|s| s.sessions.insert(p.to_string(), held.clone()));
                (held, total.parse().unwrap_or(0))
            };
            if held.len() == total && !range.starts_with('*') {
                (200, String::new(), json!({"id": "V-1"}).to_string())
            } else if held.is_empty() {
                (308, String::new(), String::new())
            } else {
                (308, format!("range: bytes=0-{}\r\n", held.len() - 1), String::new())
            }
        }
        // Shaped like an upload in INIT, APPEND, FINALIZE commands.
        ("POST", "/v1/media/init") => (200, String::new(), json!({"media_id": format!("X-{}", asked(bare))}).to_string()),
        ("POST", "/v1/media/append") => (204, String::new(), String::new()),
        ("POST", "/v1/media/finalize") => (200, String::new(), json!({"media_id": body["media_id"]}).to_string()),
        // Shaped like an upload whose parts the service names, each with its own address and byte range.
        ("POST", "/v1/videos:init") => (
            200,
            String::new(),
            json!({"instructions": [
            {"uploadUrl": format!("http://127.0.0.1:{port}/v1/part/0"), "firstByte": 0, "lastByte": 5},
            {"uploadUrl": format!("http://127.0.0.1:{port}/v1/part/1"), "firstByte": 6, "lastByte": 9}]})
            .to_string(),
        ),
        ("PUT", p) if p.starts_with("/v1/part/") => (200, format!("etag: E-{}\r\n", p.trim_start_matches("/v1/part/")), String::new()),
        ("POST", "/v1/videos:finalize") => (200, String::new(), body.to_string()),
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
                    let n = *s.asked.entry(bare.clone()).or_default() + 1;
                    s.asked.insert(bare.clone(), n);
                    let at = s.fail_at.get(&bare).filter(|(nth, _)| *nth == n).map(|f| f.1);
                    s.fail_once.remove(&bare).or(at)
                });
                if bare == "/v1/slow-step" {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
                if bare == "/v1/slow-read" {
                    with(|s| {
                        s.inflight.0 += 1;
                        s.inflight.1 = s.inflight.1.max(s.inflight.0);
                    });
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    with(|s| s.inflight.0 -= 1);
                }
                let (status, extra, body) = match failing {
                    Some(0) => {
                        // Received and taken, and its answer lost: the connection gives up waiting.
                        route(&method, &path, &raw, port);
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
    ranges: Mutex<Vec<(u64, u64)>>,
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
    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> ferrum_kernel::Result<Vec<u8>> {
        self.ranges.lock().unwrap().push((range.start, range.end));
        let f = self.files.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))?;
        Ok(f[range.start as usize..(range.end as usize).min(f.len())].to_vec())
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
    assert!(matches!(e, Error::RetryAfter(_, 7)), "{e:?}");
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

/// P2: a step repeated on a delay until its answer says so, bounded, handing "later" to the queue past its patience.
async fn waiting(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let wait = |every: u64, within: u64| json!({"until": "result.status == 'FINISHED'", "fail_if": "result.status in ['ERROR', 'EXPIRED']", "every_secs": every, "within_secs": within});
    let media = |w: Value| {
        json!({"result": {"id": "/publish/id"}, "steps": [
        {"name": "container", "operation": "container", "inputs": {"kind": "inputs.kind"}},
        {"name": "ready", "operation": "container_status", "inputs": {"id": "steps.container.id"}, "wait": w},
        {"name": "publish", "operation": "publish_container", "inputs": {"id": "steps.container.id"}}]})
    };
    let ops = json!({
        "container": {"method": "POST", "path": "/containers", "body": "{\"kind\": {{ inputs.kind | json }}}", "result": {"id": "/id"}},
        "container_status": {"path": "/containers/{{ inputs.id }}", "result": {"status": "/status_code"}},
        "publish_container": {"method": "POST", "path": "/publish/{{ inputs.id }}", "body": "{}", "result": {"id": "/id"}},
        "media": media(wait(1, 30)), "media_patient": media(wait(25, 600)), "media_short": media(wait(1, 1))
    });
    k.save_connection_def(admin, None, &input("gram", json!({"base": base, "operations": ops}), None)).await.unwrap();
    let go = |op: &str, kind: &str, key: &str| call("gram", op, json!({"kind": kind}), Some(key));
    // Processed after three looks, then published once.
    assert_eq!(connections::call(k, admin, None, &go("media", "ok", "m-1")).await.unwrap(), json!({"id": "M-C-1-ok"}));
    assert_eq!((asked("/v1/containers/C-1-ok"), asked("/v1/publish/C-1-ok")), (3, 1));
    // Failed processing fails the call, saying what the service said; nothing is published.
    let e = connections::call(k, admin, None, &go("media", "error", "m-2")).await.unwrap_err().to_string();
    assert!(e.contains("ERROR"), "{e}");
    assert_eq!(asked("/v1/publish/C-2-error"), 0);
    // Beyond its patience the call answers "later" when the service should be asked again; asked again, it carries on
    // from the wait (the container is not made twice).
    let e = connections::call(k, admin, None, &go("media_patient", "ok", "m-3")).await.unwrap_err();
    assert!(matches!(e, Error::RetryAfter(_, 25)), "{e:?}");
    assert!(connections::call(k, admin, None, &go("media_patient", "ok", "m-3")).await.is_err(), "still processing");
    assert_eq!(connections::call(k, admin, None, &go("media_patient", "ok", "m-3")).await.unwrap(), json!({"id": "M-C-3-ok"}));
    assert_eq!(asked("/v1/containers"), 3, "one container each");
    // Bounded in time from the key's first attempt.
    let e = connections::call(k, admin, None, &go("media_short", "never", "m-4")).await.unwrap_err().to_string();
    assert!(e.contains("not ready within 1 s"), "{e}");
    // Only a reading step waits.
    let bad = json!({"base": base, "operations": {"a": {"method": "POST", "path": "/a", "body": "{}"}, "b": {"steps": [{"name": "x", "operation": "a", "wait": {"until": "true"}}]}}});
    assert!(k.save_connection_def(admin, None, &input("bad", bad, None)).await.unwrap_err().to_string().contains("waits only on an operation that reads"));
}

/// The form fields of a multipart body, by name.
fn parts(raw: &str) -> BTreeMap<String, String> {
    raw.split("Content-Disposition: form-data; name=\"")
        .skip(1)
        .filter_map(|p| {
            let (name, rest) = p.split_once('"')?;
            let value = rest.split_once("\r\n\r\n")?.1.split("\r\n--").next()?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// P3: a step for each item of a list, and for each chunk of a stored file, read a range at a time.
async fn loops(k: &Arc<Kernel>, admin: &Ctx, base: &str, store: &Memory) {
    let video = k.store_file(admin, None, "clip.mp4", "video/mp4", b"0123456789").await.unwrap()["id"].as_str().unwrap().to_string();
    let big = k.store_file(admin, None, "long.mp4", "video/mp4", &[b'z'; 20]).await.unwrap()["id"].as_str().unwrap().to_string();
    let ops = json!({
        "post_item": {"method": "POST", "path": "/thread", "body": "{\"text\": {{ inputs.text | json }}, \"reply_to\": {{ inputs.reply_to | json }}}", "result": {"id": "/id"}},
        "thread": {"result": {"ids": "/items"}, "steps": [
            {"name": "items", "operation": "post_item", "for_each": "inputs.texts", "inputs": {"text": "item", "reply_to": "previous.id if previous else none"}}]},
        "start_upload": {"method": "POST", "path": "/resumable", "body": "{}", "result": {"url": {"header": "location"}}},
        "put_chunk": {"method": "PUT", "url": "{{ inputs.url }}", "headers": {"content-range": "{{ inputs.range }}"}, "body": {"file": "{{ inputs.file }}"},
                      "success": [308], "result": {"id": "/id"}},
        "probe": {"method": "PUT", "url": "{{ inputs.url }}", "headers": {"content-range": "bytes */{{ inputs.total }}"}, "changes": false, "success": [308],
                  "result": {"offset": {"header": "range"}}},
        "video": {"result": {"id": "/upload/id"}, "steps": [
            {"name": "start", "operation": "start_upload"},
            {"name": "upload", "operation": "put_chunk", "chunks": {"file": "inputs.file", "bytes": 4,
                                                                 "offset_from": {"operation": "probe", "inputs": {"url": "steps.start.url", "total": "chunk.total"}}},
             "inputs": {"url": "steps.start.url", "file": "inputs.file", "range": "'bytes ' ~ chunk.offset ~ '-' ~ chunk.end ~ '/' ~ chunk.total"}}]},
        "init": {"method": "POST", "path": "/media/init", "body": "{}", "result": {"media_id": "/media_id"}},
        "append": {"method": "POST", "path": "/media/append", "form": {"media_id": "{{ inputs.media_id }}", "segment_index": "{{ inputs.index }}",
                                                                       "media": {"file": "{{ inputs.file }}"}}},
        "finalize": {"method": "POST", "path": "/media/finalize", "body": "{\"media_id\": {{ inputs.media_id | json }}}", "result": {"media_id": "/media_id"}},
        "commands": {"result": {"media_id": "/finalize/media_id"}, "steps": [
            {"name": "init", "operation": "init"},
            {"name": "append", "operation": "append", "chunks": {"file": "inputs.file", "bytes": 4, "restart_from": "init"},
             "inputs": {"media_id": "steps.init.media_id", "index": "chunk.index", "file": "inputs.file"}},
            {"name": "finalize", "operation": "finalize", "inputs": {"media_id": "steps.init.media_id"}}]},
        "init_parts": {"method": "POST", "path": "/videos:init", "body": "{}", "result": {"instructions": "/instructions"}},
        "put_part": {"method": "PUT", "url": "{{ inputs.url }}", "body": {"file": "{{ inputs.file }}"}, "result": {"etag": {"header": "etag"}}},
        "finalize_parts": {"method": "POST", "path": "/videos:finalize", "body": "{\"parts\": {{ inputs.parts | json }}}"},
        "parts": {"result": {"parts": "/finalize/parts"}, "steps": [
            {"name": "init", "operation": "init_parts"},
            {"name": "put", "operation": "put_part", "for_each": "steps.init.instructions", "inputs": {"url": "item.uploadUrl", "file": "inputs.file"},
             "range": {"file": "inputs.file", "offset": "item.firstByte", "size": "item.lastByte - item.firstByte + 1"}},
            {"name": "finalize", "operation": "finalize_parts", "inputs": {"parts": "steps.put | map(attribute='etag') | list"}}]}
    });
    k.save_connection_def(admin, None, &input("tube", json!({"base": base, "timeout_ms": 1000, "operations": ops}), None)).await.unwrap();

    // Each item a request of its own, answering the one before; resumed at the item that did not go.
    fail_at("/v1/thread", 2, 503);
    let thread = || call("tube", "thread", json!({"texts": ["one", "two", "three"]}), Some("t-1"));
    assert!(connections::call(k, admin, None, &thread()).await.is_err());
    let r = connections::call(k, admin, None, &thread()).await.unwrap();
    assert_eq!(r["ids"].as_array().unwrap().iter().map(|i| i["id"].clone()).collect::<Vec<_>>(), vec![json!("T-1"), json!("T-3"), json!("T-4")]);
    assert_eq!(asked("/v1/thread"), 4, "the first item once");
    assert_eq!(body_of(&last("POST /v1/thread"))["reply_to"], "T-3", "each answers the one before");

    // A file in chunks, a range read at a time (never the whole file); a chunk whose answer is lost is resumed where
    // the service says it holds the upload.
    let whole = *store.whole.lock().unwrap();
    fail_at("/v1/session/1", 2, 0);
    let upload = || call("tube", "video", json!({"file": video}), Some("v-1"));
    assert!(connections::call(k, admin, None, &upload()).await.is_err(), "the second chunk's answer lost");
    assert_eq!(connections::call(k, admin, None, &upload()).await.unwrap(), json!({"id": "V-1"}));
    assert_eq!(with(|s| s.sessions["/v1/session/1"].clone()), "0123456789", "each byte once, in order");
    assert_eq!(asked("/v1/session/1"), 4, "two chunks, the probe, the last chunk");
    assert!(last("PUT /v1/session/1").contains("content-range: bytes 8-9/10"));
    assert_eq!(*store.whole.lock().unwrap(), whole, "never read whole");
    assert_eq!(store.ranges.lock().unwrap().clone(), vec![(0, 4), (4, 8), (8, 10)]);

    // Where the service reports no offset, a lost chunk restarts that upload, from its first command, and only it.
    fail_at("/v1/media/append", 2, 0);
    let commands = || call("tube", "commands", json!({"file": video}), Some("x-1"));
    assert!(connections::call(k, admin, None, &commands()).await.is_err());
    assert_eq!(connections::call(k, admin, None, &commands()).await.unwrap(), json!({"media_id": "X-2"}));
    let appended: Vec<(String, String)> = with(|s| s.seen.iter().filter(|r| r.starts_with("POST /v1/media/append")).map(|r| parts(r)).collect::<Vec<_>>())
        .into_iter()
        .map(|p| (p["media_id"].clone(), p["segment_index"].clone()))
        .collect();
    let want: Vec<(String, String)> =
        [("X-1", "0"), ("X-1", "1"), ("X-2", "0"), ("X-2", "1"), ("X-2", "2")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    assert_eq!(appended, want);
    assert_eq!(asked("/v1/media/finalize"), 1);

    // Parts the service names, each its own range and address; their answers' headers collected.
    let r = connections::call(k, admin, None, &call("tube", "parts", json!({"file": video}), Some("l-1"))).await.unwrap();
    assert_eq!(r, json!({"parts": ["E-0", "E-1"]}));
    assert!(last("PUT /v1/part/0").ends_with("\r\n\r\n012345") && last("PUT /v1/part/1").ends_with("\r\n\r\n6789"));

    // Bounded: a file over the configured cap for chunked sends; chunks and items both, or none, refused when saved.
    let e = connections::call(k, admin, None, &call("tube", "video", json!({"file": big}), Some("v-2"))).await.unwrap_err().to_string();
    assert!(e.contains("larger than the 16 bytes"), "{e}");
    let bad = |step: Value| json!({"base": base, "operations": {"a": {"method": "PUT", "path": "/a"}, "b": {"steps": [step]}}});
    for (step, says) in [
        (json!({"name": "x", "operation": "a", "for_each": "inputs.l", "chunks": {"file": "inputs.f", "bytes": 4}}), "for_each or chunks"),
        (json!({"name": "x", "operation": "a", "chunks": {"file": "inputs.f", "bytes": 0}}), "chunks.bytes"),
        (json!({"name": "x", "operation": "a", "chunks": {"file": "inputs.f", "bytes": 4, "restart_from": "nowhere"}}), "restart_from"),
    ] {
        let e = k.save_connection_def(admin, None, &input("bad", bad(step), None)).await.unwrap_err().to_string();
        assert!(e.contains(says), "{says}: {e}");
    }
}

async fn drain(k: &Kernel) {
    for _ in 0..5 {
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        while k.run_jobs_once(4).await.unwrap() > 0 {}
    }
}

/// P4: functions in templates and the clock, fixed at a key's first attempt so that a retry sends the same thing.
async fn functions(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let body = "{\"rkey\": {{ key | tid | json }}, \"createdAt\": {{ now | json }}, \"day\": {{ today | json }}, \"text\": {{ inputs.text | json }},                 \"facets\": {{ inputs.text | entities('bytes') | json }}, \"length\": {{ inputs.text | length_in('graphemes') }},                 \"sig\": {{ inputs.text | hmac(secret.token) | json }}, \"short\": {{ inputs.text | truncate(5) | json }},                 \"tags\": {{ inputs.tags | split(',') | join(' ') | json }}, \"q\": {{ inputs.text | urlencode | json }},                 \"when\": {{ now | strftime('%Y/%m/%d') | json }}, \"none\": {{ inputs.missing | default('-') | json }}}";
    let ops = json!({
        "create": {"method": "POST", "path": "/records", "body": body, "result": {"uri": "/uri"}},
        "create_twice": {"steps": [{"name": "rec", "operation": "create", "inputs": {"text": "inputs.text", "tags": "inputs.tags"}}]},
        "feed": {"path": "/feed"}
    });
    let pulls = json!([{"name": "feed", "operation": "feed", "every_minutes": 60, "items_at": "/items", "cursor": "time",
                        "map": {"entity": "Post", "by": {"title": "'pulled ' + today + ' ' + item.id"}}}]);
    k.save_connection_def(
        admin,
        None,
        &input("sky", json!({"base": base, "auth": {"type": "bearer", "secret": "token"}, "operations": ops, "pulls": pulls}), Some(json!({"token": SECRET}))),
    )
    .await
    .unwrap();
    let text = "Hi @ana.example.test 👋🏽 see https://ferrum.example/a. #news";
    fail_once("/v1/records", 503);
    let make = || call("sky", "create", json!({"text": text, "tags": "a,b"}), Some("r-1"));
    assert!(connections::call(k, admin, None, &make()).await.is_err());
    let first = body_of(&last("POST /v1/records"));
    connections::call(k, admin, None, &make()).await.unwrap();
    let second = body_of(&last("POST /v1/records"));
    assert_eq!(first, second, "a retry sends the same moment and the same record key");
    let rkey = first["rkey"].as_str().unwrap();
    assert_eq!(rkey.len(), 13, "{rkey}");
    assert!(rkey.chars().all(|c| "234567abcdefghijklmnopqrstuvwxyz".contains(c)));
    assert_eq!(first["day"], first["createdAt"].as_str().unwrap()[..10]);
    assert_eq!(first["when"], first["createdAt"].as_str().unwrap()[..10].replace('-', "/"));
    // Byte offsets, as a facet wants them: the emoji counts four and eight bytes, one grapheme.
    let link = first["facets"].as_array().unwrap().iter().find(|f| f["type"] == "link").unwrap().clone();
    assert_eq!(&text.as_bytes()[link["start"].as_u64().unwrap() as usize..link["end"].as_u64().unwrap() as usize], b"https://ferrum.example/a");
    assert_eq!(first["facets"][0], json!({"type": "mention", "text": "@ana.example.test", "value": "ana.example.test", "start": 3, "end": 20}));
    assert_eq!(first["facets"][2]["value"], "news");
    assert_eq!(first["length"], text.chars().count() - 1, "the emoji and its tone are one grapheme");
    assert_eq!((first["short"].as_str(), first["tags"].as_str(), first["none"].as_str()), (Some("Hi @a"), Some("a b"), Some("-")));
    assert!(first["q"].as_str().unwrap().starts_with("Hi%20%40ana.example.test%20"));
    assert_eq!(first["sig"].as_str().unwrap().len(), 64, "hex HMAC-SHA256");
    assert!(!first.to_string().contains(SECRET), "the key signs, it is not sent");
    // A later key sorts after.
    connections::call(k, admin, None, &call("sky", "create", json!({"text": "x", "tags": ""}), Some("r-2"))).await.unwrap();
    assert!(body_of(&last("POST /v1/records"))["rkey"].as_str().unwrap() > rkey);
    // In a step's expression too, under its own key and the operation's clock.
    let r = connections::call(k, admin, None, &call("sky", "create_twice", json!({"text": "y", "tags": ""}), Some("r-3"))).await.unwrap();
    assert!(r["rec"]["uri"].as_str().unwrap().starts_with("at://did/post/"));
    // In a mapping: `today` and `now`.
    let id: Uuid = k.connection_defs(admin).await.unwrap().iter().find(|c| c["name"] == "sky").unwrap()["id"].as_str().unwrap().parse().unwrap();
    k.pull_now(admin, id, "feed").await.unwrap();
    drain(k).await;
    let title = format!("pulled {} F-1", chrono::Utc::now().date_naive());
    let table = admin.schema.entity("Post").unwrap().table.clone();
    let made = count(k, admin.tenant, &format!("select count(*) from {table} where tenant_id = $1 and title = '{title}'")).await;
    assert_eq!(made, 1, "{:?}", k.connection_pulls(admin, id).await.unwrap());
}

/// P5: the service's Retry-After reaches an app's call; a description's limits hold across workers.
async fn limits(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let ops = json!({"busy": {"path": "/busy"}, "slow": {"path": "/slow-read"}, "counted": {"path": "/counted"}});
    k.save_connection_def(admin, None, &input("lim", json!({"base": base, "operations": ops, "limit": {"per": 2, "every": 60, "concurrent": 1}}), None))
        .await
        .unwrap();
    let e = connections::call(k, admin, None, &call("lim", "busy", json!({}), None)).await.unwrap_err();
    assert!(matches!(e, Error::RetryAfter(_, 7)), "the service's own wait: {e:?}");
    // Two calls a minute (the busy one counted too): the next is not sent, and says when to come back.
    connections::call(k, admin, None, &call("lim", "counted", json!({}), None)).await.unwrap();
    let e = connections::call(k, admin, None, &call("lim", "counted", json!({}), None)).await.unwrap_err();
    assert!(matches!(e, Error::RetryAfter(_, s) if (1..=60).contains(&s)), "{e:?}");
    assert_eq!(asked("/v1/counted"), 1, "not sent past the limit");
    // One at a time: two workers' calls do not overlap.
    let d = json!({"base": base, "operations": ops, "limit": {"concurrent": 1}});
    k.save_connection_def(admin, None, &input("one", d, None)).await.unwrap();
    let (a, b, c1, c2) = (k.clone(), k.clone(), admin.clone(), admin.clone());
    let one = tokio::spawn(async move { connections::call(&a, &c1, None, &call("one", "slow", json!({}), None)).await });
    let two = tokio::spawn(async move { connections::call(&b, &c2, None, &call("one", "slow", json!({}), None)).await });
    assert!(one.await.unwrap().is_ok() && two.await.unwrap().is_ok(), "the second waited its turn");
    assert_eq!(with(|s| s.inflight.1), 1, "never two at once");
    let bad = json!({"base": base, "limit": {"per": 0, "every": 60}});
    assert!(k.save_connection_def(admin, None, &input("bad", bad, None)).await.unwrap_err().to_string().contains("limit"));
    // BD-74: an operation's own limit, counted apart from the description's and from the other operations.
    let ops = json!({"post": {"path": "/counted", "limit": {"per": 1, "every": 60}}, "read": {"path": "/ok-true"}});
    k.save_connection_def(admin, None, &input("oplim", json!({"base": base, "operations": ops, "limit": {"per": 5, "every": 60}}), None)).await.unwrap();
    connections::call(k, admin, None, &call("oplim", "post", json!({}), None)).await.unwrap();
    let e = connections::call(k, admin, None, &call("oplim", "post", json!({}), None)).await.unwrap_err();
    assert!(matches!(&e, Error::RetryAfter(m, _) if m.contains("oplim's post")), "{e:?}");
    for _ in 0..3 {
        connections::call(k, admin, None, &call("oplim", "read", json!({}), None)).await.expect("reading is not posting");
    }
    let e = connections::call(k, admin, None, &call("oplim", "read", json!({}), None)).await.unwrap_err();
    assert!(matches!(&e, Error::RetryAfter(m, _) if !m.contains("'s")), "the description's five, the refused post counted: {e:?}");
    let bad = json!({"base": base, "operations": {"x": {"path": "/x", "limit": {"concurrent": 0}}}});
    assert!(k.save_connection_def(admin, None, &input("bad", bad, None)).await.unwrap_err().to_string().contains("limit"));
}

fn query(url: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(url).unwrap().query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// P6: a mapping a package or an administrator declared may write as the system, so a read-only field is set.
async fn as_system(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let tenant = admin.tenant;
    let ana_id = k.create_user(admin, "ana@social.test", "Ana", None, &[], false).await.unwrap();
    k.set_user_roles(admin, ana_id, &["Poster".into()]).await.unwrap();
    let ana = k.load_ctx(tenant, ana_id, None, "it".into()).await.unwrap();
    let provider = json!({"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/oauth/token"), "client_id": "ferrum-test"});
    let map = |at: &str, system: bool| {
        let mut m = json!({"entity": "Post", "by": {"title": "/title"}, "set": {"external_id": "/id"}, "outside": true});
        if system {
            m["as"] = json!("system");
        }
        json!({"name": at, "operation": at, "every_minutes": 15, "items_at": "/items", "cursor": "time", "map": m})
    };
    let d = json!({"base": base, "auth": {"type": "oauth", "owner": "person", "provider": provider}, "roles": ["Poster"],
                   "operations": {"mine": {"path": "/mine"}, "mine2": {"path": "/mine2"}}, "pulls": [map("mine", true), map("mine2", false)]});
    let id = k.save_connection_def(admin, None, &input("own", d.clone(), None)).await.unwrap();
    // Only an administrator (or a package, whose descriptions an administrator takes up) writes a description.
    assert!(matches!(k.save_connection_def(&ana, None, &input("hers", d, None)).await.unwrap_err(), Error::Forbidden(_)));
    let session = Uuid::now_v7();
    let state = query(&k.connect_start(&ana, id, session).await.unwrap())["state"].clone();
    k.connect_finish(&ana, session, &state, Some("CODE-1"), None).await.unwrap();
    for pull in ["mine", "mine2"] {
        k.pull_now(admin, id, pull).await.unwrap();
    }
    drain(k).await;
    let table = admin.schema.entity("Post").unwrap().table.clone();
    let row = |title: &str| format!("select count(*) from {table} where tenant_id = $1 and title = '{title}' and created_by = '{ana_id}'");
    assert_eq!(count(k, tenant, &(row("Native one") + " and external_id = 'N-1'")).await, 1, "as the system: the read-only field set, still hers");
    assert_eq!(count(k, tenant, &(row("Native two") + " and external_id is null")).await, 1, "as her: the read-only field left alone");
    // Her own write cannot set it.
    let mine = k.insert(&ana, "Post", serde_json::from_value(json!({"title": "Typed", "external_id": "forged"})).unwrap()).await.unwrap();
    assert!(mine["external_id"].is_null());
}

/// P7: answers in XML (RSS, Atom, plain) and form-encoded, read into the shape pointers and mappings work on.
async fn xml_answers(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let ops = json!({
        "rss": {"path": "/rss", "answer": "xml", "lists": ["item"]},
        "atom": {"path": "/atom", "answer": "xml", "lists": ["entry", "link"]},
        "dtd": {"path": "/dtd", "answer": "xml"},
        "form": {"path": "/form", "answer": "form", "result": {"b": "/b", "a": "/a"}},
        "creator": {"path": "/rss", "answer": "xml", "lists": ["item"], "result": {"by": "/rss/channel/item/0/dc:creator", "second": "/rss/channel/item/1/guid/#text"}}
    });
    let pull = |name: &str, items: &str, by: &str, id: &str| {
        json!({"name": name, "operation": name, "every_minutes": 60, "items_at": items, "cursor": "time",
                                                                "map": {"entity": "Post", "by": {"title": by}, "set": {"external_id": id}}})
    };
    let d = json!({"base": base, "operations": ops, "pulls": [pull("rss", "/rss/channel/item", "/title", "/link"), pull("atom", "/feed/entry", "/title/#text", "/link/0/@href")]});
    let id = k.save_connection_def(admin, None, &input("feeds", d, None)).await.unwrap();
    for p in ["rss", "atom"] {
        k.pull_now(admin, id, p).await.unwrap();
    }
    drain(k).await;
    let table = admin.schema.entity("Post").unwrap().table.clone();
    for (title, link) in [("First & best", "= 'https://news.example/1'"), ("Second <b>", "is null"), ("Atom one", "= 'https://news.example/a1'")] {
        let n = count(k, admin.tenant, &format!("select count(*) from {table} where tenant_id = $1 and title = '{title}' and external_id {link}")).await;
        assert_eq!(n, 1, "{title}: {:?}", k.connection_pulls(admin, id).await.unwrap());
    }
    let r = connections::call(k, admin, None, &call("feeds", "creator", json!({}), None)).await.unwrap();
    assert_eq!(r, json!({"by": "Ana", "second": "g2"}), "a prefixed element, an element's text beside its attribute");
    assert_eq!(connections::call(k, admin, None, &call("feeds", "form", json!({}), None)).await.unwrap(), json!({"b": "x y", "a": "1"}));
    let e = connections::call(k, admin, None, &call("feeds", "dtd", json!({}), None)).await.unwrap_err().to_string();
    assert!(e.contains("not XML"), "a document type (and its entities) is refused: {e}");
    let bad = json!({"name": "bad", "label": "bad", "description": {"base": base, "operations": {"a": {"path": "/a", "answer": "yaml"}}}});
    assert!(serde_json::from_value::<ConnectionInput>(bad).unwrap_err().to_string().contains("unknown variant `yaml`"));
}

/// P8: a pull that asks a second operation for each item, bounded, and inputs taken from the signed-in account.
async fn pull_each(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let provider = json!({"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/oauth/token2"), "client_id": "ferrum-test",
                          "account_at": "/account/id"});
    let ops = json!({"own_posts": {"path": "/users/{{ inputs.user }}/posts", "inputs": ["user", "cursor"]},
                     "mine": {"path": "/users/{{ account }}/posts"},
                     "numbers": {"path": "/posts/{{ inputs.id }}/numbers", "result": {"likes": "/likes"}}});
    let pull = |name: &str, max: u32| {
        json!({"name": name, "operation": "own_posts", "every_minutes": 60, "items_at": "/items", "cursor": "time",
        "inputs": {"user": "account"}, "each": {"operation": "numbers", "inputs": {"id": "item.id"}, "as": "numbers", "max": max},
        "map": {"entity": "Post", "by": {"title": "'own ' + item.id"}, "set": {"external_id": "string(item.numbers.likes)"}}})
    };
    let d = json!({"base": base, "auth": {"type": "oauth", "provider": provider}, "operations": ops, "pulls": [pull("own", 10), pull("few", 1)]});
    let id = k.save_connection_def(admin, None, &input("net_own", d, None)).await.unwrap();
    let session = Uuid::now_v7();
    let state = query(&k.connect_start(admin, id, session).await.unwrap())["state"].clone();
    k.connect_finish(admin, session, &state, Some("CODE-2"), None).await.unwrap();
    // An operation's template reads the account too.
    let r = connections::call(k, admin, None, &call("net_own", "mine", json!({}), None)).await.unwrap();
    assert_eq!(r["items"][0]["id"], "P-1");
    let before = asked("/v1/posts/P-1/numbers");
    k.pull_now(admin, id, "own").await.unwrap();
    drain(k).await;
    let table = admin.schema.entity("Post").unwrap().table.clone();
    for (title, likes) in [("own P-1", "5"), ("own P-2", "9")] {
        let n = count(k, admin.tenant, &format!("select count(*) from {table} where tenant_id = $1 and title = '{title}' and external_id = '{likes}'")).await;
        assert_eq!(n, 1, "{title}: {:?}", k.connection_pulls(admin, id).await.unwrap());
    }
    assert_eq!(asked("/v1/posts/P-1/numbers"), before + 1, "once for each item");
    // Bounded: a page with more items than the pull asks about fails, saying so, and asks nothing.
    k.pull_now(admin, id, "few").await.unwrap();
    drain(k).await;
    let few = k.connection_pulls(admin, id).await.unwrap().into_iter().find(|p| p["name"] == "few").unwrap();
    assert!(few["last_error"].as_str().unwrap_or_default().contains("more than the 1"), "{few}");
    assert_eq!(asked("/v1/posts/P-1/numbers"), before + 1);
    let bad = json!({"base": base, "operations": {"a": {"path": "/a"}, "w": {"method": "POST", "path": "/w", "body": "{}"}},
                     "pulls": [{"name": "p", "operation": "a", "every_minutes": 60, "items_at": "/i", "cursor": "time", "each": {"operation": "w", "as": "x"},
                                "map": {"entity": "Post", "by": {"title": "/t"}}}]});
    assert!(k.save_connection_def(admin, None, &input("bad", bad, None)).await.unwrap_err().to_string().contains("each"));
}

/// P9: sign-in extras: a form-encoded token answer with fields kept, a long-lived token exchanged after the sign-in,
/// a refresh by a GET, a client registered at each server by an operation, a token that never expires.
async fn signing_in(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let tenant = admin.tenant;
    let provider = json!({"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/oauth/form-token"), "client_id": "app-1",
                          "client_secret": "client", "keep": ["user_id"], "after": ["exchange"], "refresh": "refresh_long"});
    let ops = json!({
        "exchange": {"path": "/oauth/exchange", "query": {"grant_type": "fb_exchange_token", "client_id": "{{ inputs.client_id }}",
                     "client_secret": "{{ secret.client }}", "fb_exchange_token": "{{ inputs.access_token }}"},
                     "result": {"access_token": "/access_token", "expires_in": "/expires_in"}},
        "refresh_long": {"path": "/oauth/refresh_long", "query": {"access_token": "{{ inputs.access_token }}"},
                         "result": {"access_token": "/access_token", "expires_in": "/expires_in"}},
        "me": {"path": "/who/{{ auth.user_id }}"}
    });
    let id = k
        .save_connection_def(
            admin,
            None,
            &input("page", json!({"base": base, "auth": {"type": "oauth", "provider": provider}, "operations": ops}), Some(json!({"client": "cs-Gg4"}))),
        )
        .await
        .unwrap();
    let session = Uuid::now_v7();
    let state = query(&k.connect_start(admin, id, session).await.unwrap())["state"].clone();
    k.connect_finish(admin, session, &state, Some("CODE-3"), None).await.unwrap();
    let me = || call("page", "me", json!({}), None);
    assert_eq!(connections::call(k, admin, None, &me()).await.unwrap(), json!({"auth": "Bearer ••••••"}), "a token is masked in an answer");
    assert!(last("GET /v1/who/U-9").contains("authorization: Bearer LONG-Ll1"), "exchanged; the kept user id in the path");
    assert!(connections::call(k, admin, None, &call("page", "exchange", json!({}), None)).await.unwrap_err().to_string().contains("signs in"));
    // Refreshed by its GET when about to expire, without a refresh token; what was kept stays.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update conn_token set expires_at = now() - interval '1 minute' where tenant_id = $1 and conn_id = $2")
        .bind(tenant)
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    connections::call(k, admin, None, &me()).await.unwrap();
    assert!(last("GET /v1/who/U-9").contains("authorization: Bearer LONG-Ll2"));
    assert_eq!(asked("/v1/oauth/refresh_long"), 1);
    assert!(
        rests(k, &["SHORT-Ss1", "LONG-Ll1", "LONG-Ll2", "cs-Gg4"]).await.is_empty(),
        "{:?}",
        rests(k, &["SHORT-Ss1", "LONG-Ll1", "LONG-Ll2", "cs-Gg4"]).await
    );

    // A client registered at this server by its own request, once; its token kept without an expiry.
    let provider = json!({"authorize_url": "https://instance.example.test/oauth/authorize", "token_url": format!("{base}/oauth/token3"),
                          "registration": "register_app", "scopes": ["read", "write"], "never_expires": true});
    let ops = json!({"register_app": {"method": "POST", "path": "/api/v1/apps", "form": {"client_name": "{{ inputs.client_name }}",
                     "redirect_uris": "{{ inputs.redirect_uri }}", "scopes": "{{ inputs.scopes }}"}, "result": {"client_id": "/client_id", "client_secret": "/client_secret"}},
                     "me": {"path": "/who/U-9"}});
    let inst = k
        .save_connection_def(admin, None, &input("instance", json!({"base": base, "auth": {"type": "oauth", "provider": provider}, "operations": ops}), None))
        .await
        .unwrap();
    for _ in 0..2 {
        let q = query(&k.connect_start(admin, inst, session).await.unwrap());
        assert_eq!(q["client_id"], "CID-1");
    }
    assert_eq!(asked("/v1/api/v1/apps"), 1, "registered once");
    let sent = parts(&last("POST /v1/api/v1/apps"));
    let form = query(&format!("http://f/?{}", last("POST /v1/api/v1/apps").split("\r\n\r\n").nth(1).unwrap()));
    assert!(sent.is_empty() && form["redirect_uris"] == "https://ferrum.example.test/api/connections/callback" && form["scopes"] == "read write", "{form:?}");
    let state = query(&k.connect_start(admin, inst, session).await.unwrap())["state"].clone();
    k.connect_finish(admin, session, &state, Some("CODE-4"), None).await.unwrap();
    let token = last("POST /v1/oauth/token3");
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode("CID-1:CS-Rr1");
    assert!(token.contains(&format!("authorization: Basic {basic}")), "the registered client proves itself as registered: {token}");
    assert!(k.connection_defs(admin).await.unwrap().iter().find(|c| c["name"] == "instance").unwrap()["account"]["expires_at"].is_null(), "never expires");
    connections::call(k, admin, None, &call("instance", "me", json!({}), None)).await.unwrap();
    assert!(last("GET /v1/who/U-9").contains("authorization: Bearer MAST-Mm1"));
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
        connection_hosts: vec!["127.0.0.1".into(), "localhost".into()],
        public_url: Some("https://ferrum.example.test".into()),
        connection_upload_bytes: 16,
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
    waiting(&k, &admin, &base).await;
    loops(&k, &admin, &base, &store).await;
    functions(&k, &admin, &base).await;
    limits(&k, &admin, &base).await;
    as_system(&k, &admin, &base).await;
    xml_answers(&k, &admin, &base).await;
    pull_each(&k, &admin, &base).await;
    signing_in(&k, &admin, &base).await;
    any_host(&k, &admin, port).await;
    refusals(&k, &admin, &base).await;
    provider_words(&k, &admin, &base).await;
    schemes(&k, &admin, &base).await;
    repeated_parts(&k, &admin, &base).await;
    named_headers(&k, &admin, &base).await;
    handshake(&k, &admin).await;
}

/// BD-78: the check a service makes before it delivers: the challenge echoed when the token matches, else refused alike.
async fn handshake(k: &Arc<Kernel>, admin: &Ctx) {
    let d = json!({"test": true, "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "hook"},
        "map": {"entity": "Post", "by": {"external_id": "item.id"}, "set": {"title": "item.title"}}},
        "handshake": {"token": "hub.verify_token", "challenge": "hub.challenge", "secret": "verify", "expect": {"hub.mode": "subscribe"}}});
    let id = k.save_connection_def(admin, None, &input("hooked", d, Some(json!({"hook": "tok-Ww1-hook", "verify": "vt-Pp3-verify"})))).await.unwrap();
    let q = |token: &str, mode: &str| -> BTreeMap<String, String> {
        [("hub.verify_token", token), ("hub.challenge", "1158201444"), ("hub.mode", mode)].into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    };
    assert_eq!(k.connection_handshake(id, &q("vt-Pp3-verify", "subscribe")).await.unwrap(), "1158201444");
    for (token, mode) in [("vt-Pp3-verifx", "subscribe"), ("", "subscribe"), ("vt-Pp3-verify", "unsubscribe"), ("tok-Ww1-hook", "subscribe")] {
        assert!(matches!(k.connection_handshake(id, &q(token, mode)).await, Err(Error::Unauthorized(_))), "{token} {mode}");
    }
    assert!(matches!(k.connection_handshake(Uuid::now_v7(), &q("vt-Pp3-verify", "subscribe")).await, Err(Error::Unauthorized(_))), "unknown alike");
}

/// BD-76: an upload step whose headers an earlier answer named, for that request only, masked, never credentials.
async fn named_headers(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let file = k.store_file(admin, None, "v.mp4", "video/mp4", b"VIDEO").await.unwrap()["id"].as_str().unwrap().to_string();
    let ops = json!({
        "presign": {"method": "POST", "path": "/presign", "changes": false, "body": "{\"evil\": {{ inputs.evil | json }}}", "result": {"url": "/url", "headers": "/headers"}},
        "put": {"method": "PUT", "url": "{{ inputs.url }}", "changes": false, "credentials": false, "headers_from": "{{ inputs.headers | json }}", "body": {"file": "{{ inputs.file }}"}},
        "after": {"path": "/who/U-9"},
        "upload": {"result": {"done": "/put"}, "steps": [
            {"name": "presign", "operation": "presign", "inputs": {"evil": "inputs.evil"}},
            {"name": "put", "operation": "put", "inputs": {"url": "steps.presign.url", "headers": "steps.presign.headers", "file": "inputs.file"}}]}
    });
    let desc = json!({"base": base, "auth": {"type": "bearer", "secret": "token"}, "operations": ops});
    let id = k.save_connection_def(admin, None, &input("named", desc, Some(json!({"token": SECRET})))).await.unwrap();
    k.debug_connection(admin, id, 5).await.unwrap();
    connections::call(k, admin, None, &call("named", "upload", json!({"file": file, "evil": false}), Some("u-1"))).await.unwrap();
    let put = last("PUT /v1/upload/h");
    assert!(put.contains("x-upload-token: UT-Hh7-upload") && put.contains("x-amz-acl: private") && !put.contains(SECRET), "{put}");
    connections::call(k, admin, None, &call("named", "after", json!({}), None)).await.unwrap();
    assert!(!last("GET /v1/who/U-9").contains("x-upload-token"), "for that request only");
    let log = k.connection_log(admin, id, 20).await.unwrap();
    let captured = log.iter().find(|c| c["debug"]["request"]["method"] == "PUT").map(|c| c["debug"]["request"].to_string()).unwrap_or_default();
    assert!(captured.contains("x-upload-token") && !captured.contains("UT-Hh7-upload"), "masked in the request's capture: {captured}");
    let e = connections::call(k, admin, None, &call("named", "upload", json!({"file": file, "evil": true}), Some("u-2"))).await.unwrap_err();
    assert!(e.to_string().contains("may not set Authorization"), "{e}");
    let bad = json!({"base": base, "operations": {"x": {"path": "/x", "headers_from": "{\"a\": {{ secret.token | json }}}"}}});
    assert!(k.save_connection_def(admin, None, &input("named2", bad, None)).await.is_err());
}

/// BD-75: one multipart request carrying a part for each file of a list, and a value for each, from the same list.
async fn repeated_parts(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let mut files = vec![];
    for (i, body) in ["AAA", "BBB", "CCC"].into_iter().enumerate() {
        files.push(k.store_file(admin, None, &format!("p{i}.jpg"), "image/jpeg", body.as_bytes()).await.unwrap()["id"].as_str().unwrap().to_string());
    }
    let op = json!({"method": "POST", "path": "/album", "changes": false,
        "headers": {"content-type": "multipart/form-data"},
        "form_fields": "[{% for f in inputs.media %}{\"name\": \"caption{{ loop.index0 }}\", \"value\": \"#{{ loop.index }}\"}{% if not loop.last %},{% endif %}{% endfor %}]",
        "form": {"files[{{ index }}]": {"file": "{{ item }}", "each": "inputs.media", "name": "f{{ index }}.jpg"}}});
    k.save_connection_def(admin, None, &input("album", json!({"base": base, "operations": {"send": op}}), None)).await.unwrap();
    connections::call(k, admin, None, &call("album", "send", json!({"media": files}), None)).await.unwrap();
    let sent = parts(&last("POST /v1/album"));
    assert_eq!(
        sent.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
        ["caption0=#1", "caption1=#2", "caption2=#3", "files[0]=AAA", "files[1]=BBB", "files[2]=CCC"]
    );
    assert!(last("POST /v1/album").contains("filename=\"f2.jpg\""));
    let bad = json!({"base": base, "operations": {"x": {"method": "POST", "path": "/x", "form": {"f": {"file": "x", "each": "inputs.(("}}}}});
    assert!(k.save_connection_def(admin, None, &input("album2", bad, None)).await.is_err(), "checked when saved");
}

/// BD-73: an operation that sends the token under another scheme, or alone.
async fn schemes(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let ops = json!({"plain": {"path": "/who/U-9"}, "oauth": {"path": "/who/U-9", "auth_scheme": "OAuth"}, "alone": {"path": "/who/U-9", "auth_scheme": ""}});
    let desc = json!({"base": base, "auth": {"type": "bearer", "secret": "token"}, "operations": ops});
    k.save_connection_def(admin, None, &input("schemes", desc, Some(json!({"token": SECRET})))).await.unwrap();
    for (op, sent) in [("plain", "Bearer "), ("oauth", "OAuth "), ("alone", "")] {
        connections::call(k, admin, None, &call("schemes", op, json!({}), None)).await.unwrap();
        assert!(last("GET /v1/who/U-9").contains(&format!("authorization: {sent}{SECRET}\r\n")), "{op}: {}", last("GET /v1/who/U-9"));
    }
    let basic =
        json!({"base": base, "auth": {"type": "basic", "username": "u", "secret": "token"}, "operations": {"x": {"path": "/x", "auth_scheme": "OAuth"}}});
    let e = k.save_connection_def(admin, None, &input("schemes2", basic, None)).await.unwrap_err();
    assert!(e.to_string().contains("auth_scheme"), "{e}");
}

/// BD-72: a provider that calls the client's id `client_key` and joins scopes with commas.
async fn provider_words(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let provider = json!({"authorize_url": "https://id.example.test/authorize", "token_url": format!("{base}/oauth/token-key"), "client_id": "ck-1",
                          "client_id_param": "client_key", "scope_separator": ",", "scopes": ["user.info.basic", "video.publish"],
                          "keep": ["open_id", "handle"], "not_secret": ["handle"]});
    // BD-77: what the sign-in kept, in a result; a value not said to be no secret is hidden there.
    let ops = json!({"post": {"path": "/ok-true", "result": {"id": "/id", "link": {"expr": "'https://x.example/@' + auth.handle + '/' + answer.id"},
                     "open": {"expr": "auth.open_id"}}}});
    let id = k
        .save_connection_def(admin, None, &input("keyed", json!({"base": base, "auth": {"type": "oauth", "provider": provider}, "operations": ops}), None))
        .await
        .unwrap();
    let session = Uuid::now_v7();
    let asked = query(&k.connect_start(admin, id, session).await.unwrap());
    assert_eq!((asked.get("client_key").map(String::as_str), asked.get("scope").map(String::as_str)), (Some("ck-1"), Some("user.info.basic,video.publish")));
    assert!(!asked.contains_key("client_id"), "{asked:?}");
    k.connect_finish(admin, session, &asked["state"], Some("CODE-K"), None).await.expect("exchanged with client_key");
    let r = connections::call(k, admin, None, &call("keyed", "post", json!({}), None)).await.unwrap();
    assert_eq!(r, json!({"id": "M-9", "link": "https://x.example/@ana.example/M-9", "open": "••••••"}));
    let mut wrong = provider.clone();
    wrong["not_secret"] = json!(["nope"]);
    assert!(k.save_connection_def(admin, Some(id), &input("keyed", json!({"base": base, "auth": {"type": "oauth", "provider": wrong}}), None)).await.is_err());
    let mut bad = json!({"authorize_url": "https://id.example.test/a", "token_url": "https://id.example.test/t", "client_id": "x", "scope_separator": "|"});
    let e = k
        .save_connection_def(admin, None, &input("keyed2", json!({"base": base, "auth": {"type": "oauth", "provider": bad.clone()}}), None))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("scope_separator"), "{e}");
    bad["scope_separator"] = json!(",");
    bad["params"] = json!({"client_key": "y"});
    bad["client_id_param"] = json!("client_key");
    assert!(k.save_connection_def(admin, None, &input("keyed2", json!({"base": base, "auth": {"type": "oauth", "provider": bad}}), None)).await.is_err());
}

/// BD-68: a 2xx answer that is a refusal, in the service's words, temporary or not.
async fn refusals(k: &Arc<Kernel>, admin: &Ctx, base: &str) {
    let ops = json!({
        "post": {"path": "/ok-false", "refused_if": "has(answer.ok) && !answer.ok", "error_at": "/error", "result": {"id": {"at": "/id", "required": true}}},
        "fine": {"path": "/ok-true", "refused_if": "has(answer.ok) && !answer.ok", "result": {"id": "/id"}},
        "query": {"method": "POST", "path": "/graphql", "body": "{}", "changes": false, "refused_if": "has(answer.errors)",
                  "temporary_if": "has(answer.errors) && answer.errors[0].extensions.code == 'THROTTLED'", "error": "answer.errors[0].message"},
    });
    k.save_connection_def(admin, None, &input("refusing", json!({"base": base, "operations": ops}), None)).await.unwrap();
    let e = connections::call(k, admin, None, &call("refusing", "post", json!({}), None)).await.unwrap_err();
    assert!(matches!(&e, Error::Validation(_) | Error::Invalid(_)) && e.to_string().contains("channel_not_found"), "the service's words: {e:?}");
    assert!(!e.to_string().contains("has no id"), "{e}");
    assert_eq!(connections::call(k, admin, None, &call("refusing", "fine", json!({}), None)).await.unwrap(), json!({"id": "M-9"}));
    let e = connections::call(k, admin, None, &call("refusing", "query", json!({}), None)).await.unwrap_err();
    assert!(matches!(&e, Error::Unavailable(m) if m.contains("slow down")), "temporary, in its words: {e:?}");
    // BD-71: a part may declare more than 25 MB; it is sent only as far as the server's ceiling.
    let big = k.store_file(admin, None, "big.bin", "application/octet-stream", &vec![7u8; 26 << 20]).await.unwrap()["id"].as_str().unwrap().to_string();
    let up = json!({"base": base, "operations": {"up": {"method": "POST", "path": "/posts", "changes": false, "body": {"file": "{{ inputs.f }}", "max_bytes": 50 << 20}}}});
    k.save_connection_def(admin, None, &input("big", up, None)).await.expect("50 MB declared");
    let e = connections::call(k, admin, None, &call("big", "up", json!({"f": big}), None)).await.unwrap_err();
    assert!(e.to_string().contains("FERRUM_CONNECTION_UPLOAD_MB"), "{e}");
    let bad = json!({"base": base, "operations": {"x": {"path": "/x", "refused_if": "answer.ok &&"}}});
    assert!(k.save_connection_def(admin, None, &input("refusing2", bad, None)).await.is_err(), "checked when saved");
}

/// BD-65: a description with no auth and no secrets reads any public host, by GET, without credentials, the guard as ever.
async fn any_host(k: &Arc<Kernel>, admin: &Ctx, port: u16) {
    let base = format!("http://127.0.0.1:{port}/v1");
    let read = json!({"url": "{{ inputs.url }}", "answer": "xml"});
    let feed = |any: bool| input("anyfeed", json!({"base": base, "any_host": any, "operations": {"read": read}}), None);
    let elsewhere = json!({"url": format!("http://localhost:{port}/v1/rss")});
    // Without the flag, a host besides the base's is refused, and its redirects are held to the base's host.
    k.save_connection_def(admin, None, &feed(false)).await.unwrap();
    let e = connections::call(k, admin, None, &call("anyfeed", "read", elsewhere.clone(), None)).await.unwrap_err();
    assert!(e.to_string().contains("not one of the hosts"), "{e}");
    let e = connections::call(k, admin, None, &call("anyfeed", "read", json!({"url": format!("{base}/to-local")}), None)).await.unwrap_err();
    assert!(e.to_string().contains("not one of the hosts"), "{e}");
    // With it: any host, a redirect to another host too.
    let id = k.save_connection_def(admin, Some(conn_id(k, admin, "anyfeed").await), &feed(true)).await.unwrap();
    let r = connections::call(k, admin, None, &call("anyfeed", "read", elsewhere, None)).await.unwrap();
    assert!(r.to_string().contains("First & best"), "{r}");
    connections::call(k, admin, None, &call("anyfeed", "read", json!({"url": format!("{base}/to-local")}), None)).await.expect("redirected elsewhere");
    // The guard is unchanged: a redirect to a private address, plain http to a host the operator did not open.
    let e = connections::call(k, admin, None, &call("anyfeed", "read", json!({"url": format!("{base}/to-private")}), None)).await.unwrap_err();
    assert!(e.to_string().contains("private"), "{e}");
    let e = connections::call(k, admin, None, &call("anyfeed", "read", json!({"url": "http://example.com/feed"}), None)).await.unwrap_err();
    assert!(e.to_string().contains("https only"), "{e}");
    // Refused the flag: any auth, any secret, a template naming one, anything but GET.
    let refused = |desc: Value, secrets: Option<Value>| async move {
        let mut d = json!({"base": "https://feeds.example.com", "any_host": true, "operations": {"read": {"url": "{{ inputs.url }}"}}});
        d.as_object_mut().unwrap().extend(desc.as_object().unwrap().clone());
        k.save_connection_def(admin, None, &input("feeds2", d, secrets)).await.unwrap_err().to_string()
    };
    assert!(refused(json!({"auth": {"type": "bearer", "secret": "t"}}), None).await.contains("any_host"));
    assert!(refused(json!({}), Some(json!({"t": "x"}))).await.contains("any_host"));
    assert!(refused(json!({"headers": {"x-key": "{{ secret.t }}"}}), None).await.contains("any_host"));
    assert!(refused(json!({"operations": {"post": {"method": "POST", "url": "{{ inputs.url }}", "body": "{}"}}}), None).await.contains("any_host"));
    let e = k.merge_connection_secrets(admin, id, &[("t".to_string(), "x".to_string())].into(), &[]).await.unwrap_err();
    assert!(e.to_string().contains("any_host"), "{e}");
}

async fn conn_id(k: &Kernel, admin: &Ctx, name: &str) -> Uuid {
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let id = sqlx::query_scalar("select id from conn where tenant_id = $1 and name = $2").bind(admin.tenant).bind(name).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    id
}
