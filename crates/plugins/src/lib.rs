//! Tenant plugins: sandboxed WebAssembly that extends Ferrum or turns it into another platform
//! (EV charging, IoT, payments, telephony...).
//!
//! A plugin is a core WebAssembly module. Its only possible import is `ferrum.host_call`; it
//! exports `memory`, `ferrum_alloc(len) -> ptr` and `ferrum_hook(ptr, len) -> (ptr << 32 | len)`.
//! Every entry point arrives as JSON with a `kind`:
//!
//! * `hook` — a document lifecycle stage, inside the document's transaction, as the user;
//!   reply `{}`, `{"patch": {...}}` (before_validate) or a veto `{"error"}` / `{"errors": [...]}`.
//! * `http` — a request to one of its routes (`/api/ext/{plugin}/…`, or `/api/public/{tenant}/{plugin}/…`
//!   for public ones such as payment webhooks); reply `{"status", "headers", "body"}` or `{"json"}`.
//! * `event` — an outbox event it subscribed to (at least once, deduplicated per plugin).
//! * `schedule` — a schedule it declared fired (multi-replica safe).
//! * `socket` — a WebSocket it serves connected, sent a message or closed; reply
//!   `{"send": text | [texts], "close": bool}`.
//!
//! Through `host_call` (`{"op", "args"}` → `{"ok"}` / `{"error": {"code", "message"}}`) it can read
//! and write records (as its own service user with the permissions it declared, or as the calling
//! user limited to those permissions), keep private key-value data, call declared HTTPS hosts,
//! emit events, write to any of its sockets on any replica, log and read the clock. Each call is
//! a fresh instance with a fuel budget and a memory cap on the blocking pool; failures are
//! fail-closed and logged, and an administrator can disable the plugin at once.

pub mod events;
pub mod hooks;
pub mod host;
pub mod http;
pub mod runtime;
pub mod schedule;
pub mod sockets;

pub use events::PluginEvents;
pub use hooks::PluginHook;
pub use sockets::{Outgoing, SocketHub};

use dashmap::DashMap;
use ferrum_kernel::{Action, Ctx, Db, Error, FieldError, Kernel, Result};
use ferrum_meta::{EntityDef, OverlayOp, PermissionDef};
use runtime::HostApi;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wasmtime::{Config, Engine, ExternType, Module};

pub const STAGES: &[&str] = &["before_validate", "validate", "before_submit", "on_submit", "before_cancel", "on_cancel", "before_delete"];
const MAX_WASM: usize = 8 << 20;
const CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookBinding {
    /// An entity name, or `*` for every entity.
    pub entity: String,
    pub stage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    /// `/path`, or `/prefix/*` for everything below it.
    pub path: String,
    /// Callable without signing in (e.g. third-party webhooks); the plugin acts as itself.
    #[serde(default)]
    pub public: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    pub name: String,
    pub every_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SocketDef {
    pub path: String,
    #[serde(default)]
    pub public: bool,
    /// WebSocket subprotocols it speaks (e.g. `ocpp1.6`, `ocpp2.0.1`).
    #[serde(default)]
    pub subprotocols: Vec<String>,
}

/// What a plugin brings and may do; approved by the administrator who installs it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Manifest {
    pub hooks: Vec<HookBinding>,
    pub routes: Vec<Route>,
    /// Outbox topics: `SalesInvoice.submitted`, `SalesInvoice.*`, `plugin.ocpp.*`, `*`.
    pub subscriptions: Vec<String>,
    pub schedules: Vec<Schedule>,
    pub sockets: Vec<SocketDef>,
    /// Record access for its service user, and the most it may do on a user's behalf.
    pub permissions: Vec<PermissionDef>,
    /// Hosts it may call over HTTPS (`api.example.com`, `*.example.com`).
    pub http_hosts: Vec<String>,
    /// Entities it brings (stored without DDL like any custom entity; names start with `X`).
    pub entities: Vec<EntityDef>,
}

fn path_matches(pattern: &str, path: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(prefix) => path == prefix || path.starts_with(&format!("{prefix}/")),
        None => pattern == path,
    }
}

