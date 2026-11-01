//! The real binary against a real platform (`FERRUM_SITE_TEST_DATABASE_URL`, reset first), killed
//! and cut off on purpose. Run with `--features failpoints`, which adds the crashes.
//!
//! What must hold through every one of these: a mission is dispatched at most once, everything
//! established reaches the platform, and nothing claims more than was established. The simulator's
//! file stands for the equipment's own memory, so it survives what the site does not, and one line
//! in it means one mission.

#![cfg(feature = "failpoints")]

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{Duration, Utc};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, SiteRequest};
use ferrum_meta::Catalog;
use ferrum_ops::commands::{self, NewCommand};
use ferrum_ops::contract::Lifecycle;
use ferrum_ops::{authority, devices};
use ferrum_site::journal::Journal;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration as StdDuration, Instant};
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

struct Platform {
    k: Arc<Kernel>,
    down: AtomicBool,
}

async fn edge(State(p): State<Arc<Platform>>, UrlPath((tenant, package, path)): UrlPath<(String, String, String)>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    if p.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let (tenant, _) = p.k.tenant_by_slug(&tenant).await.unwrap();
    let mut map: Map<String, Value> = headers.iter().filter_map(|(k, v)| Some((k.to_string(), json!(v.to_str().ok()?)))).collect();
    map.insert("x-ferrum-uri".into(), json!(uri.path()));
    let req = SiteRequest {
        tenant,
        user: None,
        method: "POST".into(),
        path: format!("/{path}"),
        query: Map::new(),
        headers: map,
        body: body.to_vec(),
        client_ip: "127.0.0.1".into(),
    };
    let site = p.k.hooks.site(&package).unwrap().clone();
    match site.handle(&p.k, req).await {
        Ok(r) => (StatusCode::from_u16(r.status).unwrap(), r.body).into_response(),
        Err(Error::Conflict(m)) => (StatusCode::CONFLICT, axum::Json(json!({"error": m}))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, axum::Json(json!({"error": e.to_string()}))).into_response(),
    }
}

struct World {
    p: Arc<Platform>,
    url: String,
}

