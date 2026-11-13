//! Ferrum kernel: a metadata-driven document engine on PostgreSQL.
//!
//! The `Kernel` owns the pool, the metadata registry and the hook registry. Everything a
//! tenant can do (CRUD, lifecycle, customization, automation) goes through it, with tenant
//! isolation enforced twice: by the code (scopes) and by Postgres row-level security.

pub mod access;
pub mod account;
pub mod action_link;
pub mod actions;
pub mod approvals;
pub mod apps;
pub mod audit;
pub mod auth;
pub mod authoring;
pub mod automation;
pub mod backfill;
pub mod bulk;
pub mod capability;
pub mod coedit;
pub mod collab;
pub mod connect;
pub mod connections;
pub mod ctx;
pub mod customize;
pub mod db;
pub mod ddl;
pub mod describe;
pub mod doc;
pub mod docfmt;
pub mod doctor;
pub mod documents;
pub mod error;
pub mod evidence;
pub mod export;
pub mod files;
pub mod hooks;
pub mod identity;
pub mod jobs;
pub mod kms;
pub mod layout;
pub mod lifecycle;
mod links;
pub mod logic;
pub mod merge;
pub mod messages;
pub mod notify;
pub mod numbering;
pub mod oauth;
pub mod outbound;
pub mod outbox;
pub mod pack;
pub mod passkey;
pub mod pdf;
pub mod plans;
pub mod portal_signup;
pub mod prefs;
pub mod print;
pub mod privacy;
pub mod process;
pub mod published;
pub mod query;
pub mod receipt;
pub mod recovery;
pub mod registry;
pub mod resources;
pub mod retention;
pub mod richtext;
pub mod saga;
pub mod scan;
pub mod schedule;
pub mod scim;
pub mod secret;
pub mod service;
pub mod settings;
pub mod shell;
pub mod signup;
pub mod sso;
pub mod store;
pub mod support;
pub mod targets;
pub mod tenant;
pub mod twofactor;
pub mod value;
pub mod views;
pub mod webpush;
pub mod window;

pub use ctx::{Access, Action, Ctx, PermSet, row_scope};
pub use db::{Db, DbConfig, Trace, Tx, with_retry};
pub use error::{Error, FieldError, Result};
pub use ferrum_logic::calendar::Calendar;
pub use hooks::{ApprovalDecided, DocAccess, DocHook, DocHost, EventSink, HookCtx, HookRegistry, Site, SiteRequest, SiteResponse};
pub use process::load_calendar;
pub use query::{Filter, ListQuery, Op, Page};
pub use registry::Registry;

use async_trait::async_trait;
use dashmap::DashMap;
use ferrum_meta::Catalog;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

/// A document: field name → canonical JSON value (child tables are arrays of row objects).
pub type Doc = serde_json::Map<String, serde_json::Value>;

#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<()>;

    /// Whether [`send_with`](Self::send_with) can attach files.
    fn can_attach(&self) -> bool {
        false
    }

    /// Sends with files attached. Mailers that cannot attach refuse, so nothing goes out without them.
    async fn send_with(&self, to: &str, subject: &str, html: &str, attachments: &[MailAttachment]) -> Result<()> {
        if attachments.is_empty() {
            return self.send(to, subject, html).await;
        }
        Err(Error::Unavailable("this mailer cannot attach files".into()))
    }
}

