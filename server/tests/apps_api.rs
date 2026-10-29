//! The App Manager against a real PostgreSQL (`FERRUM_APPS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_apps_api_test`; the database is reset) and a
//! fake app store on a local port serving search, details, releases, bundles and a signed
//! revocation list: listing, search, plans with dependencies and permissions, consent,
//! installation from the store, the role rule, uploads under both policies, private apps,
//! enable/disable, upgrades, removal with and without data, and revocation.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrum_apps::{Bundle, PLATFORM_VERSION, Role, Signer, TrustedKey, Uninstall};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Access, Action, Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_plugins::PluginHost;
use ferrum_server::appmgr::{AppManager, Refusal, StepAction, store};
use ferrum_server::state::{AppsSettings, Uploads};
use loco_rs::storage::{Storage, drivers::mem};
use ring::signature::Ed25519KeyPair;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

// ───────────────────────── fixtures ─────────────────────────

fn files(list: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    list.iter().map(|(p, d)| (p.to_string(), d.as_bytes().to_vec())).collect()
}

/// A plugin module that answers `{}` to everything.
fn wasm() -> Vec<u8> {
    wat::parse_str(ferrum_plugins::constant_plugin_wat("{}")).expect("wat")
}

/// A metadata app: one entity, a role on it, a field on core's Customer.
fn kit(id: &str) -> BTreeMap<String, Vec<u8>> {
    files(&[
        (
            "app.yaml",
            &format!(
                "id: {id}\nname: Kit\nversion: 1.0.0\npublisher: acme\nsummary: A starter kit\ndescription: README.md\ncategories: [industry]\n\
                 runtime: meta\nplatform: \">=0.3, <0.4\"\npackages: [kit]\nprefix: Kt\n"
            ),
        ),
        ("README.md", "# Kit\nThings to track.\n"),
        (
            "packages/kit/package.yaml",
            "package: kit\nversion: 1.0.0\ndepends: [core]\nfield_prefix: kt_\nroles:\n  - name: Kit User\n    permissions:\n      - {entity: KtThing, read: true, write: true, create: true}\n\
             overlays:\n  - entity: Customer\n    ops:\n      - {op: add_field, field: {name: kt_level, type: data}}\n",
        ),
        ("packages/kit/entities/thing.yaml", "name: KtThing\nlabel: Thing\nkind: master\nfields:\n  - {name: title, type: data, required: true}\n"),
    ])
}

/// A sandboxed app that needs `kit`: a package with a table and a plugin calling `hosts`.
fn gw(version: &str, hosts: &[&str]) -> BTreeMap<String, Vec<u8>> {
    let plugin = json!({
        "name": "gw", "version": version, "config": {"greeting": "hello"},
        "hooks": [{"entity": "GwDevice", "stage": "validate"}],
        "permissions": [{"entity": "GwDevice", "read": true, "write": true}, {"entity": "Customer", "read": true}],
        "http_hosts": hosts,
    });
    let mut f = files(&[
        (
            "app.yaml",
            &format!(
                "id: gw\nname: Gateway\nversion: {version}\npublisher: acme\nsummary: Devices over WebSocket\ndescription: README.md\ncategories: [iot]\n\
                 runtime: wasm\nplatform: \"^0.3\"\nrequires: {{kit: \"^1\"}}\npackages: [gw]\nplugin: plugin.json\nprefix: Gw\nuninstall: ask\n"
            ),
        ),
        ("README.md", "# Gateway\nConnects devices.\n"),
        ("plugin.json", &plugin.to_string()),
        ("packages/gw/package.yaml", &format!("package: gw\nversion: {version}\ndepends: [core]\n")),
        ("packages/gw/entities/device.yaml", "name: GwDevice\nlabel: Device\nkind: master\nfields:\n  - {name: serial, type: data}\n"),
    ]);
    f.insert("plugin.wasm".into(), wasm());
    f
}

/// A metadata app whose role reaches beyond its own entities (refused by the role rule).
fn bad() -> BTreeMap<String, Vec<u8>> {
    files(&[
        (
            "app.yaml",
            "id: bad\nname: Bad\nversion: 1.0.0\npublisher: acme\nsummary: Wants customers\ncategories: [crm]\nruntime: meta\nplatform: \"^0.3\"\npackages: [bad]\nprefix: Bd\n",
        ),
        (
            "packages/bad/package.yaml",
            "package: bad\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Bad Role\n    permissions:\n      - {entity: BdThing, read: true}\n      - {entity: Customer, read: true}\n",
        ),
        ("packages/bad/entities/thing.yaml", "name: BdThing\nlabel: Thing\nkind: master\nfields:\n  - {name: title, type: data}\n"),
    ])
}

/// A sandboxed app with no packages (uploads; private apps).
fn plugin_only(id: &str) -> BTreeMap<String, Vec<u8>> {
    let mut f = files(&[
        (
            "app.yaml",
            &format!(
                "id: {id}\nname: {id}\nversion: 1.0.0\npublisher: acme\nsummary: Answers pings\ncategories: [developer]\nruntime: wasm\nplatform: \"^0.3\"\nplugin: plugin.json\n"
            ),
        ),
        ("plugin.json", &json!({"name": id, "routes": [{"path": "/ping"}]}).to_string()),
    ]);
    f.insert("plugin.wasm".into(), wasm());
    f
}