async fn world() -> World {
    let url = std::env::var("FERRUM_SITE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_site_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().unwrap();
    packages.extend(ferrum_ops::packages().unwrap());
    packages.extend(ferrum_edge::packages().unwrap());
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    hooks.scoped(&["ops".to_string()], ferrum_ops::register);
    hooks.scoped(&["edge".to_string()], ferrum_edge::register);
    let k = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let p = Arc::new(Platform { k, down: AtomicBool::new(false) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new().route("/api/site/{tenant}/{package}/{path}", post(edge)).with_state(p.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    World { p, url: format!("http://{addr}") }
}

struct Workspace {
    k: Arc<Kernel>,
    slug: String,
    tenant: Uuid,
    admin: Ctx,
    site: Uuid,
    gateway: Uuid,
    robot: Uuid,
    station: Uuid,
}

async fn workspace(w: &World, slug: &str) -> Workspace {
    let k = w.p.k.clone();
    let email = format!("admin@{slug}.test");
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: email.clone(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["ops".into(), "edge".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login(slug, &email, "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let site = id_of(&k.insert(&admin, "Site", obj(json!({"site_name": slug, "site_code": slug}))).await.unwrap());
    let gateway = id_of(&k.insert(&admin, "Device", obj(json!({"device_name": "GW", "device_code": "GW", "kind": "Gateway", "site": site}))).await.unwrap());
    devices::set_lifecycle(&k, &admin, gateway, Lifecycle::Claimed, "ours").await.unwrap();
    let robot = equipment(&k, &admin, site, "AMR-1", "warehouse.move").await;
    let station = equipment(&k, &admin, site, "PACK-1", "pick").await;
    let code = |device| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { ferrum_ops::identity::new_enrollment(&k, &admin, device).await.unwrap().0 }
    };
    for d in [robot, station] {
        let c = code(d).await;
        let key = hex_key(d);
        ferrum_ops::identity::enroll(&k, tenant, &ferrum_ops::contract::sign::Enroll { code: c, public_key: key }).await.unwrap();
        devices::set_lifecycle(&k, &admin, d, Lifecycle::Commissioned, "checked").await.unwrap();
        devices::set_lifecycle(&k, &admin, d, Lifecycle::Active, "in service").await.unwrap();
    }
    Workspace { k, slug: slug.into(), tenant, admin, site, gateway, robot, station }
}

fn hex_key(seed: Uuid) -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(seed.as_bytes());
    hex::encode(ed25519_dalek::SigningKey::from_bytes(&bytes).verifying_key().to_bytes())
}

async fn equipment(k: &Arc<Kernel>, admin: &Ctx, site: Uuid, code: &str, op: &str) -> Uuid {
    let d = k
        .insert(admin, "Device", obj(json!({"device_name": code, "device_code": code, "kind": "Robot", "site": site, "operations": [{"operation": op}]})))
        .await
        .unwrap();
    devices::set_lifecycle(k, admin, id_of(&d), Lifecycle::Claimed, "ours").await.unwrap();
    id_of(&d)
}

struct Site {
    dir: PathBuf,
    config: PathBuf,
    console: String,
    child: Option<Child>,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl Site {
    fn new(w: &World, ws: &Workspace, extra: &str) -> Site {
        Site::with_fleet(w, ws, extra, "")
    }

    fn with_fleet(w: &World, ws: &Workspace, extra: &str, fleet: &str) -> Site {
        let dir = std::env::temp_dir().join(format!("ferrum-site-it-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let console = format!("127.0.0.1:{}", free_port());
        let config = dir.join("site.yaml");
        std::fs::write(
            &config,
            format!(
                "server: {}\ntenant: {}\nstate_dir: {}\nsync_secs: 1\nwork_millis: 50\nread_secs: 1\nconsole: \"{console}\"\nunknown_after_secs: 2\nadapter_timeout_secs: 2\n{extra}\nadapters:\n  - {{name: fleet, kind: sim, devices: [AMR-1], concurrency: 4, mission_millis: 300, silent_file: {}{fleet}}}\n  - {{name: people, kind: operator, devices: [PACK-1]}}\n",
                w.url,
                ws.slug,
                dir.display(),
                dir.join("silent").display()
            ),
        )
        .unwrap();
        Site { dir, config, console, child: None }
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ferrum-site")).args(args).arg("--config").arg(&self.config).output().unwrap()
    }

    async fn enroll(&self, ws: &Workspace) {
        let (code, _) = ferrum_ops::identity::new_enrollment(&ws.k, &ws.admin, ws.gateway).await.unwrap();
        let exe = env!("CARGO_BIN_EXE_ferrum-site").to_string();
        let config = self.config.clone();
        let out =
            tokio::task::spawn_blocking(move || Command::new(exe).args(["enroll", "--code", &code, "--config"]).arg(config).output().unwrap()).await.unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    fn start(&mut self, crash_at: Option<&str>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ferrum-site"));
        cmd.args(["run", "--config"]).arg(&self.config).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(point) = crash_at {
            cmd.env("FERRUM_SITE_CRASH_AT", point);
        }
        self.child = Some(cmd.spawn().unwrap());
    }

    /// The power goes.
    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Waits for the process to die by itself, at its crash point.
    async fn crashed(&mut self) {
        let mut child = self.child.take().unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            assert!(start.elapsed() < StdDuration::from_secs(20), "it never reached its crash point");
            tokio::time::sleep(StdDuration::from_millis(50)).await;
        }
    }

    fn missions(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.join("fleet.sim")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn missions_for(&self, id: Uuid) -> usize {
        self.missions().iter().filter(|l| l["mission"] == json!(id.to_string())).count()
    }

    fn journal(&self) -> Journal {
        Journal::open(&self.dir.join("journal.db")).unwrap()
    }

    fn fleet_silent(&self, silent: bool) {
        let f = self.dir.join("silent");
        if silent {
            std::fs::write(f, "").unwrap();
        } else {
            let _ = std::fs::remove_file(f);
        }
    }

    async fn console(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
        let (addr, path, method, token, body) =
            (self.console.clone(), path.to_string(), method.to_string(), token.map(str::to_string), body.map(|b| b.to_string()));
        tokio::task::spawn_blocking(move || raw_http(&addr, &method, &path, token.as_deref(), body.as_deref())).await.unwrap()
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// HTTP over a socket: enough for a local console, with no client to pull in.
fn raw_http(addr: &str, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, Value) {
    use std::io::{Read, Write};
    let Ok(mut s) = std::net::TcpStream::connect(addr) else { return (0, Value::Null) };
    let body = body.unwrap_or("");
    let auth = token.map(|t| format!("authorization: Bearer {t}\r\n")).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nhost: x\r\nconnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{auth}\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let text = out.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(text).unwrap_or(Value::Null))
}

async fn until(what: &str, seconds: u64, mut check: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < StdDuration::from_secs(seconds), "waited {seconds} s for: {what}");
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
}

async fn issue(ws: &Workspace, target: Uuid, operation: &str, payload: Value, expires: i64) -> Uuid {
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let issued = commands::issue(
        &mut tx,
        &ws.admin,
        NewCommand { target, operation: operation.into(), schema_version: 1, payload, expires_in_secs: expires, ..Default::default() },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    issued.id
}

async fn described(ws: &Workspace, id: Uuid) -> Value {
    commands::describe(&ws.k, &ws.admin, id).await.unwrap()
}

async fn state_of(ws: &Workspace, id: Uuid) -> String {
    described(ws, id).await["state"].as_str().unwrap().to_string()
}

async fn reaches(ws: &Workspace, id: Uuid, state: &str, seconds: u64) {
    let what = format!("{id} to be {state}");
    until(&what, seconds, async || state_of(ws, id).await == state).await;
}

async fn activate_gateway(ws: &Workspace) {
    let k = &ws.k;
    devices::set_lifecycle(k, &ws.admin, ws.gateway, Lifecycle::Commissioned, "checked").await.unwrap();
    devices::set_lifecycle(k, &ws.admin, ws.gateway, Lifecycle::Active, "in service").await.unwrap();
}

/// A site that has done its first exchange and holds its lease.
async fn running(w: &World, ws: &Workspace, extra: &str) -> Site {
    let mut site = Site::new(w, ws, extra);
    site.enroll(ws).await;
    activate_gateway(ws).await;
    site.start(None);
    until("the first exchange", 15, async || site.console("GET", "/api/status", None, None).await.1["lease_until"].is_string()).await;
    site
}

#[test]
fn the_site_survives_what_sites_go_through() {
    // On a thread of its own: the kernel's futures are large, and this one is not spawned.
    let run = std::thread::Builder::new().stack_size(64 << 20).spawn(|| {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
        rt.block_on(scenario());
    });
    run.unwrap().join().unwrap();
}

async fn scenario() {
    let w = world().await;
    a_mission_runs_and_the_platform_hears(&w).await;
    the_site_works_while_the_platform_is_gone(&w).await;
    a_crash_before_the_adapter_answers_does_not_lose_or_repeat(&w).await;
    a_crash_after_the_adapter_answers_does_not_repeat(&w).await;
    a_lost_answer_repeats_nothing(&w).await;
    equipment_that_cannot_say_leaves_the_outcome_unknown(&w).await;
    a_fleet_with_no_memory_is_not_sent_twice(&w).await;
    what_the_lease_and_the_disk_allow(&w).await;
    a_replaced_gateway_stops(&w).await;
    people_at_the_console(&w).await;
    a_restore_is_not_believed(&w).await;
    a_gateway_is_told_what_to_be_and_carries_on_alone(&w).await;
}

async fn a_mission_runs_and_the_platform_hears(w: &World) {
    let ws = workspace(w, "plain").await;
    let site = running(w, &ws, "").await;
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "T-481"}), 60).await;
    reaches(&ws, c, "Completed", 15).await;
    let seen = described(&ws, c).await;
    assert_eq!(seen["outcome"]["evidence"]["simulated"], json!(true), "the ending carries what established it");
    let states: Vec<&str> = seen["history"].as_array().unwrap().iter().map(|e| e["state"].as_str().unwrap()).collect();
    assert!(states.contains(&"Accepted") && states.contains(&"Completed"), "{states:?}");
    assert_eq!(site.missions_for(c), 1);
    until("measurements", 10, async || {
        let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar("select count(*) from op_measurement").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n > 0
    })
    .await;
}

async fn the_site_works_while_the_platform_is_gone(w: &World) {
    let ws = workspace(w, "offline").await;
    let site = running(w, &ws, "").await;
    // Sent to the site, then the link goes. It runs them, keeps what happened, and says so later.
    let early = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "A"}), 60).await;
    let slow = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "B"}), 2).await;
    until("the site to hold them", 10, async || site.journal().entry(early).unwrap().is_some() && site.journal().entry(slow).unwrap().is_some()).await;
    w.p.down.store(true, Ordering::SeqCst);
    until("the site to notice", 10, async || site.console("GET", "/api/status", None, None).await.1["link_up"] == json!(false)).await;
    until("the work to end offline", 10, async || site.missions_for(early) == 1 && site.journal().entry(early).unwrap().unwrap().state.as_str() == "Completed")
        .await;
    assert_eq!(state_of(&ws, early).await, "Queued", "the platform cannot know yet, and says only what it knows");
    let unsent = site.console("GET", "/api/status", None, None).await.1["unsent_reports"].as_i64().unwrap();
    assert!(unsent >= 2, "kept for later: {unsent}");
    w.p.down.store(false, Ordering::SeqCst);
    reaches(&ws, early, "Completed", 20).await;
    let history = described(&ws, early).await;
    let source = history["history"].as_array().unwrap().iter().find(|e| e["state"] == "Completed").unwrap()["at_source"]
        .as_str()
        .unwrap()
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    let received = history["history"].as_array().unwrap().iter().find(|e| e["state"] == "Completed").unwrap()["received_at"]
        .as_str()
        .unwrap()
        .parse::<chrono::DateTime<Utc>>()
        .unwrap();
    assert!(received > source + Duration::milliseconds(200), "when it happened and when it was heard are told apart");
    assert_eq!(site.missions_for(early), 1, "nothing was sent twice around the outage");
}

async fn a_crash_before_the_adapter_answers_does_not_lose_or_repeat(w: &World) {
    let ws = workspace(w, "intent").await;
    let mut site = running(w, &ws, "").await;
    site.kill();
    site.start(Some("after_intent"));
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "X"}), 60).await;
    site.crashed().await;
    assert_eq!(site.missions_for(c), 0, "the crash came before the equipment heard");
    assert_eq!(site.journal().in_doubt().unwrap().len(), 1, "and the intent survived it");
    site.start(None);
    reaches(&ws, c, "Completed", 20).await;
    assert_eq!(site.missions_for(c), 1, "the fleet could say it never saw it, so it went once");
}

