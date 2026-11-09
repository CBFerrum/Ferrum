//! What the assistant may still do once things change under it (Ferrum's closure review, FEO-059 to
//! FEO-062), against a real PostgreSQL (`FERRUM_ASSISTANT_TEST_DATABASE_URL`, default
//! `.../ferrum_assistant_test`; the database is reset) and a stand-in for the model that can be held
//! at a request while the test changes something:
//!
//! - a role taken away between two rounds of calls stops the turn before the next call runs, and the
//!   assistant turned off while a call is retried is not tried again (FEO-059);
//! - a later turn sends the model only what the person may still send it: earlier calls and results
//!   never, earlier answers only while what they drew on is readable and they went to the same place
//!   under the same policy (FEO-060);
//! - a call runs without asking only when this server knows it reads (a read-only action, not one the
//!   model or a tool says is harmless), and its input is checked at every depth (FEO-061);
//! - turns at once cannot spend more than the allowance between them, and a key the model writes is
//!   never shown, even split across the pieces it streams in (FEO-062).

mod common;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::{Catalog, Package};
use ferrum_server::assistant::{self, Assistant, Config, Env, Input, Protocol};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};

const PACKAGE: &str = "package: vault\nversion: 0.1.0\nlabel: Vault\ndepends: [core]\nroles:\n  - name: Keeper\n    permissions:\n      - {entity: Secret, read: true}\n  - name: Clerk\n    permissions:\n      - {entity: Case, read: true, write: true, create: true}\n";

const ENTITIES: &str = r#"
- name: Secret
  label: Secret
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
    - {name: code, type: data}
- name: Case
  label: Case
  kind: master
  title_field: subject
  fields:
    - {name: subject, type: data, required: true}
    - {name: status, type: data}
  actions:
    - {name: refresh, label: Refresh, permission: read, read_only: true}
    - {name: close, label: Close, permission: write}
"#;

const CODE: &str = "CODE-4242";
const KEY: &str = "fk_abcdefghij_0123456789abcdefghij0123456789abcdefghij";

/// The case's app: what its actions did, and how often.
#[derive(Default)]
struct Cases {
    refreshed: AtomicUsize,
    closed: AtomicUsize,
}

#[async_trait]
impl DocHook for Cases {
    async fn action(&self, _hc: &mut HookCtx<'_>, name: &str, _doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let n = match name {
            "refresh" => &self.refreshed,
            "close" => &self.closed,
            _ => return Ok(None),
        };
        n.fetch_add(1, Ordering::SeqCst);
        Ok(Some(json!({"done": name})))
    }
}

/// A request the stand-in holds: it says when it arrived and answers once let go.
type Gate = (usize, oneshot::Sender<()>, oneshot::Receiver<()>);

/// The model: answers each request with the next scripted reply, keeps what it was sent, can turn a
/// request away as an overloaded service does, and can hold one until the test lets it go.
#[derive(Clone, Default)]
struct Model {
    script: Arc<Mutex<VecDeque<(Vec<Value>, &'static str)>>>,
    seen: Arc<Mutex<Vec<Value>>>,
    failing: Arc<AtomicUsize>,
    gate: Arc<Mutex<Option<Gate>>>,
}

impl Model {
    fn reply(&self, blocks: Vec<Value>, stop: &'static str) {
        self.script.lock().unwrap().push_back((blocks, stop));
    }
    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    /// The conversation the last request sent.
    fn last(&self) -> String {
        self.seen.lock().unwrap().last().map(|b| b["messages"].to_string()).unwrap_or_default()
    }
    /// Holds request `at`: the receiver hears it arrive; sending on the sender lets it go.
    fn hold(&self, at: usize) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (arrived_tx, arrived_rx) = oneshot::channel();
        let (go_tx, go_rx) = oneshot::channel();
        *self.gate.lock().unwrap() = Some((at, arrived_tx, go_rx));
        (arrived_rx, go_tx)
    }
    fn hold_next(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        self.hold(self.count() + 1)
    }
}

fn sse(blocks: &[Value], stop: &str) -> String {
    let mut out = String::new();
    let mut ev = |v: Value| out.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
    ev(json!({"type": "message_start", "message": {"id": "m", "model": "claude-opus-5-5", "usage": {"input_tokens": 100}}}));
    for (i, b) in blocks.iter().enumerate() {
        if b["type"] == "text" {
            ev(json!({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}));
            // A text in pieces, as a stream may cut it.
            let parts: Vec<String> = match b["parts"].as_array() {
                Some(p) => p.iter().map(|x| x.as_str().unwrap().to_string()).collect(),
                None => vec![b["text"].as_str().unwrap().to_string()],
            };
            for part in parts {
                ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": part}}));
            }
        } else {
            ev(json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": b["id"], "name": b["name"], "input": {}}}));
            ev(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": b["input"].to_string()}}));
        }
        ev(json!({"type": "content_block_stop", "index": i}));
    }
    ev(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 50}}));
    ev(json!({"type": "message_stop"}));
    out
}

