//! Process-wide state shared by handlers (stored in Loco's `SharedStore`).

use crate::error::ApiError;
use dashmap::DashMap;
use ferrum_kernel::{Db, Error, Kernel};
use loco_rs::app::AppContext;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

fn default_packages_dir() -> String {
    "packages".into()
}
fn default_db_max() -> u32 {
    20
}
fn default_upload_max() -> usize {
    25 * 1024 * 1024
}
fn default_direct_upload_max() -> u64 {
    2 * 1024 * 1024 * 1024
}
fn default_true() -> bool {
    true
}

fn default_user_requests() -> u32 {
    3000
}
fn default_address_requests() -> u32 {
    6000
}
fn default_proxy_hops() -> usize {
    1
}
fn default_files_dir() -> String {
    "storage".into()
}

/// The `settings:` section of `config/<env>.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(default = "default_packages_dir")]
    pub packages_dir: String,
    #[serde(default = "default_db_max")]
    pub db_max_connections: u32,
    /// Required (as `X-Ferrum-Bootstrap-Token`) to create tenants over HTTP. Empty disables the endpoint.
    #[serde(default)]
    pub bootstrap_token: String,
    #[serde(default)]
    pub webhooks_allow_private: bool,
    /// Single sign-on providers may sit on private addresses and speak plain http (an identity
    /// provider inside the operator's network).
    #[serde(default)]
    pub sso_allow_private: bool,
    /// Requests a user (or one of their API keys) may make per minute on each replica; 0: no limit.
    #[serde(default = "default_user_requests")]
    pub user_requests_per_minute: u32,
    /// Requests one client address may make per minute on each replica, counted for requests that
    /// came through the proxy (`X-Forwarded-For`); 0: no limit.
    #[serde(default = "default_address_requests")]
    pub address_requests_per_minute: u32,
    /// Proxies in front that append to `X-Forwarded-For` (the gateway: 1; a CDN or load balancer
    /// before it: 2): whose entry is the client's address for rate limits.
    #[serde(default = "default_proxy_hops")]
    pub proxy_hops: usize,
    /// Run outbox/webhook/maintenance loops in this process.
    #[serde(default = "default_true")]
    pub run_background: bool,
    #[serde(default = "default_upload_max")]
    pub upload_max_bytes: usize,
    /// Largest file the browser may upload straight to S3 (`POST /api/files/uploads`). Completing
    /// one reads it back once to record its hash.
    #[serde(default = "default_direct_upload_max")]
    pub direct_upload_max_bytes: u64,
    /// Local directory for uploaded files when no S3 bucket is configured.
    #[serde(default = "default_files_dir")]
    pub files_dir: String,
    #[serde(default)]
    pub s3_bucket: String,
    #[serde(default)]
    pub s3_region: String,
    /// Endpoint of an S3-compatible store (MinIO, Cloudflare R2, Wasabi, DigitalOcean Spaces,
    /// GCS interoperability...); empty means AWS S3. Credentials come from `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY` and optionally `AWS_SESSION_TOKEN`.
    #[serde(default)]
    pub s3_endpoint: String,
    /// Refuse to start without object storage, so replicas never keep attachments on local disk.
    #[serde(default)]
    pub require_object_storage: bool,
    /// Base64 of 32 random bytes sealing secret fields; required in production.
    #[serde(default)]
    pub secret_key: String,
    /// When set, `/metrics` requires `Authorization: Bearer <token>`.
    #[serde(default)]
    pub metrics_token: String,
    /// Base URL of a Gotenberg-compatible service that lays print formats out as PDFs.
    #[serde(default)]
    pub pdf_url: String,
    #[serde(default)]
    pub apps: AppsSettings,
    /// Shape A of docs/plan/apps-ui.md: the slug of the one workspace every host of this server
    /// serves (a single-workspace install). Empty: a host leads to the workspace it is registered
    /// for, and on other hosts the workspace is chosen at sign-in (shape A′).
    #[serde(default)]
    pub default_workspace: String,
    /// Interfaces of compiled-in apps: a directory holding one directory of built files per app
    /// id (`{ui_dir}/{app}/index.html`), registered as interface releases at boot.
    #[serde(default)]
    pub ui_dir: String,
    /// How long a browser stays signed in (days).
    #[serde(default = "default_session_days")]
    pub session_days: u32,
    /// Where the web app's image offers the shell's and its apps' built interfaces
    /// (`http://web:8080/_releases/`); fetched at start-up and every ten minutes.
    #[serde(default)]
    pub ui_source: String,
    /// The domain isolated apps get origins of their own under, `{app}--{workspace}.{apps_base}`
    /// (docs/plan/apps-ui.md §7.1, shape D): a site apart from the workspaces' hosts (another
    /// registrable domain, or one in the Public Suffix List), its wildcard DNS name and
    /// certificate pointing here. Empty: isolated apps run in sandboxed frames instead.
    #[serde(default)]
    pub apps_base: String,
    /// Shapes B and C (docs/plan/apps-ui.md §7.1): the cluster's base domain. Each workspace is at
    /// `{slug}.{base}`, people sign in once at `accounts.{base}`, and outside callers use
    /// `hooks.{base}`; a host the cluster does not know leads nowhere. Empty: shapes A and A′.
    #[serde(default)]
    pub base_domain: String,
    /// This cluster's region (`eu`, `in`…), as the directory names it.
    #[serde(default)]
    pub region: String,
    /// The other regions' accounts hosts, `name=https://accounts.example.eu`, comma-separated:
    /// the sign-in page offers them, and a workspace not found here is looked for there.
    #[serde(default)]
    pub regions: String,
}

fn default_session_days() -> u32 {
    14
}

