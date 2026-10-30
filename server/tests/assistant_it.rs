//! The assistant against a real PostgreSQL (`FERRUM_ASSISTANT_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_assistant_test`; the database is reset) and a
//! stand-in for the model's API on a local port: a question answered with a read that runs at once;
//! a change that waits for the person and runs when allowed; one refused, which does not run; the
//! conversation sent back exactly as stored; a workspace that turned it off, and a guest.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::assistant::{self, Assistant, Config, Env, Input};
use serde_json::{Value, json};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

// ───────────────────────── the model's stand-in ─────────────────────────

#[derive(Clone, Default)]
struct Model {
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

/// A reply as the API streams it.
fn sse(n: usize, blocks: &[Value], stop: &str) -> String {
    let mut out = String::new();
    let mut ev = |v: Value| out.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
    ev(json!({"type": "message_start", "message": {"id": format!("msg_{n}"), "model": "claude-opus-5-5", "usage": {"input_tokens": 10}}}));
    ev(json!({"type": "ping"}));
    for (i, b) in blocks.iter().enumerate() {
        match b["type"].as_str().unwrap() {
            "thinking" => {
                ev(json!({"type": "content_block_start", "index": i, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}));
                ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "thinking_delta", "thinking": b["text"]}}));
                ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "signature_delta", "signature": format!("sig-{n}-{i}")}}));
            }
            "text" => {
                ev(json!({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}));
                ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": b["text"]}}));
            }
            _ => {
                ev(json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}}));
                let input = b["input"].to_string();
                let (a, z) = input.split_at(input.len() / 2);
                for part in [a, z] {
                    ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": part}}));
                }
            }
        }
        ev(json!({"type": "content_block_stop", "index": i}));
    }
    ev(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 20}}));
    ev(json!({"type": "message_stop"}));
    out
}

async fn messages(State(m): State<Model>, headers: HeaderMap, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let n = {
        let mut r = m.requests.lock().unwrap();
        r.push((headers, body.clone()));
        r.len()
    };
    let msgs = body["messages"].as_array().unwrap();
    // Content is a list of blocks, or (the operator's check) a plain string.
    let last = match &msgs.last().unwrap()["content"] {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        other => other.as_array().cloned().unwrap_or_default(),
    };
    let result = last.iter().find(|b| b["type"] == "tool_result");
    let words = last.iter().rfind(|b| b["type"] == "text").and_then(|b| b["text"].as_str()).unwrap_or_default();
    let id = format!("toolu_{n}");
    let text = match result {
        Some(r) => {
            let call = msgs[msgs.len() - 2]["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_use").unwrap()["name"].clone();
            let answer = match (call.as_str().unwrap(), r["is_error"].as_bool().unwrap()) {
                ("create_record", false) => "Created.",
                ("create_record", true) => "Not created.",
                _ => "Here they are.",
            };
            sse(n, &[json!({"type": "text", "text": answer})], "end_turn")
        }
        None if words.contains("create") => sse(
            n,
            &[
                json!({"type": "thinking", "text": "Propose the customer."}),
                json!({"type": "text", "text": "I'll create it."}),
                json!({"type": "tool_use", "id": id, "name": "create_record", "input": {"entity": "Customer", "data": {"customer_name": format!("Made by the assistant {n}")}}}),
            ],
            "tool_use",
        ),
        None => sse(
            n,
            &[
                json!({"type": "thinking", "text": "Look them up."}),
                json!({"type": "text", "text": "Looking."}),
                json!({"type": "tool_use", "id": id, "name": "search_records", "input": {"entity": "Customer", "limit": 5}}),
            ],
            "tool_use",
        ),
    };
    Response::builder().header("content-type", "text/event-stream").body(Body::from(text)).unwrap()
}

async fn serve(m: Model) -> String {
    let app = Router::new().route("/v1/messages", post(messages)).with_state(m);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// A turn's events, to its end.
async fn turn(rx: &mut broadcast::Receiver<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Ok(v)) => {
                let end = matches!(v["type"].as_str(), Some("done" | "error"));
                out.push(v);
                if end {
                    return out;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            other => panic!("the turn did not end ({other:?}): {out:?}"),
        }
    }
}

fn kinds(events: &[Value]) -> Vec<String> {
    events.iter().map(|e| e["type"].as_str().unwrap_or_default().to_string()).collect()
}

// ───────────────────────── the scenario ─────────────────────────