async fn a_crash_after_the_adapter_answers_does_not_repeat(w: &World) {
    let ws = workspace(w, "answer").await;
    let mut site = running(w, &ws, "").await;
    site.kill();
    site.start(Some("after_dispatch"));
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "Y"}), 60).await;
    site.crashed().await;
    assert_eq!(site.missions_for(c), 1, "the equipment took it, and the journal never heard");
    site.start(None);
    reaches(&ws, c, "Completed", 20).await;
    assert_eq!(site.missions_for(c), 1, "this is the crash a blind retry would double");
}

async fn a_lost_answer_repeats_nothing(w: &World) {
    let ws = workspace(w, "lost").await;
    let mut site = running(w, &ws, "").await;
    let done = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "Z"}), 60).await;
    reaches(&ws, done, "Completed", 15).await;
    site.kill();
    site.start(Some("after_exchange"));
    site.crashed().await;
    site.start(None);
    let again = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "ZZ"}), 60).await;
    reaches(&ws, again, "Completed", 20).await;
    assert_eq!((site.missions_for(done), site.missions_for(again)), (1, 1));
    let completed = described(&ws, again).await["history"].as_array().unwrap().iter().filter(|e| e["state"] == "Completed").count();
    assert_eq!(completed, 1);
}

async fn equipment_that_cannot_say_leaves_the_outcome_unknown(w: &World) {
    let ws = workspace(w, "silent").await;
    let mut site = running(w, &ws, "").await;
    site.kill();
    site.start(Some("after_dispatch"));
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "Q"}), 60).await;
    site.crashed().await;
    site.fleet_silent(true);
    site.start(None);
    reaches(&ws, c, "OutcomeUnknown", 20).await;
    assert_eq!(site.missions_for(c), 1, "not retried on a guess");
    // The fleet comes back, and its own answer settles what nobody could say.
    site.fleet_silent(false);
    reaches(&ws, c, "Completed", 20).await;
    assert_eq!(site.missions_for(c), 1);

    // A fleet that stops answering mid-mission: silence is not success.
    let c2 = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "R"}), 60).await;
    until("it to start", 10, async || site.missions_for(c2) == 1).await;
    site.fleet_silent(true);
    let c3 = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "S"}), 60).await;
    reaches(&ws, c3, "OutcomeUnknown", 20).await;
    assert_eq!(site.missions_for(c3), 0, "an unanswered dispatch is not a sent one, but nobody can prove that here");
    site.fleet_silent(false);

    // A person says what happened, on the platform, and the site stops being in doubt.
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    ferrum_ops::commands::reconcile(&mut tx, &ws.admin, c3, ferrum_ops::contract::State::Failed, "It never reached the fleet").await.unwrap();
    tx.commit().await.unwrap();
    until("the site to hear", 10, async || site.console("GET", "/api/status", None, None).await.1["unknown"] == json!(0)).await;
}

