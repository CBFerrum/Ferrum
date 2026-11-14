//! Loco application wiring: boots the kernel (migrations + metadata DDL), registers domain
//! hooks, mounts routes, and runs background loops with graceful shutdown.

use crate::controllers;
use crate::mailer::LocoMailer;
use crate::state::{AppState, RateLimiter, Settings};
use async_trait::async_trait;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings, Mailer};
use loco_rs::app::{AppContext, Hooks, Initializer};
use loco_rs::bgworker::Queue;
use loco_rs::boot::{BootResult, ServeParams, StartMode, create_app};
use loco_rs::config::Config;
use loco_rs::controller::AppRoutes;
use loco_rs::environment::Environment;
use loco_rs::storage::{self, Storage};
use loco_rs::task::Tasks;
use loco_rs::{Error, Result};
use migration::{Migrator, MigratorTrait};
use std::future::IntoFuture;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub struct App;

pub(crate) fn build_storage(settings: &Settings) -> Result<Storage> {
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

/// How long a stopping replica keeps answering the requests in flight. Kubernetes gives a replica
/// 40 s after its 5 s preStop pause (deploy/k8s/deployment.yaml); this and the background loops'
/// 10 s fit inside it.
const DRAIN: Duration = Duration::from_secs(20);

#[async_trait]
impl Hooks for App {
    /// Loco's own `serve` runs `on_shutdown` the moment the signal comes, before the server has
    /// answered the requests in flight, so those met closed database pools (and failed with 503).
    /// Here the signal stops the background loops and live connections, the requests in flight
    /// get up to [`DRAIN`] to finish, and only then do the pools close.
    async fn serve(app: axum::Router, ctx: &AppContext, params: &ServeParams) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(format!("{}:{}", params.binding, params.port)).await?;
        let (fired, signalled) = tokio::sync::oneshot::channel::<()>();
        let stopping = ctx.clone();
        let server = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).with_graceful_shutdown(async move {
            loco_rs::boot::shutdown_signal().await;
            tracing::info!("shutting down: answering the requests in flight");
            if let Some(state) = stopping.shared_store.get::<AppState>() {
                let _ = state.shutdown.send(true);
            }
            let _ = fired.send(());
        });
        let drained = async {
            match signalled.await {
                Ok(()) => tokio::time::sleep(DRAIN).await,
                // The server ended before any signal (its error is what matters).
                Err(_) => std::future::pending().await,
            }
        };
        tokio::select! {
            served = server.into_future() => served?,
            () = drained => tracing::warn!("requests still in flight after {} s: stopping anyway", DRAIN.as_secs()),
        }
        Self::on_shutdown(ctx).await;
        Ok(())
    }

    /// Loco's middleware, but our request log (queries are left out of the log).
    fn middlewares(ctx: &AppContext) -> Vec<Box<dyn loco_rs::controller::middleware::MiddlewareLayer>> {
        loco_rs::controller::middleware::default_middleware_stack(ctx)
            .into_iter()
            .map(|m| if m.name() == "logger" { Box::new(crate::request_log::RequestLog::new(ctx)) } else { m })
            .collect()
    }

    fn app_name() -> &'static str {
        "ferrum"
    }

    /// Loco's log, with traces to an OpenTelemetry collector when one is configured (telemetry.rs).
    fn init_logger(ctx: &AppContext) -> Result<bool> {
        crate::telemetry::init(&ctx.config.logger)
    }

    fn app_version() -> String {
        format!(
            "{} ({}, built from a commit of {})",
            env!("CARGO_PKG_VERSION"),
            option_env!("BUILD_SHA").or(option_env!("GITHUB_SHA")).unwrap_or("dev"),
            ferrum_kernel::window::build_date().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )
    }

    async fn boot(mode: StartMode, environment: &Environment, config: Config) -> Result<BootResult> {
        create_app::<Self, Migrator>(mode, environment, config).await
    }

    async fn initializers(_ctx: &AppContext) -> Result<Vec<Box<dyn Initializer>>> {
        Ok(vec![])
    }

    async fn after_context(ctx: AppContext) -> Result<AppContext> {
        let settings = Settings::from_ctx(&ctx)?;
        // The hosts this server names for itself, checked before it connects to or writes anything, on every start
        // (a guest of a newer build's database too).
        crate::shell::check_names(&settings, &ctx.config.server.host).map_err(Error::Message)?;
        // Zones (docs/operations.md, "Zones"): anything not plainly right refuses to start.
        let zones = ferrum_kernel::zones::Zones::parse(&settings.zones, &settings.zone, &settings.zone_default, &settings.zone_mode).map_err(Error::Message)?;
        zones.check_background(settings.run_background).map_err(Error::Message)?;
        // The audit sink (docs/operations.md, "Sending the audit log elsewhere"): checked before anything else too.
        let audit_sink = ferrum_kernel::audit_sink::Sink::parse(&settings.audit_sink_url, &settings.audit_sink_secret).map_err(Error::Message)?;
        // `ferrum task migrate` is what brings the database up to date, whatever the setting says.
        let migrating = is_migrate_task(std::env::args());
        let migrate_now = settings.migrate_at_start || migrating;
        if !migrate_now {
            // Loco's own migrator (empty today: the kernel owns the schema): nothing may be waiting for it.
            let waiting = Migrator::get_pending_migrations(&ctx.db).await.map_err(|e| Error::Message(format!("database: {e}")))?;
            if !waiting.is_empty() {
                return Err(Error::Message(format!(
                    "the database is not ready for this build: {} Loco migrations are not applied. Run `ferrum task migrate`, or start with FERRUM_MIGRATE_AT_START=true",
                    waiting.len()
                )));
            }
        }
        crate::extract::PROXY_HOPS.store(settings.proxy_hops, std::sync::atomic::Ordering::Relaxed);
        if settings.db_max_connections < ferrum_kernel::doctor::POOL_FLOOR {
            tracing::warn!(
                connections = settings.db_max_connections,
                "fewer than {} database connections: requests will wait behind this replica's own job workers and event loops",
                ferrum_kernel::doctor::POOL_FLOOR
            );
        }
        let db = Db::connect(DbConfig {
            url: ctx.config.database.uri.clone(),
            max_connections: settings.db_max_connections,
            // Unset: the same address as DATABASE_URL, one pool for everything as before.
            direct_url: Some(Some(settings.database_direct_url.trim()).filter(|u| !u.is_empty()).unwrap_or(&ctx.config.database.uri).to_string()),
            direct_connections: ferrum_kernel::db::LISTENERS + ferrum_kernel::db::DIRECT_LOCKS + crate::packages::upkeep_concurrency() as u32,
            read_url: Some(settings.database_read_url.trim().to_string()).filter(|u| !u.is_empty()),
            read_max_lag: Duration::from_secs(settings.database_read_max_lag_secs),
            ..Default::default()
        })
        .await
        .map_err(|e| Error::Message(format!("database: {e}")))?;
        // A database fenced for a planned move: the server starts (its start-up steps write), then serves read-only.
        db.starting(true);
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
        crate::packages::register(&mut hooks).map_err(|e| Error::Message(format!("apps: {e}")))?;
        let plugins = ferrum_plugins::PluginHost::new(settings.webhooks_allow_private).map_err(|e| Error::Message(format!("plugins: {e}")))?;
        hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins.clone())));
        hooks.add_event_sink(Arc::new(ferrum_plugins::PluginEvents(plugins.clone())));
        hooks.set_connector_source(Arc::new(ferrum_plugins::connect::PluginConnectors(plugins.clone())));
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
        let transit = kms_transit(&settings)?;
        let secret_key = secret_key(&settings, transit.as_ref(), &jwt.secret, &ctx.environment).await?;
        let mut retired = Vec::new();
        for k in settings.retired_secret_keys.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            retired.push(ferrum_kernel::kms::resolve(k, transit.as_ref()).await.map_err(|e| Error::Message(format!("settings.retired_secret_keys: {e}")))?);
        }
        plugins.seal_with(secret_key.clone());
        plugins.open_also_with(retired.clone());
        let processes = crate::process::configure(&mut hooks, &secret_key, &retired).await.map_err(Error::Message)?;
        let (plans, signup) = signup_settings(|name| std::env::var(name).ok(), crate::shell::base_domain(&settings).is_some()).map_err(Error::Message)?;
        if signup.open && mailer.is_none() {
            return Err(Error::Message("FERRUM_SIGNUP=open needs mail (settings.mailer): sign-up proves addresses with emailed codes".into()));
        }
        let kernel = Kernel::boot(
            db,
            catalog,
            hooks,
            KernelSettings {
                webhooks_allow_private: settings.webhooks_allow_private,
                connection_hosts: std::env::var("FERRUM_CONNECTION_HOSTS")
                    .unwrap_or_default()
                    .split(',')
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect(),
                connection_upload_bytes: std::env::var("FERRUM_CONNECTION_UPLOAD_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(25) << 20,
                sso_allow_private: settings.sso_allow_private,
                secret_key: Some(secret_key),
                retired_secret_keys: retired,
                clamd: Some(settings.clamav_addr.trim().to_string()).filter(|a| !a.is_empty()),
                clamd_max_bytes: settings.clamav_max_mb << 20,
                pdf_url: Some(settings.pdf_url.trim().to_string()).filter(|u| !u.is_empty()),
                public_url: Some(ctx.config.server.host.trim().trim_end_matches('/').to_string()).filter(|u| u.starts_with("http")),
                push_services: push_services(&settings.push_services),
                push_contact: Some(settings.push_contact.trim().to_string()).filter(|c| c.starts_with("mailto:") || c.starts_with("https://")),
                hold_on_timeline: std::env::var("FERRUM_HOLD_ON_TIMELINE").map_or(true, |v| v != "false"),
                accounts_host: crate::shell::accounts_host(&settings),
                hooks_url: crate::shell::hooks_url(&settings, &ctx.config.server.host),
                passkey_rp: crate::shell::host_setting("FERRUM_PASSKEY_RP", &settings.passkey_rp).ok().flatten(),
                link_host: crate::shell::host_setting("FERRUM_LINK_HOST", &settings.link_host).ok().flatten(),
                plans,
                signup,
                process_runtime: processes,
                migrate_at_start: migrate_now,
                audit_sink,
                host_leaving_days: settings.host_leaving_days.min(365).into(),
                ..Default::default()
            },
            mailer,
        )
        .await
        .map_err(|e| Error::Message(format!("kernel boot: {e}")))?;
        // A zone that limits staff to some apps names them by id: each resolved to its packages, or the server does not start.
        let zones = Arc::new(crate::zone::resolve(zones, &kernel).await.map_err(Error::Message)?);
        kernel.set_zones(zones.clone());
        // From now on a rollback to a build that would not enforce them is refused (docs/operations.md, "Protections an
        // older build would not enforce"). A guest of a newer build writes nothing.
        kernel.require_zone_floors(&zones).await.map_err(|e| Error::Message(format!("zones: {e}")))?;
        ferrum_app_erp::maintenance(&kernel).await.map_err(|e| Error::Message(format!("ERP's upkeep: {e}")))?;
        if let Err(e) = kernel.registry.catalog().closure(&kernel.settings.signup.packages) {
            return Err(Error::Message(format!("FERRUM_SIGNUP_PACKAGES: {e}")));
        }
        // Plugins configured before their settings were sealed (gateways' keys among them) are sealed now.
        match plugins.seal_stored(&kernel.db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(plugins = n, "sealed stored plugin settings"),
            Err(e) => tracing::warn!("sealing stored plugin settings: {e}"),
        }
        match kernel.reseal_secrets().await {
            Ok(done) if done.unopened > 0 => tracing::error!(moved = done.moved, unopened = done.unopened, "secrets remain that no configured key opens"),
            Ok(done) if done.moved > 0 => tracing::info!(moved = done.moved, "moved stored secrets to the current key"),
            Ok(_) => {}
            Err(e) => tracing::warn!("resealing stored secrets: {e}"),
        }
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
        crate::request_metrics::Requests::install(crate::request_metrics::Requests::new(&crate::appmgr::compiled_natives()));
        let apps = crate::appmgr::AppManager::new(kernel.clone(), plugins.clone(), storage.clone(), &settings.apps, crate::appmgr::compiled_natives())
            .map_err(Error::Message)?;
        let shell = Arc::new(crate::shell::Shell::new(kernel.clone(), storage.clone(), &settings, &ctx.config.server.host));
        // What would put this build's older things back on a newer build's database (crates/kernel/src/window.rs): held
        // so that no newer build migrates meanwhile, and left out by a guest (the upgrade steps and their marker, apps
        // recorded at this build's versions, interfaces registered for them).
        match kernel.hold_start().await.map_err(|e| Error::Message(format!("kernel boot: {e}")))? {
            Some(hold) => {
                // Workspaces from before this release's package layout move onto it (logged, not fatal:
                // the next start tries again).
                if migrating {
                    // The task runs the steps itself, to print them and to fail when one fails.
                } else if migrate_now {
                    if let Err(e) = crate::upgrade::run(&kernel).await {
                        tracing::error!("moving workspaces onto this release's packages failed: {e}");
                    }
                } else {
                    crate::upgrade::verify(&kernel).await.map_err(|e| Error::Message(e.to_string()))?;
                }
                apps.ensure_native("erp", "accounting").await;
                apps.ensure_native("studio", "studio").await;
                crate::shell::register_compiled(&kernel, &storage, &settings, &apps).await;
                hold.release().await;
            }
            None => tracing::warn!("a newer build has this database: its upgrade steps, apps and interfaces are left as that build made them"),
        }
        if settings.dev_ui {
            tracing::warn!("FERRUM_DEV_UI is on: administrators may serve apps' pages from development servers on this machine; never on a server people use");
        }
        // Shapes B and C (docs/plan/apps-ui.md §7, §9.3): the directory of Ferrum accounts, and every
        // workspace's subdomain of the base domain.
        if let Some(base) = crate::shell::base_domain(&settings)
            && ferrum_kernel::shell::normalize_host(&format!("{}.{base}", ferrum_kernel::identity::DIRECTORY)).is_none()
        {
            return Err(Error::Message(format!("settings.base_domain ({base}) is not a domain name")));
        }
        // An accounts host, with a base domain or set on its own: identities sign in there.
        if crate::shell::accounts_host(&settings).is_some() {
            kernel.ensure_directory().await.map_err(|e| Error::Message(format!("the accounts directory: {e}")))?;
        }
        if let Some(base) = crate::shell::base_domain(&settings) {
            match kernel.ensure_subdomains(&base).await {
                Ok(0) => {}
                Ok(added) => tracing::info!(added, %base, "workspaces got their subdomains"),
                Err(e) => tracing::error!("workspaces' subdomains: {e}"),
            }
        }
        // `ferrum start`: Loco sets up a context only to read the configuration, then the one that serves, and keeps the
        // first as long as the process runs. Once this one is ready, the earlier one's pools are closed, so that they hold
        // no connections (docs/operations.md, "Database role").
        if is_start(std::env::args()) {
            let earlier = EARLIER_SET_UP.lock().ok().and_then(|mut e| e.replace((kernel.db.clone(), ctx.db.clone())));
            if let Some((db, loco)) = earlier {
                tokio::spawn(async move {
                    db.close().await;
                    let _ = loco.close_by_ref().await;
                });
            }
        }
        let (tx, _rx) = watch::channel(false);
        let db = kernel.db.clone();
        let user_requests = settings.user_requests_per_minute;
        let address_requests = settings.address_requests_per_minute;
        let tenant_requests = settings.tenant_requests.unwrap_or((settings.db_max_connections / 2).max(2));
        let state = AppState {
            kernel,
            plugins,
            live: crate::live::LiveHub::new(),
            docs: crate::coedit::Docs::new(),
            settings,
            shutdown: Arc::new(tx),
            background: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            jwt_secret: jwt.secret.clone(),
            jwt_expiration: jwt.expiration,
            login_limiter: Arc::new(RateLimiter::shared("login", Duration::from_secs(60), 20, db.clone())),
            mail_limiter: Arc::new(RateLimiter::shared("mail", Duration::from_secs(60), 30, db.clone())),
            // Public endpoints are open to the internet: a generous per-address budget stops floods.
            public_limiter: Arc::new(RateLimiter::shared("public", Duration::from_secs(60), 300, db)),
            // A runaway script with a valid token slows down instead of slowing everyone down.
            user_limiter: Arc::new(RateLimiter::local_only("user", Duration::from_secs(60), user_requests)),
            // One address flooding the API is turned away cheaply, whatever proxy is in front.
            address_limiter: Arc::new(RateLimiter::local_only("address", Duration::from_secs(60), address_requests)),
            // A workspace that floods the server waits for itself; everyone else has the rest.
            shares: Arc::new(crate::state::Shares::new(tenant_requests, Duration::from_secs(5))),
            apps: Arc::new(apps),
            objects,
            shell,
            assistant: crate::assistant::Assistant::new(crate::assistant::Config::from_env()),
            zones,
        };
        state.kernel.set_file_store(Arc::new(crate::mailer::StorageFiles(storage.clone(), state.objects.clone())));
        state.kernel.db.starting(false);
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
        listeners.extend(state.docs.spawn(state.kernel.clone(), state.shutdown.subscribe()));
        // The web app's image offers the shell's and its apps' interfaces: fetch them, then look
        // for new ones every ten minutes (every replica: registering is idempotent).
        if !state.settings.ui_source.trim().is_empty() {
            let (st, mut stop) = (state.clone(), state.shutdown.subscribe());
            let storage = ctx.storage.clone();
            listeners.push(tokio::spawn(async move {
                loop {
                    let wait = match crate::shell::pull_releases(&st.kernel, &storage, &st.apps, &st.settings.ui_source).await {
                        Ok(_) => Duration::from_secs(600),
                        Err(e) => {
                            tracing::warn!("fetching interfaces failed (retrying in 30 s): {e}");
                            Duration::from_secs(30)
                        }
                    };
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }));
        }
        state.background.lock().await.extend(listeners);
        // Where a region can fail over, it writes only while a witness says it may (docs/operations.md).
        if let Ok(witness) = std::env::var("FERRUM_AUTHORITY_WITNESS_URL")
            && !witness.trim().is_empty()
        {
            let region = std::env::var("FERRUM_REGION").map_err(|_| Error::Message("FERRUM_AUTHORITY_WITNESS_URL needs FERRUM_REGION".into()))?;
            let ttl = std::env::var("FERRUM_AUTHORITY_TTL_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
            let authority = ferrum_kernel::recovery::Authority::connect(&witness, "ferrum", &region, Duration::from_secs(ttl))
                .await
                .map_err(|e| Error::Message(e.to_string()))?;
            state.background.lock().await.push(state.kernel.spawn_authority(authority, state.shutdown.subscribe()));
        }
        if state.settings.run_background {
            let mut handles = state.kernel.spawn_background(state.shutdown.subscribe());
            // The operator's audit sink, where background work runs.
            handles.extend(state.kernel.spawn_audit_sink(state.shutdown.subscribe()));
            // ERP's upkeep (journal partitions for the coming year) on the same cadence.
            let (k, mut stop) = (state.kernel.clone(), state.shutdown.subscribe());
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(6 * 3600)) => {}
                    }
                    if let Err(e) = ferrum_kernel::sandbox::upkeep(ferrum_app_erp::maintenance(&k)).await {
                        tracing::warn!("ERP's upkeep failed: {e}");
                    }
                }
            }));
            handles.push(crate::packages::spawn(state.kernel.clone(), state.shutdown.subscribe()));
            if let Some(worker) = crate::process::spawn(state.kernel.clone(), state.shutdown.subscribe()).await.map_err(Error::Message)? {
                handles.push(worker);
            }
            // Apps that were waiting for their resources.
            let (apps, mut stop) = (state.apps.clone(), state.shutdown.subscribe());
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                    if let Err(e) = apps.activate_waiting().await {
                        tracing::warn!("activating waiting apps failed: {e}");
                    }
                }
            }));
            // Plugin schedules.
            handles.push(ferrum_plugins::schedule::spawn_scheduler(state.plugins.clone(), state.kernel.clone(), state.shutdown.subscribe()));
            // App stores' revocation lists.
            handles.push(state.apps.clone().spawn_revocations(state.shutdown.subscribe()));
            // Direct uploads that were never completed, with their bytes; finished rate-limit windows.
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
                    if let Err(e) = k.db.sweep_rate_windows().await {
                        tracing::warn!("sweeping rate-limit windows failed: {e}");
                    }
                }
            }));
            // Interface releases: mark those this replica served as used; purge the unused.
            let (shell, mut stop) = (state.shell.clone(), state.shutdown.subscribe());
            handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = stop.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
                    }
                    if let Err(e) = shell.touch_used().await {
                        tracing::warn!("marking interface releases as used failed: {e}");
                    }
                    match shell.purge().await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!("removed {n} unused interface releases"),
                        Err(e) => tracing::warn!("purging interface releases failed: {e}"),
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
            // Together, so a replica stops within its grace period however many loops it runs.
            let handles: Vec<_> = state.background.lock().await.drain(..).collect();
            if tokio::time::timeout(Duration::from_secs(10), futures_util::future::join_all(handles)).await.is_err() {
                tracing::warn!("background loops still busy after 10 s: stopping anyway");
            }
            state.kernel.db.close().await;
            crate::telemetry::shutdown();
            tracing::info!("shutdown complete");
        }
    }

    async fn after_routes(router: axum::Router, ctx: &AppContext) -> Result<axum::Router> {
        let state = crate::state::app_state(ctx).map_err(|e| Error::Message(e.0.to_string()))?;
        let limiter = state.address_limiter.clone();
        let apps_base: Arc<str> = Arc::from(state.settings.apps_base.trim());
        Ok(router
            .layer(axum::middleware::from_fn(hold_place))
            .layer(axum::middleware::from_fn(move |req, next| per_address(limiter.clone(), req, next)))
            .layer(axum::middleware::from_fn(api_no_store))
            .layer(axum::middleware::from_fn(speculation_csp))
            .layer(axum::middleware::from_fn(screen_paths))
            .layer(axum::middleware::from_fn(isolated_cors))
            .layer(axum::middleware::from_fn(traced))
            // A signed-out page goes by the browser's language (sign-in, a status page).
            .layer(axum::middleware::from_fn(crate::shell_words::browser))
            .layer(axum::middleware::from_fn(move |req, next| app_origins(apps_base.clone(), req, next)))
            // Outermost: a request's whole time, by app and kind (/metrics).
            .layer(axum::middleware::from_fn(crate::request_metrics::count)))
    }

    fn routes(_ctx: &AppContext) -> AppRoutes {
        AppRoutes::with_default_routes()
            .add_route(controllers::system::routes())
            .add_route(controllers::auth::routes())
            .add_route(controllers::accounts::routes())
            .add_route(controllers::meta::routes())
            .add_route(controllers::docs::routes())
            .add_route(controllers::collab::routes())
            .add_route(controllers::prefs::routes())
            .add_route(controllers::admin::routes())
            .add_route(controllers::jobs::routes())
            .add_route(controllers::connect::routes())
            .add_route(controllers::documents::routes())
            .add_route(controllers::connections::routes())
            .add_route(controllers::messages::routes())
            .add_route(controllers::apps::routes())
            .add_route(controllers::compat::routes())
            .add_route(controllers::mcp::routes())
            .add_route(controllers::oauth::routes())
            .add_route(controllers::legacy::routes())
            .add_route(controllers::lists::routes())
            .add_route(controllers::links::routes())
            .add_route(controllers::plugins::routes())
            .add_route(crate::live::routes())
            .add_route(crate::coedit::routes())
            .add_route(controllers::sites::routes())
            .add_route(controllers::dav::routes())
            .add_route(controllers::scim::routes())
            .add_route(controllers::assistant::routes())
            .add_route(controllers::search::routes())
            .add_route(controllers::notifications::routes())
            .add_route(controllers::work::routes())
            .add_route(controllers::cards::routes())
            .add_route(controllers::domains::routes())
            .add_route(controllers::shell::routes())
    }

    async fn connect_workers(_ctx: &AppContext, _queue: &Queue) -> Result<()> {
        Ok(())
    }

    fn register_tasks(tasks: &mut Tasks) {
        tasks.register(crate::tasks::CreateTenant);
        #[cfg(feature = "process")]
        tasks.register(crate::tasks::LogicReplay);
        tasks.register(crate::tasks::Doctor);
        tasks.register(crate::tasks::KmsWrap);
        tasks.register(crate::tasks::AccountState);
        tasks.register(crate::tasks::SetPlan);
        tasks.register(crate::tasks::ReapplyErasures);
        tasks.register(crate::tasks::SyncSchema);
        tasks.register(crate::tasks::Migrate);
        tasks.register(crate::tasks::ImportErpnext);
        tasks.register(crate::tasks::RunMaintenance);
        tasks.register(crate::tasks::SeedDemo);
        tasks.register(crate::tasks::DepsReport);
        tasks.register(crate::tasks::UpgradeBack);
        tasks.register(crate::tasks::AppMakeAvailable);
        tasks.register(crate::tasks::AppUpgrade);
        tasks.register(crate::tasks::AppRevoke);
        tasks.register(crate::tasks::AppReleases);
        tasks.register(crate::tasks::AssistantCheck);
        tasks.register(crate::tasks::AssistantEval);
        tasks.register(crate::tasks::ReleaseRestore);
        tasks.register(crate::tasks::AcceptAuthority);
        tasks.register(crate::tasks::ExtractTenant);
        tasks.register(crate::tasks::DeleteTenant);
        tasks.register(crate::tasks::AddHost);
        tasks.register(crate::tasks::SetHostZone);
        tasks.register(crate::tasks::OperatorAdmin(true));
        tasks.register(crate::tasks::OperatorAdmin(false));
        tasks.register(crate::tasks::ClearStaffNetworks);
        tasks.register(crate::tasks::PlanMove);
        tasks.register(crate::tasks::CancelMove);
        tasks.register(crate::tasks::AllowOlderBuilds);
        tasks.register(crate::tasks::AdoptMove);
        tasks.register(crate::tasks::CheckFiles);
        tasks.register(crate::tasks::MoveWorkspace);
        tasks.register(crate::tasks::FinishMove);
        tasks.register(crate::tasks::CopyWorkspace);
        tasks.register(crate::tasks::DeleteSandbox);
        tasks.register(crate::tasks::ReleaseHost);
        tasks.register(crate::tasks::SamlEncryptionKey);
        for app in crate::extensions::all() {
            (app.tasks)(tasks);
        }
    }

    async fn truncate(_ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    async fn seed(_ctx: &AppContext, _base: &Path) -> Result<()> {
        Ok(())
    }
}

