//! IoT Edge against a real PostgreSQL (`FERRUM_EDGE_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_edge_test`; reset first): what a gateway is told
//! to be, what comes back from it, and what a person does with that.

use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, SiteRequest, SiteResponse};
use ferrum_meta::Catalog;
use ferrum_ops::contract::edge::{AlarmChange, ConfigRequest, ConfigResponse, EdgeConfig};
use ferrum_ops::contract::sign::{Enroll, Enrolled, HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME, message};
use ferrum_ops::contract::{Fact, Lifecycle};
use ferrum_ops::devices;
use serde_json::{Map, Value, json};
use sha2::Digest;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
}

async fn boot() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_EDGE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_edge_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("packages");
    packages.extend(ferrum_ops::packages().expect("ops"));
    packages.extend(ferrum_edge::packages().expect("edge"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    assert_eq!(catalog.links_out_of_reach("edge"), Vec::<String>::new(), "whoever fills in a link can look up what it points at");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_ops::register(&mut hooks);
    ferrum_edge::register(&mut hooks);
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

fn request(t: &T, user: Option<&Ctx>, method: &str, path: &str, headers: Map<String, Value>, body: Vec<u8>) -> SiteRequest {
    SiteRequest {
        tenant: t.tenant,
        user: user.cloned(),
        method: method.into(),
        path: path.into(),
        query: Map::new(),
        headers,
        body,
        client_ip: "127.0.0.1".into(),
    }
}

impl T {
    async fn api(&self, ctx: &Ctx, method: &str, path: &str, body: Value) -> Result<Value, Error> {
        let site = self.k.hooks.site("edge").unwrap().clone();
        site.handle(&self.k, request(self, Some(ctx), method, path, Map::new(), body.to_string().into_bytes()))
            .await
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    }

    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.admin, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }

    async fn outbox(&self) {
        while self.k.dispatch_outbox_once().await.unwrap() > 0 {}
    }

    async fn fact(&self, site: Uuid, kind: &str, at: chrono::DateTime<Utc>, body: Value) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        ferrum_ops::sync::apply_fact(&mut tx, self.tenant, site, &Fact { fact_id: Uuid::now_v7(), kind: kind.into(), at_source: at, actor: None, body })
            .await
            .unwrap();
        tx.commit().await.unwrap();
        self.outbox().await;
    }
}

struct Gateway {
    key: SigningKey,
    enrolled: Enrolled,
    clock: AtomicI64,
}

impl Gateway {
    async fn config(&self, t: &T, req: &ConfigRequest) -> SiteResponse {
        let body = serde_json::to_vec(req).unwrap();
        let time = Utc::now().timestamp_millis().max(self.clock.load(Ordering::SeqCst) + 1);
        self.clock.store(time, Ordering::SeqCst);
        let uri = format!("/api/site/{}/edge/config", t.tenant);
        let signature = self.key.sign(&message("POST", &uri, time, &body));
        let mut headers = Map::new();
        headers.insert(HEADER_KEY.into(), json!(self.enrolled.key_id));
        headers.insert(HEADER_TIME.into(), json!(time.to_string()));
        headers.insert(HEADER_SIGNATURE.into(), json!(hex::encode(signature.to_bytes())));
        headers.insert("x-ferrum-uri".into(), json!(uri));
        let site = t.k.hooks.site("edge").unwrap().clone();
        site.handle(&t.k, request(t, None, "POST", "/config", headers, body)).await.expect("config")
    }

    async fn fetch(&self, t: &T, have: &str) -> Option<EdgeConfig> {
        let r = self.config(t, &ConfigRequest { have: have.into(), runtime: "0.1.0".into(), ..Default::default() }).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        serde_json::from_slice::<ConfigResponse>(&r.body).unwrap().config
    }
}

async fn gateway(t: &T, code: &str, site: Uuid, seed: u8) -> (Uuid, Gateway) {
    let doc = t.insert("Device", json!({"device_name": code, "device_code": code, "kind": "Gateway", "site": site})).await;
    let id = id_of(&doc);
    devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Claimed, "ours").await.unwrap();
    let (code, _) = ferrum_ops::identity::new_enrollment(&t.k, &t.admin, id).await.unwrap();
    let key = SigningKey::from_bytes(&[seed; 32]);
    let enrolled = ferrum_ops::identity::enroll(&t.k, t.tenant, &Enroll { code, public_key: hex::encode(key.verifying_key().to_bytes()) }).await.unwrap();
    devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Commissioned, "checked").await.unwrap();
    devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Active, "in service").await.unwrap();
    (id, Gateway { key, enrolled, clock: AtomicI64::new(0) })
}

