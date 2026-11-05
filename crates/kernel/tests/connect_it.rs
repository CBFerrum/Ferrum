//! The shared connector machinery against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database
//! is reset first) and a stand-in provider: credentials, mapping, resuming a sync, conflicts,
//! tombstones, delivery with retries and dead letters, webhooks, reconciliation, isolation. This
//! proves the machinery. It proves nothing about any real provider.

use async_trait::async_trait;
use ferrum_kernel::connect::{Connection, ConnectionInput, Connector, ConnectorInfo, Hook, Incoming, Outgoing, Pulled};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Contact
  label: Contact
  kind: master
  title_field: full_name
  fields:
    - {name: full_name, type: data, required: true, max_length: 20}
    - {name: city, type: data}
    - {name: status, type: select, options: [Active, Archived], default: Active}
"#;

const SIGNATURE: &str = "good-signature";

/// What the provider holds, and how it is behaving today.
#[derive(Default)]
struct World {
    /// Stream order: id, version, fields, deleted.
    records: Vec<(String, u32, Value, bool)>,
    token: String,
    calls: Vec<String>,
    /// Pulls to refuse with "slow down", by page.
    throttle: Vec<usize>,
    /// The provider answers nothing but errors.
    down: bool,
    pushes: Vec<(String, String)>,
    /// Pushes to fail before one succeeds.
    push_failures: usize,
}

struct Stub(Arc<Mutex<World>>);

const PAGE: usize = 2;

#[async_trait]
impl Connector for Stub {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            provider: "stub".into(),
            label: "Stub CRM".into(),
            family: "crm".into(),
            api_version: "2026-01".into(),
            streams: vec!["people".into()],
            writes: vec!["Contact".into()],
            webhooks: true,
            capabilities: vec!["read_people".into()],
            ..Default::default()
        }
    }

    async fn check(&self, _k: &Kernel, c: &Connection) -> Result<String> {
        let w = self.0.lock().unwrap();
        if c.credentials["token"].as_str() == Some(w.token.as_str()) {
            Ok("account 42".into())
        } else {
            Err(Error::Unauthorized("the provider refused these credentials".into()))
        }
    }

    async fn pull(&self, _k: &Kernel, _c: &Connection, stream: &str, cursor: Option<&str>) -> Result<Pulled> {
        let mut w = self.0.lock().unwrap();
        let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        w.calls.push(format!("{stream}@{start}"));
        if w.down {
            return Err(Error::Unavailable("the provider is down".into()));
        }
        if let Some(i) = w.throttle.iter().position(|p| *p == start) {
            w.throttle.remove(i);
            return Err(Error::TooManyRequests("slow down".into()));
        }
        let page: Vec<Incoming> = w
            .records
            .iter()
            .skip(start)
            .take(PAGE)
            .map(|(id, v, f, d)| Incoming {
                external_id: id.clone(),
                version: Some(v.to_string()),
                deleted: *d,
                fields: f.as_object().cloned().unwrap_or_default(),
            })
            .collect();
        let next = start + page.len();
        Ok(Pulled { records: page, next: (next < w.records.len()).then(|| next.to_string()).or_else(|| Some(next.to_string())) })
    }

    async fn push(&self, _k: &Kernel, _c: &Connection, change: &Outgoing) -> Result<String> {
        let mut w = self.0.lock().unwrap();
        w.pushes.push((change.key.clone(), change.fields.get("name").and_then(Value::as_str).unwrap_or_default().to_string()));
        if w.push_failures > 0 {
            w.push_failures -= 1;
            return Err(Error::Unavailable("the provider is down".into()));
        }
        Ok(change.external_id.clone().unwrap_or_else(|| format!("new-{}", change.record)))
    }

    async fn webhook(&self, _k: &Kernel, _c: &Connection, headers: &BTreeMap<String, String>, body: &[u8]) -> Result<Vec<Hook>> {
        if headers.get("x-stub-signature").map(String::as_str) != Some(SIGNATURE) {
            return Err(Error::Unauthorized("this webhook is not signed by the provider".into()));
        }
        let v: Value = serde_json::from_slice(body).map_err(|_| Error::invalid("not JSON"))?;
        Ok(vec![Hook {
            event_id: v["event"].as_str().unwrap_or_default().to_string(),
            stream: "people".into(),
            record: Incoming {
                external_id: v["id"].as_str().unwrap_or_default().to_string(),
                version: None,
                deleted: false,
                fields: v["fields"].as_object().cloned().unwrap_or_default(),
            },
        }])
    }
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-connect-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), "package: cases\nversion: 0.1.0\nlabel: Cases\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn input(token: &str, conflict: &str, tombstone: &str, push: bool) -> ConnectionInput {
    serde_json::from_value(json!({
        "provider": "stub",
        "name": "Our CRM",
        "config": {"region": "eu"},
        "credentials": {"token": token},
        "mapping": {"people": {
            "entity": "Contact",
            "fields": {"name": "full_name", "town": "city", "state": "status"},
            "values": {"status": {"ACTIVE": "Active", "ARCHIVED": "Archived"}},
            "conflict": conflict, "tombstone": tombstone, "push": push,
        }},
    }))
    .unwrap()
}

