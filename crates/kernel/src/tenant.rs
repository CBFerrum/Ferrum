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

impl NewTenant {
    /// What can be checked of a new workspace before anything is written.
    pub(crate) fn check(&self) -> Result<()> {
        if !SLUG.is_match(&self.slug) {
            return Err(Error::field("slug", "use 2-63 lowercase letters, digits or hyphens"));
        }
        if crate::identity::RESERVED_SLUGS.contains(&self.slug.as_str()) {
            return Err(Error::field("slug", "is a name the cluster keeps for itself"));
        }
        if self.name.trim().is_empty() || self.name.len() > 200 {
            return Err(Error::field("name", "is required (max 200 characters)"));
        }
        validate_password(&self.admin_password)
    }
}

impl Kernel {
    pub async fn create_tenant(&self, t: NewTenant) -> Result<Uuid> {
        t.check()?;
        let email = t.admin_email.trim().to_lowercase();
        if !email.contains('@') {
            return Err(Error::field("admin_email", "not a valid email address"));
        }
        // Every workspace has the core; the rest comes with the apps it installs.
        let mut wanted = t.packages.clone();
        if !wanted.iter().any(|p| p == "core") {
            wanted.push("core".into());
        }
        let packages = self.registry.catalog().closure(&wanted).map_err(|e| Error::field("packages", e.to_string()))?;
        let hash = hash_password(t.admin_password.clone()).await?;
        // A new workspace uses the shell and apps' own interfaces unless told otherwise (apps-ui.md
        // U4b); one made before keeps today's web app until it is switched (no `shell_v2`).
        let mut settings = t.settings.clone().unwrap_or_else(|| serde_json::json!({}));
        if let Some(s) = settings.as_object_mut() {
            s.entry("shell_v2").or_insert(serde_json::json!(true));
        }
        let tenant = Uuid::now_v7();
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into tenant (id, slug, name, settings) values ($1, $2, $3, $4)")
            .bind(tenant)
            .bind(&t.slug)
            .bind(t.name.trim())
            .bind(settings)
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
        // What it would install beside what is there: a hook must not run without one it waits for.
        let installed: Vec<String> = sqlx::query_scalar("select name from installed_package where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?;
        let unmet = self.hooks.unmet(&installed.into_iter().chain(closure.iter().cloned()).collect::<Vec<_>>());
        if !unmet.is_empty() {
            return Err(Error::invalid(format!("these packages cannot be installed: {}", unmet.join("; "))));
        }
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
                // A package's portal mark follows its version (BD-02); its roles stay its own.
                sqlx::query(
                    "insert into role (tenant_id, name, description, package, portal) values ($1, $2, $3, $4, $5) \
                     on conflict (tenant_id, name) do update set portal = excluded.portal where role.package = excluded.package",
                )
                .bind(tenant)
                .bind(&role.name)
                .bind(&role.description)
                .bind(name)
                .bind(role.portal)
                .execute(&mut *tx)
                .await?;
                for p in &role.permissions {
                    sqlx::query(
                        "insert into role_permission (tenant_id, role, entity, can_read, can_write, can_create, can_delete, can_submit, \
                         can_cancel, can_export, can_import, if_owner, read_groups, write_groups, match_rule, rule_key, via, actions) \
                         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18) on conflict do nothing",
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
                    .bind(p.if_owner || p.via.is_some())
                    .bind(&p.read_groups)
                    .bind(&p.write_groups)
                    .bind((!p.condition.is_empty()).then(|| serde_json::to_value(&p.condition).unwrap_or_default()))
                    .bind(p.condition_key())
                    .bind(&p.via)
                    .bind(&p.actions)
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
        self.report_conflicts(tenant).await?;
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

/// A language tag the interface takes (BCP 47's shape: `hi`, `pt-BR`, `zh-Hant-TW`).
pub fn valid_language(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let first = parts.next().unwrap_or_default();
    tag.len() <= 35
        && (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

impl Kernel {
    /// The workspace's own words for the interface in `language` (its `InterfaceWord` records),
    /// by their English: a base language's (`pt`) first, a regional one's (`pt-BR`) over them.
    /// None where the workspace has no such records.
    pub async fn interface_words(&self, tenant: Uuid, language: &str) -> Result<serde_json::Map<String, Value>> {
        let schema = self.registry.schema(tenant).await?;
        let Some(ent) = schema.entity("InterfaceWord") else { return Ok(Default::default()) };
        let tag = language.to_ascii_lowercase();
        let base = tag.split(['-', '_']).next().unwrap_or_default().to_string();
        let mut tx = self.db.tenant_tx(tenant).await?;
        // The table is the catalog's, quoted; the rest is bound.
        let rows: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select english, words from {} where tenant_id = $1 and lower(language) in ($2, $3) order by lower(language) = $3, english limit 5000",
            crate::ddl::q(&ent.table)
        )))
        .bind(tenant)
        .bind(&base)
        .bind(&tag)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|(english, words)| (english, Value::String(words))).collect())
    }

    /// The interface language a person reads: their own (`locale` preference), else the
    /// workspace's (`language` setting); none means English. Without a person, the workspace's.
    pub async fn language_of(&self, tenant: Uuid, user: Option<Uuid>) -> Result<Option<String>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let mine: Option<String> = match user.filter(|u| !u.is_nil()) {
            Some(u) => sqlx::query_scalar("select value->>'language' from ui_pref where tenant_id = $1 and user_id = $2 and scope = 'locale'")
                .bind(tenant)
                .bind(u)
                .fetch_optional(&mut *tx)
                .await?
                .flatten(),
            None => None,
        };
        let workspace: Option<String> = sqlx::query_scalar("select settings->>'language' from tenant where id = $1").bind(tenant).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(mine.filter(|l| valid_language(l)).or(workspace.filter(|l| valid_language(l))))
    }

    /// The zone a person chose to see times in (their `timezone` preference, `{"zone": "Asia/Kolkata"}`),
    /// when it is one; `None` for the workspace's clock. Business days (`today`) stay the workspace's.
    pub async fn time_zone_of(&self, tenant: Uuid, user: Uuid) -> Result<Option<String>> {
        if user.is_nil() {
            return Ok(None);
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        let zone: Option<String> = sqlx::query_scalar("select value->>'zone' from ui_pref where tenant_id = $1 and user_id = $2 and scope = 'timezone'")
            .bind(tenant)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        tx.commit().await?;
        Ok(zone.filter(|z| z.parse::<chrono_tz::Tz>().is_ok()))
    }

    /// The workspace's settings administrators may change: `timezone` (IANA name), `language` (the
    /// interface's, for everyone who chose none),
    /// `fiscal_year_start_month` (1-12), `require_two_factor` (everyone signs in with an
    /// authenticator app; it applies from each person's next sign-in), `require_sso` (everyone
    /// but administrators signs in through a single sign-on provider), `shell_v2` (the shell
    /// and apps with interfaces of their own instead of today's web app) and `assistant` (the
    /// assistant, where the server has one; on unless turned off).
    pub async fn tenant_settings(&self, ctx: &Ctx) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let settings: Value = sqlx::query_scalar("select settings from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(serde_json::json!({
            "timezone": settings.get("timezone").cloned().unwrap_or_else(|| serde_json::json!("UTC")),
            "language": settings.get("language").cloned().unwrap_or(Value::Null),
            "fiscal_year_start_month": settings.get("fiscal_year_start_month").cloned().unwrap_or_else(|| serde_json::json!(1)),
            "require_two_factor": settings.get("require_two_factor").and_then(Value::as_bool).unwrap_or(false),
            "require_sso": settings.get("require_sso").and_then(Value::as_bool).unwrap_or(false),
            "shell_v2": settings.get("shell_v2").and_then(Value::as_bool).unwrap_or(false),
            // The assistant, where the server has one (on unless administrators turn it off).
            "assistant": settings.get("assistant").and_then(Value::as_bool).unwrap_or(true),
            "assistant_model": settings.get("assistant_model").cloned().unwrap_or(Value::Null),
            "assistant_monthly_tokens": settings.get("assistant_monthly_tokens").cloned().unwrap_or(Value::Null),
            "assistant_egress": settings.get("assistant_egress").cloned().unwrap_or_else(|| serde_json::json!({})),
            "offline": settings.get("offline").cloned().unwrap_or_else(|| serde_json::json!({"allowed": true, "hours": 72})),
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
                "language" => {
                    if !v.is_null() && v.as_str().is_none_or(|l| !valid_language(l)) {
                        return Err(Error::field("language", "must be a language tag (e.g. hi, ar, pt-BR), or empty for English"));
                    }
                    clean.insert(k.clone(), v.clone());
                }
                "fiscal_year_start_month" => {
                    let m = v.as_u64().filter(|m| (1..=12).contains(m)).ok_or_else(|| Error::field("fiscal_year_start_month", "must be 1 to 12"))?;
                    clean.insert(k.clone(), serde_json::json!(m));
                }
                // `shell_v2`: the workspace's people use the shell and apps with interfaces of their own
                // (docs/plan/apps-ui.md §14); off, today's web app.
                "require_two_factor" | "require_sso" | "shell_v2" | "assistant" => {
                    let on = v.as_bool().ok_or_else(|| Error::field(k.clone(), "must be true or false"))?;
                    clean.insert(k.clone(), serde_json::json!(on));
                }
                // Which of the operator's models this workspace uses (the server checks it is allowed).
                "assistant_model" => {
                    if !v.is_null() && v.as_str().is_none_or(|m| m.is_empty() || m.len() > 100) {
                        return Err(Error::field(k.clone(), "must be a model's name"));
                    }
                    clean.insert(k.clone(), v.clone());
                }
                // At most this many tokens to and from the model a month.
                "assistant_monthly_tokens" => {
                    if !v.is_null() && v.as_i64().is_none_or(|n| n < 0) {
                        return Err(Error::field(k.clone(), "must be a number of tokens"));
                    }
                    clean.insert(k.clone(), v.clone());
                }
                // What may leave for the AI provider, beyond what always may (crate::ctx::Egress).
                "assistant_egress" => {
                    serde_json::from_value::<crate::ctx::Egress>(v.clone()).map_err(|e| Error::field(k.clone(), e.to_string()))?;
                    clean.insert(k.clone(), v.clone());
                }
                // What devices may keep for offline use (docs/offline.md): `allowed`, and for how many hours.
                "offline" => {
                    let o = v.as_object().ok_or_else(|| Error::field("offline", "must be {\"allowed\": true or false, \"hours\": 1 to 720}"))?;
                    let fits = o.keys().all(|k| k == "allowed" || k == "hours")
                        && o.get("allowed").is_none_or(Value::is_boolean)
                        && o.get("hours").is_none_or(|h| h.as_i64().is_some_and(|h| (1..=720).contains(&h)));
                    if !fits {
                        return Err(Error::field("offline", "must be {\"allowed\": true or false, \"hours\": 1 to 720}"));
                    }
                    clean.insert(k.clone(), v.clone());
                }
                other => return Err(Error::field(other, "unknown setting")),
            }
        }
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        if clean.get("require_sso") == Some(&serde_json::json!(true)) {
            let any: bool =
                sqlx::query_scalar("select exists (select 1 from sso_provider where tenant_id = $1 and enabled)").bind(ctx.tenant).fetch_one(&mut *tx).await?;
            if !any {
                return Err(Error::field("require_sso", "set up and enable a single sign-on provider first"));
            }
        }
        sqlx::query("update tenant set settings = settings || $2 where id = $1").bind(ctx.tenant).bind(Value::Object(clean.clone())).execute(&mut *tx).await?;
        crate::audit::record(&mut tx, ctx, "Tenant", ctx.tenant, "settings", None, Value::Object(clean)).await?;
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        self.tenant_settings(ctx).await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn language_tags() {
        for ok in ["hi", "ar", "pt-BR", "zh-Hant-TW", "ckb"] {
            assert!(super::valid_language(ok), "{ok}");
        }
        for bad in ["", "e", "english", "hi_IN", "pt-", "zh-Hant-TW-extra-x-toolongpart", "../x", "ar<"] {
            assert!(!super::valid_language(bad), "{bad}");
        }
    }
}
