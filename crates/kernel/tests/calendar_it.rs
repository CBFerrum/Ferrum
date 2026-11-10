//! Business calendars with hours of the day against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset
//! first): the one loader (what an app's hook calls with its own transaction), a process wait counted in open
//! time as the runtime works it out, and the same wait in a simulation on the workspace's calendar.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Calendar, Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings, load_calendar};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const PACKAGE: &str = "package: procs\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Requester\n    permissions:\n      - {entity: Request, read: true, write: true, create: true, submit: true}\n";
const ENTITIES: &str = "- name: Request\n  label: Request\n  kind: document\n  submittable: true\n  title_field: reference\n  fields:\n    - {name: reference, type: data, required: true}\n";

/// A process that waits eight hours of the office's open time, then ends.
fn source(calendar: &str) -> String {
    json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.hold", "version": "1.0.0", "label": "Hold",
        "processes": [{"id": "hold", "label": "Hold a request", "trigger": {"event": "procs.request.submitted", "business_key": "trigger.id"},
            "entry": "settle", "max_steps": 10, "nodes": [
            {"type": "delay", "id": "settle", "label": "Let it settle", "wait": {"working": "PT8H", "calendar": calendar}, "next": "done"},
            {"type": "end", "id": "done", "label": "Done", "outcome": "completed"}
        ]}]
    })
    .to_string()
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["procs".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

fn utc(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

#[test]
fn a_calendar_with_hours_is_loaded_once_and_waits_count_its_open_time() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/procs.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let catalog = Catalog::from_packages(vec![core, ferrum_meta::catalog::load_package_files(&files).expect("package")]).unwrap();
    // Durable processes are the Full Process profile's.
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings { process_runtime: true, ..Default::default() }, None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let other = workspace(&k, "globex").await;

    // Kolkata, Monday to Friday 09:00–13:00 and 14:00–18:00; Tuesday 10 November 2026 is a holiday.
    let day = json!([["09:00", "13:00"], ["14:00", "18:00"]]);
    let body = json!({"zone": "Asia/Kolkata", "weekends": ["sat", "sun"], "holidays": ["2026-11-10"],
        "hours": {"mon": day, "tue": day, "wed": day, "thu": day, "fri": day}});
    assert_eq!(k.put_logic_calendar(&admin, "office", &body).await.unwrap(), 1);

    // The one loader, inside a transaction as a hook has one; another workspace does not see it.
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let office: Calendar = load_calendar(&mut tx, admin.tenant, "office").await.unwrap().expect("the office calendar");
    assert!(load_calendar(&mut tx, admin.tenant, "mars").await.unwrap().is_none());
    tx.commit().await.unwrap();
    assert_eq!((office.revision, office.hours.as_ref().map(|h| h.len())), (1, Some(5)));
    assert_eq!(serde_json::to_value(&office.hours).unwrap(), body["hours"]);
    let mut tx = k.db.tenant_tx(other.tenant).await.unwrap();
    assert!(load_calendar(&mut tx, admin.tenant, "office").await.unwrap().is_none(), "another workspace's calendar");
    tx.commit().await.unwrap();

    // A wait of eight working hours from Monday 17:00: an hour on Monday, Tuesday a holiday, seven on Wednesday.
    let wait = json!({"working": "PT8H", "calendar": "office"});
    let from = utc("2026-11-09T11:30:00Z");
    let d = k.process_deadline(admin.tenant, &wait, from).await.unwrap();
    assert_eq!((d.at, d.calendar.as_deref(), d.revision, d.zone.as_deref()), (utc("2026-11-11T11:30:00Z"), Some("office"), Some(1), Some("Asia/Kolkata")));
    assert_eq!(d.at, office.add_working(from, 8 * 3600, &[]).unwrap());
    assert!(k.process_deadline(other.tenant, &wait, from).await.is_err(), "a calendar the workspace does not have");

    // The simulation counts the same open time on the workspace's calendar.
    let scenario = json!({"process": "hold", "trigger": {"id": "R-1"}, "start": from});
    let sim = k.simulate_solution(&admin, Some(&source("office")), None, scenario.clone()).await.unwrap();
    let woke = sim["simulation"]["entries"].as_array().unwrap().iter().find(|e| e["what"]["event"] == "woke").cloned().unwrap_or(Value::Null);
    assert_eq!(woke["at"].as_str().map(utc), Some(d.at), "{sim}");
    assert!(k.simulate_solution(&admin, Some(&source("mars")), None, scenario).await.is_err(), "a calendar the workspace does not have");

    // Revised without the holiday: later deadlines move, and say which revision moved them.
    let mut revised = body.clone();
    revised["holidays"] = json!([]);
    assert_eq!(k.put_logic_calendar(&admin, "office", &revised).await.unwrap(), 2);
    let d = k.process_deadline(admin.tenant, &wait, from).await.unwrap();
    assert_eq!((d.at, d.revision), (utc("2026-11-10T11:30:00Z"), Some(2)));
}
