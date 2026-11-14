//! A domain of a workspace's own is its owner's to give (docs/operations.md, "Custom domains"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset): the whole server as the binary boots it on a cluster of many
//! workspaces (a base domain), where a domain is proven by its DNS record by default, its questions answered by a DNS
//! server this test runs (`FERRUM_DNS_RESOLVERS`). That a domain points here is said for it (`Kernel::host_reached`):
//! nothing answers on port 80 here. Each way another workspace could take a domain is refused: one that points here but
//! was never given (a wildcard record), one a deleted workspace had, and one released whose quarantine is over.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::Kernel;
use ferrum_kernel::shell::{DomainProof, HostCheck, HostRoute};
use ferrum_kernel::tenant::NewTenant;
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";

#[test]
fn a_domain_is_its_owners_to_give() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

/// What the test's DNS holds: a name (lower case, no final dot) and its TXT records, each of one string.
type Zone = Arc<Mutex<HashMap<String, Vec<String>>>>;

/// A DNS server for this test: a question about a name `zone` holds is answered with its TXT records; about any other
/// name, that it does not exist. `asked` counts the questions.
async fn dns(zone: Zone, asked: Arc<AtomicUsize>) -> std::net::SocketAddr {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = socket.recv_from(&mut buf).await {
            let q = &buf[..n];
            // The question: its name's labels from byte 12 to a zero byte, then its type and class.
            let (mut i, mut labels) = (12, Vec::new());
            while i < n && q[i] != 0 {
                let len = q[i] as usize;
                labels.push(String::from_utf8_lossy(&q[(i + 1).min(n)..(i + 1 + len).min(n)]).to_ascii_lowercase());
                i += 1 + len;
            }
            if i + 5 > n {
                continue;
            }
            asked.fetch_add(1, Ordering::SeqCst);
            let found = zone.lock().unwrap().get(&labels.join(".")).cloned();
            let mut out = q[..2].to_vec();
            out.extend_from_slice(if found.is_some() { &[0x81, 0x80] } else { &[0x81, 0x83] });
            let records = found.unwrap_or_default();
            out.extend_from_slice(&[0, 1]);
            out.extend_from_slice(&(records.len() as u16).to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&q[12..i + 5]);
            for r in &records {
                out.extend_from_slice(&[0xc0, 0x0c, 0, 16, 0, 1, 0, 0, 0, 60]);
                out.extend_from_slice(&((r.len() + 1) as u16).to_be_bytes());
                out.push(r.len() as u8);
                out.extend_from_slice(r.as_bytes());
            }
            let _ = socket.send_to(&out, from).await;
        }
    });
    addr
}

/// One request to `host`: (status, body).
async fn call(router: &axum::Router, method: Method, host: &str, path: &str, bearer: Option<&str>, body: Option<Value>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host);
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A workspace `slug` with its subdomain, and its administrator's API token.
async fn workspace(k: &Kernel, router: &axum::Router, slug: &str) -> (Uuid, String) {
    let id = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    k.add_subdomain(id, &format!("{slug}.proof.test")).await.unwrap();
    let body = json!({"tenant": slug, "email": format!("admin@{slug}.test"), "password": PASSWORD});
    let (status, answer) = call(router, Method::POST, &format!("{slug}.proof.test"), "/api/auth/login", None, Some(body)).await;
    let token = serde_json::from_str::<Value>(&answer).ok().and_then(|v| v["token"].as_str().map(str::to_string));
    (id, token.unwrap_or_else(|| panic!("{slug} signs in: {status} {answer}")))
}

/// The row `GET /api/admin/domains` lists for `host`, as the administrator with `token` sees it on `at`.
async fn listed(router: &axum::Router, at: &str, token: &str, host: &str) -> Value {
    let (status, body) = call(router, Method::GET, at, "/api/admin/domains", Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let all: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(all["proof"], "record", "{all}");
    all["domains"].as_array().unwrap().iter().find(|d| d["host"] == host).cloned().unwrap_or_else(|| panic!("{host}: {all}"))
}

async fn sql(k: &Kernel, q: &str) {
    let q = format!("begin; set local ferrum.system = 'on'; {q}; commit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(q.clone())).execute(&k.db.pool).await.expect(&q);
}

/// Deleting a workspace turns referential triggers off, which the database's role may do only while it is granted that
/// (docs/operations.md, "Closing and deleting a workspace").
async fn as_superuser(url: &str, sql: &str) {
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    let pool = sqlx::PgPool::connect(&format!("{base}/{}", url.rsplit('/').next().unwrap())).await.expect("a superuser");
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string())).execute(&pool).await.unwrap();
    pool.close().await;
}

