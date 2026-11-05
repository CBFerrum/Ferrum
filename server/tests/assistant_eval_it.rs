//! The assistant's grounded-answer evaluation, against a real PostgreSQL
//! (`FERRUM_ASSISTANT_TEST_DATABASE_URL`, reset first) and a scripted model on a local port: an
//! answer read from the records passes; one made up without reading fails on both counts; each
//! names the prompt version it was answered under, whose text administrators can read back.

mod common;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::assistant::{self, Assistant, Config, Env, Protocol};
use ferrum_server::assistant_eval;
use serde_json::{Value, json};
use std::path::Path as FsPath;

fn sse(events: &[Value]) -> Response {
    let text: String = events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
    Response::builder().header("content-type", "text/event-stream").body(Body::from(text)).unwrap()
}

fn answer(blocks: Vec<Value>, stop: &str) -> Response {
    let mut events = vec![json!({"type": "message_start", "message": {"model": "scripted", "usage": {"input_tokens": 10, "output_tokens": 0}}})];
    for (i, b) in blocks.iter().enumerate() {
        // As the API streams them: a block opens empty and its text or input follows.
        if b["type"] == "text" {
            events.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}));
            events.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": b["text"]}}));
        } else {
            events
                .push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}}));
            events.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": b["input"].to_string()}}));
        }
        events.push(json!({"type": "content_block_stop", "index": i}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 5}}));
    events.push(json!({"type": "message_stop"}));
    sse(&events)
}

/// Reads customers before answering, and says what it read; told to guess, it guesses.
async fn model(body: String) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let question = messages[0]["content"].to_string();
    let result = messages.last().unwrap()["content"].as_array().and_then(|c| c.iter().find(|b| b["type"] == "tool_result").cloned());
    if question.contains("guess") {
        return answer(vec![json!({"type": "text", "text": "About 60 days, I would think."})], "end_turn");
    }
    match result {
        None => answer(vec![json!({"type": "tool_use", "id": "t1", "name": "search_records", "input": {"entity": "Customer", "limit": 10}})], "tool_use"),
        Some(r) => answer(vec![json!({"type": "text", "text": format!("From the records: {}", r["content"])})], "end_turn"),
    }
}

#[test]
fn answers_are_graded_against_the_records() {
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
            slug: "eval".into(),
            name: "Eval".into(),
            admin_email: "admin@eval.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "contacts".into(), "parties".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("eval", "admin@eval.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, Router::new().route("/v1/messages", post(model))).await.unwrap() });
    let config = Config {
        protocol: Protocol::Anthropic,
        api_key: "test-key".into(),
        model: "scripted".into(),
        base_url,
        effort: "low".into(),
        fallbacks: false,
        calls_per_hour: 100,
        models: vec![],
        monthly_tokens: None,
    };
    let env = Env { kernel: k.clone(), apps: None, assistant: Assistant::new(Some(config)) };
    let cases = assistant_eval::parse(
        "- name: read-then-answer\n  records:\n    - {entity: Customer, values: {customer_name: Globex Eval}}\n  question: Which customers do we have?\n  expect: {mentions: [Globex Eval], reads: [Customer]}\n\
         - name: made-up\n  records:\n    - {entity: Customer, values: {customer_name: Hooli Eval, payment_terms_days: 45}}\n  question: Just guess how long Hooli Eval has to pay.\n  expect: {mentions: [\"45\"], not_mentions: [\"60\"], reads: [Customer]}\n",
    )
    .unwrap();
    let outcomes = assistant_eval::run(&env, &ctx, &cases).await.unwrap();
    let (grounded, guessed) = (&outcomes[0], &outcomes[1]);
    assert!(grounded.passed, "{grounded:?}");
    assert_eq!(grounded.reads, ["Customer"]);
    assert!(!guessed.passed);
    assert_eq!(guessed.failures, ["does not say \"45\"", "says \"60\"", "answered without reading Customer"], "{guessed:?}");

    // Both were answered under one prompt version, whose text is kept.
    let version = grounded.prompt.clone().expect("a prompt version");
    assert_eq!(guessed.prompt.as_deref(), Some(version.as_str()));
    let told = assistant::prompt(&k, &ctx, &version).await.unwrap();
    assert!(told["system"].as_str().unwrap().contains("Ferrum") && told["tools"].as_array().is_some_and(|t| !t.is_empty()), "{told}");
    assert_eq!((&told["model"], &told["answers"]), (&json!("scripted"), &json!(3)), "two answers and the read before one: {told}");
}