/// An app with screens and an extension point (docs/plugins.md).
const BOARD_YAML: &str = r#"id: board
name: Board
version: 1.0.0
publisher: acme
summary: A board of cards
categories: [productivity]
runtime: meta
platform: "^0.3"
packages: [board]
prefix: Bd
extension_points:
  board.card:
    description: A card on the board
    schema: {type: object, required: [label], additionalProperties: false, properties: {label: {type: string, maxLength: 20}}}
contributes:
  ui.page:
    - {id: board, title: Board, entry: ui/board.html, icon: layout-grid}
  ui.widget:
    - {id: count, title: Cards, entry: ui/count.html, size: [3, 2]}
  ui.panel:
    - {id: cards, title: Cards, entity: BdCard, entry: ui/panel.html}
  ui.settings:
    - {id: settings, entry: ui/settings.html}
"#;

fn board() -> BTreeMap<String, Vec<u8>> {
    let page = "<!doctype html><title>Board</title><script src=board.js></script>";
    files(&[
        ("app.yaml", BOARD_YAML),
        ("packages/board/package.yaml", "package: board\nversion: 1.0.0\ndepends: [core]\nfield_prefix: bd_\n"),
        ("packages/board/entities/card.yaml", "name: BdCard\nlabel: Card\nkind: master\nfields:\n  - {name: title, type: data, required: true}\n"),
        ("ui/board.html", page),
        ("ui/board.js", "parent.postMessage('ready', '*')"),
        ("ui/count.html", page),
        ("ui/panel.html", page),
        ("ui/settings.html", page),
    ])
}

/// An add-on of `board` contributing one card.
fn board_card(label: &str, version: &str) -> BTreeMap<String, Vec<u8>> {
    let mut f = plugin_only("board-late");
    let yaml = String::from_utf8(f["app.yaml"].clone()).unwrap().replace("version: 1.0.0", &format!("version: {version}"))
        + &format!("requires: {{board: \"^1.0\"}}\nparent: board\ncontributes:\n  board.card:\n    - {{id: late, label: \"{label}\"}}\n");
    f.insert("app.yaml".into(), yaml.into_bytes());
    f
}

const NOTES_YAML: &str = "id: notes\nname: Notes\nversion: 0.2.0\npublisher: cbferrum\nsummary: Field notes\ndescription: README.md\ncategories: [productivity]\n\
runtime: native\nplatform: \">=0.3, <0.4\"\npackages: [notes]\nuninstall: ask\n";

/// The package of the native app `notes` (compiled into this test's "build").
fn notes_package() -> ferrum_meta::Package {
    let f: BTreeMap<String, String> = [
        (
            "package.yaml",
            "package: notes\nversion: 0.2.0\ndepends: [core]\nroles:\n  - name: Notes User\n    permissions:\n      - {entity: FieldNote, read: true, write: true, create: true}\n",
        ),
        ("entities/note.yaml", "name: FieldNote\nlabel: Note\nkind: master\nfields:\n  - {name: title, type: data}\n"),
    ]
    .into_iter()
    .map(|(p, d)| (p.to_string(), d.to_string()))
    .collect();
    ferrum_meta::catalog::load_package_files(&f).expect("notes package")
}

fn bundle(files: BTreeMap<String, Vec<u8>>, signers: &[(&Signer, Role)]) -> Bundle {
    let mut b = Bundle::from_files(files).expect("valid bundle");
    for (s, role) in signers {
        b.sign(s, *role).expect("sign");
    }
    b
}

// ───────────────────────── the fake store ─────────────────────────

#[derive(Clone, Default)]
struct FakeStore {
    apps: Arc<Mutex<Vec<Value>>>,
    releases: Arc<Mutex<BTreeMap<String, Vec<Value>>>>,
    bundles: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    revocations: Arc<Mutex<Value>>,
}

impl FakeStore {
    /// Lists a store-signed bundle. Listings stay sparse (no `requires`), as a young store's
    /// might: the App Manager must learn dependencies from the bundle itself.
    fn publish(&self, b: &Bundle) {
        let m = &b.manifest;
        self.bundles.lock().unwrap().insert(format!("{}/{}", m.id, m.version), b.write().unwrap());
        self.releases.lock().unwrap().entry(m.id.clone()).or_default().push(json!({
            "version": m.version.to_string(), "platform": m.platform.to_string(), "digest": b.digest(), "status": "published",
        }));
        let mut apps = self.apps.lock().unwrap();
        apps.retain(|a| a["id"] != m.id.as_str());
        apps.push(json!({
            "id": m.id, "name": m.name, "latest_version": m.version.to_string(), "runtime": m.runtime.to_string(), "summary": m.summary,
            "categories": m.categories, "publisher": {"id": m.publisher, "verified": true}, "icon": format!("/icons/{}.svg", m.id),
        }));
    }
}

async fn search(State(s): State<FakeStore>, Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    assert_eq!(q.get("platform").map(String::as_str), Some(PLATFORM_VERSION));
    let needle = q.get("q").cloned().unwrap_or_default().to_lowercase();
    let apps: Vec<Value> =
        s.apps.lock().unwrap().iter().filter(|a| needle.is_empty() || a["name"].as_str().unwrap().to_lowercase().contains(&needle)).cloned().collect();
    Json(json!({"apps": apps}))
}

