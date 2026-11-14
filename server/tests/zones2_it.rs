//! Zones, level 2 (docs/operations.md, "Zones"), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the whole server,
//! booted as the binary boots it, behind its own router. One workspace with CRM and Helpdesk installed (neither
//! depends on the other; both use the shared customers and products), a staff member with roles in both, an
//! administrator, and a zone limited to CRM. Every kind of credential on every family of routes: CRM works there
//! as with no zones, Helpdesk's records are not there (a hidden record answers as a missing one), and the
//! administrator is not one. Then each way narrowing a person's rights alone would leak, one by one.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Ctx, Kernel, Trace};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use sha2::Digest;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
/// The staff address: a host in a zone that admits staff wholly.
const STAFF_HOST: &str = "erp.acme.test";
/// A host in the zone `sales`: staff limited to CRM.
const SALES_HOST: &str = "sales.acme.test";
const ZONES: &str = r#"{"office": {}, "sales": {"staff": ["crm"]}, "own": {"staff": ["crm"], "custom": true}, "back": {"staff": "all", "admin": false}}"#;

#[test]
fn level_2() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

struct Server {
    router: axum::Router,
    st: AppState,
}

async fn server(base: &AppContext, st: &AppState, zones: Zones) -> Server {
    let st = AppState { zones: Arc::new(zones), ..st.clone() };
    let ctx = AppContext::builder(Environment::Test, Default::default(), base.config.clone()).build();
    ctx.shared_store.insert(st.clone());
    let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    Server { router: App::after_routes(router, &ctx).await.expect("layers"), st }
}

#[derive(Clone, Copy)]
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
    Basic(&'a str, &'a str),
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
        Cred::Basic(u, p) => req = req.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))),
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

async fn sign_in(s: &Server, email: &str, page: bool) -> (Option<String>, Option<String>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, STAFF_HOST)
        .header("x-forwarded-for", "10.1.2.3")
        .header(header::CONTENT_TYPE, "application/json");
    if page {
        req = req.header("sec-fetch-site", "same-origin");
    }
    let req = req.body(Body::from(json!({"tenant": "acme", "email": email, "password": PASSWORD}).to_string())).unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{email} signs in");
    let cookie = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
    });
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (v["token"].as_str().map(str::to_string), cookie)
}

async fn oauth_token(s: &Server, k: &Kernel, ctx: &Ctx) -> String {
    let client = k.oauth_register(Some("it".into()), vec!["http://127.0.0.1:9/cb".into()]).await.unwrap();
    let verifier = "v".repeat(50);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let code = k.oauth_issue_code(ctx, &client.id, "http://127.0.0.1:9/cb", &challenge, "S256").await.unwrap();
    let form =
        format!("grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&code_verifier={verifier}&client_id={}", client.id);
    let req = Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header(header::HOST, STAFF_HOST)
        .header("x-forwarded-for", "10.1.2.3")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["access_token"].as_str().unwrap().to_string()
}

/// A refusal by zone: 403, `zone`, naming the staff address.
fn refused_by_zone(status: StatusCode, body: &str) -> bool {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    status == StatusCode::FORBIDDEN && v["error"] == "zone" && v["staff_address"] == STAFF_HOST
}

fn plain_page(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN && body.contains("Not opened here") && body.contains(STAFF_HOST)
}

/// An answer as compared: its request's own operation id left out.
fn norm((status, body): (StatusCode, String)) -> (StatusCode, String) {
    match serde_json::from_str::<Value>(&body) {
        Ok(Value::Object(mut m)) => {
            m.remove("operation");
            m.remove("_ms");
            (status, Value::Object(m).to_string())
        }
        _ => (status, body),
    }
}

fn obj(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &serde_json::Map<String, Value>) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

/// `user`'s context as a request in zone `zone` builds it, in a trace of its own (what it queues is marked).
async fn in_zone(k: &Kernel, zones: &Arc<Zones>, tenant: Uuid, user: Uuid, zone: &str) -> Ctx {
    let entry = Arc::new(Entry { zones: zones.clone(), placed: Placed::In(zone.into()), address: "10.1.2.3".into(), credential: Credential::Token });
    k.request_ctx(tenant, user, None, "it".into(), &entry).await.unwrap()
}

/// Runs `f` as a request in a zone does: in a trace, which the context marks with the zone.
async fn traced<F: std::future::Future>(f: F) -> F::Output {
    Trace { correlation: format!("it-{}", Uuid::now_v7()), ..Default::default() }.scope(f).await
}

