//! Collection operations (A11 2.4, API-2) against a real PostgreSQL (`FERRUM_TESTKIT_TEST_DATABASE_URL`, emptied
//! first): an action bound to no record runs through the gate with its rights, its receipt and, when it only reads, a
//! read-only transaction; a rule naming it takes no `where`.

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::hooks::{DocHook, HookCtx};
use ferrum_kernel::{Doc, Error, HookRegistry, Result};
use ferrum_meta::{MetaError, Package, PermissionDef};
use ferrum_testkit::{App, Kit, Workspace, database, obj, run};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::OnceCell;

const PACKAGE: &str = r#"package: colltest
version: 0.1.0
depends: [core]
roles:
  - name: Tally Reader
    permissions:
      - {entity: Tally, read: true}
  - name: Tally Bumper
    permissions:
      - {entity: Tally, read: true, actions: [bump]}
"#;

const ENTITY: &str = r#"name: Tally
label: Tally
kind: master
fields:
  - {name: note, label: Note, type: data}
actions:
  - name: bump
    label: Bump
    collection: true
    permission: create
    params: [{name: note, label: Note, type: data}]
    contract: {effect: transactional_action, completion: A tally is made}
  - name: count
    label: Count
    collection: true
    read_only: true
    permission: read
    contract: {effect: query, completion: How many tallies there are}
  - name: sneak
    label: Sneak
    collection: true
    read_only: true
    permission: read
    contract: {effect: query, completion: Tries to write}
"#;