async fn a_fleet_with_no_memory_is_not_sent_twice(w: &World) {
    let ws = workspace(w, "nomemory").await;
    let mut site = Site::with_fleet(w, &ws, "", ", cannot_find: true");
    site.enroll(&ws).await;
    activate_gateway(&ws).await;
    site.start(Some("after_dispatch"));
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "M"}), 60).await;
    site.crashed().await;
    assert_eq!(site.missions_for(c), 1, "it heard, and the journal did not");
    site.start(None);
    // Nothing can be asked, so nothing is resent. Its own later answer is what settles it.
    reaches(&ws, c, "Completed", 20).await;
    assert_eq!(site.missions_for(c), 1, "a retry here would run the mission twice");
    let history = described(&ws, c).await;
    assert!(history["history"].as_array().unwrap().iter().any(|e| e["state"] == "OutcomeUnknown"), "and it said it did not know: {history}");
}

async fn what_the_lease_and_the_disk_allow(w: &World) {
    let ws = workspace(w, "limits").await;
    let mut site = running(w, &ws, "").await;
    site.kill();
    // The site holds work, and was offline past its allowance when it comes back up.
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "L"}), 60).await;
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let taken = commands::take_for_site(&mut tx, ws.tenant, &ws.slug, 1, 10).await.unwrap();
    tx.commit().await.unwrap();
    {
        let j = site.journal();
        j.receive(&taken[0], 0).unwrap();
        j.set("lease_until", &(Utc::now() - Duration::minutes(1)).to_rfc3339()).unwrap();
    }
    w.p.down.store(true, Ordering::SeqCst);
    site.start(None);
    until("the console", 10, async || site.console("GET", "/api/status", None, None).await.0 == 200).await;
    let status = site.console("GET", "/api/status", None, None).await.1;
    assert!(status["stopped"].as_str().unwrap().contains("Offline past"), "{status}");
    tokio::time::sleep(StdDuration::from_millis(800)).await;
    assert_eq!(site.missions_for(c), 0, "past the allowance nothing starts");
    assert_eq!(site.journal().entry(c).unwrap().unwrap().held_reason.as_deref().map(|h| h.contains("Offline past")), Some(true), "and it says why");
    w.p.down.store(false, Ordering::SeqCst);
    reaches(&ws, c, "Completed", 20).await;
    site.kill();

    // Almost no disk left: it takes no new work, and says why.
    let starving = workspace(w, "disk").await;
    let mut tight = Site::new(w, &starving, "reserve_mb: 900000000");
    tight.enroll(&starving).await;
    activate_gateway(&starving).await;
    tight.start(None);
    until("the first exchange", 15, async || tight.console("GET", "/api/status", None, None).await.1["lease_until"].is_string()).await;
    let status = tight.console("GET", "/api/status", None, None).await.1;
    assert!(status["stopped"].as_str().unwrap().contains("disk"), "{status}");
    let parked = issue(&starving, starving.robot, "warehouse.move", json!({"tote": "D"}), 60).await;
    tokio::time::sleep(StdDuration::from_millis(2500)).await;
    assert_eq!(state_of(&starving, parked).await, "Queued", "it was not even taken");
    assert_eq!(tight.missions_for(parked), 0);
}

