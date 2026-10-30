//! App Manager (docs/plan/apps.md): what a workspace can install — the native apps of this
//! build, releases available on the cluster, its own private uploads and the configured stores —
//! and the operations behind the Apps page: plans (dependencies, permissions and the consent
//! digest), install, upgrade, enable, disable, uninstall and uploads. Store responses are read by
//! [`store`]; one replica applies the stores' signed revocation lists every ten minutes.
//!
//! Each release records what consent needs in `NewRelease.manifest`: `{"app": <app.yaml as
//! JSON>, "permissions": <Permissions>, "plugin": <plugin.json or null>, "readme": <start of the
//! README or null>}`. Bundles stay in object storage under `apps/<id>/<version>-<digest
//! prefix>.fapp`, so any replica can install a plugin from them.

use crate::state::{AppsSettings, Uploads};
use bytes::Bytes;
use dashmap::DashMap;
use ferrum_apps::{AppError, AppManifest, Bundle, PLATFORM_VERSION, Permissions, Role, Runtime, TrustedKey, Uninstall};
use ferrum_kernel::apps::{NewRelease, Release};
use ferrum_kernel::{Access, Action, Ctx, Error, Kernel, Result, Tx};
use ferrum_plugins::{NewPlugin, PluginHost};
use futures_util::future::join_all;
use loco_rs::storage::Storage;
use semver::{Version, VersionReq};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// Largest bundle accepted from a store or an upload (50 MiB).
pub const MAX_BUNDLE: usize = 50 << 20;
const JSON_TIMEOUT: Duration = Duration::from_secs(5);
const BUNDLE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_JSON: usize = 4 << 20;
const CACHE_TTL: Duration = Duration::from_secs(60);
const REVOCATIONS_EVERY: Duration = Duration::from_secs(600);
/// Session advisory lock: one replica polls the revocation lists at a time.
const REVOCATIONS_LOCK: i64 = 0x6665_7272_756d_0002;
/// Most apps one plan may look at (the requested app, its dependencies and the installed apps).
const MAX_APPS: usize = 200;
const README_MAX: usize = 4_000;
const NOT_FOUND: &str = "HTTP 404";
/// How long a replica trusts its list of a workspace's enabled apps (changes made on this
/// replica clear it at once).
const ENABLED_TTL: Duration = Duration::from_secs(10);
/// Most bytes of screen files a replica keeps in memory.
const SCREEN_CACHE_MAX: usize = 256 << 20;

/// An app as the Apps page lists it.
#[derive(Debug, Clone, Serialize)]
pub struct AppSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    /// `native`, `wasm` or `meta`.
    pub runtime: String,
    pub summary: String,
    pub categories: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    pub verified: bool,
    pub icon_url: Option<String>,
    /// `native`, `upload` or `store:<url>`.
    pub source: String,
    pub private: bool,
    /// The app this one is an add-on of (`parent` in its app.yaml).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// An app installed in the workspace.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledApp {
    #[serde(flatten)]
    pub app: AppSummary,
    pub enabled: bool,
    pub installed_at: chrono::DateTime<chrono::Utc>,
    /// `{"version"}` when a newer compatible release (or this build's native app) is available.
    pub update: Option<Value>,
}

/// The app a plan is about, with what its consent screen shows.
#[derive(Debug, Clone, Serialize)]
pub struct PlanApp {
    #[serde(flatten)]
    pub summary: AppSummary,
    pub description: Option<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StepAction {
    Install,
    Upgrade,
    Already,
}

/// One app of a plan, in installation order (dependencies first).
#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub id: String,
    pub version: String,
    pub action: StepAction,
    /// `native`, `cluster` (already available here), `private`, or `store:<url>`.
    pub source: String,
}

/// What installing (or upgrading) an app does; `digest` is the consent the administrator gives.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub app: PlanApp,
    pub steps: Vec<Step>,
    /// Everything the apps installed or upgraded by this plan may do.
    pub permissions: Permissions,
    /// SHA-256 over the ordered steps' `(id, version, permissions digest)`.
    pub digest: String,
    /// What upgrades ask for beyond the installed versions.
    pub increases: Vec<String>,
    pub warnings: Vec<String>,
    pub uninstall: Uninstall,
}

impl Plan {
    /// Why an installation whose consent no longer matches was refused, for people.
    pub fn changed_message(&self) -> String {
        let steps: Vec<String> = self.steps.iter().filter(|s| s.action != StepAction::Already).map(|s| format!("{} {}", s.id, s.version)).collect();
        let what = if steps.is_empty() { "nothing to install".to_string() } else { steps.join(", ") };
        format!("The plan changed since it was reviewed (it now covers {what}; consent {}). Review it and allow it again.", self.digest)
    }
}

/// Why an installation did not run.
#[derive(Debug)]
pub enum Refusal {
    /// The consent is not the current plan's digest (the plan changed, or was never reviewed).
    Stale(Box<Plan>),
    Failed(Error),
}

impl From<Error> for Refusal {
    fn from(e: Error) -> Self {
        Refusal::Failed(e)
    }
}

/// Where a candidate release comes from.
#[derive(Clone)]
enum Origin {
    Native,
    /// Available on the cluster, or private to this workspace.
    Local(Box<Release>),
    Store {
        url: String,
        rel: store::Release,
    },
}

#[derive(Clone)]
struct Offer {
    res: ferrum_apps::Release,
    origin: Origin,
}

/// What a release recorded about itself when it was made available.
struct Recorded {
    app: AppManifest,
    permissions: Permissions,
    readme: Option<String>,
}

fn recorded(r: &Release) -> Option<Recorded> {
    let app = serde_json::from_value(r.manifest.get("app")?.clone()).ok()?;
    let permissions = serde_json::from_value(r.manifest.get("permissions")?.clone()).ok()?;
    let readme = r.manifest.get("readme").and_then(Value::as_str).map(String::from);
    Some(Recorded { app, permissions, readme })
}

struct Installed {
    version: String,
    enabled: bool,
    plugin_id: Option<Uuid>,
    installed_at: chrono::DateTime<chrono::Utc>,
    /// Recorded in `tenant_app` (native apps installed with the workspace may not be).
    row: bool,
    /// The app whose installation brought this one in.
    brought_by: Option<String>,
}

struct Workspace {
    apps: BTreeMap<String, Installed>,
    /// Installed package → version recorded when it was installed.
    packages: BTreeMap<String, String>,
    schema: Arc<ferrum_meta::Schema>,
}

struct StepData {
    id: String,
    version: Version,
    action: StepAction,
    origin: Origin,
    manifest: AppManifest,
    permissions: Permissions,
    readme: Option<String>,
    /// Countersigned by a trusted store key (native apps: reviewed platform code).
    verified: bool,
    /// Signed by any trusted key.
    trusted: bool,
    /// Store releases: the verified bundle and its object-storage key.
    staged: Option<(Bundle, String)>,
    source: String,
}

/// What to take back when a later step of an installation fails.
struct Undo {
    id: String,
    packages: Vec<String>,
    plugin: Option<Uuid>,
    recorded: bool,
    /// The installed version, and whether it had a `tenant_app` row.
    previous: Option<(String, bool)>,
}

enum Trust {
    Trusted,
    Untrusted(String),
}

/// One app change at a time per workspace, across replicas: a transaction-scoped advisory lock,
/// released when the transaction ends — also when the request is dropped half-way.
struct WorkspaceLock(Tx);

impl WorkspaceLock {
    async fn release(self) {
        let _ = self.0.rollback().await;
    }
}

pub struct AppManager {
    kernel: Arc<Kernel>,
    plugins: Arc<PluginHost>,
    storage: Arc<Storage>,
    /// Store base URLs, without a trailing slash.
    stores: Vec<String>,
    trusted: Vec<TrustedKey>,
    pub uploads: Uploads,
    allow_unsigned: bool,
    natives: Vec<AppManifest>,
    platform: Version,
    http: reqwest::Client,
    cache: DashMap<String, (Instant, std::result::Result<Value, String>)>,
    /// Per workspace: its enabled apps, for screens and extension points.
    enabled: DashMap<Uuid, (Instant, Arc<Vec<Enabled>>)>,
    /// Per bundle digest: the files of its screens (`ui/…`).
    screen_files: DashMap<String, Arc<BTreeMap<String, Bytes>>>,
    screen_bytes: AtomicUsize,
}

/// An installed, enabled app as screens, extension points and the shell see it.
struct Enabled {
    id: String,
    version: String,
    manifest: Arc<AppManifest>,
    /// Apps from bundles: their consent (what their screens may reach) and stored release.
    bundled: Option<(Permissions, Release)>,
}

/// Screens and extension points (docs/plugins.md): what a workspace's enabled apps contribute,
/// what their screens may reach, and the files of those screens.
impl AppManager {
    async fn enabled_apps(&self, tenant: Uuid) -> Result<Arc<Vec<Enabled>>> {
        if let Some(hit) = self.enabled.get(&tenant).filter(|e| e.0.elapsed() < ENABLED_TTL) {
            return Ok(hit.1.clone());
        }
        let ws = self.workspace(tenant).await?;
        let mut out = Vec::new();
        for (id, inst) in ws.apps.iter().filter(|(_, i)| i.enabled) {
            let version = inst.version.clone();
            if let Some(m) = self.native(id) {
                out.push(Enabled { id: id.clone(), version, manifest: Arc::new(m.clone()), bundled: None });
            } else if let Some(r) = self.release_of(tenant, id, &inst.version).await?.filter(|r| r.status != "revoked")
                && let Some(rec) = recorded(&r)
            {
                out.push(Enabled { id: id.clone(), version, manifest: Arc::new(rec.app), bundled: Some((rec.permissions, r)) });
            }
        }
        let out = Arc::new(out);
        self.enabled.insert(tenant, (Instant::now(), out.clone()));
        Ok(out)
    }

