//! FLC-UI and FLC-PROC-014 through Studio's endpoints, against a real PostgreSQL
//! (`FERRUM_STUDIO_TEST_DATABASE_URL`; reset first): drafts two editors save against the version they read, a
//! check and a simulation that keep and reach nothing, a review that sees what changed and consents to new
//! power by name, runs followed with a cursor no slow transaction can slip past, the processes about a record,
//! tasks decided over HTTP, a partner whose role ends, and another workspace that sees none of it.

use chrono::Utc;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, Site, SiteRequest, SiteResponse};
use ferrum_meta::Catalog;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const PACKAGE: &str = "package: procs\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Requester\n    permissions:\n      - {entity: Request, read: true, write: true, create: true, submit: true, cancel: true}\n      - {entity: Order, read: true, write: true, create: true}\n  - name: Reviewer\n    permissions:\n      - {entity: Request, read: true}\n      - {entity: Order, read: true}\n";
const ENTITIES: &str = r#"
- name: Request
  label: Request
  kind: document
  submittable: true
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: total, type: decimal}
- name: Order
  label: Order
  kind: master
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: request, type: link, to: Request}
"#;

fn source(version: &str, review_label: &str) -> String {
    let cap = |id: &str| json!({"id": id, "version": "1.0.0"});
    json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.requests", "version": version, "label": "Requests",
        "requires": [cap("procs.request.read"), cap("procs.order.create")],
        "processes": [{"id": "approve_request", "label": "Approve a request", "trigger": {"event": "procs.request.submitted", "business_key": "trigger.id"},
            "entry": "snapshot", "max_steps": 50, "nodes": [
            {"type": "query", "id": "snapshot", "label": "Read it", "capability": cap("procs.request.read"), "input": {"id": "trigger.id"}, "next": "review", "on_error": "attention"},
            {"type": "human_task", "id": "review", "label": review_label, "role": "Reviewer", "material": "snapshot.output",
             "timeout": {"business_days": 2, "calendar": "office"}, "next": "order", "on_reject": "refused", "on_timeout": "attention"},
            {"type": "action", "id": "order", "label": "Order it", "capability": cap("procs.order.create"), "input": {"values.reference": "trigger.id", "values.request": "trigger.id"},
             "approval": "review", "next": "completed", "on_error": "attention", "on_unknown": "attention"},
            {"type": "end", "id": "completed", "label": "Done", "outcome": "completed"},
            {"type": "end", "id": "refused", "label": "Refused", "outcome": "refused"},
            {"type": "suspend", "id": "attention", "label": "Needs attention", "reason": "Look into it."}
        ]}]
    })
    .to_string()
}

async fn call(k: &Kernel, ctx: &Ctx, method: &str, path: &str, headers: &[(&str, &str)], query: &[(&str, &str)], body: Value) -> Result<SiteResponse, Error> {
    let req = SiteRequest {
        tenant: ctx.tenant,
        user: Some(ctx.clone()),
        method: method.into(),
        path: path.into(),
        query: query.iter().map(|(k, v)| (k.to_string(), json!(v))).collect(),
        headers: headers.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        body: if body.is_null() { vec![] } else { body.to_string().into_bytes() },
        client_ip: "127.0.0.1".into(),
    };
    ferrum_studio::site::StudioSite.handle(k, req).await
}

fn json_of(r: &SiteResponse) -> Value {
    serde_json::from_slice(&r.body).expect("json")
}

async fn get(k: &Kernel, ctx: &Ctx, path: &str) -> Value {
    json_of(&call(k, ctx, "GET", path, &[], &[], Value::Null).await.unwrap_or_else(|e| panic!("GET {path}: {e}")))
}

async fn post(k: &Kernel, ctx: &Ctx, path: &str, body: Value) -> Value {
    json_of(&call(k, ctx, "POST", path, &[], &[], body).await.unwrap_or_else(|e| panic!("POST {path}: {e}")))
}