async fn scenario() {
    let zone: Zone = Arc::default();
    let asked = Arc::new(AtomicUsize::new(0));
    let resolver = dns(zone.clone(), asked.clone()).await;
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    config.database.uri = url.clone();
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
        assert_eq!(s["domain_proof"], json!(""), "FERRUM_DOMAIN_PROOF unset");
        s["base_domain"] = json!("proof.test");
        s["dns_resolvers"] = json!(resolver.to_string());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let router = App::routes(&base).to_router::<App>(base.clone(), axum::Router::new()).expect("router");
    let router = App::after_routes(router, &base).await.expect("layers");
    let k = st.kernel.clone();
    assert_eq!(k.settings.domain_proof, DomainProof::Record, "the default on a cluster of many workspaces");
    let (floors, _) = k.floors().await.unwrap();
    assert!(floors.iter().any(|f| f.feature == "domain_proof"), "a build that would verify by pointing alone may not start: {floors:?}");

    let (victim, victim_token) = workspace(&k, &router, "victim").await;
    let (taker, taker_token) = workspace(&k, &router, "taker").await;
    let at_victim = "victim.proof.test";
    let at_taker = "taker.proof.test";
    let add = async |token: &str, at: &str, host: &str| call(&router, Method::POST, at, "/api/admin/domains", Some(token), Some(json!({"host": host}))).await;
    let set = |name: &str, values: &[String]| {
        zone.lock().unwrap().insert(name.to_string(), values.to_vec());
    };

    // ── Its owner gives it: the domain waits for the domain to point here, then for its record, then works. ──
    let (status, body) = add(&victim_token, at_victim, "erp.victim.test").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let row: Value = serde_json::from_str(&body).unwrap();
    let victims = format!("ferrum-domain={}", row["verify_token"].as_str().unwrap());
    assert_eq!(row["verify_token"].as_str().unwrap().len(), 32, "128 random bits");
    assert_eq!((row["status"].clone(), row["waiting"].clone()), (json!("pending"), json!("domain")));
    assert_eq!(row["record"], json!({"name": "_ferrum.erp.victim.test", "type": "TXT", "value": victims}), "what to add");
    let (status, body) = call(&router, Method::POST, at_victim, "/api/admin/domains/erp.victim.test/check", Some(&victim_token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!({"host": "erp.victim.test", "answers": false, "points": false, "waiting": "domain"}));
    assert_eq!(listed(&router, at_victim, &victim_token, "erp.victim.test").await["status"], "failing", "it did not point here");
    let seen = asked.load(Ordering::SeqCst);
    assert_eq!(k.host_reached("erp.victim.test", true).await.unwrap(), Some(HostCheck { works: false, points: true, record: Some(false) }));
    assert!(asked.load(Ordering::SeqCst) > seen, "its record was asked for");
    let row = listed(&router, at_victim, &victim_token, "erp.victim.test").await;
    assert_eq!((row["status"].clone(), row["waiting"].clone()), (json!("pending"), json!("record")), "points here, waits for its record: {row}");
    assert_eq!(k.route_host("erp.victim.test").await.unwrap(), HostRoute::Unknown, "it opens nothing yet");
    let (status, body) = call(&router, Method::GET, "erp.victim.test", "/", None, None).await;
    assert!(status == StatusCode::NOT_FOUND && body.contains("No workspace here"), "{status} {body}");
    set("_ferrum.erp.victim.test", std::slice::from_ref(&victims));
    // The round and the administrator's check at once: done once, nothing lost.
    let (a, b) = tokio::join!(k.host_reached("erp.victim.test", true), k.host_reached("erp.victim.test", true));
    assert!(a.unwrap().unwrap().works && b.unwrap().unwrap().works);
    let row = listed(&router, at_victim, &victim_token, "erp.victim.test").await;
    assert_eq!((row["status"].clone(), row["waiting"].clone(), row["record"].clone()), (json!("verified"), json!(null), json!(null)), "{row}");
    assert_eq!(row["proved_at"], row["verified_at"], "proven as it became verified");
    assert_eq!(k.route_host("erp.victim.test").await.unwrap(), HostRoute::Serve { tenant: victim, target: "shell".into() });

    // ── 1. A domain that points here but was never given (a wildcard record): another workspace waits for ever. ──
    let (status, body) = add(&taker_token, at_taker, "shop.victim.test").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let takers = format!("ferrum-domain={}", serde_json::from_str::<Value>(&body).unwrap()["verify_token"].as_str().unwrap());
    assert_eq!(k.host_reached("shop.victim.test", true).await.unwrap().map(|c| c.works), Some(false), "no record");
    set("_ferrum.shop.victim.test", std::slice::from_ref(&victims));
    assert_eq!(k.host_reached("shop.victim.test", true).await.unwrap().map(|c| c.works), Some(false), "the owner's record for another workspace");
    set("_ferrum.shop.victim.test", &[victims.clone(), takers.clone()]);
    assert_eq!(k.host_reached("shop.victim.test", true).await.unwrap().map(|c| c.works), Some(false), "two claims there");
    assert_eq!(k.route_host("shop.victim.test").await.unwrap(), HostRoute::Unknown);
    let row = listed(&router, at_taker, &taker_token, "shop.victim.test").await;
    assert_eq!((row["status"].clone(), row["waiting"].clone(), row["verified_at"].clone()), (json!("pending"), json!("record"), json!(null)));
    // Only its owner's record for this workspace's token gives it.
    set("_ferrum.shop.victim.test", std::slice::from_ref(&takers));
    assert_eq!(k.host_reached("shop.victim.test", true).await.unwrap().map(|c| c.works), Some(true));
    assert_eq!(k.route_host("shop.victim.test").await.unwrap(), HostRoute::Serve { tenant: taker, target: "shell".into() });

    // Once proven, it goes on by pointing here alone, even after it stopped for a while: it never left its workspace.
    zone.lock().unwrap().remove("_ferrum.shop.victim.test");
    for _ in 0..7 {
        k.host_reached("shop.victim.test", false).await.unwrap();
    }
    assert_eq!(k.route_host("shop.victim.test").await.unwrap(), HostRoute::Unknown, "seven failed checks");
    let seen = asked.load(Ordering::SeqCst);
    assert_eq!(k.host_reached("shop.victim.test", true).await.unwrap(), Some(HostCheck { works: true, points: true, record: None }));
    assert_eq!(asked.load(Ordering::SeqCst), seen, "not asked again");

    // A domain verified before its record was asked for (by pointing here, on a cluster that did not ask) goes on
    // working by pointing here; it is listed with the record that would prove it, and the doctor says so, as information.
    sql(&k, &format!("insert into tenant_host (host, tenant_id, kind, target, verify_token, verified_at, checked_at) values ('old.taker.test', '{taker}', 'custom', 'shell', 'abc', now(), now())")).await;
    assert_eq!(k.host_reached("old.taker.test", true).await.unwrap().map(|c| c.works), Some(true));
    let row = listed(&router, at_taker, &taker_token, "old.taker.test").await;
    assert_eq!((row["status"].clone(), row["record"]["value"].clone()), (json!("verified"), json!("ferrum-domain=abc")));
    let doctor = k.doctor(10).await.unwrap();
    let said = doctor.iter().find(|f| f.check == "domains.proof").expect("the doctor says it");
    assert!(said.level == ferrum_kernel::doctor::Level::Ok && said.detail.starts_with("1 working domains"), "{said:?}");

    // A deletion committing while another workspace claims its domain: the claim sees its rows or its quarantine, never
    // neither (one statement reads both).
    let mut deleting = k.db.pool.begin().await.unwrap();
    sqlx::query("set local ferrum.system = 'on'").execute(&mut *deleting).await.unwrap();
    sqlx::query("insert into host_quarantine (host, until) values ('erp.victim.test', now() + interval '365 days')").execute(&mut *deleting).await.unwrap();
    sqlx::query("delete from tenant_host where host = 'erp.victim.test'").execute(&mut *deleting).await.unwrap();
    let (status, body) = add(&taker_token, at_taker, "erp.victim.test").await;
    assert!(status == StatusCode::CONFLICT && body.contains("already in use"), "before it commits: {status} {body}");
    deleting.commit().await.unwrap();
    let (status, body) = add(&taker_token, at_taker, "erp.victim.test").await;
    assert!(status == StatusCode::CONFLICT && body.contains("belonged to another workspace recently"), "after: {status} {body}");
    sql(&k, &format!("delete from host_quarantine; insert into tenant_host (host, tenant_id, kind, target, verify_token, verified_at, proved_at) values ('erp.victim.test', '{victim}', 'custom', 'shell', '{}', now(), now())", &victims["ferrum-domain=".len()..])).await;

    // ── 2. A deleted workspace's domains: kept from everyone as a released one is, with no period of leaving or
    // redirecting; and its slug, which its names under the base end in, is never another workspace's. ──
    let (status, body) = call(&router, Method::POST, at_victim, "/api/admin/domains", Some(&victim_token), Some(json!({"name": "sales"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    k.close_account(victim, chrono::Utc::now().date_naive() + chrono::Duration::days(30), "the customer left").await.unwrap();
    sql(&k, &format!("update tenant set retain_until = current_date - 1 where id = '{victim}'")).await;
    as_superuser(&url, "grant set on parameter session_replication_role to ferrum").await;
    let deleted = k.delete_tenant("victim", "victim", true, "kept as long as agreed").await;
    as_superuser(&url, "revoke set on parameter session_replication_role from ferrum").await;
    assert_eq!(deleted.expect("deleted")["deleted"], json!(true));
    assert_eq!(k.route_host("erp.victim.test").await.unwrap(), HostRoute::Unknown, "nothing is left to serve");
    let (status, body) = add(&taker_token, at_taker, "erp.victim.test").await;
    assert!(status == StatusCode::CONFLICT && body.contains("belonged to another workspace recently"), "{status} {body}");
    assert_eq!(call(&router, Method::GET, "localhost", "/_tls/ask?domain=erp.victim.test", None, None).await.0, StatusCode::NOT_FOUND, "no certificate");
    let again = k
        .create_tenant(NewTenant {
            slug: "victim".into(),
            name: "Victim again".into(),
            admin_email: "a@again.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await;
    assert!(again.is_err(), "its slug stays retired, so sales--victim.proof.test is never formed again");

    // ── 3. Once the quarantine is over, the domain is free, and its owner's old record proves nothing: only a record for
    // the new workspace's token does. ──
    sql(&k, "update host_quarantine set until = now() - interval '1 second'").await;
    let (status, body) = add(&taker_token, at_taker, "erp.victim.test").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let new = format!("ferrum-domain={}", serde_json::from_str::<Value>(&body).unwrap()["verify_token"].as_str().unwrap());
    assert_ne!(new, victims, "another token");
    assert_eq!(k.host_reached("erp.victim.test", true).await.unwrap().map(|c| c.works), Some(false), "the deleted workspace's record");
    // A released domain after its quarantine: the same.
    let (status, _) = add(&taker_token, at_taker, "www.taker.test").await;
    assert_eq!(status, StatusCode::CREATED);
    let mine = listed(&router, at_taker, &taker_token, "www.taker.test").await["record"]["value"].as_str().unwrap().to_string();
    set("_ferrum.www.taker.test", std::slice::from_ref(&mine));
    assert_eq!(k.host_reached("www.taker.test", true).await.unwrap().map(|c| c.works), Some(true));
    assert_eq!(call(&router, Method::DELETE, at_taker, "/api/admin/domains/www.taker.test", Some(&taker_token), None).await.0, StatusCode::OK);
    let (_, other_token) = workspace(&k, &router, "other").await;
    let (status, body) = add(&other_token, "other.proof.test", "www.taker.test").await;
    assert!(status == StatusCode::CONFLICT && body.contains("recently"), "quarantined: {status} {body}");
    sql(&k, "update tenant_host set released_at = now() - interval '400 days', leaving_until = now() - interval '399 days' where host = 'www.taker.test'")
        .await;
    let (status, body) = add(&other_token, "other.proof.test", "www.taker.test").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(k.host_reached("www.taker.test", true).await.unwrap().map(|c| c.works), Some(false), "the old owner's record proves nothing");
    assert_eq!(k.route_host("www.taker.test").await.unwrap(), HostRoute::Unknown);
}