    /// `GET /api/meta/contributions/{point}`: what the enabled apps contribute to a screen slot
    /// or to an enabled app's extension point, each item with its `app` and `version`. Panels
    /// only on entities the caller may read; the settings page only for administrators.
    pub async fn contributions(&self, ctx: &Ctx, point: &str) -> Result<Vec<Value>> {
        let apps = self.enabled_apps(ctx.tenant).await?;
        if !ferrum_apps::UI_POINTS.contains(&point) {
            let owner = point.split_once('.').map_or("", |(o, _)| o);
            if !apps.iter().any(|a| a.id == owner && a.manifest.extension_points.contains_key(point)) {
                return Err(Error::NotFound(format!("extension point {point}")));
            }
        }
        if point == "ui.settings" && !ctx.is_admin {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for a in apps.iter() {
            for item in a.manifest.contributes.get(point).into_iter().flatten() {
                if let Some(entity) = item.get("entity").and_then(Value::as_str)
                    && point == "ui.panel"
                    && ctx.perms.access(entity, Action::Read) == Access::None
                {
                    continue;
                }
                let mut v = item.clone();
                if let Some(o) = v.as_object_mut() {
                    o.insert("app".into(), json!(a.id));
                    o.insert("version".into(), json!(a.version));
                }
                out.push(v);
            }
        }
        Ok(out)
    }

    /// What an enabled app's screens may reach for a user: its consent's records (entity →
    /// actions) and its own entities. `None` when it has no screens here (not installed, not
    /// enabled, revoked, or native).
    pub async fn screen_scope(&self, tenant: Uuid, app: &str) -> Result<Option<(BTreeMap<String, BTreeSet<String>>, BTreeSet<String>)>> {
        let apps = self.enabled_apps(tenant).await?;
        Ok(apps
            .iter()
            .find(|a| a.id == app && a.manifest.contributes.keys().any(|p| p.starts_with("ui.")))
            .and_then(|a| a.bundled.as_ref())
            .map(|(p, _)| (p.entities.clone(), p.new_entities.clone())))
    }

    /// A file of an enabled app's screens (`path` under its `ui/`), with the bundle digest that
    /// validates it. Bundles come from object storage once per replica.
    pub async fn screen_file(&self, tenant: Uuid, app: &str, version: &str, path: &str) -> Result<Option<(Bytes, String)>> {
        let apps = self.enabled_apps(tenant).await?;
        let Some((_, r)) = apps.iter().find(|a| a.id == app && a.version == version).and_then(|a| a.bundled.as_ref()) else { return Ok(None) };
        let files = match self.screen_files.get(&r.digest).map(|f| f.clone()) {
            Some(f) => f,
            None => {
                let b = self.load_bundle(r).await?;
                let files: BTreeMap<String, Bytes> =
                    b.paths().filter(|p| p.starts_with("ui/")).map(|p| (p.to_string(), Bytes::copy_from_slice(b.file(p).unwrap_or_default()))).collect();
                let size: usize = files.values().map(Bytes::len).sum();
                // ponytail: starts over when full; an LRU if many apps' screens are busy at once.
                if self.screen_bytes.fetch_add(size, Ordering::Relaxed) + size > SCREEN_CACHE_MAX {
                    self.screen_files.clear();
                    self.screen_bytes.store(size, Ordering::Relaxed);
                }
                let files = Arc::new(files);
                self.screen_files.insert(r.digest.clone(), files.clone());
                files
            }
        };
        Ok(files.get(&format!("ui/{path}")).map(|b| (b.clone(), r.digest.clone())))
    }
}

/// The shell (docs/plan/apps-ui.md): the workspace's apps with their launcher entries, routes and
/// interfaces.
impl AppManager {
    /// A workspace's enabled apps, in install order.
    pub async fn shell_apps(&self, tenant: Uuid) -> Result<Vec<crate::shell::ShellApp>> {
        Ok(self
            .enabled_apps(tenant)
            .await?
            .iter()
            .map(|a| crate::shell::ShellApp { id: a.id.clone(), version: a.version.clone(), manifest: a.manifest.clone(), bundled: a.bundled.is_some() })
            .collect())
    }

    /// The manifest of an app compiled into this build.
    pub fn native_manifest(&self, id: &str) -> Option<&AppManifest> {
        self.native(id)
    }

    /// Records ERP as installed in every workspace that has its packages (accounting): ERP became
    /// an app of its own after those workspaces installed the packages. Runs at start-up;
    /// idempotent; logged, not fatal.
    pub async fn ensure_erp(&self) {
        let Some(m) = self.native("erp") else { return };
        let outcome: Result<usize> = async {
            let mut tx = self.kernel.db.system_tx().await?;
            let tenants: Vec<Uuid> = sqlx::query_scalar(
                "select t.id from tenant t where exists (select 1 from installed_package p where p.tenant_id = t.id and p.name = 'accounting') \
                 and not exists (select 1 from tenant_app a where a.tenant_id = t.id and a.app_id = 'erp')",
            )
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            for tenant in &tenants {
                let ctx = Ctx::system(*tenant, self.kernel.registry.schema(*tenant).await?);
                self.kernel.record_installation(&ctx, "erp", &m.version.to_string(), None, None).await?;
                self.enabled.remove(tenant);
            }
            Ok(tenants.len())
        }
        .await;
        match outcome {
            Ok(0) => {}
            Ok(n) => tracing::info!(workspaces = n, "ERP recorded as an installed app"),
            Err(e) => tracing::error!("recording ERP as an installed app failed: {e}"),
        }
    }

    /// Makes sure the interface of a workspace's enabled app from a bundle is a stored release
    /// (bundles made available before interfaces existed, or whose release files were purged).
    /// False when the app has none.
    pub async fn ensure_ui_release(&self, tenant: Uuid, app: &str, version: &str) -> Result<bool> {
        let apps = self.enabled_apps(tenant).await?;
        let Some((_, r)) = apps.iter().find(|a| a.id == app && a.version == version).and_then(|a| a.bundled.as_ref()) else { return Ok(false) };
        let b = self.load_bundle(r).await?;
        let Some(ui) = crate::shell::bundle_ui(&b).map_err(app_err)? else { return Ok(false) };
        crate::shell::store_release(&self.kernel, &self.storage, app, version, &ui).await?;
        Ok(true)
    }

    /// Refuses routes another app already owns on this cluster (its routes and their aliases are
    /// unique among available releases and compiled-in apps).
    async fn check_routes(&self, m: &AppManifest) -> Result<()> {
        let mine: Vec<String> = m.routes().into_iter().flat_map(|(r, aliases)| std::iter::once(r).chain(aliases)).collect();
        if mine.is_empty() {
            return Ok(());
        }
        let mut others: Vec<(String, AppManifest)> = self.natives.iter().filter(|n| n.id != m.id).map(|n| (n.id.clone(), n.clone())).collect();
        for r in self.kernel.releases(None).await? {
            if r.app_id != m.id
                && r.status != "revoked"
                && let Some(rec) = recorded(&r)
            {
                others.push((r.app_id.clone(), rec.app));
            }
        }
        for (id, other) in others {
            for (route, aliases) in other.routes() {
                if let Some(clash) = std::iter::once(&route).chain(aliases.iter()).find(|r| mine.contains(r)) {
                    return Err(Error::Conflict(format!("the address /{clash}/ belongs to {id} on this server; {} needs routes of its own", m.id)));
                }
            }
        }
        Ok(())
    }
}

/// The `app.yaml` of every native app compiled into this build. One that does not parse leaves
/// that app out of the App Manager (logged); its packages keep working.
pub fn compiled_natives() -> Vec<AppManifest> {
    crate::packages::native_manifests()
        .into_iter()
        .filter_map(|yaml| match AppManifest::parse(yaml) {
            Ok(m) => Some(m),
            Err(e) => {
                let id = yaml.lines().find_map(|l| l.strip_prefix("id:")).map(str::trim).unwrap_or("?");
                tracing::error!(app = %id, "native app left out of the App Manager: its {e}");
                None
            }
        })
        .collect()
}

/// `[a-z][a-z0-9-]{1,39}`, the app id syntax.
pub fn valid_id(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=40).contains(&b.len()) && b[0].is_ascii_lowercase() && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("administrator access required".into())) }
}

fn user_of(ctx: &Ctx) -> Option<Uuid> {
    (!ctx.user.is_nil()).then_some(ctx.user)
}

fn app_err(e: AppError) -> Error {
    match e {
        AppError::Signature(_) => Error::Forbidden(e.to_string()),
        _ => Error::Invalid(e.to_string()),
    }
}

fn ignore_missing(r: Result<()>) -> Result<()> {
    match r {
        Err(Error::NotFound(_)) => Ok(()),
        other => other,
    }
}

fn bundle_key(id: &str, version: &Version, digest: &str) -> String {
    format!("apps/{id}/{version}-{}.fapp", &digest[..digest.len().min(16)])
}

fn readme(b: &Bundle) -> Option<String> {
    let text = String::from_utf8_lossy(b.file(b.manifest.description.as_deref()?)?);
    let mut out: String = text.chars().take(README_MAX).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    Some(out)
}

/// Unpacks and validates a bundle off the async workers (gunzip and hashing up to 50 MiB).
async fn read_bundle(bytes: Bytes) -> Result<Bundle> {
    if bytes.len() > MAX_BUNDLE {
        return Err(Error::invalid("app bundles are limited to 50 MB"));
    }
    tokio::task::spawn_blocking(move || Bundle::read(&bytes)).await.map_err(Error::internal)?.map_err(app_err)
}

/// Whether `sig_b64` is `public_key`'s Ed25519 signature of `message`.
fn ed25519(public_key: &[u8], message: &[u8], sig_b64: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(sig_b64.trim())
        .is_ok_and(|sig| ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key).verify(message, &sig).is_ok())
}

fn summary(m: &AppManifest, source: &str, verified: bool, private: bool, icon_url: Option<String>) -> AppSummary {
    AppSummary {
        id: m.id.clone(),
        name: m.name.clone(),
        version: m.version.to_string(),
        runtime: m.runtime.to_string(),
        summary: m.summary.clone(),
        categories: m.categories.clone(),
        publisher: Some(m.publisher.clone()),
        verified,
        icon_url,
        source: source.into(),
        private,
        parent: m.parent.clone(),
    }
}

/// Adds `p` to `into` (the permissions of several apps in one consent).
fn merge(into: &mut Permissions, p: &Permissions) {
    fn sets(a: &mut BTreeMap<String, BTreeSet<String>>, b: &BTreeMap<String, BTreeSet<String>>) {
        for (k, v) in b {
            a.entry(k.clone()).or_default().extend(v.iter().cloned());
        }
    }
    // Destructured so that a new kind of permission cannot be forgotten here.
    let Permissions { native, entities, hooks, hosts, public_routes, sockets, schedules, subscriptions, new_entities, overlays, roles, sql } = p;
    into.native |= native;
    sets(&mut into.entities, entities);
    sets(&mut into.hooks, hooks);
    sets(&mut into.overlays, overlays);
    for (role, grants) in roles {
        sets(into.roles.entry(role.clone()).or_default(), grants);
    }
    into.hosts.extend(hosts.iter().cloned());
    into.public_routes.extend(public_routes.iter().cloned());
    into.sockets.extend(sockets.iter().cloned());
    into.schedules.extend(schedules.iter().cloned());
    into.subscriptions.extend(subscriptions.iter().cloned());
    into.new_entities.extend(new_entities.iter().cloned());
    into.sql.extend(sql.iter().cloned());
}

fn plan_digest(steps: &[StepData]) -> String {
    let mut h = Sha256::new();
    h.update(b"ferrum-app-plan-v1\n");
    for s in steps {
        h.update(format!("{}\0{}\0{}\n", s.id, s.version, s.permissions.digest()).as_bytes());
    }
    hex::encode(h.finalize())
}

fn pick<'a>(offers: &'a BTreeMap<String, Vec<Offer>>, id: &str, version: &Version) -> Option<&'a Offer> {
    offers.get(id)?.iter().find(|o| o.res.version == *version)
}