/// Each client address's budget. Only requests that came through the proxy (`X-Forwarded-For`)
/// count: probes, port-forwards and the cluster's own calls have no address to tell apart.
/// Runs a request under its trace: the events it writes name its request id, and the caller's W3C
/// trace context travels with them. This layer sits outside Loco's request id, so it settles the id
/// and puts it on the request: the log, the request's context and its events all name the same one.
async fn traced(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let correlation = crate::extract::request_id(req.headers());
    if let Ok(id) = axum::http::HeaderValue::from_str(&correlation) {
        req.headers_mut().insert("x-request-id", id);
    }
    let traceparent = req.headers().get("traceparent").and_then(|v| v.to_str().ok()).filter(|v| valid_traceparent(v)).map(str::to_string);
    ferrum_kernel::Trace { correlation, traceparent, ..Default::default() }.scope(next.run(req)).await
}

/// `version-traceid-parentid-flags` in lower-case hexadecimal (W3C Trace Context): anything else
/// is dropped rather than stored as the caller sent it.
fn valid_traceparent(v: &str) -> bool {
    let parts: Vec<&str> = v.split('-').collect();
    matches!(parts.as_slice(), [a, b, c, d] if a.len() == 2 && b.len() == 32 && c.len() == 16 && d.len() == 2)
        && parts.iter().all(|p| p.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

async fn per_address(limiter: Arc<RateLimiter>, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.headers().contains_key("x-forwarded-for")
        && let Err(e) = limiter.check_local(&crate::extract::client_ip(req.headers()))
    {
        return crate::error::ApiError(e).into_response();
    }
    next.run(req).await
}

/// Keeps the turn a request takes among its workspace's (`Auth`) until its response is made, and says how
/// long it waited for it, so that a slow answer can be told from a slow server.
async fn hold_place(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let place = crate::state::Place::default();
    req.extensions_mut().insert(place.clone());
    let mut response = next.run(req).await;
    if let Some(waited) = place.waited().filter(|w| *w >= Duration::from_millis(1)) {
        response.headers_mut().insert("x-ferrum-waited-ms", axum::http::HeaderValue::from(waited.as_millis() as u64));
    }
    response
}

/// A call marked as coming from an app's screen (`X-Ferrum-App`) goes only where screens may, on
/// every route whatever its extractor; `Auth` then narrows its rights (extract.rs).
async fn screen_paths(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    if let Some(app) = req.headers().get("x-ferrum-app") {
        let app = app.to_str().unwrap_or_default();
        if !crate::extract::screen_may_call(app, req.uri().path()) {
            let msg = format!("screens of {app} may not call {}", req.uri().path());
            return axum::response::IntoResponse::into_response(crate::error::ApiError(ferrum_kernel::Error::Forbidden(msg)));
        }
    }
    next.run(req).await
}

/// An isolated app's origin of its own (`{app}--{workspace}.{apps_base}`, docs/plan/apps-ui.md
/// shape D) serves that app's files and, of the server's own paths, only the API its app tokens
/// reach and the token endpoint: no cookie is ever read or set there, and no sign-in,
/// administration or other app's endpoint answers.
async fn app_origins(apps_base: Arc<str>, mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::http::header;
    if apps_base.is_empty() {
        return next.run(req).await;
    }
    let host_app = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).and_then(|h| crate::shell::app_origin_of(h, &apps_base));
    let Some((app, _)) = host_app else { return next.run(req).await };
    req.headers_mut().remove(header::COOKIE);
    let path = req.uri().path();
    let server_paths = ["/api", "/oauth", "/ws", "/files", "/mcp", "/.well-known", "/metrics", "/_shell", "/_ui", "/_isolated", "/app", "/assistant"];
    let server_path = server_paths.iter().any(|p| path == *p || path.starts_with(&format!("{p}/")));
    let allowed = path.starts_with("/api/v1/")
        || path.starts_with("/api/meta/")
        || path.starts_with(&format!("/api/ext/{app}/"))
        || matches!(path, "/oauth/token" | "/oauth/revoke");
    if server_path && !allowed {
        return axum::response::IntoResponse::into_response(crate::error::ApiError(ferrum_kernel::Error::NotFound(format!("no such endpoint here: {path}"))));
    }
    let mut res = next.run(req).await;
    res.headers_mut().remove(header::SET_COOKIE);
    res
}

/// Isolated interfaces (an opaque origin: `Origin: null`) may call the API directly with an app
/// token, on the paths apps may call (docs/plan/apps-ui.md §12.3): CORS without credentials, so
/// no cookie goes along and an answer is of use only to a caller that holds a token.
async fn isolated_cors(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::http::{HeaderValue, Method, StatusCode, header};
    let path = req.uri().path();
    let app_path = path.starts_with("/api/v1/") || path.starts_with("/api/meta/") || path.starts_with("/api/ext/");
    let opaque = req.headers().get(header::ORIGIN).is_some_and(|o| o.as_bytes() == b"null");
    if !(app_path && opaque) {
        return next.run(req).await;
    }
    let preflight = req.method() == Method::OPTIONS && req.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut res = if preflight { axum::response::IntoResponse::into_response(StatusCode::NO_CONTENT) } else { next.run(req).await };
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    if preflight {
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE"));
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type, if-match, idempotency-key, idempotency-digest"));
        h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    }
    res
}

/// API answers belong to one user and can change without a new version (an engine-set status, a
/// recorded IRN): browsers must not reuse them. Handlers that do want caching set their own
/// `Cache-Control`.
async fn api_no_store(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let api = req.uri().path().starts_with("/api/");
    let mut res = next.run(req).await;
    if api && !res.headers().contains_key(axum::http::header::CACHE_CONTROL) {
        res.headers_mut().insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("no-store"));
    }
    res
}