/// A file attached to an e-mail.
#[derive(Debug, Clone)]
pub struct MailAttachment {
    pub file_name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct KernelSettings {
    /// Allow webhooks to private/loopback addresses (development only).
    pub webhooks_allow_private: bool,
    /// Hosts the operator lets connections reach on a private address or over plain http
    /// (`FERRUM_CONNECTION_HOSTS`, comma-separated): a service inside the operator's own network.
    pub connection_hosts: Vec<String>,
    /// The largest stored file a connection sends in chunks (`FERRUM_CONNECTION_UPLOAD_MB`; 25 MB unless the operator
    /// says): read a chunk at a time, never whole.
    pub connection_upload_bytes: u64,
    /// Allow single sign-on providers on private addresses, and over plain http (an identity
    /// provider inside the operator's own network; development).
    pub sso_allow_private: bool,
    /// How long a user's roles/permissions are cached per replica.
    pub ctx_ttl: Duration,
    /// Processed outbox events and delivered webhooks are kept this long.
    pub event_retention_days: i32,
    /// Seals secret fields; without it they cannot be set or read.
    pub secret_key: Option<secret::SecretKey>,
    /// The ClamAV daemon uploads pass through (`host:port`); without one they are stored unscanned.
    pub clamd: Option<String>,
    /// The scanner's `StreamMaxLength`: files past it are stored unscanned.
    pub clamd_max_bytes: u64,
    /// Keys being rotated out: values they sealed still open, until [`Kernel::reseal_secrets`] has
    /// moved them to `secret_key`.
    pub retired_secret_keys: Vec<secret::SecretKey>,
    /// Base URL of a Gotenberg-compatible PDF renderer; without it there are no PDFs.
    pub pdf_url: Option<String>,
    /// The web app's address as people reach it (`PUBLIC_URL`), for links in mail.
    pub public_url: Option<String>,
    /// The push services the server sends notifications to (webpush.rs): hosts (their subdomains
    /// too), or exact origins such as `http://127.0.0.1:9000`.
    pub push_services: Vec<String>,
    /// Who push services may contact about this server's pushes (`mailto:` or `https:`); the
    /// public address when it is https.
    pub push_contact: Option<String>,
    /// Treat a new timeline as a restore, which it can be. Turn off only where failover is synchronous,
    /// so that promoting a replica loses nothing and a person need not look (docs/operations.md).
    pub hold_on_timeline: bool,
    /// The accounts host (`accounts.{base}`), where sign-ins may come back to as well as to a
    /// workspace's own hosts and the public address.
    pub accounts_host: Option<String>,
    /// The origin other services call back (`https://{FERRUM_HOOKS_HOST}`), when it is set on its
    /// own; else they call the public address ([`KernelSettings::callback_base`]).
    pub hooks_url: Option<String>,
    /// The passkeys' relying party for the hosts under it (`FERRUM_PASSKEY_RP`); none: each host is
    /// its own ([`crate::passkey::rp_for`]).
    pub passkey_rp: Option<String>,
    /// The host every link that leaves the system for a person goes through (`FERRUM_LINK_HOST`):
    /// `/go/{workspace}/{s|p}/…` there leads on to the workspace's current host ([`Kernel::person_link`]).
    pub link_host: Option<String>,
    /// The operator's plans by name: each a set of quotas ([`plans`]).
    pub plans: serde_json::Map<String, serde_json::Value>,
    /// Who may make a workspace of their own ([`signup`]).
    pub signup: signup::SignupPolicy,
    /// How long a chain of events may grow before it is stopped and kept as a dead event saying why:
    /// steps from the change that started it, and events that followed from that change (FLC-HOOK-008).
    pub chain_depth: i16,
    pub chain_events: i32,
    /// A durable process runtime runs this installation's processes (crates/process): releases compile under
    /// the Full Process profile. Without one, a solution with a process is refused (docs/logic/profiles.md).
    pub process_runtime: bool,
    /// Records one retention rule reaches in a daily run; the next run continues after them.
    pub retention_daily_records: usize,
    /// Migrate the database and sync the schema as the kernel boots (the default). Off, boot changes no schema and
    /// refuses to start on a database `ferrum task migrate` has not brought to this build ([`Kernel::boot`]).
    pub migrate_at_start: bool,
}

impl KernelSettings {
    /// Where other services call back (an OAuth connector's return address, the addresses they pull
    /// files from): the hooks host when it is set, else the public address; none without either.
    /// Every callback address the kernel builds starts with it.
    pub fn callback_base(&self) -> Option<String> {
        self.hooks_url.as_deref().or(self.public_url.as_deref()).map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty())
    }
}

#[cfg(test)]
mod callback_tests {
    #[test]
    fn callbacks_come_to_the_hooks_host_when_it_is_set() {
        let mut s = super::KernelSettings { public_url: Some("https://erp.acme.com/".into()), ..Default::default() };
        assert_eq!(s.callback_base().as_deref(), Some("https://erp.acme.com"), "as before");
        s.hooks_url = Some("https://hooks.acme.com".into());
        assert_eq!(s.callback_base().as_deref(), Some("https://hooks.acme.com"));
        assert_eq!(super::KernelSettings::default().callback_base(), None);
    }
}

