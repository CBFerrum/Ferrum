//! Plugins as platforms, against a real PostgreSQL (`FERRUM_PLUGINS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_plugins_test`). The database is reset first.
//!
//! The device-gateway example (built from plugins/examples/device-gateway) brings its own
//! entities, serves devices over sockets, takes commands from users and pushes them to the device
//! on whichever replica holds its socket, publishes a public status route, reacts to events, runs
//! on a schedule and guards a document with a configurable hook — all within declared permissions.

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use ferrum_plugins::http::HttpCall;
use ferrum_plugins::{NewPlugin, Outgoing, PluginEvents, PluginHook, PluginHost};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const WASM: &[u8] = include_bytes!("fixtures/device_gateway.wasm");
const MANIFEST: &str = include_str!("fixtures/device_gateway.json");

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

async fn boot(host: &Arc<PluginHost>) -> Arc<Kernel> {
    let url = std::env::var("FERRUM_PLUGINS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_plugins_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    hooks.register("*", Arc::new(PluginHook(host.clone())));
    hooks.add_event_sink(Arc::new(PluginEvents(host.clone())));
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

async fn kv(k: &Kernel, tenant: Uuid, key: &str) -> Value {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let v: Option<Value> =
        sqlx::query_scalar("select value from plugin_kv where tenant_id = $1 and key = $2").bind(tenant).bind(key).fetch_optional(&mut *tx).await.unwrap();
    v.unwrap_or(Value::Null)
}

fn call(method: &str, path: &str, body: &str) -> HttpCall {
    HttpCall { method: method.into(), path: path.into(), query: Map::new(), headers: Map::new(), body: body.into() }
}

#[test]
fn a_plugin_turns_ferrum_into_a_device_platform() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let host = PluginHost::new(false).unwrap();
    // As on a server: plugins' settings are sealed at rest with the server's secret key.
    let key = SecretKey::new([7u8; 32]);
    host.seal_with(key.clone());
    let k = boot(&host).await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "fleet".into(),
            name: "Fleet Ltd".into(),
            admin_email: "admin@fleet.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["parties".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("fleet", "admin@fleet.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();

    // ───────────── install: entities, service user, schedules ─────────────
    let manifest: NewPlugin = serde_json::from_str(MANIFEST).expect("manifest");
    let clerk = k.create_user(&admin, "clerk@fleet.test", "Clerk", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let clerk_ctx = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(host.install(&k, &clerk_ctx, manifest.clone(), WASM).await, Err(Error::Forbidden(_))), "admins only");
    let bad: NewPlugin = serde_json::from_value(json!({"name": "bad", "schedules": [{"name": "x", "every_seconds": 5}], "http_hosts": ["*"]})).unwrap();
    assert!(matches!(host.install(&k, &admin, bad, WASM).await, Err(Error::Validation(_))), "manifest is validated");
    for (fuel, mem) in [(Some(2_000_000_000), None), (None, Some(512))] {
        let greedy = NewPlugin { fuel_limit: fuel, memory_limit_mb: mem, ..manifest.clone() };
        assert!(matches!(host.install(&k, &admin, greedy, WASM).await, Err(Error::Validation(_))), "resource caps: {fuel:?} {mem:?}");
    }
    let plugin_id = host.install(&k, &admin, manifest, WASM).await.expect("install");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    assert!(admin.schema.entity("XDevice").is_some() && admin.schema.entity("XReading").is_some(), "the plugin's entities exist");
    let gateway = host.plugin(&k.db, tenant, "device-gateway").await.unwrap();

    // ───────────── a hook guards customers, configured by the administrator ─────────────
    let blocked = k.insert(&admin, "Customer", obj(json!({"customer_name": "Blocked Ltd"}))).await;
    assert!(matches!(blocked, Err(Error::Validation(ref f)) if f[0].field == "customer_name"), "{blocked:?}");
    k.insert(&admin, "Customer", obj(json!({"customer_name": "Acme"}))).await.expect("allowed");

    // ───────────── a device connects over its socket and boots ─────────────
    let c1 = Uuid::now_v7();
    let mut rx1 = host.sockets.register(tenant, c1);
    let socket = |event: &str, connection: Uuid, path: &str, message: Option<Value>| json!({"event": event, "connection": connection, "path": path, "message": message.map(|m| m.to_string())});
    host.socket_event(&k, tenant, &gateway, None, socket("connect", c1, "/devices/D1", None)).await.expect("connect");
    let boot_reply =
        host.socket_event(&k, tenant, &gateway, None, socket("message", c1, "/devices/D1", Some(json!({"type": "boot", "firmware": "1.2.0"})))).await.unwrap();
    assert!(boot_reply["send"].as_str().unwrap().contains("accepted"), "{boot_reply}");
    let read = host
        .socket_event(&k, tenant, &gateway, None, socket("message", c1, "/devices/D1", Some(json!({"type": "reading", "metric": "kWh", "value": "12.5"}))))
        .await
        .unwrap();
    assert!(read["send"].as_str().unwrap().contains("ack"));
    let devices = k.list(&admin, "XDevice", &ListQuery::default()).await.unwrap();
    assert_eq!(devices.rows.len(), 1);
    assert_eq!(devices.rows[0]["status"], json!("Online"));
    assert_eq!(devices.rows[0]["firmware"], json!("1.2.0"));
    let readings = k.list(&admin, "XReading", &ListQuery::default()).await.unwrap();
    assert_eq!(readings.rows[0]["value"], json!("12.5"));
    let trail = k.audit_trail(&admin, "XDevice", devices.rows[0]["id"].as_str().unwrap().parse().unwrap()).await.unwrap();
    assert_eq!(trail["chain_valid"], json!(true), "plugin writes are audited like any other");

    // ───────────── a customer's rule holds for what a plugin writes, as for anyone (FLC-HOOK-005) ─────────────
    let reviewer = k.create_user(&admin, "reviewer@fleet.test", "Reviewer", Some("Correct-Horse-9"), &[], true).await.unwrap();
    let reviewer = k.load_ctx(tenant, reviewer, None, "it".into()).await.unwrap();
    let source = json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "fleet.readings", "version": "1.0.0", "label": "Readings",
        "rules": [{"id": "no_forbidden", "entity": "XReading", "phase": "validate", "check": "doc.metric != \"forbidden\"", "message": "A reading of that metric is refused"}]});
    let release = k.save_release(&admin, &source.to_string()).await.expect("release");
    let release_id: Uuid = release["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&reviewer, release_id, release["artifact_digest"].as_str().unwrap(), true, "").await.unwrap();
    let generation = k.activate_release(&admin, release_id, None).await.expect("activate");
    let refused = host
        .socket_event(&k, tenant, &gateway, None, socket("message", c1, "/devices/D1", Some(json!({"type": "reading", "metric": "forbidden", "value": "1"}))))
        .await;
    let said = match &refused {
        Ok(reply) => reply.to_string(),
        Err(e) => e.to_string(),
    };
    assert!(said.contains("A reading of that metric is refused"), "the plugin's insert met the rule: {said}");
    assert_eq!(k.list(&admin, "XReading", &ListQuery::default()).await.unwrap().rows.len(), 1, "and nothing was written");
    k.deactivate_solution(&admin, "fleet.readings", generation).await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();

    // ───────────── a user sends a command; the plugin pushes it down the device's socket ─────────────
    let reply =
        host.handle_http(&k, tenant, "device-gateway", call("POST", "/devices/D1/command", r#"{"action":"reboot"}"#), Some(admin.clone())).await.unwrap();
    assert_eq!(reply.status, 200);
    match tokio::time::timeout(Duration::from_secs(5), rx1.recv()).await.unwrap() {
        Some(Outgoing::Text(t)) => assert!(t.contains("reboot") && t.contains("admin@fleet.test"), "{t}"),
        other => panic!("expected the command, got {other:?}"),
    }
    let anonymous = host.handle_http(&k, tenant, "device-gateway", call("POST", "/devices/D1/command", "{}"), None).await;
    assert!(matches!(anonymous, Err(Error::Unauthorized(_))), "private routes need a user");

    // ───────────── …even when the socket lives on another replica ─────────────
    let replica_b = PluginHost::new(false).unwrap();
    replica_b.seal_with(key.clone());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let listener = replica_b.sockets.spawn_listener(k.db.clone(), stop_rx);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let c2 = Uuid::now_v7();
    let mut rx2 = replica_b.sockets.register(tenant, c2);
    replica_b.socket_event(&k, tenant, &gateway, None, socket("connect", c2, "/devices/D2", None)).await.unwrap();
    let reply =
        host.handle_http(&k, tenant, "device-gateway", call("POST", "/devices/D2/command", r#"{"action":"start"}"#), Some(admin.clone())).await.unwrap();
    assert_eq!(reply.status, 200);
    match tokio::time::timeout(Duration::from_secs(5), rx2.recv()).await.expect("delivered across replicas within 5s") {
        Some(Outgoing::Text(t)) => assert!(t.contains("start"), "{t}"),
        other => panic!("expected the command on replica B, got {other:?}"),
    }
    let _ = stop_tx.send(true);
    let _ = listener.await;

    // ───────────── public routes act as the plugin, within its permissions ─────────────
    let status = host.handle_http(&k, tenant, "device-gateway", call("GET", "/status", ""), None).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&status.body).unwrap()["online"], json!(1));
    let probe = host.handle_http(&k, tenant, "device-gateway", call("GET", "/probe", ""), None).await.unwrap();
    assert_eq!(probe.status, 403, "the service user cannot read what the plugin did not declare");

    // ───────────── events and schedules ─────────────
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(kv(&k, tenant, "customers_seen").await, json!(1));
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(kv(&k, tenant, "customers_seen").await, json!(1), "each event reaches a plugin once");
    assert_eq!(ferrum_plugins::schedule::run_due_once(&host, &k).await.unwrap(), 1);
    assert_eq!(ferrum_plugins::schedule::run_due_once(&host, &k).await.unwrap(), 0, "not due again for a minute");
    assert_eq!(kv(&k, tenant, "sweeps").await, json!(1));

    // ───────────── it reconnects before the old socket has closed ─────────────
    let d1_status = async || {
        let filters = ferrum_kernel::query::parse_filters(&json!({"device_id": "D1"})).unwrap();
        k.list(&admin, "XDevice", &ListQuery { filters, ..Default::default() }).await.unwrap().rows[0]["status"].clone()
    };
    let c3 = Uuid::now_v7();
    let mut rx3 = host.sockets.register(tenant, c3);
    host.socket_event(&k, tenant, &gateway, None, socket("connect", c3, "/devices/D1", None)).await.expect("reconnect");
    host.socket_event(&k, tenant, &gateway, None, socket("close", c1, "/devices/D1", None)).await.unwrap();
    host.sockets.unregister(c1);
    assert_eq!(d1_status().await, json!("Online"), "the old socket's close says nothing about the new one");
    host.handle_http(&k, tenant, "device-gateway", call("POST", "/devices/D1/command", r#"{"action":"still here"}"#), Some(admin.clone())).await.unwrap();
    match tokio::time::timeout(Duration::from_secs(5), rx3.recv()).await.unwrap() {
        Some(Outgoing::Text(t)) => assert!(t.contains("still here"), "{t}"),
        other => panic!("the new socket should still be mapped, got {other:?}"),
    }

    // ───────────── the device disconnects ─────────────
    host.socket_event(&k, tenant, &gateway, None, socket("close", c3, "/devices/D1", None)).await.unwrap();
    host.sockets.unregister(c3);
    assert_eq!(d1_status().await, json!("Offline"));

    // ───────────── configuration, disabling, removal ─────────────
    let mut config = Map::new();
    config.insert("blocked_word".into(), json!("evil"));
    host.set_config(&k, &admin, plugin_id, config, false).await.unwrap();
    k.insert(&admin, "Customer", obj(json!({"customer_name": "Blocked Again Ltd"}))).await.expect("old word allowed now");
    assert!(k.insert(&admin, "Customer", obj(json!({"customer_name": "Evil Corp"}))).await.is_err(), "new word refused");
    host.set_enabled(&k, &admin, plugin_id, false).await.unwrap();
    k.insert(&admin, "Customer", obj(json!({"customer_name": "Evil Twin"}))).await.expect("a disabled plugin does nothing");
    assert!(matches!(host.handle_http(&k, tenant, "device-gateway", call("GET", "/status", ""), None).await, Err(Error::NotFound(_))));
    let listed = host.list(&k, &admin).await.unwrap();
    assert_eq!(listed[0]["config_keys"], json!(["blocked_word"]), "secrets are never listed, only their keys");
    // A merge patch changes one key and keeps the others; null removes one.
    let patch = |v: Value| v.as_object().unwrap().clone();
    host.set_config(&k, &admin, plugin_id, patch(json!({"api_key": "s3cret"})), true).await.unwrap();
    assert_eq!(host.list(&k, &admin).await.unwrap()[0]["config_keys"], json!(["api_key", "blocked_word"]));
    host.set_config(&k, &admin, plugin_id, patch(json!({"blocked_word": null})), true).await.unwrap();
    assert_eq!(host.list(&k, &admin).await.unwrap()[0]["config_keys"], json!(["api_key"]));
    // Sealed at rest, each value bound to its workspace, plugin and key; the plugin reads it as given.
    host.set_enabled(&k, &admin, plugin_id, true).await.unwrap();
    let stored = || async {
        let mut tx = k.db.system_tx().await.unwrap();
        let v: Value =
            sqlx::query_scalar("select config from plugin where tenant_id = $1 and id = $2").bind(tenant).bind(plugin_id).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        v
    };
    let at_rest = stored().await;
    assert!(at_rest["api_key"]["$sealed"].is_string() && !at_rest.to_string().contains("s3cret"), "{at_rest}");
    assert_eq!(host.plugin(&k.db, tenant, "device-gateway").await.unwrap().config["api_key"], json!("s3cret"));
    // A setting stored in the clear (before settings were sealed) is sealed at the next start.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update plugin set config = config || '{\"region\": \"eu\"}'::jsonb where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(plugin_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(host.seal_stored(&k.db).await.unwrap(), 1);
    assert_eq!(host.seal_stored(&k.db).await.unwrap(), 0, "nothing left to seal");
    let at_rest = stored().await;
    assert!(at_rest["region"]["$sealed"].is_string(), "{at_rest}");
    let config = host.plugin(&k.db, tenant, "device-gateway").await.unwrap().config;
    assert_eq!((config["api_key"].clone(), config["region"].clone()), (json!("s3cret"), json!("eu")));
    // A sealed value moved to another setting, or read with another key, is left out (and logged).
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update plugin set config = jsonb_set(config, '{region}', config->'api_key') where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(plugin_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    host.invalidate(tenant);
    let config = host.plugin(&k.db, tenant, "device-gateway").await.unwrap().config;
    assert_eq!((config["api_key"].clone(), config.get("region").cloned()), (json!("s3cret"), None), "moved: refused");
    let elsewhere = PluginHost::new(false).unwrap();
    elsewhere.seal_with(SecretKey::new([8u8; 32]));
    assert_eq!(*elsewhere.plugin(&k.db, tenant, "device-gateway").await.unwrap().config, json!({}), "another key opens nothing");
    // A key rotated out: what it sealed still opens, and moves to the new key at the next start.
    let rotated = PluginHost::new(false).unwrap();
    let new_key = SecretKey::new([9u8; 32]);
    rotated.seal_with(new_key.clone());
    rotated.open_also_with(vec![key.clone()]);
    assert_eq!(rotated.plugin(&k.db, tenant, "device-gateway").await.unwrap().config["api_key"], json!("s3cret"), "before the move");
    assert_eq!(rotated.seal_stored(&k.db).await.unwrap(), 1);
    assert_eq!(rotated.seal_stored(&k.db).await.unwrap(), 0, "nothing left to move");
    assert_eq!(stored().await["api_key"]["$key"], json!(new_key.id));
    rotated.invalidate(tenant);
    assert_eq!(rotated.plugin(&k.db, tenant, "device-gateway").await.unwrap().config["api_key"], json!("s3cret"), "after it");
    let lost = PluginHost::new(false).unwrap();
    lost.seal_with(SecretKey::new([1u8; 32]));
    assert!(lost.plugin(&k.db, tenant, "device-gateway").await.unwrap().config.get("api_key").is_none(), "without the key the setting is missing, not garbled");
    assert_eq!(lost.seal_stored(&k.db).await.unwrap(), 0, "and a value no key opens is left for the key to come back");
    assert_eq!(stored().await["api_key"]["$key"], json!(new_key.id));
    assert_eq!(rotated.seal_stored(&k.db).await.unwrap(), 0);
    host.remove(&k, &admin, plugin_id).await.unwrap();
    assert!(host.list(&k, &admin).await.unwrap().is_empty());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let active: bool = sqlx::query_scalar("select is_active from app_user where tenant_id = $1 and email = 'device-gateway@plugins.ferrum.invalid'")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!active, "the service user is deactivated");
}
