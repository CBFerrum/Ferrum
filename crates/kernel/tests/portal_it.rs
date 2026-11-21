//! Portal roles (docs/builder/decisions.md BD-02, BF-PORTAL-001 to 003, 005) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). A guardian and a patient are given
//! portal roles an administrator configures, with no app code, and the test tries every way past them:
//! a record of someone else's, one moved out of reach, a field in a group, an action outside its
//! condition, a role a portal person may not hold, another workspace with the same email, and a
//! guardian taken off a student. Lists and single records must agree throughout.

use async_trait::async_trait;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Action, Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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
    - {name: docs, type: table, of: ApplicationDoc}
- name: ApplicationDoc
  label: Application Document
  kind: child
  fields:
    - {name: file, type: attach}
- name: Visit
  label: Visit
  kind: master
  title_field: reason
  fields:
    - {name: patient, type: link, to: Patient, required: true}
    - {name: reason, type: data, required: true}
  actions:
    - {name: confirm, label: Confirm, permission: read, roles: [Patients], effects: [{type: set_fields, values: {reason: "'confirmed'"}}]}
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
    // A visit's workflow: its roles are a record's roles, which a role every portal person holds may fill (BD-89).
    std::fs::write(
        dir.join("school/package.yaml"),
        "package: school\nversion: 0.1.0\nlabel: School\ndepends: [core]\nworkflows:\n  - name: Visit flow\n    entity: Visit\n    initial_state: Open\n    \
         states:\n      - {name: Open, edit_roles: [Patients]}\n      - {name: Seen}\n    transitions:\n      - {action: see, from: Open, to: Seen, roles: [Patients]}\n",
    )
    .unwrap();
    std::fs::write(dir.join("school/entities/school.yaml"), ENTITIES).unwrap();
    // A package whose customer role every portal person holds (installed by the test that needs it).
    std::fs::create_dir_all(dir.join("clinic")).unwrap();
    std::fs::write(
        dir.join("clinic/package.yaml"),
        "package: clinic\nversion: 0.1.0\nlabel: Clinic\ndepends: [school]\nroles:\n  - {name: Visitors, portal: true, portal_default: true}\n",
    )
    .unwrap();
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

/// The tests one at a time: each starts from an empty database.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
    let _one = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f).await.unwrap() });
}

#[test]
fn portal_roles_reach_their_holders_own_records_and_nothing_else() {
    run(scenario());
}

#[test]
fn portal_default_roles_reach_only_own_records() {
    run(defaults_reach());
}

#[test]
fn portal_default_refused_on_member_role() {
    run(default_refused());
}

#[test]
fn a_package_never_marks_again_what_an_administrator_unmarked() {
    run(unmarked_stays());
}

