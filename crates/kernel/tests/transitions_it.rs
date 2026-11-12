//! Workflow transitions that do things (docs/builder/decisions.md BD-19, part one), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): a transition asks for a reason, the server refuses it
//! without one, its effects run as the system on the taker's behalf, its rules run only when it is taken, and an
//! automation answers its `transitioned` event, reading which transition it was.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-transitions-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

#[track_caller]
fn refused<T: std::fmt::Debug>(r: Result<T, Error>, says: &str) {
    let e = r.expect_err(says);
    assert!(message(&e).contains(says), "{says}: {}", message(&e));
}

async fn drain(k: &Kernel) {
    for _ in 0..60 {
        if k.dispatch_outbox_once().await.unwrap() == 0 {
            return;
        }
    }
    panic!("the outbox never emptied");
}

/// Draft → Review (asks a reason; notes it and logs it), Review → Approved, Review → Draft.
fn workflow(send_effects: Value) -> Value {
    json!({"name": "claims", "entity": "XClaim", "initial_state": "Draft",
        "states": [{"name": "Draft"}, {"name": "Review"}, {"name": "Approved"}],
        "transitions": [
            {"action": "Send", "from": "Draft", "to": "Review",
             "params": [{"name": "reason", "label": "Reason", "type": "text", "required": true}, {"name": "urgent", "label": "Urgent", "type": "data"}],
             "effects": send_effects},
            {"action": "Approve", "from": "Review", "to": "Approved"},
            {"action": "Return", "from": "Review", "to": "Draft"}]})
}

#[test]
fn transitions_that_do_things() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "firm".into(),
            name: "Firm".into(),
            admin_email: "admin@firm.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .expect("workspace");
    let user = k.login("firm", "admin@firm.test", "Correct-Horse-9").await.unwrap().user;
    let batch = |items: Vec<NewCustomization>| {
        let k = k.clone();
        async move {
            let admin = k.load_ctx(tenant, user, None, "it".into()).await.unwrap();
            k.batch_customizations(&admin, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
        }
    };
    batch(vec![
        op(
            "XClaim",
            json!({"op": "define_entity", "entity": {"name": "XClaim", "label": "Claim", "kind": "master", "title_field": "title",
            "fields": [{"name": "title", "type": "data"}, {"name": "amount", "type": "int"}, {"name": "why", "type": "text"}],
            "rules": [{"name": "approve_amount", "check": "transition.to != 'Approved' || doc.amount > 0", "message": "an amount is needed to approve",
                       "on": "transition", "field": "amount"}]}}),
        ),
        op(
            "XLog",
            json!({"op": "define_entity", "entity": {"name": "XLog", "label": "Log", "kind": "master", "fields": [
            {"name": "note", "type": "data"}, {"name": "claim", "type": "link", "to": "XClaim"}]}}),
        ),
    ])
    .await
    .expect("entities");

    // ───────────── refused when saved: an effect writing the platform's column ─────────────
    let e = batch(vec![op("XClaim", json!({"op": "set_workflow", "workflow": workflow(json!([{"type": "set_fields", "values": {"owner": "'x'"}}]))}))])
        .await
        .unwrap_err();
    assert!(e.contains("transition Send from Draft, effect 1"), "{e}");
    batch(vec![op(
        "XClaim",
        json!({"op": "set_workflow", "workflow": workflow(json!([
            {"type": "set_fields", "values": {"why": "params.reason"}},
            {"type": "create_document", "entity": "XLog", "values": {"note": "'sent: ' + params.reason", "claim": "doc.id"}, "when": "params.urgent == 'yes'"}]))}),
    )])
    .await
    .expect("workflow with effects");
    let admin = k.load_ctx(tenant, user, None, "it".into()).await.unwrap();
    let def = k.custom_definition(&admin, "XClaim").expect("definition");
    let send = &def["workflow"]["transitions"][0];
    assert_eq!(send["params"][0]["name"], json!("reason"), "the definition carries what it asks for: {send}");
    assert_eq!(send["effects"].as_array().map(Vec::len), Some(2), "and what it does");

    let claim = id_of(&k.insert(&admin, "XClaim", obj(json!({"title": "Taxi"}))).await.unwrap());

    // ───────────── what it asks for: the server refuses, not only the form ─────────────
    refused(k.transition(&admin, "XClaim", claim, "Send", None).await, "reason: is needed to Send");
    refused(k.transition_with(&admin, "XClaim", claim, "Send", None, json!({"reason": "  "})).await, "reason: is needed to Send");
    refused(k.transition_with(&admin, "XClaim", claim, "Send", None, json!({"reason": "r", "colour": "red"})).await, "colour: 'Send' does not ask for this");
    refused(k.transition_with(&admin, "XClaim", claim, "Send", None, json!("r")).await, "takes its parameters as an object");

    // ───────────── its effects run, reading the reason ─────────────
    let sent = k.transition_with(&admin, "XClaim", claim, "Send", None, json!({"reason": "client visit", "urgent": "yes"})).await.unwrap();
    assert_eq!(sent["workflow_state"], json!("Review"));
    assert_eq!(sent["why"], json!("client visit"), "an effect wrote what it was given");
    let logs = k.list(&admin, "XLog", &Default::default()).await.unwrap().rows;
    assert_eq!(logs.len(), 1, "the effect whose `when` held made its record");
    assert_eq!(logs[0]["note"], json!("sent: client visit"));
    // The history keeps what it was given.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let kept: Vec<Value> = sqlx::query_scalar("select diff from audit_event where record_id = $1 and action = 'workflow:Send' order by id")
        .bind(claim)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(kept.iter().any(|d| d["params"]["reason"] == json!("client visit")), "{kept:?}");

    k.transition(&admin, "XClaim", claim, "Return", None).await.unwrap();

    // ───────────── rules on a transition run only when it is taken ─────────────
    k.update(&admin, "XClaim", claim, obj(json!({"amount": 0})), None).await.expect("a save does not run a transition's rule");
    k.transition_with(&admin, "XClaim", claim, "Send", None, json!({"reason": "again"})).await.unwrap();
    refused(k.transition(&admin, "XClaim", claim, "Approve", None).await, "amount: an amount is needed to approve");
    assert_eq!(k.get(&admin, "XClaim", claim, false).await.unwrap()["workflow_state"], json!("Review"), "a refused transition stays");
    k.update(&admin, "XClaim", claim, obj(json!({"amount": 12})), None).await.unwrap();

    // ───────────── automations take `transitioned`, reading which one ─────────────
    automation::save(
        &k,
        &admin,
        &obj(json!({"name": "on approve", "entity": "XClaim", "event": "transitioned",
            "condition": "transition.action == 'Approve' && old.workflow_state == 'Review' && doc.workflow_state == 'Approved'",
            "actions": [{"type": "create_document", "entity": "XLog", "values": {"note": "'approved by ' + string(transition.by)", "claim": "doc.id"}}]})),
    )
    .await
    .expect("an automation on transitioned");
    drain(&k).await;
    k.transition(&admin, "XClaim", claim, "Approve", None).await.unwrap();
    drain(&k).await;
    let notes: Vec<Value> = k.list(&admin, "XLog", &Default::default()).await.unwrap().rows.into_iter().map(|r| r["note"].clone()).collect();
    assert!(notes.contains(&json!(format!("approved by {user}"))), "{notes:?}");
    assert_eq!(notes.len(), 2, "only the Approve transition answered: {notes:?}");
}