impl Default for KernelSettings {
    fn default() -> Self {
        KernelSettings {
            webhooks_allow_private: false,
            connection_hosts: Vec::new(),
            connection_upload_bytes: 25 << 20,
            sso_allow_private: false,
            ctx_ttl: Duration::from_secs(30),
            event_retention_days: 30,
            secret_key: None,
            clamd: None,
            clamd_max_bytes: 25 << 20,
            retired_secret_keys: Vec::new(),
            pdf_url: None,
            public_url: None,
            push_services: webpush::PUSH_SERVICES.iter().map(|s| s.to_string()).collect(),
            push_contact: None,
            hold_on_timeline: true,
            accounts_host: None,
            hooks_url: None,
            passkey_rp: None,
            link_host: None,
            plans: serde_json::Map::new(),
            signup: signup::SignupPolicy::default(),
            chain_depth: 8,
            chain_events: 1000,
            process_runtime: false,
            retention_daily_records: 5000,
            migrate_at_start: true,
        }
    }
}

/// The refusal of a start with `migrate_at_start` off on a database that is behind this build.
fn not_migrated(what: &str) -> Error {
    Error::Invalid(format!(
        "the database is not ready for this build: {what}. Run `ferrum task migrate` with this build and the same environment, \
         or start with FERRUM_MIGRATE_AT_START=true to migrate as the server starts"
    ))
}

/// What booting did to the database (`ferrum task migrate` prints it).
#[derive(Debug, Default, Clone)]
pub struct BootReport {
    /// Versions of the SQL migrations it applied.
    pub migrations: Vec<i64>,
    pub schema: ddl::SyncReport,
    /// A newer build migrated the database and lets this one run on it: this one changed nothing ([`window`]).
    pub guest: Option<window::Recorded>,
}

pub struct Kernel {
    pub db: Db,
    pub booted: BootReport,
    pub registry: Arc<Registry>,
    pub hooks: HookRegistry,
    pub settings: KernelSettings,
    pub mailer: Option<Arc<dyn Mailer>>,
    pub(crate) users: DashMap<(Uuid, Uuid), (Arc<auth::CtxCore>, Instant)>,
    /// Browser sessions checked recently: (tenant, session) → (what a valid one adds, when checked).
    pub(crate) sessions: DashMap<(Uuid, Uuid), (Option<(Uuid, shell::SessionState)>, Instant)>,
    /// Host names looked up recently → where they lead.
    pub(crate) hosts: DashMap<String, (shell::HostRoute, Instant)>,
    pub(crate) files: std::sync::OnceLock<Arc<dyn files::FileStore>>,
    /// The directory workspace, once known (identity.rs).
    pub(crate) directory: std::sync::OnceLock<Uuid>,
    /// The server's VAPID key, once read (webpush.rs).
    pub(crate) vapid: tokio::sync::OnceCell<Arc<webpush::Vapid>>,
    /// Submits, cancels, actions and workflow steps that did not go through, since the process
    /// started: (package, entity, operation, kind) → how many ([`Kernel::failures`]).
    pub(crate) failures: std::sync::Mutex<std::collections::BTreeMap<(String, String, String, &'static str), u64>>,
    /// What service targets take as now when a test pins it (targets.rs).
    pub(crate) clock: std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>,
}

impl Kernel {
    /// Counts an operation on a record that did not go through: `refused` when its own rules said
    /// no (a posting with no account to post to, a stale version), `fault` when something broke.
    /// Who may not do it, or a record that is not there, is not the operation failing.
    pub(crate) fn tally(&self, ctx: &Ctx, entity: &str, operation: &str, e: &Error) {
        let kind = match e {
            Error::NotFound(_) | Error::Forbidden(_) | Error::Unauthorized(_) | Error::TooManyRequests(_) | Error::Wait(..) => return,
            Error::Validation(_) | Error::Invalid(_) | Error::Conflict(_) | Error::Retryable(_) => "refused",
            _ => "fault",
        };
        let package = ctx.schema.entity(entity).map(|e| e.package.clone()).unwrap_or_default();
        if kind == "fault" {
            tracing::warn!(%package, %entity, %operation, "an operation failed: {e}");
        }
        if let Ok(mut f) = self.failures.lock() {
            *f.entry((package, entity.to_string(), operation.to_string(), kind)).or_default() += 1;
        }
    }

    /// The failures [`Self::tally`] counted, in order: (package, entity, operation, kind, count).
    pub fn failures(&self) -> Vec<(String, String, String, &'static str, u64)> {
        self.failures.lock().map(|f| f.iter().map(|((p, e, o, k), n)| (p.clone(), e.clone(), o.clone(), *k, *n)).collect()).unwrap_or_default()
    }

