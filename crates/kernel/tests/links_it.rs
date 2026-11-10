//! Linked records in expressions and effects on a record's own lines (docs/api.md, "Linked records" and "Effects
//! on lines"; docs/builder/decisions.md BD-14), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database
//! is reset first). The permit office of the paper measurement
//! (docs/builder/evidence/government-retail-on-paper-2026-11-10.md, G6, G12, G15, G16): a permit type lists
//! documents and stages; an application is lodged only with every document the type lists; passing a stage appends
//! a review; approval waits until the reviews cover every stage of the type.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// The package's office: a record with a secret no expression may read.
const OFFICES: &str =
    "- name: GovOffice\n  label: Office\n  kind: master\n  title_field: name\n  fields:\n    - {name: name, type: data}\n    - {name: api_key, type: secret}\n";

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-links-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("gov/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("gov/package.yaml"), "package: gov\nversion: 0.1.0\nlabel: Government\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("gov/entities/gov.yaml"), OFFICES).unwrap();
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

fn define(entity: Value) -> NewCustomization {
    op(entity["name"].as_str().unwrap(), json!({"op": "define_entity", "entity": entity}))
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

/// Every document the type lists has a row with a file (G6).
const DOCUMENTS: &str = "!doc.lodged || links.type == null || links.type.documents.all(d, doc.documents.exists(r, r.document == d.document && r.file != ''))";
/// The reviews cover every stage of the type (G12).
const STAGES: &str = "links.type.stages.all(s, doc.reviews.exists(r, r.stage == s.stage))";

fn entities() -> Vec<NewCustomization> {
    let child = |name: &str, fields: Value| json!({"name": name, "label": name, "kind": "child", "fields": fields});
    vec![
        define(child("XTypeDoc", json!([{"name": "document", "type": "data"}]))),
        define(child("XTypeStage", json!([{"name": "stage", "type": "data"}, {"name": "inspection", "type": "check"}]))),
        define(json!({"name": "XPermitType", "label": "Permit Type", "kind": "master", "title_field": "name", "fields": [
            {"name": "name", "type": "data"}, {"name": "kind", "type": "link", "to": "GovOffice"},
            {"name": "documents", "type": "table", "of": "XTypeDoc"}, {"name": "stages", "type": "table", "of": "XTypeStage"}]})),
        define(child("XAppDoc", json!([{"name": "document", "type": "data"}, {"name": "file", "type": "data"}]))),
        define(json!({"name": "XReview", "label": "Review", "kind": "child", "fields": [
            {"name": "stage", "type": "data"}, {"name": "reviewer", "type": "data"}, {"name": "day", "type": "date"},
            {"name": "note", "type": "data"}, {"name": "findings", "type": "data"}],
            "rules": [{"name": "named", "check": "doc.stage != ''", "message": "a review names its stage"}]})),
        define(json!({"name": "XApplication", "label": "Application", "kind": "document", "submittable": true, "title_field": "applicant", "fields": [
            {"name": "applicant", "type": "data"}, {"name": "type", "type": "link", "to": "XPermitType"},
            {"name": "office", "type": "link", "to": "GovOffice"}, {"name": "lodged", "type": "check"}, {"name": "note", "type": "data"},
            {"name": "review_count", "type": "int", "rollup": {"table": "reviews", "op": "count"}},
            {"name": "documents", "type": "table", "of": "XAppDoc"}, {"name": "reviews", "type": "table", "of": "XReview"}],
            "rules": [
                {"name": "type_named", "check": "!doc.lodged || links.type != null", "message": "name the permit type"},
                {"name": "documents", "check": DOCUMENTS, "message": "every document the permit type lists, each with a file"},
                {"name": "findings", "check": "doc.reviews.all(r, (r.findings != null && r.findings != '') || !links.type.stages.exists(s, s.stage == r.stage && s.inspection))",
                 "message": "an inspection stage records its findings"}]})),
        op(
            "XApplication",
            json!({"op": "set_workflow", "workflow": {"name": "permits", "entity": "XApplication", "initial_state": "Draft",
                "states": [{"name": "Draft"}, {"name": "Approved", "docstatus": 1}],
                "transitions": [{"action": "approve", "from": "Draft", "to": "Approved", "roles": ["Officer"], "condition": STAGES}]}}),
        ),
        op(
            "XApplication",
            json!({"op": "add_action", "action": {"name": "x_pass", "label": "Pass Stage", "condition": "doc.lodged && links.type != null",
                "params": [{"name": "stage", "label": "Stage", "type": "data", "required": true}, {"name": "note", "label": "Note", "type": "data"}],
                "refuse_if": [{"when": "!links.type.stages.exists(s, s.stage == params.stage)", "message": "the permit type has no such stage"}],
                "effects": [{"type": "append_line", "table": "reviews",
                    "values": {"stage": "params.stage", "reviewer": "user.email", "day": "today", "note": "params.note + ' (' + links.type.name + ')'"}}]}}),
        ),
        op(
            "XApplication",
            json!({"op": "add_action", "action": {"name": "x_findings", "label": "Record Findings",
                "params": [{"name": "stage", "label": "Stage", "type": "data"}, {"name": "findings", "label": "Findings", "type": "data"}],
                "effects": [{"type": "set_line", "table": "reviews", "where": "row.stage == params.stage", "values": {"findings": "params.findings"}}]}}),
        ),
        op(
            "XApplication",
            json!({"op": "add_action", "action": {"name": "x_twice", "label": "Two Notes",
                "effects": [{"type": "append_line", "table": "reviews", "values": {"stage": "'note'", "note": "'first'"}},
                    {"type": "append_line", "table": "reviews", "values": {"stage": "'note'", "note": "'after ' + string(size(results))"}}]}}),
        ),
        op(
            "XApplication",
            json!({"op": "add_action", "action": {"name": "x_blank", "label": "Blank Review",
                "effects": [{"type": "append_line", "table": "reviews", "values": {"stage": "''"}}]}}),
        ),
        op(
            "XApplication",
            json!({"op": "add_action", "action": {"name": "x_peek", "label": "Peek", "condition": "links.office != null && has(links.office.name) && !has(links.office.api_key)",
                "effects": [{"type": "set_fields", "values": {"note": "links.office.name"}}]}}),
        ),
    ]
}

struct W {
    k: Arc<Kernel>,
    tenant: Uuid,
    user: Uuid,
}

impl W {
    async fn admin(&self) -> Ctx {
        self.k.load_ctx(self.tenant, self.user, None, "it".into()).await.unwrap()
    }
    async fn batch(&self, items: Vec<NewCustomization>) -> Result<usize, String> {
        self.k.batch_customizations(&self.admin().await, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin().await, entity, id, false).await.unwrap()
    }
}

#[test]
fn linked_records_and_line_effects() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

fn col(d: &Doc, table: &str, field: &str) -> Vec<Value> {
    d[table].as_array().unwrap().iter().map(|r| r[field].clone()).collect()
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(
        db,
        catalog(),
        HookRegistry::default(),
        KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([42u8; 32])), ..Default::default() },
        None,
    )
    .await
    .expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "city".into(),
            name: "City".into(),
            admin_email: "admin@city.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["gov".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("city", "admin@city.test", "Correct-Horse-9").await.unwrap();
    let w = W { k: k.clone(), tenant, user: session.user };

    // ───────────── refused when saved ─────────────
    let mut items = entities();
    w.batch(items.drain(..6).collect()).await.expect("the entities, their rules reading the permit type");
    let rule = |check: &str| op("XApplication", json!({"op": "add_rule", "rule": {"name": "x_bad", "check": check, "message": "m"}}));
    let e = w.batch(vec![rule("links.note != null")]).await.unwrap_err();
    assert!(e.contains("note is not a link field of XApplication"), "{e}");
    let e = w.batch(vec![rule("links['type'] != null")]).await.unwrap_err();
    assert!(e.contains("links.<link field>"), "{e}");
    let field = |prop: &str| op("XApplication", json!({"op": "add_field", "field": {"name": "x_seen", "type": "data", prop: "links.type.name"}}));
    for prop in ["computed", "required_if", "visible_if"] {
        let e = w.batch(vec![field(prop)]).await.unwrap_err();
        assert!(e.contains("without the linked records"), "{prop}: {e}");
    }
    let line_effect = |effect: Value| op("XApplication", json!({"op": "add_action", "action": {"name": "x_bad", "label": "Bad", "effects": [effect]}}));
    for (effect, says) in [
        (json!({"type": "append_line", "table": "note", "values": {"stage": "'x'"}}), "is not a table of lines"),
        (json!({"type": "append_line", "table": "reviews", "values": {"colour": "'x'"}}), "has no field colour"),
        (
            json!({"type": "set_line", "table": "reviews", "where": "row.stage == ''", "values": {"stage": "links.office.name + links.kind"}}),
            "kind is not a link field",
        ),
        (json!({"type": "set_fields", "values": {"reviews": "[]"}}), "is not written by an effect"),
    ] {
        let e = w.batch(vec![line_effect(effect)]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    w.batch(items).await.expect("the workflow and the buttons");
    let admin = w.admin().await;
    let bad = automation::save(
        &k,
        &admin,
        &obj(json!({"name": "Bad", "entity": "XApplication", "event": "created", "condition": "links.applicant != null",
        "actions": [{"type": "notify", "to": "owner", "title": "'t'"}]})),
    )
    .await;
    refused(bad, "applicant is not a link field");

    // ───────────── people ─────────────
    let person = |email: &'static str, role: &'static str| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let id = k.create_user(&admin, email, email, None, &[role.to_string()], false).await.unwrap();
            k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
        }
    };
    for role in ["Clerk", "Officer"] {
        let rule = PermissionDef { entity: "XApplication".into(), read: true, write: true, create: true, submit: true, ..Default::default() };
        k.set_role_permission(&admin, role, &rule).await.unwrap();
    }
    let clerk = person("clerk@city.test", "Clerk").await;
    let officer = person("officer@city.test", "Officer").await;

    // ───────────── the permit type, the office ─────────────
    let office = k.insert(&admin, "GovOffice", obj(json!({"name": "North", "api_key": "s3cret"}))).await.expect("an office with a key");
    let ty = k
        .insert(
            &admin,
            "XPermitType",
            obj(json!({"name": "Kiosk", "kind": id_of(&office).to_string(),
                "documents": [{"document": "ID"}, {"document": "Plan"}],
                "stages": [{"stage": "Desk"}, {"stage": "Site", "inspection": true}]})),
        )
        .await
        .unwrap();
    let ty_id = id_of(&ty);
    assert!(k.get(&clerk, "XPermitType", ty_id, false).await.is_err(), "the clerk cannot open a permit type");

    // ───────────── G6: lodged only with every document the type lists ─────────────
    let app = k
        .insert(&clerk, "XApplication", obj(json!({"applicant": "Asha", "type": ty_id.to_string(), "documents": [{"document": "ID", "file": "id.pdf"}]})))
        .await
        .expect("a draft, not lodged");
    let app_id = id_of(&app);
    refused(k.update(&clerk, "XApplication", app_id, obj(json!({"lodged": true})), None).await, "every document the permit type lists");
    refused(k.update(&admin, "XApplication", app_id, obj(json!({"lodged": true})), None).await, "every document the permit type lists");
    let two = json!([{"document": "ID", "file": "id.pdf"}, {"document": "Plan", "file": ""}]);
    refused(k.update(&clerk, "XApplication", app_id, obj(json!({"lodged": true, "documents": two})), None).await, "each with a file");
    let two = json!([{"document": "ID", "file": "id.pdf"}, {"document": "Plan", "file": "plan.pdf"}]);
    let app = k.update(&clerk, "XApplication", app_id, obj(json!({"lodged": true, "documents": two})), None).await.expect("whoever saves, the same answer");
    assert_eq!(app["lodged"], json!(true));
    // An empty link is null, and the rule says so.
    let blank = k.insert(&clerk, "XApplication", obj(json!({"applicant": "Ben"}))).await.unwrap();
    refused(k.update(&clerk, "XApplication", id_of(&blank), obj(json!({"lodged": true})), None).await, "name the permit type");
    // A linked record changed earlier in the same transaction is seen as it is now.
    {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let more = json!([{"document": "ID"}, {"document": "Plan"}, {"document": "Survey"}]);
        k.update_tx(&admin, &mut tx, "XPermitType", ty_id, obj(json!({"documents": more})), None).await.unwrap();
        let e = k.update_tx(&clerk, &mut tx, "XApplication", app_id, obj(json!({"note": "again"})), None).await;
        refused(e, "every document the permit type lists");
    }
    k.update(&clerk, "XApplication", app_id, obj(json!({"note": "again"})), None).await.expect("rolled back: two documents again");

    // ───────────── a button's condition and refusal, described for the browser ─────────────
    let described = ferrum_kernel::describe::describe(&clerk, clerk.schema.entity("XApplication").unwrap());
    let pass = described["actions"].as_array().unwrap().iter().find(|a| a["name"] == "x_pass").cloned().unwrap();
    assert_eq!((pass["condition"].as_str(), pass["may_run"].as_bool()), (Some("doc.lodged && links.type != null"), Some(true)));
    refused(k.run_action(&clerk, "XApplication", id_of(&blank), "x_pass", json!({"stage": "Desk"})).await, "is not available for this document");
    refused(k.run_action(&clerk, "XApplication", app_id, "x_pass", json!({"stage": "Roof"})).await, "the permit type has no such stage");

    // ───────────── G16: passing a stage appends a review ─────────────
    let before = w.get("XApplication", app_id).await;
    let doc_ids = col(&before, "documents", "id");
    k.run_action(&clerk, "XApplication", app_id, "x_pass", json!({"stage": "Desk", "note": "complete"})).await.expect("the desk passes");
    let after = w.get("XApplication", app_id).await;
    assert_eq!(after["version"].as_i64(), before["version"].as_i64().map(|v| v + 1), "one new version");
    assert_eq!(col(&after, "documents", "id"), doc_ids, "the other table as it was");
    assert_eq!(col(&after, "reviews", "stage"), [json!("Desk")]);
    assert_eq!(col(&after, "reviews", "reviewer"), [json!("clerk@city.test")]);
    assert_eq!(col(&after, "reviews", "note"), [json!("complete (Kiosk)")], "an effect's value reads the permit type");
    assert_eq!(after["review_count"], json!(1), "totals run");
    let desk_id = after["reviews"][0]["id"].clone();

    // ───────────── G12: approval waits for every stage ─────────────
    let inbox = k.my_approvals(&officer, 20).await.unwrap();
    assert!(inbox.iter().any(|r| r["id"] == json!(app_id.to_string())), "the inbox takes a guard reading linked records to hold: {inbox:?}");
    refused(k.transition(&officer, "XApplication", app_id, "approve", None).await, "the conditions for 'approve' are not met");

    // G15: an inspection stage records its findings; set_line changes the one line.
    refused(
        k.run_action(&officer, "XApplication", app_id, "x_pass", json!({"stage": "Site", "note": "walked"})).await,
        "an inspection stage records its findings",
    );
    k.update(&admin, "XApplication", app_id, obj(json!({"reviews": [{"id": desk_id}, {"stage": "Site", "findings": "draft"}]})), None).await.unwrap();
    let before = w.get("XApplication", app_id).await;
    refused(
        k.run_action(&officer, "XApplication", app_id, "x_findings", json!({"stage": "Roof", "findings": "x"})).await,
        "no line of reviews has row.stage == params.stage",
    );
    k.run_action(&officer, "XApplication", app_id, "x_findings", json!({"stage": "Site", "findings": "sound"})).await.expect("the one line");
    let after = w.get("XApplication", app_id).await;
    assert_eq!(after["reviews"][0], before["reviews"][0], "the other line, its id and values");
    assert_eq!(after["reviews"][1]["id"], before["reviews"][1]["id"], "the line keeps its id");
    assert_eq!(col(&after, "reviews", "findings"), [json!(null), json!("sound")]);
    assert_eq!(after["version"].as_i64(), before["version"].as_i64().map(|v| v + 1));
    refused(
        k.run_action(&officer, "XApplication", app_id, "x_findings", json!({"stage": "Site", "findings": ""})).await,
        "an inspection stage records its findings",
    );
    // A row rule runs on the line appended.
    refused(k.run_action(&officer, "XApplication", app_id, "x_blank", json!({})).await, "a review names its stage");

    // Two appends in one button both appear, the second seeing the first's result.
    let before = w.get("XApplication", app_id).await;
    k.run_action(&admin, "XApplication", app_id, "x_twice", json!({})).await.expect("two notes");
    let after = w.get("XApplication", app_id).await;
    assert_eq!(col(&after, "reviews", "note")[2..], [json!("first"), json!("after 1")]);
    assert_eq!(after["version"].as_i64(), before["version"].as_i64().map(|v| v + 2), "one version per effect");
    refused(k.run_action(&officer, "XApplication", app_id, "x_findings", json!({"stage": "note", "findings": "x"})).await, "lines 3, 4 of reviews");

    let approved = k.transition(&officer, "XApplication", app_id, "approve", None).await.expect("every stage reviewed");
    assert_eq!(approved["workflow_state"], "Approved");
    refused(k.run_action(&admin, "XApplication", app_id, "x_twice", json!({})).await, "a submitted Application cannot be edited");

    // ───────────── secrets stay out; read as the workspace ─────────────
    let peek = k.insert(&clerk, "XApplication", obj(json!({"applicant": "Cy", "office": id_of(&office).to_string()}))).await.unwrap();
    k.run_action(&clerk, "XApplication", id_of(&peek), "x_peek", json!({})).await.expect("the office's name, never its key");
    assert_eq!(w.get("XApplication", id_of(&peek)).await["note"], "North");

    // ───────────── an automation: its condition, its values and its line effects ─────────────
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    automation::save(
        &k,
        &admin,
        &obj(json!({"name": "First stage", "entity": "XApplication", "event": "created", "condition": "links.type != null && links.type.name == 'Kiosk'",
            "actions": [{"type": "append_line", "table": "reviews", "values": {"stage": "links.type.stages[0].stage", "note": "'opened'"}},
                {"type": "set_line", "table": "reviews", "where": "row.note == 'opened'", "values": {"reviewer": "'desk@' + links.type.name"}}]})),
    )
    .await
    .expect("an automation reading the permit type");
    let auto = k.insert(&clerk, "XApplication", obj(json!({"applicant": "Dev", "type": ty_id.to_string()}))).await.unwrap();
    let other = k.insert(&clerk, "XApplication", obj(json!({"applicant": "Eve"}))).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    let auto = w.get("XApplication", id_of(&auto)).await;
    assert_eq!((col(&auto, "reviews", "stage"), col(&auto, "reviews", "reviewer")), (vec![json!("Desk")], vec![json!("desk@Kiosk")]));
    assert_eq!(w.get("XApplication", id_of(&other)).await["reviews"], json!([]), "no type, no run");

    franchise_periods(&w, &office).await;
}