async fn a_replaced_gateway_stops(w: &World) {
    let ws = workspace(w, "fence").await;
    let site = running(w, &ws, "").await;
    let spare = id_of(
        &ws.k.insert(&ws.admin, "Device", obj(json!({"device_name": "Spare", "device_code": "GW-2", "kind": "Gateway", "site": ws.site}))).await.unwrap(),
    );
    devices::set_lifecycle(&ws.k, &ws.admin, spare, Lifecycle::Claimed, "ours").await.unwrap();
    let (code, _) = ferrum_ops::identity::new_enrollment(&ws.k, &ws.admin, spare).await.unwrap();
    ferrum_ops::identity::enroll(&ws.k, ws.tenant, &ferrum_ops::contract::sign::Enroll { code, public_key: hex_key(spare) }).await.unwrap();
    authority::transfer(&ws.k, &ws.admin, &ws.slug, spare, "the old one is being replaced").await.unwrap();
    until("the old gateway to stop", 10, async || {
        site.console("GET", "/api/status", None, None).await.1["stopped"].as_str().is_some_and(|s| s.contains("Another gateway"))
    })
    .await;
}

async fn people_at_the_console(w: &World) {
    let ws = workspace(w, "people").await;
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let (user, may): (Uuid, Vec<String>) = (ws.admin.user, vec!["Ops Manager".to_string()]);
    sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, $3) on conflict do nothing")
        .bind(ws.tenant)
        .bind(user)
        .bind(&may[0])
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into op_operator_pin (tenant_id, site_id, user_id, pin_hash) values ($1, $2, $3, $4)")
        .bind(ws.tenant)
        .bind(ws.site)
        .bind(user)
        .bind(ferrum_ops::identity::pin_hash("246810"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let site = running(w, &ws, "").await;
    until("the operator list", 10, async || !site.journal().operators().unwrap().is_empty()).await;

    assert_eq!(site.console("GET", "/api/work", None, None).await.0, 401, "nothing without a PIN");
    assert_eq!(site.console("POST", "/api/login", None, Some(json!({"pin": "000000"}))).await.0, 401);
    let (status, signed) = site.console("POST", "/api/login", None, Some(json!({"pin": "246810"}))).await;
    assert_eq!(status, 200);
    let token = signed["token"].as_str().unwrap().to_string();

    // A task for a person, confirmed by a scan. Works with the link down.
    let c = issue(&ws, ws.station, "pick", json!({"item": "BOLT-8", "qty": 4, "from": "A-01-03", "verify": "A-01-03"}), 60).await;
    until("the task", 15, async || site.console("GET", "/api/work", Some(&token), None).await.1["tasks"].as_array().is_some_and(|t| !t.is_empty())).await;
    w.p.down.store(true, Ordering::SeqCst);
    let path = format!("/api/tasks/{c}/");
    assert_eq!(site.console("POST", &format!("{path}start"), Some(&token), Some(json!({}))).await.0, 204);
    let (wrong, why) = site.console("POST", &format!("{path}done"), Some(&token), Some(json!({"scan": "A-01-04"}))).await;
    assert_eq!(wrong, 409, "a wrong scan completes nothing: {why}");
    assert_eq!(site.console("POST", &format!("{path}done"), Some(&token), Some(json!({"scan": "A-01-03"}))).await.0, 204);
    until("it to finish offline", 10, async || site.journal().entry(c).unwrap().unwrap().state.as_str() == "Completed").await;
    // What happened on the floor is written down here, and goes up with everything else.
    assert_eq!(site.console("POST", "/api/floor", None, Some(json!({"reference": "RUN-9", "kind": "resume"}))).await.0, 401);
    assert_eq!(site.console("POST", "/api/floor", Some(&token), Some(json!({"reference": "RUN-9", "kind": "report"}))).await.0, 422, "how many came off?");
    assert_eq!(site.console("POST", "/api/floor", Some(&token), Some(json!({"reference": "RUN-9", "kind": "pause"}))).await.0, 422, "why it stopped");
    assert_eq!(site.console("POST", "/api/floor", Some(&token), Some(json!({"reference": " ", "kind": "resume"}))).await.0, 422, "which run");
    assert_eq!(
        site.console("POST", "/api/floor", Some(&token), Some(json!({"reference": "RUN-9", "kind": "report", "good": "12", "lot": "W-1"}))).await.0,
        204
    );
    w.p.down.store(false, Ordering::SeqCst);
    reaches(&ws, c, "Completed", 20).await;
    until("the report to arrive", 10, async || {
        let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
        let body: Option<Value> = sqlx::query_scalar("select body from op_fact where kind = 'mes.event'").fetch_optional(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        body.is_some_and(|b| b["reference"] == json!("RUN-9") && b["good"] == json!(12.0))
    })
    .await;
    assert_eq!(described(&ws, c).await["outcome"]["evidence"]["found"]["scan"], json!("A-01-03"), "what the person scanned is the evidence");
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let wrong_scans: i64 = sqlx::query_scalar("select count(*) from op_fact where kind = 'site.scan.mismatch'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(wrong_scans, 1, "and the wrong one was kept");

    // An unknown outcome is settled by a person, on the site, and that reaches the platform.
    site.fleet_silent(true);
    let u = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "U"}), 60).await;
    reaches(&ws, u, "OutcomeUnknown", 20).await;
    let path = format!("/api/commands/{u}/settle");
    assert_eq!(site.console("POST", &path, Some(&token), Some(json!({"to": "Failed", "found": " "}))).await.0, 409, "say what you found");
    assert_eq!(site.console("POST", &path, Some(&token), Some(json!({"to": "Failed", "found": "Still in its bin"}))).await.0, 204);
    reaches(&ws, u, "Failed", 15).await;
    site.fleet_silent(false);
}