/// `'inline-speculation-rules'` (the bar prerenders the next app with inline speculation rules) only
/// for the engines that have them: Chromium's. Safari reports the unknown keyword on every page.
async fn speculation_csp(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let chromium =
        req.headers().get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).is_some_and(|ua| ua.contains("Chrome/") || ua.contains("Chromium/"));
    let mut res = next.run(req).await;
    if !chromium
        && let Some(csp) = res.headers().get("content-security-policy").and_then(|v| v.to_str().ok())
        && csp.contains(" 'inline-speculation-rules'")
        && let Ok(v) = axum::http::HeaderValue::from_str(&csp.replace(" 'inline-speculation-rules'", ""))
    {
        res.headers_mut().insert("content-security-policy", v);
    }
    res
}

/// The push services notifications go to (`settings.push_services`): the browsers' own unless
/// the operator lists others.
/// The operator's plans (`FERRUM_PLANS`, JSON: `{"free": {"people": 5, "storage_bytes": 1073741824}}`)
/// and who may make a workspace (`FERRUM_SIGNUP=open`, with `FERRUM_SIGNUP_DOMAINS`,
/// `FERRUM_SIGNUP_PACKAGES`, `FERRUM_SIGNUP_PLAN` and `FERRUM_SIGNUP_PER_ADDRESS`). Sign-up needs
/// a base domain: its codes come from the accounts host, and each workspace gets its subdomain.
fn signup_settings(
    env: impl Fn(&str) -> Option<String>,
    has_base: bool,
) -> std::result::Result<(serde_json::Map<String, serde_json::Value>, ferrum_kernel::signup::SignupPolicy), String> {
    let list = |name: &str| -> Vec<String> { env(name).unwrap_or_default().split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect() };
    let plans: serde_json::Map<String, serde_json::Value> = match env("FERRUM_PLANS").filter(|p| !p.trim().is_empty()) {
        Some(p) => serde_json::from_str(&p).map_err(|e| format!("FERRUM_PLANS is not a JSON object of plans: {e}"))?,
        None => serde_json::Map::new(),
    };
    ferrum_kernel::plans::validate(&plans).map_err(|e| format!("FERRUM_PLANS: {e}"))?;
    let open = match env("FERRUM_SIGNUP").unwrap_or_default().trim() {
        "" | "closed" => false,
        "open" => true,
        other => return Err(format!("FERRUM_SIGNUP is open or closed, not {other:?}")),
    };
    if open && !has_base {
        return Err("FERRUM_SIGNUP=open needs FERRUM_BASE_DOMAIN: sign-up's codes come from the accounts host".into());
    }
    let plan = env("FERRUM_SIGNUP_PLAN").map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    if let Some(p) = &plan
        && !plans.contains_key(p)
    {
        return Err(format!("FERRUM_SIGNUP_PLAN {p:?} is not one of FERRUM_PLANS"));
    }
    let mut policy = ferrum_kernel::signup::SignupPolicy {
        open,
        domains: list("FERRUM_SIGNUP_DOMAINS"),
        packages: list("FERRUM_SIGNUP_PACKAGES"),
        plan,
        ..Default::default()
    };
    if let Some(n) = env("FERRUM_SIGNUP_PER_ADDRESS").filter(|n| !n.trim().is_empty()) {
        policy.per_address = n.trim().parse().ok().filter(|n| *n > 0).ok_or("FERRUM_SIGNUP_PER_ADDRESS is a whole number above 0")?;
    }
    Ok((plans, policy))
}