async fn messages(State(m): State<Model>, body: Bytes) -> Response {
    let n = {
        let mut seen = m.seen.lock().unwrap();
        seen.push(serde_json::from_slice(&body).unwrap());
        seen.len()
    };
    let gate = {
        let mut g = m.gate.lock().unwrap();
        if g.as_ref().is_some_and(|(at, ..)| *at == n) { g.take() } else { None }
    };
    if let Some((_, arrived, go)) = gate {
        let _ = arrived.send(());
        let _ = go.await;
    }
    if m.failing.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
        return Response::builder().status(529).header("retry-after", "0").body(Body::from("{}")).unwrap();
    }
    let (blocks, stop) = m.script.lock().unwrap().pop_front().unwrap_or((vec![json!({"type": "text", "text": "Done."})], "end_turn"));
    Response::builder().header("content-type", "text/event-stream").body(Body::from(sse(&blocks, stop))).unwrap()
}

async fn serve(m: Model) -> String {
    let app = Router::new().route("/v1/messages", post(messages)).with_state(m);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn call(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn text(t: &str) -> Value {
    json!({"type": "text", "text": t})
}

fn ask(t: &str) -> Input {
    Input::Message { text: t.into(), context: String::new() }
}

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

fn ended(events: &[Value]) -> (String, String) {
    let last = events.last().unwrap();
    (last["type"].as_str().unwrap_or_default().to_string(), last["message"].as_str().or(last["state"].as_str()).unwrap_or_default().to_string())
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

#[test]
fn the_assistant_does_only_what_still_holds() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ASSISTANT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_assistant_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let dir = std::env::temp_dir().join(format!("ferrum-boundaries-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("vault/entities")).unwrap();
    std::fs::write(dir.join("vault/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("vault/entities/vault.yaml"), ENTITIES).unwrap();
    let packages: Vec<Package> = vec![
        ferrum_meta::catalog::load_package_dir(&FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).unwrap(),
        ferrum_meta::catalog::load_package_dir(&dir.join("vault")).unwrap(),
    ];
    let _ = std::fs::remove_dir_all(&dir);
    let cases = Arc::new(Cases::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Case", cases.clone());
    let k = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["vault".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let pat = k.create_user(&admin, "pat@acme.test", "Pat", Some("Correct-Horse-9"), &["Keeper".into(), "Clerk".into()], false).await.unwrap();
    let as_pat = || {
        let k = k.clone();
        async move { k.load_ctx(tenant, pat, None, "it".into()).await.unwrap() }
    };
    let roles = |r: &[&str]| r.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let secret = k.insert(&admin, "Secret", obj(json!({"name": "Vault door", "code": CODE}))).await.unwrap()["id"].as_str().unwrap().to_string();
    let case = k.insert(&admin, "Case", obj(json!({"subject": "Leak"}))).await.unwrap()["id"].as_str().unwrap().to_string();

    let model = Model::default();
    let config = Config {
        protocol: Protocol::Anthropic,
        api_key: "test-key".into(),
        model: "claude-opus-5-5".into(),
        base_url: serve(model.clone()).await,
        effort: "low".into(),
        fallbacks: false,
        calls_per_hour: 1000,
        models: vec![],
        monthly_tokens: None,
    };
    let env = Env { kernel: k.clone(), apps: None, assistant: Assistant::new(Some(config.clone())) };

    // ───────────── FEO-059: a role taken away between two rounds of calls ─────────────
    let pat_ctx: Ctx = as_pat().await;
    let t1 = assistant::create_thread(&k, &pat_ctx).await.unwrap();
    model.reply(vec![call("a1", "search_records", json!({"entity": "Case"}))], "tool_use");
    model.reply(vec![call("a2", "get_record", json!({"entity": "Secret", "id": secret}))], "tool_use");
    // Held at the second request: the first call's results are in, and the code is asked for next.
    let (arrived, go) = model.hold(model.count() + 2);
    let mut rx = assistant::begin(&env, &pat_ctx, t1, ask("Find the case and the vault code"), "member").await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), arrived).await.expect("the held request came").unwrap();
    k.set_user_roles(&admin, pat, &roles(&["Clerk"])).await.unwrap();
    go.send(()).unwrap();
    let events = turn(&mut rx).await;
    let (kind, message) = ended(&events);
    assert_eq!(kind, "error", "{events:?}");
    assert!(message.contains("changed while the assistant was answering"), "{message}");
    assert_eq!(model.count(), 2, "no further call to the model");
    assert!(!model.seen.lock().unwrap().iter().any(|b| b["messages"].to_string().contains(CODE)), "the code never went");
    let stored = |thread| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let all: Vec<Value> = sqlx::query_scalar("select content from assistant_message where tenant_id = $1 and thread_id = $2 order by seq")
                .bind(tenant)
                .bind(thread)
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            Value::Array(all).to_string()
        }
    };
    let kept = stored(t1).await;
    assert!(!kept.contains(CODE) && kept.contains("Not run"), "the call after the change did not run: {kept}");

    // ───────────── FEO-059: the assistant turned off while a call is retried ─────────────
    k.set_user_roles(&admin, pat, &roles(&["Keeper", "Clerk"])).await.unwrap();
    let pat_ctx = as_pat().await;
    let t2 = assistant::create_thread(&k, &pat_ctx).await.unwrap();
    let before = model.count();
    model.failing.store(1, Ordering::SeqCst);
    let (arrived, go) = model.hold_next();
    let mut rx = assistant::begin(&env, &pat_ctx, t2, ask("Hello"), "member").await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), arrived).await.expect("the held request came").unwrap();
    k.update_tenant_settings(&admin, json!({"assistant": false}).as_object().unwrap()).await.unwrap();
    go.send(()).unwrap();
    let events = turn(&mut rx).await;
    let (kind, message) = ended(&events);
    assert_eq!(kind, "error", "{events:?}");
    assert!(message.contains("turned off"), "{message}");
    assert_eq!(model.count(), before + 1, "not tried again once turned off");
    k.update_tenant_settings(&admin, json!({"assistant": true}).as_object().unwrap()).await.unwrap();

    // ───────────── FEO-060: a later turn sends only what may still go ─────────────
    let t3 = assistant::create_thread(&k, &pat_ctx).await.unwrap();
    model.reply(vec![call("c1", "get_record", json!({"entity": "Secret", "id": secret}))], "tool_use");
    model.reply(vec![text(&format!("The code is {CODE}."))], "end_turn");
    let mut rx = assistant::begin(&env, &pat_ctx, t3, ask("What is the vault code?"), "member").await.unwrap();
    assert_eq!(ended(&turn(&mut rx).await).0, "done");
    assert!(model.last().contains(CODE), "within its turn, the result goes with its call");
    // The person may no longer read secrets: neither the result nor the answer drawn from it goes again.
    k.set_user_roles(&admin, pat, &roles(&["Clerk"])).await.unwrap();
    let pat_ctx = as_pat().await;
    model.reply(vec![text("Noted.")], "end_turn");
    let mut rx = assistant::begin(&env, &pat_ctx, t3, ask("Anything else?"), "member").await.unwrap();
    assert_eq!(ended(&turn(&mut rx).await).0, "done");
    let sent = model.last();
    assert!(!sent.contains(CODE), "{sent}");
    assert!(!sent.contains("tool_result") && !sent.contains("get_record"), "earlier calls and results are never sent again: {sent}");
    assert!(sent.contains("What is the vault code?"), "the person's own words stay");
    assert!(sent.contains("An earlier answer is left out"), "{sent}");
    // Given back, the earlier answer goes again (its result still does not).
    k.set_user_roles(&admin, pat, &roles(&["Keeper", "Clerk"])).await.unwrap();
    let pat_ctx = as_pat().await;
    model.reply(vec![text("Ok.")], "end_turn");
    let mut rx = assistant::begin(&env, &pat_ctx, t3, ask("And now?"), "member").await.unwrap();
    assert_eq!(ended(&turn(&mut rx).await).0, "done");
    let sent = model.last();
    assert!(sent.contains(&format!("The code is {CODE}.")) && !sent.contains("tool_result"), "{sent}");
    // Another destination: what went to the first is not sent to it.
    let elsewhere = Model::default();
    let env2 = Env { kernel: k.clone(), apps: None, assistant: Assistant::new(Some(Config { base_url: serve(elsewhere.clone()).await, ..config.clone() })) };
    elsewhere.reply(vec![text("Hi.")], "end_turn");
    let mut rx = assistant::begin(&env2, &pat_ctx, t3, ask("Who are you?"), "member").await.unwrap();
    assert_eq!(ended(&turn(&mut rx).await).0, "done");
    let sent = elsewhere.last();
    assert!(!sent.contains(CODE) && sent.contains("An earlier answer is left out"), "{sent}");

    // ───────────── FEO-061: what runs unasked is this server's word; inputs checked at every depth ─────────────
    let t4 = assistant::create_thread(&k, &pat_ctx).await.unwrap();
    model.reply(
        vec![
            call("d1", "run_action", json!({"entity": "Case", "id": case, "action": "refresh"})),
            call("d2", "search_records", json!({"entity": "Case", "limit": null})),
            call("d3", "run_action", json!({"entity": "Case", "id": case, "action": "close"})),
            call("d4", "add_comment", json!({"entity": "Case", "id": case, "body": "Closed by the assistant"})),
        ],
        "tool_use",
    );
    let mut rx = assistant::begin(&env, &pat_ctx, t4, ask("Refresh and close the case"), "member").await.unwrap();
    let events = turn(&mut rx).await;
    assert_eq!(ended(&events), ("done".into(), "waiting".into()), "{events:?}");
    let confirm = events.iter().find(|e| e["type"] == "confirm").expect("asked");
    let asked: Vec<&str> = confirm["pending"]["calls"].as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    assert_eq!(asked, ["d3", "d4"], "only the changes wait for the person");
    assert_eq!((cases.refreshed.load(Ordering::SeqCst), cases.closed.load(Ordering::SeqCst)), (1, 0), "the read-only action ran at once");
    model.reply(vec![text("Left open.")], "end_turn");
    let mut rx = assistant::begin(&env, &pat_ctx, t4, Input::Decision { allow: false }, "member").await.unwrap();
    assert_eq!(ended(&turn(&mut rx).await).0, "done");
    let sent = model.last();
    assert!(sent.contains("Invalid input") && sent.contains("'limit' must be integer"), "a null where a number goes is refused: {sent}");
    assert_eq!(cases.closed.load(Ordering::SeqCst), 0);

    // ───────────── FEO-062: turns at once share the allowance; keys are never shown ─────────────
    let used: i64 = assistant::usage(&k, &admin).await.unwrap()["month"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["input_tokens"].as_i64().unwrap_or(0) + r["output_tokens"].as_i64().unwrap_or(0))
        .sum();
    // Room for one call's input (a request's size, as the server reckons it) and a short answer: the
    // first call takes it all, so a second at the same time has none.
    let request = model.seen.lock().unwrap().last().unwrap().to_string().len() as i64 / 3;
    k.update_tenant_settings(&admin, json!({"assistant_monthly_tokens": used + request + 20_000}).as_object().unwrap()).await.unwrap();
    let (t5, t6) = (assistant::create_thread(&k, &pat_ctx).await.unwrap(), assistant::create_thread(&k, &pat_ctx).await.unwrap());
    model.reply(vec![text("First.")], "end_turn");
    let (arrived, go) = model.hold_next();
    let mut rx5 = assistant::begin(&env, &pat_ctx, t5, ask("One"), "member").await.unwrap();
    if tokio::time::timeout(Duration::from_secs(30), arrived).await.is_err() {
        panic!("the first call was never made: {:?}", turn(&mut rx5).await);
    }
    let asked_for = model.seen.lock().unwrap().last().unwrap()["max_tokens"].as_i64().unwrap();
    assert!(asked_for < 64_000, "the answer is capped at what is left: {asked_for}");
    // While that call is in flight, another turn finds nothing left for it.
    let mut rx6 = assistant::begin(&env, &pat_ctx, t6, ask("Two"), "member").await.unwrap();
    let (kind, message) = ended(&turn(&mut rx6).await);
    assert_eq!(kind, "error");
    assert!(message.contains("allowance"), "{message}");
    go.send(()).unwrap();
    assert_eq!(ended(&turn(&mut rx5).await).0, "done");
    // Its use recorded, what it held is let go: there is room again.
    model.reply(vec![text("Third.")], "end_turn");
    let mut rx7 = assistant::begin(&env, &pat_ctx, t6, ask("Three"), "member").await.unwrap();
    assert_eq!(ended(&turn(&mut rx7).await).0, "done");
    k.update_tenant_settings(&admin, json!({"assistant_monthly_tokens": null}).as_object().unwrap()).await.unwrap();

    let t8 = assistant::create_thread(&k, &pat_ctx).await.unwrap();
    model.reply(vec![json!({"type": "text", "parts": ["Your key is fk_abcdefghij_", "0123456789abcdefghij0123456789abcdefghij: keep it safe."]})], "end_turn");
    let mut rx = assistant::begin(&env, &pat_ctx, t8, ask("Show me my key"), "member").await.unwrap();
    let events = turn(&mut rx).await;
    let shown: String = events.iter().filter(|e| e["type"] == "delta").filter_map(|e| e["text"].as_str()).collect();
    assert!(!shown.contains(KEY) && shown.contains("[withheld]"), "{shown}");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let kept: Vec<Value> = sqlx::query_scalar("select content from assistant_message where tenant_id = $1 and thread_id = $2 and role = 'assistant'")
        .bind(tenant)
        .bind(t8)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(!Value::Array(kept.clone()).to_string().contains(KEY), "nor kept: {kept:?}");
}