/// Who may upload app bundles (`settings.apps.uploads`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Uploads {
    /// Uploads are refused.
    #[default]
    Off,
    /// Sandboxed apps without tables, visible only in the uploading workspace.
    Private,
    /// Any app; it becomes available to every workspace of the cluster.
    Cluster,
}

/// `settings.apps`: app stores, trusted signing keys and uploads (docs/operations.md).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AppsSettings {
    /// Store base URLs, comma-separated; empty means no store.
    pub registries: String,
    /// Trusted keys, comma-separated `store:<base64>` / `publisher:<base64>`.
    pub trusted_keys: String,
    pub uploads: Uploads,
    /// Accept uploads that no trusted key signed (development).
    pub allow_unsigned_uploads: bool,
}

impl Settings {
    pub fn from_ctx(ctx: &AppContext) -> loco_rs::Result<Settings> {
        let raw = ctx.config.settings.clone().unwrap_or_else(|| serde_json::json!({}));
        serde_json::from_value(raw).map_err(|e| loco_rs::Error::Message(format!("invalid settings: {e}")))
    }
}

/// Fixed-window limiter keyed by e.g. client IP, its count shared by every replica on the
/// database (`rate_window`). The replica's own count, bounded so a flood of keys cannot exhaust
/// memory, turns a flood away before it reaches the database.
pub struct RateLimiter {
    name: &'static str,
    window: Duration,
    max: u32,
    hits: DashMap<String, (Instant, u32)>,
    /// Where the count is shared; without it the limit is the replica's own.
    db: Option<Db>,
}

impl RateLimiter {
    /// `name` keeps this limiter's counts apart from other limiters'.
    pub fn shared(name: &'static str, window: Duration, max: u32, db: Db) -> Self {
        RateLimiter { name, window, max, hits: DashMap::new(), db: Some(db) }
    }

    /// Counted on this replica only: cheap enough for every request.
    pub fn local_only(name: &'static str, window: Duration, max: u32) -> Self {
        RateLimiter { name, window, max, hits: DashMap::new(), db: None }
    }

    /// The replica's own count (`local_only` limiters).
    pub fn check_local(&self, key: &str) -> Result<(), Error> {
        if self.max > 0 && self.local(key) > self.max {
            return Err(Error::TooManyRequests(format!("more than {} requests a minute; slow down", self.max)));
        }
        Ok(())
    }

    pub async fn check(&self, key: &str) -> Result<(), Error> {
        let refused = || Err(Error::TooManyRequests("too many requests; slow down".into()));
        if self.local(key) > self.max {
            return refused();
        }
        let Some(db) = &self.db else { return Ok(()) };
        match db.count_hit(&format!("{}|{key}", self.name), self.window).await {
            Ok(n) if n > i64::from(self.max) => refused(),
            Ok(_) => Ok(()),
            // The replica's own count still applies; a database hiccup must not lock everyone out.
            Err(e) => {
                tracing::warn!("shared rate limit unavailable: {e}");
                Ok(())
            }
        }
    }

    fn local(&self, key: &str) -> u32 {
        if self.hits.len() > 100_000 {
            let window = self.window;
            self.hits.retain(|_, (t, _)| t.elapsed() < window);
        }
        let mut e = self.hits.entry(key.to_string()).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= self.window {
            *e = (Instant::now(), 0);
        }
        e.1 += 1;
        e.1
    }
}

#[derive(Clone)]
pub struct AppState {
    pub kernel: Arc<Kernel>,
    pub plugins: Arc<ferrum_plugins::PluginHost>,
    pub live: Arc<crate::live::LiveHub>,
    /// Documents open for live editing on this replica.
    pub docs: Arc<crate::coedit::Docs>,
    pub settings: Settings,
    pub shutdown: Arc<watch::Sender<bool>>,
    pub background: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    pub jwt_secret: String,
    pub jwt_expiration: u64,
    pub login_limiter: Arc<RateLimiter>,
    /// Documents a user may email per minute.
    pub mail_limiter: Arc<RateLimiter>,
    /// Requests per minute a client may make to a workspace's public pages and endpoints.
    pub public_limiter: Arc<RateLimiter>,
    /// Requests per minute a user may make (on this replica), whatever the token.
    pub user_limiter: Arc<RateLimiter>,
    pub address_limiter: Arc<RateLimiter>,
    pub apps: Arc<crate::appmgr::AppManager>,
    pub objects: Arc<crate::objects::Objects>,
    /// App pages, interface releases and browser sessions (docs/plan/apps-ui.md).
    pub shell: Arc<crate::shell::Shell>,
    /// The assistant's turns under way on this replica, and the operator's settings for it.
    pub assistant: Arc<crate::assistant::Assistant>,
}

pub fn app_state(ctx: &AppContext) -> Result<AppState, ApiError> {
    ctx.shared_store.get::<AppState>().ok_or_else(|| ApiError(Error::Unavailable("server is still starting".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_users_budget_is_counted_per_window() {
        let l = RateLimiter::local_only("user", Duration::from_secs(60), 3);
        for _ in 0..3 {
            assert!(l.check_local("t|alice").is_ok());
        }
        assert!(matches!(l.check_local("t|alice"), Err(Error::TooManyRequests(_))));
        assert!(l.check_local("t|bob").is_ok(), "per user");
        let off = RateLimiter::local_only("user", Duration::from_secs(60), 0);
        assert!((0..10_000).all(|_| off.check_local("t|alice").is_ok()), "0 turns it off");
        let short = RateLimiter::local_only("user", Duration::from_millis(30), 1);
        assert!(short.check_local("k").is_ok() && short.check_local("k").is_err());
        std::thread::sleep(Duration::from_millis(40));
        assert!(short.check_local("k").is_ok(), "a new window");
    }
}
