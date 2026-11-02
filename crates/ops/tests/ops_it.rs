//! The ledger, fencing, identity and telemetry against a real PostgreSQL
//! (`FERRUM_OPS_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_ops_test`;
//! the database is reset first), through the endpoints a site runtime and a person use.
//!
//! The scenario is the assessment's: a tote moves. Everything that can go wrong with knowing what
//! happened is tried: repeats, reports out of order, a lost answer, an expired command, a crash that
//! leaves the outcome unknown, a gateway replaced while work was running.

use chrono::{Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, SiteRequest, SiteResponse};
use ferrum_meta::Catalog;
use ferrum_ops::commands::{self, Issuer, NewCommand};
use ferrum_ops::contract::command::Report;
use ferrum_ops::contract::sign::{Enrolled, HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME, message};
use ferrum_ops::contract::telemetry::{Class, Measurement, Quality};
use ferrum_ops::contract::{Fact, Health, Lifecycle, State, SyncRequest, SyncResponse, VERSION};
use ferrum_ops::{authority, devices, telemetry};
use serde_json::{Map, Value, json};
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
    let url = std::env::var("FERRUM_OPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_ops_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("packages");
    packages.extend(ferrum_ops::packages().expect("ops"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    assert_eq!(catalog.links_out_of_reach("ops"), Vec::<String>::new(), "whoever fills in a link can look up what it points at");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    hooks.scoped(&["ops".to_string()], ferrum_ops::register);
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

async fn workspace(k: &Arc<Kernel>, slug: &str) -> T {
    let email = format!("admin@{slug}.test");
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: email.clone(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["ops".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login(slug, &email, "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    T { k: k.clone(), tenant, admin }
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

async fn api(t: &T, ctx: &Ctx, method: &str, path: &str, body: Value) -> Result<SiteResponse, Error> {
    let site = t.k.hooks.site("ops").unwrap().clone();
    site.handle(&t.k, request(t, Some(ctx), method, path, Map::new(), body.to_string().into_bytes())).await
}

fn json_of(r: SiteResponse) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}

/// A site runtime, as far as the platform can tell: a key, and what it believes.
struct Gateway {
    key: SigningKey,
    enrolled: Enrolled,
    epoch: i64,
    boot: Uuid,
    /// Each request's time; always rising, as a real site's is.
    clock: AtomicI64,
}

impl Gateway {
    fn tick(&self) -> i64 {
        let now = Utc::now().timestamp_millis().max(self.clock.load(Ordering::SeqCst) + 1);
        self.clock.store(now, Ordering::SeqCst);
        now
    }

    fn signed(&self, t: &T, body: &[u8], time_ms: i64, key: &SigningKey) -> SiteRequest {
        let uri = format!("/api/site/{}/ops/sync", t.tenant);
        let signature = key.sign(&message("POST", &uri, time_ms, body));
        let headers = obj(
            json!({HEADER_KEY: self.enrolled.key_id, HEADER_TIME: time_ms.to_string(), HEADER_SIGNATURE: hex::encode(signature.to_bytes()), "x-ferrum-uri": uri}),
        );
        request(t, None, "POST", "/sync", headers, body.to_vec())
    }

    fn sync_request(&self) -> SyncRequest {
        SyncRequest {
            contract: VERSION,
            epoch: self.epoch,
            boot: self.boot,
            in_doubt: vec![],
            reports: vec![],
            measurements: vec![],
            facts: vec![],
            health: Health { version: "test".into(), ..Default::default() },
            held: vec![],
            want: 10,
            config_version: String::new(),
        }
    }

    async fn send(&self, t: &T, req: SyncRequest) -> SiteResponse {
        let body = serde_json::to_vec(&req).unwrap();
        let site = t.k.hooks.site("ops").unwrap().clone();
        site.handle(&t.k, self.signed(t, &body, self.tick(), &self.key)).await.expect("sync")
    }

    async fn sync(&self, t: &T, req: SyncRequest) -> SyncResponse {
        let r = self.send(t, req).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        serde_json::from_slice(&r.body).unwrap()
    }

    fn report(&self, command: Uuid, state: State) -> Report {
        Report { report_id: Uuid::now_v7(), command_id: command, state, at_source: Utc::now(), epoch: self.epoch, detail: Value::Null, evidence: None }
    }
}

async fn gateway(t: &T, code: &str, site: Uuid, seed: u8) -> (Uuid, Gateway) {
    let doc =
        t.k.insert(&t.admin, "Device", obj(json!({"device_name": code, "device_code": code, "kind": "Gateway", "site": site, "lifecycle": "Imported"})))
            .await
            .expect("gateway");
    let id = id_of(&doc);
    devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Claimed, "ours").await.expect("claimed");
    (id, enroll(t, id, seed).await)
}

async fn enroll(t: &T, device: Uuid, seed: u8) -> Gateway {
    let code =
        json_of(api(t, &t.admin, "POST", &format!("/devices/{device}/enrollment"), Value::Null).await.expect("code"))["code"].as_str().unwrap().to_string();
    let key = SigningKey::from_bytes(&[seed; 32]);
    let site = t.k.hooks.site("ops").unwrap().clone();
    let body = json!({"code": code, "public_key": hex::encode(key.verifying_key().to_bytes())}).to_string().into_bytes();
    let r = site.handle(&t.k, request(t, None, "POST", "/enroll", Map::new(), body.clone())).await.expect("enroll");
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let enrolled: Enrolled = serde_json::from_slice(&r.body).unwrap();
    assert!(site.handle(&t.k, request(t, None, "POST", "/enroll", Map::new(), body)).await.is_err(), "a code works once");
    Gateway { key, enrolled, epoch: 1, boot: Uuid::now_v7(), clock: AtomicI64::new(0) }
}

async fn activate(t: &T, device: Uuid) {
    devices::set_lifecycle(&t.k, &t.admin, device, Lifecycle::Commissioned, "checked").await.expect("commissioned");
    devices::set_lifecycle(&t.k, &t.admin, device, Lifecycle::Active, "in service").await.expect("active");
}

/// A robot through its whole lifecycle, allowed `warehouse.move`, and `door.force` with approval.
async fn robot(t: &T, code: &str, site: Uuid) -> Uuid {
    let doc =
        t.k.insert(
            &t.admin,
            "Device",
            obj(json!({"device_name": code, "device_code": code, "kind": "Robot", "site": site,
        "operations": [{"operation": "warehouse.move"}, {"operation": "door.force", "requires_approval": true}]})),
        )
        .await
        .expect("robot");
    let id = id_of(&doc);
    devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Claimed, "ours").await.unwrap();
    enroll(t, id, code.as_bytes()[0]).await;
    activate(t, id).await;
    id
}

async fn issue(t: &T, ctx: &Ctx, cmd: NewCommand) -> Result<commands::Issued, Error> {
    let mut tx = t.k.db.tenant_tx(t.tenant).await?;
    let issued = commands::issue(&mut tx, ctx, cmd).await?;
    tx.commit().await?;
    Ok(issued)
}

fn move_tote(target: Uuid) -> NewCommand {
    NewCommand {
        target,
        operation: "warehouse.move".into(),
        schema_version: 1,
        payload: json!({"tote": "T-481", "to": "PACK-1"}),
        expires_in_secs: 600,
        ..Default::default()
    }
}

async fn state(t: &T, id: Uuid) -> String {
    json_of(api(t, &t.admin, "GET", &format!("/commands/{id}"), Value::Null).await.unwrap())["state"].as_str().unwrap().to_string()
}

async fn events(t: &T, topic: &str) -> i64 {
    let mut tx = t.k.db.system_tx().await.unwrap();
    let n = sqlx::query_scalar("select count(*) from outbox where tenant_id = $1 and topic = $2").bind(t.tenant).bind(topic).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[test]
fn the_ledger_says_only_what_was_established() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = boot().await;
    let t = workspace(&k, "north").await;
    let site = id_of(&k.insert(&t.admin, "Site", obj(json!({"site_name": "North DC", "site_code": "north"}))).await.expect("site"));
    let (gw_id, gw) = gateway(&t, "GW-1", site, 7).await;
    activate(&t, gw_id).await;
    let bot = robot(&t, "AMR-1", site).await;

    // ───── what a device was not commissioned for cannot be sent, whoever asks ─────
    let no = issue(&t, &t.admin, NewCommand { operation: "arm.swing".into(), ..move_tote(bot) }).await;
    assert!(matches!(no, Err(Error::Invalid(ref m)) if m.contains("not commissioned")), "{no:?}");
    let idle = id_of(&k.insert(&t.admin, "Device", obj(json!({"device_name": "Spare", "device_code": "AMR-2", "kind": "Robot", "site": site}))).await.unwrap());
    assert!(matches!(issue(&t, &t.admin, move_tote(idle)).await, Err(Error::Invalid(ref m)) if m.contains("takes no work")));
    let forever = issue(&t, &t.admin, NewCommand { expires_in_secs: 30 * 24 * 3600, ..move_tote(bot) }).await;
    assert!(matches!(forever, Err(Error::Validation(_))), "a deadline is required, and bounded");

    // ───── a command exists only if the transaction that wanted it commits ─────
    let mut tx = k.db.tenant_tx(t.tenant).await.unwrap();
    let ghost = commands::issue(&mut tx, &t.admin, move_tote(bot)).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(matches!(api(&t, &t.admin, "GET", &format!("/commands/{}", ghost.id), Value::Null).await, Err(Error::NotFound(_))));
    assert_eq!(gw.sync(&t, gw.sync_request()).await.commands.len(), 0, "and nothing reaches the site");

    // ───── the same intent twice is one command ─────
    let first = issue(&t, &t.admin, NewCommand { dedupe_key: Some("wave-1/line-1".into()), ..move_tote(bot) }).await.unwrap();
    let again = issue(&t, &t.admin, NewCommand { dedupe_key: Some("wave-1/line-1".into()), ..move_tote(bot) }).await.unwrap();
    assert_eq!((first.id, first.state, first.repeated), (again.id, State::Queued, false));
    assert!(again.repeated);
    let c = first.id;

    // ───── a lost answer: the site never heard, so it is sent again until the journal has it ─────
    let lost = gw.sync(&t, gw.sync_request()).await;
    assert_eq!(lost.commands.len(), 1);
    assert!(lost.commands[0].intact(), "the payload is what was authorized");
    assert!(lost.commands[0].startable(Utc::now(), 1).is_ok());
    let resent = gw.sync(&t, gw.sync_request()).await;
    assert_eq!(resent.commands.iter().map(|c| c.id).collect::<Vec<_>>(), vec![c], "not forgotten");
    let mut held = gw.sync_request();
    held.held = vec![c];
    assert_eq!(gw.sync(&t, held).await.commands.len(), 0, "once the journal has it, it stops");

    // ───── reports out of order, and repeated: the ending does not depend on it ─────
    let completed = Report { evidence: Some(json!({"scanned": "T-481", "at": "PACK-1"})), ..gw.report(c, State::Completed) };
    let started = gw.report(c, State::Executing);
    let accepted = gw.report(c, State::Accepted);
    let mut out_of_order = gw.sync_request();
    out_of_order.reports = vec![completed.clone()];
    let r = gw.sync(&t, out_of_order).await;
    assert_eq!(r.reports_done, vec![completed.report_id]);
    assert_eq!(state(&t, c).await, "Completed", "a completion that arrives first is still a completion");
    let mut late = gw.sync_request();
    late.reports = vec![accepted, started, completed.clone()];
    gw.sync(&t, late).await;
    assert_eq!(state(&t, c).await, "Completed", "what arrived late changes nothing");
    let seen = json_of(api(&t, &t.admin, "GET", &format!("/commands/{c}"), Value::Null).await.unwrap());
    assert_eq!(seen["history"].as_array().unwrap().iter().filter(|e| e["state"] == "Completed").count(), 1, "repeating a report does not repeat it");
    assert_eq!(seen["history"].as_array().unwrap().iter().filter(|e| e["state"] == "Executing").count(), 1, "and late ones are kept");
    assert_eq!(events(&t, "ops.command.completed").await, 1, "subscribers hear of it once");

    // A second, different ending: the first stands, and someone is asked to look.
    let mut second = gw.sync_request();
    second.reports = vec![gw.report(c, State::Failed)];
    gw.sync(&t, second).await;
    let conflicted = json_of(api(&t, &t.admin, "GET", &format!("/commands/{c}"), Value::Null).await.unwrap());
    assert_eq!((conflicted["state"].as_str(), conflicted["conflict"].as_bool()), (Some("Completed"), Some(true)));
    assert_eq!(json_of(api(&t, &t.admin, "GET", "/overview", Value::Null).await.unwrap())["sites"][0]["needs_person"], json!(1));
    api(
        &t,
        &t.admin,
        "POST",
        &format!("/commands/{c}/reconcile"),
        json!({"to": "Completed", "evidence": "The tote is at PACK-1; the robot's fault came after"}),
    )
    .await
    .unwrap();
    assert_eq!(json_of(api(&t, &t.admin, "GET", "/overview", Value::Null).await.unwrap())["sites"][0]["needs_person"], json!(0));

    // ───── nobody knows: only evidence settles it, and nothing retries it ─────
    let u = issue(&t, &t.admin, NewCommand { payload: json!({"tote": "T-500"}), ..move_tote(bot) }).await.unwrap().id;
    assert_eq!(gw.sync(&t, gw.sync_request()).await.commands.len(), 1);
    let mut r1 = gw.sync_request();
    r1.held = vec![u];
    r1.reports = vec![gw.report(u, State::Accepted), Report { detail: json!({"why": "no answer for 300 s"}), ..gw.report(u, State::OutcomeUnknown) }];
    gw.sync(&t, r1).await;
    assert_eq!(state(&t, u).await, "OutcomeUnknown");
    assert_eq!(gw.sync(&t, gw.sync_request()).await.commands.len(), 0, "never sent again");
    let mut bare = gw.sync_request();
    bare.reports = vec![gw.report(u, State::Completed)];
    gw.sync(&t, bare).await;
    assert_eq!(state(&t, u).await, "OutcomeUnknown", "a report without evidence does not settle it");

    let dispatcher = person(&t, "dana@north.test", &["Ops Dispatcher"]).await;
    let recovery = person(&t, "rae@north.test", &["Ops Recovery"]).await;
    let viewer = person(&t, "vic@north.test", &["Ops Viewer"]).await;
    assert!(
        matches!(api(&t, &dispatcher, "POST", &format!("/commands/{u}/reconcile"), json!({"to": "Failed", "evidence": "x"})).await, Err(Error::Forbidden(_))),
        "recovery is its own privilege"
    );
    assert!(
        matches!(api(&t, &recovery, "POST", &format!("/commands/{u}/reconcile"), json!({"to": "Failed", "evidence": " "})).await, Err(Error::Validation(_))),
        "say what you found"
    );
    assert!(
        matches!(
            api(
                &t,
                &recovery,
                "POST",
                "/commands",
                serde_json::to_value(json!({"target": bot, "operation": "warehouse.move", "expires_in_secs": 60})).unwrap()
            )
            .await,
            Err(Error::Forbidden(_))
        ),
        "and it does not include dispatching"
    );
    assert!(matches!(
        api(&t, &viewer, "POST", "/commands", json!({"target": bot, "operation": "warehouse.move", "expires_in_secs": 60})).await,
        Err(Error::Forbidden(_))
    ));
    api(&t, &recovery, "POST", &format!("/commands/{u}/reconcile"), json!({"to": "Failed", "evidence": "Tote still in its bin, robot stopped in aisle 4"}))
        .await
        .expect("settled");
    assert_eq!(state(&t, u).await, "Failed");
    let mut asking = gw.sync_request();
    asking.in_doubt = vec![u];
    assert_eq!(gw.sync(&t, asking).await.settled, vec![(u, State::Failed)], "the site that could not tell hears what a person found");
    assert!(
        api(&t, &recovery, "POST", &format!("/commands/{u}/reconcile"), json!({"to": "Completed", "evidence": "changed my mind"})).await.is_err(),
        "settled is settled"
    );

    // ───── a second person, and software proposes ─────
    let force = NewCommand { operation: "door.force".into(), payload: json!({"door": 3}), ..move_tote(bot) };
    let p = json_of(
        api(
            &t,
            &dispatcher,
            "POST",
            "/commands",
            serde_json::to_value(json!({"target": bot, "operation": "door.force", "payload": {"door": 3}, "expires_in_secs": 600})).unwrap(),
        )
        .await
        .expect("proposed"),
    );
    let proposed: Uuid = p["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(p["state"], json!("Proposed"));
    assert_eq!(gw.sync(&t, gw.sync_request()).await.commands.len(), 0, "not until it is approved");
    let manager = person(&t, "mo@north.test", &["Ops Manager"]).await;
    assert!(matches!(api(&t, &dispatcher, "POST", &format!("/commands/{proposed}/approve"), Value::Null).await, Err(Error::Forbidden(_))));
    let self_approved = {
        let own = issue(&t, &manager, force.clone()).await.unwrap().id;
        api(&t, &manager, "POST", &format!("/commands/{own}/approve"), Value::Null).await
    };
    assert!(matches!(self_approved, Err(Error::Forbidden(_))), "nobody approves their own");
    api(&t, &manager, "POST", &format!("/commands/{proposed}/approve"), Value::Null).await.expect("approved");
    assert_eq!(state(&t, proposed).await, "Queued");

    let by_ai = issue(&t, &manager, NewCommand { issuer: Issuer::Ai, ..move_tote(bot) }).await.unwrap();
    assert_eq!(by_ai.state, State::Proposed, "an AI proposes, even for what a person could send at once");
    let envelope = k
        .insert(&manager, "AutomationEnvelope", obj(json!({"envelope_name": "Moves", "site": site, "operation": "warehouse.move", "max_per_hour": 2, "valid_until": (Utc::now() + Duration::hours(1)).to_rfc3339()})))
        .await
        .expect("envelope");
    for _ in 0..2 {
        assert_eq!(issue(&t, &manager, NewCommand { issuer: Issuer::Automation, ..move_tote(bot) }).await.unwrap().state, State::Queued, "inside its limits");
    }
    assert_eq!(
        issue(&t, &manager, NewCommand { issuer: Issuer::Automation, ..move_tote(bot) }).await.unwrap().state,
        State::Proposed,
        "past them, a person decides"
    );
    k.update(&manager, "AutomationEnvelope", id_of(&envelope), obj(json!({"max_per_hour": 100, "enabled": false})), None).await.expect("switched off");
    assert_eq!(
        issue(&t, &manager, NewCommand { issuer: Issuer::Ai, ..move_tote(bot) }).await.unwrap().state,
        State::Proposed,
        "switching it off takes effect at once"
    );

    // An agent has one way in, and it proposes, whatever it asks for.
    let tools = ferrum_ops::mcp::OpsTools;
    use ferrum_mcp::ToolProvider;
    let names: Vec<String> = tools.tools(&dispatcher).iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["ops_overview", "ops_command", "ops_propose"], "looking and proposing, and no tool for anything else");
    let proposed_by_ai = tools
        .call(&k, &dispatcher, "ops_propose", &json!({"target": bot, "operation": "warehouse.move", "reason": "Aisle 4 is congested", "issuer": "person"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(proposed_by_ai["state"], json!("Proposed"), "it cannot call itself a person");
    assert!(
        tools.call(&k, &viewer, "ops_propose", &json!({"target": bot, "operation": "warehouse.move", "reason": "x"})).await.unwrap().is_err(),
        "and it has no more rights than its person"
    );
    assert!(
        tools.call(&k, &dispatcher, "ops_propose", &json!({"target": bot, "operation": "arm.swing", "reason": "x"})).await.unwrap().is_err(),
        "nor can it invent what a device can do"
    );
    assert!(tools.call(&k, &dispatcher, "ops_overview", &json!({})).await.unwrap().is_ok());

    // ───── cancelling: what never left stops, the rest is a request ─────
    let unsent = issue(&t, &t.admin, NewCommand { payload: json!({"tote": "T-9"}), expires_in_secs: 1, ..move_tote(idle_free(&t, site).await) }).await;
    assert!(unsent.is_err(), "that device is not active");
    let drained = gw.sync(&t, gw.sync_request()).await;
    let mut confirm = gw.sync_request();
    confirm.held = drained.commands.iter().map(|c| c.id).collect();
    gw.sync(&t, confirm).await;
    let never = issue(&t, &t.admin, move_tote(bot)).await.unwrap().id;
    let r = json_of(api(&t, &t.admin, "POST", &format!("/commands/{never}/cancel"), json!({"reason": "wave cancelled"})).await.unwrap());
    assert_eq!(r["state"], json!("Aborted"));
    let onsite = issue(&t, &t.admin, move_tote(bot)).await.unwrap().id;
    assert_eq!(gw.sync(&t, gw.sync_request()).await.commands.len(), 1);
    let mut h = gw.sync_request();
    h.held = vec![onsite];
    h.reports = vec![gw.report(onsite, State::Executing)];
    gw.sync(&t, h).await;
    let r = json_of(api(&t, &t.admin, "POST", &format!("/commands/{onsite}/cancel"), json!({"reason": "wave cancelled"})).await.unwrap());
    assert_eq!(r["state"], json!("Executing"), "requested is not cancelled");
    assert_eq!(gw.sync(&t, gw.sync_request()).await.cancel, vec![onsite]);
    let mut stop = gw.sync_request();
    stop.reports = vec![gw.report(onsite, State::Aborted)];
    gw.sync(&t, stop).await;
    assert_eq!(state(&t, onsite).await, "Aborted");
    assert!(gw.sync(&t, gw.sync_request()).await.cancel.is_empty());

    // ───── deadlines ─────
    let late = issue(&t, &t.admin, NewCommand { expires_in_secs: 1, ..move_tote(bot) }).await.unwrap().id;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(commands::expire_unsent(&k).await.unwrap() >= 1);
    assert_eq!(state(&t, late).await, "Expired");
    assert_eq!(gw.sync(&t, gw.sync_request()).await.commands.len(), 0, "a stale intent never reaches the floor");

    // ───── a gateway dies with work running: a person moves authority and fences it ─────
    let running = issue(&t, &t.admin, move_tote(bot)).await.unwrap().id;
    let queued_on_site = issue(&t, &t.admin, move_tote(bot)).await.unwrap().id;
    let got = gw.sync(&t, gw.sync_request()).await;
    assert_eq!(got.commands.len(), 2);
    let mut h = gw.sync_request();
    h.held = vec![running, queued_on_site];
    h.reports = vec![gw.report(running, State::Executing)];
    gw.sync(&t, h).await;
    let waiting = issue(&t, &t.admin, move_tote(bot)).await.unwrap().id;

    let (gw2_id, mut gw2) = gateway(&t, "GW-2", site, 8).await;
    assert!(matches!(api(&t, &viewer, "POST", "/authority/transfer", json!({"domain": "north", "to": gw2_id, "reason": "x"})).await, Err(Error::Forbidden(_))));
    let unheld = {
        let mut r = gw2.sync_request();
        r.epoch = 0;
        r
    };
    assert!(gw2.sync(&t, unheld).await.fenced, "a second gateway holds nothing until it is given it");
    assert!(authority::transfer(&k, &t.admin, "north", gw2_id, " ").await.is_err(), "the reason is required");
    let epoch = authority::transfer(&k, &t.admin, "north", gw2_id, "GW-1 lost power").await.expect("transferred");
    assert_eq!(epoch, 2);
    assert_eq!(state(&t, running).await, "OutcomeUnknown", "the new holder cannot know how it went");
    assert_eq!(state(&t, queued_on_site).await, "OutcomeUnknown", "the old one may have started it");
    assert_eq!(state(&t, waiting).await, "Queued", "what never left is the new holder's");

    // The old gateway comes back, and believes it still holds the domain.
    let mut stale = gw.sync_request();
    let done = gw.report(running, State::Completed);
    stale.reports = vec![Report { evidence: Some(json!({"it": "says so"})), ..done.clone() }];
    let answer = gw.sync(&t, stale).await;
    assert!(answer.fenced, "it is told at once");
    assert!(answer.lease.is_none() && answer.commands.is_empty(), "and gets nothing");
    assert_eq!(answer.reports_refused.len(), 1);
    assert_eq!(state(&t, running).await, "OutcomeUnknown", "what it says joins the history and changes nothing");
    let trail = json_of(api(&t, &t.admin, "GET", &format!("/commands/{running}"), Value::Null).await.unwrap());
    assert!(trail["history"].as_array().unwrap().iter().any(|e| e["step"] == "refuse"));

    gw2.epoch = epoch;
    let taken = gw2.sync(&t, gw2.sync_request()).await;
    assert!(!taken.fenced);
    assert!(taken.commands.is_empty(), "a gateway that is not Active gets no work");
    activate(&t, gw2_id).await;
    let taken = gw2.sync(&t, gw2.sync_request()).await;
    assert_eq!(taken.commands.iter().map(|c| c.id).collect::<Vec<_>>(), vec![waiting]);
    assert_eq!(taken.commands[0].epoch, 2, "reissued under the new authority");
    assert!(taken.lease.as_ref().is_some_and(|l| l.epoch == 2 && l.valid_until > Utc::now() + Duration::hours(71)), "with the site's offline limit");
    let names: Vec<String> = taken.devices.as_ref().unwrap().iter().map(|d| d.code.clone()).collect();
    assert!(names.contains(&"AMR-1".to_string()), "{names:?}");
    let mut same = gw2.sync_request();
    same.config_version = taken.config_version.clone();
    assert!(gw2.sync(&t, same).await.devices.is_none(), "unchanged, not sent again");

    // ───── identity ─────
    let body = serde_json::to_vec(&gw2.sync_request()).unwrap();
    let site_ep = k.hooks.site("ops").unwrap().clone();
    let time = gw2.tick();
    let good = gw2.signed(&t, &body, time, &gw2.key);
    assert_eq!(site_ep.handle(&k, good.clone()).await.unwrap().status, 200);
    assert_eq!(site_ep.handle(&k, good).await.unwrap().status, 401, "a request cannot be replayed");
    let forged = gw2.signed(&t, &body, gw2.tick(), &SigningKey::from_bytes(&[99; 32]));
    assert_eq!(site_ep.handle(&k, forged).await.unwrap().status, 401, "only its key signs for it");
    let mut altered = gw2.signed(&t, &body, gw2.tick(), &gw2.key);
    altered.body = b"{}".to_vec();
    assert_eq!(site_ep.handle(&k, altered).await.unwrap().status, 401, "nothing signed can change");
    let old_time = Utc::now().timestamp_millis() - 3_600_000;
    let skewed = site_ep.handle(&k, gw2.signed(&t, &body, old_time, &gw2.key)).await.unwrap();
    assert_eq!(skewed.status, 401);
    assert!(json_of(skewed)["server_time"].is_string(), "a site with a wrong clock learns the right one");
    let unsigned = site_ep.handle(&k, request(&t, None, "POST", "/sync", Map::new(), body.clone())).await.unwrap();
    assert_eq!(unsigned.status, 401);

    // ───── another workspace is another world ─────
    let other = workspace(&k, "south").await;
    let elsewhere = other.k.hooks.site("ops").unwrap().clone();
    let mut foreign = gw2.signed(&t, &body, gw2.tick(), &gw2.key);
    foreign.tenant = other.tenant;
    assert_eq!(elsewhere.handle(&k, foreign).await.unwrap().status, 401, "a key is its workspace's");
    assert!(matches!(api(&other, &other.admin, "GET", &format!("/commands/{waiting}"), Value::Null).await, Err(Error::NotFound(_))));
    assert_eq!(json_of(api(&other, &other.admin, "GET", "/commands", Value::Null).await.unwrap())["commands"], json!([]));
    assert!(devices::target(&mut other.k.db.tenant_tx(other.tenant).await.unwrap(), other.tenant, bot).await.is_err());

    // ───── a members-only site is closed to everyone else, whatever their roles ─────
    k.update(&t.admin, "Site", site, obj(json!({"restricted": true, "members": [{"user_email": "mo@north.test"}]})), None).await.expect("restricted");
    assert_eq!(json_of(api(&t, &viewer, "GET", "/overview", Value::Null).await.unwrap())["sites"], json!([]));
    assert!(matches!(api(&t, &viewer, "GET", &format!("/commands/{waiting}"), Value::Null).await, Err(Error::NotFound(_))));
    assert_eq!(json_of(api(&t, &manager, "GET", "/overview", Value::Null).await.unwrap())["sites"].as_array().unwrap().len(), 1);
    k.update(&t.admin, "Site", site, obj(json!({"restricted": false})), None).await.unwrap();

    // ───── telemetry ─────
    let now = Utc::now();
    let boot_id = Uuid::now_v7();
    let m = |seq: i64, value: f64, quality: Quality| Measurement {
        device_id: bot,
        metric: "battery.pct".into(),
        value,
        unit: Some("%".into()),
        source_time: now - Duration::seconds(60 - seq),
        boot: boot_id,
        seq,
        quality,
        class: Class::Sample,
    };
    let mut batch = gw2.sync_request();
    batch.measurements = vec![m(1, 90.0, Quality::Good), m(2, 80.0, Quality::Good), m(3, 700.0, Quality::Bad)];
    assert_eq!(gw2.sync(&t, batch.clone()).await.measurements_stored, 3);
    assert_eq!(gw2.sync(&t, batch).await.measurements_stored, 0, "a batch sent twice lands once");
    assert!(api(&t, &t.admin, "GET", "/series", Value::Null).await.is_err(), "device and metric are required");
    let from = (now - Duration::hours(1)).to_rfc3339();
    let mut q = request(&t, Some(&t.admin), "GET", "/series", Map::new(), vec![]);
    q.query = obj(json!({"device": bot, "metric": "battery.pct", "from": from}));
    let rows = json_of(k.hooks.site("ops").unwrap().handle(&k, q).await.unwrap())["series"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.iter().map(|r| r["samples"].as_i64().unwrap()).sum::<i64>(), 2, "only good values count, and only once");
    assert_eq!(rows.iter().map(|r| r["bad"].as_i64().unwrap()).sum::<i64>(), 1, "bad ones are counted as bad");
    let latest = json_of(api(&t, &t.admin, "GET", &format!("/devices/{bot}/latest"), Value::Null).await.unwrap());
    assert_eq!(latest["metrics"][0]["value"].as_f64(), Some(80.0), "the newest good value, not the bad one that came after");

    // A device of another site is not this site's to speak for; a wrong clock is kept and marked.
    let other_site = id_of(&k.insert(&t.admin, "Site", obj(json!({"site_name": "South", "site_code": "south"}))).await.unwrap());
    let theirs =
        id_of(&k.insert(&t.admin, "Device", obj(json!({"device_name": "Far", "device_code": "FAR-1", "kind": "Sensor", "site": other_site}))).await.unwrap());
    let mut spoof = gw2.sync_request();
    spoof.measurements = vec![
        Measurement { device_id: theirs, ..m(10, 1.0, Quality::Good) },
        Measurement { source_time: now + Duration::days(4000), ..m(11, 2.0, Quality::Good) },
    ];
    assert_eq!(gw2.sync(&t, spoof).await.measurements_stored, 1);
    let mut tx = k.db.tenant_tx(t.tenant).await.unwrap();
    let (quality, stored_for): (i16, Uuid) = sqlx::query_as("select quality, device_id from op_measurement where seq = 11").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((quality, stored_for), (Quality::Bad.code(), bot));
    assert_eq!(telemetry::quality_name(quality), "bad");

    // Retention drops a day, and nothing today.
    assert_eq!(telemetry::purge(&k, Utc::now()).await.unwrap(), 0);
    let future = Utc::now() + Duration::days(telemetry::retention_days() + 3);
    assert!(telemetry::purge(&k, future).await.unwrap() >= 1);

    // ───── a business fact recorded at the site is applied once ─────
    let fact =
        Fact { fact_id: Uuid::now_v7(), kind: "wms.task.completed".into(), at_source: Utc::now(), actor: Some("pia".into()), body: json!({"task": "T1"}) };
    let state_fact = Fact {
        fact_id: Uuid::now_v7(),
        kind: "equipment.state".into(),
        at_source: Utc::now(),
        actor: None,
        body: json!({"device": bot, "state": "Stopped", "reason": "jam"}),
    };
    let mut f = gw2.sync_request();
    f.facts = vec![fact.clone(), state_fact.clone()];
    assert_eq!(gw2.sync(&t, f.clone()).await.facts_done.len(), 2);
    assert_eq!(gw2.sync(&t, f).await.facts_done.len(), 2, "acknowledged again, so the site can forget it");
    assert_eq!(events(&t, "ops.fact.wms.task.completed").await, 1, "and applied once");
    let mut tx = k.db.tenant_tx(t.tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from op_equipment_event where device_id = $1").bind(bot).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(n, 1);
    let mut wrong = gw2.sync_request();
    wrong.facts = vec![Fact { kind: "not a kind!".into(), ..fact }];
    let site_ep = k.hooks.site("ops").unwrap().clone();
    let body = serde_json::to_vec(&wrong).unwrap();
    assert!(site_ep.handle(&k, gw2.signed(&t, &body, gw2.tick(), &gw2.key)).await.is_err());

    // ───── a site on another contract exchanges nothing ─────
    let mut newer = gw2.sync_request();
    newer.contract = VERSION + 1;
    let body = serde_json::to_vec(&newer).unwrap();
    assert!(matches!(site_ep.handle(&k, gw2.signed(&t, &body, gw2.tick(), &gw2.key)).await, Err(Error::Conflict(_))));

    // ───── quarantine: it may still report, it gets no work, and what it held is held by nobody ─────
    devices::set_lifecycle(&k, &t.admin, gw2_id, Lifecycle::Quarantined, "tamper switch").await.expect("quarantined");
    let q = gw2.sync(&t, gw2.sync_request()).await;
    assert!(q.fenced && q.commands.is_empty());
    assert!(matches!(issue(&t, &t.admin, NewCommand { operation: "warehouse.move".into(), ..move_tote(gw2_id) }).await, Err(Error::Invalid(_))));
    let holders = json_of(api(&t, &t.admin, "GET", "/authority", Value::Null).await.unwrap());
    assert_eq!(holders["domains"][0]["holder"], Value::Null);
    assert_eq!(holders["domains"][0]["epoch"], json!(3));
    assert!(
        matches!(devices::set_lifecycle(&k, &t.admin, gw2_id, Lifecycle::Active, "looks fine").await, Err(Error::Invalid(_))),
        "it comes back only with a new key"
    );
    devices::set_lifecycle(&k, &t.admin, gw_id, Lifecycle::Retired, "scrapped").await.expect("retired");

    // Equipment behind a gateway has no key of its own: naming the gateway provisions it.
    let behind = id_of(
        &t.k.insert(
            &t.admin,
            "Device",
            obj(json!({"device_name": "Press", "device_code": "PRESS-9", "kind": "Machine", "site": site, "operations": [{"operation": "machine.start_job"}]})),
        )
        .await
        .unwrap(),
    );
    devices::set_lifecycle(&k, &t.admin, behind, Lifecycle::Claimed, "ours").await.unwrap();
    let alone = devices::set_lifecycle(&k, &t.admin, behind, Lifecycle::Provisioned, "says so").await;
    assert!(matches!(alone, Err(Error::Invalid(ref m)) if m.contains("enrolling a key")), "nothing is provisioned by saying so: {alone:?}");
    t.k.update(&t.admin, "Device", behind, obj(json!({"gateway": gw2_id})), None).await.expect("reached through it");
    devices::set_lifecycle(&k, &t.admin, behind, Lifecycle::Provisioned, "reached through the gateway").await.expect("provisioned");
    let dead = gw.send(&t, gw.sync_request()).await;
    assert_eq!(dead.status, 401, "a retired device's keys are gone");
}

/// A device that exists and is not active.
async fn idle_free(t: &T, site: Uuid) -> Uuid {
    id_of(
        &t.k.insert(&t.admin, "Device", obj(json!({"device_name": "Idle", "device_code": format!("IDLE-{}", Uuid::now_v7()), "kind": "Robot", "site": site})))
            .await
            .unwrap(),
    )
}

async fn person(t: &T, email: &str, roles: &[&str]) -> Ctx {
    let id =
        t.k.create_user(&t.admin, email, "Someone", Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false)
            .await
            .expect("user");
    t.k.load_ctx(t.tenant, id, None, "it".into()).await.unwrap()
}