fn packages() -> std::result::Result<Vec<Package>, MetaError> {
    let files = BTreeMap::from([("package.yaml".to_string(), PACKAGE.to_string()), ("entities/tally.yaml".to_string(), ENTITY.to_string())]);
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

/// The app's code behind its collection actions.
struct Tallies;

#[async_trait]
impl DocHook for Tallies {
    async fn collection(&self, hc: &mut HookCtx<'_>, name: &str, params: &Value) -> Result<Option<Value>> {
        let note = params.get("note").cloned().unwrap_or(Value::Null);
        Ok(Some(match name {
            "bump" => {
                // The app writes for whoever may run the action, as itself: running it is the right (a rule naming it).
                let made = hc.kernel.insert_tx(&ferrum_kernel::Ctx::system_for(hc.ctx), &mut *hc.tx, "Tally", obj(json!({"note": note}))).await?;
                json!({"entity": "Tally", "id": made["id"], "params": params})
            }
            "count" | "sneak" => {
                if name == "sneak" {
                    hc.kernel.insert_tx(hc.ctx, &mut *hc.tx, "Tally", Doc::new()).await?;
                }
                let n: i64 = sqlx::query_scalar("select count(*) from e_tally where tenant_id = $1").bind(hc.ctx.tenant).fetch_one(&mut **hc.tx).await?;
                json!({"count": n})
            }
            _ => return Ok(None),
        }))
    }
}

fn register(h: &mut HookRegistry) {
    h.register("Tally", Arc::new(Tallies));
}

const APP: App = App { id: "colltest", packages, register, reports: Vec::new };

static KIT: OnceCell<Kit> = OnceCell::const_new();

async fn workspace(slug: &str) -> (&'static Kit, Workspace) {
    let kit = KIT.get_or_init(|| async { Kit::boot(&database("FERRUM_TESTKIT_TEST_DATABASE_URL"), &[APP]).await }).await;
    (kit, kit.workspace(slug, &["colltest"], Value::Null).await)
}

async fn tallies(ws: &Workspace) -> i64 {
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from e_tally where tenant_id = $1").bind(ws.tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[test]
fn a_collection_action_runs_through_the_gate_without_a_record() {
    run(async {
        let (kit, ws) = workspace("gate").await;
        let cap = kit.k.catalog(&ws.admin).into_iter().find(|c| c.id == "colltest.tally.bump").expect("in the catalog");
        let props = cap.input["properties"].as_object().unwrap();
        assert!(props.contains_key("params") && !props.contains_key("id"), "its input is {{params}}, no id: {}", cap.input);
        let done = kit.k.invoke(&ws.admin, Invocation::new("colltest.tally.bump", json!({"params": {"note": "one"}})).key("bump-gate-1")).await.unwrap();
        assert_eq!(done.result["params"], json!({"note": "one"}), "the handler got its declared params");
        assert_eq!(tallies(&ws).await, 1);
        // Undeclared parameters never reach the handler.
        let refused = kit.k.invoke(&ws.admin, Invocation::new("colltest.tally.bump", json!({"params": {"note": "x", "price": 1}})).key("bump-gate-2")).await;
        assert!(refused.is_err(), "{refused:?}");
        // A person without the right is refused; a rule naming the action is the right.
        let reader = ws.user("reader@gate.test", &["Tally Reader"]).await;
        let refused = kit.k.invoke(&reader, Invocation::new("colltest.tally.bump", json!({"params": {}})).key("bump-gate-3")).await;
        assert!(matches!(&refused, Err(Error::Forbidden(_)) | Err(Error::Refused { .. })), "{refused:?}");
        let bumper = ws.user("bumper@gate.test", &["Tally Bumper"]).await;
        kit.k.invoke(&bumper, Invocation::new("colltest.tally.bump", json!({"params": {}})).key("bump-gate-4")).await.unwrap();
        assert_eq!(tallies(&ws).await, 2);
        assert_eq!(kit.k.invoke(&reader, Invocation::new("colltest.tally.count", json!({}))).await.unwrap().result["count"], json!(2));
    });
}

#[test]
fn its_receipt_answers_a_repeat_once() {
    run(async {
        let (kit, ws) = workspace("receipt").await;
        let inv = || Invocation::new("colltest.tally.bump", json!({"params": {"note": "once"}})).key("bump-receipt-1");
        let first = kit.k.invoke(&ws.admin, inv()).await.unwrap();
        let again = kit.k.invoke(&ws.admin, inv()).await.unwrap();
        assert!(!first.repeated && again.repeated);
        assert_eq!(again.result["id"], first.result["id"], "the same answer");
        assert_eq!(tallies(&ws).await, 1, "done once");
    });
}

#[test]
fn a_collection_query_runs_read_only() {
    run(async {
        let (kit, ws) = workspace("readonly").await;
        let refused = kit.k.invoke(&ws.admin, Invocation::new("colltest.tally.sneak", json!({}))).await;
        // The database refuses the write (read-only transaction, said as "takes no changes").
        assert!(matches!(&refused, Err(Error::Unavailable(m)) if m.contains("takes no changes")), "a query cannot write: {refused:?}");
        let refused = kit.k.run_collection(&ws.admin, "Tally", "sneak", json!({}), None).await;
        assert!(matches!(&refused, Err(Error::Unavailable(m)) if m.contains("takes no changes")), "on the screen's route either: {refused:?}");
        assert_eq!(tallies(&ws).await, 0);
    });
}

#[test]
fn a_rule_naming_a_collection_action_with_where_is_refused() {
    run(async {
        let (kit, ws) = workspace("rules").await;
        let rule = |condition: BTreeMap<String, Value>| PermissionDef {
            entity: "Tally".into(),
            read: true,
            actions: vec!["bump".into()],
            condition,
            ..Default::default()
        };
        let refused = kit.k.set_role_permission(&ws.admin, "Clerk", &rule(BTreeMap::from([("note".to_string(), json!("x"))]))).await;
        assert!(matches!(&refused, Err(Error::Validation(f)) if f.iter().any(|f| f.field == "actions")), "{refused:?}");
        kit.k.set_role_permission(&ws.admin, "Clerk", &rule(BTreeMap::new())).await.unwrap();
    });
}

#[test]
fn the_screen_route_takes_an_idempotency_key() {
    run(async {
        let (kit, ws) = workspace("screen").await;
        let (first, again) = kit.k.run_collection(&ws.admin, "Tally", "bump", json!({"note": "s"}), Some("screen-key-1")).await.unwrap();
        assert!(!again);
        let (second, again) = kit.k.run_collection(&ws.admin, "Tally", "bump", json!({"note": "s"}), Some("screen-key-1")).await.unwrap();
        assert!(again, "the same key answers again");
        assert_eq!(second["id"], first["id"]);
        assert_eq!(tallies(&ws).await, 1);
        kit.k.run_collection(&ws.admin, "Tally", "bump", json!({}), None).await.unwrap();
        assert_eq!(tallies(&ws).await, 2, "without a key it runs each time");
    });
}
