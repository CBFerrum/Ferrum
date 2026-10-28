//! Loco application wiring: boots the kernel (migrations + metadata DDL), registers domain
//! hooks, mounts routes, and runs background loops with graceful shutdown.

use crate::controllers;
use crate::mailer::LocoMailer;
use crate::state::{AppState, RateLimiter, Settings};
use async_trait::async_trait;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings, Mailer};
use ferrum_meta::Catalog;
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
        let driver = storage::drivers::aws::new(&settings.s3_bucket, &settings.s3_region).map_err(|e| Error::Message(format!("S3 storage: {e}")))?;
        return Ok(Storage::single(driver));
    }
    std::fs::create_dir_all(&settings.files_dir).map_err(|e| Error::Message(format!("files_dir: {e}")))?;
    Ok(Storage::single(storage::drivers::local::new_with_prefix(&settings.files_dir).map_err(|e| Error::Message(e.to_string()))?))
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
        let catalog = Catalog::load_dir(Path::new(&settings.packages_dir)).map_err(|e| Error::Message(format!("packages: {e}")))?;
        let mut hooks = HookRegistry::default();
        ferrum_domain::register(&mut hooks);
        let plugins = ferrum_plugins::PluginHost::new().map_err(|e| Error::Message(format!("plugins: {e}")))?;
        hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins.clone())));
        let mailer: Option<Arc<dyn Mailer>> = ctx.mailer.clone().map(|sender| Arc::new(LocoMailer { sender, from: None }) as Arc<dyn Mailer>);
        let kernel = Kernel::boot(db, catalog, hooks, KernelSettings { webhooks_allow_private: settings.webhooks_allow_private, ..Default::default() }, mailer)
            .await
            .map_err(|e| Error::Message(format!("kernel boot: {e}")))?;
        ferrum_domain::maintenance(&kernel).await.map_err(|e| Error::Message(format!("domain maintenance: {e}")))?;
        let jwt = ctx.config.auth.as_ref().and_then(|a| a.jwt.as_ref()).ok_or_else(|| Error::Message("auth.jwt is not configured".into()))?;
        // Loco signs with the base64-decoded secret: reject anything that is not valid base64 at boot.
        if loco_rs::auth::jwt::JWT::new(&jwt.secret).generate_token(60, "probe".into(), serde_json::Map::new()).is_err() {
            return Err(Error::Message("auth.jwt.secret must be base64 (generate one with `openssl rand -base64 48`)".into()));
        }
        if jwt.secret.len() < 44 && ctx.environment == Environment::Production {
            return Err(Error::Message("auth.jwt.secret must encode at least 32 random bytes in production".into()));
        }
        let storage = build_storage(&settings)?;
        let (tx, _rx) = watch::channel(false);
        let state = AppState {
            kernel,
            plugins,
            settings,
            shutdown: Arc::new(tx),
            background: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            jwt_secret: jwt.secret.clone(),
            jwt_expiration: jwt.expiration,
            login_limiter: Arc::new(RateLimiter::new(Duration::from_secs(60), 20)),
        };
        let ctx = ctx.into_builder().storage(Arc::new(storage)).build();
        ctx.shared_store.insert(state);
        Ok(ctx)
    }

    async fn before_run(ctx: &AppContext) -> Result<()> {
        let Some(state) = ctx.shared_store.get::<AppState>() else { return Ok(()) };
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
            .add_route(controllers::admin::routes())
            .add_route(controllers::compat::routes())
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
    }

    async fn truncate(_ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    async fn seed(_ctx: &AppContext, _base: &Path) -> Result<()> {
        Ok(())
    }
}
