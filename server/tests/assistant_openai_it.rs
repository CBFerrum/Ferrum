//! The assistant over an OpenAI-compatible chat API, against a real PostgreSQL
//! (`FERRUM_ASSISTANT_TEST_DATABASE_URL`, reset first) and a stand-in for the API on a local port:
//! the model asks for a read, gets its result back as a tool message, and answers; the key goes as
//! a bearer token, and the turn ends as it does with Anthropic's API.

mod common;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::assistant::{self, Assistant, Config, Env, Input, Protocol};
use serde_json::{Value, json};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Default)]
struct Model {
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

fn chunks(parts: &[Value]) -> String {
    parts.iter().map(|p| format!("data: {p}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

async fn completions(State(m): State<Model>, headers: HeaderMap, body: String) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap();
    let answered = body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool");
    m.requests.lock().unwrap().push((headers, body));
    let text = if answered {
        chunks(&[
            json!({"choices": [{"delta": {"role": "assistant", "content": "There is one "}}]}),
            json!({"choices": [{"delta": {"content": "customer."}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 40, "completion_tokens": 6}}),
        ])
    } else {
        chunks(&[
            json!({"choices": [{"delta": {"role": "assistant", "content": "Looking."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "search_records", "arguments": "{\"entity\": \"Cus"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "tomer\", \"limit\": 5}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 9}}),
        ])
    };
    Response::builder().header("content-type", "text/event-stream").body(Body::from(text)).unwrap()
}

#[test]
fn the_assistant_speaks_an_openai_compatible_api() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_assistant_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let dir = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let packages = ["core", "contacts", "parties"].map(|p| ferrum_meta::catalog::load_package_dir(&dir.join(p)).expect(p));
    let k = Kernel::boot(db, Catalog::from_packages(packages.to_vec()).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "contacts".into(), "parties".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    k.insert(&ctx, "Customer", json!({"customer_name": "Globex"}).as_object().cloned().unwrap()).await.unwrap();

    let model = Model::default();
    let app = Router::new().route("/v1/chat/completions", post(completions)).with_state(model.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config {
        protocol: Protocol::OpenAi,
        api_key: "test-key".into(),
        model: "gpt-test".into(),
        base_url,
        effort: "low".into(),
        fallbacks: false,
        calls_per_hour: 100,
        models: vec![],
        monthly_tokens: None,
    };
    let env = Env { kernel: k.clone(), apps: None, assistant: Assistant::new(Some(config)) };
    let thread = assistant::create_thread(&k, &ctx).await.unwrap();
    let mut rx =
        assistant::begin(&env, &ctx, thread, Input::Message { text: "Which customers do we have?".into(), context: String::new() }, "member").await.unwrap();
    let mut events = Vec::new();
    loop {
        let v = tokio::time::timeout(Duration::from_secs(30), rx.recv()).await.expect("the turn ends").expect("an event");
        let end = matches!(v["type"].as_str(), Some("done" | "error"));
        events.push(v);
        if end {
            break;
        }
    }
    assert_eq!(events.last().unwrap(), &json!({"type": "done", "state": "idle"}), "{events:?}");
    let t = assistant::thread(&env, &ctx, thread).await.unwrap();
    let roles: String = t["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap().chars().next().unwrap()).collect();
    assert_eq!(roles, "uaua", "the question, the read, its result, the answer");
    let answer = t["messages"][3]["items"].to_string();
    assert!(answer.contains("There is one customer."), "{answer}");

    let used = assistant::usage(&k, &ctx).await.unwrap();
    let row = &used["month"][0];
    assert_eq!((&row["input_tokens"], &row["output_tokens"]), (&json!(70), &json!(15)), "its tokens are metered: {used}");

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].0.get("authorization").and_then(|v| v.to_str().ok()), Some("Bearer test-key"), "the key as a bearer token");
    assert_eq!(requests[0].1["model"], "gpt-test");
    assert!(requests[0].1["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "search_records"));
    let tool = requests[1].1["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").expect("the read's result went back");
    assert_eq!(tool["tool_call_id"], "call_1");
    assert!(tool["content"].as_str().unwrap().contains("Globex"), "{tool}");
}