    /// Counts a request for `key` against `max` per fixed `window`, across every replica, and
    /// refuses it past the limit (`message` says why). For apps' public endpoints; keys should
    /// name the app, e.g. `bookings|{tenant}|{ip}`.
    pub async fn rate_limit(&self, key: &str, window: std::time::Duration, max: u32, message: &str) -> Result<()> {
        if self.db.count_hit(key, window).await? > i64::from(max) {
            return Err(Error::TooManyRequests(message.into()));
        }
        Ok(())
    }

    /// Migrates the kernel schema, syncs entity tables for the catalog, and returns a ready kernel.
    pub async fn boot(db: Db, catalog: Catalog, mut hooks: HookRegistry, settings: KernelSettings, mailer: Option<Arc<dyn Mailer>>) -> Result<Arc<Kernel>> {
        // Customer logic's rules run in every workspace's saves, after the apps' own hooks (logic.rs).
        hooks.register("*", Arc::new(logic::TenantRules));
        // Active releases' processes start on, and wait for, the workspace's events (process.rs).
        hooks.add_event_sink(Arc::new(process::Processes));
        hooks.settle().map_err(|problems| Error::Invalid(format!("hooks cannot run as registered: {}", problems.join("; "))))?;
        // Superusers and BYPASSRLS roles ignore row-level security, which would silently remove
        // tenant isolation. Refuse to run that way.
        let (superuser, bypass): (bool, bool) =
            sqlx::query_as("select rolsuper, rolbypassrls from pg_roles where rolname = current_user").fetch_one(&db.pool).await?;
        if superuser || bypass {
            return Err(Error::Invalid(
                "the database role is a superuser or has BYPASSRLS, which bypasses row-level security and with it tenant \
                 isolation; connect as an ordinary role that owns the database (see docs/operations.md)"
                    .into(),
            ));
        }
        let mut booted = BootReport::default();
        // The platform's packages plus those of apps made available on this cluster.
        let combine = |db: &Db| {
            let db = db.clone();
            let own = catalog.packages().cloned().collect::<Vec<_>>();
            async move {
                let (full, skipped) = Registry::combine(&own, Registry::app_packages(&db).await?);
                for (app, why) in skipped {
                    tracing::error!(app = %app, "app left out of the catalog: {why}");
                }
                Ok::<_, Error>(full)
            }
        };
        // The two-version window (window.rs): a database a newer build migrated is first asked whether this build may run on
        // it, as a guest that changes nothing; only a database that is this build's or behind it is migrated or checked.
        let full;
        if settings.migrate_at_start {
            // Held until the schema is synced: two builds of different ages starting together each decide on what the
            // other left, never between its migrating and its recording.
            let mut deciding = window::Deciding::lock(&db).await?;
            let decided = async {
                match deciding.standing(&db).await? {
                    window::Standing::Refused(why) => Err(Error::Invalid(why)),
                    window::Standing::Guest(rec) => {
                        let full = combine(&db).await?;
                        window::admit(&db, &full, rec).await?;
                        booted.guest = Some(rec);
                        Ok(full)
                    }
                    window::Standing::Level => {
                        let before = db.applied_migrations().await?;
                        db.migrate().await?;
                        booted.migrations = db.applied_migrations().await?.into_iter().filter(|v| !before.contains(v)).collect();
                        let full = combine(&db).await?;
                        // Records this build's date and COMPATIBLE_FROM too, even when nothing else changed.
                        booted.schema = ddl::sync(&db, &full, false).await?;
                        if !booted.schema.skipped {
                            tracing::info!(statements = booted.schema.statements, sql = ?booted.schema.sql_files_applied, warnings = booted.schema.warnings.len(), "schema synced");
                        }
                        Ok(full)
                    }
                }
            }
            .await;
            deciding.release().await;
            full = decided?;
        } else {
            // Another step has brought the database here (`ferrum task migrate`): this only checks that it did.
            let mut conn = db.pool.acquire().await?;
            let standing = window::standing(window::build_date(), window::dated(), &db.unknown_migrations().await?, window::recorded(&mut conn).await?);
            drop(conn);
            match standing {
                window::Standing::Refused(why) => return Err(Error::Invalid(why)),
                window::Standing::Guest(rec) => {
                    full = combine(&db).await?;
                    window::admit(&db, &full, rec).await?;
                    booted.guest = Some(rec);
                }
                window::Standing::Level => {
                    let pending = db.pending_migrations().await?;
                    if !pending.is_empty() {
                        return Err(not_migrated(&format!(
                            "{} of this build's database migrations are not applied (the first is {})",
                            pending.len(),
                            pending[0]
                        )));
                    }
                    full = combine(&db).await?;
                    if !ddl::is_current(&db, &full).await? {
                        return Err(not_migrated("the schema was last synced for other packages than this build's"));
                    }
                }
            }
        }
        if booted.guest.is_some() || !settings.migrate_at_start {
            ddl::words(&full)?;
        }
        if let Some(rec) = booted.guest {
            tracing::warn!(
                newer = %rec.build,
                this = %window::build_date(),
                "a newer build has migrated the database: this server runs on it as a guest, and leaves the schema, the start-up steps and the newer build's work alone"
            );
        }
        let registry = Arc::new(Registry::new(&catalog, Arc::new(full), db.clone()));
        let kernel = Arc::new(Kernel {
            db,
            booted,
            registry,
            hooks,
            settings,
            mailer,
            users: DashMap::new(),
            sessions: DashMap::new(),
            hosts: DashMap::new(),
            files: std::sync::OnceLock::new(),
            directory: std::sync::OnceLock::new(),
            vapid: tokio::sync::OnceCell::new(),
            failures: Default::default(),
            clock: Default::default(),
        });
        kernel.notice_restore().await?;
        // From this build's packages: a guest's are older than the database's (window.rs).
        match if kernel.booted.guest.is_some() { Ok(0) } else { kernel.adopt_customization_baselines().await } {
            Ok(0) => {}
            Ok(n) => tracing::info!(customizations = n, "customizations compared from here on with what their packages say now"),
            Err(e) => tracing::warn!("customizations' baselines: {e}"),
        }
        Ok(kernel)
    }

