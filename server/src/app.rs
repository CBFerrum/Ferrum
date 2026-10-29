//! Loco application wiring: boots the kernel (migrations + metadata DDL), registers domain
//! hooks, mounts routes, and runs background loops with graceful shutdown.

use crate::controllers;
use crate::mailer::LocoMailer;
use crate::state::{AppState, RateLimiter, Settings};
use async_trait::async_trait;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings, Mailer};
use loco_rs::app::{AppContext, Hooks, Initializer};
use loco_rs::bgworker::Queue;
use loco_rs::boot::{BootResult, StartMode, create_app};
use loco_rs::config::Config;
use loco_rs::controller::AppRoutes;
use loco_rs::environment::Environment;
use loco_rs::storage::{self, Storage};
use loco_rs::task::Tasks;
use loco_rs::{Error, Result};
use migration::Migrator;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub struct App;

fn build_storage(settings: &Settings) -> Result<Storage> {
    if !settings.s3_bucket.is_empty() {
        // OpenDAL talks HTTP through a process-wide transport that must be installed first.
        opendal::install_default();
        let region = if settings.s3_region.is_empty() { "auto" } else { settings.s3_region.as_str() };
        let driver = if settings.s3_endpoint.is_empty() {
            storage::drivers::aws::new(&settings.s3_bucket, region)
        } else {
            let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
            let credential = storage::drivers::aws::Credential {
                key_id: env("AWS_ACCESS_KEY_ID").ok_or_else(|| Error::Message("AWS_ACCESS_KEY_ID is required with FERRUM_S3_ENDPOINT".into()))?,
                secret_key: env("AWS_SECRET_ACCESS_KEY").ok_or_else(|| Error::Message("AWS_SECRET_ACCESS_KEY is required with FERRUM_S3_ENDPOINT".into()))?,
                token: env("AWS_SESSION_TOKEN"),
            };
            storage::drivers::aws::with_credentials_and_endpoint(&settings.s3_bucket, region, &settings.s3_endpoint, credential)
        }
        .map_err(|e| Error::Message(format!("object storage: {e}")))?;
        return Ok(Storage::single(driver));
    }
    if settings.require_object_storage {
        return Err(Error::Message(
            "object storage is required so every replica sees the same attachments: set FERRUM_S3_BUCKET (and FERRUM_S3_ENDPOINT for \
             S3-compatible stores), or FERRUM_REQUIRE_OBJECT_STORAGE=false for a single-server install"
                .into(),
        ));
    }
    std::fs::create_dir_all(&settings.files_dir).map_err(|e| Error::Message(format!("files_dir: {e}")))?;
    Ok(Storage::single(storage::drivers::local::new_with_prefix(&settings.files_dir).map_err(|e| Error::Message(e.to_string()))?))
}

/// Writes and deletes a probe object so a wrong bucket or credentials show up at boot, not at
/// the first upload. Logged, not fatal: a store outage must not keep the API from starting.
async fn probe_storage(storage: &Storage) {
    let path = std::path::PathBuf::from(format!(".ferrum-probe/{}", uuid::Uuid::now_v7()));
    let outcome = async {
        storage.upload(&path, &bytes::Bytes::from_static(b"ok")).await?;
        storage.delete(&path).await
    }
    .await;
    match outcome {
        Ok(()) => tracing::info!("object storage reachable"),
        Err(e) => tracing::error!("object storage probe failed (uploads will fail until it is fixed): {e}"),
    }
}