/// R30: no two reports of one franchisee overlap, by an aggregate whose filter reads the saved record's own fields.
async fn franchise_periods(w: &W, office: &Doc) {
    let k = &w.k;
    w.batch(vec![define(json!({"name": "XReport", "label": "Franchise Report", "kind": "master", "fields": [
        {"name": "franchisee", "type": "link", "to": "GovOffice"}, {"name": "period_from", "type": "date"}, {"name": "period_to", "type": "date"}]}))])
        .await
        .unwrap();
    let admin = w.admin().await;
    let solution = |filter: Value| {
        json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "city.franchise", "version": "1.0.0", "label": "Franchise",
            "rules": [{"id": "no_overlap", "entity": "XReport", "phase": "validate", "message": "another report of this franchisee overlaps these dates",
                "aggregates": [{"name": "overlap", "entity": "XReport", "link": "franchisee", "to": "franchisee", "filter": filter, "op": "exists"}],
                "check": "!aggregates.overlap"}]})
        .to_string()
    };
    let wrong = solution(json!([["period_from", "<=", {"doc": "franchisee"}]]));
    let faults = k.compile_solution(&admin, &wrong).await.unwrap().unwrap_err();
    assert!(faults.iter().any(|d| d.message.contains("does not fit")), "{faults:?}");
    let saved = k
        .save_release(
            &admin,
            &solution(json!([["period_from", "<=", {"doc": "period_to"}], ["period_to", ">=", {"doc": "period_from"}], ["id", "!=", {"doc": "id"}]])),
        )
        .await
        .unwrap();
    let second = k.create_user(&admin, "admin2@city.test", "Second", None, &[], true).await.unwrap();
    let second = k.load_ctx(admin.tenant, second, None, "it".into()).await.unwrap();
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&second, release, saved["artifact_digest"].as_str().unwrap(), true, "Read").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    let report = |from: &str, to: &str| obj(json!({"franchisee": id_of(office).to_string(), "period_from": from, "period_to": to}));
    let jan = k.insert(&admin, "XReport", report("2026-01-01", "2026-01-31")).await.expect("January");
    k.insert(&admin, "XReport", report("2026-02-01", "2026-02-28")).await.expect("February, after it");
    refused(k.insert(&admin, "XReport", report("2026-01-15", "2026-02-10")).await, "another report of this franchisee overlaps these dates");
    k.update(&admin, "XReport", id_of(&jan), obj(json!({"period_to": "2026-01-30"})), None).await.expect("a report does not overlap itself");
}