impl Manifest {
    pub fn allows(&self, entity: &str, action: Action) -> bool {
        self.permissions.iter().any(|p| {
            (p.entity == entity || p.entity == "*")
                && match action {
                    Action::Read => p.read,
                    Action::Write => p.write,
                    Action::Create => p.create,
                    Action::Delete => p.delete,
                    Action::Submit => p.submit,
                    Action::Cancel => p.cancel,
                    Action::Export => p.export,
                    Action::Import => p.import,
                }
        })
    }

    pub fn subscribes(&self, topic: &str) -> bool {
        self.subscriptions.iter().any(|s| s == "*" || s == topic || s.strip_suffix(".*").is_some_and(|prefix| topic.starts_with(&format!("{prefix}."))))
    }

    pub fn route(&self, path: &str) -> Option<&Route> {
        self.routes.iter().find(|r| path_matches(&r.path, path))
    }

    pub fn socket(&self, path: &str) -> Option<&SocketDef> {
        self.sockets.iter().find(|s| path_matches(&s.path, path))
    }

    fn validate(&self, ctx: &Ctx) -> Vec<FieldError> {
        let mut errors = Vec::new();
        let own: Vec<&str> = self.entities.iter().map(|e| e.name.as_str()).collect();
        let known = |e: &str| e == "*" || ctx.schema.entity(e).is_some() || own.contains(&e);
        let path_ok = |p: &str| p.starts_with('/') && p.len() <= 100 && !p.contains("..") && !p.contains("//");
        for (i, h) in self.hooks.iter().enumerate() {
            if !STAGES.contains(&h.stage.as_str()) {
                errors.push(FieldError::new("stage", format!("must be one of: {}", STAGES.join(", "))).in_row("hooks", i));
            }
            if !known(&h.entity) {
                errors.push(FieldError::new("entity", format!("unknown entity {}", h.entity)).in_row("hooks", i));
            }
        }
        for (i, r) in self.routes.iter().enumerate() {
            if !path_ok(&r.path) {
                errors.push(FieldError::new("path", "must start with / (at most 100 characters, no ..)").in_row("routes", i));
            }
        }
        for (i, s) in self.sockets.iter().enumerate() {
            if !path_ok(&s.path) {
                errors.push(FieldError::new("path", "must start with / (at most 100 characters, no ..)").in_row("sockets", i));
            }
        }
        for (i, s) in self.schedules.iter().enumerate() {
            if s.every_seconds < 60 {
                errors.push(FieldError::new("every_seconds", "must be at least 60").in_row("schedules", i));
            }
            if s.name.is_empty() || s.name.len() > 60 || self.schedules.iter().filter(|o| o.name == s.name).count() > 1 {
                errors.push(FieldError::new("name", "needs a unique name of at most 60 characters").in_row("schedules", i));
            }
        }
        for (i, t) in self.subscriptions.iter().enumerate() {
            if t.is_empty() || t.len() > 120 || !t.chars().all(|c| c.is_ascii_alphanumeric() || "._-*".contains(c)) {
                errors.push(FieldError::new("subscriptions", "use topics like SalesInvoice.submitted, SalesInvoice.* or *").in_row("subscriptions", i));
            }
        }
        for (i, p) in self.permissions.iter().enumerate() {
            if !known(&p.entity) {
                errors.push(FieldError::new("entity", format!("unknown entity {}", p.entity)).in_row("permissions", i));
            }
        }
        for (i, h) in self.http_hosts.iter().enumerate() {
            let bare = h.strip_prefix("*.").unwrap_or(h);
            if bare.is_empty() || bare.contains('*') || !bare.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)) || !bare.contains('.') {
                errors.push(FieldError::new("http_hosts", "use host names such as api.example.com or *.example.com").in_row("http_hosts", i));
            }
        }
        for (i, e) in self.entities.iter().enumerate() {
            if !e.name.starts_with('X') {
                errors.push(FieldError::new("entities", "plugin entities must be named with an X prefix (like custom entities)").in_row("entities", i));
            }
        }
        errors
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewPlugin {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub fuel_limit: Option<i64>,
    #[serde(default)]
    pub memory_limit_mb: Option<i32>,
    /// Settings and secrets the plugin reads from its input (`config`), set by the administrator.
    #[serde(default)]
    pub config: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub manifest: Manifest,
}