#[async_trait]
impl Hooks for App {
    fn app_name() -> &'static str {
        "ferrum"
    }

    fn app_version() -> String {
        format!("{} ({})", env!("CARGO_PKG_VERSION"), option_env!("BUILD_SHA").or(option_env!("GITHUB_SHA")).unwrap_or("dev"))
    }

    async fn boot(mode: StartMode, environment: &Environment, config: Config) -> Result<BootResult> {
        create_app::<Self, Migrator>(mode, environment, config).await
    }

    async fn initializers(_ctx: &AppContext) -> Result<Vec<Box<dyn Initializer>>> {
        Ok(vec![])
    }

    async fn after_context(ctx: AppContext) -> Result<AppContext> {
        let settings = Settings::from_ctx(&ctx)?;
        let db = Db::connect(DbConfig { url: ctx.config.database.uri.clone(), max_connections: settings.db_max_connections, ..Default::default() })
            .await
            .map_err(|e| Error::Message(format!("database: {e}")))?;
        let catalog = crate::packages::base_catalog(Path::new(&settings.packages_dir)).map_err(|e| Error::Message(format!("packages: {e}")))?;
        let missing = crate::packages::missing_logic(&catalog);
        if !missing.is_empty() {
            return Err(Error::Message(format!(
                "packages {} are in {} but this build does not include their logic; build with those features or remove them",
                missing.join(", "),
                settings.packages_dir
            )));
        }
        let mut hooks = HookRegistry::default();
        ferrum_domain::register(&mut hooks);
        crate::packages::register(&mut hooks);
        let plugins = ferrum_plugins::PluginHost::new(settings.webhooks_allow_private).map_err(|e| Error::Message(format!("plugins: {e}")))?;
        hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins.clone())));
        hooks.add_event_sink(Arc::new(ferrum_plugins::PluginEvents(plugins.clone())));
        hooks.add_event_sink(Arc::new(crate::live::LiveEvents));
        let smtp = ctx
            .config
            .mailer
            .as_ref()
            .and_then(|m| m.smtp.as_ref())
            .filter(|s| s.enable)
            .map(crate::mailer::smtp_transport)
            .transpose()
            .map_err(Error::Message)?;
        let mailer: Option<Arc<dyn Mailer>> = ctx.mailer.clone().map(|sender| Arc::new(LocoMailer { sender, from: None, smtp }) as Arc<dyn Mailer>);
        let jwt = ctx.config.auth.as_ref().and_then(|a| a.jwt.as_ref()).ok_or_else(|| Error::Message("auth.jwt is not configured".into()))?;
        let secret_key = secret_key(&settings, &jwt.secret, &ctx.environment)?;
        let kernel = Kernel::boot(
            db,
            catalog,
            hooks,
            KernelSettings {
                webhooks_allow_private: settings.webhooks_allow_private,
                secret_key: Some(secret_key),
                pdf_url: Some(settings.pdf_url.trim().to_string()).filter(|u| !u.is_empty()),
                ..Default::default()
            },
            mailer,
        )
        .await
        .map_err(|e| Error::Message(format!("kernel boot: {e}")))?;
        ferrum_domain::maintenance(&kernel).await.map_err(|e| Error::Message(format!("domain maintenance: {e}")))?;
        // Loco signs with the base64-decoded secret: reject anything that is not valid base64 at boot.
        if loco_rs::auth::jwt::JWT::new(&jwt.secret).generate_token(60, "probe".into(), serde_json::Map::new()).is_err() {
            return Err(Error::Message("auth.jwt.secret must be base64 (generate one with `openssl rand -base64 48`)".into()));
        }
        if jwt.secret.len() < 44 && ctx.environment == Environment::Production {
            return Err(Error::Message("auth.jwt.secret must encode at least 32 random bytes in production".into()));
        }
        let storage = build_storage(&settings)?;
        probe_storage(&storage).await;
        let objects = Arc::new(crate::objects::Objects::new(&settings).map_err(|e| Error::Message(format!("object storage: {e}")))?);
        let storage = Arc::new(storage);
        let apps = crate::appmgr::AppManager::new(kernel.clone(), plugins.clone(), storage.clone(), &settings.apps, crate::appmgr::compiled_natives())
            .map_err(Error::Message)?;
        let (tx, _rx) = watch::channel(false);
        let state = AppState {
            kernel,
            plugins,
            live: crate::live::LiveHub::new(),
            settings,
            shutdown: Arc::new(tx),
            background: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            jwt_secret: jwt.secret.clone(),
            jwt_expiration: jwt.expiration,
            login_limiter: Arc::new(RateLimiter::new(Duration::from_secs(60), 20)),
            mail_limiter: Arc::new(RateLimiter::new(Duration::from_secs(60), 30)),
            apps: Arc::new(apps),
            objects,
        };
        state.kernel.set_file_store(Arc::new(crate::mailer::StorageFiles(storage.clone())));
        let ctx = ctx.into_builder().storage(storage).build();
        ctx.shared_store.insert(state);
        Ok(ctx)
    }

    async fn before_run(ctx: &AppContext) -> Result<()> {
        let Some(state) = ctx.shared_store.get::<AppState>() else { return Ok(()) };
        // Every replica that serves requests: cache invalidation and delivery to the sockets it holds.
        let mut listeners = state.kernel.spawn_listeners(state.shutdown.subscribe());
        listeners.push(state.plugins.sockets.spawn_listener(state.kernel.db.clone(), state.shutdown.subscribe()));
        listeners.push(state.live.spawn_listener(state.kernel.clone(), state.shutdown.subscribe()));
        state.background.lock().await.extend(listeners);
        if state.settings.run_background {
            let mut handles = state.kernel.spawn_background(state.shutdown.subscribe());
            // Domain upkeep (ledger partitions for the coming year) on the same cadence.
            let (k, mut stop) = (state.kernel.clone(), state.shutdown.subscribe());
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(6 * 3600)) => {}
                    }
                    if let Err(e) = ferrum_domain::maintenance(&k).await {
                        tracing::warn!("domain maintenance failed: {e}");
                    }
                }
            }));
            handles.push(crate::packages::spawn(state.kernel.clone(), state.shutdown.subscribe()));
            // Plugin schedules.
            handles.push(ferrum_plugins::schedule::spawn_scheduler(state.plugins.clone(), state.kernel.clone(), state.shutdown.subscribe()));
            // App stores' revocation lists.
            handles.push(state.apps.clone().spawn_revocations(state.shutdown.subscribe()));
            // Direct uploads that were never completed, with their bytes.
            let (k, objects, mut stop) = (state.kernel.clone(), state.objects.clone(), state.shutdown.subscribe());
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(600)) => {}
                    }
                    match crate::objects::sweep(&k, &objects).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!("removed {n} expired direct uploads"),
                        Err(e) => tracing::warn!("sweeping expired uploads failed: {e}"),
                    }
                }
            }));
            state.background.lock().await.extend(handles);
            tracing::info!("background loops started (outbox, webhooks, metadata listener, maintenance)");
        }
        Ok(())
    }

    async fn on_shutdown(ctx: &AppContext) {
        if let Some(state) = ctx.shared_store.get::<AppState>() {
            let _ = state.shutdown.send(true);
            let handles: Vec<_> = state.background.lock().await.drain(..).collect();
            for h in handles {
                let _ = tokio::time::timeout(Duration::from_secs(10), h).await;
            }
            state.kernel.db.pool.close().await;
            state.kernel.db.side.close().await;
            tracing::info!("shutdown complete");
        }
    }

    fn routes(_ctx: &AppContext) -> AppRoutes {
        AppRoutes::with_default_routes()
            .add_route(controllers::system::routes())
            .add_route(controllers::auth::routes())
            .add_route(controllers::meta::routes())
            .add_route(controllers::docs::routes())
            .add_route(controllers::collab::routes())
            .add_route(controllers::prefs::routes())
            .add_route(controllers::admin::routes())
            .add_route(controllers::apps::routes())
            .add_route(controllers::compat::routes())
            .add_route(controllers::mcp::routes())
            .add_route(controllers::oauth::routes())
            .add_route(controllers::plugins::routes())
            .add_route(crate::live::routes())
            .add_route(controllers::sites::routes())
    }

    async fn connect_workers(_ctx: &AppContext, _queue: &Queue) -> Result<()> {
        Ok(())
    }

    fn register_tasks(tasks: &mut Tasks) {
        tasks.register(crate::tasks::CreateTenant);
        tasks.register(crate::tasks::SyncSchema);
        tasks.register(crate::tasks::ImportErpnext);
        tasks.register(crate::tasks::RunMaintenance);
        tasks.register(crate::tasks::SeedDemo);
        tasks.register(crate::tasks::AppMakeAvailable);
        tasks.register(crate::tasks::AppUpgrade);
        tasks.register(crate::tasks::AppRevoke);
        tasks.register(crate::tasks::AppReleases);
    }

    async fn truncate(_ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    async fn seed(_ctx: &AppContext, _base: &Path) -> Result<()> {
        Ok(())
    }
}

