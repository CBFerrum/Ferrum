//! Tenant provisioning and per-tenant package installation (roles, permissions, seed data).

use crate::auth::{hash_password, validate_password};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::query::Filter;
use crate::registry::Registry;
use crate::{Doc, Kernel};
use serde_json::Value;
use std::sync::LazyLock;
use uuid::Uuid;

static SLUG: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[a-z0-9][a-z0-9-]{1,62}$").unwrap());

#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewTenant {
    pub slug: String,
    pub name: String,
    pub admin_email: String,
    pub admin_password: String,
    #[serde(default)]
    pub admin_name: Option<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    /// e.g. `{"fiscal_year_start_month": 4, "timezone": "Asia/Kolkata"}`
    #[serde(default)]
    pub settings: Option<Value>,
}

impl Kernel {
    pub async fn create_tenant(&self, t: NewTenant) -> Result<Uuid> {
        if !SLUG.is_match(&t.slug) {
            return Err(Error::field("slug", "use 2-63 lowercase letters, digits or hyphens"));
        }
        if t.name.trim().is_empty() || t.name.len() > 200 {
            return Err(Error::field("name", "is required (max 200 characters)"));
        }
        validate_password(&t.admin_password)?;
        let email = t.admin_email.trim().to_lowercase();
        if !email.contains('@') {
            return Err(Error::field("admin_email", "not a valid email address"));
        }
        let packages = self.registry.catalog().closure(&t.packages).map_err(|e| Error::field("packages", e.to_string()))?;
        let hash = hash_password(t.admin_password.clone()).await?;
        let tenant = Uuid::now_v7();
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into tenant (id, slug, name, settings) values ($1, $2, $3, $4)")
            .bind(tenant)
            .bind(&t.slug)
            .bind(t.name.trim())
            .bind(t.settings.clone().unwrap_or_else(|| serde_json::json!({})))
            .execute(&mut *tx)
            .await
            .map_err(|e| match Error::from(e) {
                Error::Conflict(_) => Error::field("slug", "is already taken"),
                other => other,
            })?;
        sqlx::query("insert into app_user (tenant_id, id, email, full_name, password_hash, is_admin) values ($1, $2, $3, $4, $5, true)")
            .bind(tenant)
            .bind(Uuid::now_v7())
            .bind(&email)
            .bind(t.admin_name.clone().unwrap_or_else(|| "Administrator".into()))
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.install_packages(tenant, &packages).await?;
        tracing::info!(%tenant, slug = %t.slug, "tenant created");
        Ok(tenant)
    }