fn default_version() -> String {
    "0.1.0".into()
}

/// An enabled plugin, as cached per tenant.
#[derive(Clone)]
pub struct Loaded {
    pub id: Uuid,
    pub name: String,
    pub sha: String,
    pub manifest: Arc<Manifest>,
    pub config: Arc<Value>,
    pub user_id: Option<Uuid>,
    pub fuel: u64,
    pub memory_mb: u32,
}

pub struct PluginHost {
    engine: Engine,
    modules: DashMap<String, Module>,
    loaded: DashMap<Uuid, (Instant, Arc<Vec<Loaded>>)>,
    pub sockets: Arc<SocketHub>,
    allow_private_http: bool,
}

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("only administrators manage plugins".into())) }
}

/// `{"error": …}` / `{"errors": […]}` in a plugin reply, as a kernel error.
pub fn reply_error(reply: &Value) -> Option<Error> {
    match reply.get("error") {
        Some(Value::String(m)) => return Some(Error::Invalid(m.clone())),
        Some(Value::Object(o)) => return Some(Error::Invalid(o.get("message").and_then(Value::as_str).unwrap_or("refused by a plugin").to_string())),
        _ => {}
    }
    let list = reply.get("errors").and_then(Value::as_array)?;
    let errs: Vec<FieldError> = list
        .iter()
        .map(|e| {
            let err = FieldError::new(
                e.get("field").and_then(Value::as_str).unwrap_or("_"),
                e.get("message").and_then(Value::as_str).unwrap_or("refused by a plugin"),
            );
            // Optional `"row": ["items", 2]` points the error at a child row.
            match e.get("row").and_then(Value::as_array).map(Vec::as_slice) {
                Some([table, idx]) => match (table.as_str(), idx.as_u64()) {
                    (Some(t), Some(i)) => err.in_row(t, i as usize),
                    _ => err,
                },
                _ => err,
            }
        })
        .collect();
    (!errs.is_empty()).then_some(Error::Validation(errs))
}

pub fn user_json(ctx: &Ctx) -> Value {
    json!({"id": ctx.user, "email": ctx.email, "full_name": ctx.full_name, "roles": ctx.roles, "is_admin": ctx.is_admin})
}

