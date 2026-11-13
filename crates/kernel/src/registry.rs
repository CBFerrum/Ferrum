//! Per-tenant effective-schema cache with LISTEN/NOTIFY invalidation and a TTL fallback.

use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use dashmap::DashMap;
use ferrum_meta::{Catalog, Overlay, OverlayOp, Schema, SchemaWarning, effective};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

pub const META_CHANNEL: &str = "ferrum_meta";

struct Cached {
    schema: Arc<Schema>,
    checked_at: Instant,
}

/// `META_CHANNEL` payload announcing that the cluster catalog changed (an app became available).
pub const CATALOG_CHANGED: &str = "catalog";

pub struct Registry {
    /// Platform packages (and native apps) this build ships; apps made available at runtime are
    /// added on top.
    base: Vec<ferrum_meta::Package>,
    catalog: std::sync::RwLock<Arc<Catalog>>,
    db: Db,
    cache: DashMap<Uuid, Cached>,
    ttl: Duration,
    /// How often each workspace's settings were seen to change, from this replica or another.
    changes: DashMap<Uuid, u64>,
    /// How often everything was dropped, a notification having perhaps been missed.
    epoch: AtomicU64,
}

impl Registry {
    pub fn new(base: &Catalog, catalog: Arc<Catalog>, db: Db) -> Self {
        Registry {
            base: base.packages().cloned().collect(),
            catalog: std::sync::RwLock::new(catalog),
            db,
            cache: DashMap::new(),
            ttl: Duration::from_secs(30),
            changes: DashMap::new(),
            epoch: AtomicU64::new(0),
        }
    }

    /// The cluster catalog: platform packages plus the packages of available apps.
    pub fn catalog(&self) -> Arc<Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Platform packages plus `apps`, validated together. An app whose packages no longer fit
    /// (say, after a platform upgrade) is left out with an error instead of stopping the cluster.
    pub fn combine(base: &[ferrum_meta::Package], apps: Vec<(String, Vec<ferrum_meta::Package>)>) -> (Catalog, Vec<(String, String)>) {
        let mut all = base.to_vec();
        let mut skipped = Vec::new();
        for (app, pkgs) in apps {
            let mut trial = all.clone();
            trial.extend(pkgs.iter().cloned());
            match Catalog::from_packages(trial) {
                Ok(_) => all.extend(pkgs),
                Err(e) => skipped.push((app, e.to_string())),
            }
        }
        let catalog = Catalog::from_packages(all).expect("each addition was validated");
        (catalog, skipped)
    }

    pub fn base(&self) -> &[ferrum_meta::Package] {
        &self.base
    }

    /// Installs a new cluster catalog on this replica; every tenant schema is rebuilt lazily.
    pub fn set_catalog(&self, catalog: Arc<Catalog>) {
        *self.catalog.write().unwrap_or_else(|e| e.into_inner()) = catalog;
        self.invalidate_all();
    }

    /// The tenant's effective schema; rebuilt only when its `meta_version` changed.
    pub async fn schema(&self, tenant: Uuid) -> Result<Arc<Schema>> {
        if let Some(c) = self.cache.get(&tenant)
            && c.checked_at.elapsed() < self.ttl
        {
            return Ok(c.schema.clone());
        }
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let row: Option<(i64, serde_json::Value)> =
            sqlx::query_as("select meta_version, settings from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
        let Some((version, settings)) = row else {
            return Err(Error::NotFound("tenant".into()));
        };
        if let Some(mut c) = self.cache.get_mut(&tenant)
            && c.schema.version == version
        {
            c.checked_at = Instant::now();
            return Ok(c.schema.clone());
        }
        let installed: Vec<String> =
            sqlx::query_scalar("select name from installed_package where tenant_id = $1 order by name").bind(tenant).fetch_all(&mut *tx).await?;
        // A package whose app was revoked or removed from the cluster drops out (its data stays)
        // instead of breaking the whole workspace.
        let catalog = self.catalog();
        let (installed, missing): (Vec<String>, Vec<String>) = installed.into_iter().partition(|p| catalog.package(p).is_some());
        let rows: Vec<(String, serde_json::Value, Option<serde_json::Value>)> =
            sqlx::query_as("select entity, op, base from meta_overlay where tenant_id = $1 and is_active order by seq")
                .bind(tenant)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let mut overlays = Vec::with_capacity(rows.len());
        let mut parse_warnings = Vec::new();
        let mut upstream = None;
        for (entity, op, base) in rows {
            match serde_json::from_value::<OverlayOp>(op) {
                Ok(op) => {
                    // A field its package has come to require stays required until someone decides.
                    if crate::customize::relaxes(&op, base.as_ref()) {
                        let packaged = match &upstream {
                            Some(s) => s,
                            None => upstream.insert(
                                effective::build(&catalog, &installed, &[], version)
                                    .map_err(|e| Error::Internal(format!("schema for tenant {tenant}: {e}")))?,
                            ),
                        };
                        if crate::customize::now_required(packaged, &entity, &op) {
                            parse_warnings.push(SchemaWarning {
                                entity,
                                message: "a customization that made a field optional is suspended: its package now requires it".into(),
                            });
                            continue;
                        }
                    }
                    overlays.push(Overlay { entity, ops: vec![op] })
                }
                Err(e) => parse_warnings.push(SchemaWarning { entity, message: format!("unreadable customization: {e}") }),
            }
        }
        let mut schema = effective::build(&catalog, &installed, &overlays, version).map_err(|e| Error::Internal(format!("schema for tenant {tenant}: {e}")))?;
        schema.warnings.extend(parse_warnings);
        for m in missing {
            schema.warnings.push(SchemaWarning { entity: m.clone(), message: format!("package {m} is installed but no longer available on this cluster") });
        }
        schema.fy_start_month = settings.get("fiscal_year_start_month").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 12) as u32;
        // Every workspace uses the shell: today's web app has left /app/ (docs/plan/apps-ui.md U4b).
        // The `shell_v2` setting stays as it was, for the release before, which still has it.
        schema.shell_v2 = true;
        schema.timezone = match settings.get("timezone").and_then(|v| v.as_str()) {
            Some(tz) if tz.parse::<chrono_tz::Tz>().is_ok() => tz.to_string(),
            Some(tz) => {
                schema.warnings.push(SchemaWarning { entity: "tenant".into(), message: format!("unknown time zone '{tz}'; using UTC") });
                "UTC".into()
            }
            None => "UTC".into(),
        };
        for w in &schema.warnings {
            tracing::warn!(%tenant, entity = %w.entity, "customization skipped: {}", w.message);
        }
        let schema = Arc::new(schema);
        self.cache.insert(tenant, Cached { schema: schema.clone(), checked_at: Instant::now() });
        Ok(schema)
    }

