//! Per-tenant effective-schema cache with LISTEN/NOTIFY invalidation and a TTL fallback.

use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use dashmap::DashMap;
use ferrum_meta::{Catalog, Overlay, OverlayOp, Schema, SchemaWarning, effective};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

pub const META_CHANNEL: &str = "ferrum_meta";

/// The channel a change of the server's settings is announced on (server/src/settings.rs).
pub const SETTINGS_CHANNEL: &str = "ferrum_settings";

/// What a replica may hold of workspaces' schemas, counted in entities: 512 MiB over [`ENTITY_BYTES`]. Past it the entries
/// checked longest ago leave ([`SchemaCache::put`]) and are built again when their workspace is next used. Not a setting:
/// a replica that touches more than this is told by `ferrum_schema_cache_misses_total`.
///
/// A schema's size follows its entities (the packages installed), and varies 50-fold between workspaces, so a count of
/// schemas would bound nothing. Counted by `schema_size_is_printed` (a counting allocator around `effective::build`):
/// `cargo test -p ferrum-kernel --test registry_it schema_size_is_printed -- --nocapture`
/// * 10,238,857 bytes for 745 entities (13,743 each) with every package of the platform and of 39 apps' repositories
///   installed (66 packages; Edge and Cloud left out), with `FERRUM_SCHEMA_SIZE_PACKAGES` naming each app's `packages`
///   directory, colon-separated;
/// * 198,841 bytes for 21 entities (9,468 each) with the platform's five packages, without it.
const ENTITY_BYTES: usize = 14_000;
const BUDGET: usize = 512 * 1024 * 1024 / ENTITY_BYTES;

/// What makes two workspaces' schemas the same but for what is set per workspace: the cluster catalog as this replica has it
/// (its fingerprint covers every package's definition and version), and the installed packages that are in it, by name.
/// Only a workspace with no active `meta_overlay` row is keyed; one with any builds its own schema.
///
/// Why that is exact. `Registry::schema` reads, for a workspace: (1) `tenant.meta_version` and `tenant.settings`;
/// (2) `installed_package.name` (its `version` column is never read, so workspaces cannot differ by package version: an
/// upgrade is a new catalog and so a new key); (3) the active `meta_overlay` rows (none, for a key); (4) the audit topics of
/// its webhooks. And it asks `effective::build(catalog, installed, overlays, version)`, a function of those four arguments
/// alone (crates/meta/src/effective.rs: no clock, environment or global is read; `version` is only copied to
/// `Schema::version`, at the construction of the `Schema`). So with no overlays what `build` returns depends on the key
/// alone, and everything else is written afterwards, per workspace, onto the clone of it: `version` (1), the warnings of
/// packages no longer in the catalog (2), `audit_topics` (4), `controls`, `fy_start_month` and `timezone` with their warnings
/// (from `settings`, 1). Custom entities, studio objects, translations and roles are rows read elsewhere or through
/// overlays, never by `build`; zones and permissions are not in a schema.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key(String, Vec<String>);

/// What `build` made for a [`Key`], before anything per workspace was written onto it. Held by every cached workspace made
/// from it, so it lives as long as one does; its entities are what they share (`Schema`'s clone copies the map, not the
/// entities, each an `Arc`). It weighs its entity count against the cache's total from its making to its drop.
struct Template {
    schema: Schema,
    weight: usize,
    total: Arc<AtomicUsize>,
}

impl Template {
    fn new(schema: Schema, total: &Arc<AtomicUsize>) -> Arc<Template> {
        let weight = schema.entities.len().max(1);
        total.fetch_add(weight, Ordering::Relaxed);
        Arc::new(Template { schema, weight, total: total.clone() })
    }
}

impl Drop for Template {
    fn drop(&mut self) {
        self.total.fetch_sub(self.weight, Ordering::Relaxed);
    }
}

struct Cached {
    schema: Arc<Schema>,
    /// What it was made from, when it was made from one: kept for as long as this entry is.
    _template: Option<Arc<Template>>,
    /// What it counts against the budget: its entities, at least one; a quarter of that when what is shared sits in its
    /// template (the rest of a schema, which is not shared, measured about 23% of it).
    weight: usize,
    /// Milliseconds after the cache was made at which the database last said this schema is current.
    checked_at: AtomicU64,
}