async fn scalar<T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(k: &Kernel, sql: &str, id: Uuid) -> T {
    let mut tx = k.db.system_tx().await.unwrap();
    let v = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(id).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    v
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

    // The settings are checked at start: unknown apps and level 2's shapes.
    let resolve = |json: &str| {
        let k = k.clone();
        let z = Zones::parse(json, "", "", "").map_err(|e| e.to_string());
        async move { ferrum_server::zone::resolve(z?, &k).await }
    };
    assert!(resolve(r#"{"p": {"staff": ["no-such-app"]}}"#).await.unwrap_err().contains("does not know"), "an unknown app refuses to start");
    let zones = Arc::new(resolve(ZONES).await.expect("crm resolves to its packages"));
    assert!(zones.rule("sales").unwrap().packages.contains("crm"));
    k.set_zones(zones.clone());

    // A workspace with CRM and Helpdesk.
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
    let root = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap().user;
    let admin = k.load_ctx(acme, root, None, "it".into()).await.unwrap();
    for app in ["crm", "helpdesk"] {
        let plan = st.apps.plan(&admin, app, None).await.unwrap_or_else(|e| panic!("plan {app}: {e}"));
        st.apps.install(&admin, app, None, &plan.digest, false).await.unwrap_or_else(|e| panic!("install {app}: {e:?}"));
    }
    for (host, zone) in [(STAFF_HOST, "office"), (SALES_HOST, "sales")] {
        k.add_operator_host("acme", host, "shell", Some(zone)).await.unwrap();
    }
    let admin = k.load_ctx(acme, root, None, "it".into()).await.unwrap();
    assert!(admin.schema.packages.iter().any(|p| p == "helpdesk") && admin.schema.packages.iter().any(|p| p == "crm"));
    assert!(!admin.schema.package_depends_on("crm", "helpdesk") && !admin.schema.package_depends_on("helpdesk", "crm"), "neither depends on the other");

    // A custom entity, with a link to a ticket, a button that writes it, a refusal reading it, and an approval.
    let note = json!({"name": "XNote", "label": "Note", "kind": "master", "title_field": "title",
        "fields": [{"name": "title", "type": "data"}, {"name": "ticket", "type": "link", "to": "Ticket"}, {"name": "approver", "type": "data"}],
        "actions": [
            {"name": "x_touch", "label": "Touch", "permission": "write", "effects": [{"type": "update_linked", "link": "ticket", "values": {"priority": "'Urgent'"}}]},
            {"name": "x_check", "label": "Check", "permission": "write", "refuse_if": [{"when": "links.ticket != null && links.ticket.subject == 'Printer on fire'", "message": "linked to a ticket"}],
             "effects": [{"type": "set_fields", "values": {"title": "'checked'"}}]}]});
    let ops: Vec<ferrum_kernel::customize::NewCustomization> = serde_json::from_value(json!([
        {"entity": "XNote", "op": {"op": "define_entity", "entity": note}},
        {"entity": "XNote", "op": {"op": "set_workflow", "workflow": {"name": "notes", "entity": "XNote", "initial_state": "Draft",
            "states": [{"name": "Draft"}, {"name": "Done"}],
            "transitions": [{"action": "Finish", "from": "Draft", "to": "Done",
                "approval": {"steps": [{"label": "Approver", "approvers": {"field": "approver"}}]}}]}}}
    ]))
    .unwrap();
    k.batch_customizations(&admin, ops, false).await.map_err(|(_, e)| e.to_string()).unwrap();
    let admin = k.load_ctx(acme, root, None, "it".into()).await.unwrap();

    // Data, and a staff member with a workspace role over both apps' records and each app's own role.
    let coin = id_of(&k.insert(&admin, "Currency", obj(json!({"code": "XTS", "currency_name": "Testing Coin"}))).await.unwrap());
    k.insert(&admin, "Company", obj(json!({"company_name": "Acme", "abbr": "AC", "default_currency": coin.to_string()}))).await.unwrap();
    for entity in ["Lead", "Ticket", "Customer", "XNote", "Company"] {
        let rule = ferrum_meta::PermissionDef { entity: entity.into(), read: true, write: true, create: true, export: true, ..Default::default() };
        k.set_role_permission(&admin, "Desk", &rule).await.unwrap();
    }
    let staff = k
        .create_user(&admin, "staff@acme.test", "Sam Staff", Some(PASSWORD), &["Desk".into(), "Sales User".into(), "Support Agent".into()], false)
        .await
        .unwrap();
    let portal = k.create_user(&admin, "portal@acme.test", "Pat Portal", Some(PASSWORD), &[], false).await.unwrap();
    k.set_user_kind(&admin, portal, "portal").await.unwrap();
    let service = k.create_user(&admin, "service@acme.test", "Svc", Some(PASSWORD), &["Desk".into()], false).await.unwrap();
    k.set_user_kind(&admin, service, "service").await.unwrap();
    let full = k.load_ctx(acme, staff, None, "it".into()).await.unwrap();
    let customer = id_of(&k.insert(&full, "Customer", obj(json!({"customer_name": "Globex"}))).await.unwrap());
    let lead = id_of(&k.insert(&full, "Lead", obj(json!({"first_name": "Lee", "organization_name": "Globex"}))).await.unwrap());
    let ticket = id_of(&k.insert(&full, "Ticket", obj(json!({"subject": "Printer on fire", "customer": customer.to_string()}))).await.unwrap());
    let missing = Uuid::now_v7();

    // Notifications and a mention, about a lead and about a ticket.
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    ferrum_kernel::notify::send(&mut tx, acme, staff, "About the lead", None, Some("Lead"), Some(lead)).await.unwrap();
    ferrum_kernel::notify::send(&mut tx, acme, staff, "About the ticket", None, Some("Ticket"), Some(ticket)).await.unwrap();
    tx.commit().await.unwrap();
    k.add_comment(&admin, "Ticket", ticket, "@Sam look", &[staff]).await.unwrap();

    // Credentials, made without zones.
    let open = server(&base, &st, Zones::default()).await;
    let sales = server(&base, &st, (*zones).clone()).await;
    let (staff_token, staff_cookie) = (sign_in(&open, "staff@acme.test", false).await.0.unwrap(), sign_in(&open, "staff@acme.test", true).await.1.unwrap());
    let (admin_token, admin_cookie) = (sign_in(&open, "admin@acme.test", false).await.0.unwrap(), sign_in(&open, "admin@acme.test", true).await.1.unwrap());
    let staff_key = k.create_api_key(&admin, staff, "it", None, None).await.unwrap().1;
    let staff_dav = k.create_api_key(&admin, staff, "dav", None, Some("dav")).await.unwrap().1;
    let staff_oauth = oauth_token(&open, &k, &full).await;
    let admin_key = k.create_api_key(&admin, root, "it", None, None).await.unwrap().1;
    let portal_token = sign_in(&open, "portal@acme.test", false).await.0.unwrap();
    let service_key = k.create_api_key(&admin, service, "it", None, None).await.unwrap().1;
    let creds = [
        ("token", Cred::Bearer(&staff_token)),
        ("cookie", Cred::Cookie(&staff_cookie)),
        ("api key", Cred::Bearer(&staff_key)),
        ("oauth", Cred::Bearer(&staff_oauth)),
    ];

    // ── The wall, for every credential of the staff member ──
    let visible = [
        format!("/api/v1/Lead/{lead}"),
        "/api/v1/Lead?limit=50".to_string(),
        format!("/api/v1/Customer/{customer}"),
        "/api/meta/Lead".to_string(),
        format!("/api/cards/Lead/{lead}"),
    ];
    // A hidden record and a missing one, side by side: the same answer, word for word.
    let hidden_pairs = [
        (format!("/api/v1/Ticket/{ticket}"), format!("/api/v1/Ticket/{missing}")),
        (format!("/api/cards/Ticket/{ticket}"), format!("/api/cards/Ticket/{missing}")),
        (format!("/api/v1/Ticket/{ticket}/audit"), format!("/api/v1/Ticket/{missing}/audit")),
        (format!("/api/v1/Ticket/{ticket}/approval"), format!("/api/v1/Ticket/{missing}/approval")),
    ];
    for (name, cred) in creds {
        for path in &visible {
            let (a, b) = (call(&sales, Method::GET, SALES_HOST, path, cred, None).await, call(&open, Method::GET, SALES_HOST, path, cred, None).await);
            assert_eq!(a.0, StatusCode::OK, "{name} {path}: {}", a.1);
            let strip = |s: &str| {
                serde_json::from_str::<Value>(s).map(|mut v| {
                    v.as_object_mut().map(|o| o.remove("_ms"));
                    v
                })
            };
            assert_eq!(strip(&a.1).ok(), strip(&b.1).ok(), "{name} {path}: as with no zones");
        }
        for (hidden, gone) in &hidden_pairs {
            let a = norm(call(&sales, Method::GET, SALES_HOST, hidden, cred, None).await);
            let b = norm(call(&sales, Method::GET, SALES_HOST, gone, cred, None).await);
            assert_eq!(a, b, "{name}: {hidden} answers as {gone}");
            assert!(!a.0.is_success() && !a.1.contains("Printer"), "{name} {hidden}: {}", a.1);
            assert!(call(&open, Method::GET, SALES_HOST, hidden, cred, None).await.0.is_success(), "{name}: {hidden} without zones");
        }
        for path in ["/api/v1/Ticket", "/api/meta/Ticket", "/api/v1/XNote"] {
            let (status, body) = call(&sales, Method::GET, SALES_HOST, path, cred, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{name} {path}: {body}");
        }
        let (status, body) = call(&sales, Method::POST, SALES_HOST, "/api/v1/Ticket", cred, Some(json!({"subject": "new"}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}: no ticket is made: {body}");
        // The app endpoints: Helpdesk's refused outright, CRM's as without zones.
        let (status, body) = call(&sales, Method::GET, SALES_HOST, "/api/app/helpdesk/anything", cred, None).await;
        assert!(refused_by_zone(status, &body), "{name}: helpdesk's endpoint: {status} {body}");
        assert_eq!(
            call(&sales, Method::GET, SALES_HOST, "/api/app/crm/anything", cred, None).await.0,
            call(&open, Method::GET, SALES_HOST, "/api/app/crm/anything", cred, None).await.0,
            "{name}: crm's endpoint"
        );
        // Search, the metadata, reports, notifications, My work, MCP.
        let (_, found) = call(&sales, Method::GET, SALES_HOST, "/api/search?q=Printer", cred, None).await;
        assert!(!found.contains("Printer"), "{name}: search: {found}");
        assert!(call(&open, Method::GET, SALES_HOST, "/api/search?q=Printer", cred, None).await.1.contains("Printer"), "{name}: search without zones");
        let (status, meta) = call(&sales, Method::GET, SALES_HOST, "/api/meta", cred, None).await;
        let meta: Value = serde_json::from_str(&meta).unwrap_or_else(|_| panic!("{name}: meta: {status} {meta}"));
        assert!(meta["entities"].is_array(), "{name}: meta: {status} {meta}");
        assert!(meta["entities"].as_array().unwrap().iter().all(|e| e["name"] != "Ticket" && e["package"] != "helpdesk"), "{name}: meta");
        assert!(meta["entities"].as_array().unwrap().iter().any(|e| e["name"] == "Lead"));
        assert!(!meta["packages"].as_array().unwrap().contains(&json!("helpdesk")));
        let (_, reports) = call(&sales, Method::GET, SALES_HOST, "/api/reports", cred, None).await;
        let (_, all) = call(&open, Method::GET, SALES_HOST, "/api/reports", cred, None).await;
        let names = |r: &str| -> Vec<String> {
            serde_json::from_str::<Value>(r).unwrap()["reports"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
        };
        let (here, there) = (names(&reports), names(&all));
        assert!(
            there.contains(&"support_agent_workload".to_string()) && !here.contains(&"support_agent_workload".to_string()),
            "{name}: helpdesk's reports: {here:?}"
        );
        assert!(here.iter().all(|r| there.contains(r)), "{name}: nothing more than without zones");
        let (status, body) = call(&sales, Method::GET, SALES_HOST, "/api/reports/support_agent_workload", cred, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}: helpdesk's report is not there: {body}");
        let (_, notes) = call(&sales, Method::GET, SALES_HOST, "/api/notifications", cred, None).await;
        assert!(notes.contains("About the lead") && !notes.contains("About the ticket") && !notes.contains("look"), "{name}: notifications: {notes}");
        let (_, everything) = call(&open, Method::GET, SALES_HOST, "/api/notifications", cred, None).await;
        assert!(everything.contains("About the ticket"), "{name}: notifications without zones");
        let (_, work) = call(&sales, Method::GET, SALES_HOST, "/api/work", cred, None).await;
        assert!(!work.contains("look") && !work.contains(&ticket.to_string()), "{name}: my work: {work}");
        for (method, params) in [("resources/list", json!({})), ("resources/read", json!({"uri": "ferrum://overview"}))] {
            let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
            let (_, here) = call(&sales, Method::POST, SALES_HOST, "/mcp", cred, Some(body.clone())).await;
            let (_, there) = call(&open, Method::POST, SALES_HOST, "/mcp", cred, Some(body)).await;
            assert!(!here.contains("Ticket") && here.contains("Lead") && there.contains("Ticket"), "{name}: MCP {method}: {here}");
        }
    }
    // Files: one on the ticket answers as a file that is not there.
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let file = Uuid::now_v7();
    sqlx::query("insert into file_object (tenant_id, id, file_name, content_type, storage_key, size_bytes, entity, record_id, created_by, sha256) values ($1, $2, 'x.txt', 'text/plain', 'k/x', 1, 'Ticket', $3, $4, 'x')")
        .bind(acme)
        .bind(file)
        .bind(ticket)
        .bind(staff)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        norm(call(&sales, Method::GET, SALES_HOST, &format!("/files/{file}"), Cred::Bearer(&staff_token), None).await),
        norm(call(&sales, Method::GET, SALES_HOST, &format!("/files/{missing}"), Cred::Bearer(&staff_token), None).await),
        "a ticket's file answers as a missing one"
    );
    // Exports: of a hidden entity, none; a bulk job on one is refused when queued.
    let (status, body) = call(&sales, Method::GET, SALES_HOST, "/api/v1/Ticket/_export", Cred::Bearer(&staff_token), None).await;
    assert!(!status.is_success() && !body.contains("Printer"), "an export of tickets: {status} {body}");
    assert!(call(&sales, Method::GET, SALES_HOST, "/api/v1/Lead/_export", Cred::Bearer(&staff_token), None).await.0.is_success());
    let (status, _) = call(
        &sales,
        Method::POST,
        SALES_HOST,
        "/api/v1/Ticket/_bulk/jobs",
        Cred::Bearer(&staff_token),
        Some(json!({"filters": {}, "op": "update", "values": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // An app password of calendar and contacts apps: as without zones where the app is not installed.
    assert_eq!(
        call(&sales, Method::from_bytes(b"PROPFIND").unwrap(), SALES_HOST, "/api/dav/comms/", Cred::Basic("staff@acme.test", &staff_dav), None).await.0,
        call(&open, Method::from_bytes(b"PROPFIND").unwrap(), SALES_HOST, "/api/dav/comms/", Cred::Basic("staff@acme.test", &staff_dav), None).await.0
    );
    // A socket keeps its zone, and is narrowed by it.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::HOST, SALES_HOST.parse().unwrap());
    headers.insert("x-forwarded-for", "10.1.2.3".parse().unwrap());
    headers.insert(header::AUTHORIZATION, format!("Bearer {staff_token}").parse().unwrap());
    let entry = ferrum_server::extract::socket_entry(&sales.st, &headers).await;
    let (_, socket) = ferrum_server::extract::socket_credential(&sales.st, &headers, &entry).await.unwrap().unwrap();
    assert!(socket.sees("Lead") && !socket.sees("Ticket"), "a live socket's context is narrowed");
    let again = ferrum_server::extract::ctx_from_token(&sales.st, &staff_token, "it".into(), &entry, false).await.unwrap();
    assert!(!again.sees("Ticket") && again.zone.as_ref().and_then(|e| e.name().map(str::to_string)) == Some("sales".into()), "and rechecked in it");

    // ── The administrator is not one there ──
    for cred in [Cred::Bearer(&admin_token), Cred::Bearer(&admin_key), Cred::Cookie(&admin_cookie)] {
        let (status, body) = call(&sales, Method::GET, SALES_HOST, "/api/admin/users", cred, None).await;
        assert!(refused_by_zone(status, &body), "the admin API: {status} {body}");
        assert_eq!(call(&sales, Method::GET, STAFF_HOST, "/api/admin/users", cred, None).await.0, StatusCode::OK, "at the staff address");
        let (status, me) = call(&sales, Method::GET, SALES_HOST, "/api/auth/me", cred, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!me.contains("\"is_admin\":true"), "{me}");
        assert_eq!(call(&sales, Method::GET, SALES_HOST, &format!("/api/v1/Ticket/{ticket}"), cred, None).await.0, StatusCode::NOT_FOUND);
    }
    let (_, answer) = call(&sales, Method::GET, SALES_HOST, "/_zone", Cred::Bearer(""), None).await;
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert_eq!((answer["zone"].clone(), answer["staff"].clone(), answer["admin"].clone()), (json!("sales"), json!(["crm"]), json!(false)));

    // ── Pages ──
    let (status, body) = call(&sales, Method::GET, SALES_HOST, "/admin/", Cred::Cookie(&admin_cookie), None).await;
    assert!(plain_page(status, &body), "administration: {status}");
    let ticket_route = ["helpdesk", "support"];
    let mut hidden_page = false;
    for r in ticket_route {
        let (status, body) = call(&sales, Method::GET, SALES_HOST, &format!("/{r}/"), Cred::Cookie(&staff_cookie), None).await;
        if status != StatusCode::NOT_FOUND {
            assert!(plain_page(status, &body), "helpdesk's page /{r}/: {status}");
            hidden_page = true;
        }
    }
    assert!(hidden_page, "helpdesk has a page");
    assert_eq!(
        call(&sales, Method::GET, SALES_HOST, "/crm/", Cred::Cookie(&staff_cookie), None).await.0,
        call(&open, Method::GET, SALES_HOST, "/crm/", Cred::Cookie(&staff_cookie), None).await.0,
        "crm's page as without zones"
    );

    // ── A notification pushed live: not to a socket opened in the zone when it is about a hidden record ──
    let hub = ferrum_server::live::LiveHub::new();
    let mut office_headers = headers.clone();
    office_headers.insert(header::HOST, STAFF_HOST.parse().unwrap());
    let office_entry = ferrum_server::extract::socket_entry(&sales.st, &office_headers).await;
    let (_, office_socket) = ferrum_server::extract::socket_credential(&sales.st, &office_headers, &office_entry).await.unwrap().unwrap();
    let mut in_sales = hub.register(Uuid::now_v7(), socket.clone());
    let mut in_office = hub.register(Uuid::now_v7(), office_socket);
    for (entity, record) in [("Ticket", ticket), ("Lead", lead)] {
        let push = json!({"tenant": acme, "user": staff, "notification": {"id": Uuid::now_v7(), "title": entity, "entity": entity, "record_id": record, "kind": "general"}});
        hub.dispatch(&k, &push.to_string()).await;
    }
    let drain = |rx: &mut tokio::sync::mpsc::Receiver<String>| std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>().join("\n");
    let (here, there) = (drain(&mut in_sales), drain(&mut in_office));
    assert!(here.contains("Lead") && !here.contains("Ticket"), "pushed in the zone: {here}");
    assert!(there.contains("Lead") && there.contains("Ticket"), "pushed where staff are admitted wholly: {there}");

    // ── A plugin's signed-in endpoint is refused unless an app the zone names installed it; its public one is not ──
    let mut manifest: Value = serde_json::from_str(include_str!("../../crates/plugins/tests/fixtures/device_gateway.json")).unwrap();
    manifest["hooks"] = json!([]);
    manifest["subscriptions"] = json!([]);
    manifest["permissions"].as_array_mut().unwrap().retain(|p| p["entity"] != "Customer");
    let manifest: ferrum_plugins::NewPlugin = serde_json::from_value(manifest).unwrap();
    st.plugins.install(&k, &admin, manifest, include_bytes!("../../crates/plugins/tests/fixtures/device_gateway.wasm")).await.unwrap();
    let (status, body) = call(&sales, Method::GET, SALES_HOST, "/api/ext/device-gateway/status", Cred::Bearer(&staff_token), None).await;
    assert!(refused_by_zone(status, &body), "a plugin no named app installed: {status} {body}");
    assert!(!refused_by_zone(call(&open, Method::GET, SALES_HOST, "/api/ext/device-gateway/status", Cred::Bearer(&staff_token), None).await.0, ""));
    let req = Request::builder().uri("/api/public/acme/device-gateway/status").header(header::HOST, SALES_HOST).header("x-forwarded-for", "10.1.2.3");
    let res = sales.router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "its public callback works in the zone");
    assert_eq!(
        norm(call(&sales, Method::GET, SALES_HOST, "/api/ext/device-gateway/status", Cred::Bearer(&service_key), None).await),
        norm(call(&open, Method::GET, SALES_HOST, "/api/ext/device-gateway/status", Cred::Bearer(&service_key), None).await),
        "a service account's call as without zones"
    );

    // ── Outside people and machines are untouched ──
    for (who, cred) in [("portal", Cred::Bearer(&portal_token)), ("service", Cred::Bearer(&service_key))] {
        for path in ["/api/auth/me".to_string(), format!("/api/v1/Ticket/{ticket}"), format!("/api/v1/Lead/{lead}"), "/api/notifications".into()] {
            assert_eq!(
                norm(call(&sales, Method::GET, SALES_HOST, &path, cred, None).await),
                norm(call(&open, Method::GET, SALES_HOST, &path, cred, None).await),
                "{who} {path}"
            );
        }
    }

    // ── 1. Work that runs later keeps the zone ──
    let op: ferrum_kernel::bulk::BulkOp = serde_json::from_value(json!({"op": "update", "values": {"priority": "Urgent"}})).unwrap();
    let job = traced(async {
        // Queued as from the zone: the queue checks with the person's rights as they came in, unlimited here.
        let ctx = in_zone(&k, &zones, acme, staff, "sales").await;
        assert!(!ctx.sees("Ticket"));
        Trace::mark_zone("sales");
        k.bulk_job(&full, "Ticket", Some(vec![ticket]), None, &op).await.unwrap()
    })
    .await;
    assert_eq!(scalar::<Option<String>>(&k, "select zone from job where id = $1", job).await.as_deref(), Some("sales"), "the job names its zone");
    while k.run_jobs_once(10).await.unwrap() > 0 {}
    assert_eq!(k.get(&admin, "Ticket", ticket, false).await.unwrap()["priority"], "Medium", "the worker did it in the zone: nothing to the ticket");
    // A worker that does not know the zone does nothing.
    let lost = traced(async {
        Trace::mark_zone("sales");
        k.bulk_job(&full, "Lead", Some(vec![lead]), None, &serde_json::from_value(json!({"op": "update", "values": {"job_title": "X"}})).unwrap())
            .await
            .unwrap()
    })
    .await;
    k.set_zones(Arc::new(Zones::default()));
    while k.run_jobs_once(10).await.unwrap() > 0 {}
    k.set_zones(zones.clone());
    let (status, error): (String, Option<String>) = {
        let mut tx = k.db.system_tx().await.unwrap();
        let r = sqlx::query_as("select status, last_error from job where id = $1").bind(lost).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        r
    };
    assert_eq!(status, "failed", "a zone the worker does not hold fails the work, closed");
    assert!(error.unwrap_or_default().contains("zone"));
    // From inside (no request): unlimited, as before.
    let inside = k.bulk_job(&full, "Ticket", Some(vec![ticket]), None, &op).await.unwrap();
    assert_eq!(scalar::<Option<String>>(&k, "select zone from job where id = $1", inside).await, None);
    while k.run_jobs_once(10).await.unwrap() > 0 {}
    assert_eq!(k.get(&admin, "Ticket", ticket, false).await.unwrap()["priority"], "Urgent", "work queued from inside reaches everything");
    k.update(&admin, "Ticket", ticket, obj(json!({"priority": "Medium"})), None).await.unwrap();

    // ── 2. Buttons, effects and links ──
    let own = in_zone(&k, &zones, acme, staff, "own").await;
    assert!(own.sees("XNote") && !own.sees("Ticket"), "custom: true shows the workspace's own entities");
    let sales_ctx = in_zone(&k, &zones, acme, staff, "sales").await;
    assert!(!sales_ctx.sees("XNote"), "custom entities are hidden unless the rule says so");
    let e = k.insert(&own, "XNote", obj(json!({"title": "n", "ticket": ticket.to_string()}))).await.unwrap_err();
    assert!(e.to_string().contains("not installed") || format!("{e:?}").contains("not installed"), "a link set to a hidden record: {e:?}");
    let n1 = id_of(&k.insert(&admin, "XNote", obj(json!({"title": "n1", "ticket": ticket.to_string(), "approver": root.to_string()}))).await.unwrap());
    k.update(&own, "XNote", n1, obj(json!({"title": "n1 again"})), None).await.expect("a link already there, left as it was, stays");
    // Refused, saying why: the person pressed a button on a record they see, so "not installed" would mislead (this is
    // what an app's hook writing another app's records in the zone meets too, the CRM-to-ERP connector's among them).
    let e = k.run_action(&own, "XNote", n1, "x_touch", json!({})).await.unwrap_err();
    assert!(
        e.to_string().contains("which the zone own leaves out") && e.to_string().contains("helpdesk"),
        "a button whose effect writes a hidden app's record: {e}"
    );
    assert_eq!(k.get(&admin, "Ticket", ticket, false).await.unwrap()["priority"], "Medium");
    let office = in_zone(&k, &zones, acme, staff, "office").await;
    k.run_action(&office, "XNote", n1, "x_touch", json!({})).await.expect("in the office");
    assert_eq!(k.get(&admin, "Ticket", ticket, false).await.unwrap()["priority"], "Urgent");
    k.update(&admin, "Ticket", ticket, obj(json!({"priority": "Medium"})), None).await.unwrap();
    // `links.` reads a hidden record as absent, and no message shows anything of it.
    k.run_action(&own, "XNote", n1, "x_check", json!({})).await.expect("links.ticket is null in the zone");
    let refused = k.run_action(&office, "XNote", n1, "x_check", json!({})).await.unwrap_err();
    assert!(refused.to_string().contains("linked to a ticket"), "in the office it reads the ticket: {refused}");

    // An automation set going by a save in the zone reaches no hidden app.
    let auto = json!({"name": "Touch", "entity": "XNote", "event": "updated", "actions": [{"type": "update_linked", "link": "ticket", "values": {"priority": "'High'"}}]});
    // What is already waiting is handled first, before the automation exists.
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    ferrum_kernel::automation::save(&k, &admin, &obj(auto)).await.unwrap();
    traced(async {
        let ctx = in_zone(&k, &zones, acme, staff, "own").await;
        k.update(&ctx, "XNote", n1, obj(json!({"title": "saved in the zone"})), None).await.unwrap();
    })
    .await;
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!(k.get(&admin, "Ticket", ticket, false).await.unwrap()["priority"], "Medium", "the automation acted within the zone");
    k.update(&office, "XNote", n1, obj(json!({"title": "saved in the office"})), None).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!(k.get(&admin, "Ticket", ticket, false).await.unwrap()["priority"], "High", "and outside it, as before");
    k.update(&admin, "Ticket", ticket, obj(json!({"priority": "Medium"})), None).await.unwrap();

    // ── 3. Approvals: asked for in the zone, decided by whoever may read the record ──
    traced(async {
        let ctx = in_zone(&k, &zones, acme, staff, "own").await;
        k.transition(&ctx, "XNote", n1, "Finish", None).await.unwrap();
    })
    .await;
    let req: Option<String> = {
        let mut tx = k.db.system_tx().await.unwrap();
        let z = sqlx::query_scalar("select zone from approval_request where record_id = $1").bind(n1).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        z
    };
    assert_eq!(req.as_deref(), Some("own"), "the request names the zone it was asked in");
    let admin_in_sales = in_zone(&k, &zones, acme, root, "sales").await;
    assert!(!admin_in_sales.is_admin);
    let e = k.decide_approval(&admin_in_sales, "XNote", n1, true, None).await.unwrap_err();
    assert!(matches!(e, ferrum_kernel::Error::NotFound(_)), "an approver who cannot read the record there: {e:?}");
    k.decide_approval(&admin, "XNote", n1, true, None).await.expect("decided where it is read");
    // What completing it wrote is marked with the requester's zone, not the decider's (who decided from inside).
    let last: Option<String> = {
        let mut tx = k.db.system_tx().await.unwrap();
        let z = sqlx::query_scalar("select zone from outbox where record_id = $1 order by id desc limit 1").bind(n1).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        z
    };
    assert_eq!(last.as_deref(), Some("own"), "the completion's events carry the zone the request was made in");
    assert_eq!(k.get(&admin, "XNote", n1, false).await.unwrap()["workflow_state"], "Done", "completed as the requester, in their zone");

    // ── 4. Role names are narrowed with rights ──
    assert!(sales_ctx.roles.contains(&"Sales User".to_string()) && sales_ctx.roles.contains(&"Desk".to_string()));
    assert!(!sales_ctx.roles.contains(&"Support Agent".to_string()), "a role only Helpdesk ships: {:?}", sales_ctx.roles);
    assert!(office.roles.contains(&"Support Agent".to_string()));

    // ── The same person where staff are admitted wholly has everything; `back` keeps only administration out ──
    assert!(office.sees("Ticket") && office.sees("XNote"));
    assert!(k.get(&office, "Ticket", ticket, false).await.is_ok());
    let back = in_zone(&k, &zones, acme, root, "back").await;
    assert!(!back.is_admin && back.sees("Ticket"), "staff: all, admin: false");
    assert!(k.get(&back, "Ticket", ticket, false).await.is_err(), "an administrator there has their roles' rights: none here");
    let staff_back = in_zone(&k, &zones, acme, staff, "back").await;
    assert!(k.get(&staff_back, "Ticket", ticket, false).await.is_ok() && staff_back.roles.contains(&"Support Agent".to_string()), "staff keep everything");

    // Report mode: nothing hidden, and counted.
    let reported = Arc::new(
        Zones::parse(ZONES, "", "", "report")
            .map(|mut z| {
                z.resolve(|a| (a == "crm").then(|| vec!["crm".to_string()])).unwrap();
                z
            })
            .unwrap(),
    );
    let r = in_zone(&k, &reported, acme, root, "sales").await;
    assert!(r.is_admin && k.get(&r, "Ticket", ticket, false).await.is_ok(), "report mode admits as before");
    let s = in_zone(&k, &reported, acme, staff, "sales").await;
    assert!(k.get(&s, "Ticket", ticket, false).await.is_ok());
    assert!(ferrum_kernel::zones::hidden().iter().any(|(z, m, w, n)| z == "sales" && *m == "report" && *w == "entity" && *n > 0), "what was hidden is counted");
    assert!(ferrum_kernel::zones::hidden().iter().any(|(z, m, w, _)| z == "sales" && *m == "report" && *w == "admin"));
    let _ = (portal, service);
}