/// Sandboxed and metadata apps may give their roles access only to entities they define (their
/// packages' and their plugin's): anything more would widen what the role's holders see of the
/// rest of the workspace — installing even merges it into a platform role of the same name.
fn role_rule(b: &Bundle) -> Result<()> {
    let packages = b.packages().map_err(app_err)?;
    let mut own: BTreeSet<String> = packages.iter().flat_map(|p| p.entities.iter().map(|e| e.name.clone())).collect();
    if let Some((_, plugin)) = b.plugin().map_err(app_err)? {
        own.extend(plugin["entities"].as_array().into_iter().flatten().filter_map(|e| e["name"].as_str().map(String::from)));
    }
    for p in &packages {
        for role in &p.manifest.roles {
            if let Some(g) = role.permissions.iter().find(|g| !own.contains(&g.entity)) {
                return Err(Error::Forbidden(format!(
                    "{}: role '{}' (package {}) grants access to {}, which this app does not define; apps may give roles access only to their own entities",
                    b.manifest.id,
                    role.name,
                    p.name(),
                    g.entity
                )));
            }
        }
    }
    Ok(())
}

impl AppManager {
    pub fn new(
        kernel: Arc<Kernel>,
        plugins: Arc<PluginHost>,
        storage: Arc<Storage>,
        settings: &AppsSettings,
        natives: Vec<AppManifest>,
    ) -> std::result::Result<AppManager, String> {
        let list = |s: &str| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>();
        let mut stores = Vec::new();
        for s in list(&settings.registries) {
            let url = reqwest::Url::parse(&s).map_err(|e| format!("apps.registries: '{s}' is not a URL: {e}"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(format!("apps.registries: '{s}' must be an http(s) URL"));
            }
            stores.push(s.trim_end_matches('/').to_string());
        }
        let trusted = list(&settings.trusted_keys)
            .iter()
            .map(|k| TrustedKey::parse(k).map_err(|e| format!("apps.trusted_keys: {e}")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !stores.is_empty() && !trusted.iter().any(|k| k.role == Role::Store) {
            tracing::warn!("apps.registries is set but apps.trusted_keys holds no store key: apps from the stores cannot be installed");
        }
        let catalog = kernel.registry.catalog();
        let natives = natives
            .into_iter()
            .filter(|m| {
                let ok = m.packages.iter().all(|p| catalog.package(p).is_some());
                if !ok {
                    tracing::warn!(app = %m.id, "native app left out of the App Manager: its packages are not in the catalog");
                }
                ok
            })
            .collect();
        let http = reqwest::Client::builder()
            .user_agent(concat!("ferrum/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(JSON_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;
        Ok(AppManager {
            kernel,
            plugins,
            storage,
            stores,
            trusted,
            uploads: settings.uploads,
            allow_unsigned: settings.allow_unsigned_uploads,
            natives,
            platform: Version::parse(PLATFORM_VERSION).map_err(|e| e.to_string())?,
            http,
            cache: DashMap::new(),
            enabled: DashMap::new(),
            screen_files: DashMap::new(),
            screen_bytes: AtomicUsize::new(0),
        })
    }

    fn native(&self, id: &str) -> Option<&AppManifest> {
        self.natives.iter().find(|m| m.id == id)
    }

    /// A native app asks for everything (it is reviewed platform code); its consent screen lists
    /// the entities its packages bring.
    fn native_permissions(&self, m: &AppManifest) -> Permissions {
        let catalog = self.kernel.registry.catalog();
        let new_entities = m.packages.iter().filter_map(|p| catalog.package(p)).flat_map(|p| p.entities.iter().map(|e| e.name.clone())).collect();
        Permissions { native: true, new_entities, ..Permissions::default() }
    }

    /// Whether the workspace's packages of a native app predate this build's (roles and seed
    /// data of the new version are installed by upgrading).
    fn stale(&self, ws: &Workspace, m: &AppManifest) -> bool {
        let catalog = self.kernel.registry.catalog();
        m.packages.iter().any(|p| matches!((ws.packages.get(p), catalog.package(p)), (Some(v), Some(pkg)) if *v != pkg.manifest.version))
    }

    /// Whether a trusted key (of `role`, or of any role) signed this release's digest.
    fn signed(&self, r: &Release, role: Option<Role>) -> bool {
        let Some(sigs) = r.signatures.clone().and_then(|s| serde_json::from_value::<ferrum_apps::Signatures>(s).ok()) else { return false };
        let Ok(digest) = hex::decode(&r.digest) else { return false };
        sigs.digest == r.digest
            && sigs.signatures.iter().any(|e| {
                role.is_none_or(|want| want == e.role)
                    && self.trusted.iter().any(|k| k.key_id == e.key_id && k.role == e.role && ed25519(&k.public_key, &digest, &e.sig))
            })
    }

    fn local_summary(&self, r: &Release) -> Option<AppSummary> {
        let rec = recorded(r)?;
        Some(summary(&rec.app, &r.source, self.signed(r, Some(Role::Store)), r.private_tenant.is_some(), None))
    }

    async fn fresh(&self, ctx: &Ctx) -> Result<Ctx> {
        if ctx.is_system {
            Ok(Ctx::system(ctx.tenant, self.kernel.registry.schema(ctx.tenant).await?))
        } else {
            self.kernel.load_ctx(ctx.tenant, ctx.user, None, ctx.request_id.clone()).await
        }
    }

    async fn lock(&self, tenant: Uuid) -> Result<WorkspaceLock> {
        let mut tx = self.kernel.db.pool.begin().await?;
        // Store downloads and table creation may take a while; keep the lock meanwhile.
        sqlx::query("select set_config('idle_in_transaction_session_timeout', '900000', true)").execute(&mut *tx).await?;
        let got: bool =
            sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("ferrum:apps:{tenant}")).fetch_one(&mut *tx).await?;
        if !got {
            return Err(Error::Conflict("another app change is running in this workspace; try again when it has finished".into()));
        }
        Ok(WorkspaceLock(tx))
    }

    async fn workspace(&self, tenant: Uuid) -> Result<Workspace> {
        let schema = self.kernel.registry.schema(tenant).await?;
        let rows = self.kernel.installations(tenant).await?;
        let mut tx = self.kernel.db.tenant_tx(tenant).await?;
        let pkgs: Vec<(String, String, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as("select name, version, installed_at from installed_package where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let mut apps: BTreeMap<String, Installed> = rows
            .into_iter()
            .map(|i| {
                (
                    i.app_id,
                    Installed {
                        version: i.version,
                        enabled: i.enabled,
                        plugin_id: i.plugin_id,
                        installed_at: i.installed_at,
                        row: true,
                        brought_by: i.brought_by,
                    },
                )
            })
            .collect();
        // Native apps whose packages the workspace already has (installed when it was created).
        for m in &self.natives {
            if apps.contains_key(&m.id) || m.packages.is_empty() || !m.packages.iter().all(|p| schema.packages.contains(p)) {
                continue;
            }
            let at = pkgs.iter().filter(|(n, ..)| m.packages.contains(n)).map(|(.., t)| *t).min().unwrap_or_else(chrono::Utc::now);
            apps.insert(
                m.id.clone(),
                Installed { version: m.version.to_string(), enabled: true, plugin_id: None, installed_at: at, row: false, brought_by: None },
            );
        }
        Ok(Workspace { apps, packages: pkgs.into_iter().map(|(n, v, _)| (n, v)).collect(), schema })
    }

    /// A release of an app this workspace can see, whatever its status.
    async fn release_of(&self, tenant: Uuid, id: &str, version: &str) -> Result<Option<Release>> {
        let mut tx = self.kernel.db.system_tx().await?;
        let r = sqlx::query_as::<_, Release>(
            "select id, app_id, version, runtime, manifest, digest, signatures, source, bundle_key, packages, plugin_sha256, status, status_reason, \
             private_tenant, created_at from app_release where app_id = $1 and version = $2 and (private_tenant is null or private_tenant = $3) \
             order by private_tenant nulls last limit 1",
        )
        .bind(id)
        .bind(version)
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// The manifest of an installed app's version.
    async fn manifest_of(&self, tenant: Uuid, id: &str, version: &str) -> Result<Option<AppManifest>> {
        if let Some(m) = self.native(id) {
            return Ok(Some(m.clone()));
        }
        Ok(self.release_of(tenant, id, version).await?.as_ref().and_then(recorded).map(|r| r.app))
    }

    /// Installed apps (optionally only enabled ones) that require `id`.
    async fn dependents(&self, tenant: Uuid, ws: &Workspace, id: &str, enabled_only: bool) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (other, inst) in ws.apps.iter().filter(|(o, i)| o.as_str() != id && (!enabled_only || i.enabled)) {
            if self.manifest_of(tenant, other, &inst.version).await?.is_some_and(|m| m.requires.contains_key(id)) {
                out.push(other.clone());
            }
        }
        Ok(out)
    }

    async fn plugin_named(&self, tenant: Uuid, name: &str) -> Result<Option<(Uuid, bool)>> {
        let mut tx = self.kernel.db.tenant_tx(tenant).await?;
        let row = sqlx::query_as("select id, enabled from plugin where tenant_id = $1 and name = $2").bind(tenant).bind(name).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(row)
    }

    // ───────────────────────── stores ─────────────────────────

    /// GETs `url` (a cap on the size, a timeout) into memory.
    async fn fetch(&self, url: &str, max: usize, timeout: Duration) -> std::result::Result<Bytes, String> {
        let mut resp = self.http.get(url).timeout(timeout).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        if resp.content_length().is_some_and(|n| n > max as u64) {
            return Err(format!("the response is larger than {} MiB", max >> 20));
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            if out.len() + chunk.len() > max {
                return Err(format!("the response is larger than {} MiB", max >> 20));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(out))
    }

    /// A store's JSON answer, cached for a minute (failures too, so a store that is down does
    /// not slow every page).
    async fn get_json(&self, url: String) -> std::result::Result<Value, String> {
        if let Some(hit) = self.cache.get(&url)
            && hit.0.elapsed() < CACHE_TTL
        {
            return hit.1.clone();
        }
        let out = match self.fetch(&url, MAX_JSON, JSON_TIMEOUT).await {
            Ok(body) => serde_json::from_slice(&body).map_err(|e| format!("not JSON: {e}")),
            Err(e) => Err(e),
        };
        if self.cache.len() > 1_000 {
            self.cache.retain(|_, v| v.0.elapsed() < CACHE_TTL);
            if self.cache.len() > 1_000 {
                self.cache.clear();
            }
        }
        self.cache.insert(url, (Instant::now(), out.clone()));
        out
    }

    async fn store_search(&self, url: &str, q: &str, category: &str) -> std::result::Result<Vec<store::App>, String> {
        let u = reqwest::Url::parse_with_params(&format!("{url}/v1/apps"), &[("q", q), ("category", category), ("platform", PLATFORM_VERSION)])
            .map_err(|e| e.to_string())?;
        Ok(store::apps(&self.get_json(u.to_string()).await?, url))
    }

    async fn store_app(&self, url: &str, id: &str) -> Option<store::App> {
        store::app(&self.get_json(format!("{url}/v1/apps/{id}")).await.ok()?, url)
    }

    /// A store's releases of an app (none when the store does not have it).
    async fn store_releases(&self, url: &str, id: &str) -> std::result::Result<Vec<store::Release>, String> {
        match self.get_json(format!("{url}/v1/apps/{id}/releases")).await {
            Ok(v) => Ok(store::releases(&v)),
            Err(e) if e == NOT_FOUND => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    // ───────────────────────── bundles ─────────────────────────

    async fn put(&self, key: &str, bytes: Bytes) -> Result<()> {
        let path = Path::new(key);
        // Keys carry the content digest: an object already there holds the same bundle.
        if self.storage.exists(path).await.unwrap_or(false) {
            return Ok(());
        }
        self.storage.upload(path, &bytes).await.map_err(|e| Error::Unavailable(format!("object storage: {e}")))
    }

    async fn load_bundle(&self, r: &Release) -> Result<Bundle> {
        let key = r.bundle_key.as_deref().ok_or_else(|| Error::internal(format!("{} {} has no stored bundle", r.app_id, r.version)))?;
        let bytes: Vec<u8> = self.storage.download(Path::new(key)).await.map_err(|e| Error::Unavailable(format!("object storage ({key}): {e}")))?;
        let b = read_bundle(Bytes::from(bytes)).await?;
        if b.digest() != r.digest {
            return Err(Error::internal(format!("the stored bundle {key} does not match its release (digest {})", r.digest)));
        }
        Ok(b)
    }

    fn trust(&self, b: &Bundle) -> Result<Trust> {
        match b.verify(&self.trusted) {
            Ok(_) => Ok(Trust::Trusted),
            Err(e) => {
                // Changed after signing, or a bad signature by a key we trust: never acceptable.
                let sigs = b.signatures.as_ref();
                let tampered = sigs.is_some_and(|s| s.digest != b.digest());
                let by_trusted = sigs.is_some_and(|s| s.signatures.iter().any(|x| self.trusted.iter().any(|k| k.key_id == x.key_id && k.role == x.role)));
                if tampered || by_trusted {
                    Err(Error::Forbidden(format!("{} {}: {e}", b.manifest.id, b.manifest.version)))
                } else {
                    Ok(Trust::Untrusted(e.to_string()))
                }
            }
        }
    }

    /// Rules for every bundle this server takes: no native code, a compatible platform, the role rule.
    fn check_app(&self, b: &Bundle) -> Result<()> {
        let m = &b.manifest;
        if m.runtime == Runtime::Native {
            return Err(Error::Forbidden(format!("{} is a native app: its code is compiled into Ferrum builds and cannot be installed from a bundle", m.id)));
        }
        if !m.compatible_with(&self.platform) {
            return Err(Error::Invalid(format!("{} {} needs platform {}, but this server provides {}", m.id, m.version, m.platform, self.platform)));
        }
        role_rule(b)
    }

    fn check_store_bundle(&self, b: &Bundle, id: &str, what: &str, rel: &store::Release) -> Result<()> {
        let signed = b.verify(&self.trusted).map_err(|e| Error::Forbidden(format!("{what}: {e}")))?;
        if !signed.iter().any(|(_, role)| *role == Role::Store) {
            return Err(Error::Forbidden(format!("{what} is not countersigned by a store key this server trusts (apps.trusted_keys)")));
        }
        if let Some(d) = &rel.digest
            && *d != b.digest()
        {
            return Err(Error::Forbidden(format!("{what}: the bundle's digest {} differs from the store's listing ({d})", b.digest())));
        }
        if b.manifest.id != id || b.manifest.version != rel.version {
            return Err(Error::Forbidden(format!("{what}: the store sent {} {} instead", b.manifest.id, b.manifest.version)));
        }
        self.check_app(b)
    }

    /// A store release, verified and kept in object storage (a bundle staged by an earlier plan
    /// is reused): the store's countersignature by a trusted key, the listed digest, the role rule.
    async fn stage(&self, id: &str, url: &str, rel: &store::Release) -> Result<(Bundle, String)> {
        let what = format!("{id} {} from {url}", rel.version);
        if let Some(d) = &rel.digest {
            let key = bundle_key(id, &rel.version, d);
            if let Ok(bytes) = self.storage.download::<Vec<u8>>(Path::new(&key)).await
                && let Ok(b) = read_bundle(Bytes::from(bytes)).await
                && b.digest() == *d
            {
                self.check_store_bundle(&b, id, &what, rel)?;
                return Ok((b, key));
            }
        }
        let bytes =
            self.fetch(&rel.bundle_url(url, id), MAX_BUNDLE, BUNDLE_TIMEOUT).await.map_err(|e| Error::Unavailable(format!("{what}: download failed: {e}")))?;
        let b = read_bundle(bytes.clone()).await.map_err(|e| Error::Invalid(format!("{what}: {e}")))?;
        self.check_store_bundle(&b, id, &what, rel)?;
        let key = bundle_key(id, &rel.version, &b.digest());
        self.put(&key, bytes).await?;
        Ok((b, key))
    }

    /// Makes a verified bundle (already in object storage at `key`) available: on the cluster,
    /// or to one workspace (`private`).
    async fn publish(&self, b: &Bundle, key: &str, private: Option<Uuid>, source: &str, by: Option<Uuid>) -> Result<Release> {
        let plugin = b.plugin().map_err(app_err)?;
        self.check_routes(&b.manifest).await?;
        // An interface of its own runs on the workspace's host with the person's session (until
        // third parties get origins of their own, docs/plan/apps-ui.md U8): only from bundles this
        // server trusts, or uploaded for the whole cluster where its operator allows that.
        if b.manifest.ui.is_some() && private.is_some() && !matches!(self.trust(b)?, Trust::Trusted) {
            return Err(Error::Forbidden(format!(
                "{} brings an interface of its own, which only apps signed by a key this server trusts may do when private to a workspace",
                b.manifest.id
            )));
        }
        if let Some(ui) = crate::shell::bundle_ui(b).map_err(app_err)? {
            crate::shell::store_release(&self.kernel, &self.storage, &b.manifest.id, &b.manifest.version.to_string(), &ui).await?;
        }
        let manifest = json!({
            "app": b.manifest,
            "permissions": b.permissions().map_err(app_err)?,
            "plugin": plugin.as_ref().map(|(_, json)| json.clone()),
            "readme": readme(b),
        });
        self.kernel
            .make_available(NewRelease {
                app_id: b.manifest.id.clone(),
                version: b.manifest.version.to_string(),
                runtime: b.manifest.runtime.to_string(),
                manifest,
                digest: b.digest(),
                signatures: b.signatures.as_ref().map(serde_json::to_value).transpose().map_err(Error::internal)?,
                source: source.into(),
                bundle_key: Some(key.into()),
                packages: b.package_files(),
                plugin_sha256: plugin.map(|(wasm, _)| hex::encode(Sha256::digest(&wasm))),
                private_tenant: private,
                created_by: by,
            })
            .await
    }

    /// Verifies an uploaded bundle and makes it available (to the cluster, or privately).
    async fn accept(&self, bytes: Bytes, private: Option<Uuid>, allow_unsigned: bool, by: Option<Uuid>) -> Result<(Bundle, Release)> {
        let b = read_bundle(bytes.clone()).await?;
        let (id, version) = (b.manifest.id.clone(), b.manifest.version.clone());
        if let Trust::Untrusted(why) = self.trust(&b)?
            && !allow_unsigned
        {
            return Err(Error::Forbidden(format!("{id} {version}: {why}; this server accepts only bundles signed by a key it trusts (apps.trusted_keys)")));
        }
        self.check_app(&b)?;
        if self.native(&id).is_some() {
            return Err(Error::Conflict(format!("{id} is built into this server and cannot be replaced by an upload")));
        }
        let local = self.kernel.releases(private).await?;
        if private.is_some() {
            if !b.manifest.packages.is_empty() {
                return Err(Error::Forbidden(format!(
                    "{id} brings packages (tables of its own), which apps private to a workspace cannot; it must be made available from a store"
                )));
            }
            if local.iter().any(|r| r.app_id == id && r.private_tenant.is_none()) {
                return Err(Error::Conflict(format!("an app called {id} is already available on this server; a private app needs an id of its own")));
            }
        }
        if let Some(current) = local.iter().filter(|r| r.app_id == id && r.private_tenant == private).find_map(|r| Version::parse(&r.version).ok())
            && version < current
        {
            return Err(Error::Conflict(format!("{id} {current} is already available; going back to {version} is not supported")));
        }
        let key = bundle_key(&id, &version, &b.digest());
        self.put(&key, bytes).await?;
        let r = self.publish(&b, &key, private, "upload", by).await?;
        Ok((b, r))
    }

    // ───────────────────────── plans ─────────────────────────

    /// Releases of `id` this workspace may install: this build's native app; else the cluster's
    /// release (and the workspace's private ones), plus the stores' releases for apps the cluster
    /// does not have yet — or for every app when uploads are cluster-wide (an upgrade then makes
    /// the new release available to all workspaces).
    async fn offers_for(&self, id: &str, local: &[Release], with_store: bool, errors: &mut Vec<String>) -> Vec<Offer> {
        if let Some(m) = self.native(id) {
            return vec![Offer { res: m.into(), origin: Origin::Native }];
        }
        let mut out: Vec<Offer> = local
            .iter()
            .filter(|r| r.app_id == id)
            .filter_map(|r| Some(Offer { res: (&recorded(r)?.app).into(), origin: Origin::Local(Box::new(r.clone())) }))
            .collect();
        let private = local.iter().any(|r| r.app_id == id && r.private_tenant.is_some());
        let cluster = local.iter().any(|r| r.app_id == id && r.private_tenant.is_none());
        if !with_store || private || (cluster && self.uploads != Uploads::Cluster) {
            return out;
        }
        for url in &self.stores {
            match self.store_releases(url, id).await {
                Ok(list) => out.extend(list.into_iter().map(|rel| Offer {
                    res: ferrum_apps::Release { id: id.into(), version: rel.version.clone(), requires: rel.requires.clone(), platform: rel.platform.clone() },
                    origin: Origin::Store { url: url.clone(), rel },
                })),
                Err(e) => errors.push(format!("{url}: {e}")),
            }
        }
        out
    }

    /// Adds the offers for `queue` and everything they may require. Installed apps are never
    /// changed by a plan unless requested, so the stores are asked only about the others.
    async fn gather(
        &self,
        root: &str,
        ws: &Workspace,
        local: &[Release],
        offers: &mut BTreeMap<String, Vec<Offer>>,
        mut queue: Vec<String>,
        errors: &mut Vec<String>,
    ) -> Result<()> {
        while let Some(id) = queue.pop() {
            if offers.contains_key(&id) || !valid_id(&id) {
                continue;
            }
            if offers.len() >= MAX_APPS {
                return Err(Error::invalid(format!("{root} involves more than {MAX_APPS} apps; install its dependencies first")));
            }
            let with_store = id == root || !ws.apps.contains_key(&id);
            let list = self.offers_for(&id, local, with_store, errors).await;
            queue.extend(list.iter().flat_map(|o| o.res.requires.keys().cloned()));
            offers.insert(id, list);
        }
        Ok(())
    }

    fn resolve(&self, root: &str, req: &VersionReq, ws: &Workspace, offers: &BTreeMap<String, Vec<Offer>>) -> Result<Vec<ferrum_apps::Release>> {
        let installed: BTreeMap<String, Version> = ws.apps.iter().filter_map(|(id, i)| Some((id.clone(), Version::parse(&i.version).ok()?))).collect();
        let available = |id: &str| offers.get(id).map(|l| l.iter().map(|o| o.res.clone()).collect()).unwrap_or_default();
        ferrum_apps::resolve(&[(root.to_string(), req.clone())], &installed, &available, &self.platform).map_err(|e| Error::Invalid(e.to_string()))
    }

    /// What installing `id` (at `version`, or the newest that fits) would do in this workspace.
    pub async fn plan(&self, ctx: &Ctx, id: &str, version: Option<&str>) -> Result<Plan> {
        Ok(self.plan_inner(ctx, id, version).await?.0)
    }

    async fn plan_inner(&self, ctx: &Ctx, id: &str, version: Option<&str>) -> Result<(Plan, Vec<StepData>, Workspace)> {
        admin(ctx)?;
        if !valid_id(id) {
            return Err(Error::NotFound(format!("app {id}")));
        }
        let req = match version.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => VersionReq::parse(&format!("={v}")).map_err(|e| Error::field("version", format!("'{v}' is not a version: {e}")))?,
            None => VersionReq::STAR,
        };
        let ws = self.workspace(ctx.tenant).await?;
        let local = self.kernel.releases(Some(ctx.tenant)).await?;
        let mut offers = BTreeMap::new();
        let mut errors = Vec::new();
        let mut queue: Vec<String> = ws.apps.keys().cloned().collect();
        queue.push(id.to_string());
        self.gather(id, &ws, &local, &mut offers, queue, &mut errors).await?;

        // Store listings may be sparse: once a store bundle is fetched its manifest is the
        // authority, and the plan is resolved again if it says something else.
        let mut staged: BTreeMap<(String, Version), (Bundle, String)> = BTreeMap::new();
        let mut rounds = 0;
        let order = loop {
            let order = self.resolve(id, &req, &ws, &offers).map_err(|e| match e {
                Error::Invalid(m) if !errors.is_empty() => Error::Invalid(format!("{m} (stores that did not answer: {})", errors.join("; "))),
                e => e,
            })?;
            let (mut changed, mut more) = (false, Vec::new());
            for r in &order {
                let key = (r.id.clone(), r.version.clone());
                if staged.contains_key(&key) {
                    continue;
                }
                let Some(Origin::Store { url, rel }) = pick(&offers, &r.id, &r.version).map(|o| o.origin.clone()) else { continue };
                let (bundle, object) = self.stage(&r.id, &url, &rel).await?;
                let res = ferrum_apps::Release::from(&bundle.manifest);
                if res.requires != r.requires || res.platform != r.platform {
                    changed = true;
                    more.extend(res.requires.keys().cloned());
                    for o in offers.get_mut(&r.id).into_iter().flatten().filter(|o| o.res.version == r.version) {
                        o.res = res.clone();
                    }
                }
                staged.insert(key, (bundle, object));
            }
            if !changed {
                break order;
            }
            rounds += 1;
            if rounds > 3 {
                return Err(Error::invalid(format!("the stores' listings of {id} and its dependencies do not match their bundles")));
            }
            self.gather(id, &ws, &local, &mut offers, more, &mut errors).await?;
        };

        let mut steps = Vec::with_capacity(order.len());
        for r in order {
            let offer = pick(&offers, &r.id, &r.version).cloned().ok_or_else(|| Error::internal(format!("{} {} vanished from the plan", r.id, r.version)))?;
            let bundle = staged.remove(&(r.id.clone(), r.version.clone()));
            let (manifest, permissions, readme_text, verified, trusted, source) = match &offer.origin {
                Origin::Native => {
                    let m = self.native(&r.id).cloned().ok_or_else(|| Error::internal("native app vanished"))?;
                    let p = self.native_permissions(&m);
                    (m, p, None, true, true, "native".to_string())
                }
                Origin::Local(rel) => {
                    let rec = recorded(rel).ok_or_else(|| Error::internal(format!("{} {}: unreadable release record", rel.app_id, rel.version)))?;
                    let source = if rel.private_tenant.is_some() { "private" } else { "cluster" };
                    (rec.app, rec.permissions, rec.readme, self.signed(rel, Some(Role::Store)), self.signed(rel, None), source.to_string())
                }
                Origin::Store { url, .. } => {
                    let (b, _) = bundle.as_ref().ok_or_else(|| Error::internal("store bundle not staged"))?;
                    (b.manifest.clone(), b.permissions().map_err(app_err)?, readme(b), true, true, format!("store:{url}"))
                }
            };
            let action = match ws.apps.get(&r.id).map(|i| Version::parse(&i.version)) {
                None => StepAction::Install,
                Some(Ok(v)) if v == r.version && !(matches!(offer.origin, Origin::Native) && self.stale(&ws, &manifest)) => StepAction::Already,
                Some(_) => StepAction::Upgrade,
            };
            steps.push(StepData {
                id: r.id,
                version: r.version,
                action,
                origin: offer.origin,
                manifest,
                permissions,
                readme: readme_text,
                verified,
                trusted,
                staged: bundle,
                source,
            });
        }

        // A contribution to another app's extension point must meet that app's contract: the
        // version this plan brings, or the one installed.
        for s in steps.iter().filter(|s| s.action != StepAction::Already) {
            for (point, items) in &s.manifest.contributes {
                let Some((owner, _)) = point.split_once('.').filter(|(o, _)| *o != s.id && !ferrum_apps::UI_POINTS.contains(&point.as_str())) else { continue };
                let manifest = match (steps.iter().find(|o| o.id == owner), ws.apps.get(owner)) {
                    (Some(o), _) => Some(o.manifest.clone()),
                    (None, Some(inst)) => self.manifest_of(ctx.tenant, owner, &inst.version).await?,
                    // `requires` names the owner: the plan itself reports it missing.
                    (None, None) => None,
                };
                let Some(o) = manifest else { continue };
                let Some(ep) = o.extension_points.get(point) else {
                    return Err(Error::invalid(format!("{} contributes to {point}, which {owner} {} does not offer", s.id, o.version)));
                };
                for item in items.iter().filter(|_| ep.schema.is_some()) {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                    ferrum_apps::contract::conforms(&ferrum_apps::contribution_body(item), ep.schema.as_ref().unwrap_or(&Value::Null))
                        .map_err(|e| Error::invalid(format!("{} contributes {point} {id}, which breaks {owner}'s contract: {e}", s.id)))?;
                }
            }
        }
        let root = steps.iter().find(|s| s.id == id).ok_or_else(|| Error::internal("the plan lost its app"))?;
        let changing: Vec<&StepData> = steps.iter().filter(|s| s.action != StepAction::Already).collect();
        for s in &changing {
            if let Some(dep) = s.manifest.requires.keys().find(|d| ws.apps.get(*d).is_some_and(|i| !i.enabled)) {
                return Err(Error::invalid(format!("{} needs {dep}, which is disabled in this workspace; enable it first", s.id)));
            }
        }
        let mut permissions = Permissions::default();
        if changing.is_empty() {
            permissions = root.permissions.clone();
        }
        for s in &changing {
            merge(&mut permissions, &s.permissions);
        }
        let mut increases = Vec::new();
        for s in changing.iter().filter(|s| s.action == StepAction::Upgrade && !matches!(s.origin, Origin::Native)) {
            // What the installed version was allowed; unknown counts as nothing (all is new).
            let before = match ws.apps.get(&s.id) {
                Some(i) => self.release_of(ctx.tenant, &s.id, &i.version).await?.as_ref().and_then(recorded).map(|r| r.permissions).unwrap_or_default(),
                None => Permissions::default(),
            };
            increases.extend(s.permissions.increases(&before).into_iter().map(|l| if s.id == id { l } else { format!("{}: {l}", s.id) }));
        }
        let mut warnings: Vec<String> = changing
            .iter()
            .filter(|s| !s.trusted)
            .map(|s| format!("{} {} was uploaded without a signature this server trusts: nobody has reviewed its code.", s.id, s.version))
            .collect();
        if !errors.is_empty() {
            warnings.push(format!("Some app stores did not answer ({}); newer versions may exist.", errors.join("; ")));
        }
        let store_url = match &root.origin {
            Origin::Store { url, .. } => Some(url.clone()),
            Origin::Local(r) => r.source.strip_prefix("store:").filter(|u| self.stores.iter().any(|s| s == u)).map(String::from),
            Origin::Native => None,
        };
        let icon_url = match store_url {
            Some(url) => self.store_app(&url, id).await.and_then(|a| a.icon_url),
            None => None,
        };
        let m = &root.manifest;
        let origin = match &root.origin {
            Origin::Native => "native".to_string(),
            Origin::Local(r) => r.source.clone(),
            Origin::Store { url, .. } => format!("store:{url}"),
        };
        let plan = Plan {
            app: PlanApp {
                summary: summary(m, &origin, root.verified, root.source == "private", icon_url),
                description: root.readme.clone(),
                license: m.license.clone(),
                homepage: m.homepage.clone(),
            },
            steps: steps.iter().map(|s| Step { id: s.id.clone(), version: s.version.to_string(), action: s.action, source: s.source.clone() }).collect(),
            permissions,
            digest: plan_digest(&steps),
            increases,
            warnings,
            uninstall: m.uninstall,
        };
        Ok((plan, steps, ws))
    }

    // ───────────────────────── workspace operations ─────────────────────────

    /// Carries out the plan for `id` with the dependencies it lists; `upgrade` requires the app
    /// to be installed already (installing an installed app applies its plan too, as after
    /// uploading a newer version). The plan is computed again and `consent` must be its digest.
    /// Store releases are verified and made available on the cluster first; then the workspace
    /// gets packages, plugin and record, app by app — a failure takes back what this call did in
    /// the workspace.
    pub async fn install(&self, ctx: &Ctx, id: &str, version: Option<&str>, consent: &str, upgrade: bool) -> std::result::Result<Plan, Refusal> {
        admin(ctx)?;
        let lock = self.lock(ctx.tenant).await?;
        let out = self.install_locked(ctx, id, version, consent, upgrade).await;
        lock.release().await;
        self.enabled.remove(&ctx.tenant);
        out
    }

    async fn install_locked(&self, ctx: &Ctx, id: &str, version: Option<&str>, consent: &str, upgrade: bool) -> std::result::Result<Plan, Refusal> {
        let (plan, steps, ws) = self.plan_inner(ctx, id, version).await?;
        if upgrade && !ws.apps.contains_key(id) {
            return Err(Error::NotFound(format!("app {id} is not installed")).into());
        }
        if consent.trim() != plan.digest {
            return Err(Refusal::Stale(Box::new(plan)));
        }
        let mut releases: BTreeMap<String, Release> = BTreeMap::new();
        for s in steps.iter().filter(|s| s.action != StepAction::Already) {
            match &s.origin {
                Origin::Local(r) => {
                    releases.insert(s.id.clone(), (**r).clone());
                }
                Origin::Store { url, .. } => {
                    let (bundle, key) = s.staged.as_ref().ok_or_else(|| Error::internal("store bundle not staged"))?;
                    let r = self.publish(bundle, key, None, &format!("store:{url}"), user_of(ctx)).await?;
                    releases.insert(s.id.clone(), r);
                }
                Origin::Native => {}
            }
        }
        let mut done: Vec<Undo> = Vec::new();
        for s in steps.iter().filter(|s| s.action != StepAction::Already) {
            let mut undo = Undo {
                id: s.id.clone(),
                packages: Vec::new(),
                plugin: None,
                recorded: false,
                previous: ws.apps.get(&s.id).map(|i| (i.version.clone(), i.row)),
            };
            let outcome = self.apply_step(ctx, s, releases.get(&s.id), &plan.digest, &mut undo).await;
            done.push(undo);
            if let Err(e) = outcome {
                tracing::warn!(tenant = %ctx.tenant, app = %s.id, "app installation failed, undoing it: {e}");
                self.compensate(ctx, done).await;
                return Err(e.into());
            }
        }
        // What this installation brought in for itself is marked so; the app asked for is the
        // administrator's own choice, even if something brought it in before.
        for s in steps.iter().filter(|s| s.action == StepAction::Install && s.id != id && !ws.apps.contains_key(&s.id)) {
            self.kernel.set_brought_by(ctx, &s.id, Some(id)).await?;
        }
        if ws.apps.get(id).is_some_and(|i| i.brought_by.is_some()) {
            self.kernel.set_brought_by(ctx, id, None).await?;
        }
        tracing::info!(tenant = %ctx.tenant, app = %id, version = %plan.app.summary.version, consent = %plan.digest, "app installed");
        Ok(plan)
    }

    async fn apply_step(&self, ctx: &Ctx, s: &StepData, release: Option<&Release>, digest: &str, undo: &mut Undo) -> Result<()> {
        if !s.manifest.packages.is_empty() {
            let before: BTreeSet<String> = self.kernel.registry.schema(ctx.tenant).await?.packages.iter().cloned().collect();
            let closure = self.kernel.install_packages(ctx.tenant, &s.manifest.packages).await?;
            undo.packages = closure.into_iter().filter(|p| !before.contains(p)).collect();
        }
        let mut plugin_id = None;
        if s.manifest.runtime == Runtime::Wasm {
            let loaded;
            let bundle = match (&s.staged, release) {
                (Some((b, _)), _) => b,
                (None, Some(r)) => {
                    loaded = self.load_bundle(r).await?;
                    &loaded
                }
                (None, None) => return Err(Error::internal(format!("{} {}: no bundle holds its plugin", s.id, s.version))),
            };
            let (wasm, json) = bundle.plugin().map_err(app_err)?.ok_or_else(|| Error::internal(format!("{}: a wasm app without a plugin", s.id)))?;
            let mut np: NewPlugin = serde_json::from_value(json).map_err(|e| Error::Invalid(format!("{}: plugin.json: {e}", s.id)))?;
            np.name = s.id.clone();
            np.version = s.version.to_string();
            let existing = self.plugin_named(ctx.tenant, &s.id).await?;
            if existing.is_some() {
                // Upgrades keep the administrator's settings and secrets.
                np.config = None;
            }
            let fresh = self.fresh(ctx).await?;
            let pid = self.plugins.install(&self.kernel, &fresh, np, &wasm).await?;
            match existing {
                None => undo.plugin = Some(pid),
                Some((_, false)) => self.plugins.set_enabled(&self.kernel, &fresh, pid, true).await?,
                Some(_) => {}
            }
            plugin_id = Some(pid);
        }
        self.kernel.record_installation(ctx, &s.id, &s.version.to_string(), Some(digest), plugin_id).await?;
        undo.recorded = true;
        Ok(())
    }

    /// Best effort: takes back, newest first, what a failed installation did in the workspace.
    async fn compensate(&self, ctx: &Ctx, done: Vec<Undo>) {
        for u in done.into_iter().rev() {
            let outcome: Result<()> = async {
                if let Some(p) = u.plugin {
                    ignore_missing(self.plugins.remove(&self.kernel, ctx, p).await)?;
                }
                if !u.packages.is_empty() {
                    self.kernel.uninstall_packages(&self.fresh(ctx).await?, &u.packages, false).await?;
                }
                if u.recorded {
                    match &u.previous {
                        Some((version, true)) => self.kernel.record_installation(ctx, &u.id, version, None, None).await?,
                        _ => self.kernel.forget_installation(ctx, &u.id, false).await?,
                    }
                }
                Ok(())
            }
            .await;
            if let Err(e) = outcome {
                tracing::error!(tenant = %ctx.tenant, app = %u.id, "undoing a failed app installation: {e}");
            }
        }
    }

    /// Disabling stops the app's plugin and takes its packages out of the workspace (data stays);
    /// enabling puts them back. Refused while an enabled app needs it, or for a revoked release.
    pub async fn set_enabled(&self, ctx: &Ctx, id: &str, enabled: bool) -> Result<()> {
        admin(ctx)?;
        let lock = self.lock(ctx.tenant).await?;
        let out = self.set_enabled_locked(ctx, id, enabled).await;
        lock.release().await;
        self.enabled.remove(&ctx.tenant);
        out
    }

    async fn set_enabled_locked(&self, ctx: &Ctx, id: &str, enabled: bool) -> Result<()> {
        let ws = self.workspace(ctx.tenant).await?;
        let inst = ws.apps.get(id).ok_or_else(|| Error::NotFound(format!("app {id}")))?;
        if inst.enabled == enabled {
            return Ok(());
        }
        let release = if self.native(id).is_some() { None } else { self.release_of(ctx.tenant, id, &inst.version).await? };
        let manifest = match self.native(id) {
            Some(m) => Some(m.clone()),
            None => release.as_ref().and_then(recorded).map(|r| r.app),
        };
        let packages = manifest.as_ref().map(|m| m.packages.clone()).unwrap_or_default();
        if enabled {
            if let Some(r) = release.as_ref().filter(|r| r.status == "revoked") {
                return Err(Error::Forbidden(format!(
                    "{id} {} was revoked ({}); update it to a fixed release instead",
                    r.version,
                    r.status_reason.as_deref().unwrap_or("no reason given")
                )));
            }
            if let Some(dep) = manifest.iter().flat_map(|m| m.requires.keys()).find(|d| !ws.apps.get(*d).is_some_and(|i| i.enabled)) {
                return Err(Error::invalid(format!("{id} needs {dep}; install or enable {dep} first")));
            }
            if !packages.is_empty() {
                self.kernel.install_packages(ctx.tenant, &packages).await?;
            }
            if let Some(p) = inst.plugin_id {
                ignore_missing(self.plugins.set_enabled(&self.kernel, ctx, p, true).await)?;
            }
        } else {
            let users = self.dependents(ctx.tenant, &ws, id, true).await?;
            if !users.is_empty() {
                return Err(Error::invalid(format!("{} need {id}; disable or remove them first", users.join(", "))));
            }
            let installed: Vec<String> = packages.into_iter().filter(|p| ws.packages.contains_key(p)).collect();
            if !installed.is_empty() {
                // Disabling keeps who had which role, so enabling restores their access.
                self.kernel.disable_packages(&self.fresh(ctx).await?, &installed).await?;
            }
            if let Some(p) = inst.plugin_id {
                ignore_missing(self.plugins.set_enabled(&self.kernel, ctx, p, false).await)?;
            }
            if !inst.row {
                self.kernel.record_installation(ctx, id, &inst.version, None, None).await?;
            }
        }
        self.kernel.set_app_enabled(ctx, id, enabled).await
    }

    /// The apps `removed` brought in that nothing installed needs any more, in an order they can be
    /// removed in (those requiring others first): what removing a suite offers to remove too.
    pub async fn leftovers(&self, tenant: Uuid, removed: &str) -> Result<Vec<String>> {
        let ws = self.workspace(tenant).await?;
        let mut left: Vec<String> = ws.apps.iter().filter(|(_, i)| i.brought_by.as_deref() == Some(removed)).map(|(a, _)| a.clone()).collect();
        let mut order = Vec::new();
        loop {
            let mut next = None;
            for a in &left {
                // Required only by apps that are themselves on their way out (or by none).
                let users = self.dependents(tenant, &ws, a, false).await?;
                if users.iter().all(|u| order.contains(u)) {
                    next = Some(a.clone());
                    break;
                }
            }
            let Some(a) = next else { break };
            left.retain(|x| x != &a);
            order.push(a);
        }
        Ok(order)
    }

    /// Removes an app from the workspace: its packages (and with `delete_data`, when the app
    /// allows it — `uninstall: ask` — the workspace's rows in its tables), its plugin and its
    /// record. Refused while another installed app requires it. Returns whether data was deleted.
    pub async fn uninstall(&self, ctx: &Ctx, id: &str, delete_data: bool) -> Result<bool> {
        admin(ctx)?;
        let lock = self.lock(ctx.tenant).await?;
        let out = self.uninstall_locked(ctx, id, delete_data).await;
        lock.release().await;
        self.enabled.remove(&ctx.tenant);
        out
    }

    async fn uninstall_locked(&self, ctx: &Ctx, id: &str, delete_data: bool) -> Result<bool> {
        let ws = self.workspace(ctx.tenant).await?;
        let inst = ws.apps.get(id).ok_or_else(|| Error::NotFound(format!("app {id}")))?;
        let users = self.dependents(ctx.tenant, &ws, id, false).await?;
        if !users.is_empty() {
            return Err(Error::invalid(format!("{} need {id}; remove them first", users.join(", "))));
        }
        let manifest = self.manifest_of(ctx.tenant, id, &inst.version).await?;
        let catalog = self.kernel.registry.catalog();
        let all = manifest.as_ref().map(|m| m.packages.clone()).unwrap_or_default();
        // Rows are found through the cluster's definitions: a revoked release's data stays.
        let delete = delete_data && manifest.as_ref().is_some_and(|m| m.uninstall == Uninstall::Ask) && all.iter().all(|p| catalog.package(p).is_some());
        let packages: Vec<String> = all.into_iter().filter(|p| delete || ws.packages.contains_key(p)).collect();
        if delete && packages.iter().any(|p| !ws.schema.packages.contains(p)) {
            // A disabled app's packages are out of the schema: bring them back so its rows are found.
            self.kernel.install_packages(ctx.tenant, &packages).await?;
        }
        if !packages.is_empty() {
            self.kernel.uninstall_packages(&self.fresh(ctx).await?, &packages, delete).await?;
        }
        if let Some(p) = inst.plugin_id {
            ignore_missing(self.plugins.remove(&self.kernel, ctx, p).await)?;
        }
        self.kernel.forget_installation(ctx, id, delete).await?;
        tracing::info!(tenant = %ctx.tenant, app = %id, deleted_data = delete, "app removed");
        Ok(delete)
    }

    /// An uploaded bundle: verified against the trusted keys, then — by `apps.uploads` — made
    /// available to every workspace, or privately to this one (sandboxed code without tables).
    /// Returns the app and the plan to install it.
    pub async fn upload(&self, ctx: &Ctx, bytes: Bytes) -> Result<(AppSummary, Plan)> {
        admin(ctx)?;
        let private = match self.uploads {
            Uploads::Off => return Err(Error::Forbidden("uploading apps is turned off on this server (apps.uploads)".into())),
            Uploads::Private => Some(ctx.tenant),
            Uploads::Cluster => None,
        };
        let (b, _) = self.accept(bytes, private, self.allow_unsigned, user_of(ctx)).await?;
        tracing::info!(tenant = %ctx.tenant, app = %b.manifest.id, version = %b.manifest.version, private = private.is_some(), "app uploaded");
        let plan = self.plan(ctx, &b.manifest.id, Some(&b.manifest.version.to_string())).await?;
        Ok((plan.app.summary.clone(), plan))
    }

    // ───────────────────────── listings ─────────────────────────

    /// Native apps of this build, then releases available here (cluster and private), one per id.
    fn available(&self, local: &[Release]) -> Vec<AppSummary> {
        let mut out: Vec<AppSummary> = self.natives.iter().map(|m| summary(m, "native", true, false, None)).collect();
        for r in local {
            if !out.iter().any(|s| s.id == r.app_id)
                && let Some(s) = self.local_summary(r)
            {
                out.push(s);
            }
        }
        out
    }

    /// The newest compatible version above the installed one, if any.
    async fn update_for(&self, id: &str, inst: &Installed, ws: &Workspace, local: &[Release]) -> Option<String> {
        let current = Version::parse(&inst.version).ok()?;
        if let Some(m) = self.native(id) {
            return (m.version > current || self.stale(ws, m)).then(|| m.version.to_string());
        }
        let mut errors = Vec::new();
        let offers = self.offers_for(id, local, true, &mut errors).await;
        offers
            .into_iter()
            .map(|o| o.res)
            .filter(|r| r.version > current && r.version.pre.is_empty() && r.platform.matches(&self.platform))
            .map(|r| r.version)
            .max()
            .map(|v| v.to_string())
    }

    /// `GET /api/admin/apps`: installed apps with available updates, apps available here,
    /// the upload policy, the platform version and whether each store answers.
    pub async fn overview(&self, ctx: &Ctx) -> Result<Value> {
        admin(ctx)?;
        let ws = self.workspace(ctx.tenant).await?;
        let local = self.kernel.releases(Some(ctx.tenant)).await?;
        let updates = join_all(ws.apps.iter().map(|(id, inst)| self.update_for(id, inst, &ws, &local))).await;
        let mut installed = Vec::with_capacity(ws.apps.len());
        for ((id, inst), update) in ws.apps.iter().zip(updates) {
            let mut app = match self.native(id) {
                Some(m) => summary(m, "native", true, false, None),
                None => match self.release_of(ctx.tenant, id, &inst.version).await?.as_ref().and_then(|r| self.local_summary(r)) {
                    Some(s) => s,
                    None => AppSummary {
                        id: id.clone(),
                        name: id.clone(),
                        version: inst.version.clone(),
                        runtime: if inst.plugin_id.is_some() { "wasm" } else { "meta" }.into(),
                        summary: "This version is no longer available on this server.".into(),
                        categories: Vec::new(),
                        publisher: None,
                        verified: false,
                        icon_url: None,
                        source: "unknown".into(),
                        private: false,
                        parent: None,
                    },
                },
            };
            app.version = inst.version.clone();
            installed.push(InstalledApp { app, enabled: inst.enabled, installed_at: inst.installed_at, update: update.map(|v| json!({"version": v})) });
        }
        let stores = join_all(self.stores.iter().map(|url| async move {
            match self.store_search(url, "", "").await {
                Ok(_) => json!({"url": url, "ok": true, "error": null}),
                Err(e) => json!({"url": url, "ok": false, "error": e}),
            }
        }))
        .await;
        Ok(json!({
            "installed": installed,
            "available": self.available(&local),
            "uploads": self.uploads,
            "platform": PLATFORM_VERSION,
            "stores": stores,
        }))
    }

    /// `GET /api/admin/apps/store`: apps available here that match, then the stores' results
    /// (apps with an id already listed, and native apps this build lacks, left out).
    pub async fn search(&self, ctx: &Ctx, q: &str, category: &str) -> Result<Vec<AppSummary>> {
        admin(ctx)?;
        let (q, category) = (q.trim(), category.trim());
        if q.chars().count() > 200 || category.chars().count() > 40 {
            return Err(Error::invalid("search text or category too long"));
        }
        let local = self.available(&self.kernel.releases(Some(ctx.tenant)).await?);
        let known: BTreeSet<String> = local.iter().map(|s| s.id.clone()).collect();
        let needle = q.to_lowercase();
        let hit = |s: &AppSummary| {
            (needle.is_empty() || [&s.id, &s.name, &s.summary].iter().any(|t| t.to_lowercase().contains(&needle)))
                && (category.is_empty() || s.categories.iter().any(|c| c == category))
        };
        let mut out: Vec<AppSummary> = local.into_iter().filter(|s| hit(s)).collect();
        let found = join_all(self.stores.iter().map(|url| self.store_search(url, q, category))).await;
        for (url, result) in self.stores.iter().zip(found) {
            match result {
                Ok(apps) => {
                    for a in apps {
                        if a.runtime != "native" && !known.contains(&a.id) && !out.iter().any(|s| s.id == a.id) {
                            out.push(a.summary(url));
                        }
                    }
                }
                Err(e) => tracing::warn!(store = %url, "store search failed: {e}"),
            }
        }
        Ok(out)
    }

    // ───────────────────────── operators and revocations ─────────────────────────

    /// Makes a bundle file available on the cluster (operator task): signed by a trusted key
    /// unless `allow_unsigned`; the role rule and the platform check apply as for uploads.
    pub async fn make_available_file(&self, bytes: Bytes, allow_unsigned: bool) -> Result<Release> {
        Ok(self.accept(bytes, None, allow_unsigned, None).await?.1)
    }

    /// Makes the newest compatible store release of every store app on the cluster (or of
    /// `only`, which may be new to the cluster) available; returns one line per app.
    pub async fn upgrade_from_stores(&self, only: Option<&str>) -> Result<Vec<String>> {
        let local = self.kernel.releases(None).await?;
        let ids: Vec<String> = match only {
            Some(id) if valid_id(id) => vec![id.to_string()],
            Some(id) => return Err(Error::NotFound(format!("app {id}"))),
            None => local.iter().filter(|r| r.source.starts_with("store:")).map(|r| r.app_id.clone()).collect(),
        };
        let mut lines = Vec::new();
        for id in ids {
            if self.native(&id).is_some() {
                lines.push(format!("{id}: built into this server; it upgrades with the server image"));
                continue;
            }
            let current = local.iter().find(|r| r.app_id == id).and_then(|r| Version::parse(&r.version).ok());
            let mut best: Option<(String, store::Release)> = None;
            for url in &self.stores {
                match self.store_releases(url, &id).await {
                    Ok(list) => {
                        for rel in list {
                            let newer = current.as_ref().is_none_or(|c| rel.version > *c) && best.as_ref().is_none_or(|(_, b)| rel.version > b.version);
                            if newer && rel.version.pre.is_empty() && rel.platform.matches(&self.platform) {
                                best = Some((url.clone(), rel));
                            }
                        }
                    }
                    Err(e) => lines.push(format!("{id}: {url}: {e}")),
                }
            }
            let Some((url, rel)) = best else {
                lines.push(format!("{id}: up to date ({})", current.map(|c| c.to_string()).unwrap_or_else(|| "not in any store".into())));
                continue;
            };
            let outcome = async {
                let (b, key) = self.stage(&id, &url, &rel).await?;
                self.publish(&b, &key, None, &format!("store:{url}"), None).await
            }
            .await;
            match outcome {
                Ok(r) => lines.push(format!("{id}: {} available from {url}", r.version)),
                Err(e) => lines.push(format!("{id}: {} from {url} failed: {e}", rel.version)),
            }
        }
        Ok(lines)
    }

    /// Every release recorded on the cluster (operator task), newest first per app.
    pub async fn release_lines(&self, only: Option<&str>) -> Result<Vec<String>> {
        let mut tx = self.kernel.db.system_tx().await?;
        let rows: Vec<Release> = sqlx::query_as(
            "select id, app_id, version, runtime, manifest, digest, signatures, source, bundle_key, packages, plugin_sha256, status, status_reason, \
             private_tenant, created_at from app_release where ($1::text is null or app_id = $1) order by app_id, created_at desc",
        )
        .bind(only)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .iter()
            .map(|r| {
                let private = r.private_tenant.map(|t| format!(" private:{t}")).unwrap_or_default();
                let reason = r.status_reason.as_deref().map(|s| format!(" ({s})")).unwrap_or_default();
                format!("{} {} {} {} {} {}{private}{reason}", r.app_id, r.version, r.status, r.runtime, r.source, r.created_at.format("%Y-%m-%d %H:%M"))
            })
            .collect())
    }

    /// Takes a release (or, without `version`, every release of the app) out of service and
    /// disables the app — plugin and record — in every workspace running it; data is kept.
    /// Returns how many releases it revoked now.
    pub async fn revoke(&self, id: &str, version: Option<&str>, reason: &str) -> Result<usize> {
        let versions: Vec<String> = match version {
            Some(v) => vec![v.to_string()],
            None => self.kernel.release_history(id).await?.into_iter().map(|r| r.version).collect(),
        };
        let mut revoked = 0;
        for v in versions {
            if self.kernel.revoke_release(id, &v, reason).await? {
                revoked += 1;
            }
            // Also for releases revoked before: a replica may have stopped half-way.
            self.disable_everywhere(id, &v).await?;
        }
        Ok(revoked)
    }

    async fn disable_everywhere(&self, id: &str, version: &str) -> Result<()> {
        let mut tx = self.kernel.db.system_tx().await?;
        let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as("select tenant_id, plugin_id from tenant_app where app_id = $1 and version = $2 and enabled")
            .bind(id)
            .bind(version)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        for (tenant, plugin) in rows {
            let outcome: Result<()> = async {
                let ctx = Ctx::system(tenant, self.kernel.registry.schema(tenant).await?);
                if let Some(p) = plugin {
                    ignore_missing(self.plugins.set_enabled(&self.kernel, &ctx, p, false).await)?;
                }
                self.kernel.set_app_enabled(&ctx, id, false).await
            }
            .await;
            match outcome {
                Ok(()) => tracing::warn!(%tenant, app = %id, %version, "revoked app disabled"),
                Err(e) => tracing::error!(%tenant, app = %id, %version, "could not disable a revoked app: {e}"),
            }
        }
        Ok(())
    }

    /// Fetches every store's signed revocation list and applies it; returns how many releases
    /// were revoked. A list that does not verify is ignored (and logged), and so is one generated
    /// before the newest list already applied from that store (stale caches, replays).
    pub async fn sync_revocations(&self) -> Result<usize> {
        let mut revoked = 0;
        for url in &self.stores {
            let list = match self.fetch(&format!("{url}/v1/revocations"), MAX_JSON, JSON_TIMEOUT).await {
                Ok(body) => {
                    serde_json::from_slice::<Value>(&body).map_err(|e| format!("not JSON: {e}")).and_then(|v| store::revocation_list(&v, &self.trusted))
                }
                Err(e) => Err(e),
            };
            match list {
                Ok(list) => {
                    match list.generated_at {
                        Some(at) => {
                            if let Some(newest) = self.revocations_seen(url).await?
                                && at < newest
                            {
                                tracing::warn!(store = %url, "revocation list generated at {at} is older than the one applied ({newest}); ignored");
                                continue;
                            }
                            if chrono::Utc::now() - at > chrono::Duration::days(1) {
                                tracing::warn!(store = %url, "the store's newest revocation list was generated at {at}");
                            }
                        }
                        None => tracing::warn!(store = %url, "revocation list without generated_at: older lists cannot be told apart"),
                    }
                    // Native apps' code ships with the server image, not from a store.
                    for r in list.entries.into_iter().filter(|r| self.native(&r.app).is_none()) {
                        revoked += self.revoke(&r.app, r.version.as_deref(), &format!("{} (store {url})", r.reason)).await?;
                    }
                    if let Some(at) = list.generated_at {
                        self.record_revocations_seen(url, at).await?;
                    }
                }
                Err(e) => tracing::warn!(store = %url, "revocation list not applied: {e}"),
            }
        }
        Ok(revoked)
    }

    /// When the newest revocation list applied from `store` was generated.
    async fn revocations_seen(&self, store: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let mut tx = self.kernel.db.system_tx().await?;
        let at = sqlx::query_scalar("select revocations_at from app_store_state where store = $1").bind(store).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(at)
    }

    async fn record_revocations_seen(&self, store: &str, at: chrono::DateTime<chrono::Utc>) -> Result<()> {
        let mut tx = self.kernel.db.system_tx().await?;
        sqlx::query(
            "insert into app_store_state (store, revocations_at) values ($1, $2) on conflict (store) do update \
             set revocations_at = greatest(app_store_state.revocations_at, excluded.revocations_at), updated_at = now()",
        )
        .bind(store)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn revocation_round(&self) -> Result<usize> {
        let mut conn = self.kernel.db.pool.acquire().await?;
        let got: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(REVOCATIONS_LOCK).fetch_one(&mut *conn).await?;
        if !got {
            return Ok(0);
        }
        let out = self.sync_revocations().await;
        let _ = sqlx::query("select pg_advisory_unlock($1)").bind(REVOCATIONS_LOCK).execute(&mut *conn).await;
        out
    }

    /// Polls the stores' revocation lists every ten minutes (one replica at a time).
    pub fn spawn_revocations(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if self.stores.is_empty() {
                return;
            }
            loop {
                match self.revocation_round().await {
                    Ok(0) => {}
                    Ok(n) => tracing::warn!(revoked = n, "store revocations applied"),
                    Err(e) => tracing::warn!("store revocations: {e}"),
                }
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(REVOCATIONS_EVERY) => {}
                }
            }
        })
    }
}

/// The store API as this server reads it (docs/api.md, "App stores"). Only an app's `id` and a
/// release's `version` are required; everything else is optional and a few alternative names
/// are accepted, so stores keep working while their API settles.
pub mod store {
    use super::{AppSummary, ed25519, valid_id};
    use ferrum_apps::{Role, TrustedKey};
    use semver::{Version, VersionReq};
    use serde_json::Value;
    use std::collections::BTreeMap;

    /// An app in a store's search results or details.
    #[derive(Debug, Clone, PartialEq)]
    pub struct App {
        pub id: String,
        pub name: String,
        pub version: String,
        pub runtime: String,
        pub summary: String,
        pub categories: Vec<String>,
        pub publisher: Option<String>,
        pub verified: bool,
        pub icon_url: Option<String>,
    }

    impl App {
        pub fn summary(&self, store: &str) -> AppSummary {
            AppSummary {
                id: self.id.clone(),
                name: self.name.clone(),
                version: self.version.clone(),
                runtime: self.runtime.clone(),
                summary: self.summary.clone(),
                categories: self.categories.clone(),
                publisher: self.publisher.clone(),
                verified: self.verified,
                icon_url: self.icon_url.clone(),
                source: format!("store:{store}"),
                private: false,
                parent: None,
            }
        }
    }

    /// One installable version in a store's release listing.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Release {
        pub version: Version,
        pub requires: BTreeMap<String, VersionReq>,
        pub platform: VersionReq,
        /// The bundle's content digest, when the store lists it (checked after download).
        pub digest: Option<String>,
        /// Where to download the bundle, when not at the standard path.
        pub bundle: Option<String>,
    }

    impl Release {
        pub fn bundle_url(&self, store: &str, id: &str) -> String {
            self.bundle.as_deref().and_then(|b| url(store, b)).unwrap_or_else(|| format!("{store}/v1/apps/{id}/releases/{}/bundle", self.version))
        }
    }

    /// An entry of a revocation list; no version means every version of the app.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Revocation {
        pub app: String,
        pub version: Option<String>,
        pub reason: String,
    }

    fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
        path.split('.').try_fold(v, |v, k| v.get(k))
    }

    /// The trimmed, non-empty text at the first of `paths` (dotted for nested objects) holding one.
    fn text<'a>(v: &'a Value, paths: &[&str]) -> Option<&'a str> {
        paths.iter().find_map(|p| at(v, p)?.as_str().map(str::trim).filter(|s| !s.is_empty()))
    }

