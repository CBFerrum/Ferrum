//! Field groups follow the rule that reaches the record (S9 R13, K0-4), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset by each test, which take their turn). A grouped field shows on a
//! record, and is written there, only when a rule reaching that record grants its group (counting the same role's
//! group-only rows); a line's grouped fields follow the rules reaching its record; a grouped field some reachable record
//! hides is neither filtered nor sorted.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const PACKAGE: &str = r#"package: gp
version: 0.1.0
label: Groups
depends: [core]
roles:
  - name: HrUser
    permissions:
      - {entity: GpEmployee, read: true, write: true, create: true, read_groups: [identity, personal], write_groups: [identity, personal]}
      - {entity: GpGoal, read_groups: [review]}
  - name: SelfService
    permissions:
      - {entity: GpEmployee, read: true, write: true, via: user_email, read_groups: [bank], write_groups: [bank]}
  - name: Payroll
    permissions:
      - {entity: GpEmployee, read: true, read_groups: [bank]}
  - name: Viewer
    permissions:
      - {entity: GpEmployee, read: true}
  - name: SalesLead
    permissions:
      - {entity: GpEmployee, read: true, where: {department: sales}}
      - {entity: GpEmployee, read_groups: [personal]}
      - {entity: GpGoal, read_groups: [review]}
  - name: Snoop
    permissions:
      - {entity: GpEmployee, read_groups: [bank]}
  - name: Editor
    permissions:
      - {entity: GpEmployee, read: true, write: true}
"#;

const ENTITIES: &str = r#"
- name: GpEmployee
  label: Employee
  kind: master
  title_field: full_name
  fields:
    - {name: full_name, type: data, required: true}
    - {name: department, type: data}
    - {name: user_email, type: email}
    - {name: bank_account, type: data, perm_group: bank}
    - {name: id_number, type: data, perm_group: identity}
    - {name: personal_phone, type: data, perm_group: personal}
    - {name: goals, type: table, of: GpGoal}