/// The pools of the last set-up of this process (see `is_start`).
static EARLIER_SET_UP: std::sync::Mutex<Option<(Db, sea_orm::DatabaseConnection)>> = std::sync::Mutex::new(None);

/// `ferrum start` (the server binary, not a test's): the command whose set-up Loco does twice.
fn is_start(args: impl IntoIterator<Item = String>) -> bool {
    let args: Vec<String> = args.into_iter().collect();
    let server = args.first().and_then(|a| Path::new(a).file_name()).is_some_and(|n| n == "ferrum");
    let mut rest = args.iter().skip(1);
    let command = loop {
        match rest.next() {
            Some(a) if a == "-e" || a == "--environment" => {
                rest.next();
            }
            Some(a) if a.starts_with('-') => {}
            other => break other,
        }
    };
    server && command.is_some_and(|c| c == "start")
}

/// Whether the command line runs `ferrum task migrate`.
fn is_migrate_task(args: impl IntoIterator<Item = String>) -> bool {
    let args: Vec<String> = args.into_iter().collect();
    args.windows(2).any(|w| w[0] == "task" && w[1] == "migrate")
}

fn push_services(listed: &str) -> Vec<String> {
    let listed: Vec<String> = listed.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if listed.is_empty() { ferrum_kernel::webpush::PUSH_SERVICES.iter().map(|s| s.to_string()).collect() } else { listed }
}