async fn details(State(s): State<FakeStore>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    s.apps.lock().unwrap().iter().find(|a| a["id"] == id.as_str()).map(|a| Json(json!({"app": a}))).ok_or(StatusCode::NOT_FOUND)
}

async fn releases(State(s): State<FakeStore>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    s.releases.lock().unwrap().get(&id).map(|r| Json(json!({"releases": r}))).ok_or(StatusCode::NOT_FOUND)
}

async fn download(State(s): State<FakeStore>, Path((id, version)): Path<(String, String)>) -> Result<Vec<u8>, StatusCode> {
    s.bundles.lock().unwrap().get(&format!("{id}/{version}")).cloned().ok_or(StatusCode::NOT_FOUND)
}

async fn revocations(State(s): State<FakeStore>) -> Json<Value> {
    Json(s.revocations.lock().unwrap().clone())
}

async fn serve(store: FakeStore) -> String {
    let app = axum::Router::new()
        .route("/v1/apps", get(search))
        .route("/v1/apps/{id}", get(details))
        .route("/v1/apps/{id}/releases", get(releases))
        .route("/v1/apps/{id}/releases/{version}/bundle", get(download))
        .route("/v1/revocations", get(revocations))
        .with_state(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// `{"payload", "signatures"}` signed by `key`.
fn signed_list(key: &Ed25519KeyPair, key_id: &str, entries: Value) -> Value {
    sign_payload(key, key_id, json!({"revocations": entries}))
}

/// A revocation list as the store publishes it, with its generation time.
fn dated_list(key: &Ed25519KeyPair, key_id: &str, entries: Value, generated_at: &str) -> Value {
    sign_payload(key, key_id, json!({"generated_at": generated_at, "revocations": entries}))
}

fn sign_payload(key: &Ed25519KeyPair, key_id: &str, payload: Value) -> Value {
    let payload = payload.to_string();
    json!({"payload": payload, "signatures": [{"key_id": key_id, "sig": STANDARD.encode(key.sign(payload.as_bytes()).as_ref())}]})
}

// ───────────────────────── helpers ─────────────────────────

struct Keys {
    store: Signer,
    store_pair: Ed25519KeyPair,
    publisher: Signer,
    publisher_pair: Ed25519KeyPair,
}

impl Keys {
    fn new() -> Keys {
        let (s, p) = (Signer::generate_pkcs8().unwrap(), Signer::generate_pkcs8().unwrap());
        Keys {
            store: Signer::from_pkcs8(&s).unwrap(),
            store_pair: Ed25519KeyPair::from_pkcs8_maybe_unchecked(&s).unwrap(),
            publisher: Signer::from_pkcs8(&p).unwrap(),
            publisher_pair: Ed25519KeyPair::from_pkcs8_maybe_unchecked(&p).unwrap(),
        }
    }

    fn trusted(&self) -> String {
        format!("store:{}, publisher:{}", STANDARD.encode(self.store.public_key()), STANDARD.encode(self.publisher.public_key()))
    }
}

async fn count(k: &Kernel, sql: &'static str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn plugin(host: &PluginHost, k: &Kernel, ctx: &Ctx, name: &str) -> Option<Value> {
    host.list(k, ctx).await.unwrap().into_iter().find(|p| p["name"] == name)
}

fn installed<'a>(overview: &'a Value, id: &str) -> Option<&'a Value> {
    overview["installed"].as_array().unwrap().iter().find(|a| a["id"] == id)
}

// ───────────────────────── tests ─────────────────────────

#[test]
fn store_responses_are_read_tolerantly() {
    let base = "https://store.example";
    let apps = store::apps(
        &json!({"items": [
            {"id": "ev-charging", "name": "EV Charging", "latest": {"version": "1.4.0", "runtime": "wasm"},
             "publisher": {"id": "voltworks", "verified": true}, "icon": "/icons/ev.svg", "categories": ["energy"]},
            {"app_id": "kit", "summary": "Kit\nand more", "icon_url": "javascript:alert(1)"},
            {"name": "no id"},
            {"id": "Not An Id"},
        ]}),
        base,
    );
    assert_eq!(apps.len(), 2);
    assert_eq!(
        (apps[0].version.as_str(), apps[0].runtime.as_str(), apps[0].publisher.as_deref(), apps[0].verified),
        ("1.4.0", "wasm", Some("voltworks"), true)
    );
    assert_eq!(apps[0].icon_url.as_deref(), Some("https://store.example/icons/ev.svg"));
    assert_eq!((apps[1].name.as_str(), apps[1].summary.as_str(), apps[1].icon_url.as_deref()), ("kit", "Kit", None), "only http(s) icons");

    let rels = store::releases(&json!([
        {"version": "1.0.0", "manifest": {"requires": {"kit": "^1"}, "platform": ">=0.3"}, "sha256": "AB".repeat(32)},
        {"version": "1.1.0", "yanked": true},
        {"version": "1.2.0", "status": "pending"},
        {"version": "not semver"},
        {"version": "2.0.0", "requires": {"kit": "not a requirement"}},
        {"version": "2.1.0", "requires": {"Bad Id": "^1"}},
    ]));
    assert_eq!(rels.len(), 1, "withdrawn and unreadable releases are skipped");
    assert_eq!(rels[0].requires["kit"], semver::VersionReq::parse("^1").unwrap());
    assert_eq!(rels[0].digest.as_deref(), Some("ab".repeat(32).as_str()));
    assert_eq!(rels[0].bundle_url(base, "kit"), "https://store.example/v1/apps/kit/releases/1.0.0/bundle");
    let sparse = store::releases(&json!({"releases": [{"version": "3.0.0", "bundle_url": "files/kit-3.fapp"}]}));
    assert_eq!(sparse[0].platform, semver::VersionReq::STAR);
    assert_eq!(sparse[0].bundle_url(base, "kit"), "https://store.example/files/kit-3.fapp");
}

