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
        check_slug(&self.slug)?;
        if self.name.trim().is_empty() || self.name.len() > 200 {
            return Err(Error::field("name", "is required (max 200 characters)"));
        }
        validate_password(&self.admin_password)
    }
}

/// A workspace's name as its addresses use it: 2-63 lowercase letters, digits or hyphens, and none the cluster keeps.
pub(crate) fn check_slug(slug: &str) -> Result<()> {
    if !SLUG.is_match(slug) {
        return Err(Error::field("slug", "use 2-63 lowercase letters, digits or hyphens"));
    }
    if crate::identity::RESERVED_SLUGS.contains(&slug) {
        return Err(Error::field("slug", "is a name the cluster keeps for itself"));
    }
    Ok(())
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
    /// Returns what it installed: a package the workspace holds newer than this build's is left
    /// as it is ([`Kernel::install_packages_reporting`] says which).
    pub async fn install_packages(&self, tenant: Uuid, names: &[String]) -> Result<Vec<String>> {
        Ok(self.install_packages_reporting(tenant, names).await?.packages)
    }

    /// [`Kernel::install_packages`], saying which packages it left alone because the workspace holds
    /// them newer than this build's (a server rolled back, or a workspace moved from a newer build):
    /// their version, roles and seed data stay the newer one's, never stamped or seeded older. The
    /// others install. This build's definitions are what the workspace runs on either way (the schema
    /// is built from the catalog, not from the stored version), so going on is no less safe than the
    /// workspace already is on this build; `ferrum task doctor` reports it as `packages.newer`.
    pub async fn install_packages_reporting(&self, tenant: Uuid, names: &[String]) -> Result<Installed> {
        let catalog = self.registry.catalog();
        let closure = catalog.closure(names).map_err(|e| Error::invalid(e.to_string()))?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let have: std::collections::HashMap<String, String> =
            sqlx::query_as("select name, version from installed_package where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await?.into_iter().collect();
        // What it would install beside what is there: a hook must not run without one it waits for.
        let unmet = self.hooks.unmet(&have.keys().cloned().chain(closure.iter().cloned()).collect::<Vec<_>>());
        if !unmet.is_empty() {
            return Err(Error::invalid(format!("these packages cannot be installed: {}", unmet.join("; "))));
        }
        let kept_newer = newer_here(&have, &closure, |p| catalog.package(p).map(|pkg| pkg.manifest.version.clone()));
        let closure: Vec<String> = closure.into_iter().filter(|p| !kept_newer.iter().any(|k| &k.name == p)).collect();
        for k in &kept_newer {
            tracing::warn!(%tenant, package = %k.name, have = %k.version, build = %k.build, "package kept: the workspace has it newer than this build");
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
                // A rule row follows the releases of the package that wrote it, and of none other: two packages
                // declaring a rule for the same role and entity leave the first one's row as it is (BD-40).
                for p in &role.permissions {
                    sqlx::query(
                        "insert into role_permission (tenant_id, role, entity, can_read, can_write, can_create, can_delete, can_submit, \
                         can_cancel, can_export, can_import, if_owner, read_groups, write_groups, match_rule, rule_key, via, actions, shared, hide, packaged, package) \
                         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, true, $21) \
                         on conflict (tenant_id, role, entity, if_owner, rule_key) do update set \
                         can_read = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_read else role_permission.can_read end, \
                         can_write = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_write else role_permission.can_write end, \
                         can_create = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_create else role_permission.can_create end, \
                         can_delete = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_delete else role_permission.can_delete end, \
                         can_submit = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_submit else role_permission.can_submit end, \
                         can_cancel = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_cancel else role_permission.can_cancel end, \
                         can_export = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_export else role_permission.can_export end, \
                         can_import = case when role_permission.packaged and role_permission.package = excluded.package then excluded.can_import else role_permission.can_import end, \
                         read_groups = case when role_permission.packaged and role_permission.package = excluded.package then excluded.read_groups else role_permission.read_groups end, \
                         write_groups = case when role_permission.packaged and role_permission.package = excluded.package then excluded.write_groups else role_permission.write_groups end, \
                         match_rule = case when role_permission.packaged and role_permission.package = excluded.package then excluded.match_rule else role_permission.match_rule end, \
                         via = case when role_permission.packaged and role_permission.package = excluded.package then excluded.via else role_permission.via end, \
                         actions = case when role_permission.packaged and role_permission.package = excluded.package then excluded.actions else role_permission.actions end, \
                         shared = case when role_permission.packaged and role_permission.package = excluded.package then excluded.shared else role_permission.shared end, \
                         hide = case when role_permission.packaged and role_permission.package = excluded.package then excluded.hide \
                           when (role_permission.packaged is null or (role_permission.packaged and role_permission.package is null)) and role_permission.hide = '{}' then excluded.hide \
                           else role_permission.hide end",
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
                    .bind(&p.shared)
                    .bind(&p.hide)
                    .bind(name)
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
            // Its automations, as defaults the workspace keeps, switches off or edits.
            if !pkg.manifest.automations.is_empty() {
                let mut tx = self.db.tenant_tx(tenant).await?;
                for a in &pkg.manifest.automations {
                    crate::automation::install_default(&ctx, &mut tx, name, a)
                        .await
                        .map_err(|e| Error::Internal(format!("{name}: automation {}: {e}", a.name)))?;
                }
                tx.commit().await?;
            }
        }
        self.report_conflicts(tenant).await?;
        Ok(Installed { packages: closure, kept_newer })
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
            "messages": settings.get("messages").cloned().unwrap_or_else(|| serde_json::json!({})),
            // The networks staff may work from (crate::gate); no networks: off.
            // Staff are told of a sign-in from a device they had not used.
            "new_device_notice": settings.get("new_device_notice").and_then(Value::as_bool).unwrap_or(false),
            // The administrators who keep password sign-in when single sign-on is required (null: all of them).
            "break_glass": settings.get("break_glass").cloned().unwrap_or(Value::Null),
            // Minutes of no use after which a browser session ends (0: never, as before).
            "idle_minutes": settings.get("idle_minutes").and_then(Value::as_i64).unwrap_or(0),
            "staff_networks": crate::gate::StaffNetworks::parse(settings.get("staff_networks")).map_err(Error::Internal)?.to_value(),
            // Entities whose work needs a recent step-up, and whose reads are recorded (crate::controls); null: off.
            "step_up": settings.get("step_up").cloned().unwrap_or(Value::Null),
            "record_reads": settings.get("record_reads").cloned().unwrap_or(Value::Null),
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
                // Messages to people (messages.rs): a calling code for numbers written without one, quiet hours, limits.
                "messages" => {
                    crate::messages::check_settings(v)?;
                    clean.insert(k.clone(), v.clone());
                }
                // The networks staff may work from (crate::gate). Saved in `enforce` only with the saver's own
                // address inside: nobody shuts themselves out by saving.
                "staff_networks" => {
                    let nets = crate::gate::StaffNetworks::parse(Some(v)).map_err(|m| Error::field(k.clone(), m))?;
                    let address = ctx.zone.as_ref().map(|e| e.address.as_str()).unwrap_or_default();
                    if nets.mode == crate::zones::Mode::Enforce && !nets.admits(address) {
                        return Err(Error::field(
                            k.clone(),
                            format!(
                                "your own address ({}) is not inside these networks, so saving them would shut you out: add it, or save them in report mode first",
                                if address.is_empty() { "unknown" } else { address }
                            ),
                        ));
                    }
                    clean.insert(k.clone(), if nets.is_off() { Value::Null } else { nets.to_value() });
                }
                // Browser sessions unused this many minutes end (0: never).
                "idle_minutes" => {
                    let (lo, hi) = (crate::shell::IDLE_MIN_MINUTES, crate::shell::IDLE_MAX_MINUTES);
                    let m = v
                        .as_i64()
                        .filter(|m| *m == 0 || (lo..=hi).contains(m))
                        .ok_or_else(|| Error::field(k.clone(), format!("must be 0 (never) or {lo} to {hi} minutes")))?;
                    clean.insert(k.clone(), serde_json::json!(m));
                }
                // Tell staff of a sign-in from a device they had not used (crate::gate).
                "new_device_notice" => {
                    let on = v.as_bool().ok_or_else(|| Error::field(k.clone(), "must be true or false"))?;
                    clean.insert(k.clone(), serde_json::json!(on));
                }
                // Checked against the workspace's administrators below (crate::gate::check_break_glass).
                "break_glass" => {}
                // Entities whose work needs a recent step-up, and whose reads are recorded (crate::controls). Each must be
                // an entity of the workspace that the saver's zone shows: a zone that leaves one out cannot set its rules.
                "step_up" | "record_reads" => {
                    let step_up = k == "step_up";
                    let kept = if v.is_null() {
                        Value::Null
                    } else {
                        let checked = if step_up { crate::controls::check_step_up(v) } else { crate::controls::check_reads(v) };
                        checked.map_err(|m| Error::field(k.clone(), m))?
                    };
                    for e in crate::controls::named(&kept, step_up) {
                        if ctx.schema.entity(&e).is_none() || !ctx.sees(&e) {
                            return Err(Error::field(k.clone(), format!("{e} is not an entity of this workspace")));
                        }
                    }
                    let empty = crate::controls::named(&kept, step_up).is_empty();
                    clean.insert(k.clone(), if empty { Value::Null } else { kept });
                }
                other => return Err(Error::field(other, "unknown setting")),
            }
        }
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        if let Some(v) = patch.get("break_glass") {
            clean.insert("break_glass".into(), crate::gate::check_break_glass(&mut tx, ctx.tenant, v).await?);
        }
        if clean.get("require_sso") == Some(&serde_json::json!(true)) {
            let any: bool =
                sqlx::query_scalar("select exists (select 1 from sso_provider where tenant_id = $1 and enabled)").bind(ctx.tenant).fetch_one(&mut *tx).await?;
            if !any {
                return Err(Error::field("require_sso", "set up and enable a single sign-on provider first"));
            }
        }
        // Protections an older build does not enforce: such builds may no longer start (crate::window::require). Turning
        // one off leaves its floor (docs/operations.md, "Protections an older build would not enforce").
        let on = |k: &str| clean.get(k).is_some_and(|v| !v.is_null() && v != &serde_json::json!(0));
        for (key, floor) in [
            ("staff_networks", &crate::gate::STAFF_NETWORKS_FLOOR),
            ("idle_minutes", &crate::gate::IDLE_FLOOR),
            ("break_glass", &crate::gate::BREAK_GLASS_FLOOR),
            ("step_up", &crate::controls::STEP_UP_FLOOR),
            ("record_reads", &crate::controls::READS_FLOOR),
        ] {
            if on(key) {
                crate::window::require(&mut tx, floor, "").await?;
            }
        }
        sqlx::query("update tenant set settings = settings || $2 where id = $1").bind(ctx.tenant).bind(Value::Object(clean.clone())).execute(&mut *tx).await?;
        // People's cached contexts hold the networks: every replica drops them now.
        let people = clean.contains_key("staff_networks");
        if people {
            crate::auth::notify_users(&mut tx, ctx.tenant, None).await?;
        }
        crate::audit::record_admin(&mut tx, ctx, "Tenant", ctx.tenant, "settings", None, Value::Object(clean)).await?;
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        if people {
            self.invalidate_tenant_users(ctx.tenant);
        }
        self.tenant_settings(ctx).await
    }
}

/// What [`Kernel::install_packages_reporting`] did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Installed {
    /// The packages installed (with their dependencies), in installation order.
    pub packages: Vec<String>,
    /// The packages left alone: the workspace holds them newer than this build's.
    pub kept_newer: Vec<KeptNewer>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KeptNewer {
    pub name: String,
    /// The workspace's version, which stays.
    pub version: String,
    /// This build's.
    pub build: String,
}

/// Of `closure`, the packages `have` holds newer than `build` says this build has.
fn newer_here(have: &std::collections::HashMap<String, String>, closure: &[String], build: impl Fn(&str) -> Option<String>) -> Vec<KeptNewer> {
    closure
        .iter()
        .filter_map(|p| {
            let (version, want) = (have.get(p)?, build(p)?);
            package_newer(version, &want).then(|| KeptNewer { name: p.clone(), version: version.clone(), build: want })
        })
        .collect()
}

/// Whether a workspace's package at `have` is newer than `want`: by semver only (a version that is
/// not one can only be told to differ, and counts as behind).
pub fn package_newer(have: &str, want: &str) -> bool {
    have != want && !package_behind(have, want)
}

/// Whether a workspace's package at `have` is behind `want`: by semver, or, when either is not one,
/// by differing at all.
pub fn package_behind(have: &str, want: &str) -> bool {
    match (semver::Version::parse(have), semver::Version::parse(want)) {
        (Ok(h), Ok(w)) => h < w,
        _ => have != want,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_package_is_behind_only_when_older() {
        use super::package_behind;
        assert!(package_behind("0.2.0", "0.4.0"));
        assert!(package_behind("0.9.0", "0.10.0"), "by semver, not by text");
        assert!(!package_behind("0.4.0", "0.4.0"));
        assert!(!package_behind("0.5.0", "0.4.0"), "a newer package (a rolled-back server) is not behind");
        assert!(package_behind("dev", "0.4.0"), "not a version: differing is all that can be told");
    }

    #[test]
    fn install_leaves_a_package_the_workspace_has_newer() {
        let build = |p: &str| match p {
            "core" => Some("0.5.0".to_string()),
            "crm" => Some("0.4.0".to_string()),
            "sales" => Some("0.3.0".to_string()),
            _ => None,
        };
        let have = |r: &[(&str, &str)]| r.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<std::collections::HashMap<_, _>>();
        let closure = |r: &[&str]| r.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let names = |k: Vec<super::KeptNewer>| k.into_iter().map(|k| k.name).collect::<Vec<_>>();
        // Older on newer: the workspace's crm 0.6.0 stays.
        let kept = super::newer_here(&have(&[("crm", "0.6.0")]), &closure(&["crm"]), build);
        assert_eq!(kept, vec![super::KeptNewer { name: "crm".into(), version: "0.6.0".into(), build: "0.4.0".into() }]);
        // Equal: reinstalled as today.
        assert!(super::newer_here(&have(&[("crm", "0.4.0")]), &closure(&["crm"]), build).is_empty());
        // Newer on older (an upgrade) and not installed yet: installed.
        assert!(super::newer_here(&have(&[("crm", "0.2.0")]), &closure(&["crm", "sales"]), build).is_empty());
        // A chain: sales depends on crm depends on core; core is newer here, crm behind, sales new.
        let kept = super::newer_here(&have(&[("core", "0.10.0"), ("crm", "0.3.9")]), &closure(&["core", "crm", "sales"]), build);
        assert_eq!(names(kept), vec!["core"], "only core is left; crm upgrades and sales installs");
        // Not a version, or unknown to this build: never kept (as `package_behind` says, differing is all that can be told).
        assert!(super::newer_here(&have(&[("crm", "dev"), ("gone", "9.0.0")]), &closure(&["crm", "gone"]), build).is_empty());
    }

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
