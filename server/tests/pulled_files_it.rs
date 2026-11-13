//! A stored file's short-lived public address for a service that pulls media (BD-64), against a real PostgreSQL
//! (`FERRUM_APPS_TEST_DATABASE_URL`, reset first), files on a local directory, the server's route on 127.0.0.1 as the
//! public address and a stand-in service that pulls the address it is handed during the call, whole and by a range.
//! The address serves the bytes and ranges with the file's type and length, cannot be altered to reach another file,
//! is gone once ended or past its time, is hidden in the call's capture and result, and is refused without a public
//! address.

mod common;

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use ferrum_kernel::connections::{self, Call, ConnectionInput};
use ferrum_kernel::files::FileStore;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use ferrum_server::objects::Objects;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Dir(PathBuf);

#[async_trait]
impl FileStore for Dir {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.0.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(Error::internal)?;
        std::fs::write(path, data).map_err(Error::internal)
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(|_| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        std::fs::remove_file(self.0.join(key)).map_err(Error::internal)
    }
}

/// The addresses the stand-in was handed, in order.
static HANDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Shaped like a network that makes a media container from an address it fetches itself.
async fn container(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
    let url = body["image_url"].as_str().unwrap_or_default().to_string();
    HANDED.lock().unwrap().push(url.clone());
    let c = reqwest::Client::new();
    let whole = c.get(&url).send().await.unwrap();
    let (status, kind) = (whole.status().as_u16(), whole.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()));
    let bytes = whole.bytes().await.unwrap();
    let part = c.get(&url).header("range", "bytes=0-3").send().await.unwrap();
    let part_status = part.status().as_u16();
    let part = part.bytes().await.unwrap();
    axum::Json(json!({"id": "C-1", "status": status, "type": kind, "length": bytes.len(), "first": String::from_utf8_lossy(&part),
        "part_status": part_status, "echo": url}))
}