/// The schemas kept, their weights adding up to at most `budget` (but for the entry just put, which stays even if it alone
/// is over). A map and a stamp each, no thread of its own: entries checked longest ago are dropped while the total is
/// over. Workspaces in use are checked again at least every `ttl`, so the stamp is a last-use time good to that; upkeep
/// touching every workspace blurs it, which only costs a rebuild.
/// ponytail: the oldest is found by scanning all entries, once per drop; a rebuild reads the database several times and
/// costs far more. An ordered structure if misses at the budget ever become the common case.
struct SchemaCache {
    map: DashMap<Uuid, Cached>,
    /// The weights of everything in `map` and of the templates behind it, moved with each insertion and removal.
    total: Arc<AtomicUsize>,
    budget: usize,
    start: Instant,
}

impl SchemaCache {
    fn new(budget: usize) -> Self {
        SchemaCache { map: DashMap::new(), total: Arc::new(AtomicUsize::new(0)), budget, start: Instant::now() }
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// The schema, if it was checked within `ttl`.
    fn fresh(&self, tenant: Uuid, ttl: Duration) -> Option<Arc<Schema>> {
        let c = self.map.get(&tenant)?;
        (self.now().saturating_sub(c.checked_at.load(Ordering::Relaxed)) < ttl.as_millis() as u64).then(|| c.schema.clone())
    }

    /// The schema, if it is of `version`; it counts as checked now.
    fn current(&self, tenant: Uuid, version: i64) -> Option<Arc<Schema>> {
        let c = self.map.get(&tenant)?;
        (c.schema.version == version).then(|| {
            c.checked_at.store(self.now(), Ordering::Relaxed);
            c.schema.clone()
        })
    }

    fn put(&self, tenant: Uuid, schema: Arc<Schema>, template: Option<Arc<Template>>) {
        let weight = if template.is_some() { schema.entities.len().div_ceil(4).max(1) } else { schema.entities.len().max(1) };
        if let Some(old) = self.map.insert(tenant, Cached { schema, _template: template, weight, checked_at: AtomicU64::new(self.now()) }) {
            self.total.fetch_sub(old.weight, Ordering::Relaxed);
        }
        self.total.fetch_add(weight, Ordering::Relaxed);
        // Several puts at once may each see it over and each drop some: a loop, and never the entry just put.
        while self.total.load(Ordering::Relaxed) > self.budget {
            let oldest = self.map.iter().filter(|e| *e.key() != tenant).min_by_key(|e| e.checked_at.load(Ordering::Relaxed)).map(|e| *e.key());
            match oldest {
                Some(t) => self.remove(t),
                None => break,
            }
        }
    }

    /// Drops a workspace's schema, and what the kernel keeps beside it by the schema's version (rules, listeners).
    fn remove(&self, tenant: Uuid) {
        if let Some((_, c)) = self.map.remove(&tenant) {
            self.total.fetch_sub(c.weight, Ordering::Relaxed);
        }
        crate::logic::forget(tenant);
        crate::process::forget(tenant);
    }

    fn clear(&self) {
        let all: Vec<Uuid> = self.map.iter().map(|e| *e.key()).collect();
        for t in all {
            self.remove(t);
        }
    }
}

/// `META_CHANNEL` payload announcing that the cluster catalog changed (an app became available).
pub const CATALOG_CHANGED: &str = "catalog";

pub struct Registry {
    /// Platform packages (and native apps) this build ships; apps made available at runtime are
    /// added on top.
    base: Vec<ferrum_meta::Package>,
    catalog: std::sync::RwLock<Arc<Catalog>>,
    db: Db,
    cache: SchemaCache,
    /// What was built for each [`Key`], for as long as a cached workspace uses it.
    templates: DashMap<Key, Weak<Template>>,
    /// Schemas built from the database (a workspace not kept, or whose settings changed), since this replica started.
    misses: AtomicU64,
    ttl: Duration,
    /// How often each workspace's settings were seen to change, from this replica or another.
    changes: DashMap<Uuid, u64>,
    /// How often everything was dropped, a notification having perhaps been missed.
    epoch: AtomicU64,
    /// How often any workspace's settings were seen to change, or everything dropped: a replica's answers for hosts
    /// ([`crate::Kernel::route_host`]) are kept only while this stands.
    bumps: AtomicU64,
    /// Moves on each notice of [`SETTINGS_CHANNEL`], and whenever one may have been missed ([`Registry::settings_notices`]).
    settings: watch::Sender<u64>,
}

impl Registry {
    pub fn new(base: &Catalog, catalog: Arc<Catalog>, db: Db) -> Self {
        Registry {
            base: base.packages().cloned().collect(),
            catalog: std::sync::RwLock::new(catalog),
            db,
            cache: SchemaCache::new(BUDGET),
            templates: DashMap::new(),
            misses: AtomicU64::new(0),
            ttl: Duration::from_secs(30),
            changes: DashMap::new(),
            epoch: AtomicU64::new(0),
            bumps: AtomicU64::new(0),
            settings: watch::channel(0).0,
        }
    }

