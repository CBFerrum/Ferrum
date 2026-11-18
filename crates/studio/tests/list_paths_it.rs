//! One read path for what a caller chose (docs/guides/05, S9 K0-1), against a real PostgreSQL
//! (`FERRUM_STUDIO_TEST_DATABASE_URL`; the database is reset by each test, which take their turn): a caller cannot
//! filter, sort or group by a field they cannot read, through any path; what leaves the workspace holds only what
//! rules that grant export reach; a preview or a save reads and fetches nothing outside the caller's reach; a query one
//! app asks another answers as the caller.

use ferrum_kernel::capability::Invocation;
use ferrum_kernel::ctx::Egress;
use ferrum_kernel::{Action, Ctx, HookRegistry, ListQuery};
use ferrum_meta::{MetaError, Package};
use ferrum_testkit::{App, Kit, Workspace, database, obj, refused, run, uid};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use uuid::Uuid;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

const LP: &str = "package: lp\nversion: 0.1.0\nlabel: Paths\ndepends: [core]\nroles:\n  - name: LpClerk\n    permissions:\n      - {entity: LpStaff, read: true, write: true, create: true}\n      - {entity: LpUnit, read: true}\n      - {entity: LpStatement, read: true, write: true, create: true}\n      - {entity: LpSeparation, read: true, create: true, if_owner: true}\n  - name: LpLister\n    permissions:\n      - {entity: LpStaff, read: true}\n      - {entity: LpStaff, export: true, where: {team: north}}\n";
const LP_ENTITIES: &str = r#"
- name: LpUnit
  label: Unit
  kind: master
  title_field: unit_name
  fields:
    - {name: unit_name, type: data, required: true}
    - {name: code, type: data, perm_group: bank}
- name: LpStaff
  label: Staff
  kind: master
  title_field: staff_name
  fields:
    - {name: staff_name, type: data, required: true, in_list: true}
    - {name: team, type: data}
    - {name: bank_account, type: data, perm_group: bank}
    - {name: unit, type: link, to: LpUnit}
- name: LpSeparation
  label: Separation
  kind: master
  reference: restricted
  title_field: who
  fields:
    - {name: who, type: data, required: true}
    - {name: relieving_date, type: date}
- name: LpStatement
  label: Statement
  kind: master
  fields:
    - {name: separation, type: link, to: LpSeparation}
    - {name: relieving_date, type: date, read_only: true, fetch_from: separation.relieving_date}
"#;
const CALLER: &str = "package: callr\nversion: 0.1.0\nlabel: Caller\ndepends: [lp]\nuses:\n  - {capability: lp.lp_staff.list, version: '>=0'}\n";

fn load(files: &[(&str, &str)]) -> Result<Vec<Package>, MetaError> {
    let files: BTreeMap<String, String> = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

fn lp_packages() -> Result<Vec<Package>, MetaError> {
    load(&[("package.yaml", LP), ("entities/lp.yaml", LP_ENTITIES)])
}

fn caller_packages() -> Result<Vec<Package>, MetaError> {
    load(&[("package.yaml", CALLER)])
}

fn nothing(_: &mut HookRegistry) {}

fn studio_packages() -> Result<Vec<Package>, MetaError> {
    ferrum_studio::packages()
}

struct World {
    kit: Kit,
    ws: Workspace,
    clerk: Ctx,
    lister: Ctx,
    north: Uuid,
    south: Uuid,
    theirs: Uuid,
    mine: Uuid,
}

/// Two staff (a north one, a south one, each with a bank account), a unit, the administrator's separation and the
/// clerk's own.
async fn world() -> World {
    let apps = [
        App { id: "lp", packages: lp_packages, register: nothing, reports: Vec::new },
        App { id: "callr", packages: caller_packages, register: nothing, reports: Vec::new },
        App { id: "studio", packages: studio_packages, register: ferrum_studio::register, reports: ferrum_studio::reports },
    ];
    let kit = Kit::boot(&database("FERRUM_STUDIO_TEST_DATABASE_URL"), &apps).await;
    let ws = kit.workspace("paths", &["lp", "callr", "studio"], Value::Null).await;
    let k = &kit.k;
    let unit = k.insert(&ws.admin, "LpUnit", obj(json!({"unit_name": "Plant", "code": "SECRET-7"}))).await.unwrap();
    let staff = |name: &str, team: &str, bank: &str| obj(json!({"staff_name": name, "team": team, "bank_account": bank, "unit": uid(&unit).to_string()}));
    let north = uid(&k.insert(&ws.admin, "LpStaff", staff("Asha", "north", "1111-2222")).await.unwrap());
    let south = uid(&k.insert(&ws.admin, "LpStaff", staff("Bo", "south", "9999-8888")).await.unwrap());
    let theirs = uid(&k.insert(&ws.admin, "LpSeparation", obj(json!({"who": "Bo", "relieving_date": "2026-12-31"}))).await.unwrap());
    let clerk = ws.user("clerk@paths.test", &["LpClerk", "Studio User"]).await;
    let lister = ws.user("lister@paths.test", &["LpLister", "Studio User"]).await;
    let mine = uid(&k.insert(&clerk, "LpSeparation", obj(json!({"who": "Me", "relieving_date": "2026-11-30"}))).await.unwrap());
    World { kit, ws, clerk, lister, north, south, theirs, mine }
}

fn go<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    run(f());
}

