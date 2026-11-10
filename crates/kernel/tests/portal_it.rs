//! Portal roles (docs/builder/decisions.md BD-02, BF-PORTAL-001 to 003, 005) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). A guardian and a patient are given
//! portal roles an administrator configures, with no app code, and the test tries every way past them:
//! a record of someone else's, one moved out of reach, a field in a group, an action outside its
//! condition, a role a portal person may not hold, another workspace with the same email, and a
//! guardian taken off a student. Lists and single records must agree throughout.

use async_trait::async_trait;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Action, Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Student
  label: Student
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
    - {name: email, type: email}
    - {name: notes, type: data, perm_group: staff}
    - {name: guardians, type: table, of: StudentGuardian}
- name: StudentGuardian
  label: Guardian
  kind: child
  fields:
    # Text, not an email field: kept as typed, so a path must compare it as the kernel compares emails.
    - {name: email, type: data, required: true}
    - {name: relation, type: data}
- name: ReportCard
  label: Report Card
  kind: master
  title_field: term
  fields:
    - {name: student, type: link, to: Student, required: true}
    - {name: term, type: data, required: true}
    - {name: status, type: select, options: [Draft, Published], default: Draft}
    - {name: comment, type: data, perm_group: staff}
    - {name: acknowledged, type: check}
  actions:
    - {name: acknowledge, label: Acknowledge, permission: read, condition: 'doc.status == "Published"'}
- name: Term
  label: Term
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
- name: Patient
  label: Patient
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
    - {name: email, type: email}
- name: Appointment
  label: Appointment
  kind: master
  title_field: reason
  fields:
    - {name: patient, type: link, to: Patient, required: true}
    - {name: reason, type: data, required: true}
  actions:
    - {name: cancel_it, label: Cancel It, permission: write, condition: 'doc.reason != "locked"'}
    - {name: reschedule, label: Reschedule, permission: write}
- name: Application
  label: Application
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: email, type: email}
    - {name: cv, type: attach}
