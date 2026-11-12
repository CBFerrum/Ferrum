//! The approval and transition routes (BD-19) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset first),
//! through the functions the routes call and the error answers the API gives, as `calendars_api_it` does: taking a
//! transition with what it asks for, the record's approval, deciding, recalling, the caller's approvals and Home's work
//! list, and a person's away periods. Each with its success, a refusal, what is not allowed and what is not there.

mod common;

use axum::response::IntoResponse;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, PermissionDef};
use ferrum_server::controllers::docs::{DecisionBody, decided, take_transition};
use ferrum_server::controllers::work::{away_cleared, away_of, work_of};
use ferrum_server::error::{ApiError, ApiResult};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

/// What the API answers: its status and JSON.
async fn answer<T: IntoResponse>(r: ApiResult<T>) -> (u16, Value) {
    let r = match r {
        Ok(v) => v.into_response(),
        Err(e) => e.into_response(),
    };
    let status = r.status().as_u16();
    (status, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap_or(Value::Null))
}

fn body(outcome: &str, comment: Option<&str>) -> DecisionBody {
    serde_json::from_value(json!({"outcome": outcome, "comment": comment})).unwrap()
}

#[test]
fn approval_routes_answer() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let root = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap().user;
    let admin: Ctx = k.load_ctx(tenant, root, None, "it".into()).await.unwrap();
    let ops: Vec<NewCustomization> = serde_json::from_value(json!([
        {"entity": "XOrder", "op": {"op": "define_entity", "entity": {"name": "XOrder", "label": "Order", "kind": "master", "title_field": "title",
            "fields": [{"name": "title", "type": "data"}, {"name": "manager", "type": "data"}]}}},
        {"entity": "XOrder", "op": {"op": "set_workflow", "workflow": {"name": "orders", "entity": "XOrder", "initial_state": "Draft",
            "states": [{"name": "Draft"}, {"name": "Approved"}],
            "transitions": [{"action": "Submit", "from": "Draft", "to": "Approved",
                "params": [{"name": "reason", "label": "Reason", "required": true}],
                "approval": {"steps": [{"label": "Manager", "approvers": {"field": "manager"}, "remind_after": "PT1H"}]}}]}}}
    ]))
    .unwrap();
    k.batch_customizations(&admin, ops, false).await.map_err(|(_, e)| e).expect("the workflow");
    let admin: Ctx = k.load_ctx(tenant, root, None, "it".into()).await.unwrap();
    k.set_role_permission(&admin, "Staff", &PermissionDef { entity: "XOrder".into(), read: true, write: true, create: true, ..Default::default() })
        .await
        .unwrap();
    let user = |email: &'static str, name: &'static str, roles: &'static [&'static str]| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let u = k.create_user(&admin, email, name, None, &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false).await.unwrap();
            (u, k.load_ctx(tenant, u, None, "it".into()).await.unwrap())
        }
    };
    let (_, annc) = user("ann@acme.test", "Ann Asker", &["Staff"]).await;
    let (max, maxc) = user("max@acme.test", "Max Manager", &["Staff"]).await;
    let (sub, subc) = user("sub@acme.test", "Sue Standin", &["Staff"]).await;
    let (_, nobody) = user("nobody@acme.test", "No Body", &[]).await;
    let order = |title: &'static str| {
        let (k, annc) = (k.clone(), annc.clone());
        async move {
            let d = k.insert(&annc, "XOrder", json!({"title": title, "manager": max.to_string()}).as_object().cloned().unwrap()).await.unwrap();
            d["id"].as_str().unwrap().parse::<Uuid>().unwrap()
        }
    };
    let take = |c: Ctx, id: Uuid, action: &'static str, b: &'static str| {
        let k = k.clone();
        async move { answer(take_transition(&k, &c, "XOrder", id, action, None, b.as_bytes()).await).await }
    };
    let o1 = order("One").await;

    // ───────────── POST …/workflow/{action} ─────────────
    let (status, b) = take(annc.clone(), o1, "Submit", "").await;
    assert_eq!((status, b["error"].as_str()), (422, Some("validation_failed")), "a required parameter missing: {b}");
    let (status, b) = take(annc.clone(), o1, "Submit", "{not json").await;
    assert_eq!(status, 422, "{b}");
    let (status, _) = take(annc.clone(), Uuid::now_v7(), "Submit", r#"{"reason": "stock"}"#).await;
    assert_eq!(status, 404, "no such record");
    let (status, _) = take(nobody.clone(), o1, "Submit", r#"{"reason": "stock"}"#).await;
    assert_eq!(status, 403, "no right to the entity");
    let (status, b) = take(annc.clone(), o1, "Fly", "{}").await;
    assert_eq!((status, b["error"].as_str()), (422, Some("invalid")), "not allowed in this state: {b}");
    let (status, b) = take(annc.clone(), o1, "Submit", r#"{"reason": "stock"}"#).await;
    assert_eq!((status, b["workflow_state"].as_str(), b["_approval"]["status"].as_str()), (200, Some("Draft"), Some("pending")), "{b}");
    let (status, b) = take(annc.clone(), o1, "Submit", r#"{"reason": "again"}"#).await;
    assert_eq!((status, b["error"].as_str()), (409, Some("conflict")), "already awaiting: {b}");

    // ───────────── GET …/approval ─────────────
    let (status, b) = answer(k.approval_of(&maxc, "XOrder", o1).await.map(axum::Json).map_err(ApiError)).await;
    assert_eq!((status, b["status"].as_str(), b["steps"][0]["label"].as_str()), (200, Some("pending"), Some("Manager")), "{b}");
    assert_eq!((b["steps"][0]["reminded_at"].clone(), b["steps"][0]["escalated_at"].clone()), (Value::Null, Value::Null), "not yet");
    assert_eq!(answer(k.approval_of(&nobody, "XOrder", o1).await.map(axum::Json).map_err(ApiError)).await.0, 404, "neither reader nor involved");
    assert_eq!(answer(k.approval_of(&annc, "XOrder", Uuid::now_v7()).await.map(axum::Json).map_err(ApiError)).await.0, 404);
    let o0 = order("Never asked").await;
    assert_eq!(answer(k.approval_of(&annc, "XOrder", o0).await.map(axum::Json).map_err(ApiError)).await, (200, Value::Null), "none yet");
    // The minute loop's reminder is said when it was made, not only when it was due.
    assert_eq!(k.sweep_approvals_at(chrono::Utc::now() + chrono::Duration::minutes(61)).await.unwrap(), 1);
    let b = k.approval_of(&maxc, "XOrder", o1).await.unwrap();
    assert!(b["steps"][0]["reminded_at"].is_string() && b["steps"][0]["escalated_at"].is_null(), "{b}");

    // ───────────── GET /api/approvals and Home's work list ─────────────
    let mine = k.my_approval_requests(&maxc, 50).await.unwrap();
    let item = &mine["to_decide"][0];
    assert_eq!((item["record"].as_str(), item["requester_name"].as_str()), (Some(o1.to_string().as_str()), Some("Ann Asker")), "{mine}");
    assert_eq!((item["step"]["index"].as_u64(), item["step"]["label"].as_str()), (Some(0), Some("Manager")), "{item}");
    assert!(item["step"]["opened_at"].is_string(), "{item}");
    let theirs = k.my_approval_requests(&annc, 50).await.unwrap();
    assert_eq!((theirs["to_decide"].as_array().map(Vec::len), theirs["awaiting_others"][0]["step"]["label"].as_str()), (Some(0), Some("Manager")));
    assert_eq!(k.my_approval_requests(&nobody, 50).await.unwrap(), json!({"to_decide": [], "awaiting_others": []}));
    let work = work_of(&k, &maxc).await;
    let waiting = &work["groups"][1]["items"];
    assert!(waiting.as_array().unwrap().iter().any(|i| i["kind"] == "approval_request" && i["url"] == format!("/open/XOrder/{o1}")), "{work}");

    // ───────────── POST …/approval/decide ─────────────
    let (status, b) = answer(decided(&k, &maxc, "XOrder", o1, body("maybe", None)).await.map(axum::Json)).await;
    assert_eq!((status, b["fields"][0]["field"].as_str()), (422, Some("outcome")), "{b}");
    let (status, b) = answer(decided(&k, &maxc, "XOrder", o1, body("reject", None)).await.map(axum::Json)).await;
    assert_eq!((status, b["fields"][0]["field"].as_str()), (422, Some("comment")), "a reason to reject: {b}");
    assert_eq!(answer(decided(&k, &annc, "XOrder", o1, body("approve", None)).await.map(axum::Json)).await.0, 403, "not theirs");
    assert_eq!(answer(decided(&k, &maxc, "XOrder", Uuid::now_v7(), body("approve", None)).await.map(axum::Json)).await.0, 404);
    assert_eq!(answer(decided(&k, &maxc, "Nothing", o1, body("approve", None)).await.map(axum::Json)).await.0, 404, "no such entity");
    let (status, b) = answer(decided(&k, &maxc, "XOrder", o1, body("approve", Some("fine"))).await.map(axum::Json)).await;
    assert_eq!((status, b["status"].as_str()), (200, Some("approved")), "{b}");
    assert_eq!(answer(decided(&k, &maxc, "XOrder", o1, body("approve", None)).await.map(axum::Json)).await.0, 404, "nothing awaits now");

    // ───────────── POST …/approval/recall ─────────────
    let o2 = order("Two").await;
    take(annc.clone(), o2, "Submit", r#"{"reason": "stock"}"#).await;
    assert_eq!(answer(k.recall_approval(&maxc, "XOrder", o2).await.map(axum::Json).map_err(ApiError)).await.0, 403, "whoever asked recalls");
    let (status, b) = answer(k.recall_approval(&annc, "XOrder", o2).await.map(axum::Json).map_err(ApiError)).await;
    assert_eq!((status, b["status"].as_str()), (200, Some("recalled")));
    assert_eq!(answer(k.recall_approval(&annc, "XOrder", o2).await.map(axum::Json).map_err(ApiError)).await.0, 404, "nothing pending");

    // ───────────── GET and DELETE /api/away ─────────────
    let today = chrono::Utc::now().date_naive();
    k.set_away(&maxc, max, today, today + chrono::Duration::days(2), sub, "leave").await.unwrap();
    k.set_away(&maxc, max, today + chrono::Duration::days(10), today + chrono::Duration::days(12), sub, "travel").await.unwrap();
    let (status, b) = answer(away_of(&k, &maxc, None).await.map(axum::Json)).await;
    assert_eq!(status, 200);
    assert_eq!(
        (b[0]["current"].as_bool(), b[0]["substitute"].as_str(), b[0]["substitute_name"].as_str()),
        (Some(true), Some(sub.to_string().as_str()), Some("Sue Standin")),
        "{b}"
    );
    assert_eq!((b[1]["current"].as_bool(), b[1]["reason"].as_str()), (Some(false), Some("travel")), "and the one to come: {b}");
    assert_eq!(answer(away_of(&k, &subc, Some(max)).await.map(axum::Json)).await.0, 403, "someone else's, not an administrator");
    assert_eq!(answer(away_cleared(&k, &subc, Some(max), None).await.map(axum::Json)).await.0, 403);
    assert_eq!(answer(away_of(&k, &admin, Some(max)).await.map(axum::Json)).await.1.as_array().map(Vec::len), Some(2), "an administrator reads anyone's");
    let (status, b) = answer(away_cleared(&k, &maxc, None, Some(today + chrono::Duration::days(10))).await.map(axum::Json)).await;
    assert_eq!((status, b), (200, json!({"cleared": 1})));
    assert_eq!(answer(away_cleared(&k, &admin, Some(max), None).await.map(axum::Json)).await, (200, json!({"cleared": 1})));
    assert_eq!(answer(away_of(&k, &maxc, None).await.map(axum::Json)).await, (200, json!([])));
}