async fn refused(k: &Kernel, ctx: &Ctx, method: &str, path: &str, headers: &[(&str, &str)], body: Value) -> &'static str {
    match call(k, ctx, method, path, headers, &[], body).await {
        Ok(r) => panic!("{method} {path} went through: {}", String::from_utf8_lossy(&r.body)),
        Err(e) => e.code(),
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["procs".into(), "studio".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

async fn user(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str], is_admin: bool) -> Ctx {
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    let id = k.create_user(admin, email, email, Some("Correct-Horse-9"), &roles, is_admin).await.unwrap();
    k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
}

#[test]
fn solutions_through_studio() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/procs.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut packages = vec![core, ferrum_meta::catalog::load_package_files(&files).expect("package")];
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let mut hooks = HookRegistry::default();
    ferrum_studio::register(&mut hooks);
    let settings = KernelSettings { process_runtime: true, ..Default::default() };
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, settings, None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let admin2 = user(&k, &admin, "admin2@acme.test", &[], true).await;
    // An author builds with the capabilities their own roles grant: the catalog never describes what its reader
    // may not use. A release runs with its requester's rights, not its author's.
    let author = user(&k, &admin, "author@acme.test", &["Solution Author", "Requester"], false).await;
    let requester = user(&k, &admin, "requester@acme.test", &["Requester"], false).await;
    let reviewer = user(&k, &admin, "reviewer@acme.test", &["Reviewer"], false).await;
    let outsider = user(&k, &admin, "outsider@acme.test", &[], false).await;

    // A calendar is an administrator's; its name and zone are checked.
    let office = json!({"zone": "Asia/Kolkata", "weekends": ["sat", "sun"], "holidays": []});
    assert_eq!(refused(&k, &author, "PUT", "/calendars/office", &[], office.clone()).await, "forbidden");
    assert_eq!(json_of(&call(&k, &admin, "PUT", "/calendars/office", &[], &[], office).await.unwrap())["revision"], json!(1));
    assert_eq!(refused(&k, &admin, "PUT", "/calendars/office", &[], json!({"zone": "Mars/Base", "weekends": [], "holidays": []})).await, "validation_failed");

    // Two editors of one draft: neither overwrites the other (FLC-UI-006).
    assert_eq!(refused(&k, &outsider, "GET", "/solutions", &[], Value::Null).await, "forbidden");
    let draft = post(&k, &author, "/solutions/drafts", json!({"source": source("1.0.0", "Approve the request")})).await;
    let d = format!("/solutions/drafts/{}", draft["id"].as_str().unwrap());
    let opened = call(&k, &author, "GET", &d, &[], &[], Value::Null).await.unwrap();
    assert!(opened.headers.iter().any(|(h, v)| h == "etag" && v == "\"1\""), "{:?}", opened.headers);
    assert_eq!(json_of(&opened)["solution"], json!("acme.requests"));
    let mine = call(&k, &author, "PUT", &d, &[("if-match", "\"1\"")], &[], json!({"source": source("1.0.0", "Approve this request")})).await.unwrap();
    assert_eq!(json_of(&mine)["version"], json!(2));
    assert_eq!(refused(&k, &admin, "PUT", &d, &[("if-match", "\"1\"")], json!({"source": source("1.0.0", "Theirs")})).await, "conflict", "a stale save");
    assert_eq!(refused(&k, &admin, "PUT", &d, &[], json!({"source": "{}"})).await, "validation_failed", "no If-Match");
    assert!(get(&k, &admin, &d).await["source"].as_str().unwrap().contains("Approve this request"), "the first save stands");

    // A check keeps nothing and points at what is wrong.
    let broken =
        post(&k, &author, "/solutions/compile", json!({"source": source("1.0.0", "x").replace("\"next\":\"completed\"", "\"next\":\"nowhere\"")})).await;
    assert_eq!(broken["ok"], json!(false));
    assert!(
        broken["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["message"].as_str().unwrap().contains("nowhere") && x["pointer"].as_str().unwrap().starts_with("/processes/0")),
        "{broken}"
    );
    // Anyone may check a source, against what they may use: someone without a role sees the capabilities as
    // unavailable, not described.
    let blind = post(&k, &outsider, "/solutions/compile", json!({"source": source("1.0.0", "x")})).await;
    assert_eq!(blind["ok"], json!(false));
    assert!(blind["diagnostics"].as_array().unwrap().iter().all(|x| x["code"] == json!("CAPABILITY_UNAVAILABLE")), "{blind}");
    let checked = post(&k, &requester, "/solutions/compile", json!({"source": source("1.0.0", "x")})).await;
    assert_eq!(checked["ok"], json!(true), "{checked}");

    // A simulation reaches nothing: no run, no task, no order (FLC-UI-007, FLC-UI-010).
    let start = "2026-11-06T10:00:00Z";
    let scenario = json!({"process": "approve_request", "trigger": {"id": "R-SIM"}, "requester": "u-req", "start": start,
        "results": {"snapshot": {"result": "done", "output": {"reference": "R-SIM", "total": "5000"}}},
        "decisions": {"review": {"approve": true, "by": "u-rev", "after": 3600}}});
    let sim = post(&k, &author, "/solutions/simulate", json!({"source": source("1.0.0", "x"), "scenario": scenario})).await;
    let entries = sim["simulation"]["entries"].as_array().unwrap();
    let decided = entries.iter().find(|e| e["kind"] == json!("event") && e["activation"] == json!("review")).unwrap();
    assert_eq!(decided["at"].as_str().unwrap()[..19], *"2026-11-06T11:00:00", "an hour later on the virtual clock");
    assert_eq!(sim["simulation"]["not_modelled"], json!(["order"]), "the order was not answered, so the run saw it as unknown");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let touched: (i64, i64, i64) = sqlx::query_as("select (select count(*) from logic_run), (select count(*) from logic_task), (select count(*) from e_order)")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(touched, (0, 0, 0), "nothing reached anything");
    // Without the review's decision, its deadline is two business days on the workspace's calendar.
    let mut waiting = scenario.clone();
    waiting["decisions"] = json!({});
    let late = post(&k, &author, "/solutions/simulate", json!({"source": source("1.0.0", "x"), "scenario": waiting})).await;
    let timed_out = late["simulation"]["entries"].as_array().unwrap().iter().find(|e| e["what"]["event"] == json!("task_timed_out")).unwrap().clone();
    assert_eq!(timed_out["at"].as_str().unwrap()[..19], *"2026-11-10T10:00:00", "Friday to Tuesday, the weekend skipped");

    // The author keeps a release; an administrator reviews it, seeing what it may do that nothing did before.
    let rel = json_of(&call(&k, &author, "POST", &format!("{d}/release"), &[("if-match", "2")], &[], Value::Null).await.unwrap());
    let r = format!("/solutions/releases/{}", rel["id"].as_str().unwrap());
    let view = get(&k, &admin, &r).await;
    assert_eq!(view["diff"]["privilege_growth"], json!(["procs.order.create"]));
    assert!(view["diff"]["changes"].as_array().unwrap().iter().any(|c| c["area"] == json!("step") && c["id"] == json!("approve_request/review")));
    let digest = view["release"]["artifact_digest"].as_str().unwrap().to_string();
    assert_eq!(
        refused(&k, &author, "POST", &format!("{r}/review"), &[], json!({"digest": digest, "privileges": ["procs.order.create"], "approve": true})).await,
        "forbidden"
    );
    assert_eq!(
        refused(&k, &admin, "POST", &format!("{r}/review"), &[], json!({"digest": digest, "approve": true})).await,
        "validation_failed",
        "new power not consented to"
    );
    assert_eq!(
        refused(&k, &admin, "POST", &format!("{r}/review"), &[], json!({"digest": digest, "privileges": ["procs.request.read"], "approve": true})).await,
        "validation_failed"
    );
    post(&k, &admin, &format!("{r}/review"), json!({"digest": digest, "privileges": ["procs.order.create"], "approve": true})).await;
    assert_eq!(post(&k, &admin, &format!("{r}/activate"), json!({})).await["generation"], json!(1));
    // A new version that only rewords the task: its review names the step, and there is nothing to consent to.
    let next = post(&k, &author, "/solutions/drafts", json!({"source": source("1.1.0", "Approve it, two days")})).await;
    let next_rel = json_of(
        &call(&k, &author, "POST", &format!("/solutions/drafts/{}/release", next["id"].as_str().unwrap()), &[("if-match", "1")], &[], Value::Null)
            .await
            .unwrap(),
    );
    let nr = format!("/solutions/releases/{}", next_rel["id"].as_str().unwrap());
    let next_view = get(&k, &admin2, &nr).await;
    assert_eq!(next_view["active"]["generation"], json!(1));
    assert_eq!(next_view["diff"]["privilege_growth"], json!([]));
    assert_eq!(
        next_view["diff"]["changes"].as_array().unwrap().iter().filter(|c| c["area"] != json!("process")).map(|c| c["id"].clone()).collect::<Vec<_>>(),
        vec![json!("approve_request/review")]
    );

    // Runs followed with a cursor: a change a slower transaction commits later is not skipped (FLC-PROC-014).
    let first = get(&k, &requester, "/runs").await;
    assert_eq!((first["runs"].as_array().unwrap().len(), first["resync"].clone()), (0, json!(false)));
    let mut cursor = first["cursor"].as_str().unwrap().to_string();
    let submit = |reference: &'static str| {
        let (k, requester) = (k.clone(), requester.clone());
        async move {
            let r = k.insert(&requester, "Request", obj(json!({"reference": reference, "total": "5000"}))).await.unwrap();
            let id: Uuid = r["id"].as_str().unwrap().parse().unwrap();
            k.submit(&requester, "Request", id, None).await.unwrap();
            for _ in 0..5 {
                k.dispatch_outbox_once().await.unwrap();
            }
            id
        }
    };
    let r1 = submit("R-1").await;
    let seen = json_of(&call(&k, &requester, "GET", "/runs", &[], &[("cursor", &cursor)], Value::Null).await.unwrap());
    let run1: Uuid = seen["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["subject_id"] == json!(r1))
        .expect("the new run")
        .get("id")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    cursor = seen["cursor"].as_str().unwrap().to_string();
    let r2 = submit("R-2").await;
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let run2: Uuid = sqlx::query_scalar("select id from logic_run where subject_id = $1").bind(r2).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // A slow transaction changes run 1 and stays open while run 2 changes and commits after it.
    let mut slow = k.db.tenant_tx(admin.tenant).await.unwrap();
    sqlx::query("update logic_run set status = 'running' where id = $1").bind(run1).execute(&mut *slow).await.unwrap();
    let mut quick = k.db.tenant_tx(admin.tenant).await.unwrap();
    sqlx::query("update logic_run set status = 'running' where id = $1").bind(run2).execute(&mut *quick).await.unwrap();
    quick.commit().await.unwrap();
    let during = json_of(&call(&k, &requester, "GET", "/runs", &[], &[("cursor", &cursor)], Value::Null).await.unwrap());
    assert!(during["runs"].as_array().unwrap().iter().any(|x| x["id"] == json!(run2) && x["status"] == json!("running")), "{during}");
    assert!(!during["runs"].as_array().unwrap().iter().any(|x| x["id"] == json!(run1) && x["status"] == json!("running")), "not committed yet");
    slow.commit().await.unwrap();
    let after = json_of(&call(&k, &requester, "GET", "/runs", &[], &[("cursor", during["cursor"].as_str().unwrap())], Value::Null).await.unwrap());
    assert!(
        after["runs"].as_array().unwrap().iter().any(|x| x["id"] == json!(run1) && x["status"] == json!("running")),
        "the slow change, not skipped: {after}"
    );
    let restored = json_of(&call(&k, &requester, "GET", "/runs", &[], &[("cursor", "999999999999")], Value::Null).await.unwrap());
    assert_eq!(restored["resync"], json!(true), "a cursor from another database starts again");
    assert!(get(&k, &outsider, "/runs").await["runs"].as_array().unwrap().is_empty(), "nobody else's runs");

    // The processes about a record, to whoever may read it; nothing of what their tasks are about.
    let material = json!({"reference": "R-1", "total": "5000", "id": r1.to_string()});
    let digest_of = ferrum_logic::canonical::digest(ferrum_logic::process::MATERIAL_SCHEMA, &ferrum_logic::capability::digestible(&material)).unwrap();
    k.process_open_task(
        admin.tenant,
        run1,
        "review",
        "task",
        "Approve this request",
        "Reviewer",
        &material,
        &digest_of,
        &json!([]),
        1,
        0,
        &[requester.user.to_string()],
        &json!({}),
        None,
    )
    .await
    .unwrap();
    let about = get(&k, &requester, &format!("/runs/about/Request/{r1}")).await;
    assert_eq!(about[0]["waiting"][0]["label"], json!("Approve this request"));
    assert!(about[0]["waiting"][0].get("material").is_none());
    assert!(matches!(refused(&k, &outsider, "GET", &format!("/runs/about/Request/{r1}"), &[], Value::Null).await, "forbidden" | "not_found"));

    // A task over HTTP: the requester cannot decide it, the reviewer does, on what they were shown.
    let tasks = get(&k, &reviewer, "/tasks").await;
    let task = tasks.as_array().unwrap().iter().find(|t| t["label"] == json!("Approve this request")).unwrap()["id"].as_str().unwrap().to_string();
    let seen_task = get(&k, &reviewer, &format!("/tasks/{task}")).await;
    assert_eq!(seen_task["material"]["reference"], json!("R-1"));
    assert_eq!(refused(&k, &requester, "POST", &format!("/tasks/{task}/decide"), &[], json!({"approve": true, "digest": digest_of})).await, "forbidden");
    assert_eq!(
        refused(&k, &reviewer, "POST", &format!("/tasks/{task}/decide"), &[], json!({"approve": true, "digest": "sha256:other"})).await,
        "stale_approval"
    );
    assert_eq!(post(&k, &reviewer, &format!("/tasks/{task}/decide"), json!({"approve": true, "digest": digest_of})).await["closed"], json!(true));

    // A partner authors until their role ends, and not after (BUILDER_UX journey 8).
    let partner = user(&k, &admin, "partner@acme.test", &[], false).await;
    let until = BTreeMap::from([("Solution Author".to_string(), Utc::now() + chrono::Duration::seconds(2))]);
    k.set_user_roles_until(&admin, partner.user, &["Solution Author".to_string(), "Requester".to_string()], &until).await.unwrap();
    let partner = k.load_ctx(admin.tenant, partner.user, None, "it".into()).await.unwrap();
    assert!(get(&k, &partner, "/solutions").await["drafts"].as_array().unwrap().len() >= 2);
    assert_eq!(refused(&k, &partner, "POST", &format!("{nr}/review"), &[], json!({"digest": "x", "approve": true})).await, "forbidden", "authors never review");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let partner = k.load_ctx(admin.tenant, partner.user, None, "it".into()).await.unwrap();
    assert_eq!(refused(&k, &partner, "GET", "/solutions", &[], Value::Null).await, "forbidden", "the role ended");

    // Another workspace sees none of it.
    let other = workspace(&k, "globex").await;
    assert_eq!(refused(&k, &other, "GET", &d, &[], Value::Null).await, "not_found");
    assert_eq!(refused(&k, &other, "GET", &r, &[], Value::Null).await, "not_found");
    assert!(get(&k, &other, "/runs").await["runs"].as_array().unwrap().is_empty());
}