/// Install, update and re-enable leave a role's "every portal person holds it" mark as the workspace has it
/// (S4-P4 review M4).
async fn unmarked_stays() {
    let (k, tenant, admin) = school().await;
    k.install_packages(tenant, &["clinic".into()]).await.unwrap();
    let marked = |k: Arc<Kernel>| async move {
        let mut tx = k.db.system_tx().await.unwrap();
        let m: bool =
            sqlx::query_scalar("select portal_default from role where tenant_id = $1 and name = 'Visitors'").bind(tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        m
    };
    assert!(marked(k.clone()).await, "the package's mark on install");
    k.set_role_portal_default(&admin, "Visitors", false).await.unwrap();
    k.install_packages(tenant, &["clinic".into()]).await.unwrap();
    assert!(!marked(k.clone()).await, "installed again: still unmarked");
}

#[test]
fn a_pack_carries_the_default_mark_and_applies_it_only_at_a_screen() {
    run(pack_mark());
}

/// A pack built from a workspace carries the mark; installing it applies the mark only for an administrator at a screen
/// (S4-P4 review L8).
async fn pack_mark() {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    k.set_role_portal_default(&admin, "Patients", true).await.unwrap();
    let options = ferrum_kernel::pack::ExportOptions { name: "patients".into(), label: "Patients".into(), version: 1, records: vec![] };
    let pack = k.export_pack(&admin, &options).await.unwrap();
    let (elsewhere, other) = workspace(&k, "elsewhere").await;
    let screen = Ctx { made: ferrum_kernel::ctx::Made::Screen, ..other.clone() };
    let digest = k.plan_pack(&screen, &pack).await.unwrap()["digest"].as_str().unwrap().to_string();
    let e = k.install_pack(&other, &pack, &digest).await.expect_err("not at a screen");
    assert!(e.to_string().contains("screen"), "{e}");
    k.install_pack(&screen, &pack, &digest).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let marked: bool =
        sqlx::query_scalar("select portal_default from role where tenant_id = $1 and name = 'Patients'").bind(elsewhere).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(marked, "carried and applied");
}

#[test]
fn a_default_role_is_named_by_no_conflict_or_recipient() {
    run(default_unnamed());
}

/// A role every portal person holds is in no conflict rule and is no `role:` recipient (refused when saved, and when a
/// message or notice is sent); the evidence pack and a person's data copy say it is held by every portal person
/// (S4-P4-fix-2, the lead's answer 3).
async fn default_unnamed() {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    k.set_role_permission(&admin, "Staff", &rule(json!({"entity": "Patient", "read": true}))).await.unwrap();
    k.set_role_conflict(&admin, "Patients", "Staff", "kept apart").await.unwrap();
    let e = k.set_role_portal_default(&admin, "Patients", true).await.expect_err("a conflict rule names it");
    assert!(e.to_string().contains("every portal person"), "{e}");
    k.delete_role_conflict(&admin, "Patients", "Staff").await.unwrap();
    k.set_role_portal_default(&admin, "Patients", true).await.unwrap();
    let e = k.set_role_conflict(&admin, "Staff", "Patients", "kept apart").await.expect_err("names a default role");
    assert!(e.to_string().contains("every portal person"), "{e}");
    let notify = obj(json!({"name": "Tell", "entity": "Patient", "event": "created",
        "actions": [{"type": "notify", "to": "role:Patients", "title": "'new'"}]}));
    let e = ferrum_kernel::automation::save(&k, &admin, &notify).await.expect_err("a recipient naming a default role");
    assert!(e.to_string().contains("every portal person"), "{e}");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let e = ferrum_kernel::messages::recipients(&k, &mut tx, &admin, "Patient", &json!({}), "role:Patients").await.expect_err("refused when sent");
    tx.commit().await.unwrap();
    assert!(e.to_string().contains("every portal person"), "{e}");
    let _ann = portal_person(&k, &admin, "ann@x.test", &[]).await;
    let pack = k.evidence_pack(&admin).await.unwrap().to_string();
    assert!(pack.contains("held_by_every_portal_person") && pack.contains("Patients"), "the evidence pack says so");
    let copy = k.personal_data(&admin, "ann@x.test").await.unwrap();
    assert!(copy.to_string().contains("held_by_every_portal_person"), "the data copy says so: {copy}");
}

#[test]
fn a_members_customer_self_owns_nothing_it_made_as_staff() {
    run(made_as_staff());
}

/// A rule reaching "own" records (`if_owner`): a member who is also a customer owns, on the site, what names their
/// address, never what they created as staff (S4-P4 review M3); a portal person still owns what they made.
async fn made_as_staff() {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "if_owner": true}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    k.set_role_portal_default(&admin, "Patients", true).await.unwrap();
    let mut all = vec![];
    for (name, email) in [("Ann", "ann@x.test"), ("Bob", "bob@x.test")] {
        all.push((id_of(&k.insert(&admin, "Patient", obj(json!({"name": name, "email": email}))).await.unwrap()), name));
    }
    let me = k.customer_ctx(admin.tenant, admin.user, None, "it".into(), None).await.unwrap();
    assert_eq!(reach(&k, &me, "Patient", &all, "name").await, names(&[]), "made as staff: not the customer's");
    let ann = portal_person(&k, &admin, "ann@x.test", &[]).await;
    let own = id_of(&k.insert(&admin, "Patient", obj(json!({"name": "Ann's own", "email": "other@x.test"}))).await.unwrap());
    let table = admin.schema.entity("Patient").unwrap().table.clone();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("update {table} set created_by = $1 where id = $2"))).bind(ann.user).bind(own).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    all.push((own, "Ann's own"));
    assert_eq!(reach(&k, &ann, "Patient", &all, "name").await, names(&["Ann's own"]), "a portal person owns what they made");
}