impl PluginHost {
    /// `allow_private_http` lets plugins call private addresses and plain http (development only).
    pub fn new(allow_private_http: bool) -> Result<Arc<PluginHost>> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.max_wasm_stack(512 << 10);
        let engine = Engine::new(&config).map_err(|e| Error::internal(format!("wasm engine: {e}")))?;
        Ok(Arc::new(PluginHost { engine, modules: DashMap::new(), loaded: DashMap::new(), sockets: SocketHub::new(), allow_private_http }))
    }

    /// Compiles a module and checks the contract: its only import may be `ferrum.host_call`.
    pub fn validate(&self, wasm: &[u8]) -> std::result::Result<Module, String> {
        if wasm.len() > MAX_WASM {
            return Err(format!("module is larger than {} MB", MAX_WASM >> 20));
        }
        let module = Module::new(&self.engine, wasm).map_err(|e| format!("not a valid WebAssembly module: {e}"))?;
        for i in module.imports() {
            let ok =
                i.module() == "ferrum" && i.name() == "host_call" && matches!(i.ty(), ExternType::Func(f) if f.params().len() == 2 && f.results().len() == 1);
            if !ok {
                return Err(format!("plugins may only import ferrum.host_call (found {}::{})", i.module(), i.name()));
            }
        }
        for name in ["memory", "ferrum_alloc", "ferrum_hook"] {
            if module.get_export(name).is_none() {
                return Err(format!("missing export `{name}`"));
            }
        }
        Ok(module)
    }

    /// Runs one call without host access (tests, pure plugins).
    pub async fn execute(&self, module: &Module, fuel: u64, memory_mb: u32, input: &[u8]) -> std::result::Result<Vec<u8>, String> {
        runtime::run(&self.engine, module, fuel, memory_mb, input.to_vec(), &mut runtime::NoHost).await
    }

    pub fn invalidate(&self, tenant: Uuid) {
        self.loaded.remove(&tenant);
    }

    /// The tenant's enabled plugins (cached briefly; read on the side pool so it never competes
    /// with the caller's own transaction for a connection).
    pub async fn plugins(&self, db: &Db, tenant: Uuid) -> Result<Arc<Vec<Loaded>>> {
        if let Some(entry) = self.loaded.get(&tenant)
            && entry.0.elapsed() < CACHE_TTL
        {
            return Ok(entry.1.clone());
        }
        let mut tx = db.side_tenant_tx(tenant).await?;
        let rows: Vec<(Uuid, String, String, Value, Value, Value, Option<Uuid>, i64, i32)> = sqlx::query_as(
            "select id, name, sha256, hooks, manifest, config, user_id, fuel_limit, memory_limit_mb from plugin \
             where tenant_id = $1 and enabled order by name",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let list: Vec<Loaded> = rows
            .into_iter()
            .map(|(id, name, sha, hooks, manifest, config, user_id, fuel, mem)| {
                let mut m: Manifest = serde_json::from_value(manifest).unwrap_or_default();
                if m.hooks.is_empty() {
                    m.hooks = serde_json::from_value(hooks).unwrap_or_default();
                }
                Loaded { id, name, sha, manifest: Arc::new(m), config: Arc::new(config), user_id, fuel: fuel.max(0) as u64, memory_mb: mem.max(1) as u32 }
            })
            .collect();
        let list = Arc::new(list);
        self.loaded.insert(tenant, (Instant::now(), list.clone()));
        Ok(list)
    }

    pub async fn plugin(&self, db: &Db, tenant: Uuid, name: &str) -> Result<Loaded> {
        self.plugins(db, tenant).await?.iter().find(|p| p.name == name).cloned().ok_or_else(|| Error::NotFound(format!("plugin {name}")))
    }

    async fn module(&self, db: &Db, tenant: Uuid, sha: &str) -> Result<Module> {
        if let Some(m) = self.modules.get(sha) {
            return Ok(m.clone());
        }
        let mut tx = db.side_tenant_tx(tenant).await?;
        let wasm: Vec<u8> =
            sqlx::query_scalar("select wasm from plugin where tenant_id = $1 and sha256 = $2 limit 1").bind(tenant).bind(sha).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        let module = self.validate(&wasm).map_err(|e| Error::internal(format!("stored plugin no longer loads: {e}")))?;
        self.modules.insert(sha.to_string(), module.clone());
        Ok(module)
    }

    /// Runs `p` with `input`, serving its host calls through `api`. Returns the parsed reply.
    pub async fn invoke(&self, db: &Db, tenant: Uuid, p: &Loaded, mut input: Value, api: &mut dyn HostApi) -> Result<Value> {
        let module = self.module(db, tenant, &p.sha).await?;
        input["config"] = (*p.config).clone();
        let bytes = serde_json::to_vec(&input).map_err(Error::internal)?;
        let out = runtime::run(&self.engine, &module, p.fuel, p.memory_mb, bytes, api).await.map_err(|m| {
            tracing::warn!(plugin = %p.name, %tenant, "plugin failed: {m}");
            Error::Invalid(format!("plugin '{}' failed: {m}", p.name))
        })?;
        if out.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_slice(&out).map_err(|e| Error::Invalid(format!("plugin '{}' returned invalid JSON: {e}", p.name)))
    }

    /// The plugin's own identity: its service user with the permissions it declared.
    pub async fn service_ctx(&self, k: &Kernel, tenant: Uuid, p: &Loaded) -> Result<Ctx> {
        let user = p.user_id.ok_or_else(|| Error::invalid(format!("plugin '{}' has no service user; reinstall it", p.name)))?;
        k.load_ctx(tenant, user, None, format!("plugin-{}-{}", p.name, Uuid::now_v7())).await
    }

    // ───────────────────────── administration ─────────────────────────

    /// Installs a plugin, or upgrades the one with the same name: module, manifest, entities,
    /// service user and role, and schedules.
    pub async fn install(&self, k: &Kernel, ctx: &Ctx, p: NewPlugin, wasm: &[u8]) -> Result<Uuid> {
        require_admin(ctx)?;
        let mut errors = Vec::new();
        let name_ok = (2..=40).contains(&p.name.len())
            && p.name.starts_with(|c: char| c.is_ascii_lowercase())
            && p.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !name_ok {
            errors.push(FieldError::new("name", "use 2-40 lower-case letters, digits or '-', starting with a letter"));
        }
        let m = &p.manifest;
        if m.hooks.is_empty() && m.routes.is_empty() && m.subscriptions.is_empty() && m.schedules.is_empty() && m.sockets.is_empty() {
            errors.push(FieldError::new("hooks", "a plugin needs at least one hook, route, subscription, schedule or socket"));
        }
        errors.extend(m.validate(ctx));
        if let Err(msg) = self.validate(wasm) {
            errors.push(FieldError::new("wasm", msg));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }

        // Entities first, so its permissions can name them.
        for def in &m.entities {
            if ctx.schema.entity(&def.name).is_none() {
                k.add_customization(ctx, &def.name, OverlayOp::DefineEntity { entity: Box::new(def.clone()) }, Some(format!("plugin {}", p.name))).await?;
            }
        }
        let fresh;
        let ctx = if m.entities.is_empty() {
            ctx
        } else {
            fresh = k.load_ctx(ctx.tenant, ctx.user, None, ctx.request_id.clone()).await?;
            &fresh
        };

        // The service user acts for the plugin outside users' requests; its role carries exactly
        // the declared permissions.
        let role = format!("plugin:{}", p.name);
        let email = format!("{}@plugins.ferrum.invalid", p.name);
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into role (tenant_id, name) values ($1, $2) on conflict do nothing").bind(ctx.tenant).bind(&role).execute(&mut *tx).await?;
        sqlx::query("delete from role_permission where tenant_id = $1 and role = $2").bind(ctx.tenant).bind(&role).execute(&mut *tx).await?;
        let existing: Option<Uuid> =
            sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2").bind(ctx.tenant).bind(&email).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let user = match existing {
            Some(u) => {
                k.set_user_active(ctx, u, true).await?;
                k.set_user_roles(ctx, u, std::slice::from_ref(&role)).await?;
                u
            }
            None => k.create_user(ctx, &email, &format!("Plugin: {}", p.name), None, std::slice::from_ref(&role), false).await?,
        };
        for perm in &m.permissions {
            k.set_role_permission(ctx, &role, perm).await?;
        }

        let sha = hex::encode(Sha256::digest(wasm));
        let manifest = serde_json::to_value(m).map_err(Error::internal)?;
        let hooks = serde_json::to_value(&m.hooks).map_err(Error::internal)?;
        let config = Value::Object(p.config.clone().unwrap_or_default());
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let id: Uuid = sqlx::query_scalar(
            "insert into plugin (tenant_id, id, name, version, description, sha256, wasm, hooks, manifest, config, user_id, fuel_limit, memory_limit_mb, created_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
             on conflict (tenant_id, name) do update set version = excluded.version, description = excluded.description, sha256 = excluded.sha256, \
             wasm = excluded.wasm, hooks = excluded.hooks, manifest = excluded.manifest, \
             config = case when $15 then excluded.config else plugin.config end, user_id = excluded.user_id, \
             fuel_limit = excluded.fuel_limit, memory_limit_mb = excluded.memory_limit_mb, updated_at = now() \
             returning id",
        )
        .bind(ctx.tenant)
        .bind(Uuid::now_v7())
        .bind(&p.name)
        .bind(&p.version)
        .bind(&p.description)
        .bind(&sha)
        .bind(wasm)
        .bind(&hooks)
        .bind(&manifest)
        .bind(&config)
        .bind(user)
        .bind(p.fuel_limit.unwrap_or(50_000_000))
        .bind(p.memory_limit_mb.unwrap_or(32))
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(p.config.is_some())
        .fetch_one(&mut *tx)
        .await?;
        let names: Vec<String> = m.schedules.iter().map(|s| s.name.clone()).collect();
        sqlx::query("delete from plugin_schedule where tenant_id = $1 and plugin_id = $2 and not (name = any($3))")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&names)
            .execute(&mut *tx)
            .await?;
        for s in &m.schedules {
            sqlx::query(
                "insert into plugin_schedule (tenant_id, plugin_id, name, every_secs) values ($1, $2, $3, $4) \
                 on conflict (tenant_id, plugin_id, name) do update set every_secs = excluded.every_secs",
            )
            .bind(ctx.tenant)
            .bind(id)
            .bind(&s.name)
            .bind(s.every_seconds as i32)
            .execute(&mut *tx)
            .await?;
        }
        ferrum_kernel::audit::record(
            &mut tx,
            ctx,
            "Plugin",
            id,
            "install",
            None,
            json!({"name": p.name, "version": p.version, "sha256": sha, "manifest": manifest}),
        )
        .await?;
        tx.commit().await?;
        self.invalidate(ctx.tenant);
        tracing::info!(tenant = %ctx.tenant, plugin = %p.name, %sha, "plugin installed");
        Ok(id)
    }

    pub async fn list(&self, k: &Kernel, ctx: &Ctx) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String, Option<String>, String, Value, Value, i64, i32, bool, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "select id, name, version, description, sha256, manifest, config, fuel_limit, memory_limit_mb, enabled, updated_at \
             from plugin where tenant_id = $1 order by name",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, version, description, sha, manifest, config, fuel, mem, enabled, updated)| {
                // Configuration may hold secrets: show which keys are set, never their values.
                let config_keys: Vec<String> = config.as_object().map(|c| c.keys().cloned().collect()).unwrap_or_default();
                json!({"id": id, "name": name, "version": version, "description": description, "sha256": sha, "manifest": manifest,
                    "hooks": manifest.get("hooks"), "config_keys": config_keys, "fuel_limit": fuel, "memory_limit_mb": mem, "enabled": enabled,
                    "updated_at": updated})
            })
            .collect())
    }

    pub async fn set_enabled(&self, k: &Kernel, ctx: &Ctx, id: Uuid, enabled: bool) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update plugin set enabled = $3, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(enabled)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("plugin".into()));
        }
        ferrum_kernel::audit::record(&mut tx, ctx, "Plugin", id, if enabled { "enable" } else { "disable" }, None, json!({})).await?;
        tx.commit().await?;
        self.invalidate(ctx.tenant);
        Ok(())
    }

    /// Replaces the plugin's configuration (settings and secrets it reads from `config`).
    pub async fn set_config(&self, k: &Kernel, ctx: &Ctx, id: Uuid, config: Map<String, Value>) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update plugin set config = $3, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(Value::Object(config.clone()))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("plugin".into()));
        }
        let keys: Vec<&String> = config.keys().collect();
        ferrum_kernel::audit::record(&mut tx, ctx, "Plugin", id, "configure", None, json!({"keys": keys})).await?;
        tx.commit().await?;
        self.invalidate(ctx.tenant);
        Ok(())
    }

    /// Removes a plugin and its private data; its service user is deactivated. Records it created
    /// (and entities it defined) stay.
    pub async fn remove(&self, k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, Option<Uuid>)> = sqlx::query_as("delete from plugin where tenant_id = $1 and id = $2 returning name, user_id")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let (name, user) = row.ok_or_else(|| Error::NotFound("plugin".into()))?;
        sqlx::query("delete from role_permission where tenant_id = $1 and role = $2").bind(ctx.tenant).bind(format!("plugin:{name}")).execute(&mut *tx).await?;
        ferrum_kernel::audit::record(&mut tx, ctx, "Plugin", id, "remove", None, json!({"name": name})).await?;
        tx.commit().await?;
        if let Some(u) = user {
            k.set_user_active(ctx, u, false).await?;
        }
        self.invalidate(ctx.tenant);
        Ok(())
    }
}

