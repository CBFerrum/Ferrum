//! Apps on this cluster (docs/plan/apps.md). The kernel sees a release as packages: making one
//! available adds its packages to the cluster catalog (validated together with everything else,
//! tables created), records it, and tells every replica to reload; workspaces then install and
//! remove those packages. Bundles, signatures and stores are the server's business (`ferrum-apps`).

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::registry::{CATALOG_CHANGED, META_CHANNEL, Registry};
use crate::{Ctx, Kernel, ddl};
use ferrum_meta::{Catalog, EntityKind, Package};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// A release to make available (from the store, an upload, or a native app in this build).
#[derive(Debug, Clone)]
pub struct NewRelease {
    pub app_id: String,
    pub version: String,
    /// `native`, `wasm` or `meta`.
    pub runtime: String,
    pub manifest: Value,
    pub digest: String,
    pub signatures: Option<Value>,
    /// `native`, `upload`, or `store:<url>`.
    pub source: String,
    pub bundle_key: Option<String>,
    /// Package name → (path inside the package → text).
    pub packages: BTreeMap<String, BTreeMap<String, String>>,
    pub plugin_sha256: Option<String>,
    /// Workspace-private uploads: sandboxed code only, visible to that workspace alone.
    pub private_tenant: Option<Uuid>,
    pub created_by: Option<Uuid>,
}

/// A release recorded on the cluster.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Release {
    pub id: Uuid,
    pub app_id: String,
    pub version: String,
    pub runtime: String,
    pub manifest: Value,
    pub digest: String,
    pub signatures: Option<Value>,
    pub source: String,
    pub bundle_key: Option<String>,
    pub packages: Value,
    pub plugin_sha256: Option<String>,
    pub status: String,
    pub status_reason: Option<String>,
    pub private_tenant: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Release {
    pub fn package_names(&self) -> Vec<String> {
        self.packages.as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default()
    }
}

/// A workspace's installation of an app.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Installation {
    pub app_id: String,
    pub version: String,
    pub enabled: bool,
    pub consent_digest: Option<String>,
    pub plugin_id: Option<Uuid>,
    pub installed_by: Option<Uuid>,
    pub installed_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

const RELEASE_COLUMNS: &str = "id, app_id, version, runtime, manifest, digest, signatures, source, bundle_key, packages, plugin_sha256, status, \
                               status_reason, private_tenant, created_at";

fn parse_packages(app: &str, packages: &BTreeMap<String, BTreeMap<String, String>>) -> Result<Vec<Package>> {
    packages
        .iter()
        .map(|(name, files)| {
            let p = ferrum_meta::catalog::load_package_files(files).map_err(|e| Error::invalid(format!("{app}: package {name}: {e}")))?;
            if p.name() != name {
                return Err(Error::invalid(format!("{app}: package directory {name} holds package {}", p.name())));
            }
            Ok(p)
        })
        .collect()
}

