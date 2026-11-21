//! The agents' console over HTTP (gapclose/design/A2-A3-agents.md 2.5, 2.11; AG-2), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, reset first): the whole server, booted as the binary boots it, behind its own router.
//! Administrators see every agent and members their own; an owner changes only what §2.2's table lets them, against the
//! revision they read; a key is shown once; usage and activity are paged and bounded; nothing changes with a key, the
//! owner's own included.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
const HOST: &str = "acme.agents.test";

#[test]
fn the_agents_console() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

#[derive(Clone, Copy)]
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

async fn call(router: &axum::Router, method: Method, path: &str, cred: Cred<'_>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, HOST).header("x-forwarded-for", "10.1.2.3");
    match cred {
        Cred::Bearer(t) => req = req.header(header::AUTHORIZATION, format!("Bearer {t}")),
        Cred::Cookie(t) => {
            req = req
                .header(header::COOKIE, format!("{}={t}", ferrum_server::shell::cookie_name(false)))
                .header("sec-fetch-site", "same-origin")
                .header(header::ORIGIN, format!("http://{HOST}"))
        }
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// A browser session of `email`: its cookie.
async fn cookie(router: &axum::Router, email: &str) -> String {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, HOST)
        .header("x-forwarded-for", "10.1.2.3")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"tenant": "acme", "email": email, "password": PASSWORD}).to_string()))
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "{email} signs in");
    res.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
        })
        .expect("a cookie")
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, url);
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let ctx = AppContext::builder(Environment::Test, base.config.clone()).build();
    ctx.shared_store.insert(st.clone());
    let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    let router = App::after_routes(router, &ctx).await.expect("layers");

    let acme = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["contacts".into()],
            settings: None,
        })
        .await
        .unwrap();
    k.add_operator_host("acme", HOST, "shell", None).await.unwrap();
    let root = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin_ctx = k.load_ctx(acme, root.user, None, "it".into()).await.unwrap();
    let rule = ferrum_meta::PermissionDef { entity: "Contact".into(), read: true, write: true, create: true, ..Default::default() };
    k.set_role_permission(&admin_ctx, "Desk", &rule).await.unwrap();
    let mia = k.create_user(&admin_ctx, "mia@acme.test", "Mia Member", Some(PASSWORD), &["Desk".into()], false).await.unwrap();
    let mia_key = k.create_api_key(&admin_ctx, mia, "her script", None, None).await.unwrap().1;
    let (admin, her) = (cookie(&router, "admin@acme.test").await, cookie(&router, "mia@acme.test").await);
    let (admin, her) = (Cred::Cookie(&admin), Cred::Cookie(&her));
    let at = |id: &Value, rest: &str| format!("/api/agents/{}{rest}", id.as_str().unwrap());

    // admins_list_every_agent_members_their_own
    let (status, mine) = call(&router, Method::POST, "/api/agents", her, Some(json!({"name": "Mia's", "grants": {"entities": {"Contact": ["read"]}}}))).await;
    assert_eq!(status, StatusCode::CREATED, "{mine}");
    let (status, admins) = call(&router, Method::POST, "/api/agents", admin, Some(json!({"name": "Admin's"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{admins}");
    let (status, for_her) = call(&router, Method::POST, "/api/agents", admin, Some(json!({"name": "For Mia", "owner": mia}))).await;
    assert_eq!(status, StatusCode::CREATED, "{for_her}");
    let (_, all) = call(&router, Method::GET, "/api/agents", admin, None).await;
    assert_eq!(all["agents"].as_array().unwrap().len(), 3);
    let (_, hers) = call(&router, Method::GET, "/api/agents", her, None).await;
    let names: Vec<&str> = hers["agents"].as_array().unwrap().iter().filter_map(|a| a["name"].as_str()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(!names.contains(&"Admin's"));
    let (status, _) = call(&router, Method::GET, &format!("/api/agents?owner={}", root.user), her, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&router, Method::GET, &at(&admins["id"], ""), her, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "another's agent is not there for her");
    let (_, one) = call(&router, Method::GET, &format!("/api/agents?state=active&owner={mia}"), admin, None).await;
    assert_eq!(one["agents"].as_array().unwrap().len(), 2);

    // Nothing changes with a key, her own included (403 at_screen); she still reads her console with it.
    let (status, body) = call(&router, Method::POST, "/api/agents", Cred::Bearer(&mia_key), Some(json!({"name": "Scripted"}))).await;
    assert_eq!((status, body["error"].clone()), (StatusCode::FORBIDDEN, json!("at_screen")), "{body}");
    let (status, body) = call(&router, Method::POST, &at(&mine["id"], "/pause"), Cred::Bearer(&mia_key), None).await;
    assert_eq!((status, body["error"].clone()), (StatusCode::FORBIDDEN, json!("at_screen")), "{body}");
    let (status, _) = call(&router, Method::GET, &at(&mine["id"], ""), Cred::Bearer(&mia_key), None).await;
    assert_eq!(status, StatusCode::OK);

    // an_owner_changes_only_what_the_table_lets_them
    let rev = mine["revision"].as_i64().unwrap();
    for (field, value) in [
        ("data_policy", json!({"groups": ["payroll"]})),
        ("approver_role", json!("Desk")),
        ("limits", json!({"money": {"INR": {"per_call": "10", "per_day": "10"}}})),
        ("classes", json!({"money": "allowed"})),
    ] {
        let (status, body) = call(&router, Method::PATCH, &at(&mine["id"], ""), her, Some(json!({"revision": rev, field: value}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{field}: {body}");
    }
    let until = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
    let change = json!({"revision": rev, "name": "Mia's helper", "networks": ["10.0.0.0/8"], "expires_at": until, "classes": {"external": "forbidden"},
                        "limits": {"calls_per_minute": 30}});
    let (status, changed) = call(&router, Method::PATCH, &at(&mine["id"], ""), her, Some(change)).await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!((changed["name"].clone(), changed["revision"].clone()), (json!("Mia's helper"), json!(rev + 1)));
    assert_eq!(changed["classes"]["effective"]["external"], "forbidden");
    // An administrator may change all of it.
    let (status, by_admin) =
        call(&router, Method::PATCH, &at(&mine["id"], ""), admin, Some(json!({"revision": rev + 1, "classes": {"money": "allowed"}, "approver_role": "Desk"})))
            .await;
    assert_eq!(status, StatusCode::OK, "{by_admin}");
    assert_eq!(by_admin["classes"]["set_by_administrator"], json!(["money"]));

    // a_stale_revision_is_409
    let (status, body) = call(&router, Method::PATCH, &at(&mine["id"], ""), her, Some(json!({"revision": rev + 1, "name": "Late"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // a_key_is_shown_once
    let (status, made) = call(&router, Method::POST, &at(&mine["id"], "/keys"), her, Some(json!({"name": "laptop"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let secret = made["key"].as_str().unwrap().to_string();
    assert!(secret.starts_with("fk_"));
    let (_, keys) = call(&router, Method::GET, &at(&mine["id"], "/keys"), her, None).await;
    assert_eq!(keys["keys"].as_array().unwrap().len(), 1);
    assert!(!keys.to_string().contains(&secret[3..]), "never shown again");
    let (_, people) = call(&router, Method::GET, "/api/auth/api_keys", her, None).await;
    assert_eq!(people["keys"].as_array().unwrap().len(), 1, "her own keys only, not her agent's: {people}");
    // It opens nothing yet (agents sign in from AG-3 on).
    let (status, _) = call(&router, Method::GET, "/api/auth/me", Cred::Bearer(&secret), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let key_path = format!("{}/{}", at(&mine["id"], "/keys"), made["id"].as_str().unwrap());
    let (status, _) = call(&router, Method::DELETE, &key_path, her, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&router, Method::DELETE, &key_path, her, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // usage_and_activity_are_paged_and_bounded
    for days in ["0", "401"] {
        let (status, _) = call(&router, Method::GET, &at(&mine["id"], &format!("/usage?days={days}")), her, None).await;
        assert!(status.is_client_error(), "days={days}: {status}");
    }
    let (status, usage) = call(&router, Method::GET, &at(&mine["id"], "/usage"), her, None).await;
    assert_eq!((status, usage["days"].clone()), (StatusCode::OK, json!([])));
    let (status, _) = call(&router, Method::GET, &at(&mine["id"], "/activity?limit=201"), her, None).await;
    assert!(status.is_client_error());
    let (_, first) = call(&router, Method::GET, &at(&mine["id"], "/activity?limit=2"), her, None).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let next = first["next_before"].as_i64().expect("more are left");
    let (_, rest) = call(&router, Method::GET, &at(&mine["id"], &format!("/activity?limit=50&before={next}")), her, None).await;
    let actions: Vec<&str> = first["items"].as_array().unwrap().iter().chain(rest["items"].as_array().unwrap()).filter_map(|i| i["action"].as_str()).collect();
    assert_eq!(actions, ["revoke_key", "key", "update", "update", "create"], "newest first, each once");
    assert!(rest["next_before"].is_null());

    // Pause, resume and revoke, by her at her screen.
    for (to, state) in [("pause", "paused"), ("resume", "active"), ("revoke", "revoked")] {
        let (status, body) = call(&router, Method::POST, &at(&mine["id"], &format!("/{to}")), her, None).await;
        assert_eq!((status, body["state"].clone()), (StatusCode::OK, json!(state)), "{to}: {body}");
    }
    let (status, _) = call(&router, Method::POST, &at(&mine["id"], "/resume"), her, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "revoked for good");
    let (status, _) = call(&router, Method::POST, &at(&for_her["id"], "/pause"), her, None).await;
    assert_eq!(status, StatusCode::OK, "an agent an administrator made for her is hers to pause");
    let (status, _) = call(&router, Method::POST, &at(&admins["id"], "/pause"), her, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Her key changes none of it either: a change, a key made (AG-1-2-fix 9).
    for (m, path, body) in [
        (Method::PATCH, at(&for_her["id"], ""), json!({"revision": 1, "name": "Scripted"})),
        (Method::POST, at(&for_her["id"], "/keys"), json!({"name": "scripted"})),
    ] {
        let (status, answer) = call(&router, m.clone(), &path, Cred::Bearer(&mia_key), Some(body)).await;
        assert_eq!((status, answer["error"].clone()), (StatusCode::FORBIDDEN, json!("at_screen")), "{m} {path}: {answer}");
    }

    // While the door is shut, as in a build without agents: no part of the console is there, and no agent's key opens
    // anything (AG-1-2-fix 5, 6).
    let (status, live) = call(&router, Method::POST, &at(&for_her["id"], "/keys"), her, Some(json!({"name": "before"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{live}");
    st.kernel.close_agents();
    for (m, path) in [
        (Method::GET, "/api/agents".to_string()),
        (Method::POST, "/api/agents".to_string()),
        (Method::GET, at(&for_her["id"], "")),
        (Method::GET, at(&for_her["id"], "/keys")),
        (Method::POST, at(&for_her["id"], "/keys")),
        (Method::GET, at(&for_her["id"], "/usage")),
    ] {
        let (status, answer) = call(&router, m.clone(), &path, admin, Some(json!({"name": "After"}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{m} {path}: {answer}");
    }
    let (status, _) = call(&router, Method::GET, "/api/v1/Contact", Cred::Bearer(live["key"].as_str().unwrap()), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "an agent's key opens nothing");
}
