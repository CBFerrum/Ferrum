//! Approvals on a workflow transition (docs/builder/decisions.md BD-19, part two), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). An order's Submit needs its manager (named on the order)
//! and, above 10,000, two of Finance who neither prepared it nor decided before; a change of amount starts again, a
//! rejection sends it back, an absent manager's stand-in decides for them, and Finance is reminded, then Directors may
//! decide too.

use chrono::{DateTime, Duration, Utc};
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-approvals-it-{}", std::process::id()));
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

fn policy() -> Value {
    json!({"steps": [
        {"label": "Manager", "approvers": {"field": "manager"}, "when": "doc.amount > 0"},
        {"label": "Finance", "approvers": {"role": "Finance"}, "need": 2, "when": "doc.amount > 10000",
         "not_earlier_deciders": true, "not_who_took": ["Prepare"],
         "remind_after": "PT1H", "escalate_after": "PT2H", "escalate_to": {"role": "Director"}}],
        "reset_on": ["amount"], "on_reject": "Rejected"})
}

fn workflow(policy: Value) -> Value {
    json!({"name": "orders", "entity": "XOrder", "initial_state": "Draft",
        "states": [{"name": "Draft"}, {"name": "Ready"}, {"name": "Approved"}, {"name": "Rejected"}],
        "transitions": [
            {"action": "Prepare", "from": "Draft", "to": "Ready"},
            {"action": "Submit", "from": "Ready", "to": "Approved", "approval": policy,
             "params": [{"name": "reason", "label": "Reason", "required": true}],
             "effects": [{"type": "set_fields", "values": {"note": "'approved: ' + params.reason"}}]},
            {"action": "Retry", "from": "Rejected", "to": "Ready"},
            {"action": "Reopen", "from": "Approved", "to": "Draft"}]})
}

