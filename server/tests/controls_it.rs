//! Step-up by rule and recorded reads (docs/operations.md, "Asking again before sensitive work", "Recording who read
//! what"), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the whole server, booted as the binary
//! boots it, behind its own router. One workspace with two entities of its own, `XSecret` (marked) and `XPlain` (not),
//! a member with every right on both, and zones: `office`, `outside` (asks staff to step up) and `public` (no staff).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::zones::Zones;
use ferrum_kernel::{Ctx, Kernel, Result as KResult};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
const OFFICE: &str = "erp.acme.test";
const OUTSIDE: &str = "out.acme.test";
const PUBLIC: &str = "www.acme.test";
const ZONES: &str = r#"{"office": {}, "outside": {"step_up": true}, "public": {"staff": "none"}}"#;
/// What the marked record holds: none of it may appear in an access event.
const SECRET_TITLE: &str = "Alice Quinn";
const SECRET_NOTE: &str = "salary 98765";

#[test]
fn controls() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

struct Server {
    router: axum::Router,
}

async fn server(base: &AppContext, st: &AppState, zones: Arc<Zones>) -> Server {
    let st = AppState { zones, ..st.clone() };
    let ctx = AppContext::builder(Environment::Test, Default::default(), base.config.clone()).build();
    ctx.shared_store.insert(st.clone());
    let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    Server { router: App::after_routes(router, &ctx).await.expect("layers") }
}

#[derive(Clone, Copy)]
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

async fn call(s: &Server, method: Method, host: &str, path: &str, cred: Cred<'_>, body: Option<Value>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "10.1.2.3");
    match cred {
        Cred::Bearer(t) => req = req.header(header::AUTHORIZATION, format!("Bearer {t}")),
        Cred::Cookie(t) => {
            req = req
                .header(header::COOKIE, format!("{}={t}", ferrum_server::shell::cookie_name(false)))
                .header("sec-fetch-site", "same-origin")
                .header(header::ORIGIN, format!("http://{host}"))
        }
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A browser session of `email` that proved who they are an hour ago: aged before its first use, so no replica holds
/// it as fresh.
async fn aged_cookie(s: &Server, k: &Kernel, email: &str) -> String {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, OFFICE)
        .header("x-forwarded-for", "10.1.2.3")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"tenant": "acme", "email": email, "password": PASSWORD}).to_string()))
        .unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{email} signs in");
    let cookie = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
        })
        .expect("a cookie");
    exec(k, "update user_session set step_up_at = now() - interval '1 hour' where id = (select id from user_session order by created_at desc limit 1)").await;
    cookie
}