    /// Changes of the server's settings this replica heard of: the metadata listener hears [`SETTINGS_CHANNEL`] too, and
    /// the value moves on each notice, and whenever the listener (re)connects, since one may have been missed then.
    pub fn settings_notices(&self) -> watch::Receiver<u64> {
        self.settings.subscribe()
    }

    fn settings_changed(&self) {
        self.settings.send_modify(|n| *n += 1);
    }

    pub(crate) fn bumps(&self) -> u64 {
        self.bumps.load(Ordering::SeqCst)
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
        if let Some(schema) = self.cache.fresh(tenant, self.ttl) {
            return Ok(schema);
        }
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let row: Option<(i64, serde_json::Value)> =
            sqlx::query_as("select meta_version, settings from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
        let Some((version, settings)) = row else {
            return Err(Error::NotFound("tenant".into()));
        };
        if let Some(schema) = self.cache.current(tenant, version) {
            return Ok(schema);
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
        // Which audit categories its webhooks want (crate::audit): read here, with the schema, and dropped with it when a
        // webhook changes (Registry::bump_version), so an audit write asks the database nothing more.
        let audit_topics: Vec<String> = sqlx::query_scalar(
            "select distinct t from webhook w, unnest(w.topics) t where w.tenant_id = $1 and w.is_active and w.app is null and t like 'audit.%' order by t",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let shared = rows.is_empty();
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
        let (mut schema, template) = if shared {
            let template = self.template(&catalog, &installed, version).map_err(|e| Error::Internal(format!("schema for tenant {tenant}: {e}")))?;
            (template.schema.clone(), Some(template))
        } else {
            (effective::build(&catalog, &installed, &overlays, version).map_err(|e| Error::Internal(format!("schema for tenant {tenant}: {e}")))?, None)
        };
        schema.version = version;
        schema.warnings.extend(parse_warnings);
        for m in missing {
            schema.warnings.push(SchemaWarning { entity: m.clone(), message: format!("package {m} is installed but no longer available on this cluster") });
        }
        schema.audit_topics = audit_topics;
        // Step-up rules and recorded reads (crate::controls), from the settings read above (no statement of their own), over
        // the reads its entities declare recorded.
        let declared = schema.entities.values().filter_map(|e| e.def.record_reads.map(|r| (e.def.name.clone(), r == ferrum_meta::RecordReads::All)));
        let (controls, wrong) = crate::controls::load(&settings, declared);
        schema.controls = controls;
        if let Some(wrong) = wrong {
            schema
                .warnings
                .push(SchemaWarning { entity: "tenant".into(), message: format!("{wrong}; every entity is treated as marked until it is saved again") });
        }
        schema.fy_start_month = settings.get("fiscal_year_start_month").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 12) as u32;
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
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.cache.put(tenant, schema.clone(), template);
        Ok(schema)
    }

    /// What `build` makes for these installed packages and no customizations: one for all workspaces using them at once. Two
    /// first uses together may each build one; the later is the one kept in the map, and both are right. No lock is held
    /// across the build.
    fn template(&self, catalog: &Catalog, installed: &[String], version: i64) -> std::result::Result<Arc<Template>, ferrum_meta::MetaError> {
        let key = Key(catalog.fingerprint().to_string(), installed.to_vec());
        if let Some(t) = self.templates.get(&key).and_then(|w| w.upgrade()) {
            return Ok(t);
        }
        let template = Template::new(effective::build(catalog, installed, &[], version)?, &self.cache.total);
        if self.templates.len() > 1024 {
            self.templates.retain(|_, w| w.strong_count() > 0);
        }
        self.templates.insert(key, Arc::downgrade(&template));
        Ok(template)
    }

    /// How many schemas were built from the database since this replica started (`ferrum_schema_cache_misses_total`): it
    /// climbing while the workspaces in use stay the same means more are in use than [`BUDGET`] keeps.
    pub fn schema_cache_misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn invalidate(&self, tenant: Uuid) {
        self.cache.remove(tenant);
        *self.changes.entry(tenant).or_insert(0) += 1;
        self.bumps.fetch_add(1, Ordering::SeqCst);
    }

    pub fn invalidate_all(&self) {
        self.templates.clear();
        self.cache.clear();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.bumps.fetch_add(1, Ordering::SeqCst);
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

    /// Listens for metadata and settings changes from any replica. Whenever some may have been missed (at the start, a
    /// lost listening connection, a full queue) the settings are read again, the app catalog reloaded and the whole
    /// cache dropped.
    pub fn spawn_listener(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        use crate::db::Heard;
        let me = self.clone();
        tokio::spawn(async move {
            if *shutdown.borrow() {
                return;
            }
            let mut heard = tokio::select! {
                _ = shutdown.changed() => return,
                h = me.db.subscribe(&[META_CHANNEL, SETTINGS_CHANNEL]) => h,
            };
            loop {
                let h = tokio::select! {
                    _ = shutdown.changed() => return,
                    h = heard.recv() => h,
                };
                match h {
                    None => return,
                    Some(Heard::Missed { .. }) => {
                        me.settings_changed();
                        if let Err(e) = me.reload_catalog().await {
                            tracing::error!("reloading the app catalog failed: {e}");
                        }
                        me.invalidate_all();
                    }
                    Some(Heard::Said { channel, .. }) if channel == SETTINGS_CHANNEL => me.settings_changed(),
                    Some(Heard::Said { payload, .. }) if payload == CATALOG_CHANGED => {
                        if let Err(e) = me.reload_catalog().await {
                            tracing::error!("reloading the app catalog failed: {e}");
                        }
                    }
                    Some(Heard::Said { payload, .. }) => match payload.parse::<Uuid>() {
                        Ok(t) => me.invalidate(t),
                        Err(_) => me.invalidate_all(),
                    },
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema of `n` entities (the one entity of a small package, named over and over).
    fn schema(n: usize) -> Arc<Schema> {
        let files: std::collections::BTreeMap<String, String> = [
            ("package.yaml", "package: small\nversion: 0.1.0\n"),
            ("entities/lead.yaml", "name: Lead\nlabel: Lead\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data}\n"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let package = ferrum_meta::catalog::load_package_files(&files).unwrap();
        let mut schema = effective::build(&Catalog::from_packages(vec![package]).unwrap(), &["small".to_string()], &[], 1).unwrap();
        let one = schema.entities.values().next().expect("an entity").clone();
        for i in 1..n {
            schema.entities.insert(format!("Lead{i}"), one.clone());
        }
        assert_eq!(schema.entities.len(), n);
        Arc::new(schema)
    }

    fn weights(cache: &SchemaCache) -> usize {
        cache.map.iter().map(|e| e.weight).sum()
    }

    /// Workspaces of many sizes touched, the budget forced to 1,000 entities: what is kept never weighs more, the running
    /// total is the sum of the weights, the one touched last is kept, an entry over the budget by itself is kept and
    /// served, and no evicted workspace keeps its rules or listeners (logic.rs, process.rs).
    #[test]
    fn cache_is_bounded() {
        let cache = SchemaCache::new(1000);
        let (small, large) = (schema(1), schema(120));
        let tenants: Vec<Uuid> = (0..1000).map(|_| Uuid::new_v4()).collect();
        for (i, t) in tenants.iter().enumerate() {
            crate::logic::keep(*t);
            crate::process::keep(*t);
            // Nine hundred small and a hundred large, mixed: 12,900 entities in all.
            cache.put(*t, if i % 10 == 9 { large.clone() } else { small.clone() }, None);
            assert!(cache.total.load(Ordering::Relaxed) <= 1000, "over the budget after {} puts", i + 1);
        }
        assert_eq!(cache.total.load(Ordering::Relaxed), weights(&cache), "the running total is what is held");
        assert!(cache.map.contains_key(tenants.last().unwrap()), "the one touched last is kept");
        assert!((50..100).contains(&cache.map.len()), "{} kept", cache.map.len());
        for t in &tenants {
            let held = cache.map.contains_key(t);
            assert_eq!((crate::logic::kept(*t), crate::process::kept(*t)), (held, held), "rules and listeners are kept as long as the schema, no longer");
        }
        // The one checked longest ago goes, not one in use: a kept workspace checked again outlives the rest.
        let old = *tenants.iter().find(|t| cache.map.contains_key(t)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.current(old, 1).is_some());
        for _ in 0..800 {
            cache.put(Uuid::new_v4(), small.clone(), None);
        }
        assert!(cache.map.contains_key(&old), "a workspace checked just now outlives the rest put after it");
        // One schema over the budget by itself stays and is served; the rest make room, and it is the oldest after.
        let (huge, big) = (Uuid::new_v4(), schema(5000));
        cache.put(huge, big.clone(), None);
        assert!(Arc::ptr_eq(&cache.fresh(huge, Duration::from_secs(60)).expect("served"), &big));
        assert_eq!((cache.map.len(), cache.total.load(Ordering::Relaxed)), (1, 5000));
        std::thread::sleep(Duration::from_millis(5));
        cache.put(Uuid::new_v4(), small.clone(), None);
        assert_eq!((cache.map.contains_key(&huge), cache.total.load(Ordering::Relaxed)), (false, 1));
        // Everything dropped (a notice may have been missed) takes the rules and listeners with it.
        cache.clear();
        assert_eq!((cache.map.len(), cache.total.load(Ordering::Relaxed)), (0, 0));
        assert!(tenants.iter().all(|t| !crate::logic::kept(*t) && !crate::process::kept(*t)));
    }

    /// A workspace made from a template: its entities are the template's, it weighs a quarter of them, the template
    /// weighs them once for all and goes, with its weight, with the last workspace using it.
    #[test]
    fn a_template_weighs_while_it_is_used() {
        let cache = SchemaCache::new(50);
        let base = schema(40);
        let template = Template::new((*base).clone(), &cache.total);
        let from_template = |version| {
            let mut s = (*base).clone();
            s.version = version;
            Arc::new(s)
        };
        assert_eq!(cache.total.load(Ordering::Relaxed), 40, "the template weighs its entities");
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        cache.put(a, from_template(1), Some(template.clone()));
        assert_eq!(cache.total.load(Ordering::Relaxed), 50, "and the workspace a quarter: 40 + 10");
        cache.put(b, from_template(2), Some(template.clone()));
        drop(template);
        // 60 is over 50: the one checked longest ago goes, the template stays for the one left.
        assert_eq!((cache.map.contains_key(&a), cache.map.contains_key(&b), cache.total.load(Ordering::Relaxed)), (false, true, 50));
        let (x, y) = (cache.fresh(b, Duration::from_secs(60)).unwrap(), from_template(3));
        assert!(Arc::ptr_eq(x.entities.values().next().unwrap(), y.entities.values().next().unwrap()), "their entities are one allocation");
        // A schema of its own, as large as it can be and stay: everything else makes room, the template with the last.
        std::thread::sleep(Duration::from_millis(5));
        cache.put(c, schema(100), None);
        assert_eq!((cache.map.len(), cache.total.load(Ordering::Relaxed)), (1, 100), "the template's weight went with its last workspace");
    }
}