    pub fn invalidate(&self, tenant: Uuid) {
        self.cache.remove(&tenant);
        *self.changes.entry(tenant).or_insert(0) += 1;
    }

    pub fn invalidate_all(&self) {
        self.cache.clear();
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Where a workspace's settings stand as this replica knows: `(epoch, changes)`. Something read
    /// from them is current while this is unchanged; `changes` moving means they did change, `epoch`
    /// moving that they may have. Taken before the read it goes with.
    pub fn revision(&self, tenant: Uuid) -> (u64, u64) {
        (self.epoch.load(Ordering::SeqCst), self.changes.get(&tenant).map_or(0, |c| *c))
    }

    /// Call inside the transaction that changed a tenant's metadata; the notification fires on commit.
    pub async fn bump_version(tx: &mut Tx, tenant: Uuid) -> Result<()> {
        sqlx::query("update tenant set meta_version = meta_version + 1 where id = $1").bind(tenant).execute(&mut **tx).await?;
        sqlx::query("select pg_notify($1, $2)").bind(META_CHANNEL).bind(tenant.to_string()).execute(&mut **tx).await?;
        Ok(())
    }

    /// Listens for metadata changes from any replica. On connection loss the whole cache is
    /// dropped (notifications may have been missed) and the listener reconnects with backoff.
    pub fn spawn_listener(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            loop {
                if *shutdown.borrow() {
                    return;
                }
                match me.db.listen(META_CHANNEL).await {
                    Ok(mut listener) => {
                        backoff = Duration::from_millis(250);
                        if let Err(e) = me.reload_catalog().await {
                            tracing::error!("reloading the app catalog failed: {e}");
                        }
                        me.invalidate_all();
                        loop {
                            tokio::select! {
                                _ = shutdown.changed() => return,
                                msg = crate::db::notification(&mut listener) => match msg {
                                    Ok(n) if n.payload() == CATALOG_CHANGED => {
                                        if let Err(e) = me.reload_catalog().await {
                                            tracing::error!("reloading the app catalog failed: {e}");
                                        }
                                    }
                                    Ok(n) => match n.payload().parse::<Uuid>() {
                                        Ok(t) => me.invalidate(t),
                                        Err(_) => me.invalidate_all(),
                                    },
                                    Err(e) => {
                                        tracing::warn!("metadata listener lost connection: {e}");
                                        me.invalidate_all();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => tracing::warn!("metadata listener cannot connect: {e}"),
                }
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        })
    }
}

impl Registry {
    /// Packages of the apps currently available on the cluster, from the database.
    pub async fn app_packages(db: &Db) -> Result<Vec<(String, Vec<ferrum_meta::Package>)>> {
        let mut tx = db.system_tx().await?;
        let rows: Vec<(String, serde_json::Value)> =
            sqlx::query_as("select app_id, packages from app_release where status = 'active' and private_tenant is null order by created_at")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let mut out = Vec::new();
        for (app, packages) in rows {
            let mut pkgs = Vec::new();
            let files: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
                serde_json::from_value(packages).map_err(|e| Error::Internal(format!("app {app}: stored packages unreadable: {e}")))?;
            for (name, files) in files {
                pkgs.push(ferrum_meta::catalog::load_package_files(&files).map_err(|e| Error::Internal(format!("app {app}, package {name}: {e}")))?);
            }
            out.push((app, pkgs));
        }
        Ok(out)
    }

    /// Rebuilds this replica's catalog from the platform packages and the available apps.
    pub async fn reload_catalog(&self) -> Result<()> {
        let apps = Self::app_packages(&self.db).await?;
        let (catalog, skipped) = Self::combine(&self.base, apps);
        for (app, why) in skipped {
            tracing::error!(app = %app, "app left out of the catalog: {why}");
        }
        if catalog.fingerprint() != self.catalog().fingerprint() {
            self.set_catalog(Arc::new(catalog));
        }
        Ok(())
    }
}