#[test]
fn revocation_lists_need_a_trusted_store_signature() {
    let keys = Keys::new();
    let store_key = TrustedKey::parse(&format!("store:{}", STANDARD.encode(keys.store.public_key()))).unwrap();
    let entries = json!([{"app": "gw", "version": "2.1.0", "reason": "malware"}, {"app": "kit", "version": "*"}, {"app": "Not An Id"}]);
    let list = signed_list(&keys.store_pair, &keys.store.key_id(), entries.clone());
    let got = store::revocations(&list, std::slice::from_ref(&store_key)).unwrap();
    assert_eq!(
        got,
        vec![
            store::Revocation { app: "gw".into(), version: Some("2.1.0".into()), reason: "malware".into() },
            store::Revocation { app: "kit".into(), version: None, reason: "revoked by the store".into() },
        ]
    );
    let mut forged = list.clone();
    forged["payload"] = json!(list["payload"].as_str().unwrap().replace("malware", "fine"));
    assert!(store::revocations(&forged, std::slice::from_ref(&store_key)).unwrap_err().contains("does not match"));
    let by_publisher = signed_list(&keys.publisher_pair, &keys.publisher.key_id(), entries);
    let publisher_key = TrustedKey::parse(&format!("publisher:{}", STANDARD.encode(keys.publisher.public_key()))).unwrap();
    assert!(store::revocations(&by_publisher, &[store_key.clone(), publisher_key]).is_err(), "only store keys sign revocations");
    assert!(store::revocations(&json!({"payload": "{}"}), &[store_key]).is_err(), "unsigned");
}