fn person(id: &str, name: &str, town: &str) -> (String, u32, Value, bool) {
    (id.to_string(), 1, json!({"name": name, "town": town, "state": "ACTIVE", "not_mapped": "ignored"}), false)
}

#[test]
fn the_connector_machinery_holds_up() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let world = Arc::new(Mutex::new(World { token: "right".into(), ..Default::default() }));
    let mut hooks = HookRegistry::default();
    hooks.add_connector(Arc::new(Stub(world.clone())));
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog(), hooks, settings, None).await.expect("boot");
    let workspace = |slug: &str| NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["cases".into()],
        settings: None,
    };
    let tenant = k.create_tenant(workspace("acme")).await.expect("workspace");
    let other = k.create_tenant(workspace("other")).await.expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let session = k.login("other", "admin@other.test", "Correct-Horse-9").await.unwrap();
    let outsider = k.load_ctx(other, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();

    let contacts = || {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let page = k.list(&admin, "Contact", &ListQuery { limit: Some(100), sort: Some("full_name asc".into()), ..Default::default() }).await.unwrap();
            page.rows.iter().map(|r| r["full_name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        }
    };
    let jobs = |k: Arc<Kernel>| async move {
        // Time passes between attempts.
        loop {
            let mut tx = k.db.system_tx().await.unwrap();
            sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
            sqlx::query("update connection set waiting_until = now() where waiting_until is not null").execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            if k.run_jobs_once(4).await.unwrap() == 0 {
                break;
            }
        }
    };
    let drain = |k: Arc<Kernel>| async move { while k.dispatch_outbox_once().await.unwrap() > 0 {} };

    // ───────────── setting up ─────────────
    assert_eq!(k.connectors(&admin).await.unwrap()[0].streams, ["people"], "what the provider really offers");
    assert_eq!(k.connectors(&admin).await.unwrap()[0].capabilities, ["read_people"], "each thing it can do, by name");
    assert!(matches!(k.connectors(&clerk).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.save_connection(&clerk, None, input("x", "review", "keep", false)).await, Err(Error::Forbidden(_))));
    let mut bad = input("x", "review", "keep", false);
    bad.provider = "nobody".into();
    assert!(k.save_connection(&admin, None, bad).await.is_err(), "no connector for it");
    let mut bad = input("x", "review", "keep", false);
    bad.mapping.get_mut("people").unwrap().fields.insert("name".into(), "nothing".into());
    assert!(matches!(k.save_connection(&admin, None, bad).await, Err(Error::Validation(_))), "no such field");
    let mut bad = input("x", "review", "keep", false);
    let map = bad.mapping.remove("people").unwrap();
    bad.mapping.insert("orders".into(), map);
    assert!(matches!(k.save_connection(&admin, None, bad).await, Err(Error::Validation(_))), "a stream the provider does not have");

    let id = k.save_connection(&admin, None, input("wrong", "review", "keep", false)).await.expect("saved");
    assert!(matches!(k.sync_connection(&admin, id, false).await, Err(Error::Invalid(_))), "it has not proved itself");
    assert!(matches!(k.test_connection(&admin, id).await, Err(Error::Unauthorized(_))));
    let view = k.connections(&admin).await.unwrap();
    assert_eq!((view[0]["enabled"].as_bool(), view[0]["has_credentials"].as_bool()), (Some(false), Some(true)));
    assert!(view[0]["last_error"].as_str().unwrap().contains("refused"), "{view:?}");
    let text = json!(view).to_string();
    assert!(!text.contains("wrong") && !text.contains("sealed\":\""), "credentials are never read back: {text}");
    k.save_connection(&admin, Some(id), input("right", "review", "keep", false)).await.unwrap();
    assert_eq!(k.test_connection(&admin, id).await.unwrap(), "account 42");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let sealed: String =
        sqlx::query_scalar("select sealed from connection where tenant_id = $1 and id = $2").bind(tenant).bind(id).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(!sealed.contains("right"), "sealed at rest");
    assert!(matches!(k.connections(&clerk).await, Err(Error::Forbidden(_))));
    assert!(k.connections(&outsider).await.unwrap().is_empty(), "another workspace");
    assert!(matches!(k.test_connection(&outsider, id).await, Err(Error::NotFound(_))));

    // ───────────── first import: pages, and a provider that says slow down ─────────────
    {
        let mut w = world.lock().unwrap();
        w.records = vec![
            person("1", "Ann", "Oslo"),
            person("2", "Ben", "Rome"),
            person("3", "Cid", "Lima"),
            person("4", "Dee", "Bonn"),
            person("5", "This name is far too long to fit", "Kiev"),
        ];
        w.throttle = vec![2];
    }
    let run = k.sync_connection(&admin, id, false).await.unwrap();
    assert_eq!(k.sync_connection(&admin, id, false).await.unwrap(), run, "one run at a time");
    jobs(k.clone()).await;
    assert_eq!(contacts().await, ["Ann", "Ben", "Cid", "Dee"], "the too-long one was refused, and the rest came in");
    let v = k.job(&admin, run).await.unwrap();
    assert_eq!(
        (v["status"].as_str(), v["attempts"].as_i64(), v["result"]["created"].as_i64(), v["result"]["failed"].as_i64()),
        (Some("done"), Some(1), Some(2), Some(1)),
        "being told to wait costs no attempt: {v}"
    );
    let calls = world.lock().unwrap().calls.clone();
    assert_eq!(calls.iter().filter(|c| *c == "people@0").count(), 1, "the retry carried on from its cursor, not from the top: {calls:?}");
    let drift = k.connection_drift(&admin, id).await.unwrap();
    assert_eq!(
        (drift.len(), drift[0]["kind"].as_str(), drift[0]["external_id"].as_str()),
        (1, Some("refused"), Some("5")),
        "the bad record is there for a person: {drift:?}"
    );
    k.resolve_drift(&admin, drift[0]["id"].as_str().unwrap().parse().unwrap()).await.unwrap();
    assert!(k.connections(&admin).await.unwrap()[0]["last_success_at"].is_string());

    // Only what is new, and a repeat changes nothing.
    world.lock().unwrap().records.push(person("6", "Eve", "Lyon"));
    let run = k.sync_connection(&admin, id, false).await.unwrap();
    jobs(k.clone()).await;
    assert_eq!(k.job(&admin, run).await.unwrap()["result"]["created"], json!(1));
    let run = k.sync_connection(&admin, id, true).await.unwrap();
    jobs(k.clone()).await;
    let v = k.job(&admin, run).await.unwrap();
    assert_eq!((v["result"]["created"].as_i64(), v["result"]["updated"].as_i64()), (Some(0), Some(0)), "from the start, nothing doubles: {v}");
    assert_eq!(contacts().await.len(), 5);

    // ───────────── what the provider holds, and how the connection is doing ─────────────
    let schema = k.connection_schema(&admin, id).await.expect("schema");
    assert_eq!(schema[0].stream, "people");
    let health = k.connections(&admin).await.unwrap()[0]["health"].clone();
    assert_eq!(health["authentication"], json!("ok"), "{health}");
    assert!(health["freshness"].is_string() && health["quota"]["waiting_until"].is_null(), "{health}");

    // ───────────── a provider that keeps failing is left alone ─────────────
    world.lock().unwrap().down = true;
    let run = k.sync_connection(&admin, id, false).await.unwrap();
    let again = |k: Arc<Kernel>| async move {
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        k.run_jobs_once(4).await.unwrap();
    };
    for _ in 0..3 {
        again(k.clone()).await;
    }
    let health = k.connections(&admin).await.unwrap()[0]["health"].clone();
    assert!(health["quota"]["waiting_until"].is_string(), "after three failures in a row: {health}");
    assert_eq!(health["quota"]["failures"], json!(3));
    assert!(health["quota"]["reason"].as_str().unwrap().contains("provider is down"), "{health}");
    let (on, failing, waiting, stale, _) = k.connection_gauges().await.unwrap();
    assert_eq!((on, failing, waiting, stale), (1, 1, 1, 0), "and monitoring sees it across workspaces");
    let tried = world.lock().unwrap().calls.len();
    let attempts = k.job(&admin, run).await.unwrap()["attempts"].clone();
    again(k.clone()).await;
    again(k.clone()).await;
    assert_eq!(world.lock().unwrap().calls.len(), tried, "nobody calls it while it is left alone");
    let v = k.job(&admin, run).await.unwrap();
    assert_eq!((v["status"].as_str(), &v["attempts"]), (Some("queued"), &attempts), "and the job waits, at no cost in attempts: {v}");
    world.lock().unwrap().down = false;
    jobs(k.clone()).await;
    assert_eq!(k.job(&admin, run).await.unwrap()["status"], json!("done"), "when it is back, the work carries on");
    let quota = k.connections(&admin).await.unwrap()[0]["health"]["quota"].clone();
    assert_eq!((quota["failures"].as_i64(), quota["waiting_until"].is_null()), (Some(0), true), "{quota}");

    // ───────────── both sides changed ─────────────
    let ann = |k: Arc<Kernel>, admin: ferrum_kernel::Ctx| async move {
        let page = k.list(&admin, "Contact", &ListQuery { limit: Some(100), ..Default::default() }).await.unwrap();
        page.rows.iter().find(|r| r["city"] == json!("Oslo") || r["full_name"].as_str().unwrap().starts_with("Ann")).unwrap().clone()
    };
    let local: Uuid = ann(k.clone(), admin.clone()).await["id"].as_str().unwrap().parse().unwrap();
    k.update(&admin, "Contact", local, Doc::from_iter([("city".to_string(), json!("Bergen"))]), None).await.unwrap();
    world.lock().unwrap().records[0].2 = json!({"name": "Ann", "town": "Tromso", "state": "ARCHIVED"});
    k.sync_connection(&admin, id, true).await.unwrap();
    jobs(k.clone()).await;
    let now = k.get(&admin, "Contact", local, false).await.unwrap();
    assert_eq!(now["city"], json!("Bergen"), "review: ours stays until someone looks");
    assert!(k.connection_drift(&admin, id).await.unwrap().iter().any(|d| d["kind"] == json!("conflict")));
    k.save_connection(&admin, Some(id), serde_json::from_value(json!({"provider": "stub", "name": "Our CRM", "mapping": {"people": {"entity": "Contact", "fields": {"name": "full_name", "town": "city", "state": "status"}, "values": {"status": {"ACTIVE": "Active", "ARCHIVED": "Archived"}}, "conflict": "source", "tombstone": "delete"}}})).unwrap()).await.unwrap();
    k.test_connection(&admin, id).await.expect("still the stored credentials");
    k.sync_connection(&admin, id, true).await.unwrap();
    jobs(k.clone()).await;
    let now = k.get(&admin, "Contact", local, false).await.unwrap();
    assert_eq!((now["city"].as_str(), now["status"].as_str()), (Some("Tromso"), Some("Archived")), "source: the provider wins, with its value translated");

    // A record gone at the provider.
    world.lock().unwrap().records[1].3 = true;
    k.sync_connection(&admin, id, true).await.unwrap();
    jobs(k.clone()).await;
    assert!(!contacts().await.contains(&"Ben".to_string()), "tombstone: delete");

    // ───────────── webhooks ─────────────
    let headers = |sig: &str| BTreeMap::from([("x-stub-signature".to_string(), sig.to_string())]);
    let body = json!({"event": "evt-1", "id": "7", "fields": {"name": "Fay", "town": "Riga"}}).to_string();
    assert!(matches!(k.connection_webhook(id, &headers("forged"), body.as_bytes()).await, Err(Error::Unauthorized(_))), "not from the provider");
    assert!(matches!(k.connection_webhook(Uuid::now_v7(), &headers(SIGNATURE), body.as_bytes()).await, Err(Error::NotFound(_))));
    assert_eq!(k.connection_webhook(id, &headers(SIGNATURE), body.as_bytes()).await.unwrap(), 1);
    assert_eq!(k.connection_webhook(id, &headers(SIGNATURE), body.as_bytes()).await.unwrap(), 0, "the same event again is not a new one");
    assert!(!contacts().await.contains(&"Fay".to_string()), "answered at once: the work is in a job");
    jobs(k.clone()).await;
    assert!(contacts().await.contains(&"Fay".to_string()));

    // ───────────── changes going out ─────────────
    k.save_connection(&admin, Some(id), input("right", "source", "keep", true)).await.unwrap();
    k.test_connection(&admin, id).await.unwrap();
    drain(k.clone()).await;
    jobs(k.clone()).await;
    world.lock().unwrap().pushes.clear();

    let made = k.insert(&admin, "Contact", Doc::from_iter([("full_name".to_string(), json!("Gus"))])).await.unwrap();
    let gus: Uuid = made["id"].as_str().unwrap().parse().unwrap();
    world.lock().unwrap().push_failures = 2;
    drain(k.clone()).await;
    jobs(k.clone()).await;
    let pushes = world.lock().unwrap().pushes.clone();
    assert_eq!(pushes.len(), 3, "retried: {pushes:?}");
    assert!(pushes.iter().all(|p| p.0 == pushes[0].0), "one change, one key, however many tries");
    drain(k.clone()).await;
    jobs(k.clone()).await;
    assert_eq!(world.lock().unwrap().pushes.len(), 3, "what we sent does not come back round");

    // Given up on, kept, and replayed.
    world.lock().unwrap().push_failures = 100;
    k.update(&admin, "Contact", gus, Doc::from_iter([("city".to_string(), json!("Graz"))]), None).await.unwrap();
    drain(k.clone()).await;
    jobs(k.clone()).await;
    let dead = k.connections(&admin).await.unwrap()[0]["dead_letters"].as_i64().unwrap();
    assert_eq!(dead, 1, "a dead letter");
    let activity = k.connection_activity(&admin, id, 50).await.unwrap();
    let given_up = activity["runs"].as_array().unwrap().iter().find(|r| r["status"] == json!("failed")).expect("the hub shows what failed");
    assert_eq!((given_up["kind"].as_str(), given_up["stream"].as_str()), (Some("platform.connector.deliver"), Some("people")));
    assert!(given_up["last_error"].as_str().is_some_and(|e| e.contains("down")), "{given_up}");
    assert!(activity["runs"].as_array().unwrap().iter().any(|r| r["kind"] == json!("platform.connector.sync") && r["status"] == json!("done")));
    assert_eq!((activity["streams"][0]["stream"].as_str(), activity["streams"][0]["entity"].as_str()), (Some("people"), Some("Contact")));
    assert!(activity["streams"][0]["records"].as_i64().is_some_and(|n| n > 0));
    assert!(matches!(k.connection_activity(&clerk, id, 50).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.connection_activity(&admin, Uuid::new_v4(), 50).await, Err(Error::NotFound(_))));
    let failed = k
        .jobs(&admin, 50)
        .await
        .unwrap()
        .into_iter()
        .find(|j| j["status"] == json!("failed") && j["kind"] == json!("platform.connector.deliver"))
        .expect("listed");
    world.lock().unwrap().push_failures = 0;
    k.retry_job(&admin, failed["id"].as_str().unwrap().parse().unwrap()).await.expect("replayed");
    jobs(k.clone()).await;
    assert_eq!(k.connections(&admin).await.unwrap()[0]["dead_letters"], json!(0));

    // An incoming change does not echo out.
    world.lock().unwrap().pushes.clear();
    world.lock().unwrap().records.push(person("8", "Hal", "Nice"));
    k.sync_connection(&admin, id, false).await.unwrap();
    jobs(k.clone()).await;
    drain(k.clone()).await;
    jobs(k.clone()).await;
    assert!(contacts().await.contains(&"Hal".to_string()));
    assert!(world.lock().unwrap().pushes.iter().all(|p| p.1 != "Hal"), "what came from there does not go back");

    // ───────────── reconciliation ─────────────
    world.lock().unwrap().records[3].2 = json!({"name": "Dee", "town": "Changed there", "state": "ACTIVE"});
    world.lock().unwrap().records.push(person("9", "Ivy", "Cork"));
    world.lock().unwrap().records.retain(|r| r.0 != "3");
    let mut before: HashMap<String, usize> = HashMap::new();
    for d in k.connection_drift(&admin, id).await.unwrap() {
        *before.entry(d["kind"].as_str().unwrap().to_string()).or_default() += 1;
        k.resolve_drift(&admin, d["id"].as_str().unwrap().parse().unwrap()).await.unwrap();
    }
    let run = k.reconcile_connection(&admin, id).await.unwrap();
    jobs(k.clone()).await;
    let v = k.job(&admin, run).await.unwrap();
    assert_eq!(v["status"], json!("done"), "{v}");
    let mut kinds: Vec<String> = k
        .connection_drift(&admin, id)
        .await
        .unwrap()
        .iter()
        .map(|d| format!("{}:{}", d["kind"].as_str().unwrap(), d["external_id"].as_str().unwrap()))
        .collect();
    kinds.sort();
    assert!(kinds.contains(&"differs:4".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"missing_here:9".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"missing_there:3".to_string()), "{kinds:?}");
    assert_eq!(contacts().await.iter().filter(|n| *n == "Dee").count(), 1, "reconciliation reports, and repairs nothing by itself");

    // ───────────── disconnecting ─────────────
    k.disconnect(&admin, id).await.expect("disconnected");
    let view = k.connections(&admin).await.unwrap();
    assert_eq!((view[0]["enabled"].as_bool(), view[0]["has_credentials"].as_bool()), (Some(false), Some(false)), "its credentials are gone");
    assert!(matches!(k.sync_connection(&admin, id, false).await, Err(Error::Invalid(_))));
    assert!(matches!(k.connection_webhook(id, &headers(SIGNATURE), body.as_bytes()).await, Err(Error::NotFound(_))), "nothing gets in");
    assert!(view[0]["linked"].as_i64().unwrap() > 0, "what it knew stays, for reconnecting");
}