const SCALE: &str = r#"
name: acme-scale
connect: tcp://10.0.0.7:9100
frame: {line: "\r\n"}
read:
  - {match: '^(?P<kg>[0-9.]+) kg$', metric: weight.kg, group: kg}
commands:
  set.tare:
    send: "T {payload.value}\r\n"
    once: true
"#;

#[test]
fn iot_edge() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(32 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = boot().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "edge".into(),
            name: "Edge".into(),
            admin_email: "a@edge.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["edge".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("edge", "a@edge.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), tenant, admin };
    let person = |email: &'static str, roles: &'static [&'static str]| {
        let t = &t;
        async move {
            let id =
                t.k.create_user(&t.admin, email, "Someone", Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false)
                    .await
                    .unwrap();
            t.k.load_ctx(t.tenant, id, None, "it".into()).await.unwrap()
        }
    };
    let viewer = person("view@edge.test", &["Edge Viewer"]).await;
    let engineer = person("eng@edge.test", &["Edge Engineer", "Ops Dispatcher", "Device Administrator"]).await;

    // ───── a definition is checked when it is saved, and tried with no device attached ─────
    let bad = k.insert(&engineer, "DeviceDefinition", obj(json!({"definition_name": "bad", "body": "name: x\nconnect: foo://y\n"}))).await;
    assert!(matches!(bad, Err(Error::Validation(ref v)) if v[0].message.contains("not an address")), "{bad:?}");
    let tried = t.api(&engineer, "POST", "/definitions/try", json!({"body": SCALE, "sample": "12.5 kg"})).await.unwrap();
    assert_eq!(tried["ok"], json!(true), "{tried}");
    assert_eq!(tried["decoded"][0]["value"], json!(12.5));
    let wrong = t.api(&engineer, "POST", "/definitions/try", json!({"body": "name: x\nread:\n  - {metirc: a}\n"})).await.unwrap();
    assert!(wrong["error"].as_str().unwrap().contains("metirc"), "{wrong}");
    assert!(t.api(&viewer, "POST", "/definitions/try", json!({"body": SCALE})).await.is_err(), "a viewer does not write definitions");
    let definition = id_of(&k.insert(&engineer, "DeviceDefinition", obj(json!({"definition_name": "acme-scale", "body": SCALE}))).await.expect("a definition"));

    // ───── a gateway gets what it reaches, once it is published ─────
    let site = id_of(&t.insert("Site", json!({"site_name": "North", "site_code": "north"})).await);
    let other_site = id_of(&t.insert("Site", json!({"site_name": "South", "site_code": "south"})).await);
    let (gw, gateway1) = gateway(&t, "GW-1", site, 1).await;
    let scale = id_of(&t.insert("Device", json!({"device_name": "Scale", "device_code": "SCALE-1", "kind": "Machine", "site": site, "gateway": gw, "edge_definition": definition, "edge_connect": "tcp://10.0.0.8:9100", "edge_settings": {"unit": 3}, "edge_concurrency": 2, "operations": [{"operation": "set.tare"}]})).await);
    t.insert("Device", json!({"device_name": "Elsewhere", "device_code": "OTHER-1", "kind": "Machine", "site": other_site, "edge_definition": definition}))
        .await;
    let draft = gateway1.fetch(&t, "").await.expect("a first configuration");
    assert!(draft.devices.is_empty(), "a draft reaches nothing");
    k.run_action(&engineer, "DeviceDefinition", definition, "publish", json!({})).await.expect("published");
    let config = gateway1.fetch(&t, &draft.version).await.expect("it changed");
    assert_eq!(config.devices.len(), 1, "only what this gateway reaches: {config:?}");
    assert_eq!(config.devices[0].code, "SCALE-1");
    assert_eq!(config.devices[0].connect.as_deref(), Some("tcp://10.0.0.8:9100"));
    assert_eq!(config.devices[0].settings, json!({"unit": 3}));
    assert_eq!(config.devices[0].concurrency, 2);
    assert_eq!(config.definitions["acme-scale"], SCALE);
    assert!(gateway1.fetch(&t, &config.version).await.is_none(), "an unchanged configuration costs one short answer");

    // A saved draft does not move what gateways run. Publishing does.
    k.update(&engineer, "DeviceDefinition", definition, obj(json!({"body": format!("{SCALE}\n# better\n")})), None).await.unwrap();
    assert!(gateway1.fetch(&t, &config.version).await.is_none());
    k.run_action(&engineer, "DeviceDefinition", definition, "publish", json!({})).await.unwrap();
    let config = gateway1.fetch(&t, &config.version).await.expect("published again");
    assert!(config.definitions["acme-scale"].contains("better"));

    // ───── a rule reaches the sites it is for ─────
    assert!(k.insert(&engineer, "EdgeRule", obj(json!({"rule_name": "x", "metric": "m", "comparison": "Silent"}))).await.is_err(), "silence needs a length");
    assert!(k.insert(&engineer, "EdgeRule", obj(json!({"rule_name": "x", "metric": "m", "comparison": "Above"}))).await.is_err(), "a limit needs a value");
    assert!(
        k.insert(&engineer, "EdgeRule", obj(json!({"rule_name": "x", "metric": "m", "comparison": "Above", "limit_value": 80, "clear_value": 90})))
            .await
            .is_err(),
        "it clears on the safe side"
    );
    let rule = id_of(&k.insert(&engineer, "EdgeRule", obj(json!({"rule_name": "Overweight", "metric": "weight.kg", "comparison": "Above", "limit_value": 100, "clear_value": 90, "hold_secs": 5, "severity": "Critical", "definition": definition}))).await.expect("a rule"));
    k.insert(
        &engineer,
        "EdgeRule",
        obj(json!({"rule_name": "South only", "metric": "weight.kg", "comparison": "Above", "limit_value": 1, "site": other_site})),
    )
    .await
    .unwrap();
    k.insert(
        &engineer,
        "EdgeExport",
        obj(json!({"export_name": "Historian", "send_to": "https://historian.local/in", "metrics": "weight.*, temp.c", "sign_with": "historian"})),
    )
    .await
    .unwrap();
    let config = gateway1.fetch(&t, &config.version).await.expect("rules changed it");
    assert_eq!(config.rules.len(), 1, "{:?}", config.rules);
    assert_eq!((config.rules[0].value, config.rules[0].clear, config.rules[0].hold_secs), (100.0, Some(90.0), 5));
    assert_eq!(config.rules[0].devices, vec!["SCALE-1".to_string()], "a rule for a definition is for its devices");
    assert_eq!(config.exports[0].only, vec!["weight.*".to_string(), "temp.c".to_string()]);

    // ───── what comes back, in any order ─────
    let base = Utc::now();
    let raised = AlarmChange { rule, device: scale, raised: true, value: Some(120.0) };
    let cleared = AlarmChange { rule, device: scale, raised: false, value: Some(80.0) };
    t.fact(site, "edge.alarm", base, json!(raised)).await;
    t.fact(site, "edge.alarm", base + Duration::seconds(1), json!(raised)).await;
    let overview = t.api(&viewer, "GET", "/overview", Value::Null).await.unwrap();
    assert_eq!(overview["alarms"]["Critical"], json!(1), "raised once, however often it is said: {overview}");
    let device = t.api(&viewer, "GET", &format!("/devices/{scale}"), Value::Null).await.unwrap();
    let alarm = device["alarms"][0]["id"].as_str().unwrap().to_string();
    assert!(t.api(&viewer, "POST", &format!("/alarms/{alarm}/acknowledge"), json!({})).await.is_err());
    t.api(&engineer, "POST", &format!("/alarms/{alarm}/acknowledge"), json!({"note": "Going to look"})).await.expect("acknowledged");
    assert!(t.api(&engineer, "POST", &format!("/alarms/{alarm}/acknowledge"), json!({})).await.is_err(), "once");
    t.fact(site, "edge.alarm", base + Duration::seconds(30), json!(cleared)).await;
    let device = t.api(&viewer, "GET", &format!("/devices/{scale}"), Value::Null).await.unwrap();
    assert_eq!(device["alarms"], json!([]), "the edge cleared it");

    t.fact(site, "edge.attribute", base, json!({"device": scale, "key": "firmware", "value": "1.2"})).await;
    t.fact(site, "edge.attribute", base - Duration::hours(1), json!({"device": scale, "key": "firmware", "value": "1.1"})).await;
    t.fact(site, "edge.event", base, json!({"device": scale, "name": "door.open", "value": true})).await;
    let device = t.api(&viewer, "GET", &format!("/devices/{scale}"), Value::Null).await.unwrap();
    assert_eq!(device["attributes"][0]["value"], json!("1.2"), "a late report does not replace a newer one");
    assert_eq!(device["events"][0]["name"], json!("door.open"));

    // ───── asking for a value goes through the ledger, and stays out of step until it is true ─────
    devices::set_lifecycle(&k, &t.admin, scale, Lifecycle::Claimed, "ours").await.unwrap();
    devices::set_lifecycle(&k, &t.admin, scale, Lifecycle::Provisioned, "behind its gateway").await.unwrap();
    devices::set_lifecycle(&k, &t.admin, scale, Lifecycle::Commissioned, "checked").await.unwrap();
    devices::set_lifecycle(&k, &t.admin, scale, Lifecycle::Active, "in service").await.unwrap();
    assert!(t.api(&viewer, "PUT", &format!("/devices/{scale}/desired"), json!({"key": "tare", "value": 5})).await.is_err());
    let asked = t.api(&engineer, "PUT", &format!("/devices/{scale}/desired"), json!({"key": "tare", "value": 5})).await.expect("asked");
    assert!(asked["command"].is_string(), "{asked}");
    let device = t.api(&viewer, "GET", &format!("/devices/{scale}"), Value::Null).await.unwrap();
    let tare = device["attributes"].as_array().unwrap().iter().find(|a| a["key"] == json!("tare")).unwrap().clone();
    assert_eq!(tare["in_step"], json!(false), "{tare}");
    t.fact(site, "edge.attribute", Utc::now(), json!({"device": scale, "key": "tare", "value": 5})).await;
    let device = t.api(&viewer, "GET", &format!("/devices/{scale}"), Value::Null).await.unwrap();
    let tare = device["attributes"].as_array().unwrap().iter().find(|a| a["key"] == json!("tare")).unwrap().clone();
    assert_eq!(tare["in_step"], json!(true), "{tare}");

    // ───── what a gateway heard and nobody claimed ─────
    gateway1
        .config(
            &t,
            &ConfigRequest {
                have: "x".into(),
                strangers: vec!["V-9".into()],
                problems: vec!["bad: broken".into()],
                links: json!([{"address": "tcp://a", "up": true}]),
                runtime: "0.1.0".into(),
            },
        )
        .await;
    let overview = t.api(&viewer, "GET", "/overview", Value::Null).await.unwrap();
    assert_eq!(overview["strangers"][0]["code"], json!("V-9"), "{overview}");
    assert_eq!(overview["gateways"][0]["problems"], json!(["bad: broken"]));
    assert_eq!(overview["gateways"][0]["links"][0]["up"], json!(true));
    let claimed = t.api(&engineer, "POST", "/strangers/claim", json!({"gateway": gw, "code": "V-9", "definition": definition})).await.expect("claimed");
    assert!(claimed["id"].is_string());
    let overview = t.api(&viewer, "GET", "/overview", Value::Null).await.unwrap();
    assert_eq!(overview["strangers"], json!([]));

    // ───── a device with no gateway ─────
    let probe = id_of(&t.insert("Device", json!({"device_name": "Probe", "device_code": "PROBE-1", "kind": "Sensor", "site": site})).await);
    let token = t.api(&engineer, "POST", &format!("/devices/{probe}/token"), json!({})).await.unwrap()["token"].as_str().unwrap().to_string();
    let site_api = k.hooks.site("edge").unwrap().clone();
    let ingest = |tok: String, body: Value| {
        let mut headers = Map::new();
        headers.insert("x-ferrum-device-token".into(), json!(tok));
        request(&t, None, "POST", "/ingest", headers, body.to_string().into_bytes())
    };
    let refused = site_api.handle(&k, ingest("0".repeat(48), json!({"values": {"temp.c": 1}}))).await.unwrap();
    assert_eq!(refused.status, 401);
    let sent = site_api
        .handle(&k, ingest(token.clone(), json!({"values": {"temp.c": 21.5, "text": "skip"}, "state": "Running", "attributes": {"model": "P1"}})))
        .await
        .unwrap();
    assert_eq!(sent.status, 200, "{}", String::from_utf8_lossy(&sent.body));
    t.outbox().await;
    let device = t.api(&viewer, "GET", &format!("/devices/{probe}"), Value::Null).await.unwrap();
    assert_eq!(device["attributes"][0]["key"], json!("model"), "{device}");
    assert_eq!(device["latest"][0]["metric"], json!("temp.c"));
    devices::set_lifecycle(&k, &t.admin, probe, Lifecycle::Retired, "stolen").await.unwrap();
    let stopped = site_api.handle(&k, ingest(token, json!({"values": {"temp.c": 1}}))).await.unwrap();
    assert_eq!(stopped.status, 403, "its token ends with it");

    // ───── a release goes to a share of gateways, the same share each time ─────
    let release = json!({"release_version": "0.2.0", "url": "https://r/x", "sha256": "ab", "signature": "cd", "expires_at": (Utc::now() + Duration::days(7)).to_rfc3339(), "status": "Rolling Out", "percent": 100});
    let r = id_of(&t.insert("EdgeRelease", release).await);
    let config = gateway1.fetch(&t, "").await.unwrap();
    assert_eq!(config.release.as_ref().map(|r| r.version.as_str()), Some("0.2.0"));
    k.update(&t.admin, "EdgeRelease", r, obj(json!({"percent": 0})), None).await.unwrap();
    assert!(gateway1.fetch(&t, "").await.unwrap().release.is_none(), "not this gateway's turn");
    let _ = sha2::Sha256::digest(b"");

    // ───── robots ─────
    assert_eq!(t.api(&viewer, "GET", "/robots", Value::Null).await.unwrap()["robots"], json!([]));
}
