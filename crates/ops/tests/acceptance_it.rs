//! An app's industrial solution in Operations (FERRUM-IND-018) against a real PostgreSQL
//! (`FERRUM_OPS_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_ops_test`;
//! the database is reset first): its profiles and procedures become the workspace's, an upgrade
//! updates them and leaves what people recorded, and a device of the profile is commissioned only
//! after a run of each procedure passed on site, through the endpoints a person uses.

use ferrum_apps::industrial::{self, IndustrialSection, Solution};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, SiteRequest, SiteResponse};
use ferrum_meta::Catalog;
use ferrum_ops::acceptance;
use ferrum_ops::contract::Lifecycle;
use ferrum_ops::devices;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

async fn boot() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_OPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_ops_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("packages");
    packages.extend(ferrum_ops::packages().expect("ops"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    assert_eq!(catalog.links_out_of_reach("ops"), Vec::<String>::new());
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    hooks.scoped(&["ops".to_string()], ferrum_ops::register);
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

const PROFILE: &str = r#"
id: line-meter
vendor: Acme
model: PM-200
scheme: modbus+tcp
points:
  - {name: voltage, address: "30001", type: float32, unit: V}
  - {name: relay, address: "00001", type: bool, access: read_write}
commands:
  - {name: trip, point: relay}
"#;

const SAT: &str = r#"
id: meter-sat
title: Meter site acceptance
steps:
  - {id: voltage, kind: read, profile: line-meter, point: voltage, expect: {min: 207, max: 253}}
  - {id: trip, kind: command, profile: line-meter, command: trip, point: relay, expect: {equals: false}}
  - {id: seal, kind: manual, instruction: The meter's seal is intact}
"#;

/// The solution an app's bundle would carry, checked as publishing checks it.
fn solution(sat: &str) -> Solution {
    let section: IndustrialSection = serde_yaml::from_str(
        "{connectors: [{scheme: modbus+tcp, version: '>=0.3'}], device_profiles: [industrial/profiles/meter.yaml], acceptance: [industrial/acceptance/sat.yaml]}",
    )
    .unwrap();
    let files = BTreeMap::from([
        ("industrial/profiles/meter.yaml".to_string(), PROFILE.as_bytes().to_vec()),
        ("industrial/acceptance/sat.yaml".to_string(), sat.as_bytes().to_vec()),
    ]);
    industrial::check(&section, &files).expect("a valid solution")
}

async fn api(k: &Arc<Kernel>, tenant: Uuid, ctx: &Ctx, method: &str, path: &str, body: Value) -> Result<SiteResponse, Error> {
    let site = k.hooks.site("ops").unwrap().clone();
    let req = SiteRequest {
        tenant,
        user: Some(ctx.clone()),
        method: method.into(),
        path: path.into(),
        query: Map::new(),
        headers: Map::new(),
        body: body.to_string().into_bytes(),
        client_ip: "127.0.0.1".into(),
    };
    site.handle(k, req).await
}

fn json_of(r: SiteResponse) -> Value {
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    serde_json::from_slice(&r.body).unwrap()
}

async fn one(k: &Kernel, tenant: Uuid, sql: &'static str) -> Value {
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    let v: Value = sqlx::query_scalar(sql).bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    v
}

#[tokio::test]
async fn a_solution_on_site() {
    let k = boot().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "plant".into(),
            name: "Plant".into(),
            admin_email: "admin@plant.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("plant", "admin@plant.test", "Correct-Horse-9").await.unwrap();
    let before = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();

    // Installing the app brings Operations in the same request, so the import gets a context from
    // before Operations was there: its profile and procedure still become the workspace's.
    k.install_packages(tenant, &["ops".to_string()]).await.unwrap();
    acceptance::import(&k, &before, "meters", &solution(SAT)).await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let profile = one(&k, tenant, "select to_jsonb(p) from e_device_profile p where tenant_id = $1 and solution = 'meters/line-meter'").await;
    assert_eq!((profile["vendor"].as_str(), profile["model"].as_str(), profile["protocol"].as_str()), (Some("Acme"), Some("PM-200"), Some("modbus+tcp")));
    assert_eq!(profile["tested_on"], "Simulator", "nothing claims it met equipment");
    assert!(profile["reads"].as_str().unwrap().contains("voltage: 30001 (float32, V)"), "{profile}");
    assert!(profile["writes"].as_str().unwrap().contains("command trip: writes relay"), "{profile}");
    let profile_id: Uuid = profile["id"].as_str().unwrap().parse().unwrap();

    // A person records how far they tried it; the app's upgrade changes the procedure, not that.
    k.update(&admin, "DeviceProfile", profile_id, obj(json!({"tested_on": "Bench Hardware"})), None).await.unwrap();
    let upgraded = SAT.replace("title: Meter site acceptance", "title: Meter acceptance on site").replace("max: 253", "max: 250");
    acceptance::import(&k, &admin, "meters", &solution(&upgraded)).await.unwrap();
    let counts = one(&k, tenant, "select jsonb_build_array((select count(*) from e_device_profile where tenant_id = $1), (select count(*) from e_acceptance_procedure where tenant_id = $1))").await;
    assert_eq!(counts, json!([1, 1]), "an upgrade updates, it does not add");
    let procedure = one(&k, tenant, "select to_jsonb(p) from e_acceptance_procedure p where tenant_id = $1").await;
    assert_eq!(procedure["procedure_name"], "Meter acceptance on site");
    let max = one(&k, tenant, "select expect from e_acceptance_procedure__steps where tenant_id = $1 and step_id = 'voltage'").await;
    assert_eq!(max, json!({"min": 207.0, "max": 250.0}));
    assert_eq!(one(&k, tenant, "select to_jsonb(tested_on) from e_device_profile where tenant_id = $1").await, "Bench Hardware");
    let procedure_id: Uuid = procedure["id"].as_str().unwrap().parse().unwrap();

    // A meter of that profile, reached through a gateway.
    let site = id_of(&k.insert(&admin, "Site", obj(json!({"site_name": "North", "site_code": "north"}))).await.unwrap());
    let gw = id_of(&k.insert(&admin, "Device", obj(json!({"device_name": "GW", "device_code": "GW-1", "kind": "Gateway", "site": site}))).await.unwrap());
    let meter = id_of(
        &k.insert(
            &admin,
            "Device",
            obj(json!({"device_name": "Meter", "device_code": "M-1", "kind": "Meter", "site": site, "gateway": gw, "profile": profile_id})),
        )
        .await
        .unwrap(),
    );
    let plain = id_of(
        &k.insert(&admin, "Device", obj(json!({"device_name": "Other", "device_code": "M-2", "kind": "Meter", "site": site, "gateway": gw}))).await.unwrap(),
    );
    for d in [meter, plain] {
        devices::set_lifecycle(&k, &admin, d, Lifecycle::Claimed, "ours").await.unwrap();
        devices::set_lifecycle(&k, &admin, d, Lifecycle::Provisioned, "through the gateway").await.unwrap();
    }
    devices::set_lifecycle(&k, &admin, plain, Lifecycle::Commissioned, "no profile, no procedure").await.expect("nothing to pass");
    let early = devices::set_lifecycle(&k, &admin, meter, Lifecycle::Commissioned, "looks fine").await;
    assert!(matches!(&early, Err(Error::Invalid(m)) if m.contains("Meter acceptance on site")), "{early:?}");

    // The procedure is for it; not for the other.
    let listed = json_of(api(&k, tenant, &admin, "GET", &format!("/devices/{meter}/acceptance"), Value::Null).await.unwrap());
    assert_eq!(listed["procedures"][0]["passed"], false);
    assert!(api(&k, tenant, &admin, "POST", &format!("/devices/{plain}/acceptance"), json!({"procedure": procedure_id})).await.is_err());

    // A run that fails at a reading out of range.
    let run = json_of(api(&k, tenant, &admin, "POST", &format!("/devices/{meter}/acceptance"), json!({"procedure": procedure_id})).await.unwrap());
    let run_id = run["id"].as_str().unwrap().to_string();
    assert_eq!(run["results"].as_array().unwrap().len(), 3);
    let step = |run: &str, step: &str| format!("/acceptance/{run}/steps/{step}");
    assert!(api(&k, tenant, &admin, "POST", &step(&run_id, "voltage"), json!({})).await.is_err(), "a reading needs what was read");
    let failed = json_of(api(&k, tenant, &admin, "POST", &step(&run_id, "voltage"), json!({"value": 251.0})).await.unwrap());
    assert_eq!(failed["status"], "Failed");
    assert!(api(&k, tenant, &admin, "POST", &step(&run_id, "trip"), json!({"value": false})).await.is_err(), "a failed run is over");

    // Another that passes every step: the reading, the command's check, the person's look.
    let run = json_of(api(&k, tenant, &admin, "POST", &format!("/devices/{meter}/acceptance"), json!({"procedure": procedure_id})).await.unwrap());
    let run_id = run["id"].as_str().unwrap().to_string();
    json_of(api(&k, tenant, &admin, "POST", &step(&run_id, "voltage"), json!({"value": 231.4})).await.unwrap());
    assert!(api(&k, tenant, &admin, "POST", &step(&run_id, "voltage"), json!({"value": 231.4})).await.is_err(), "a step is recorded once");
    let mid = json_of(api(&k, tenant, &admin, "POST", &step(&run_id, "trip"), json!({"value": false})).await.unwrap());
    assert_eq!(mid["status"], "Running", "a step is still to do");
    assert!(devices::set_lifecycle(&k, &admin, meter, Lifecycle::Commissioned, "nearly").await.is_err(), "a run still running is not a pass");
    assert!(api(&k, tenant, &admin, "POST", &step(&run_id, "seal"), json!({"value": "fine"})).await.is_err(), "a manual step is said to hold, or not");
    let done = json_of(api(&k, tenant, &admin, "POST", &step(&run_id, "seal"), json!({"passed": true, "note": "seal 4471"})).await.unwrap());
    assert_eq!(done["status"], "Passed");
    let seal = done["results"].as_array().unwrap().iter().find(|r| r["step_id"] == "seal").unwrap().clone();
    assert_eq!((seal["outcome"].as_str(), seal["recorded_by"].as_str(), seal["note"].as_str()), (Some("Passed"), Some("admin@plant.test"), Some("seal 4471")));

    devices::set_lifecycle(&k, &admin, meter, Lifecycle::Commissioned, "passed on site").await.expect("commissioned after its acceptance");
    let listed = json_of(api(&k, tenant, &admin, "GET", &format!("/devices/{meter}/acceptance"), Value::Null).await.unwrap());
    assert_eq!(listed["procedures"][0]["passed"], true);
    assert_eq!(listed["runs"].as_array().unwrap().len(), 2);

    // Someone who only looks may read a run, not record one.
    let viewer_id = k.create_user(&admin, "vi@plant.test", "Vi", Some("Vi-Pass-12345"), &["Ops Viewer".into()], false).await.unwrap();
    let s = k.login("plant", "vi@plant.test", "Vi-Pass-12345").await.unwrap();
    let viewer = k.load_ctx(tenant, viewer_id, Some(s.token_version), "it".into()).await.unwrap();
    json_of(api(&k, tenant, &viewer, "GET", &format!("/devices/{meter}/acceptance"), Value::Null).await.unwrap());
    assert!(matches!(
        api(&k, tenant, &viewer, "POST", &format!("/devices/{meter}/acceptance"), json!({"procedure": procedure_id})).await,
        Err(Error::Forbidden(_))
    ));
    assert!(
        k.insert(&admin, "AcceptanceRun", obj(json!({"device": meter, "procedure": procedure_id, "status": "Passed"}))).await.is_err(),
        "a run is made by starting one, not by writing it"
    );
}