#[test]
fn the_assistant_reads_at_once_and_changes_only_when_allowed() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_assistant_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
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

    let model = Model::default();
    let base_url = serve(model.clone()).await;
    let config = Config { api_key: "test-key".into(), model: "claude-opus-5-5".into(), base_url, effort: "low".into(), fallbacks: false, calls_per_hour: 100 };
    let env = Env { kernel: k.clone(), apps: None, assistant: Assistant::new(Some(config)) };
    let thread = assistant::create_thread(&k, &ctx).await.unwrap();
    let ask = |text: &str| Input::Message { text: text.into(), context: "Page: the test".into() };

    // ── a question: the read runs at once, and the answer follows ──
    let mut rx = assistant::begin(&env, &ctx, thread, ask("Which customers do we have?"), "member").await.unwrap();
    let events = turn(&mut rx).await;
    assert_eq!(events.last().unwrap(), &json!({"type": "done", "state": "idle"}), "{events:?}");
    let k_ = kinds(&events);
    assert!(k_.iter().any(|t| t == "delta") && k_.iter().filter(|t| *t == "message").count() == 4, "the question, the call, its result, the answer: {k_:?}");
    let t = assistant::thread(&env, &ctx, thread).await.unwrap();
    let roles: String = t["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap().chars().next().unwrap()).collect();
    assert_eq!((t["state"].as_str(), roles.as_str()), (Some("idle"), "uaua"));
    assert_eq!(t["title"], "Which customers do we have?");
    assert_eq!(t["messages"][0]["items"], json!([{"kind": "text", "text": "Which customers do we have?"}]), "the context block is for the model");

    // A second turn while one runs is refused.
    let mut rx = assistant::begin(&env, &ctx, thread, ask("Please create a customer"), "member").await.unwrap();
    assert!(matches!(assistant::begin(&env, &ctx, thread, ask("again"), "member").await, Err(Error::Conflict(_))));

    // ── a change: it waits for the person, then runs when allowed ──
    let events = turn(&mut rx).await;
    assert_eq!(events.last().unwrap(), &json!({"type": "done", "state": "waiting"}), "{events:?}");
    let confirm = events.iter().find(|e| e["type"] == "confirm").expect("asked");
    assert_eq!(confirm["pending"]["calls"][0]["name"], "create_record");
    let t = assistant::thread(&env, &ctx, thread).await.unwrap();
    assert_eq!((t["state"].as_str(), t["pending"]["calls"].as_array().map(Vec::len)), (Some("waiting"), Some(1)));
    let search = || async {
        let r = ferrum_mcp::handle(&k, &ctx, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "search_records", "arguments": {"entity": "Customer", "search": "Made by the assistant"}}})).await.unwrap();
        r["result"]["structuredContent"]["rows"].as_array().map(Vec::len).unwrap_or(0)
    };
    assert_eq!(search().await, 0, "nothing ran before the person decided");
    let mut rx = assistant::begin(&env, &ctx, thread, Input::Decision { allow: true }, "member").await.unwrap();
    let events = turn(&mut rx).await;
    assert_eq!(events.last().unwrap(), &json!({"type": "done", "state": "idle"}), "{events:?}");
    assert!(events.iter().any(|e| e["type"] == "tool" && e["status"] == "done"));
    assert_eq!(search().await, 1, "allowed, the change ran");

    // ── another change, refused: nothing runs, and the model hears so ──
    let mut rx = assistant::begin(&env, &ctx, thread, ask("create one more"), "member").await.unwrap();
    assert_eq!(turn(&mut rx).await.last().unwrap()["state"], "waiting");
    let mut rx = assistant::begin(&env, &ctx, thread, Input::Decision { allow: false }, "member").await.unwrap();
    turn(&mut rx).await;
    assert_eq!(search().await, 1, "refused, nothing was made");
    let t = assistant::thread(&env, &ctx, thread).await.unwrap();
    let last_result =
        t["messages"].as_array().unwrap().iter().rev().flat_map(|m| m["items"].as_array().unwrap().iter()).find(|i| i["kind"] == "result").unwrap().clone();
    assert_eq!(last_result["error"], true);
    assert!(last_result["text"].as_str().unwrap().contains("did not allow"));
    assert!(matches!(assistant::begin(&env, &ctx, thread, Input::Decision { allow: true }, "member").await, Err(Error::Conflict(_))), "nothing waits now");

    // ── what was sent: the API's version, history exactly as stored, calls paired with results ──
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 6);
    for (i, (h, body)) in requests.iter().enumerate() {
        assert_eq!(h["x-api-key"], "test-key");
        assert_eq!(h["anthropic-version"], "2026-10-21");
        assert!(h["anthropic-beta"].to_str().unwrap().contains("thinking-binding-controls-2026-08-01"));
        assert_eq!((body["model"].as_str(), body["stream"].as_bool()), (Some("claude-opus-5-5"), Some(true)));
        assert_eq!(body["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert!(body.get("fallbacks").is_none());
        assert!(body["tools"].as_array().unwrap().iter().all(|t| t["eager_input_streaming"] == true && t["input_schema"].is_object()));
        let msgs = body["messages"].as_array().unwrap();
        for (j, m) in msgs.iter().enumerate() {
            assert_eq!(m["role"], if j % 2 == 0 { "user" } else { "assistant" }, "request {i}: roles alternate");
            for b in m["content"].as_array().unwrap() {
                if b["type"] == "thinking" {
                    assert!(b["signature"].as_str().unwrap().starts_with("sig-"), "request {i}: thinking comes back unchanged");
                }
                if b["type"] == "tool_use" {
                    let next = msgs[j + 1]["content"].as_array().unwrap();
                    assert!(next.iter().any(|r| r["tool_use_id"] == b["id"]), "request {i}: every call has its result");
                }
            }
        }
        // Each request holds the previous one's messages, unchanged, at its start.
        if i > 0 {
            let before = requests[i - 1].1["messages"].as_array().unwrap();
            assert_eq!(&msgs[..before.len()], &before[..], "request {i}: history is only appended to");
        }
    }

    // ── the operator's check: one short call with the key, no tools ──
    let (answered_by, _) = env.assistant.check().await.expect("the check");
    assert_eq!(answered_by, "claude-opus-5-5");
    let checked = model.requests.lock().unwrap().last().unwrap().1.clone();
    assert!(checked.get("tools").is_none() && checked["messages"].as_array().unwrap().len() == 1);
    assert!(Assistant::new(None).check().await.unwrap_err().contains("FERRUM_ASSISTANT_API_KEY"));

    // ── a workspace that turned it off, and a guest ──
    let mut off = serde_json::Map::new();
    off.insert("assistant".into(), json!(false));
    k.update_tenant_settings(&ctx, &off).await.unwrap();
    env.assistant.setting_changed(tenant);
    assert!(!env.assistant.available(&k, &ctx, "member").await);
    assert!(matches!(assistant::begin(&env, &ctx, thread, ask("hello"), "member").await, Err(Error::Forbidden(_))));
    let mut on = serde_json::Map::new();
    on.insert("assistant".into(), json!(true));
    k.update_tenant_settings(&ctx, &on).await.unwrap();
    env.assistant.setting_changed(tenant);
    assert!(env.assistant.available(&k, &ctx, "member").await);
    assert!(matches!(assistant::begin(&env, &ctx, thread, ask("hello"), "guest").await, Err(Error::Forbidden(_))), "the workspace's own people only");
    let none = Env { kernel: k.clone(), apps: None, assistant: Assistant::new(None) };
    assert!(!none.assistant.available(&k, &ctx, "member").await, "no key, no assistant");

    // ── the bar's search: records by title, as the person may read them ──
    let found = ferrum_server::search::search(&k, &ctx, "made by the assistant", 10).await.unwrap();
    let titles: Vec<&str> = found["results"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap()).collect();
    assert_eq!(titles.len(), 1, "the one customer the allowed change made: {found}");
    assert!(titles[0].starts_with("Made by the assistant"));
    assert_eq!(found["results"][0]["url"].as_str().unwrap(), format!("/open/Customer/{}", found["results"][0]["id"].as_str().unwrap()));
    assert_eq!(ferrum_server::search::search(&k, &ctx, "m", 10).await.unwrap()["results"], json!([]), "one letter finds nothing");
    let nobody = k.create_user(&ctx, "nobody@acme.test", "No Roles", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let nobody = k.load_ctx(tenant, nobody, None, "it".into()).await.unwrap();
    assert_eq!(ferrum_server::search::search(&k, &nobody, "made by the assistant", 10).await.unwrap()["results"], json!([]), "nothing the person may not read");

    // ── deleting a conversation ──
    assistant::delete_thread(&env, &ctx, thread).await.unwrap();
    assert!(matches!(assistant::thread(&env, &ctx, thread).await, Err(Error::NotFound(_))));
}