fn list_of(filters: Value, sort: Option<&str>) -> Value {
    let mut input = json!({"filters": filters});
    if let Some(s) = sort {
        input["sort"] = json!(s);
    }
    input
}

#[test]
fn the_list_capability_refuses_a_filter_on_a_field_the_caller_cannot_read() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        // A character at a time would recover the number: refused at the first.
        let asked = Invocation::new("lp.lp_staff.list", list_of(json!([["bank_account", "like", "1%"]]), None));
        refused(k.invoke(&w.clerk, asked).await, "forbidden", "bank_account");
        // A field they read: answered, and the rows never carry the hidden one.
        let rows = k.invoke(&w.clerk, Invocation::new("lp.lp_staff.list", list_of(json!([["team", "=", "north"]]), None))).await.unwrap().result;
        assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
        assert!(rows[0].get("bank_account").is_none_or(Value::is_null), "{rows}");
        // The administrator, who reads it, filters by it.
        let rows = k.invoke(&w.ws.admin, Invocation::new("lp.lp_staff.list", list_of(json!([["bank_account", "like", "1%"]]), None))).await.unwrap();
        assert_eq!(rows.result.as_array().unwrap().len(), 1);
    });
}

#[test]
fn refuses_a_sort_on_one() {
    go(|| async {
        let w = world().await;
        let asked = Invocation::new("lp.lp_staff.list", list_of(json!([]), Some("bank_account asc")));
        refused(w.kit.k.invoke(&w.clerk, asked).await, "forbidden", "bank_account");
        // The list route says the same.
        let q = ListQuery { sort: Some("bank_account desc".into()), ..Default::default() };
        refused(w.kit.k.list(&w.clerk, "LpStaff", &q).await, "forbidden", "bank_account");
    });
}

#[test]
fn scopes_a_filter_through_a_link() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        // A hidden field of the linked record: refused.
        let asked = Invocation::new("lp.lp_staff.list", list_of(json!([["unit.code", "like", "S%"]]), None));
        refused(k.invoke(&w.clerk, asked).await, "forbidden", "unit.code");
        // Through a restricted link to records the caller does not read: nothing matches, whatever they hold.
        k.insert(&w.ws.admin, "LpStatement", obj(json!({"separation": w.theirs.to_string()}))).await.unwrap();
        let asked = Invocation::new("lp.lp_statement.list", list_of(json!([["separation.relieving_date", "=", "2026-12-31"]]), None));
        assert_eq!(k.invoke(&w.clerk, asked).await.unwrap().result, json!([]), "the other's separation is not theirs to match");
        let asked = Invocation::new("lp.lp_statement.list", list_of(json!([["separation.relieving_date", "=", "2026-12-31"]]), None));
        assert_eq!(k.invoke(&w.ws.admin, asked).await.unwrap().result.as_array().unwrap().len(), 1);
    });
}