async fn listen(app: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

#[derive(Clone)]
struct Served(Arc<Kernel>, Arc<Objects>);

async fn pulled(
    State(s): State<Served>,
    UrlPath(id): UrlPath<uuid::Uuid>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match ferrum_server::controllers::system::pulled_response(&s.0, &s.1, id, q.get("s").map_or("", String::as_str), &headers).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

#[test]
fn a_service_pulls_a_file_by_its_short_lived_address() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_APPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_apps_api_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    // The server's route listens first: its address is the public address the kernel boots with.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ferrum_port = listener.local_addr().unwrap().port();
    let settings = KernelSettings {
        secret_key: Some(SecretKey::new([9u8; 32])),
        connection_hosts: vec!["127.0.0.1".into()],
        public_url: Some(format!("http://127.0.0.1:{ferrum_port}")),
        ..Default::default()
    };
    let k = Kernel::boot(db.clone(), Catalog::from_packages(vec![core.clone()]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let root = std::env::temp_dir().join(format!("ferrum-pulled-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&root).unwrap();
    k.set_file_store(Arc::new(Dir(root.clone())));
    let objects = Arc::new(Objects::local(root.to_str().unwrap()).unwrap());
    let served = Router::new().route("/files/pull/{id}", get(pulled)).with_state(Served(k.clone(), objects));
    tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let service = listen(Router::new().route("/v1/containers", post(container))).await;

    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let photo = k.store_file(&admin, None, "photo.jpg", "image/jpeg", b"JPEGBYTES-0123456789").await.unwrap()["id"].as_str().unwrap().to_string();
    let other = k.store_file(&admin, None, "other.jpg", "image/jpeg", b"OTHER").await.unwrap()["id"].as_str().unwrap().to_string();

    let make = |end: bool| {
        json!({"method": "POST", "path": "/containers", "body": "{\"image_url\": {{ public_files.photo | json }}}",
               "public_files": {"photo": {"file": "{{ inputs.photo }}", "minutes": 30, "end_with_call": end}}, "result": {"id": "/id", "answer": ""}})
    };
    let desc = json!({"base": format!("http://127.0.0.1:{service}/v1"), "operations": {"post": make(true), "keep": make(false)}});
    let input: ConnectionInput = serde_json::from_value(json!({"name": "net", "label": "Net", "description": desc, "enabled": true})).unwrap();
    let conn = k.save_connection_def(&admin, None, &input).await.unwrap();
    k.debug_connection(&admin, conn, 5).await.unwrap();
    let call = |op: &str, file: &str, key: &str| Call {
        connection: "net".into(),
        operation: op.into(),
        inputs: json!({"photo": file}),
        key: Some(key.into()),
        ..Default::default()
    };

    // The service pulls the whole file and a range of it, with its type, during the call.
    let r = connections::call(&k, &admin, None, &call("post", &photo, "k-1")).await.unwrap();
    let a = &r["answer"];
    assert_eq!((a["status"].as_u64(), a["type"].as_str(), a["length"].as_u64()), (Some(200), Some("image/jpeg"), Some(20)), "{r}");
    assert_eq!((a["part_status"].as_u64(), a["first"].as_str()), (Some(206), Some("JPEG")), "{r}");
    let handed = HANDED.lock().unwrap().last().cloned().unwrap();
    assert!(handed.starts_with(&format!("http://127.0.0.1:{ferrum_port}/files/pull/")) && handed.contains("?s="), "{handed}");
    let sig = handed.split("?s=").nth(1).unwrap().to_string();
    // The address is a secret: hidden in the result the app gets and in the capture.
    assert!(!r.to_string().contains(&sig), "{r}");
    let log = k.connection_log(&admin, conn, 10).await.unwrap();
    assert!(!serde_json::to_string(&log).unwrap().contains(&sig), "the capture hides it: {log:?}");
    // Ended with the call: gone, and the record says which file, for which call, until when, ended when.
    let c = reqwest::Client::new();
    assert_eq!(c.get(&handed).send().await.unwrap().status().as_u16(), 404, "ended with its call");
    let rec = k.connection_addresses(&admin, conn, 10).await.unwrap();
    assert_eq!((rec[0]["file"].as_str(), rec[0]["key"].as_str(), rec[0]["operation"].as_str()), (Some(photo.as_str()), Some("k-1"), Some("post")));
    assert!(!rec[0]["ended_at"].is_null() && !rec[0]["expires_at"].is_null(), "{rec:?}");

    // Kept past the call: served to anyone holding it, never cached, ranges answered.
    connections::call(&k, &admin, None, &call("keep", &photo, "k-2")).await.unwrap();
    let kept = HANDED.lock().unwrap().last().cloned().unwrap();
    let got = c.get(&kept).send().await.unwrap();
    assert_eq!(got.status().as_u16(), 200);
    assert_eq!(got.headers()["content-length"], "20");
    assert_eq!(got.headers()["cache-control"], "private, no-store");
    let part = c.get(&kept).header("range", "bytes=10-13").send().await.unwrap();
    assert_eq!((part.status().as_u16(), part.headers()["content-range"].to_str().unwrap().to_string()), (206, "bytes 10-13/20".to_string()));
    assert_eq!(part.bytes().await.unwrap().as_ref(), b"0123");
    // Altered: another id under its signature, its id under another signature, no signature: not found alike.
    connections::call(&k, &admin, None, &call("keep", &other, "k-3")).await.unwrap();
    let theirs = HANDED.lock().unwrap().last().cloned().unwrap();
    let (kept_id, kept_sig) = kept.split_once("?s=").unwrap();
    let (their_id, their_sig) = theirs.split_once("?s=").unwrap();
    for bad in [format!("{their_id}?s={kept_sig}"), format!("{kept_id}?s={their_sig}"), kept_id.to_string(), format!("{kept_id}?s={}", "0".repeat(64))] {
        assert_eq!(c.get(&bad).send().await.unwrap().status().as_u16(), 404, "{bad}");
    }
    assert_eq!(c.get(&theirs).send().await.unwrap().bytes().await.unwrap().as_ref(), b"OTHER", "each address its own file");
    // The same keyed call made again answers what it gave: nothing is sent, no address made.
    let handed_before = HANDED.lock().unwrap().len();
    let made_before = k.connection_addresses(&admin, conn, 50).await.unwrap().len();
    connections::call(&k, &admin, None, &call("keep", &photo, "k-2")).await.unwrap();
    let handed_after = HANDED.lock().unwrap().len();
    assert_eq!((handed_after, k.connection_addresses(&admin, conn, 50).await.unwrap().len()), (handed_before, made_before));
    // Past its time: gone.
    let mut tx = db.system_tx().await.unwrap();
    sqlx::query("update conn_file_address set expires_at = now() - interval '1 second' where call_key = 'k-2'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(c.get(&kept).send().await.unwrap().status().as_u16(), 404, "expired");
    // A stored file the caller names that is no file is refused before anything is sent.
    let e = connections::call(&k, &admin, None, &call("post", &uuid::Uuid::now_v7().to_string(), "k-4")).await.unwrap_err();
    assert!(e.to_string().contains("not one you may send"), "{e}");

    // A server without a public address refuses, saying why.
    let bare = KernelSettings { secret_key: Some(SecretKey::new([9u8; 32])), connection_hosts: vec!["127.0.0.1".into()], ..Default::default() };
    let k2 = Kernel::boot(db.clone(), Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), bare, None).await.expect("boot");
    k2.set_file_store(Arc::new(Dir(root.clone())));
    let admin2 = k2.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let e = connections::call(&k2, &admin2, None, &call("post", &photo, "k-5")).await.unwrap_err();
    assert!(e.to_string().contains("cannot pull a file from localhost"), "{e}");
    let _ = std::fs::remove_dir_all(root);
}