- name: Claim
  label: Claim
  kind: document
  submittable: true
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: email, type: email}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-portal-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("school/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("school/package.yaml"), "package: school\nversion: 0.1.0\nlabel: School\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("school/entities/school.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

/// The app's own actions: acknowledging a report card, cancelling or rescheduling an appointment.
struct Actions;

#[async_trait]
impl DocHook for Actions {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
        let sql = match name {
            "acknowledge" => "update e_report_card set acknowledged = true where tenant_id = $1 and id = $2",
            "cancel_it" => "update e_appointment set reason = 'cancelled' where tenant_id = $1 and id = $2",
            "reschedule" => return Ok(Some(json!({"rescheduled": true}))),
            _ => return Ok(None),
        };
        sqlx::query(sql).bind(hc.ctx.tenant).bind(id).execute(&mut **hc.tx).await?;
        Ok(Some(json!({"done": name})))
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn rule(v: Value) -> PermissionDef {
    serde_json::from_value(v).expect("a rule")
}

async fn workspace(k: &Kernel, slug: &str) -> (Uuid, Ctx) {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["school".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    (tenant, admin)
}

/// A portal person of the workspace holding `roles`, as a fresh context sees them.
async fn portal_person(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str]) -> Ctx {
    let user = k.create_user(admin, email, email, None, &[], false).await.unwrap();
    k.set_user_kind(admin, user, "portal").await.unwrap();
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    k.set_user_roles_until(admin, user, &roles, &Default::default()).await.expect("portal roles given");
    k.load_ctx(admin.tenant, user, None, "it".into()).await.unwrap()
}

/// What `ctx` reaches of `entity`, through the list and record by record: the two must agree.
async fn reach(k: &Kernel, ctx: &Ctx, entity: &str, all: &[(Uuid, &str)], title: &str) -> BTreeSet<String> {
    let listed: BTreeSet<String> = k
        .list(ctx, entity, &ListQuery { limit: Some(500), ..Default::default() })
        .await
        .unwrap()
        .rows
        .iter()
        .map(|r| r[title].as_str().unwrap().to_string())
        .collect();
    let mut opened = BTreeSet::new();
    for (id, name) in all {
        match k.get(ctx, entity, *id, false).await {
            Ok(_) => {
                opened.insert(name.to_string());
            }
            Err(Error::NotFound(_) | Error::Forbidden(_)) => {}
            Err(e) => panic!("{name}: {e}"),
        }
    }
    assert_eq!(listed, opened, "{entity}: the list and the record check disagree");
    listed
}

fn names(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn portal_roles_reach_their_holders_own_records_and_nothing_else() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut hooks = HookRegistry::default();
    hooks.register("ReportCard", Arc::new(Actions));
    hooks.register("Appointment", Arc::new(Actions));
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    let (tenant, admin) = workspace(&k, "school").await;

    // ───────────── a portal role holds only own-record rules ─────────────
    k.set_role_permission(&admin, "Family", &rule(json!({"entity": "Student", "read": true}))).await.unwrap();
    let refused = k.set_role_portal(&admin, "Family", true).await.expect_err("a rule reaching every student cannot be a portal role's");
    assert!(refused.to_string().contains("own"), "{refused}");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("delete from role_permission where tenant_id = $1 and role = 'Family'").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.set_role_portal(&admin, "Family", true).await.expect("an empty role may be marked");
    for (bad, why) in [
        (json!({"entity": "*", "read": true, "if_owner": true}), "every entity"),
        (json!({"entity": "Student", "read": true}), "every record"),
        (json!({"entity": "Student", "read": true, "delete": true, "via": "guardians.email"}), "delete"),
        (json!({"entity": "Student", "read": true, "export": true, "via": "guardians.email"}), "export"),
        (json!({"entity": "Student", "read": true, "import": true, "via": "guardians.email"}), "import"),
        (json!({"entity": "ReportCard", "read": true, "cancel": true, "via": "student.guardians.email"}), "cancel"),
    ] {
        assert!(k.set_role_permission(&admin, "Family", &rule(bad.clone())).await.is_err(), "a portal role accepted a rule reaching {why}: {bad}");
    }
    // Reference data, holding nobody's personal data, may be read whole: to pick a term from.
    k.set_role_permission(&admin, "Family", &rule(json!({"entity": "Term", "read": true}))).await.expect("reading reference data whole");
    for (bad, why) in [
        (json!({"entity": "Term", "read": true, "write": true}), "changing reference data"),
        (json!({"entity": "Term", "read": true, "create": true}), "adding reference data"),
        (json!({"entity": "Patient", "read": true}), "every record of an entity holding personal data"),
    ] {
        assert!(k.set_role_permission(&admin, "Family", &rule(bad.clone())).await.is_err(), "a portal role accepted a rule {why}: {bad}");
    }
    // A path is checked against the entities it crosses when saved.
    for bad in ["student.term", "student.guardians.relation.email", "student.nobody.email", "student"] {
        let e = k.set_role_permission(&admin, "Staff", &rule(json!({"entity": "ReportCard", "read": true, "via": bad}))).await.expect_err(bad);
        assert!(matches!(e, Error::Validation(_)), "{bad}: {e}");
    }
    k.set_role_permission(&admin, "Family", &rule(json!({"entity": "Student", "read": true, "via": "guardians.email"}))).await.unwrap();
    k.set_role_permission(
        &admin,
        "Family",
        &rule(json!({"entity": "ReportCard", "read": true, "via": "student.guardians.email", "where": {"status": "Published"}})),
    )
    .await
    .unwrap();
    k.set_role_portal(&admin, "Patients", false).await.expect_err("there is no such role yet");
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Appointment", "read": true, "create": true, "write": true, "via": "patient.email"})))
        .await
        .unwrap();
    k.set_role_portal(&admin, "Patients", true).await.expect("own-record rules only");
    k.set_role_permission(
        &admin,
        "Staff",
        &rule(json!({"entity": "*", "read": true, "write": true, "create": true, "read_groups": ["staff"], "write_groups": ["staff"]})),
    )
    .await
    .unwrap();

    // ───────────── who may hold what ─────────────
    let pia = portal_person(&k, &admin, "pia@home.test", &["Family"]).await;
    let pia_id = pia.user;
    let e =
        k.set_user_roles_until(&admin, pia_id, &["Family".into(), "Staff".into()], &Default::default()).await.expect_err("a member role for a portal person");
    assert!(e.to_string().contains("portal roles"), "{e}");
    // A member holding a member role who becomes a portal person keeps nothing of it.
    let moved = k.create_user(&admin, "moved@home.test", "Moved", None, &["Staff".to_string()], false).await.unwrap();
    k.set_user_kind(&admin, moved, "portal").await.unwrap();
    let moved_ctx = k.load_ctx(tenant, moved, None, "it".into()).await.unwrap();
    assert!(moved_ctx.roles.is_empty() && !moved_ctx.is_admin, "a member role counts for nothing for a portal person: {:?}", moved_ctx.roles);
    assert!(k.list(&moved_ctx, "Student", &ListQuery::default()).await.is_err(), "nothing reaches students for them");
    let guest = k.create_user(&admin, "guest@else.test", "Guest", None, &[], false).await.unwrap();
    k.set_user_kind(&admin, guest, "guest").await.unwrap();
    assert!(k.set_user_roles_until(&admin, guest, &["Family".into()], &Default::default()).await.is_err(), "guests hold no roles, portal ones included");

    // ───────────── a guardian reaches their children's records, through the path ─────────────
    let teacher = {
        let id = k.create_user(&admin, "teacher@school.test", "Teacher", None, &["Staff".to_string()], false).await.unwrap();
        k.load_ctx(tenant, id, None, "it".into()).await.unwrap()
    };
    let ann = k
        .insert(&teacher, "Student", obj(json!({"name": "Ann", "notes": "staff only", "guardians": [{"email": " PIA@home.test ", "relation": "mother"}]})))
        .await
        .unwrap();
    let bob = k.insert(&teacher, "Student", obj(json!({"name": "Bob", "guardians": [{"email": "rob@home.test"}]}))).await.unwrap();
    let card =
        |student: &Doc, term: &str, status: &str| obj(json!({"student": id_of(student).to_string(), "term": term, "status": status, "comment": "for staff"}));
    let ann_spring = k.insert(&teacher, "ReportCard", card(&ann, "ann spring", "Published")).await.unwrap();
    let ann_draft = k.insert(&teacher, "ReportCard", card(&ann, "ann draft", "Draft")).await.unwrap();
    let bob_spring = k.insert(&teacher, "ReportCard", card(&bob, "bob spring", "Published")).await.unwrap();
    let students = [(id_of(&ann), "Ann"), (id_of(&bob), "Bob")];
    let cards = [(id_of(&ann_spring), "ann spring"), (id_of(&ann_draft), "ann draft"), (id_of(&bob_spring), "bob spring")];
    assert_eq!(reach(&k, &pia, "Student", &students, "name").await, names(&["Ann"]));
    assert_eq!(reach(&k, &pia, "ReportCard", &cards, "term").await, names(&["ann spring"]), "a draft is not published, and Bob is not hers");
    assert_eq!(reach(&k, &teacher, "ReportCard", &cards, "term").await, names(&["ann spring", "ann draft", "bob spring"]));

    // A field in a group the portal role does not name never reaches her.
    let seen = k.get(&pia, "ReportCard", id_of(&ann_spring), false).await.unwrap();
    assert!(!seen.contains_key("comment"), "a staff field shown to a guardian: {seen:?}");
    let listed = k.list(&pia, "Student", &ListQuery::default()).await.unwrap();
    assert!(listed.rows.iter().all(|r| r.get("notes").is_none_or(Value::is_null)), "a staff field listed for a guardian");

    // Reading is all her role gives: no change, no new card, no export.
    assert!(k.update(&pia, "ReportCard", id_of(&ann_spring), obj(json!({"term": "rewritten"})), None).await.is_err());
    assert!(k.insert(&pia, "ReportCard", card(&ann, "forged", "Published")).await.is_err());
    assert!(pia.require("ReportCard", Action::Export).is_err());

    // Her action runs under its own condition, on her records only.
    assert_eq!(k.run_action(&pia, "ReportCard", id_of(&ann_spring), "acknowledge", json!({})).await.unwrap()["done"], json!("acknowledge"));
    assert!(k.run_action(&pia, "ReportCard", id_of(&bob_spring), "acknowledge", json!({})).await.is_err(), "Bob's card is not hers");
    let draft = k.run_action(&pia, "ReportCard", id_of(&ann_draft), "acknowledge", json!({})).await.expect_err("a draft is outside the rule");
    assert!(matches!(draft, Error::NotFound(_)), "{draft}");

    // The administrator's explanation names the rule's path.
    let why = k.explain_access(&admin, pia_id, "ReportCard", Action::Read, Some(id_of(&ann_spring)), None).await.unwrap();
    assert_eq!(why["allowed"], json!(true), "{why}");
    assert_eq!(why["record"]["via"], json!("student.guardians.email"), "{why}");
    assert!(why["reason"].as_str().unwrap().contains("student.guardians.email"), "{why}");
    let not = k.explain_access(&admin, pia_id, "ReportCard", Action::Read, Some(id_of(&bob_spring)), None).await.unwrap();
    assert_eq!(not["allowed"], json!(false), "{not}");
    assert!(not["rules"].as_array().unwrap().iter().any(|r| r["via"] == json!("student.guardians.email")), "the rules shown carry their paths: {not}");

    // The record is hers; what staff say, attach and tag around it is not, nor its history.
    let card_id = id_of(&ann_spring);
    k.add_comment(&teacher, "ReportCard", card_id, "a quiet term", &[]).await.unwrap();
    assert_eq!(k.comments(&teacher, "ReportCard", card_id).await.unwrap().len(), 1, "staff read their comments");
    for (what, refused) in [
        ("comments", k.comments(&pia, "ReportCard", card_id).await.err()),
        ("files", k.attachments(&pia, "ReportCard", card_id).await.err()),
        ("people working on it", k.assignments(&pia, "ReportCard", card_id).await.err()),
        ("tags", k.tags(&pia, "ReportCard", card_id).await.err()),
        ("tag counts", k.tag_counts(&pia, "ReportCard").await.err()),
        ("history", k.audit_trail(&pia, "ReportCard", card_id).await.err()),
    ] {
        assert!(matches!(refused, Some(Error::Forbidden(_))), "a guardian reached a record's {what}: {refused:?}");
    }
    // The portal's generic page lists what her portal roles reach, and nothing for staff.
    let shared: Vec<Value> = k.portal_entities(&pia).await.unwrap();
    assert_eq!(shared.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), ["ReportCard", "Student"]);
    assert!(k.portal_entities(&teacher).await.unwrap().is_empty(), "a member holding no portal role has no portal records");

    // Taken off Ann, she loses Ann's records at once: nothing kept from before.
    k.update(&teacher, "Student", id_of(&ann), obj(json!({"guardians": [{"email": "dad@home.test"}]})), None).await.unwrap();
    assert!(reach(&k, &pia, "ReportCard", &cards, "term").await.is_empty(), "a guardian taken off a student still reaches the student's cards");
    assert!(reach(&k, &pia, "Student", &students, "name").await.is_empty());

    // ───────────── a patient books for themselves, and no one else ─────────────
    let mia = k.insert(&teacher, "Patient", obj(json!({"name": "Mia", "email": "mia@home.test"}))).await.unwrap();
    let ned = k.insert(&teacher, "Patient", obj(json!({"name": "Ned", "email": "ned@home.test"}))).await.unwrap();
    let mia_ctx = portal_person(&k, &admin, "mia@home.test", &["Patients"]).await;
    let mine = k.insert(&mia_ctx, "Appointment", obj(json!({"patient": id_of(&mia).to_string(), "reason": "check-up"}))).await.expect("booked for herself");
    let theirs = k.insert(&mia_ctx, "Appointment", obj(json!({"patient": id_of(&ned).to_string(), "reason": "on his behalf"}))).await;
    assert!(matches!(theirs, Err(Error::Forbidden(_))), "booked for someone else: {theirs:?}");
    let moved = k.update(&mia_ctx, "Appointment", id_of(&mine), obj(json!({"patient": id_of(&ned).to_string()})), None).await;
    assert!(matches!(moved, Err(Error::Forbidden(_))), "moved her appointment onto someone else: {moved:?}");
    k.update(&mia_ctx, "Appointment", id_of(&mine), obj(json!({"reason": "follow-up"})), None).await.expect("her own, changed");
    let neds = k.insert(&teacher, "Appointment", obj(json!({"patient": id_of(&ned).to_string(), "reason": "his own"}))).await.unwrap();
    let appts = [(id_of(&mine), "follow-up"), (id_of(&neds), "his own")];
    assert_eq!(reach(&k, &mia_ctx, "Appointment", &appts, "reason").await, names(&["follow-up"]));
    assert!(k.update(&mia_ctx, "Appointment", id_of(&neds), obj(json!({"reason": "mine now"})), None).await.is_err(), "changed someone else's");
    assert_eq!(reach(&k, &mia_ctx, "Patient", &[(id_of(&mia), "Mia"), (id_of(&ned), "Ned")], "name").await, names(&["Mia"]));
    // Someone's appointments are not theirs because they made them: only the path counts.
    let staff_made = k.insert(&teacher, "Appointment", obj(json!({"patient": id_of(&mia).to_string(), "reason": "staff booked"}))).await.unwrap();
    assert!(k.get(&mia_ctx, "Appointment", id_of(&staff_made), false).await.is_ok(), "her appointment, whoever booked it");

    // ───────────── a role naming an action runs it without the right to edit ─────────────
    let named =
        k.set_role_permission(&admin, "Bookers", &rule(json!({"entity": "Appointment", "read": true, "via": "patient.email", "actions": ["vanish"]}))).await;
    assert!(matches!(named, Err(Error::Validation(_))), "a rule naming an action the entity does not have: {named:?}");
    k.set_role_permission(&admin, "Bookers", &rule(json!({"entity": "Appointment", "read": true, "via": "patient.email", "actions": ["cancel_it"]})))
        .await
        .unwrap();
    k.set_role_portal(&admin, "Bookers", true).await.unwrap();
    let nia = k.insert(&teacher, "Patient", obj(json!({"name": "Nia", "email": "nia@home.test"}))).await.unwrap();
    let nia_ctx = portal_person(&k, &admin, "nia@home.test", &["Bookers"]).await;
    assert_eq!(ferrum_kernel::describe::permissions(&nia_ctx, "Appointment")["actions"], json!(["cancel_it"]), "the interface learns the actions she may run");
    let hers = k.insert(&teacher, "Appointment", obj(json!({"patient": id_of(&nia).to_string(), "reason": "check-up"}))).await.unwrap();
    let locked = k.insert(&teacher, "Appointment", obj(json!({"patient": id_of(&nia).to_string(), "reason": "locked"}))).await.unwrap();
    assert!(k.update(&nia_ctx, "Appointment", id_of(&hers), obj(json!({"reason": "edited"})), None).await.is_err(), "naming an action gives no right to edit");
    assert!(k.run_action(&nia_ctx, "Appointment", id_of(&hers), "reschedule", json!({})).await.is_err(), "an action no rule names needs its own permission");
    assert!(k.run_action(&nia_ctx, "Appointment", id_of(&neds), "cancel_it", json!({})).await.is_err(), "someone else's appointment");
    assert!(
        matches!(k.run_action(&nia_ctx, "Appointment", id_of(&locked), "cancel_it", json!({})).await, Err(Error::Invalid(_))),
        "the action's condition still holds"
    );
    k.run_action(&nia_ctx, "Appointment", id_of(&hers), "cancel_it", json!({})).await.expect("her own, cancelled");
    assert_eq!(k.get(&teacher, "Appointment", id_of(&hers), false).await.unwrap()["reason"], json!("cancelled"));

    // ───────────── another workspace with the same emails reaches nothing here ─────────────
    let (other, other_admin) = workspace(&k, "elsewhere").await;
    k.set_role_permission(&other_admin, "Family", &rule(json!({"entity": "ReportCard", "read": true, "via": "student.guardians.email"}))).await.unwrap();
    k.set_role_portal(&other_admin, "Family", true).await.unwrap();
    let there = k.insert(&other_admin, "Student", obj(json!({"name": "Cal", "guardians": [{"email": "pia@home.test"}]}))).await.unwrap();
    let cal_card = k.insert(&other_admin, "ReportCard", card(&there, "cal spring", "Published")).await.unwrap();
    let pia_there = portal_person(&k, &other_admin, "pia@home.test", &["Family"]).await;
    assert_eq!(pia_there.tenant, other);
    let here = [(id_of(&ann_spring), "ann spring"), (id_of(&bob_spring), "bob spring"), (id_of(&cal_card), "cal spring")];
    assert_eq!(reach(&k, &pia_there, "ReportCard", &here, "term").await, names(&["cal spring"]), "her records there, and none of here");
    assert!(reach(&k, &pia, "ReportCard", &here, "term").await.is_empty(), "nothing of the other workspace's reaches her here");

    transitions(&k, &admin).await;
    files(&k, &admin).await;
}

/// Who takes a workflow transition: one naming roles, a holder of one who can read the record; one
/// naming none, whoever may change the record; an outside person, only one naming their role.
async fn transitions(k: &Kernel, admin: &Ctx) {
    let workflow: ferrum_meta::WorkflowDef = serde_json::from_value(json!({
        "name": "Applications", "entity": "Application", "initial_state": "Draft",
        "states": [{"name": "Draft"}, {"name": "Sent"}, {"name": "Approved"}, {"name": "Reviewed"}],
        "transitions": [
            {"action": "Send", "from": "Draft", "to": "Sent"},
            // Its author forgot the roles: still nobody's who may only read it, nor an applicant's.
            {"action": "Approve", "from": "Sent", "to": "Approved"},
            {"action": "Withdraw", "from": "Sent", "to": "Draft", "roles": ["Applicants"]},
            {"action": "Review", "from": "Sent", "to": "Reviewed", "roles": ["Reviewer"]},
        ],
    }))
    .unwrap();
    k.add_customization(admin, "Application", ferrum_meta::OverlayOp::SetWorkflow { workflow }, None).await.unwrap();
    for (role, r) in [
        ("Viewer", json!({"entity": "Application", "read": true})),
        ("Reviewer", json!({"entity": "Application", "read": true})),
        ("Editor", json!({"entity": "Application", "read": true, "write": true, "create": true})),
        ("Applicants", json!({"entity": "Application", "read": true, "write": true, "create": true, "via": "email"})),
        ("Applicants", json!({"entity": "Claim", "read": true, "write": true, "create": true, "submit": true, "via": "email"})),
        ("Editor", json!({"entity": "Claim", "read": true, "write": true, "create": true, "submit": true, "cancel": true})),
    ] {
        k.set_role_permission(admin, role, &rule(r)).await.unwrap();
    }
    k.set_role_portal(admin, "Applicants", true).await.unwrap();
    let member = |email: &'static str, role: &'static str| async move {
        let id = k.create_user(admin, email, email, None, &[role.to_string()], false).await.unwrap();
        k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
    };
    let (viewer, reviewer, editor) =
        (member("viewer@school.test", "Viewer").await, member("rev@school.test", "Reviewer").await, member("ed@school.test", "Editor").await);
    let ava = portal_person(k, admin, "ava@home.test", &["Applicants"]).await;
    let hers = id_of(&k.insert(&ava, "Application", obj(json!({"title": "Ava's", "email": "ava@home.test"}))).await.expect("her own application"));
    let other = id_of(&k.insert(&editor, "Application", obj(json!({"title": "Someone's", "email": "sam@home.test"}))).await.unwrap());
    let forbidden = |r: Result<Doc>, why: &str| assert!(matches!(r, Err(Error::Forbidden(_))), "{why}: {r:?}");

    forbidden(k.transition(&viewer, "Application", hers, "Send", None).await, "a transition naming no roles, by someone who may only read");
    forbidden(k.transition(&ava, "Application", hers, "Send", None).await, "a transition naming no roles, by an outside person on her own record");
    k.transition(&editor, "Application", hers, "Send", None).await.expect("whoever may change the record");
    forbidden(k.transition(&ava, "Application", hers, "Approve", None).await, "an applicant approving her own application");
    let theirs = k.transition(&ava, "Application", other, "Withdraw", None).await;
    assert!(matches!(theirs, Err(Error::NotFound(_))), "someone else's application: {theirs:?}");
    k.transition(&ava, "Application", hers, "Withdraw", None).await.expect("a transition naming her portal role, on her own record");
    k.transition(&editor, "Application", hers, "Send", None).await.unwrap();
    forbidden(k.transition(&viewer, "Application", hers, "Review", None).await, "a named role she does not hold");
    k.transition(&reviewer, "Application", hers, "Review", None).await.expect("a holder of the named role who may only read");
    k.transition(&editor, "Application", other, "Send", None).await.unwrap();
    k.transition(&editor, "Application", other, "Approve", None).await.expect("an editor takes a transition naming no roles");

    // The form offers what the server would take: each transition says whether the caller may.
    let may = |ctx: &Ctx| -> Vec<(String, bool)> {
        let d = ferrum_kernel::describe::describe(ctx, ctx.schema.entity("Application").unwrap());
        d["workflow"]["transitions"].as_array().unwrap().iter().map(|t| (t["action"].as_str().unwrap().to_string(), t["may_take"] == json!(true))).collect()
    };
    let expect = |v: [bool; 4]| ["Send", "Approve", "Withdraw", "Review"].iter().map(|a| a.to_string()).zip(v).collect::<Vec<_>>();
    assert_eq!(may(&viewer), expect([false, false, false, false]), "viewer");
    assert_eq!(may(&reviewer), expect([false, false, false, true]), "reviewer");
    assert_eq!(may(&editor), expect([true, true, false, false]), "editor");
    assert_eq!(may(&ava), expect([false, false, true, false]), "applicant");
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    assert_eq!(may(&admin), expect([true, true, true, true]), "administrator");

    // Submitting and cancelling are as BD-02 has them: her own claim she submits, and cancels none.
    let claim = id_of(&k.insert(&ava, "Claim", obj(json!({"title": "Ava's claim", "email": "ava@home.test"}))).await.unwrap());
    k.submit(&ava, "Claim", claim, None).await.expect("she submits her own");
    forbidden(k.cancel(&ava, "Claim", claim, None).await, "an outside person cancelling");
    k.cancel(&editor, "Claim", claim, None).await.expect("a member who may cancel");
}

/// Objects kept in memory, as the server's storage would keep them.
#[derive(Default)]
struct MemFiles(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

#[async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// A file tied to no record is its uploader's (and an administrator's) until a record they save names it in an
/// `attach` field: then it is that record's, and its readers', and nobody names a file they could not download.
async fn files(k: &Kernel, admin: &Ctx) {
    k.set_file_store(Arc::new(MemFiles::default()));
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    let ctx_of = |email: &str| {
        let email = email.to_string();
        async move {
            let id: Uuid = sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2")
                .bind(admin.tenant)
                .bind(&email)
                .fetch_one(&mut *k.db.tenant_tx(admin.tenant).await.unwrap())
                .await
                .unwrap();
            k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
        }
    };
    let (viewer, editor, ava) = (ctx_of("viewer@school.test").await, ctx_of("ed@school.test").await, ctx_of("ava@home.test").await);
    let stranger = {
        let id = k.create_user(&admin, "stranger@school.test", "Stranger", None, &[], false).await.unwrap();
        k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
    };
    let loose = |ctx: &Ctx, name: &str| {
        let (ctx, name) = (ctx.clone(), name.to_string());
        async move { Uuid::parse_str(k.store_file(&ctx, None, &name, "text/plain", b"a cv").await.unwrap()["id"].as_str().unwrap()).unwrap() }
    };
    let cv = loose(&editor, "cv.txt").await;
    k.file_info(&editor, cv).await.expect("its uploader");
    k.file_info(&admin, cv).await.expect("an administrator");
    for (who, ctx) in [("another member", &viewer), ("an outside person", &ava)] {
        let r = k.file_info(ctx, cv).await;
        assert!(matches!(r, Err(Error::NotFound(_))), "{who} downloaded a file tied to no record: {:?}", r.map(|f| f.name));
    }

    // Saved in a record's attach field, it is the record's: its readers download it, others do not.
    let app = id_of(&k.insert(&editor, "Application", obj(json!({"title": "With a cv", "cv": format!("/files/{cv}")}))).await.expect("named by its uploader"));
    k.file_info(&viewer, cv).await.expect("a reader of the record it is tied to");
    assert!(k.file_info(&stranger, cv).await.is_err(), "someone who cannot read the record");
    assert!(k.attachments(&viewer, "Application", app).await.unwrap().iter().any(|f| f["id"] == json!(cv)), "listed with the record's files");
    // Naming a file the saver could not download is refused: an id is no way to read a file.
    let viewers = loose(&viewer, "private.txt").await;
    let named = k.update(&editor, "Application", app, obj(json!({"cv": format!("/files/{viewers}")})), None).await;
    assert!(matches!(named, Err(Error::Forbidden(_))), "someone else's loose file named: {named:?}");
    assert!(k.file_info(&editor, viewers).await.is_err(), "and it stays its uploader's");
    let theirs = k.insert(&ava, "Application", obj(json!({"title": "Hers", "email": "ava@home.test", "cv": format!("/files/{cv}")}))).await;
    assert!(matches!(theirs, Err(Error::Forbidden(_))), "a file tied to a record she cannot read: {theirs:?}");
    // A file tied to a record the saver can read may be named again (a copy of the record), and stays where it is.
    k.insert(&editor, "Application", obj(json!({"title": "A copy", "cv": format!("/files/{cv}")}))).await.expect("a file she can download");
    // An outside person's own upload goes with her own record.
    let hers = loose(&ava, "mine.txt").await;
    let app = id_of(&k.insert(&ava, "Application", obj(json!({"title": "Hers", "email": "ava@home.test", "cv": format!("/files/{hers}")}))).await.unwrap());
    k.file_info(&ava, hers).await.expect("she reads her own record's file");
    k.file_info(&editor, hers).await.expect("and so do its other readers");
    assert!(k.attachments(&editor, "Application", app).await.unwrap().iter().any(|f| f["id"] == json!(hers)));
}