- name: GpGoal
  label: Goal
  kind: child
  fields:
    - {name: goal, type: data}
    - {name: review_note, type: data, perm_group: review}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-groups-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("gp/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("gp/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("gp/entities/gp.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

struct W {
    k: Arc<Kernel>,
    admin: Ctx,
    /// Asha (sales, asha@gp.test) and Bo (support, bo@gp.test), each with every grouped field and one goal.
    asha: Uuid,
    bo: Uuid,
}

impl W {
    async fn person(&self, email: &str, roles: &[&str]) -> Ctx {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        let u = self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &roles, false).await.unwrap();
        self.k.load_ctx(self.admin.tenant, u, None, "it".into()).await.unwrap()
    }
    async fn read(&self, ctx: &Ctx, id: Uuid) -> Doc {
        self.k.get(ctx, "GpEmployee", id, false).await.unwrap()
    }
}

fn shows(d: &Doc, f: &str) -> bool {
    d.get(f).is_some_and(|v| !v.is_null())
}

fn goal_shows(d: &Doc) -> bool {
    d["goals"].as_array().unwrap().iter().all(|g| g.get("review_note").is_some_and(|v| !v.is_null()))
}

async fn world() -> W {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "gp".into(),
            name: "Groups".into(),
            admin_email: "admin@gp.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["gp".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("gp", "admin@gp.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    let employee = |name: &str, dept: &str, email: &str| {
        obj(json!({"full_name": name, "department": dept, "user_email": email, "bank_account": format!("BANK-{name}"),
            "id_number": format!("ID-{name}"), "personal_phone": "+1 555 0100", "goals": [{"goal": "ship", "review_note": format!("REVIEW-{name}")}]}))
    };
    let asha = k.insert(&admin, "GpEmployee", employee("Asha", "sales", "asha@gp.test")).await.unwrap();
    let bo = k.insert(&admin, "GpEmployee", employee("Bo", "support", "bo@gp.test")).await.unwrap();
    let id = |d: &Doc| d["id"].as_str().unwrap().parse().unwrap();
    W { k, admin, asha: id(&asha), bo: id(&bo) }
}

fn go<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

#[test]
fn a_group_an_own_rule_grants_does_not_show_on_records_another_rule_reaches() {
    go(|| async {
        let w = world().await;
        // Asha works in HR and, like everyone, has self-service: her own bank details, and HR's groups on everyone.
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        let own = w.read(&asha, w.asha).await;
        assert!(shows(&own, "bank_account") && shows(&own, "id_number"), "{own:?}");
        let other = w.read(&asha, w.bo).await;
        assert!(shows(&other, "id_number") && shows(&other, "personal_phone"), "HR's groups everywhere: {other:?}");
        assert!(!shows(&other, "bank_account"), "her own rule's group stays on her own record: {other:?}");
    });
}

#[test]
fn an_hr_user_holding_employee_reads_no_one_else_s_bank_details() {
    go(|| async {
        let w = world().await;
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        let page = w.k.list(&asha, "GpEmployee", &ListQuery { limit: Some(10), ..Default::default() }).await.unwrap();
        for r in &page.rows {
            let mine = r["id"] == json!(w.asha.to_string());
            assert_eq!(shows(r, "bank_account"), mine, "only her own in a list: {r:?}");
        }
        let csv = serde_json::to_string(&page.rows).unwrap();
        assert!(!csv.contains("BANK-Bo"), "{csv}");
    });
}

#[test]
fn a_whole_rule_s_group_still_shows_everywhere() {
    go(|| async {
        let w = world().await;
        let pay = w.person("pay@gp.test", &["Payroll", "SelfService"]).await;
        assert!(shows(&w.read(&pay, w.bo).await, "bank_account") && shows(&w.read(&pay, w.asha).await, "bank_account"));
        let q = ListQuery { filters: vec![ferrum_kernel::Filter::eq("bank_account", "BANK-Bo")], ..Default::default() };
        assert_eq!(w.k.list(&pay, "GpEmployee", &q).await.unwrap().rows.len(), 1, "a whole rule's group filters too");
    });
}

#[test]
fn a_group_only_row_applies_to_its_own_role_s_reach() {
    go(|| async {
        let w = world().await;
        // The sales lead's group-only row adds `personal` to their sales rule; their Viewer rule reads everyone.
        let lead = w.person("lead@gp.test", &["SalesLead", "Viewer"]).await;
        assert!(shows(&w.read(&lead, w.asha).await, "personal_phone"), "a sales employee");
        assert!(!shows(&w.read(&lead, w.bo).await, "personal_phone"), "not support's, which only Viewer reaches");
        // A group-only row of a role that reads nothing of the entity grants nothing.
        let snoop = w.person("snoop@gp.test", &["Snoop", "Viewer"]).await;
        assert!(!shows(&w.read(&snoop, w.bo).await, "bank_account") && !shows(&w.read(&snoop, w.asha).await, "bank_account"));
    });
}

#[test]
fn lines_are_masked_by_the_parent_s_reaching_rules() {
    go(|| async {
        let w = world().await;
        let lead = w.person("lead@gp.test", &["SalesLead", "Viewer"]).await;
        assert!(goal_shows(&w.read(&lead, w.asha).await), "a sales employee's goals: the sales rule's role grants review");
        let bo = w.read(&lead, w.bo).await;
        assert!(!goal_shows(&bo,), "support's goals, reached only by Viewer: {bo:?}");
        assert!(!serde_json::to_string(&bo).unwrap().contains("REVIEW-Bo"));
    });
}

#[test]
fn a_grouped_field_some_reachable_record_hides_is_neither_filtered_nor_sorted() {
    go(|| async {
        let w = world().await;
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        let q = ListQuery { filters: vec![ferrum_kernel::Filter::eq("bank_account", "BANK-Bo")], ..Default::default() };
        assert!(matches!(w.k.list(&asha, "GpEmployee", &q).await, Err(Error::Forbidden(_))), "a filter would name Bo's number");
        let q = ListQuery { sort: Some("bank_account asc".into()), ..Default::default() };
        assert!(matches!(w.k.list(&asha, "GpEmployee", &q).await, Err(Error::Forbidden(_))), "an order would rank it");
        let q = ListQuery { filters: vec![ferrum_kernel::Filter::eq("goals.review_note", "REVIEW-Bo")], ..Default::default() };
        let lead = w.person("lead@gp.test", &["SalesLead", "Viewer"]).await;
        assert!(matches!(w.k.list(&lead, "GpEmployee", &q).await, Err(Error::Forbidden(_))), "a line's grouped field likewise");
    });
}

#[test]
fn a_grouped_field_hidden_on_a_record_is_not_written_there() {
    go(|| async {
        let w = world().await;
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        let e = w.k.update(&asha, "GpEmployee", w.bo, obj(json!({"bank_account": "BANK-MINE"})), None).await.unwrap_err();
        assert!(matches!(&e, Error::Validation(f) if f.iter().any(|f| f.field == "bank_account")), "{e:?}");
        assert_eq!(w.read(&w.admin, w.bo).await["bank_account"], json!("BANK-Bo"));
        w.k.update(&asha, "GpEmployee", w.asha, obj(json!({"bank_account": "BANK-NEW"})), None).await.expect("her own");
        w.k.update(&asha, "GpEmployee", w.bo, obj(json!({"id_number": "ID-2"})), None).await.expect("a group HR's rule grants on Bo");
    });
}

#[test]
fn a_records_key_keeps_its_owner_s_field_hiding() {
    go(|| async {
        let w = world().await;
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        let declared: BTreeMap<String, BTreeSet<String>> =
            [("GpEmployee".to_string(), ["read", "read field group bank"].into_iter().map(String::from).collect())].into();
        let key = Ctx { perms: Arc::new(asha.perms.limited_to(&declared, &BTreeSet::new())), ..asha.clone() };
        assert!(!shows(&w.read(&key, w.bo).await, "bank_account"), "a key narrows; it never widens a group to others' records");
        assert!(shows(&w.read(&key, w.asha).await, "bank_account"));
    });
}

#[test]
fn a_schema_change_reloads_the_cached_rights() {
    go(|| async {
        let w = world().await;
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        w.read(&asha, w.bo).await;
        let field = json!({"op": "add_field", "field": {"name": "x_iban", "type": "data", "perm_group": "bank"}});
        w.k.add_customization(&w.admin, "GpEmployee", serde_json::from_value::<OverlayOp>(field).unwrap(), None).await.unwrap();
        let admin = w.k.load_ctx(w.admin.tenant, w.admin.user, None, "it".into()).await.unwrap();
        w.k.update(&admin, "GpEmployee", w.bo, obj(json!({"x_iban": "IBAN-Bo"})), None).await.unwrap();
        let asha = w.k.load_ctx(asha.tenant, asha.user, None, "it".into()).await.unwrap();
        assert!(!shows(&w.read(&asha, w.bo).await, "x_iban"), "the new grouped field is hidden on others' records at once");
    });
}

#[test]
fn administrators_are_told_of_roles_whose_groups_narrow() {
    go(|| async {
        let w = world().await;
        assert_eq!(w.k.tell_of_narrowed_groups().await.unwrap(), 2, "SalesLead's groups show on its sales records only; Snoop's on none");
        let mut tx = w.k.db.tenant_tx(w.admin.tenant).await.unwrap();
        let said: Vec<(String, String)> =
            sqlx::query_as("select title, body from notification where user_id = $1 order by title").bind(w.admin.user).fetch_all(&mut *tx).await.unwrap();
        tx.rollback().await.unwrap();
        assert!(said.iter().any(|(t, b)| t.contains("SalesLead") && b.contains("only on the GpEmployee records its own rules reach")), "{said:?}");
        assert!(said.iter().any(|(t, b)| t.contains("Snoop") && b.contains("show nothing")), "{said:?}");
        // Once: the next round tells nobody again.
        assert_eq!(w.k.tell_of_narrowed_groups().await.unwrap(), 0);
    });
}

/// What a form sends back: the record as its writer read it, without the system's own keys.
fn as_sent(read: &Doc) -> Doc {
    let system = ["id", "version", "docstatus", "created_at", "updated_at", "created_by", "updated_by", "number", "amended_from", "workflow_state", "_titles"];
    read.iter().filter(|(k, _)| !system.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[test]
fn a_save_by_someone_who_does_not_see_a_field_keeps_its_value() {
    go(|| async {
        let w = world().await;
        let asha = w.person("asha@gp.test", &["HrUser", "SelfService"]).await;
        // A patch of other fields.
        w.k.update(&asha, "GpEmployee", w.bo, obj(json!({"id_number": "ID-Bo-2"})), None).await.unwrap();
        assert_eq!(w.read(&w.admin, w.bo).await["bank_account"], json!("BANK-Bo"));
        // A whole save, as a form sends what it read.
        let read = w.read(&asha, w.bo).await;
        assert!(!shows(&read, "bank_account"));
        let mut sent = as_sent(&read);
        sent.insert("department".into(), json!("support desk"));
        w.k.update(&asha, "GpEmployee", w.bo, sent, None).await.unwrap();
        let now = w.read(&w.admin, w.bo).await;
        assert_eq!((now["bank_account"].clone(), now["department"].clone()), (json!("BANK-Bo"), json!("support desk")), "{now:?}");
    });
}

#[test]
fn a_save_of_lines_by_someone_who_does_not_see_a_line_s_field_keeps_each_line_s_value() {
    go(|| async {
        let w = world().await;
        let goals = json!([{"goal": "one", "review_note": "R1"}, {"goal": "two", "review_note": "R2"}, {"goal": "three", "review_note": "R3"}]);
        w.k.update(&w.admin, "GpEmployee", w.bo, obj(json!({"goals": goals})), None).await.unwrap();
        // The editor reads every employee, and no role of theirs gives `review` on goals.
        let editor = w.person("ed@gp.test", &["Editor"]).await;
        let read = w.read(&editor, w.bo).await;
        assert!(!goal_shows(&read), "{read:?}");
        let mut lines: Vec<Value> = read["goals"].as_array().unwrap().clone();
        // Kept as read, one edited, one removed, one added.
        lines[1]["goal"] = json!("two, sharper");
        lines.remove(2);
        lines.push(json!({"goal": "four"}));
        let mut sent = as_sent(&read);
        sent.insert("goals".into(), Value::Array(lines));
        w.k.update(&editor, "GpEmployee", w.bo, sent, None).await.unwrap();
        let now = w.read(&w.admin, w.bo).await;
        let got: Vec<(Value, Value)> = now["goals"].as_array().unwrap().iter().map(|g| (g["goal"].clone(), g["review_note"].clone())).collect();
        assert_eq!(
            got,
            vec![(json!("one"), json!("R1")), (json!("two, sharper"), json!("R2")), (json!("four"), Value::Null)],
            "each kept line keeps its value; the new one starts empty"
        );
    });
}