#[test]
fn approvals_on_a_transition() {
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
    let root = k.login("firm", "admin@firm.test", "Correct-Horse-9").await.unwrap().user;
    let as_ = |u: Uuid| {
        let k = k.clone();
        async move { k.load_ctx(tenant, u, None, "it".into()).await.unwrap() }
    };
    let batch = |items: Vec<NewCustomization>| {
        let k = k.clone();
        async move {
            let admin = k.load_ctx(tenant, root, None, "it".into()).await.unwrap();
            k.batch_customizations(&admin, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
        }
    };
    batch(vec![op(
        "XOrder",
        json!({"op": "define_entity", "entity": {"name": "XOrder", "label": "Order", "kind": "master", "title_field": "title", "fields": [
            {"name": "title", "type": "data"}, {"name": "amount", "type": "int"}, {"name": "manager", "type": "data"}, {"name": "note", "type": "data"}]}}),
    )])
    .await
    .expect("entity");
    let admin = as_(root).await;
    let staff = PermissionDef { entity: "XOrder".into(), read: true, write: true, create: true, ..Default::default() };
    for role in ["Staff", "Finance", "Director"] {
        k.set_role_permission(&admin, role, &staff).await.unwrap();
    }
    let person = |email: &'static str, roles: &'static [&'static str]| {
        let k = k.clone();
        let admin = admin.clone();
        async move { k.create_user(&admin, email, email, Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false).await.unwrap() }
    };
    let ann = person("ann@firm.test", &["Staff"]).await;
    let max = person("max@firm.test", &["Staff"]).await;
    let fin1 = person("fin1@firm.test", &["Staff", "Finance"]).await;
    let fin2 = person("fin2@firm.test", &["Staff", "Finance"]).await;
    let fin3 = person("fin3@firm.test", &["Staff", "Finance"]).await;
    let dir = person("dir@firm.test", &["Staff", "Director"]).await;
    let sub = person("sub@firm.test", &["Staff"]).await;

    // ───────────── everything is checked when saved, saying which step ─────────────
    let mut p = policy();
    p["steps"][1]["approvers"] = json!({"role": "Nope"});
    let e = batch(vec![op("XOrder", json!({"op": "set_workflow", "workflow": workflow(p)}))]).await.unwrap_err();
    assert!(e.contains("transition Submit, approval step 2: there is no role Nope"), "{e}");
    let mut p = policy();
    p["steps"][1]["calendar"] = json!("Nocal");
    let e = batch(vec![op("XOrder", json!({"op": "set_workflow", "workflow": workflow(p)}))]).await.unwrap_err();
    assert!(e.contains("approval step 2: there is no calendar Nocal"), "{e}");
    for (change, says) in [
        (json!({"0": {"approvers": {"field": "boss"}}}), "approval step 1 approvers: boss is not a field"),
        (json!({"0": {"approvers": {"field": "manager", "role": "Staff"}}}), "names exactly one of role, field and expr"),
        (json!({"1": {"remind_after": "soon"}}), "approval step 2: soon is not a time"),
        (json!({"1": {"not_who_took": ["Fly"]}}), "not_who_took: Fly is not a transition"),
        (json!({"1": {"when": "doc.amount >"}}), "approval step 2 when"),
        (json!({"1": {"with_next": true}}), "with_next, but no step comes after it"),
        (json!({"on_reject": "Gone"}), "on_reject: Gone is not a state"),
    ] {
        let mut p = policy();
        for (k, v) in change.as_object().unwrap() {
            match k.parse::<usize>() {
                Ok(i) => {
                    for (f, x) in v.as_object().unwrap() {
                        p["steps"][i][f] = x.clone();
                    }
                }
                Err(_) => p[k] = v.clone(),
            }
        }
        let e = batch(vec![op("XOrder", json!({"op": "set_workflow", "workflow": workflow(p)}))]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    batch(vec![op("XOrder", json!({"op": "set_workflow", "workflow": workflow(policy())}))]).await.expect("the policy");
    let def = k.custom_definition(&as_(root).await, "XOrder").unwrap();
    assert_eq!(def["workflow"]["transitions"][1]["approval"], policy(), "the definition carries the policy as written");

    let (annc, maxc, fin1c, fin2c, fin3c, dirc, subc) =
        (as_(ann).await, as_(max).await, as_(fin1).await, as_(fin2).await, as_(fin3).await, as_(dir).await, as_(sub).await);
    let order = |amount: i64, manager: Uuid| {
        let k = k.clone();
        let annc = annc.clone();
        async move {
            let id = id_of(
                &k.insert(&annc, "XOrder", obj(json!({"title": format!("Order {amount}"), "amount": amount, "manager": manager.to_string()}))).await.unwrap(),
            );
            k.transition(&annc, "XOrder", id, "Prepare", None).await.unwrap();
            id
        }
    };
    let submit = |ctx: Ctx, id: Uuid| {
        let k = k.clone();
        async move { k.transition_with(&ctx, "XOrder", id, "Submit", None, json!({"reason": "stock"})).await }
    };
    let state = |id: Uuid| {
        let k = k.clone();
        let admin = admin.clone();
        async move { k.get(&admin, "XOrder", id, false).await.unwrap()["workflow_state"].as_str().unwrap().to_string() }
    };
    let approve = |ctx: Ctx, id: Uuid| {
        let k = k.clone();
        async move { k.decide_approval(&ctx, "XOrder", id, true, Some("fine")).await }
    };

    // ───────────── no step applies: through at once ─────────────
    let free = order(0, max).await;
    let done = submit(annc.clone(), free).await.unwrap();
    assert_eq!(done["workflow_state"], json!("Approved"), "every step skipped lets it through");
    assert!(done.get("_approval").is_none());

    // ───────────── one step: asked, shown, decided once ─────────────
    let small = order(500, max).await;
    refused(k.transition(&annc, "XOrder", small, "Submit", None).await, "reason: is needed to Submit");
    let asked = submit(annc.clone(), small).await.unwrap();
    assert_eq!(asked["workflow_state"], json!("Ready"), "the record stays where it is");
    let a = &asked["_approval"];
    assert_eq!((a["status"].as_str(), a["to"].as_str()), (Some("pending"), Some("Approved")), "{a}");
    assert_eq!(a["steps"][0]["approvers"], json!([max]), "who may decide, worked out as it opened");
    assert_eq!(a["steps"][1]["state"], json!("skipped"), "Finance only above 10,000");
    refused(submit(annc.clone(), small).await, "awaiting approval of 'Submit'");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let told: i64 = sqlx::query_scalar("select count(*) from notification where user_id = $1 and kind = 'approval' and record_id = $2")
        .bind(max)
        .bind(small)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(told, 1, "the approver is told when the step opens");
    let mine = k.my_approval_requests(&maxc, 50).await.unwrap();
    assert_eq!(mine["to_decide"].as_array().map(Vec::len), Some(1), "{mine}");
    assert_eq!(k.my_approval_requests(&annc, 50).await.unwrap()["awaiting_others"][0]["record"], json!(small));
    refused(approve(annc.clone(), small).await, "not yours to decide");
    refused(approve(fin1c.clone(), small).await, "not yours to decide");
    let decided = approve(maxc.clone(), small).await.unwrap();
    assert_eq!(decided["status"], json!("approved"));
    let after = k.get(&admin, "XOrder", small, false).await.unwrap();
    assert_eq!(after["workflow_state"], json!("Approved"), "the transition completed");
    assert_eq!(after["note"], json!("approved: stock"), "with its effects and the reason given when asked");
    assert_eq!(after["updated_by"], json!(ann.to_string()), "on the requester's behalf");
    refused(approve(maxc.clone(), small).await, "an approval awaiting a decision");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let history: Vec<String> = sqlx::query_scalar("select action from audit_event where record_id = $1 and action like 'approval:%' order by id")
        .bind(small)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(history, ["approval:requested", "approval:approve"], "the decision is in the record's history");
    assert_eq!(k.my_approval_requests(&maxc, 50).await.unwrap()["to_decide"], json!([]));

    // ───────────── two steps; two people at the same moment complete it once ─────────────
    let big = order(20_000, max).await;
    submit(annc.clone(), big).await.unwrap();
    refused(approve(fin1c.clone(), big).await, "not yours to decide");
    let s = approve(maxc.clone(), big).await.unwrap();
    assert_eq!((s["steps"][0]["state"].as_str(), s["steps"][1]["state"].as_str()), (Some("approved"), Some("open")), "{s}");
    assert_eq!(s["steps"][1]["approvers"], json!([fin1, fin2, fin3]));
    let (one, two) = tokio::join!(approve(fin1c.clone(), big), approve(fin3c.clone(), big));
    let (one, two) = (one.expect("first"), two.expect("second"));
    assert!([&one, &two].iter().any(|s| s["status"] == json!("approved")), "{one} {two}");
    assert_eq!(state(big).await, "Approved");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let moves: i64 = sqlx::query_scalar("select count(*) from audit_event where record_id = $1 and action = 'workflow:Submit' and diff ? 'workflow_state'")
        .bind(big)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let kept: i64 = sqlx::query_scalar("select count(*) from approval_decision where outcome = 'approve' and not void and request_id = $1")
        .bind(Uuid::parse_str(one["id"].as_str().unwrap()).unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((moves, kept), (1, 3), "completed once, no decision lost");

    // ───────────── independence ─────────────
    // The requester is not an approver, even when the field names them.
    let own = id_of(&k.insert(&maxc, "XOrder", obj(json!({"title": "Own", "amount": 5, "manager": max.to_string()}))).await.unwrap());
    k.transition(&maxc, "XOrder", own, "Prepare", None).await.unwrap();
    refused(submit(maxc.clone(), own).await, "approval step 1 (Manager): 0 may decide it, and it needs 1");
    // Whoever prepared it, or decided the step before, does not decide Finance's.
    let prepared = id_of(&k.insert(&annc, "XOrder", obj(json!({"title": "Prep", "amount": 30_000, "manager": fin1.to_string()}))).await.unwrap());
    k.transition(&fin2c, "XOrder", prepared, "Prepare", None).await.unwrap();
    submit(annc.clone(), prepared).await.unwrap();
    // Of Finance's three, fin1 decided the manager's step and fin2 prepared it: one is left where two are needed, so
    // the step cannot open, and the decision that would open it is refused with it.
    refused(approve(fin1c.clone(), prepared).await, "approval step 2 (Finance): 1 may decide it, and it needs 2");
    assert_eq!(k.approval_of(&annc, "XOrder", prepared).await.unwrap()["decisions"], json!([]), "nothing of it kept");

    // ───────────── a change to the amount starts again; other edits do not ─────────────
    let moving = order(15_000, max).await;
    submit(annc.clone(), moving).await.unwrap();
    approve(maxc.clone(), moving).await.unwrap();
    k.update(&annc, "XOrder", moving, obj(json!({"title": "Renamed"})), None).await.unwrap();
    let s = k.approval_of(&annc, "XOrder", moving).await.unwrap();
    assert_eq!(s["steps"][1]["state"], json!("open"), "a title change keeps the decisions");
    k.update(&annc, "XOrder", moving, obj(json!({"amount": 16_000})), None).await.unwrap();
    let s = k.approval_of(&annc, "XOrder", moving).await.unwrap();
    assert_eq!((s["steps"][0]["state"].as_str(), s["steps"][1]["state"].as_str()), (Some("open"), Some("waiting")), "{s}");
    assert_eq!(s["decisions"][0]["void"], json!(true), "kept, but no longer counted");

    // ───────────── rejection: a reason, and where the record goes ─────────────
    refused(k.decide_approval(&maxc, "XOrder", moving, false, Some("  ")).await, "comment: a reason is needed to reject");
    let s = k.decide_approval(&maxc, "XOrder", moving, false, Some("too dear")).await.unwrap();
    assert_eq!(s["status"], json!("rejected"));
    assert_eq!(state(moving).await, "Rejected", "on_reject moves it");
    assert_eq!(s["decisions"].as_array().unwrap().last().unwrap()["comment"], json!("too dear"));

    // ───────────── recall ─────────────
    let recalled = order(100, max).await;
    submit(annc.clone(), recalled).await.unwrap();
    refused(k.recall_approval(&maxc, "XOrder", recalled).await, "whoever asked for it recalls it");
    assert_eq!(k.recall_approval(&annc, "XOrder", recalled).await.unwrap()["status"], json!("recalled"));
    refused(approve(maxc.clone(), recalled).await, "an approval awaiting a decision");
    submit(annc.clone(), recalled).await.expect("asked again");

    // ───────────── who may decide is checked again at the decision ─────────────
    let checked = order(12_000, max).await;
    submit(annc.clone(), checked).await.unwrap();
    approve(maxc.clone(), checked).await.unwrap();
    k.set_user_roles(&admin, fin3, &["Staff".into()]).await.unwrap();
    refused(approve(as_(fin3).await, checked).await, "you may no longer decide this step");
    k.set_user_roles(&admin, fin3, &["Staff".into(), "Finance".into()]).await.unwrap();

    // ───────────── a stand-in decides for whom they stand in for ─────────────
    let today = Utc::now().date_naive();
    k.set_away(&maxc, max, today, today + Duration::days(1), sub, "leave").await.unwrap();
    let away = order(200, max).await;
    submit(annc.clone(), away).await.unwrap();
    assert_eq!(k.my_approval_requests(&subc, 50).await.unwrap()["to_decide"][0]["record"], json!(away));
    let s = approve(subc.clone(), away).await.unwrap();
    assert_eq!(s["status"], json!("approved"));
    let d = &s["decisions"][0];
    assert_eq!((d["decider"].as_str(), d["on_behalf_of"].as_str()), (Some(sub.to_string().as_str()), Some(max.to_string().as_str())), "both recorded");
    // A stand-in who asked for it is skipped.
    k.set_away(&maxc, max, today, today + Duration::days(1), ann, "leave").await.unwrap();
    let skipped = order(300, max).await;
    submit(annc.clone(), skipped).await.unwrap();
    refused(approve(annc.clone(), skipped).await, "someone else has to approve this");
    approve(maxc.clone(), skipped).await.expect("the person themselves still may");

    // ───────────── reminders, then escalation, once each, by the minute loop ─────────────
    // A day back, so the requests above (opened at the clock) are not yet owed anything at these moments.
    let t0: DateTime<Utc> = Utc::now() - Duration::days(1);
    k.pin_clock(Some(t0));
    let late = order(50_000, max).await;
    submit(annc.clone(), late).await.unwrap();
    approve(maxc.clone(), late).await.unwrap();
    assert_eq!(k.sweep_approvals_at(t0 + Duration::minutes(30)).await.unwrap(), 0, "not yet");
    assert_eq!(k.sweep_approvals_at(t0 + Duration::minutes(61)).await.unwrap(), 1, "reminded");
    assert_eq!(k.sweep_approvals_at(t0 + Duration::minutes(62)).await.unwrap(), 0, "once");
    refused(approve(dirc.clone(), late).await, "not yours to decide");
    let (a, b) = tokio::join!(k.sweep_approvals_at(t0 + Duration::minutes(121)), k.sweep_approvals_at(t0 + Duration::minutes(121)));
    assert_eq!(a.unwrap() + b.unwrap(), 1, "escalated once, however many passes");
    // A pass that found the request before another marked it (its due moment read earlier) does nothing again.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update approval_request set next_due = $1 where record_id = $2 and status = 'pending'").bind(t0).bind(late).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.sweep_approvals_at(t0 + Duration::minutes(122)).await.unwrap(), 0, "each threshold once, under the request's lock");
    let s = k.approval_of(&annc, "XOrder", late).await.unwrap();
    assert_eq!(s["steps"][1]["escalated"], json!([dir]));
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let notes: Vec<String> =
        sqlx::query_scalar("select title from notification where record_id = $1 and kind = 'approval' and user_id = $2 order by created_at")
            .bind(late)
            .bind(fin1)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(notes.len(), 2, "opened, then one reminder: {notes:?}");
    approve(dirc.clone(), late).await.expect("a director may decide now");
    approve(fin2c.clone(), late).await.unwrap();
    assert_eq!(state(late).await, "Approved");
    k.pin_clock(None);

    // ───────────── a packaged entity takes a policy the same way ─────────────
    batch(vec![op(
        "Uom",
        json!({"op": "set_workflow", "workflow": {"name": "units", "entity": "Uom", "initial_state": "Open",
            "states": [{"name": "Open"}, {"name": "Retired"}],
            "transitions": [{"action": "Retire", "from": "Open", "to": "Retired",
                "approval": {"steps": [{"label": "Director", "approvers": {"role": "Director"}}]}}]}}),
    )])
    .await
    .expect("a policy on a package's entity");
    let admin = as_(root).await;
    let unit = id_of(&k.insert(&admin, "Uom", obj(json!({"uom_name": "Crate"}))).await.unwrap());
    let asked = k.transition(&admin, "Uom", unit, "Retire", None).await.unwrap();
    assert_eq!(asked["_approval"]["status"], json!("pending"));
    k.decide_approval(&as_(dir).await, "Uom", unit, true, None).await.unwrap();
    assert_eq!(k.get(&admin, "Uom", unit, false).await.unwrap()["workflow_state"], json!("Retired"));

    // ───────────── an approver with no role on the entity reads the one record awaiting them, while it does ─────────────
    batch(vec![
        op("XClaimLine", json!({"op": "define_entity", "entity": {"name": "XClaimLine", "label": "Claim Line", "kind": "child", "fields": [{"name": "item", "type": "data"}]}})),
        op(
            "XClaim",
            json!({"op": "define_entity", "entity": {"name": "XClaim", "label": "Claim", "kind": "document", "title_field": "title", "fields": [
                {"name": "title", "type": "data"}, {"name": "approver", "type": "data"}, {"name": "second", "type": "data"},
                {"name": "salary", "type": "int", "perm_group": "pay"}, {"name": "bank", "type": "data", "perm_group": "bank"},
                {"name": "lines", "type": "table", "of": "XClaimLine"}]}}),
        ),
        op(
            "XClaim",
            json!({"op": "set_workflow", "workflow": {"name": "claims", "entity": "XClaim", "initial_state": "Draft",
                "states": [{"name": "Draft"}, {"name": "Paid"}],
                "transitions": [{"action": "Pay", "from": "Draft", "to": "Paid", "approval": {"show": ["salary"], "steps": [
                    {"label": "First", "approvers": {"field": "approver"}}, {"label": "Second", "approvers": {"field": "second"}}]}}]}}),
        ),
    ])
    .await
    .expect("claims");
    let mut p = json!({"show": ["nope"], "steps": [{"label": "First", "approvers": {"field": "approver"}}]});
    let e = batch(vec![op(
        "XClaim",
        json!({"op": "set_workflow", "workflow": {"name": "claims", "entity": "XClaim", "initial_state": "Draft",
        "states": [{"name": "Draft"}, {"name": "Paid"}], "transitions": [{"action": "Pay", "from": "Draft", "to": "Paid", "approval": p.take()}]}}),
    )])
    .await
    .unwrap_err();
    assert!(e.contains("show: nope is not a field"), "{e}");
    let admin = as_(root).await;
    k.set_role_permission(&admin, "Staff", &PermissionDef { entity: "XClaim".into(), read: true, write: true, create: true, ..Default::default() })
        .await
        .unwrap();
    let out1 = person("out1@firm.test", &[]).await;
    let out2 = person("out2@firm.test", &[]).await;
    let out3 = person("out3@firm.test", &[]).await;
    let (annc, out1c, out2c, out3c) = (as_(ann).await, as_(out1).await, as_(out2).await, as_(out3).await);
    let claim = |title: &'static str| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let d = json!({"title": title, "approver": out1.to_string(), "second": out2.to_string(), "salary": 5000, "bank": "DE00 1234",
                "lines": [{"item": "Taxi"}]});
            id_of(&k.insert(&admin, "XClaim", obj(d)).await.unwrap())
        }
    };
    let (paid, other, taken_back) = (claim("Paid").await, claim("Other").await, claim("Taken back").await);
    refused(k.read_record(&out1c, "XClaim", paid, false).await, "not permitted");
    k.transition(&annc, "XClaim", paid, "Pay", None).await.unwrap();
    let seen = k.read_record(&out1c, "XClaim", paid, true).await.expect("the approver reads the record awaiting them");
    assert_eq!((seen["title"].as_str(), seen["lines"][0]["item"].as_str()), (Some("Paid"), Some("Taxi")), "with its lines");
    assert_eq!(seen["salary"], json!(5000), "a field the policy shows them");
    assert!(seen.get("bank").is_none(), "a field group not shown stays hidden: {seen:?}");
    assert!(k.audit_trail(&out1c, "XClaim", paid).await.is_ok(), "and its history");
    refused(k.read_record(&out1c, "XClaim", other, false).await, "not permitted");
    refused(k.read_record(&out2c, "XClaim", paid, false).await, "not permitted");
    refused(k.get(&out1c, "XClaim", paid, false).await, "not permitted");
    refused(k.list(&out1c, "XClaim", &ListQuery::default()).await, "not permitted");
    refused(k.update(&out1c, "XClaim", paid, obj(json!({"title": "Mine"})), None).await, "not permitted");
    k.decide_approval(&out1c, "XClaim", paid, true, None).await.expect("the first decides");
    refused(k.read_record(&out1c, "XClaim", paid, false).await, "not permitted");
    // The second step is theirs: away, their stand-in reads and decides for them.
    let today = Utc::now().date_naive();
    k.set_away(&out2c, out2, today, today + Duration::days(1), out3, "leave").await.unwrap();
    k.read_record(&out3c, "XClaim", paid, false).await.expect("the stand-in reads it");
    k.decide_approval(&out3c, "XClaim", paid, true, None).await.expect("the stand-in decides");
    assert_eq!(k.get(&admin, "XClaim", paid, false).await.unwrap()["workflow_state"], json!("Paid"));
    for c in [&out1c, &out2c, &out3c] {
        refused(k.read_record(c, "XClaim", paid, false).await, "not permitted");
    }
    // Recalled, the access ends with it.
    k.transition(&annc, "XClaim", taken_back, "Pay", None).await.unwrap();
    k.read_record(&out1c, "XClaim", taken_back, false).await.expect("while pending");
    k.recall_approval(&annc, "XClaim", taken_back).await.unwrap();
    refused(k.read_record(&out1c, "XClaim", taken_back, false).await, "not permitted");
}