/// The Transit key that unwraps keys configured as ciphertext (`FERRUM_KMS_TRANSIT`), signing in as
/// the environment says (crates/kernel/src/kms.rs).
pub fn kms_transit(settings: &Settings) -> Result<Option<ferrum_kernel::kms::Transit>> {
    let url = settings.kms_transit.trim();
    if url.is_empty() {
        return Ok(None);
    }
    ferrum_kernel::kms::Transit::from_env(url, |name| std::env::var(name).ok()).map(Some).map_err(|e| Error::Message(e.to_string()))
}

/// The key sealing secret fields: printed by `FERRUM_SECRET_KEY_COMMAND`, or configured
/// (`FERRUM_SECRET_KEY`, itself or as Transit ciphertext), required in production; in development
/// derived from the JWT secret so secret fields work out of the box.
async fn secret_key(
    settings: &Settings,
    transit: Option<&ferrum_kernel::kms::Transit>,
    jwt_secret: &str,
    env: &Environment,
) -> Result<ferrum_kernel::secret::SecretKey> {
    if !settings.secret_key_command.trim().is_empty() {
        return ferrum_kernel::kms::from_command(&settings.secret_key_command).await.map_err(|e| Error::Message(e.to_string()));
    }
    if !settings.secret_key.trim().is_empty() {
        return ferrum_kernel::kms::resolve(&settings.secret_key, transit).await.map_err(|e| Error::Message(format!("settings.secret_key: {e}")));
    }
    if *env == Environment::Production {
        return Err(Error::Message("FERRUM_SECRET_KEY (or FERRUM_SECRET_KEY_COMMAND) is required in production (openssl rand -base64 32)".into()));
    }
    use sha2::Digest;
    Ok(ferrum_kernel::secret::SecretKey::new(sha2::Sha256::digest(format!("ferrum-secret-fields:{jwt_secret}")).into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_migrate_task_is_found_on_the_command_line() {
        let args = |a: &[&str]| super::is_migrate_task(a.iter().map(|s| s.to_string()));
        assert!(args(&["ferrum", "task", "migrate"]));
        assert!(args(&["ferrum", "--environment", "production", "task", "migrate"]));
        assert!(!args(&["ferrum", "start"]));
        assert!(!args(&["ferrum", "task", "doctor", "migrate"]));
    }

    #[test]
    fn the_start_command_is_the_server_binarys() {
        let args = |a: &[&str]| super::is_start(a.iter().map(|s| s.to_string()));
        assert!(args(&["/app/ferrum", "start"]));
        assert!(args(&["ferrum", "--environment", "production", "start", "--no-banner"]));
        assert!(args(&["ferrum", "-e", "production", "start"]));
        assert!(!args(&["ferrum", "task", "start"]));
        assert!(!args(&["ferrum", "task", "migrate"]));
        // A test binary given `start` as its filter: its set-ups are each a test's own.
        assert!(!args(&["/target/debug/deps/apps_api-0123abcd", "start"]));
    }

    use super::*;

    /// Round trip through an S3-compatible store. Runs when `FERRUM_TEST_S3_ENDPOINT` is set (CI
    /// starts an S3 emulator); `FERRUM_TEST_S3_BUCKET`, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` go with it.
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
    fn signup_and_plans_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        let (plans, policy) = signup_settings(env(&[]), false).unwrap();
        assert!(plans.is_empty() && !policy.open, "closed, with no plans, unless told");
        let (plans, policy) = signup_settings(
            env(&[
                ("FERRUM_PLANS", r#"{"free": {"people": 3}, "team": {"people": 25, "storage_bytes": 10737418240}}"#),
                ("FERRUM_SIGNUP", "open"),
                ("FERRUM_SIGNUP_DOMAINS", "Example.com, x.test"),
                ("FERRUM_SIGNUP_PACKAGES", "accounting,selling"),
                ("FERRUM_SIGNUP_PLAN", "free"),
                ("FERRUM_SIGNUP_PER_ADDRESS", "2"),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(plans.len(), 2);
        assert!(policy.open);
        assert_eq!(policy.domains, ["example.com", "x.test"]);
        assert_eq!(policy.packages, ["accounting", "selling"]);
        assert_eq!(policy.plan.as_deref(), Some("free"));
        assert_eq!(policy.per_address, 2);
        assert!(signup_settings(env(&[("FERRUM_SIGNUP", "open")]), false).is_err(), "no base domain");
        assert!(signup_settings(env(&[("FERRUM_SIGNUP", "yes")]), true).is_err());
        assert!(signup_settings(env(&[("FERRUM_SIGNUP_PLAN", "gold")]), true).is_err(), "not one of the plans");
        assert!(signup_settings(env(&[("FERRUM_PLANS", r#"{"free": {"seats": 3}}"#)]), true).is_err(), "not a quota");
        assert!(signup_settings(env(&[("FERRUM_PLANS", "[1]")]), true).is_err());
        assert!(signup_settings(env(&[("FERRUM_SIGNUP_PER_ADDRESS", "0")]), true).is_err());
    }

    #[test]
    fn local_files_need_an_explicit_opt_out_when_object_storage_is_required() {
        let settings: Settings = serde_json::from_value(serde_json::json!({"require_object_storage": true})).unwrap();
        assert!(build_storage(&settings).is_err());
    }

    #[tokio::test]
    async fn an_apps_origin_reaches_its_own_api_without_cookies() {
        use axum::http::header;
        use tower::ServiceExt;
        let base: Arc<str> = Arc::from("apps.example");
        // Answers whether a cookie arrived, and always tries to set one.
        let echo = |headers: axum::http::HeaderMap| async move {
            ([(header::SET_COOKIE, "ferrum_session=x; Path=/")], if headers.contains_key(header::COOKIE) { "cookie" } else { "none" })
        };
        let app = axum::Router::new()
            .route("/", axum::routing::get(echo))
            .route("/{*path}", axum::routing::get(echo).post(echo))
            .layer(axum::middleware::from_fn(move |req, next| app_origins(base.clone(), req, next)));
        let call = |host: &str, method: &str, path: &str| {
            let req = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, host)
                .header(header::COOKIE, "ferrum_session=secret")
                .body(axum::body::Body::empty())
                .unwrap();
            let app = app.clone();
            async move {
                let res = app.oneshot(req).await.unwrap();
                let (status, set_cookie) = (res.status().as_u16(), res.headers().contains_key(header::SET_COOKIE));
                let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
                (status, String::from_utf8_lossy(&body).to_string(), set_cookie)
            }
        };
        let app_host = "cards--acme.apps.example";
        for (method, path) in [
            ("GET", "/"),
            ("GET", "/board/42"),
            ("GET", "/api/v1/Customer"),
            ("GET", "/api/meta/entities"),
            ("GET", "/api/ext/cards/stats"),
            ("POST", "/oauth/token"),
        ] {
            assert_eq!(call(app_host, method, path).await, (200, "none".to_string(), false), "{method} {path}: no cookie in or out");
        }
        for path in [
            "/api/auth/me",
            "/api/admin/users",
            "/api/ext/other/x",
            "/api/shell/app-token",
            "/oauth/authorize",
            "/ws/live",
            "/files/x",
            "/mcp",
            "/_shell/v1/runtime.js",
            "/_ui/cards/d/a.js",
            "/_isolated/cards/d/index.html",
            "/.well-known/oauth-authorization-server",
        ] {
            assert_eq!(call(app_host, "GET", path).await.0, 404, "{path} is not the app's");
        }
        assert_eq!(call("acme.example", "GET", "/api/auth/me").await, (200, "cookie".to_string(), true), "workspace hosts are untouched");
        assert_eq!(call("apps.example", "GET", "/api/auth/me").await.1, "cookie", "the apps domain itself is no app's origin");
    }

    #[tokio::test]
    async fn each_address_through_the_proxy_has_a_budget() {
        use tower::ServiceExt;
        let limiter = Arc::new(RateLimiter::local_only("address", Duration::from_secs(60), 2));
        let app = axum::Router::new()
            .route("/api/x", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(move |req, next| per_address(limiter.clone(), req, next)));
        let call = |forwarded: Option<&str>| {
            let mut req = axum::http::Request::get("/api/x");
            if let Some(v) = forwarded {
                req = req.header("x-forwarded-for", v);
            }
            let app = app.clone();
            async move { app.oneshot(req.body(axum::body::Body::empty()).unwrap()).await.unwrap().status().as_u16() }
        };
        assert_eq!((call(Some("203.0.113.7")).await, call(Some("203.0.113.7")).await), (200, 200));
        assert_eq!(call(Some("203.0.113.7")).await, 429);
        assert_eq!(call(Some("198.51.100.1, 203.0.113.7")).await, 429, "an address the client made up does not help: the proxy's own entry counts");
        assert_eq!(call(Some("203.0.113.7, 203.0.113.8")).await, 200, "another address");
        for _ in 0..10 {
            assert_eq!(call(None).await, 200, "not through the proxy: not counted");
        }
    }
}

#[cfg(test)]
mod trace_tests {
    use super::valid_traceparent;

    #[test]
    fn only_a_well_formed_traceparent_is_kept() {
        assert!(valid_traceparent("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"));
        for bad in [
            "",
            "00-abc-def-01",
            "00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extra",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-0g",
        ] {
            assert!(!valid_traceparent(bad), "{bad}");
        }
    }
}
