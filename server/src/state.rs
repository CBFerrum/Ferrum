//! Process-wide state shared by handlers (stored in Loco's `SharedStore`).

use crate::error::ApiError;
use dashmap::DashMap;
use ferrum_kernel::{Error, Kernel};
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

/// Fixed-window limiter keyed by e.g. client IP. Bounded so a flood of keys cannot exhaust memory.
pub struct RateLimiter {
    window: Duration,
    max: u32,
    hits: DashMap<String, (Instant, u32)>,
}

impl RateLimiter {
    pub fn new(window: Duration, max: u32) -> Self {
        RateLimiter { window, max, hits: DashMap::new() }
    }

    pub fn check(&self, key: &str) -> Result<(), Error> {
        if self.hits.len() > 100_000 {
            let window = self.window;
            self.hits.retain(|_, (t, _)| t.elapsed() < window);
        }
        let mut e = self.hits.entry(key.to_string()).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= self.window {
            *e = (Instant::now(), 0);
        }
        e.1 += 1;
        if e.1 > self.max {
            return Err(Error::TooManyRequests("too many requests; slow down".into()));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub kernel: Arc<Kernel>,
    pub plugins: Arc<ferrum_plugins::PluginHost>,
    pub live: Arc<crate::live::LiveHub>,
    pub settings: Settings,
    pub shutdown: Arc<watch::Sender<bool>>,
    pub background: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    pub jwt_secret: String,
    pub jwt_expiration: u64,
    pub login_limiter: Arc<RateLimiter>,
    pub apps: Arc<crate::appmgr::AppManager>,
    pub objects: Arc<crate::objects::Objects>,
}

pub fn app_state(ctx: &AppContext) -> Result<AppState, ApiError> {
    ctx.shared_store.get::<AppState>().ok_or_else(|| ApiError(Error::Unavailable("server is still starting".into())))
}