/// The key sealing secret fields: configured (`FERRUM_SECRET_KEY`), required in production; in
/// development derived from the JWT secret so secret fields work out of the box.
fn secret_key(settings: &Settings, jwt_secret: &str, env: &Environment) -> Result<ferrum_kernel::secret::SecretKey> {
    if !settings.secret_key.trim().is_empty() {
        return ferrum_kernel::secret::SecretKey::from_base64(&settings.secret_key).map_err(|e| Error::Message(format!("settings.secret_key: {e}")));
    }
    if *env == Environment::Production {
        return Err(Error::Message("FERRUM_SECRET_KEY is required in production (openssl rand -base64 32)".into()));
    }
    use sha2::Digest;
    Ok(ferrum_kernel::secret::SecretKey::new(sha2::Sha256::digest(format!("ferrum-secret-fields:{jwt_secret}")).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip through an S3-compatible store. Runs when `FERRUM_TEST_S3_ENDPOINT` is set (CI
    /// starts MinIO); `FERRUM_TEST_S3_BUCKET`, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` go with it.
    #[tokio::test]
    async fn s3_compatible_storage_round_trip() {
        let Ok(endpoint) = std::env::var("FERRUM_TEST_S3_ENDPOINT") else {
            eprintln!("skipped: FERRUM_TEST_S3_ENDPOINT is not set");
            return;
        };
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "s3_bucket": std::env::var("FERRUM_TEST_S3_BUCKET").unwrap_or_else(|_| "ferrum-ci".into()),
            "s3_region": "us-east-1",
            "s3_endpoint": endpoint,
        }))
        .unwrap();
        let storage = build_storage(&settings).expect("storage");
        let path = std::path::PathBuf::from(format!("tests/{}", uuid::Uuid::now_v7()));
        storage.upload(&path, &bytes::Bytes::from_static(b"ferrum")).await.expect("upload");
        let back: Vec<u8> = storage.download(&path).await.expect("download");
        assert_eq!(back, b"ferrum");
        storage.delete(&path).await.expect("delete");
        assert!(storage.download::<Vec<u8>>(&path).await.is_err(), "deleted");
    }

    #[test]
    fn local_files_need_an_explicit_opt_out_when_object_storage_is_required() {
        let settings: Settings = serde_json::from_value(serde_json::json!({"require_object_storage": true})).unwrap();
        assert!(build_storage(&settings).is_err());
    }
}