    /// Installs packages (and their dependencies) for a tenant. Idempotent; existing role
    /// permissions and seeded records are left untouched so tenant edits survive reinstalls.
    pub async fn install_packages(&self, tenant: Uuid, names: &[String]) -> Result<Vec<String>> {
        let catalog = self.registry.catalog();
        let closure = catalog.closure(names).map_err(|e| Error::invalid(e.to_string()))?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        for name in &closure {
            let pkg = catalog.package(name).ok_or_else(|| Error::internal("package vanished"))?;
            sqlx::query(
                "insert into installed_package (tenant_id, name, version) values ($1, $2, $3) \
                 on conflict (tenant_id, name) do update set version = excluded.version",
            )
            .bind(tenant)
            .bind(name)
            .bind(&pkg.manifest.version)
            .execute(&mut *tx)
            .await?;
            for role in &pkg.manifest.roles {
                sqlx::query("insert into role (tenant_id, name, description, package) values ($1, $2, $3, $4) on conflict do nothing")
                    .bind(tenant)
                    .bind(&role.name)
                    .bind(&role.description)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
                for p in &role.permissions {
                    sqlx::query(
                        "insert into role_permission (tenant_id, role, entity, can_read, can_write, can_create, can_delete, can_submit, \
                         can_cancel, can_export, can_import, if_owner, read_groups, write_groups, match_rule, rule_key) \
                         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) on conflict do nothing",
                    )
                    .bind(tenant)
                    .bind(&role.name)
                    .bind(&p.entity)
                    .bind(p.read)
                    .bind(p.write)
                    .bind(p.create)
                    .bind(p.delete)
                    .bind(p.submit)
                    .bind(p.cancel)
                    .bind(p.export)
                    .bind(p.import)
                    .bind(p.if_owner)
                    .bind(&p.read_groups)
                    .bind(&p.write_groups)
                    .bind((!p.condition.is_empty()).then(|| serde_json::to_value(&p.condition).unwrap_or_default()))
                    .bind(p.condition_key())
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        Registry::bump_version(&mut tx, tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(tenant);
        let schema = self.registry.schema(tenant).await?;
        let ctx = Ctx::system(tenant, schema);
        for name in &closure {
            let pkg = catalog.package(name).ok_or_else(|| Error::internal("package vanished"))?;
            for seed in &pkg.manifest.seed {
                for rec in &seed.records {
                    self.seed_record(&ctx, &seed.entity, &seed.key, rec)
                        .await
                        .map_err(|e| Error::Internal(format!("seeding {}/{}: {e}", name, seed.entity)))?;
                }
            }
        }
        Ok(closure)
    }

    async fn seed_record(&self, ctx: &Ctx, entity: &str, key: &[String], rec: &serde_json::Map<String, Value>) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut resolved = Doc::new();
        for (k, v) in rec {
            resolved.insert(k.clone(), self.resolve_ref(ctx, &mut tx, v).await?);
        }
        let filters: Vec<Filter> = key.iter().map(|k| Filter::eq(k, resolved.get(k).cloned().unwrap_or(Value::Null))).collect();
        let existing = self.find_tx(ctx, &mut tx, entity, &filters, None, 1).await?;
        if existing.is_empty() {
            self.insert_tx(ctx, &mut tx, entity, resolved).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Seed values of the form `@Entity:field=value` resolve to that record's id.
    pub(crate) async fn resolve_ref(&self, ctx: &Ctx, tx: &mut crate::db::Tx, v: &Value) -> Result<Value> {
        if let Value::String(s) = v
            && let Some(rest) = s.strip_prefix('@')
            && let Some((entity, cond)) = rest.split_once(':')
            && let Some((field, value)) = cond.split_once('=')
        {
            let found = self.find_tx(ctx, tx, entity, &[Filter::eq(field, value)], None, 1).await?;
            let id = found.first().and_then(|d| d.get("id").cloned()).ok_or_else(|| Error::invalid(format!("seed reference {s} not found")))?;
            return Ok(id);
        }
        if let Value::Array(items) = v {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Object(m) => {
                        let mut r = serde_json::Map::new();
                        for (k, x) in m {
                            r.insert(k.clone(), Box::pin(self.resolve_ref(ctx, tx, x)).await?);
                        }
                        out.push(Value::Object(r));
                    }
                    other => out.push(other.clone()),
                }
            }
            return Ok(Value::Array(out));
        }
        Ok(v.clone())
    }

    pub async fn tenant_by_slug(&self, slug: &str) -> Result<(Uuid, String)> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String)> =
            sqlx::query_as("select id, status from tenant where slug = $1").bind(slug.trim().to_lowercase()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("tenant".into()))
    }
}

impl Kernel {
    /// The workspace's settings administrators may change: `timezone` (IANA name) and
    /// `fiscal_year_start_month` (1-12).
    pub async fn tenant_settings(&self, ctx: &Ctx) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let settings: Value = sqlx::query_scalar("select settings from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(serde_json::json!({
            "timezone": settings.get("timezone").cloned().unwrap_or_else(|| serde_json::json!("UTC")),
            "fiscal_year_start_month": settings.get("fiscal_year_start_month").cloned().unwrap_or_else(|| serde_json::json!(1)),
        }))
    }

    /// Changes workspace settings; every replica picks them up with the next schema load.
    pub async fn update_tenant_settings(&self, ctx: &Ctx, patch: &serde_json::Map<String, Value>) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("only administrators can change workspace settings".into()));
        }
        let mut clean = serde_json::Map::new();
        for (k, v) in patch {
            match k.as_str() {
                "timezone" => {
                    let tz = v
                        .as_str()
                        .filter(|t| t.parse::<chrono_tz::Tz>().is_ok())
                        .ok_or_else(|| Error::field("timezone", "not a known time zone (e.g. Asia/Kolkata)"))?;
                    clean.insert(k.clone(), serde_json::json!(tz));
                }
                "fiscal_year_start_month" => {
                    let m = v.as_u64().filter(|m| (1..=12).contains(m)).ok_or_else(|| Error::field("fiscal_year_start_month", "must be 1 to 12"))?;
                    clean.insert(k.clone(), serde_json::json!(m));
                }
                other => return Err(Error::field(other, "unknown setting")),
            }
        }
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        sqlx::query("update tenant set settings = settings || $2 where id = $1").bind(ctx.tenant).bind(Value::Object(clean.clone())).execute(&mut *tx).await?;
        crate::audit::record(&mut tx, ctx, "Tenant", ctx.tenant, "settings", None, Value::Object(clean)).await?;
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        self.tenant_settings(ctx).await
    }
}