    /// Cache invalidation (metadata, users) — every replica that serves requests needs these, or it
    /// keeps answering from stale permissions and schemas.
    pub fn spawn_listeners(self: &Arc<Self>, shutdown: watch::Receiver<bool>) -> Vec<tokio::task::JoinHandle<()>> {
        vec![self.registry.spawn_listener(shutdown.clone()), self.spawn_user_listener(shutdown)]
    }

    /// Background work: outbox, webhooks, scheduled automations, jobs, maintenance. Safe to run concurrently on many replicas
    /// (no single point of failure); replicas that only serve requests may skip it.
    pub fn spawn_background(self: &Arc<Self>, shutdown: watch::Receiver<bool>) -> Vec<tokio::task::JoinHandle<()>> {
        let maintenance = {
            let k = self.clone();
            let mut shutdown = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = k.maintenance().await {
                        tracing::warn!("maintenance failed: {e}");
                    }
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
                    }
                }
            })
        };
        vec![
            self.spawn_outbox_dispatcher(shutdown.clone()),
            self.spawn_webhook_sender(shutdown.clone()),
            self.spawn_schedule_scanner(shutdown.clone()),
            self.spawn_job_runner(shutdown),
            maintenance,
        ]
    }

    /// Hourly: anchor yesterday's audit trail, purge expired events and idempotency keys.
    pub async fn maintenance(&self) -> Result<()> {
        let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
        audit::anchor_day(&self.db, yesterday).await?;
        if let Err(e) = self.queue_retention().await {
            tracing::warn!("queueing retention: {e}");
        }
        if let Err(e) = self.record_usage().await {
            tracing::warn!("recording usage: {e}");
        }
        if let Err(e) = self.expire_support().await {
            tracing::warn!("closing expired support access: {e}");
        }
        if let Err(e) = self.expire_roles().await {
            tracing::warn!("removing ended role grants: {e}");
        }
        if let Err(e) = self.start_backfills().await {
            tracing::warn!("starting backfills: {e}");
        }
        let purged = self.purge_old_events(self.settings.event_retention_days).await?
            + self.oauth_purge().await?
            + self.purge_sessions().await?
            + self.purge_exports().await?
            + self.purge_connection_events().await?
            + self.purge_connections().await?
            + self.purge_public_files().await?
            + self.prune_occurrences().await?;
        if purged > 0 {
            tracing::info!(purged, "old events purged");
        }
        // A failed digest waits for the next run; it must not stop the rest of the upkeep.
        match self.send_digests().await {
            Ok(0) => {}
            Ok(digests) => tracing::info!(digests, "notification digests queued"),
            Err(e) => tracing::warn!("notification digests failed: {e}"),
        }
        Ok(())
    }

    /// Readiness: database reachable and migrations applied.
    pub async fn health(&self) -> Result<()> {
        self.db.ping().await
    }
}