async fn step_up(s: &Server, cookie: &str) {
    let (status, body) = call(s, Method::POST, OFFICE, "/api/auth/step-up", Cred::Cookie(cookie), Some(json!({"password": PASSWORD}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

async fn exec(k: &Kernel, sql: &str) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string())).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// The access events written since `after` (an audit id), oldest first.
async fn access_since(k: &Kernel, after: i64) -> Vec<(String, Option<Uuid>, Value)> {
    let mut tx = k.db.system_tx().await.unwrap();
    let rows = sqlx::query_as("select action, actor, diff from audit_event where category = 'access' and id > $1 order by id")
        .bind(after)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rows
}

async fn last_audit(k: &Kernel) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n: Option<i64> = sqlx::query_scalar("select max(id) from audit_event").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n.unwrap_or(0)
}

async fn floors(k: &Kernel) -> Vec<String> {
    let mut tx = k.db.system_tx().await.unwrap();
    let f = sqlx::query_scalar("select feature from window_floor order by feature").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    f
}

fn step_up_asked(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN && serde_json::from_str::<Value>(body).is_ok_and(|v| v["error"] == "step_up")
}

fn obj(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap()
}

/// A report summing up every secret.
struct Secrets;

#[async_trait::async_trait]
impl ferrum_domain::reports::ReportSource for Secrets {
    fn catalog(&self) -> Vec<Value> {
        vec![json!({"name": "zz_secrets", "label": "Secrets", "module": "test", "entity": "XSecret", "scoped": false, "params": []})]
    }
    async fn run(&self, k: &Kernel, ctx: &Ctx, _: &str, _: &HashMap<String, String>) -> KResult<Value> {
        let page = k.list(&Ctx::system_for(ctx), "XSecret", &Default::default()).await?;
        Ok(json!({"rows": page.rows.iter().map(|r| json!({"title": r["title"]})).collect::<Vec<_>>()}))
    }
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    config.database.uri = url;
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let zones = Arc::new(Zones::parse(ZONES, "", "", "").unwrap());
    k.set_zones(zones.clone());
    let s = server(&base, &st, zones.clone()).await;

    let acme = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    for (host, zone) in [(OFFICE, "office"), (OUTSIDE, "outside"), (PUBLIC, "public")] {
        k.add_operator_host("acme", host, "shell", Some(zone)).await.unwrap();
    }
    let root = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, root.user, None, "it".into()).await.unwrap();
    let entity = |name: &str| {
        json!({"name": name, "label": name, "kind": "master", "title_field": "title",
        "fields": [{"name": "title", "type": "data", "in_list": true}, {"name": "note", "type": "data", "in_list": true}]})
    };
    let ops: Vec<ferrum_kernel::customize::NewCustomization> = serde_json::from_value(json!([
        {"entity": "XSecret", "op": {"op": "define_entity", "entity": entity("XSecret")}},
        {"entity": "XPlain", "op": {"op": "define_entity", "entity": entity("XPlain")}},
    ]))
    .unwrap();
    k.batch_customizations(&admin, ops, false).await.map_err(|(_, e)| e.to_string()).unwrap();
    let admin = k.load_ctx(acme, root.user, None, "it".into()).await.unwrap();
    for e in ["XSecret", "XPlain"] {
        let rule = ferrum_meta::PermissionDef { entity: e.into(), read: true, write: true, create: true, delete: true, export: true, ..Default::default() };
        k.set_role_permission(&admin, "Desk", &rule).await.unwrap();
    }
    let member = k.create_user(&admin, "member@acme.test", "Mia Member", Some(PASSWORD), &["Desk".into()], false).await.unwrap();
    let key = k.create_api_key(&admin, member, "it", None, None).await.unwrap().1;
    let secret = k.insert(&admin, "XSecret", obj(json!({"title": SECRET_TITLE, "note": SECRET_NOTE}))).await.unwrap();
    let secret_id = secret["id"].as_str().unwrap().to_string();
    let plain = k.insert(&admin, "XPlain", obj(json!({"title": "Plain one"}))).await.unwrap();
    let plain_id = plain["id"].as_str().unwrap().to_string();
    let (_, admin_token) = {
        let (status, body) = call(&s, Method::GET, OFFICE, "/api/auth/me", Cred::Bearer("x"), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::HOST, OFFICE)
            .header("x-forwarded-for", "10.1.2.3")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"tenant": "acme", "email": "admin@acme.test", "password": PASSWORD}).to_string()))
            .unwrap();
        let res = s.router.clone().oneshot(req).await.unwrap();
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
        ((), v["token"].as_str().unwrap().to_string())
    };
    let settings = |v: Value| call(&s, Method::PUT, OFFICE, "/api/admin/settings", Cred::Bearer(&admin_token), Some(v));

    // ── Settings: checked, kept, on the audit trail, and a floor ──
    let (status, body) = settings(json!({"step_up": {"entities": {"Nope": {"work": ["write"]}}}})).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains("Nope is not an entity"), "{status} {body}");
    let (status, body) = settings(json!({"step_up": {"entities": {"XSecret": {"work": ["print"]}}}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = settings(json!({"record_reads": {"XSecret": "sometimes"}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(!floors(&k).await.contains(&"step_up".to_string()), "nothing switched on: no floor");
    let every = json!({"entities": {"XSecret": {"work": ["write", "delete", "export", "read"]}}});
    let (status, body) = settings(json!({"step_up": every})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["step_up"]["entities"]["XSecret"]["keys"], json!(true), "keys go through by default");
    assert!(floors(&k).await.contains(&"step_up".to_string()), "switching it on puts its floor in place");
    let trail: i64 = {
        let mut tx = k.db.system_tx().await.unwrap();
        let n = sqlx::query_scalar(
            "select count(*) from audit_event where entity = 'Tenant' and action = 'settings' and diff ? 'step_up' and category = 'administration'",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        n
    };
    assert_eq!(trail, 1, "the change is administration on the audit trail");
    // Every start puts the floors of what is in use in place again.
    exec(&k, "delete from window_floor where feature = 'step_up'").await;
    if let Some(hold) = k.hold_start().await.unwrap() {
        hold.release().await;
    }
    assert!(floors(&k).await.contains(&"step_up".to_string()), "the start-up check finds the rule in use");

    // ── Each kind of work on the marked entity asks; the unmarked one is untouched ──
    let cookie = aged_cookie(&s, &k, "member@acme.test").await;
    let c = Cred::Cookie(&cookie);
    let secret_path = format!("/api/v1/XSecret/{secret_id}");
    let plain_path = format!("/api/v1/XPlain/{plain_id}");
    let tries: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, secret_path.clone(), None),
        (Method::GET, "/api/v1/XSecret".into(), None),
        (Method::GET, "/api/v1/XSecret/_export".into(), None),
        (Method::POST, "/api/v1/XSecret".into(), Some(json!({"title": "new"}))),
        (Method::PATCH, secret_path.clone(), Some(json!({"note": SECRET_NOTE}))),
    ];
    for (m, path, body) in &tries {
        let (status, answer) = call(&s, m.clone(), OFFICE, path, c, body.clone()).await;
        assert!(step_up_asked(status, &answer), "{m} {path}: {status} {answer}");
        let plain_path = path.replace("XSecret", "XPlain").replace(&secret_id, &plain_id);
        let (status, answer) = call(&s, m.clone(), OFFICE, &plain_path, c, body.clone()).await;
        assert!(status.is_success(), "unmarked {m} {plain_path}: {status} {answer}");
    }
    let doomed = k.insert(&admin, "XSecret", obj(json!({"title": "doomed"}))).await.unwrap()["id"].as_str().unwrap().to_string();
    let (status, answer) = call(&s, Method::DELETE, OFFICE, &format!("/api/v1/XSecret/{doomed}"), c, None).await;
    assert!(step_up_asked(status, &answer), "delete: {status} {answer}");
    // The description still offers what the person may do: the step-up is asked when they do it.
    assert!(call(&s, Method::GET, OFFICE, "/api/meta/XSecret", c, None).await.0.is_success());
    step_up(&s, &cookie).await;
    for (m, path, body) in &tries {
        let (status, answer) = call(&s, m.clone(), OFFICE, path, c, body.clone()).await;
        assert!(status.is_success(), "after stepping up, {m} {path}: {status} {answer}");
    }
    assert!(call(&s, Method::DELETE, OFFICE, &format!("/api/v1/XSecret/{doomed}"), c, None).await.0.is_success());

    // ── Keys: let through by default, refused with keys: false ──
    let kb = Cred::Bearer(&key);
    assert!(call(&s, Method::GET, OFFICE, &secret_path, kb, None).await.0.is_success(), "a key is not asked by default");
    assert!(call(&s, Method::PATCH, OFFICE, &secret_path, kb, Some(json!({"note": SECRET_NOTE}))).await.0.is_success());
    let (status, body) = settings(json!({"step_up": {"entities": {"XSecret": {"work": ["write", "read"], "keys": false}}}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, answer) = call(&s, Method::GET, OFFICE, &secret_path, kb, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "keys: false refuses a key: {answer}");
    assert!(answer.contains("a key cannot"), "{answer}");
    assert!(call(&s, Method::GET, OFFICE, &plain_path, kb, None).await.0.is_success(), "an unmarked entity is not");

    // ── A queued bulk change keeps the request's "not stepped up" ──
    let (status, body) = settings(json!({"step_up": {"entities": {"XSecret": {"work": ["write"]}}}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let late = aged_cookie(&s, &k, "member@acme.test").await;
    let bulk = json!({"ids": [secret_id], "op": "update", "values": {"note": "changed in bulk"}});
    let (status, body) = call(&s, Method::POST, OFFICE, "/api/v1/XSecret/_bulk/jobs", Cred::Cookie(&late), Some(bulk.clone())).await;
    assert_eq!(status, StatusCode::ACCEPTED, "queued: reading is not marked: {body}");
    let job: Uuid = serde_json::from_str::<Value>(&body).unwrap()["job"].as_str().unwrap().parse().unwrap();
    {
        let mut tx = k.db.system_tx().await.unwrap();
        let mark: Option<String> = sqlx::query_scalar("select step_up from job where id = $1").bind(job).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(mark.as_deref(), Some("no"), "the job carries it");
    }
    // A step-up after queueing does not reach back into the job.
    step_up(&s, &late).await;
    while k.run_jobs_once(10).await.unwrap() > 0 {}
    assert_eq!(k.get(&admin, "XSecret", secret_id.parse().unwrap(), false).await.unwrap()["note"], json!(SECRET_NOTE), "the job could not do it");
    let (status, body) = call(&s, Method::POST, OFFICE, "/api/v1/XSecret/_bulk/jobs", Cred::Cookie(&late), Some(bulk)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    while k.run_jobs_once(10).await.unwrap() > 0 {}
    assert_eq!(k.get(&admin, "XSecret", secret_id.parse().unwrap(), false).await.unwrap()["note"], json!("changed in bulk"), "queued stepped up: done");
    k.update(&admin, "XSecret", secret_id.parse().unwrap(), obj(json!({"note": SECRET_NOTE})), None).await.unwrap();

    // ── A zone that asks: changes, and reads of what the workspace marked; a zone refusal wins ──
    let (status, body) = settings(json!({"step_up": {"entities": {"XSecret": {"work": ["delete"]}}}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    k.require_zone_floors(&zones).await.unwrap();
    assert!(floors(&k).await.contains(&"zones_step_up".to_string()), "a zone that asks has its floor");
    let away = aged_cookie(&s, &k, "member@acme.test").await;
    let a = Cred::Cookie(&away);
    let (status, answer) = call(&s, Method::PATCH, OUTSIDE, &plain_path, a, Some(json!({"note": "x"}))).await;
    assert!(step_up_asked(status, &answer), "a change in the zone: {status} {answer}");
    assert!(call(&s, Method::GET, OUTSIDE, &plain_path, a, None).await.0.is_success(), "reading what nobody marked is not asked");
    let (status, answer) = call(&s, Method::GET, OUTSIDE, &secret_path, a, None).await;
    assert!(step_up_asked(status, &answer), "reading a marked entity in the zone: {status} {answer}");
    assert!(call(&s, Method::PATCH, OFFICE, &plain_path, a, Some(json!({"note": "x"}))).await.0.is_success(), "the office does not ask");
    assert!(call(&s, Method::GET, OFFICE, &secret_path, a, None).await.0.is_success());
    let (status, answer) = call(&s, Method::PATCH, PUBLIC, &plain_path, a, Some(json!({"note": "x"}))).await;
    assert!(status == StatusCode::FORBIDDEN && answer.contains("\"zone\""), "a zone that admits no staff refuses first: {status} {answer}");
    // Report mode reports and lets it through.
    let report = Arc::new(Zones::parse(ZONES, "", "", "report").unwrap());
    let r = server(&base, &st, report).await;
    let counted = || ferrum_kernel::zones::hidden().iter().filter(|(z, m, w, _)| z == "outside" && *m == "report" && *w == "step_up").map(|h| h.3).sum::<u64>();
    let before = counted();
    assert!(call(&r, Method::PATCH, OUTSIDE, &plain_path, a, Some(json!({"note": "y"}))).await.0.is_success(), "report mode does not ask");
    assert!(counted() > before, "it is counted");
    step_up(&s, &away).await;
    assert!(call(&s, Method::PATCH, OUTSIDE, &plain_path, a, Some(json!({"note": "z"}))).await.0.is_success(), "stepped up, the zone lets it through");
    let (status, body) = settings(json!({"step_up": null})).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // ── Who read it: at `open`, single records by every route; lists are not recorded ──
    ferrum_domain::reports::add_source(Arc::new(Secrets));
    let (status, body) = settings(json!({"record_reads": {"XSecret": "open"}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(floors(&k).await.contains(&"record_reads".to_string()));
    let reader = Cred::Cookie(&cookie);
    let mcp_get =
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_record", "arguments": {"entity": "XSecret", "id": secret_id}}});
    let opens: Vec<(&str, Method, String, Cred, Option<Value>)> = vec![
        ("record", Method::GET, secret_path.clone(), reader, None),
        ("card", Method::GET, format!("/api/cards/XSecret/{secret_id}"), reader, None),
        ("print", Method::GET, format!("{secret_path}/print"), reader, None),
        ("mcp", Method::POST, "/mcp".into(), kb, Some(mcp_get)),
    ];
    for (route, m, path, cred, body) in &opens {
        let from = last_audit(&k).await;
        let (status, answer) = call(&s, m.clone(), OFFICE, path, *cred, body.clone()).await;
        assert!(status.is_success(), "{route}: {status} {answer}");
        let events = access_since(&k, from).await;
        assert_eq!(events.len(), 1, "{route}: one event: {events:?}");
        let (action, actor, details) = &events[0];
        assert_eq!((action.as_str(), *actor), ("open", Some(member)), "{route}");
        assert_eq!(details["entity"], "XSecret");
        assert_eq!(details["record"], json!(secret_id));
        assert_eq!(details["address"], "10.1.2.3");
        assert_eq!(details["zone"], "office");
        let text = details.to_string();
        assert!(!text.contains("Alice") && !text.contains("98765"), "{route}: no field's value: {text}");
    }
    let from = last_audit(&k).await;
    assert!(call(&s, Method::GET, OFFICE, "/api/v1/XSecret", reader, None).await.0.is_success());
    assert!(access_since(&k, from).await.is_empty(), "a list is not recorded at open");

    // ── At `all`: lists, searches, reports and exports, one event per request ──
    let (status, body) = settings(json!({"record_reads": {"XSecret": "all"}})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for i in 0..3 {
        k.insert(&admin, "XSecret", obj(json!({"title": format!("Alice {i}"), "note": "98765"}))).await.unwrap();
    }
    let filters = urlencoding_filters(&json!([["title", "like", "%Alice%"]]));
    let reads: Vec<(&str, &str, String)> = vec![
        ("list", "list", format!("/api/v1/XSecret?filters={filters}")),
        ("search", "search", "/api/search?q=Alice".into()),
        ("report", "report", "/api/reports/zz_secrets".into()),
        ("export", "export", format!("/api/v1/XSecret/_export?filters={filters}")),
    ];
    for (route, action, path) in &reads {
        let from = last_audit(&k).await;
        let (status, answer) = call(&s, Method::GET, OFFICE, path, reader, None).await;
        assert!(status.is_success(), "{route}: {status} {answer}");
        let events = access_since(&k, from).await;
        assert_eq!(events.len(), 1, "{route}: one event: {events:?}");
        let (got, actor, details) = &events[0];
        assert_eq!((got.as_str(), *actor), (*action, Some(member)), "{route}");
        assert_eq!(details["entity"], "XSecret");
        assert!(details["rows"].as_i64().unwrap() >= 1, "{route}: rows: {details}");
        let text = details.to_string();
        assert!(!text.contains("Alice") && !text.contains("98765"), "{route}: no value: {text}");
        if *route == "list" || *route == "export" {
            assert_eq!(details["query"]["filters"], json!([{"field": "title", "op": "like"}]), "{route}");
        }
        if *route == "export" {
            assert_eq!(details["rows"], json!(4), "every row the export sends, counted once");
            assert_eq!(details["to"], "download, csv");
        }
    }
    let (status, body) = call(&s, Method::GET, OFFICE, "/api/admin/audit?category=access", Cred::Bearer(&admin_token), None).await;
    assert!(status.is_success() && body.contains("\"category\":\"access\""), "{body}");

    // ── An unmarked read sends no statement; a marked read whose event cannot be written fails ──
    exec(
        &k,
        "create function it_no_access() returns trigger language plpgsql as $$ begin raise exception 'no access events'; end $$; \
         create trigger it_no_access before insert on audit_event for each row when (new.category = 'access') execute function it_no_access()",
    )
    .await;
    let sent = ferrum_kernel::controls::statements();
    assert!(call(&s, Method::GET, OFFICE, &plain_path, reader, None).await.0.is_success());
    assert!(call(&s, Method::GET, OFFICE, "/api/v1/XPlain", reader, None).await.0.is_success());
    assert_eq!(ferrum_kernel::controls::statements(), sent, "unmarked reads ask the database nothing more");
    let (status, answer) = call(&s, Method::GET, OFFICE, &secret_path, reader, None).await;
    assert!(!status.is_success() && !answer.contains("Alice"), "a read that cannot be recorded fails: {status} {answer}");
    let (status, answer) = call(&s, Method::GET, OFFICE, "/api/v1/XSecret/_export", reader, None).await;
    assert!(!status.is_success() && !answer.contains("Alice"), "an export too, before any row: {status} {answer}");
    exec(&k, "drop trigger it_no_access on audit_event; drop function it_no_access()").await;
    let _ = acme;
}

fn urlencoding_filters(v: &Value) -> String {
    v.to_string().bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