#[test]
fn the_app_manager_installs_upgrades_and_removes_apps() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    // ── a cluster: core plus the native app `notes`, two workspaces ──
    let url = std::env::var("FERRUM_APPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_apps_api_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, notes_package()]).unwrap(), HookRegistry::default(), KernelSettings::default(), None)
        .await
        .expect("boot");
    // Rate limits hold across replicas: two limiters on one database share their count.
    let (a, b) = (
        ferrum_server::state::RateLimiter::shared("it", std::time::Duration::from_secs(60), 3, k.db.clone()),
        ferrum_server::state::RateLimiter::shared("it", std::time::Duration::from_secs(60), 3, k.db.clone()),
    );
    for l in [&a, &b, &a] {
        l.check("10.0.0.1").await.expect("within the limit");
    }
    assert!(matches!(b.check("10.0.0.1").await, Err(Error::TooManyRequests(_))), "the fourth request, on another replica");
    b.check("10.0.0.2").await.expect("another client has its own count");
    let host = PluginHost::new(false).unwrap();
    let mut tenants = Vec::new();
    for slug in ["acme", "other"] {
        let id = k
            .create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec!["core".into()],
                settings: None,
            })
            .await
            .unwrap();
        tenants.push(id);
    }
    let (tenant, other) = (tenants[0], tenants[1]);
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let ctx = || async { k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap() };
    let admin = ctx().await;

    // ── a store with three apps, and two policies over the same cluster ──
    let keys = Keys::new();
    let fake = FakeStore::default();
    *fake.revocations.lock().unwrap() = signed_list(&keys.store_pair, &keys.store.key_id(), json!([]));
    let store_sig = [(&keys.store, Role::Store)];
    fake.publish(&bundle(kit("kit"), &store_sig));
    fake.publish(&bundle(gw("2.1.0", &["api.example.com"]), &[(&keys.publisher, Role::Publisher), (&keys.store, Role::Store)]));
    fake.publish(&bundle(bad(), &store_sig));
    let store_url = serve(fake.clone()).await;
    let storage = Arc::new(Storage::single(mem::new()));
    let natives = vec![ferrum_apps::AppManifest::parse(NOTES_YAML).unwrap()];
    let manager = |uploads, allow_unsigned_uploads| {
        let settings = AppsSettings { registries: format!("{store_url}/"), trusted_keys: keys.trusted(), uploads, allow_unsigned_uploads };
        AppManager::new(k.clone(), host.clone(), storage.clone(), &settings, natives.clone()).expect("app manager")
    };
    let dev = manager(Uploads::Cluster, true);
    let prod = manager(Uploads::Private, false);

    // ── listing ──
    let o = dev.overview(&admin).await.unwrap();
    assert_eq!((o["platform"].as_str(), o["uploads"].as_str()), (Some(PLATFORM_VERSION), Some("cluster")));
    assert_eq!(o["stores"], json!([{"url": store_url, "ok": true, "error": null}]));
    assert_eq!(o["installed"], json!([]));
    let available: Vec<&str> = o["available"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert_eq!(available, ["notes"], "native apps of this build; nothing else is on the cluster yet");
    let found: Vec<String> = dev.search(&admin, "", "").await.unwrap().into_iter().map(|a| format!("{} {}", a.id, a.source)).collect();
    assert_eq!(found, ["notes native".to_string(), format!("kit store:{store_url}"), format!("gw store:{store_url}"), format!("bad store:{store_url}")]);
    let gateway = dev.search(&admin, "gate", "").await.unwrap();
    assert_eq!(gateway.len(), 1);
    assert_eq!((gateway[0].id.as_str(), gateway[0].verified, gateway[0].icon_url.clone()), ("gw", true, Some(format!("{store_url}/icons/gw.svg"))));
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(dev.overview(&clerk).await, Err(Error::Forbidden(_))), "administrators only");

    // ── the same over MCP: administrators get the app tools, others don't ──
    use ferrum_mcp::ToolProvider;
    let tools = ferrum_server::appmgr_mcp::AppTools(Arc::new(manager(Uploads::Cluster, true)));
    let names = |ctx: &Ctx| tools.tools(ctx).iter().map(|t| t["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(&admin), ["list_apps", "plan_app", "install_app", "set_app_enabled", "uninstall_app"]);
    assert!(names(&clerk).is_empty());
    let listed = ferrum_mcp::handle_with(&k, &admin, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}), &[&tools]).await.unwrap();
    assert!(listed["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == json!("plan_app")), "listed beside the platform's tools");
    let called = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "list_apps", "arguments": {}}});
    let reply = ferrum_mcp::handle_with(&k, &admin, called, &[&tools]).await.unwrap();
    assert_eq!(reply["result"]["isError"], json!(false), "{reply}");
    let stale = tools.call(&k, &admin, "install_app", &json!({"app": "gw", "consent": "not-the-digest"})).await.unwrap().unwrap();
    assert_eq!(stale["installed"], json!(false), "a wrong consent installs nothing and hands back the plan to review");
    assert!(stale["plan"]["digest"].is_string());
    assert!(matches!(tools.call(&k, &clerk, "list_apps", &json!({})).await, Some(Err(Error::Forbidden(_)))));
    assert!(tools.call(&k, &admin, "no_such_tool", &json!({})).await.is_none());

    // ── a plan: the dependency comes first; permissions of both; nothing changes yet ──
    let plan = dev.plan(&admin, "gw", None).await.unwrap();
    let steps: Vec<(String, String, StepAction, String)> = plan.steps.iter().map(|s| (s.id.clone(), s.version.clone(), s.action, s.source.clone())).collect();
    let src = format!("store:{store_url}");
    assert_eq!(
        steps,
        [("kit".to_string(), "1.0.0".to_string(), StepAction::Install, src.clone()), ("gw".to_string(), "2.1.0".to_string(), StepAction::Install, src.clone())]
    );
    assert!(plan.permissions.hosts.contains("api.example.com"));
    assert!(plan.permissions.new_entities.contains("GwDevice") && plan.permissions.new_entities.contains("KtThing"));
    assert!(plan.permissions.roles.contains_key("Kit User"));
    assert!(plan.permissions.overlays["Customer"].contains("adds field kt_level"));
    assert_eq!(plan.digest.len(), 64);
    assert!(plan.increases.is_empty() && plan.warnings.is_empty(), "{:?} {:?}", plan.increases, plan.warnings);
    assert_eq!((plan.app.summary.runtime.as_str(), plan.app.summary.verified, plan.uninstall), ("wasm", true, Uninstall::Ask));
    assert_eq!(plan.app.summary.icon_url, Some(format!("{store_url}/icons/gw.svg")));
    assert!(plan.app.description.as_deref().is_some_and(|d| d.contains("Connects devices")));
    assert_eq!(dev.plan(&admin, "gw", None).await.unwrap().digest, plan.digest, "plans are deterministic");
    assert!(k.release_history("gw").await.unwrap().is_empty(), "planning makes nothing available");

    // ── consent must be the plan's digest ──
    match dev.install(&admin, "gw", Some("2.1.0"), "not-the-digest", false).await {
        Err(Refusal::Stale(fresh)) => assert_eq!(fresh.digest, plan.digest),
        other => panic!("expected a stale consent: {other:?}"),
    }
    assert!(matches!(dev.install(&clerk, "gw", None, &plan.digest, false).await, Err(Refusal::Failed(Error::Forbidden(_)))));

    // ── installing from the store: verified, made available, packages and plugin in place ──
    dev.install(&admin, "gw", Some("2.1.0"), &plan.digest, false).await.unwrap();
    for app in ["kit", "gw"] {
        let history = k.release_history(app).await.unwrap();
        assert_eq!(history.iter().map(|r| (r.status.as_str(), r.source.as_str())).collect::<Vec<_>>(), [("active", src.as_str())], "{app}");
    }
    let c = ctx().await;
    assert!(c.schema.entity("GwDevice").is_some() && c.schema.entity("KtThing").is_some());
    assert!(c.schema.entity("Customer").unwrap().field("kt_level").is_some());
    let p = plugin(&host, &k, &c, "gw").await.expect("plugin installed");
    assert_eq!((p["enabled"].as_bool(), p["version"].as_str(), p["config_keys"].clone()), (Some(true), Some("2.1.0"), json!(["greeting"])));
    let o = dev.overview(&c).await.unwrap();
    let g = installed(&o, "gw").unwrap();
    assert_eq!((g["enabled"].as_bool(), g["verified"].as_bool(), g["update"].clone()), (Some(true), Some(true), Value::Null));
    assert!(installed(&o, "kit").is_some());
    let same = dev.plan(&admin, "gw", None).await.unwrap();
    assert_eq!(same.steps.iter().map(|s| (s.id.as_str(), s.action)).collect::<Vec<_>>(), [("gw", StepAction::Already)]);
    dev.install(&admin, "gw", None, &same.digest, false).await.expect("nothing to do");
    assert!(matches!(dev.install(&admin, "notes", None, "x", true).await, Err(Refusal::Failed(Error::NotFound(_)))), "only installed apps are upgraded");

    // ── the role rule: a role may grant only the app's own entities ──
    let refused = dev.plan(&admin, "bad", None).await.unwrap_err();
    assert!(matches!(&refused, Error::Forbidden(m) if m.contains("Customer")), "{refused}");
    assert!(k.release_history("bad").await.unwrap().is_empty());

    // ── a native app of this build ──
    let notes = dev.plan(&admin, "notes", None).await.unwrap();
    assert_eq!(notes.steps.iter().map(|s| (s.action, s.source.as_str())).collect::<Vec<_>>(), [(StepAction::Install, "native")]);
    assert!(notes.permissions.native && notes.permissions.new_entities.contains("FieldNote"));
    dev.install(&admin, "notes", None, &notes.digest, false).await.unwrap();
    assert!(ctx().await.schema.entity("FieldNote").is_some());

    // ── dependencies hold: kit cannot go while gw needs it ──
    let err = dev.uninstall(&admin, "kit", false).await.unwrap_err();
    assert!(err.to_string().contains("gw"), "{err}");
    let err = dev.set_enabled(&admin, "kit", false).await.unwrap_err();
    assert!(err.to_string().contains("gw"), "{err}");

    // ── disable and enable: plugin off, packages out of the workspace, data kept ──
    let c = ctx().await;
    k.insert(&c, "GwDevice", json!({"serial": "A-1"}).as_object().cloned().unwrap()).await.unwrap();
    dev.set_enabled(&admin, "gw", false).await.unwrap();
    let c = ctx().await;
    assert!(c.schema.entity("GwDevice").is_none());
    assert_eq!(plugin(&host, &k, &c, "gw").await.unwrap()["enabled"], json!(false));
    assert_eq!(installed(&dev.overview(&c).await.unwrap(), "gw").unwrap()["enabled"], json!(false));
    assert_eq!(count(&k, "select count(*) from e_gw_device").await, 1);
    dev.set_enabled(&admin, "gw", true).await.unwrap();
    let c = ctx().await;
    assert!(c.schema.entity("GwDevice").is_some());
    assert_eq!(plugin(&host, &k, &c, "gw").await.unwrap()["enabled"], json!(true));

    // ── uploads: refused unsigned in production, allowed in development ──
    let tick = bundle(plugin_only("tick"), &[]).write().unwrap();
    let err = prod.upload(&admin, tick.clone().into()).await.unwrap_err();
    assert!(matches!(&err, Error::Forbidden(m) if m.contains("not signed")), "{err}");
    let (app, up) = dev.upload(&admin, tick.into()).await.unwrap();
    assert_eq!((app.id.as_str(), app.private, app.source.as_str()), ("tick", false, "upload"));
    assert_eq!(up.steps.iter().map(|s| (s.action, s.source.as_str())).collect::<Vec<_>>(), [(StepAction::Install, "cluster")]);
    assert!(up.warnings.iter().any(|w| w.contains("without a signature")), "{:?}", up.warnings);
    assert!(k.releases(Some(other)).await.unwrap().iter().any(|r| r.app_id == "tick"), "cluster uploads reach every workspace");

    // ── private uploads: sandboxed code without tables, for this workspace only ──
    let pv = bundle(plugin_only("pv"), &[(&keys.publisher, Role::Publisher)]).write().unwrap();
    let (app, up) = prod.upload(&admin, pv.into()).await.unwrap();
    assert!(app.private);
    assert_eq!(up.steps.iter().map(|s| s.source.as_str()).collect::<Vec<_>>(), ["private"]);
    assert!(k.releases(Some(tenant)).await.unwrap().iter().any(|r| r.app_id == "pv" && r.private_tenant == Some(tenant)));
    assert!(!k.releases(Some(other)).await.unwrap().iter().any(|r| r.app_id == "pv"), "invisible to other workspaces");
    prod.install(&admin, "pv", None, &up.digest, false).await.unwrap();
    assert!(plugin(&host, &k, &ctx().await, "pv").await.is_some());
    let notebook = bundle(kit("notebook"), &[(&keys.publisher, Role::Publisher)]).write().unwrap();
    let err = prod.upload(&admin, notebook.into()).await.unwrap_err();
    assert!(matches!(&err, Error::Forbidden(m) if m.contains("packages")), "{err}");
    let off = manager(Uploads::Off, true);
    assert!(matches!(off.upload(&admin, bundle(plugin_only("nope"), &[]).write().unwrap().into()).await, Err(Error::Forbidden(_))));

    // ── removing: data kept, unless deleting it was asked for and the app allows it ──
    assert!(!dev.uninstall(&admin, "gw", false).await.unwrap());
    let c = ctx().await;
    assert!(c.schema.entity("GwDevice").is_none() && plugin(&host, &k, &c, "gw").await.is_none());
    assert!(installed(&dev.overview(&c).await.unwrap(), "gw").is_none());
    assert_eq!(count(&k, "select count(*) from e_gw_device").await, 1, "kept");
    let again = dev.plan(&admin, "gw", None).await.unwrap();
    assert_eq!(again.steps.iter().map(|s| (s.id.as_str(), s.source.as_str())).collect::<Vec<_>>(), [("gw", "cluster")], "kit stays installed");
    dev.install(&admin, "gw", None, &again.digest, false).await.unwrap();
    assert!(dev.uninstall(&admin, "gw", true).await.unwrap(), "gw allows deleting its data");
    assert_eq!(count(&k, "select count(*) from e_gw_device").await, 0);
    k.insert(&ctx().await, "KtThing", json!({"title": "kept"}).as_object().cloned().unwrap()).await.unwrap();
    assert!(!dev.uninstall(&admin, "kit", true).await.unwrap(), "kit says uninstall: keep");
    assert_eq!(count(&k, "select count(*) from e_kt_thing").await, 1);

    // ── upgrades: a newer store release, with more access, needs a new consent ──
    let reinstall = dev.plan(&admin, "gw", None).await.unwrap();
    assert_eq!(reinstall.steps.iter().map(|s| (s.id.as_str(), s.source.as_str())).collect::<Vec<_>>(), [("kit", "cluster"), ("gw", "cluster")]);
    dev.install(&admin, "gw", None, &reinstall.digest, false).await.unwrap();
    fake.publish(&bundle(gw("2.2.0", &["api.example.com", "hooks.example.com"]), &store_sig));
    // Store answers are cached for a minute; a replica that has not asked yet sees the release.
    let dev = manager(Uploads::Cluster, true);
    let c = ctx().await;
    assert_eq!(installed(&dev.overview(&c).await.unwrap(), "gw").unwrap()["update"], json!({"version": "2.2.0"}));
    assert_eq!(
        installed(&prod.overview(&c).await.unwrap(), "gw").unwrap()["update"],
        Value::Null,
        "without cluster-wide uploads only the operator brings store upgrades to the cluster"
    );
    let up = dev.plan(&admin, "gw", Some("2.2.0")).await.unwrap();
    assert_eq!(up.steps.iter().map(|s| (s.id.as_str(), s.action)).collect::<Vec<_>>(), [("gw", StepAction::Upgrade)]);
    assert_eq!(up.increases, ["calls host: hooks.example.com"]);
    assert!(matches!(dev.install(&admin, "gw", Some("2.2.0"), &reinstall.digest, true).await, Err(Refusal::Stale(_))));
    dev.install(&admin, "gw", Some("2.2.0"), &up.digest, true).await.unwrap();
    let history: Vec<(String, String)> = k.release_history("gw").await.unwrap().into_iter().map(|r| (r.version, r.status)).collect();
    assert_eq!(history, [("2.2.0".to_string(), "active".to_string()), ("2.1.0".to_string(), "superseded".to_string())]);
    let c = ctx().await;
    assert_eq!(plugin(&host, &k, &c, "gw").await.unwrap()["version"], json!("2.2.0"));
    assert_eq!(installed(&dev.overview(&c).await.unwrap(), "gw").unwrap()["version"], json!("2.2.0"));

    // ── revocation: a forged list is ignored; the store's list disables the app everywhere ──
    *fake.revocations.lock().unwrap() = signed_list(&keys.publisher_pair, &keys.publisher.key_id(), json!([{"app": "gw", "version": "2.2.0"}]));
    assert_eq!(dev.sync_revocations().await.unwrap(), 0);
    assert_eq!(k.release_history("gw").await.unwrap()[0].status, "active");
    *fake.revocations.lock().unwrap() = signed_list(&keys.store_pair, &keys.store.key_id(), json!([{"app": "gw", "version": "2.2.0", "reason": "leaks data"}]));
    assert_eq!(dev.sync_revocations().await.unwrap(), 1);
    assert_eq!(dev.sync_revocations().await.unwrap(), 0, "applied once");
    let r = &k.release_history("gw").await.unwrap()[0];
    assert_eq!((r.version.as_str(), r.status.as_str()), ("2.2.0", "revoked"));
    assert!(r.status_reason.as_deref().is_some_and(|s| s.contains("leaks data")));
    let c = ctx().await;
    assert_eq!(plugin(&host, &k, &c, "gw").await.unwrap()["enabled"], json!(false));
    assert_eq!(installed(&dev.overview(&c).await.unwrap(), "gw").unwrap()["enabled"], json!(false));
    let err = dev.set_enabled(&admin, "gw", true).await.unwrap_err();
    assert!(matches!(&err, Error::Forbidden(m) if m.contains("revoked")), "{err}");
    let lines = dev.release_lines(Some("gw")).await.unwrap();
    assert!(lines[0].starts_with("gw 2.2.0 revoked wasm store:"), "{lines:?}");
    // A revoked app can still be removed; its rows stay (the cluster no longer defines its tables).
    k.insert(&ctx().await, "KtThing", json!({"title": "second"}).as_object().cloned().unwrap()).await.unwrap();
    assert!(!dev.uninstall(&admin, "gw", true).await.unwrap());
    let c = ctx().await;
    assert!(plugin(&host, &k, &c, "gw").await.is_none() && installed(&dev.overview(&c).await.unwrap(), "gw").is_none());
    assert!(c.schema.warnings.is_empty(), "nothing of the revoked app is left installed: {:?}", c.schema.warnings);

    // A list older than the newest one applied (a stale cache or a replay) is ignored.
    let list = |entries: Value, at: &str| dated_list(&keys.store_pair, &keys.store.key_id(), entries, at);
    *fake.revocations.lock().unwrap() = list(json!([{"app": "gw", "version": "2.2.0"}]), "2026-10-29T10:00:00Z");
    assert_eq!(dev.sync_revocations().await.unwrap(), 0, "already revoked");
    *fake.revocations.lock().unwrap() = list(json!([{"app": "gw", "version": "2.1.0"}]), "2026-10-29T09:00:00Z");
    assert_eq!(dev.sync_revocations().await.unwrap(), 0, "an older list is ignored");
    assert_ne!(k.release_history("gw").await.unwrap()[1].status, "revoked");
    *fake.revocations.lock().unwrap() = list(json!([{"app": "gw", "version": "2.1.0"}]), "2026-10-29T11:00:00Z");
    assert_eq!(dev.sync_revocations().await.unwrap(), 1, "a newer one applies");
    assert_eq!(k.release_history("gw").await.unwrap()[1].status, "revoked");

    // ── screens, extension points and add-ons (docs/plugins.md) ──
    let (_, up) = dev.upload(&admin, bundle(board(), &[]).write().unwrap().into()).await.unwrap();
    dev.install(&admin, "board", None, &up.digest, false).await.unwrap();
    let pages = dev.contributions(&admin, "ui.page").await.unwrap();
    assert_eq!(pages, [json!({"id": "board", "title": "Board", "entry": "ui/board.html", "icon": "layout-grid", "app": "board", "version": "1.0.0"})]);
    assert_eq!(dev.contributions(&admin, "ui.widget").await.unwrap()[0]["size"], json!([3, 2]));
    assert_eq!(dev.contributions(&admin, "ui.settings").await.unwrap().len(), 1);
    assert!(dev.contributions(&clerk, "ui.settings").await.unwrap().is_empty(), "settings pages are for administrators");
    assert_eq!(dev.contributions(&admin, "ui.panel").await.unwrap().len(), 1);
    assert!(dev.contributions(&clerk, "ui.panel").await.unwrap().is_empty(), "the clerk cannot read cards");
    assert!(dev.contributions(&admin, "board.card").await.unwrap().is_empty());
    assert!(matches!(dev.contributions(&admin, "kit.card").await, Err(Error::NotFound(_))), "no such point");

    let late = |label: &str, version: &str| bundle(board_card(label, version), &[]).write().unwrap();
    let err = dev.upload(&admin, late("a label far too long for a card", "0.9.0").into()).await.unwrap_err();
    assert!(err.to_string().contains("board-late contributes board.card late, which breaks board's contract: label is longer than 20"), "{err}");
    let (app, up) = dev.upload(&admin, late("Late", "1.0.0").into()).await.unwrap();
    assert_eq!(app.parent.as_deref(), Some("board"));
    dev.install(&admin, "board-late", None, &up.digest, false).await.unwrap();
    let cards = dev.contributions(&admin, "board.card").await.unwrap();
    assert_eq!(cards, [json!({"id": "late", "label": "Late", "app": "board-late", "version": "1.0.0"})]);
    assert_eq!(installed(&dev.overview(&admin).await.unwrap(), "board-late").unwrap()["parent"], json!("board"));
    let err = dev.uninstall(&admin, "board", false).await.unwrap_err();
    assert!(err.to_string().contains("board-late need board"), "an add-on keeps its parent: {err}");

    // The files of screens, and what a screen may reach for its user.
    let (page, digest) = dev.screen_file(tenant, "board", "1.0.0", "board.html").await.unwrap().expect("the page");
    assert!(std::str::from_utf8(&page).unwrap().contains("<title>Board</title>") && !digest.is_empty());
    assert!(dev.screen_file(tenant, "board", "1.0.0", "missing.html").await.unwrap().is_none());
    assert!(dev.screen_file(tenant, "board", "0.1.0", "board.html").await.unwrap().is_none(), "only the installed version");
    assert!(dev.screen_file(other, "board", "1.0.0", "board.html").await.unwrap().is_none(), "only where it is installed");
    assert!(dev.screen_scope(tenant, "board-late").await.unwrap().is_none(), "an app without screens has no bridge");
    let (entities, own) = dev.screen_scope(tenant, "board").await.unwrap().expect("board has screens");
    assert!(entities.is_empty() && own.contains("BdCard"), "{entities:?} {own:?}");
    let screen = admin.perms.limited_to(&entities, &own);
    assert_eq!(screen.access("BdCard", Action::Read), Access::All);
    assert_eq!(screen.access("Customer", Action::Read), Access::None, "an administrator's screen reaches only what the app declared");
    dev.set_enabled(&admin, "board-late", false).await.unwrap();
    dev.set_enabled(&admin, "board", false).await.unwrap();
    assert!(dev.contributions(&admin, "ui.page").await.unwrap().is_empty(), "disabled apps contribute nothing");
    assert!(dev.screen_scope(tenant, "board").await.unwrap().is_none());
}