#[test]
fn an_export_holds_only_what_rules_with_export_reach() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        let q = ListQuery { limit: Some(100), ..Default::default() };
        let read = k.list(&w.lister, "LpStaff", &q).await.unwrap();
        assert_eq!(read.rows.len(), 2, "they read both");
        let out = k.list_as(&w.lister, "LpStaff", &q, 30_000, Action::Export).await.unwrap();
        let names: Vec<&str> = out.rows.iter().filter_map(|r| r["staff_name"].as_str()).collect();
        assert_eq!(names, ["Asha"], "only the team their export rule names");
        // No export right at all: refused.
        refused(k.list_as(&w.clerk, "LpStaff", &q, 30_000, Action::Export).await, "forbidden", "");
    });
}

#[test]
fn mailing_a_record_needs_export_reach_on_it() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        k.get_for_export(&w.lister, "LpStaff", w.north).await.expect("the north one is theirs to send");
        refused(k.get_for_export(&w.lister, "LpStaff", w.south).await, "not found", "Staff");
        k.get(&w.lister, "LpStaff", w.south, false).await.expect("which they still read");
    });
}

fn spec(v: Value) -> HashMap<String, String> {
    [("spec".to_string(), v.to_string())].into()
}

#[test]
fn a_data_file_document_holds_only_what_rules_with_export_reach() {
    go(|| async {
        let w = world().await;
        let asked = spec(json!({"entity": "LpStaff", "fields": [{"field": "staff_name"}]}));
        let read = ferrum_domain::reports::run(&w.kit.k, &w.lister, "studio_query", &asked).await.unwrap();
        assert_eq!(read["rows"].as_array().unwrap().len(), 2, "{read}");
        // What a document's data file reads (StudioRows with export): only what the export rule reaches.
        let out = ferrum_studio::query::as_export(ferrum_domain::reports::run(&w.kit.k, &w.lister, "studio_query", &asked)).await.unwrap();
        let rows = out["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{out}");
        assert_eq!(rows[0]["staff_name"], json!("Asha"));
    });
}

#[test]
fn a_studio_query_honours_the_data_policy() {
    go(|| async {
        let w = world().await;
        let asked = spec(json!({"entity": "LpStaff", "fields": [{"field": "staff_name"}, {"field": "bank_account"}]}));
        ferrum_domain::reports::run(&w.kit.k, &w.ws.admin, "studio_query", &asked).await.expect("the administrator reads it");
        // What goes on to an AI provider leaves out grouped fields: so does a query run for it.
        let for_provider = w.ws.admin.for_provider(Egress::default());
        refused(ferrum_domain::reports::run(&w.kit.k, &for_provider, "studio_query", &asked).await, "forbidden", "Bank Account");
    });
}

#[test]
fn studio_names_no_linked_record_the_caller_cannot_read() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        k.insert(&w.clerk, "LpStatement", obj(json!({"separation": w.theirs.to_string()}))).await.unwrap();
        k.insert(&w.clerk, "LpStatement", obj(json!({"separation": w.mine.to_string()}))).await.unwrap();
        let asked = spec(json!({"entity": "LpStatement", "fields": [{"field": "separation"}]}));
        let out = ferrum_domain::reports::run(k, &w.clerk, "studio_query", &asked).await.unwrap();
        let titles: Vec<Value> = out["rows"].as_array().unwrap().iter().map(|r| r["_titles"]["separation"].clone()).collect();
        assert!(titles.contains(&json!("Me")), "their own is named: {out}");
        assert!(!titles.contains(&json!("Bo")), "the other's is not: {out}");
    });
}

#[test]
fn a_preview_never_reads_a_record_outside_the_caller_s_reach() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        refused(k.preview(&w.clerk, "LpSeparation", obj(json!({"id": w.theirs.to_string()}))).await, "not found", "Separation");
        let own = k.preview(&w.clerk, "LpSeparation", obj(json!({"id": w.mine.to_string()}))).await.unwrap();
        assert_eq!(own["who"], json!("Me"));
    });
}