async fn a_restore_is_not_believed(w: &World) {
    let ws = workspace(w, "restore").await;
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, 'Ops Manager') on conflict do nothing")
        .bind(ws.tenant)
        .bind(ws.admin.user)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into op_operator_pin (tenant_id, site_id, user_id, pin_hash) values ($1, $2, $3, $4)")
        .bind(ws.tenant)
        .bind(ws.site)
        .bind(ws.admin.user)
        .bind(ferrum_ops::identity::pin_hash("135790"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut site = running(w, &ws, "").await;
    until("the operator list", 10, async || !site.journal().operators().unwrap().is_empty()).await;
    let backup = site.dir.join("backup.db");
    site.fleet_silent(true);
    let c = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "B1"}), 60).await;
    reaches(&ws, c, "OutcomeUnknown", 20).await;
    let out = site.cli(&["backup", "--to", backup.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    site.fleet_silent(false);
    site.kill();
    let out = site.cli(&["restore", "--from", backup.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    site.start(None);
    until("the console", 10, async || site.console("GET", "/api/status", None, None).await.0 == 200).await;
    let status = site.console("GET", "/api/status", None, None).await.1;
    assert!(status["stopped"].as_str().unwrap().contains("Restored"), "{status}");
    let before = site.missions().len();
    let parked = issue(&ws, ws.robot, "warehouse.move", json!({"tote": "B2"}), 60).await;
    until("it to wait in the journal", 10, async || site.journal().entry(parked).unwrap().is_some()).await;
    tokio::time::sleep(StdDuration::from_millis(600)).await;
    assert_eq!(site.missions().len(), before, "a restored site starts nothing until a person says the floor matches");
    let (_, signed) = site.console("POST", "/api/login", None, Some(json!({"pin": "135790"}))).await;
    let token = signed["token"].as_str().unwrap().to_string();
    let unknown = site.console("GET", "/api/work", Some(&token), None).await.1["unknown"].as_array().unwrap().clone();
    for u in &unknown {
        let id = u["id"].as_str().unwrap();
        site.console("POST", &format!("/api/commands/{id}/settle"), Some(&token), Some(json!({"to": "Failed", "found": "Walked the floor: not done"}))).await;
    }
    assert_eq!(site.console("POST", "/api/resume", Some(&token), Some(json!({"found": "Walked aisles 1 to 4"}))).await.0, 204);
    reaches(&ws, parked, "Completed", 20).await;
}

/// A machine on TCP that says what it is told to, to whoever is connected. It outlives the site.
struct Machine {
    say: tokio::sync::broadcast::Sender<String>,
    address: String,
}

async fn machine() -> Machine {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (say, _) = tokio::sync::broadcast::channel::<String>(16);
    let lines = say.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut heard = lines.subscribe();
            tokio::spawn(async move {
                while let Ok(line) = heard.recv().await {
                    if socket.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    Machine { say, address }
}

/// What the platform has been told. There is no background dispatcher here, so this is it.
async fn alarms(ws: &Workspace) -> Vec<(String, String)> {
    while ws.k.dispatch_outbox_once().await.unwrap() > 0 {}
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    sqlx::query_as("select summary, status from e_edge_alarm where tenant_id = $1 order by raised_at").bind(ws.tenant).fetch_all(&mut *tx).await.unwrap()
}

/// All of it at once, with the real binary: a gateway takes its definition and a reviewed flow from
/// the platform, reads a device with no local configuration, and the flow goes on deciding through
/// an outage and a power cut, from what it last heard and what it remembered.
async fn a_gateway_is_told_what_to_be_and_carries_on_alone(w: &World) {
    let ws = workspace(w, "managed").await;
    let (k, admin) = (&ws.k, &ws.admin);
    let plc = machine().await;

    let definition = format!(
        "name: press\nconnect: tcp://{}\nframe: {{line: \"\\n\"}}\nread:\n  - {{match: '^temp=(?P<t>[0-9.]+)$', metric: temp.c, group: t}}\n",
        plc.address
    );
    let definition = id_of(&k.insert(admin, "DeviceDefinition", obj(json!({"definition_name": "press", "body": definition}))).await.unwrap());
    k.run_action(admin, "DeviceDefinition", definition, "publish", json!({})).await.expect("published");
    let press = equipment(k, admin, ws.site, "PRESS-1", "press.set").await;
    k.update(admin, "Device", press, obj(json!({"gateway": ws.gateway, "edge_definition": definition})), None).await.expect("assigned");
    devices::set_lifecycle(k, admin, press, Lifecycle::Provisioned, "behind its gateway").await.unwrap();
    devices::set_lifecycle(k, admin, press, Lifecycle::Commissioned, "checked").await.unwrap();
    devices::set_lifecycle(k, admin, press, Lifecycle::Active, "in service").await.unwrap();

    // Hot for a moment does nothing. Hot for two seconds raises, and cooling clears.
    let body = json!({
        "nodes": [
            {"id": "t", "kind": "metric", "metric": "temp.c"},
            {"id": "hot", "kind": "compare", "op": ">", "value": 80},
            {"id": "long", "kind": "hold", "for": "2s"},
            {"id": "a", "kind": "alarm", "name": "Running hot", "severity": "Warning"},
        ],
        "edges": [
            {"from": "t", "to": "hot"},
            {"from": "hot", "port": "yes", "to": "long"},
            {"from": "hot", "port": "no", "to": "long", "input": "reset"},
            {"from": "hot", "port": "no", "to": "a", "input": "clear"},
            {"from": "long", "to": "a"},
        ],
    });
    let flow = id_of(&k.insert(admin, "EdgeFlow", obj(json!({"flow_name": "Hot press", "body": body.to_string()}))).await.unwrap());
    let edge = k.hooks.site("edge").unwrap().clone();
    let simulate = SiteRequest {
        tenant: ws.tenant,
        user: Some(admin.clone()),
        method: "POST".into(),
        path: format!("/flows/{flow}/simulate"),
        query: Map::new(),
        headers: Map::new(),
        body: json!({"script": [{"at": 0, "device": "PRESS-1", "metric": "temp.c", "value": 90}], "until": 3}).to_string().into_bytes(),
        client_ip: "::1".into(),
    };
    edge.handle(k, simulate).await.expect("simulated");
    k.run_action(admin, "EdgeFlow", flow, "submit_for_review", json!({})).await.expect("sent");
    let reviewer = k.create_user(admin, "rev@managed.test", "Reviewer", Some("Correct-Horse-9"), &["Edge Reviewer".to_string()], false).await.unwrap();
    let reviewer = k.load_ctx(ws.tenant, reviewer, None, "it".into()).await.unwrap();
    k.run_action(&reviewer, "EdgeFlow", flow, "approve", json!({})).await.expect("approved");
    k.run_action(&reviewer, "EdgeFlow", flow, "activate", json!({})).await.expect("running");

    let mut site = running(w, &ws, "managed: true\n").await;
    until("the link to the press", 15, async || site.console("GET", "/api/status", None, None).await.1["links"][0]["up"] == json!(true)).await;
    let status = site.console("GET", "/api/status", None, None).await.1;
    assert_eq!(status["flows"][0]["name"], json!("Hot press"), "{status}");

    let say = |line: &str| {
        let _ = plc.say.send(format!("{line}\n"));
    };
    let keep_saying = |line: &'static str| {
        let say = plc.say.clone();
        tokio::spawn(async move {
            loop {
                let _ = say.send(format!("{line}\n"));
                tokio::time::sleep(StdDuration::from_millis(200)).await;
            }
        })
    };
    let hot = keep_saying("temp=90");
    until("the alarm to reach the platform", 20, async || alarms(&ws).await == [("Running hot on PRESS-1".to_string(), "Active".to_string())]).await;
    hot.abort();
    let latest = ferrum_ops::telemetry::latest(k, admin, press).await.unwrap();
    assert!(latest.iter().any(|m| m["metric"] == json!("temp.c")), "and what it read: {latest:?}");

    // The link goes, and then the power. Nothing on this machine says what to read or what to
    // decide: only what it last heard.
    w.p.down.store(true, Ordering::SeqCst);
    until("the site to notice", 10, async || site.console("GET", "/api/status", None, None).await.1["link_up"] == json!(false)).await;
    site.kill();
    site.start(None);
    until("it to find the press again", 15, async || site.console("GET", "/api/status", None, None).await.1["links"][0]["up"] == json!(true)).await;
    let status = site.console("GET", "/api/status", None, None).await.1;
    assert_eq!(status["flows"][0]["name"], json!("Hot press"), "it remembers its flow with no link: {status}");

    // Cooled while nobody could hear. It decides anyway, and keeps what it decided for later.
    let before = status["unsent_facts"].as_i64().unwrap();
    let cool = keep_saying("temp=40");
    until("the flow to decide offline", 15, async || site.console("GET", "/api/status", None, None).await.1["unsent_facts"].as_i64().unwrap() > before).await;
    cool.abort();
    assert_eq!(alarms(&ws).await[0].1, "Active", "the platform cannot know yet, and says only what it knows");

    let metrics = tokio::task::spawn_blocking({
        let console = site.console.clone();
        move || {
            use std::io::{Read, Write};
            let mut s = std::net::TcpStream::connect(console).unwrap();
            s.write_all(b"GET /metrics HTTP/1.0\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        }
    })
    .await
    .unwrap();
    assert!(metrics.contains("ferrum_site_link_up{definition=\"press\"") && metrics.contains("ferrum_site_flows 1"), "{metrics}");

    w.p.down.store(false, Ordering::SeqCst);
    until("the alarm to clear on the platform", 20, async || alarms(&ws).await[0].1 == "Cleared").await;

    // A flow that is stopped stops.
    k.run_action(admin, "EdgeFlow", flow, "stop", json!({})).await.expect("stopped");
    until("the gateway to drop it", 15, async || site.console("GET", "/api/status", None, None).await.1["flows"] == json!([])).await;
    say("temp=95");
    tokio::time::sleep(StdDuration::from_secs(3)).await;
    assert_eq!(alarms(&ws).await.len(), 1, "nothing raises any more");
}