impl Kernel {
    /// Makes a release available on the cluster: its packages are validated together with the
    /// catalog, their tables and columns created (additive DDL under the cluster lock), the app's
    /// previous release superseded, and every replica reloads its catalog. The same version with
    /// different content is refused — publish a new version instead.
    pub async fn make_available(&self, rel: NewRelease) -> Result<Release> {
        let pkgs = parse_packages(&rel.app_id, &rel.packages)?;
        if rel.private_tenant.is_some() && !pkgs.is_empty() {
            return Err(Error::invalid("workspace-private apps cannot bring packages (tables); ask the operator to make the app available on the cluster"));
        }
        let catalog = if pkgs.is_empty() {
            None
        } else {
            let mut apps = Registry::app_packages(&self.db).await?;
            apps.retain(|(a, _)| a != &rel.app_id);
            let mut all: Vec<Package> = self.registry.base().to_vec();
            all.extend(apps.into_iter().flat_map(|(_, p)| p));
            all.extend(pkgs);
            let catalog = Catalog::from_packages(all).map_err(|e| Error::invalid(format!("{} {} does not fit this platform: {e}", rel.app_id, rel.version)))?;
            ddl::sync(&self.db, &catalog, false).await?;
            Some(catalog)
        };

        let mut tx = self.db.system_tx().await?;
        let same: Option<(String, String)> =
            sqlx::query_as("select digest, status from app_release where app_id = $1 and version = $2 and private_tenant is not distinct from $3 for update")
                .bind(&rel.app_id)
                .bind(&rel.version)
                .bind(rel.private_tenant)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((digest, status)) = &same {
            if digest != &rel.digest {
                return Err(Error::Conflict(format!("{} {} already exists with different content; publish a new version", rel.app_id, rel.version)));
            }
            if status == "revoked" {
                return Err(Error::Forbidden(format!("{} {} was revoked", rel.app_id, rel.version)));
            }
        }
        sqlx::query(
            "update app_release set status = 'superseded' where app_id = $1 and version <> $2 and status = 'active' \
             and private_tenant is not distinct from $3",
        )
        .bind(&rel.app_id)
        .bind(&rel.version)
        .bind(rel.private_tenant)
        .execute(&mut *tx)
        .await?;
        let packages = serde_json::to_value(&rel.packages).map_err(|e| Error::Internal(e.to_string()))?;
        let release: Release = if same.is_some() {
            sqlx::query_as(AssertSqlSafe(format!(
                "update app_release set status = 'active', status_reason = null, manifest = $4, signatures = coalesce($5, signatures), \
                 bundle_key = coalesce($6, bundle_key) where app_id = $1 and version = $2 and private_tenant is not distinct from $3 \
                 returning {RELEASE_COLUMNS}"
            )))
            .bind(&rel.app_id)
            .bind(&rel.version)
            .bind(rel.private_tenant)
            .bind(&rel.manifest)
            .bind(&rel.signatures)
            .bind(&rel.bundle_key)
            .fetch_one(&mut *tx)
            .await?
        } else {
            sqlx::query_as(AssertSqlSafe(format!(
                "insert into app_release (app_id, version, runtime, manifest, digest, signatures, source, bundle_key, packages, plugin_sha256, \
                 private_tenant, created_by) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) returning {RELEASE_COLUMNS}"
            )))
            .bind(&rel.app_id)
            .bind(&rel.version)
            .bind(&rel.runtime)
            .bind(&rel.manifest)
            .bind(&rel.digest)
            .bind(&rel.signatures)
            .bind(&rel.source)
            .bind(&rel.bundle_key)
            .bind(packages)
            .bind(&rel.plugin_sha256)
            .bind(rel.private_tenant)
            .bind(rel.created_by)
            .fetch_one(&mut *tx)
            .await?
        };
        if catalog.is_some() {
            sqlx::query("select pg_notify($1, $2)").bind(META_CHANNEL).bind(CATALOG_CHANGED).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if let Some(c) = catalog {
            self.registry.set_catalog(Arc::new(c));
        }
        tracing::info!(app = %rel.app_id, version = %rel.version, source = %rel.source, "app release available");
        Ok(release)
    }

    /// Takes a release out of service everywhere (a store revocation or an operator decision):
    /// its packages leave the catalog — workspaces keep their data but lose the app until a good
    /// release is available — and the server disables its code.
    pub async fn revoke_release(&self, app_id: &str, version: &str, reason: &str) -> Result<bool> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("update app_release set status = 'revoked', status_reason = $3 where app_id = $1 and version = $2 and status <> 'revoked'")
            .bind(app_id)
            .bind(version)
            .bind(reason)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n > 0 {
            sqlx::query("select pg_notify($1, $2)").bind(META_CHANNEL).bind(CATALOG_CHANGED).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if n > 0 {
            self.registry.reload_catalog().await?;
            tracing::warn!(app = %app_id, %version, %reason, "app release revoked");
        }
        Ok(n > 0)
    }

    /// Releases visible to a workspace: active cluster releases and its own private ones.
    pub async fn releases(&self, tenant: Option<Uuid>) -> Result<Vec<Release>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_as::<_, Release>(AssertSqlSafe(format!(
            "select {RELEASE_COLUMNS} from app_release where status = 'active' and (private_tenant is null or private_tenant = $1) order by app_id"
        )))
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Every recorded release of an app (history, including superseded and revoked ones).
    pub async fn release_history(&self, app_id: &str) -> Result<Vec<Release>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_as::<_, Release>(AssertSqlSafe(format!(
            "select {RELEASE_COLUMNS} from app_release where app_id = $1 and private_tenant is null order by created_at desc"
        )))
        .bind(app_id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn installations(&self, tenant: Uuid) -> Result<Vec<Installation>> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let rows = sqlx::query_as::<_, Installation>(
            "select app_id, version, enabled, consent_digest, plugin_id, installed_by, installed_at, updated_at from tenant_app where tenant_id = $1 order by app_id",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Records a workspace's installation (after its packages and plugin are in place).
    pub async fn record_installation(&self, ctx: &Ctx, app_id: &str, version: &str, consent: Option<&str>, plugin_id: Option<Uuid>) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query(
            "insert into tenant_app (tenant_id, app_id, version, enabled, consent_digest, plugin_id, installed_by) values ($1, $2, $3, true, $4, $5, $6) \
             on conflict (tenant_id, app_id) do update set version = excluded.version, enabled = true, \
             consent_digest = coalesce(excluded.consent_digest, tenant_app.consent_digest), plugin_id = coalesce(excluded.plugin_id, tenant_app.plugin_id), \
             updated_at = now()",
        )
        .bind(ctx.tenant)
        .bind(app_id)
        .bind(version)
        .bind(consent)
        .bind(plugin_id)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut *tx)
        .await?;
        crate::audit::record(&mut tx, ctx, "App", Uuid::nil(), "install", None, json!({"app": app_id, "version": version, "consent": consent})).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_app_enabled(&self, ctx: &Ctx, app_id: &str, enabled: bool) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update tenant_app set enabled = $3, updated_at = now() where tenant_id = $1 and app_id = $2")
            .bind(ctx.tenant)
            .bind(app_id)
            .bind(enabled)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound(format!("app {app_id}")));
        }
        crate::audit::record(&mut tx, ctx, "App", Uuid::nil(), if enabled { "enable" } else { "disable" }, None, json!({"app": app_id})).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn forget_installation(&self, ctx: &Ctx, app_id: &str, deleted_data: bool) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from tenant_app where tenant_id = $1 and app_id = $2").bind(ctx.tenant).bind(app_id).execute(&mut *tx).await?;
        crate::audit::record(&mut tx, ctx, "App", Uuid::nil(), "uninstall", None, json!({"app": app_id, "deleted_data": deleted_data})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes packages from a workspace: refuses while another installed package depends on
    /// them; drops their roles (and those roles' grants); with `delete_data`, deletes the
    /// workspace's rows in their tables (child tables first). Tables stay — other workspaces may
    /// use them.
    pub async fn uninstall_packages(&self, ctx: &Ctx, names: &[String], delete_data: bool) -> Result<()> {
        if !ctx.is_admin && !ctx.is_system {
            return Err(Error::Forbidden("only administrators can remove apps".into()));
        }
        let catalog = self.registry.catalog();
        let remaining: Vec<String> = ctx.schema.packages.iter().filter(|p| !names.contains(p)).cloned().collect();
        let still_needed = catalog.closure(&remaining).map_err(|e| Error::invalid(e.to_string()))?;
        if let Some(blocker) = names.iter().find(|n| still_needed.contains(n)) {
            let users: Vec<&String> = remaining.iter().filter(|r| catalog.closure(std::slice::from_ref(*r)).is_ok_and(|c| c.contains(blocker))).collect();
            return Err(Error::invalid(format!(
                "{blocker} is needed by {}; remove those first",
                users.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if delete_data {
            for name in names {
                let Some(pkg) = catalog.package(name) else { continue };
                for e in pkg.entities.iter().filter(|e| e.kind != EntityKind::Child && e.kind != EntityKind::Ledger) {
                    let Some(ent) = ctx.schema.entity(&e.name) else { continue };
                    delete_entity_rows(&mut tx, ctx.tenant, ent).await?;
                }
            }
        }
        for name in names {
            sqlx::query("delete from role_permission where tenant_id = $1 and role in (select name from role where tenant_id = $1 and package = $2)")
                .bind(ctx.tenant)
                .bind(name)
                .execute(&mut *tx)
                .await?;
            sqlx::query("delete from user_role where tenant_id = $1 and role in (select name from role where tenant_id = $1 and package = $2)")
                .bind(ctx.tenant)
                .bind(name)
                .execute(&mut *tx)
                .await?;
            sqlx::query("delete from role where tenant_id = $1 and package = $2").bind(ctx.tenant).bind(name).execute(&mut *tx).await?;
            sqlx::query("delete from installed_package where tenant_id = $1 and name = $2").bind(ctx.tenant).bind(name).execute(&mut *tx).await?;
        }
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        Ok(())
    }
}

/// Deletes a workspace's rows of an entity and of its child tables.
async fn delete_entity_rows(tx: &mut Tx, tenant: Uuid, ent: &ferrum_meta::EffEntity) -> Result<()> {
    if ent.table.is_empty() || ent.custom {
        return Ok(());
    }
    for child in ent.children.values() {
        sqlx::query(AssertSqlSafe(format!("delete from {} where tenant_id = $1", ddl::q(&child.table)))).bind(tenant).execute(&mut **tx).await?;
    }
    sqlx::query(AssertSqlSafe(format!("delete from {} where tenant_id = $1", ddl::q(&ent.table)))).bind(tenant).execute(&mut **tx).await?;
    for table in ["record_comment", "record_assignment", "record_tag", "record_follower", "record_secret"] {
        sqlx::query(AssertSqlSafe(format!("delete from {table} where tenant_id = $1 and entity = $2")))
            .bind(tenant)
            .bind(&ent.def.name)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