/// A plugin in WebAssembly text that always answers `reply` (for tests and documentation).
pub fn constant_plugin_wat(reply: &str) -> String {
    let escaped: String = reply.bytes().map(|b| format!("\\{b:02x}")).collect();
    format!(
        r#"(module
  (memory (export "memory") 2)
  (global $next (mut i32) (i32.const 4096))
  (func (export "ferrum_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $next))
    (global.set $next (i32.add (global.get $next) (local.get $n)))
    (local.get $p))
  (data (i32.const 16) "{escaped}")
  (func (export "ferrum_hook") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const {len}))))"#,
        len = reply.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Arc<PluginHost> {
        PluginHost::new(false).unwrap()
    }

    fn compile(h: &PluginHost, wat: &str) -> Module {
        h.validate(wat.as_bytes()).unwrap_or_else(|e| panic!("{e}"))
    }

    #[tokio::test]
    async fn answers_through_linear_memory() {
        let h = host();
        let m = compile(&h, &constant_plugin_wat(r#"{"error":"blocked"}"#));
        let out = h.execute(&m, 1_000_000, 4, br#"{"doc":{}}"#).await.unwrap();
        assert_eq!(out, br#"{"error":"blocked"}"#);
    }

    #[tokio::test]
    async fn infinite_loops_run_out_of_fuel() {
        let h = host();
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64) (loop $l (br $l)) (i64.const 0)))"#;
        let m = compile(&h, wat);
        let err = h.execute(&m, 100_000, 4, b"{}").await.unwrap_err();
        assert!(err.contains("CPU budget"), "{err}");
    }

    #[tokio::test]
    async fn memory_is_capped() {
        let h = host();
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64)
              (if (i32.eq (memory.grow (i32.const 200)) (i32.const -1)) (then unreachable))
              (i64.const 0)))"#;
        let m = compile(&h, wat);
        let err = h.execute(&m, 1_000_000, 4, b"{}").await.unwrap_err();
        assert!(err.contains("trapped"), "{err}");
    }

    #[test]
    fn only_host_call_may_be_imported() {
        let h = host();
        let wasi = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1))"#;
        assert!(h.validate(wasi.as_bytes()).unwrap_err().contains("may only import"));
        let host_call = r#"(module (import "ferrum" "host_call" (func (param i32 i32) (result i64))) (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64) (i64.const 0)))"#;
        assert!(h.validate(host_call.as_bytes()).is_ok());
        assert!(h.validate(br#"(module (memory (export "memory") 1))"#).unwrap_err().contains("missing export"));
        assert!(h.validate(b"not wasm").is_err());
    }

    #[tokio::test]
    async fn outputs_outside_memory_are_rejected() {
        let h = host();
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64) (i64.const 0x0000fff000000100)))"#;
        let m = compile(&h, wat);
        assert!(h.execute(&m, 1_000_000, 4, b"{}").await.unwrap_err().contains("outside memory"));
    }

    #[test]
    fn manifest_matching() {
        let m: Manifest = serde_json::from_value(json!({
            "subscriptions": ["SalesInvoice.*", "plugin.ocpp.boot"],
            "routes": [{"path": "/webhook"}, {"path": "/api/*", "public": true}],
            "permissions": [{"entity": "Customer", "read": true}],
        }))
        .unwrap();
        assert!(m.subscribes("SalesInvoice.submitted") && m.subscribes("plugin.ocpp.boot") && !m.subscribes("SalesOrder.created"));
        assert!(m.route("/webhook").is_some() && m.route("/api/x/y").is_some_and(|r| r.public) && m.route("/other").is_none());
        assert!(m.allows("Customer", Action::Read) && !m.allows("Customer", Action::Write) && !m.allows("Item", Action::Read));
    }
}