/// An empty database and the school workspace.
async fn school() -> (Arc<Kernel>, Uuid, Ctx) {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let (tenant, admin) = workspace(&k, "school").await;
    (k, tenant, admin)
}

/// A role every portal person holds without being given it (BD-89): each reaches their own records, not each other's,
/// and a member's customer self reaches only theirs too.
async fn defaults_reach() {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    let mut all = vec![];
    for (name, email) in [("Ann", "ann@x.test"), ("Bob", "bob@x.test"), ("Admin", "admin@school.test")] {
        all.push((id_of(&k.insert(&admin, "Patient", obj(json!({"name": name, "email": email}))).await.unwrap()), name));
    }
    let ann = portal_person(&k, &admin, "ann@x.test", &[]).await;
    let bob = portal_person(&k, &admin, "bob@x.test", &[]).await;
    let refused = |r: Result<ferrum_kernel::Page>| matches!(r, Err(Error::Forbidden(_)));
    assert!(refused(k.list(&ann, "Patient", &ListQuery::default()).await), "not held before it is a default");
    k.set_role_portal_default(&admin, "Patients", true).await.unwrap();
    let ann = k.load_ctx(admin.tenant, ann.user, None, "it".into()).await.unwrap();
    let bob = k.load_ctx(admin.tenant, bob.user, None, "it".into()).await.unwrap();
    assert_eq!((ann.roles.clone(), ann.default_roles.clone()), (vec![], vec!["Patients".to_string()]), "held without being given");
    assert_eq!(reach(&k, &ann, "Patient", &all, "name").await, names(&["Ann"]));
    assert_eq!(reach(&k, &bob, "Patient", &all, "name").await, names(&["Bob"]));
    // A member as a customer (a site session, BD-89): outside, no administrator, the portal roles alone.
    let me = k.customer_ctx(admin.tenant, admin.user, None, "it".into(), None).await.unwrap();
    assert!(me.outside && !me.is_admin && me.roles.is_empty() && me.default_roles == vec!["Patients".to_string()], "{:?}", me.default_roles);
    assert_eq!(reach(&k, &me, "Patient", &all, "name").await, names(&["Admin"]));
    // Unmarked as a portal role, it is no default either.
    k.set_role_portal(&admin, "Patients", false).await.unwrap();
    let ann = k.load_ctx(admin.tenant, ann.user, None, "it".into()).await.unwrap();
    assert!(refused(k.list(&ann, "Patient", &ListQuery::default()).await), "no longer held");
}