    /// The array `v` is, or holds under the first of `keys` that is one.
    fn items<'a>(v: &'a Value, keys: &[&str]) -> &'a [Value] {
        v.as_array().or_else(|| keys.iter().find_map(|k| v.get(*k)?.as_array())).map(Vec::as_slice).unwrap_or_default()
    }

    /// `path` resolved against the store's base URL; http(s) only.
    pub fn url(store: &str, path: &str) -> Option<String> {
        let u = reqwest::Url::parse(&format!("{store}/")).ok()?.join(path).ok()?;
        matches!(u.scheme(), "http" | "https").then(|| u.to_string())
    }

    /// Search results: `[...]` or `{"apps" | "items" | "results" | "data": [...]}`.
    pub fn apps(v: &Value, store: &str) -> Vec<App> {
        items(v, &["apps", "items", "results", "data"]).iter().filter_map(|a| app(a, store)).collect()
    }

    /// One app (possibly wrapped as `{"app": {...}}`).
    pub fn app(v: &Value, store: &str) -> Option<App> {
        let v = v.get("app").filter(|a| a.is_object()).unwrap_or(v);
        let id = text(v, &["id", "app_id"]).filter(|id| valid_id(id))?;
        let runtime = text(v, &["runtime", "latest.runtime", "latest_release.runtime"]).filter(|r| matches!(*r, "native" | "wasm" | "meta")).unwrap_or("wasm");
        let flag = |paths: &[&str]| paths.iter().any(|p| at(v, p).and_then(Value::as_bool) == Some(true));
        Some(App {
            id: id.into(),
            name: text(v, &["name", "title"]).unwrap_or(id).into(),
            version: text(v, &["version", "latest_version", "latest.version", "latest_release.version"]).unwrap_or_default().into(),
            runtime: runtime.into(),
            summary: text(v, &["summary", "description"]).and_then(|s| s.lines().next()).map(|s| s.chars().take(200).collect()).unwrap_or_default(),
            categories: v
                .get("categories")
                .and_then(Value::as_array)
                .map(|c| c.iter().filter_map(Value::as_str).take(5).map(String::from).collect())
                .unwrap_or_default(),
            publisher: text(v, &["publisher", "publisher.id", "publisher.name", "publisher_id"]).map(String::from),
            verified: flag(&["verified", "publisher_verified", "publisher.verified"]),
            icon_url: text(v, &["icon_url", "icon"]).and_then(|i| url(store, i)),
        })
    }

    /// A release listing: `[...]` or `{"releases" | "versions" | "items" | "data": [...]}`.
    /// Withdrawn releases (yanked, revoked, rejected, under review) and unreadable ones are skipped.
    pub fn releases(v: &Value) -> Vec<Release> {
        items(v, &["releases", "versions", "items", "data"]).iter().filter_map(release).collect()
    }

    fn release(v: &Value) -> Option<Release> {
        let withdrawn = v.get("yanked").and_then(Value::as_bool) == Some(true)
            || matches!(text(v, &["status"]), Some("yanked" | "revoked" | "rejected" | "pending" | "in_review" | "review"));
        if withdrawn {
            return None;
        }
        let version = Version::parse(text(v, &["version"])?).ok()?;
        let mut requires = BTreeMap::new();
        match v.get("requires").or_else(|| at(v, "manifest.requires")) {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => {
                for (id, req) in m {
                    if !valid_id(id) {
                        return None;
                    }
                    requires.insert(id.clone(), VersionReq::parse(req.as_str()?).ok()?);
                }
            }
            Some(_) => return None,
        }
        let platform = match text(v, &["platform", "manifest.platform"]) {
            Some(p) => VersionReq::parse(p).ok()?,
            None => VersionReq::STAR,
        };
        let digest = text(v, &["digest", "sha256"]).filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_ascii_lowercase);
        Some(Release { version, requires, platform, digest, bundle: text(v, &["bundle_url", "download_url", "bundle"]).map(String::from) })
    }

    /// A store's signed revocation list: `{"payload": "<JSON text>", "signatures": [{"key_id",
    /// "sig"}]}`, each `sig` the base64 Ed25519 signature of the payload's UTF-8 bytes by a store
    /// key; the payload is `{"revocations": [{"app", "version"?, "reason"?}]}`. At least one
    /// trusted store key must have signed it, and every signature by one must verify.
    pub fn revocations(v: &Value, trusted: &[TrustedKey]) -> Result<Vec<Revocation>, String> {
        revocation_list(v, trusted).map(|l| l.entries)
    }

    /// A verified revocation list and when the store generated it (`generated_at`, RFC 3339).
    #[derive(Debug, Clone, PartialEq)]
    pub struct RevocationList {
        pub generated_at: Option<chrono::DateTime<chrono::Utc>>,
        pub entries: Vec<Revocation>,
    }

    /// [`revocations`] with the payload's generation time, so older lists can be refused.
    pub fn revocation_list(v: &Value, trusted: &[TrustedKey]) -> Result<RevocationList, String> {
        let payload = v.get("payload").and_then(Value::as_str).ok_or("the list has no payload")?;
        let mut signed = false;
        for s in v.get("signatures").and_then(Value::as_array).into_iter().flatten() {
            let (Some(key_id), Some(sig)) = (s.get("key_id").and_then(Value::as_str), s.get("sig").and_then(Value::as_str)) else { continue };
            for key in trusted.iter().filter(|k| k.role == Role::Store && k.key_id == key_id) {
                if !ed25519(&key.public_key, payload.as_bytes(), sig) {
                    return Err(format!("the signature by trusted store key {key_id} does not match the list"));
                }
                signed = true;
            }
        }
        if !signed {
            return Err("no trusted store key signed the list".into());
        }
        let list: Value = serde_json::from_str(payload).map_err(|e| format!("the signed payload is not JSON: {e}"))?;
        let generated_at = match text(&list, &["generated_at"]) {
            Some(t) => Some(chrono::DateTime::parse_from_rfc3339(t).map_err(|e| format!("generated_at is not RFC 3339: {e}"))?.to_utc()),
            None => None,
        };
        let entries = items(&list, &["revocations", "items"])
            .iter()
            .filter_map(|r| {
                Some(Revocation {
                    app: text(r, &["app", "app_id", "id"]).filter(|a| valid_id(a))?.into(),
                    version: text(r, &["version"]).filter(|v| *v != "*").map(String::from),
                    reason: text(r, &["reason"]).unwrap_or("revoked by the store").into(),
                })
            })
            .collect();
        Ok(RevocationList { generated_at, entries })
    }
}