#[test]
fn a_preview_never_fetches_a_linked_record_outside_the_caller_s_reach() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        let theirs = k.preview(&w.clerk, "LpStatement", obj(json!({"separation": w.theirs.to_string()}))).await.unwrap();
        assert!(theirs.get("relieving_date").is_none_or(Value::is_null), "nothing fetched from another's: {theirs:?}");
        let mine = k.preview(&w.clerk, "LpStatement", obj(json!({"separation": w.mine.to_string()}))).await.unwrap();
        assert_eq!(mine["relieving_date"], json!("2026-11-30"));
    });
}

#[test]
fn a_save_fetches_nothing_from_a_restricted_record_outside_reach() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        let theirs = k.insert(&w.clerk, "LpStatement", obj(json!({"separation": w.theirs.to_string()}))).await.unwrap();
        assert!(theirs.get("relieving_date").is_none_or(Value::is_null), "{theirs:?}");
        let stored = k.get(&w.ws.admin, "LpStatement", uid(&theirs), false).await.unwrap();
        assert!(stored.get("relieving_date").is_none_or(Value::is_null), "nor stored: {stored:?}");
        let mine = k.insert(&w.clerk, "LpStatement", obj(json!({"separation": w.mine.to_string()}))).await.unwrap();
        assert_eq!(mine["relieving_date"], json!("2026-11-30"));
        // The administrator's save fetches as before.
        let all = k.insert(&w.ws.admin, "LpStatement", obj(json!({"separation": w.theirs.to_string()}))).await.unwrap();
        assert_eq!(all["relieving_date"], json!("2026-12-31"));
    });
}

#[test]
fn a_report_refuses_grouping_by_a_field_the_caller_cannot_read() {
    go(|| async {
        let w = world().await;
        use ferrum_domain::reports::{readable_choice, readable_columns, readable_default};
        refused(readable_choice(&w.clerk, "LpStaff", "bank_account"), "forbidden", "bank_account");
        readable_choice(&w.clerk, "LpStaff", "team").expect("a field they read");
        readable_choice(&w.ws.admin, "LpStaff", "bank_account").expect("the administrator reads it");
        assert_eq!(readable_default(&w.clerk, "LpStaff", &["bank_account", "team"]), Some("team"));
        assert_eq!(readable_default(&w.clerk, "LpStaff", &["bank_account"]), None);
        let cols =
            readable_columns(&w.clerk, "LpStaff", vec![json!({"name": "team"}), json!({"name": "acct", "field": "bank_account"}), json!({"name": "count"})]);
        assert_eq!(cols, vec![json!({"name": "team"}), json!({"name": "count"})]);
        // Studio's grouping says the same.
        let asked = spec(json!({"entity": "LpStaff", "group_by": [{"field": "bank_account"}], "measures": [{"fn": "count"}]}));
        refused(ferrum_domain::reports::run(&w.kit.k, &w.clerk, "studio_query", &asked).await, "forbidden", "Bank Account");
    });
}

#[test]
fn a_query_between_apps_sees_what_the_caller_sees() {
    go(|| async {
        let w = world().await;
        let k = &w.kit.k;
        let asked = Invocation::new("lp.lp_staff.list", list_of(json!([["bank_account", "like", "1%"]]), None));
        let mut tx = k.db.tenant_tx(w.ws.tenant).await.unwrap();
        refused(k.call_tx(&w.kit.vault("callr"), &w.clerk, &mut tx, &asked).await, "forbidden", "bank_account");
        tx.rollback().await.unwrap();
        // Someone who reads none of them gets none of them, though the system could.
        let nobody = w.ws.user("nobody@paths.test", &[]).await;
        let mut tx = k.db.tenant_tx(w.ws.tenant).await.unwrap();
        let asked = Invocation::new("lp.lp_staff.list", list_of(json!([]), None));
        assert!(k.call_tx(&w.kit.vault("callr"), &nobody, &mut tx, &asked).await.is_err(), "the system would have read every staff");
        tx.rollback().await.unwrap();
        let mut tx = k.db.tenant_tx(w.ws.tenant).await.unwrap();
        let rows = k.call_tx(&w.kit.vault("callr"), &w.clerk, &mut tx, &asked).await.unwrap().result;
        tx.rollback().await.unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert!(rows.as_array().unwrap().iter().all(|r| r.get("bank_account").is_none_or(Value::is_null)), "masked as the caller: {rows}");
        let _ = (w.north, w.south);
    });
}