/// Only a portal role may be held by every portal person: refused for a member's role, by an administrator only, and in
/// a package.
async fn default_refused() {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Staff", &rule(json!({"entity": "Patient", "read": true}))).await.unwrap();
    let e = k.set_role_portal_default(&admin, "Staff", true).await.expect_err("not a portal role");
    assert!(e.to_string().contains("portal role"), "{e}");
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    let ann = portal_person(&k, &admin, "ann@x.test", &[]).await;
    assert!(matches!(k.set_role_portal_default(&ann, "Patients", true).await, Err(Error::Forbidden(_))), "administrators only");
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    files.insert("package.yaml".into(), "package: clinic\nversion: 0.1.0\ndepends: [core]\nroles:\n  - {name: Clinic Staff, portal_default: true}\n".into());
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).unwrap();
    let e = ferrum_meta::catalog::load_package_files(&files).and_then(|p| Catalog::from_packages(vec![core, p])).map(|_| ()).expect_err("refused in a package");
    assert!(e.to_string().contains("portal role"), "{e}");
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
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

    // ───────────── the workspace's menus marked for the portal (docs/builder/portal.md) ─────────────
    let menu = |name: &str, portal: bool, items: Value, create: Value| -> NewCustomization {
        serde_json::from_value(json!({"entity": name, "op": {"op": "define_module", "module": {"name": name, "label": format!("Menu {name}"),
            "description": "What is yours", "icon": "heart-pulse", "items": items, "create": create, "portal": portal}}}))
        .unwrap()
    };
    let defined = k
        .batch_customizations(
            &k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap(),
            vec![
                menu("x_care", true, json!(["Patient", "Appointment", "Student"]), json!(["Patient", "Appointment"])),
                menu("x_terms", true, json!(["Term"]), json!([])),
                menu("x_inside", false, json!(["Appointment", "ReportCard"]), json!([])),
            ],
            false,
        )
        .await;
    assert!(defined.is_ok(), "{:?}", defined.err());
    let fresh = |c: &Ctx| k.load_ctx(tenant, c.user, None, "it".into());
    let (mia, pia, nia, teacher) = (fresh(&mia_ctx).await.unwrap(), fresh(&pia).await.unwrap(), fresh(&nia_ctx).await.unwrap(), fresh(&teacher).await.unwrap());
    let view = |v: &Value| -> (Vec<(String, Vec<(String, bool)>)>, Vec<String>) {
        let menus = v["menus"].as_array().unwrap().iter().map(|m| {
            let items = m["items"].as_array().unwrap().iter().map(|i| (i["name"].as_str().unwrap().to_string(), i["create"].as_bool().unwrap())).collect();
            (m["name"].as_str().unwrap().to_string(), items)
        });
        (menus.collect(), v["other"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect())
    };
    let s = |x: &str| x.to_string();
    // Two of the menu's three; she may book, not add a patient; a menu reaching nothing of hers is not there,
    // nor one for members only.
    let care = k.portal_menus(&mia).await.unwrap();
    assert_eq!(view(&care), (vec![(s("x_care"), vec![(s("Patient"), false), (s("Appointment"), true)])], vec![]), "{care}");
    assert_eq!(
        (&care["menus"][0]["label"], &care["menus"][0]["description"], &care["menus"][0]["icon"]),
        (&json!("Menu x_care"), &json!("What is yours"), &json!("heart-pulse"))
    );
    assert_eq!(care["entities"].as_array().unwrap().len(), 2, "the entities as before, for an older page");
    // Another role, another view: what is in no portal menu is the rest.
    assert_eq!(view(&k.portal_menus(&pia).await.unwrap()), (vec![(s("x_care"), vec![(s("Student"), false)])], vec![s("ReportCard")]));
    assert_eq!(view(&k.portal_menus(&nia).await.unwrap()), (vec![(s("x_care"), vec![(s("Appointment"), false)])], vec![]), "her role may not create");
    // Members: their menus as they were, nothing of the portal's.
    assert_eq!(view(&k.portal_menus(&teacher).await.unwrap()), (vec![], vec![]));
    let ours: Vec<(&str, bool)> = teacher.schema.modules.iter().filter(|m| m.package == "tenant").map(|m| (m.name.as_str(), m.portal)).collect();
    assert_eq!(ours, [("x_care", true), ("x_inside", false), ("x_terms", true)]);
    // The records behind a menu's item are each person's own, by their roles' rules.
    let all =
        [(id_of(&mine), "follow-up"), (id_of(&staff_made), "staff booked"), (id_of(&neds), "his own"), (id_of(&locked), "locked"), (id_of(&hers), "cancelled")];
    assert_eq!(reach(&k, &mia, "Appointment", &all, "reason").await, names(&["follow-up", "staff booked"]));
    assert_eq!(reach(&k, &nia, "Appointment", &all, "reason").await, names(&["locked", "cancelled"]));
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

/// The school with the role Patients every portal person holds (reaching their own patient and visits), Ann holding it
/// only so, Bob given it as well, and one visit of each.
async fn held_by_default() -> (Arc<Kernel>, Ctx, Ctx, Ctx, Uuid, Uuid) {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Visit", "read": true, "write": true, "via": "patient.email"}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    k.set_role_portal_default(&admin, "Patients", true).await.unwrap();
    let mut visits = vec![];
    for (name, email) in [("Ann", "ann@x.test"), ("Bob", "bob@x.test")] {
        let patient = id_of(&k.insert(&admin, "Patient", obj(json!({"name": name, "email": email}))).await.unwrap());
        visits.push(id_of(&k.insert(&admin, "Visit", obj(json!({"patient": patient.to_string(), "reason": name}))).await.unwrap()));
    }
    let ann = portal_person(&k, &admin, "ann@x.test", &[]).await;
    let bob = portal_person(&k, &admin, "bob@x.test", &["Patients"]).await;
    // Ann's two sets are `defaults_reach`'s to check: here each door is.
    assert_eq!(bob.roles, vec!["Patients".to_string()]);
    (k, admin, ann, bob, visits[0], visits[1])
}

#[test]
fn a_default_role_counts_for_a_records_actions_transitions_and_states() {
    run(record_roles());
}

/// Held by default, a role counts for an action's, a transition's and a workflow state's roles on a record the person
/// may reach, and for the portal's own pages (BD-89, second read); never on a record out of reach.
async fn record_roles() {
    let (k, _, ann, bob, anns, bobs) = held_by_default().await;
    k.run_action(&ann, "Visit", anns, "confirm", json!({})).await.expect("an action's role");
    assert!(k.run_action(&ann, "Visit", bobs, "confirm", json!({})).await.is_err(), "not on another's record");
    k.update(&ann, "Visit", anns, obj(json!({"reason": "Ann, again"})), None).await.expect("a state's edit role");
    k.transition(&ann, "Visit", anns, "see", None).await.expect("a transition's role");
    assert!(k.transition(&ann, "Visit", bobs, "see", None).await.is_err(), "not on another's record");
    k.transition(&bob, "Visit", bobs, "see", None).await.expect("given");
    let pages: Vec<String> = k.portal_entities(&ann).await.unwrap().iter().filter_map(|e| e["name"].as_str().map(str::to_string)).collect();
    assert!(pages.contains(&"Visit".to_string()), "the portal's pages: {pages:?}");
}

/// A task and a case of the role Patients, opened in a run of a stand-in solution about `subject` (a visit), or about no
/// record.
async fn tasks_of(k: &Kernel, admin: &Ctx, subject: Option<Uuid>) -> (Uuid, Uuid) {
    let mut tx = k.db.system_tx().await.unwrap();
    let release: Uuid = sqlx::query_scalar(
        "insert into logic_release (tenant_id, solution, version, source_digest, artifact_digest, compiler, profile, source, release) \
         values ($1, 'clinic', $2, 'x', 'x', 'x', 'core', '{}', '{}') returning id",
    )
    .bind(admin.tenant)
    .bind(format!("1.0.{}", Uuid::now_v7().simple()).chars().take(32).collect::<String>())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let run: Uuid = sqlx::query_scalar(
        "insert into logic_run (tenant_id, release_id, solution, process, business_key, trigger, workflow_id, subject_entity, subject_id) \
         values ($1, $2, 'clinic', 'visits', $3, '{}', 'w', case when $4::uuid is null then null else 'Visit' end, $4) returning id",
    )
    .bind(admin.tenant)
    .bind(release)
    .bind(Uuid::now_v7().to_string())
    .bind(subject)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let mut ids = vec![];
    for kind in ["task", "case"] {
        ids.push(
            sqlx::query_scalar::<_, Uuid>(
                "insert into logic_task (tenant_id, run_id, activation, kind, label, role, digest) values ($1, $2, $3, $3, 'Look', 'Patients', 'd') returning id",
            )
            .bind(admin.tenant)
            .bind(run)
            .bind(kind)
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
        );
    }
    tx.commit().await.unwrap();
    (ids[0], ids[1])
}

#[test]
fn a_default_role_decides_no_task_or_case() {
    run(no_tasks());
}

/// A task or a case is decided and seen by those GIVEN its role (BD-89, second read): held by default, a role lists,
/// opens, decides and closes nothing.
async fn no_tasks() {
    let (k, admin, ann, bob, anns, bobs) = held_by_default().await;
    // Each about the record of the person asking, so that the role alone decides.
    let (task, case) = tasks_of(&k, &admin, Some(anns)).await;
    let (btask, bcase) = tasks_of(&k, &admin, Some(bobs)).await;
    let listed = |v: Vec<Value>, id: Uuid| v.iter().any(|t| t["id"] == json!(id));
    assert!(!listed(k.my_logic_tasks(&ann).await.unwrap(), task), "not listed by default");
    assert!(listed(k.my_logic_tasks(&bob).await.unwrap(), btask), "listed when given");
    assert!(matches!(k.logic_task(&ann, task).await, Err(Error::NotFound(_))), "not opened by default");
    k.logic_task(&bob, btask).await.expect("opened when given");
    let e = k.decide_logic_task(&ann, task, true, "stale", &json!({}), "").await.expect_err("not decided by default");
    assert!(matches!(e, Error::Forbidden(_)), "{e}");
    let e = k.decide_logic_task(&bob, btask, true, "stale", &json!({}), "").await.expect_err("past the role, the stale digest");
    assert!(!matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "given: {e}");
    assert!(matches!(k.close_case(&ann, case).await, Err(Error::Forbidden(_))), "a case not closed by default");
    assert!(!matches!(k.close_case(&bob, bcase).await, Err(Error::Forbidden(_) | Error::NotFound(_))), "given: past the role");
}

#[test]
fn a_portal_person_decides_only_tasks_on_records_they_read() {
    run(tasks_as_caller());
}

/// A portal person given a task's role lists, opens, decides, hands on and closes it only when they may read, as
/// themselves, the record it is about; one about no record is the workspace's (task-decide-as-caller).
async fn tasks_as_caller() {
    let (k, admin, _, bob, anns, bobs) = held_by_default().await;
    let (theirs, their_case) = tasks_of(&k, &admin, Some(anns)).await;
    let (nobodys, _) = tasks_of(&k, &admin, None).await;
    let (own, _) = tasks_of(&k, &admin, Some(bobs)).await;
    let listed: Vec<Value> = k.my_logic_tasks(&bob).await.unwrap();
    let ids: Vec<&Value> = listed.iter().map(|t| &t["id"]).collect();
    assert!(ids.contains(&&json!(own)) && !ids.contains(&&json!(theirs)) && !ids.contains(&&json!(nobodys)), "{listed:?}");
    for task in [theirs, nobodys] {
        assert!(matches!(k.logic_task(&bob, task).await, Err(Error::NotFound(_))), "not opened");
        let e = k.decide_logic_task(&bob, task, true, "stale", &json!({}), "").await.expect_err("not decided");
        assert!(matches!(e, Error::NotFound(_)), "{e}");
        let e = k.delegate_logic_task(&bob, task, admin.user, "away").await.expect_err("not handed on");
        assert!(matches!(e, Error::NotFound(_)), "{e}");
    }
    assert!(matches!(k.close_case(&bob, their_case).await, Err(Error::NotFound(_))), "another's case");
    assert!(matches!(k.case_task_done(&bob, their_case, "x", &json!({})).await, Err(Error::NotFound(_))), "another's case's part");
}

#[test]
fn a_default_role_uses_no_connection() {
    run(no_connection());
}

/// A connection's `roles` are roles GIVEN (BD-89, second read).
async fn no_connection() {
    let (k, admin, ann, bob, _, _) = held_by_default().await;
    let input = serde_json::from_value(json!({"name": "clinic", "label": "Clinic", "enabled": true,
        "description": {"base": "https://clinic.example.test", "roles": ["Patients"], "operations": {"ping": {"path": "/ping"}}}}))
    .unwrap();
    k.save_connection_def(&admin, None, &input).await.unwrap();
    let call = ferrum_kernel::connections::Call { connection: "clinic".into(), operation: "nope".into(), ..Default::default() };
    let e = ferrum_kernel::connections::call(&k, &ann, None, &call).await.expect_err("not by default");
    assert!(matches!(e, Error::Forbidden(_)), "{e}");
    let e = ferrum_kernel::connections::call(&k, &bob, None, &call).await.expect_err("given: past the role, no such operation");
    assert!(!matches!(e, Error::Forbidden(_)) && e.to_string().contains("no operation"), "{e}");
}

#[test]
fn a_pack_changes_an_existing_roles_marks_only_at_a_screen() {
    run(pack_marks_existing());
}

/// A pack may bring a NEW role marked for the portal; changing an existing role's portal mark, or any role's default
/// mark, is an administrator's at a screen (BD-89, second read, 3).
async fn pack_marks_existing() {
    let (k, _, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    let options = ferrum_kernel::pack::ExportOptions { name: "patients".into(), label: "Patients".into(), version: 1, records: vec![] };
    let pack = k.export_pack(&admin, &options).await.unwrap();
    let (elsewhere, other) = workspace(&k, "elsewhere").await;
    let screen = Ctx { made: ferrum_kernel::ctx::Made::Screen, ..other.clone() };
    // A new role: a key may bring it marked.
    let digest = k.plan_pack(&screen, &pack).await.unwrap()["digest"].as_str().unwrap().to_string();
    k.install_pack(&other, &pack, &digest).await.expect("a new portal role by a key");
    // The same role, unmarked here: a key may not mark it again.
    k.set_role_portal(&other, "Patients", false).await.unwrap();
    let digest = k.plan_pack(&screen, &pack).await.unwrap()["digest"].as_str().unwrap().to_string();
    let e = k.install_pack(&other, &pack, &digest).await.expect_err("an existing role's portal mark by a key");
    assert!(e.to_string().contains("Patients") && e.to_string().contains("screen"), "{e}");
    let mut tx = k.db.system_tx().await.unwrap();
    let portal: bool =
        sqlx::query_scalar("select portal from role where tenant_id = $1 and name = 'Patients'").bind(elsewhere).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(!portal, "nothing changed");
    k.install_pack(&screen, &pack, &digest).await.expect("at a screen");
    // Held by every portal person here; the pack says not: a key may not clear it either.
    k.set_role_portal_default(&other, "Patients", true).await.unwrap();
    let digest = k.plan_pack(&screen, &pack).await.unwrap()["digest"].as_str().unwrap().to_string();
    let e = k.install_pack(&other, &pack, &digest).await.expect_err("a default mark cleared by a key");
    assert!(e.to_string().contains("Patients"), "{e}");
}

#[test]
fn portal_marks_change_together_and_are_audited() {
    run(marks_together());
}

/// Removing the portal mark records the default mark it clears; the two marks an administrator sets together are one
/// change, both or neither (BD-89, second read, 4).
async fn marks_together() {
    let (k, tenant, admin) = school().await;
    k.set_role_permission(&admin, "Patients", &rule(json!({"entity": "Patient", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal_marks(&admin, "Patients", true, Some(true)).await.unwrap();
    k.set_role_portal(&admin, "Patients", false).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let cleared: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where tenant_id = $1 and entity = 'Role' and action = 'portal_default' and diff = '{\"role\": \"Patients\", \"portal_default\": false}'::jsonb",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(cleared, 1, "the cleared default mark is in the audit trail");
    k.set_role_permission(&admin, "Staff", &rule(json!({"entity": "Patient", "read": true}))).await.unwrap();
    k.set_role_conflict(&admin, "Patients", "Staff", "kept apart").await.unwrap();
    k.set_role_portal_marks(&admin, "Patients", true, Some(true)).await.expect_err("a conflict rule names it");
    let mut tx = k.db.system_tx().await.unwrap();
    let marks: (bool, bool) =
        sqlx::query_as("select portal, portal_default from role where tenant_id = $1 and name = 'Patients'").bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(marks, (false, false), "neither mark");
    // A conflict rule saved while the role is being made one every portal person holds waits for it, and is refused.
    k.delete_role_conflict(&admin, "Patients", "Staff").await.unwrap();
    k.set_role_portal(&admin, "Patients", true).await.unwrap();
    let mut marking = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update role set portal_default = true where tenant_id = $1 and name = 'Patients'").bind(tenant).execute(&mut *marking).await.unwrap();
    let (k2, admin2) = (k.clone(), admin.clone());
    let saving = tokio::spawn(async move { k2.set_role_conflict(&admin2, "Patients", "Staff", "kept apart").await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    marking.commit().await.unwrap();
    let e = saving.await.unwrap().expect_err("the default mark came first");
    assert!(e.to_string().contains("every portal person"), "{e}");
}
