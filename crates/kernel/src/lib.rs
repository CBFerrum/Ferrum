//! Ferrum kernel: a metadata-driven document engine on PostgreSQL.
//!
//! The `Kernel` owns the pool, the metadata registry and the hook registry. Everything a
//! tenant can do (CRUD, lifecycle, customization, automation) goes through it, with tenant
//! isolation enforced twice: by the code (scopes) and by Postgres row-level security.

pub mod audit;
pub mod auth;
pub mod automation;
pub mod ctx;
pub mod customize;
pub mod db;
pub mod ddl;
pub mod describe;
pub mod doc;
pub mod error;
pub mod hooks;
pub mod numbering;
pub mod outbox;
pub mod print;
pub mod query;
pub mod registry;
pub mod store;
pub mod tenant;
pub mod value;

pub use ctx::{Access, Action, Ctx, PermSet, row_scope};
pub use db::{Db, DbConfig, Tx, with_retry};
pub use error::{Error, FieldError, Result};
pub use hooks::{DocHook, HookCtx, HookRegistry};
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
}

#[derive(Debug, Clone)]
pub struct KernelSettings {
    /// Allow webhooks to private/loopback addresses (development only).
    pub webhooks_allow_private: bool,
    /// How long a user's roles/permissions are cached per replica.
    pub ctx_ttl: Duration,
    /// Processed outbox events and delivered webhooks are kept this long.
    pub event_retention_days: i32,
}

impl Default for KernelSettings {
    fn default() -> Self {
        KernelSettings { webhooks_allow_private: false, ctx_ttl: Duration::from_secs(30), event_retention_days: 30 }
    }
}

pub struct Kernel {
    pub db: Db,
    pub registry: Arc<Registry>,
    pub hooks: HookRegistry,
    pub settings: KernelSettings,
    pub mailer: Option<Arc<dyn Mailer>>,
    pub(crate) users: DashMap<(Uuid, Uuid), (Arc<auth::CtxCore>, Instant)>,
}

impl Kernel {
    /// Migrates the kernel schema, syncs entity tables for the catalog, and returns a ready kernel.
    pub async fn boot(db: Db, catalog: Catalog, hooks: HookRegistry, settings: KernelSettings, mailer: Option<Arc<dyn Mailer>>) -> Result<Arc<Kernel>> {
        db.migrate().await?;
        let report = ddl::sync(&db, &catalog, false).await?;
        if !report.skipped {
            tracing::info!(statements = report.statements, sql = ?report.sql_files_applied, warnings = report.warnings.len(), "schema synced");
        }
        let registry = Arc::new(Registry::new(Arc::new(catalog), db.clone()));
        Ok(Arc::new(Kernel { db, registry, hooks, settings, mailer, users: DashMap::new() }))
    }

    /// Background work every replica runs: metadata listener, outbox, webhooks, maintenance.
    /// All of it is safe to run concurrently on many replicas (no single point of failure).
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
        vec![self.registry.spawn_listener(shutdown.clone()), self.spawn_outbox_dispatcher(shutdown.clone()), self.spawn_webhook_sender(shutdown), maintenance]
    }

    /// Hourly: anchor yesterday's audit trail, purge expired events and idempotency keys.
    pub async fn maintenance(&self) -> Result<()> {
        let yesterday = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
        audit::anchor_day(&self.db, yesterday).await?;
        let purged = self.purge_old_events(self.settings.event_retention_days).await?;
        if purged > 0 {
            tracing::info!(purged, "old events purged");
        }
        Ok(())
    }

    /// Readiness: database reachable and migrations applied.
    pub async fn health(&self) -> Result<()> {
        self.db.ping().await
    }
}
