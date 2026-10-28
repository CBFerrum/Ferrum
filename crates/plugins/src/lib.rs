//! Tenant plugins: WebAssembly modules attached to document hooks, run in a sandbox.
//!
//! A plugin is a core WebAssembly module with **no imports**: no WASI and no host calls, so it
//! cannot reach files, the network, the clock or the database. It exports:
//!
//! * `memory`
//! * `ferrum_alloc(len: i32) -> i32` — a buffer the host writes the input into
//! * `ferrum_hook(ptr: i32, len: i32) -> i64` — handles the input, returns `(out_ptr << 32) | out_len`
//!
//! Input: `{"entity", "stage", "doc", "user": {"id", "email", "roles"}}` (canonical JSON, decimals
//! as strings). Output: `{}` (no opinion), `{"patch": {field: value}}` (only in `before_validate`),
//! or a veto: `{"error": "message"}` / `{"errors": [{"field", "message", "row"?: [table, index]}]}`.
//!
//! Each call gets a fresh instance with a fuel budget (a deterministic CPU limit) and a memory
//! cap, on the blocking pool so a busy plugin never stalls the async executor. Failures are
//! fail-closed: a trapping or exhausted plugin aborts the operation with a clear message, and an
//! administrator can disable it.

use async_trait::async_trait;
use dashmap::DashMap;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wasmtime::{Config, Engine, Instance, Module, Store, StoreLimits, StoreLimitsBuilder, Trap};

pub const STAGES: &[&str] = &["before_validate", "validate", "before_submit", "on_submit", "before_cancel", "on_cancel", "before_delete"];
const MAX_WASM: usize = 8 << 20;
const MAX_OUTPUT: usize = 1 << 20;
const BINDINGS_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookBinding {
    /// An entity name, or `*` for every entity.
    pub entity: String,
    pub stage: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewPlugin {
    pub name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    pub hooks: Vec<HookBinding>,
    #[serde(default)]
    pub fuel_limit: Option<i64>,
    #[serde(default)]
    pub memory_limit_mb: Option<i32>,
}

fn default_version() -> String {
    "0.1.0".into()
}

#[derive(Clone)]
struct Bound {
    name: String,
    sha: String,
    entity: String,
    stage: String,
    fuel: u64,
    memory_mb: u32,
}

struct Limits {
    limits: StoreLimits,
}

pub struct PluginHost {
    engine: Engine,
    modules: DashMap<String, Module>,
    bindings: DashMap<Uuid, (Instant, Arc<Vec<Bound>>)>,
}

impl PluginHost {
    pub fn new() -> Result<Arc<PluginHost>> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.max_wasm_stack(512 << 10);
        let engine = Engine::new(&config).map_err(|e| Error::internal(format!("wasm engine: {e}")))?;
        Ok(Arc::new(PluginHost { engine, modules: DashMap::new(), bindings: DashMap::new() }))
    }

    /// Compiles a module and checks the plugin contract: no imports, the three exports.
    pub fn validate(&self, wasm: &[u8]) -> std::result::Result<Module, String> {
        if wasm.len() > MAX_WASM {
            return Err(format!("module is larger than {} MB", MAX_WASM >> 20));
        }
        let module = Module::new(&self.engine, wasm).map_err(|e| format!("not a valid WebAssembly module: {e}"))?;
        if let Some(i) = module.imports().next() {
            return Err(format!("plugins may not import anything (found {}::{})", i.module(), i.name()));
        }
        for name in ["memory", "ferrum_alloc", "ferrum_hook"] {
            if module.get_export(name).is_none() {
                return Err(format!("missing export `{name}`"));
            }
        }
        Ok(module)
    }

    /// Runs one call in a fresh, limited instance.
    pub fn execute(&self, module: &Module, fuel: u64, memory_mb: u32, input: &[u8]) -> std::result::Result<Vec<u8>, String> {
        let limits = StoreLimitsBuilder::new().memory_size((memory_mb as usize) << 20).instances(1).memories(1).tables(4).table_elements(100_000).build();
        let mut store = Store::new(&self.engine, Limits { limits });
        store.limiter(|s| &mut s.limits);
        store.set_fuel(fuel).map_err(|e| e.to_string())?;
        let describe = |e: wasmtime::Error| match e.downcast_ref::<Trap>() {
            Some(Trap::OutOfFuel) => "exceeded its CPU budget".to_string(),
            Some(t) => format!("trapped: {t}"),
            None => e.to_string(),
        };
        let instance = Instance::new(&mut store, module, &[]).map_err(describe)?;
        let memory = instance.get_memory(&mut store, "memory").ok_or("missing export `memory`")?;
        let alloc = instance.get_typed_func::<i32, i32>(&mut store, "ferrum_alloc").map_err(describe)?;
        let hook = instance.get_typed_func::<(i32, i32), i64>(&mut store, "ferrum_hook").map_err(describe)?;
        let len = i32::try_from(input.len()).map_err(|_| "input too large".to_string())?;
        let ptr = alloc.call(&mut store, len).map_err(describe)?;
        memory.write(&mut store, ptr as u32 as usize, input).map_err(|_| "ferrum_alloc returned an invalid buffer".to_string())?;
        let packed = hook.call(&mut store, (ptr, len)).map_err(describe)? as u64;
        let (out_ptr, out_len) = ((packed >> 32) as usize, (packed & 0xffff_ffff) as usize);
        if out_len > MAX_OUTPUT {
            return Err(format!("output larger than {} KB", MAX_OUTPUT >> 10));
        }
        let mut out = vec![0; out_len];
        memory.read(&store, out_ptr, &mut out).map_err(|_| "output points outside memory".to_string())?;
        Ok(out)
    }

    pub fn invalidate(&self, tenant: Uuid) {
        self.bindings.remove(&tenant);
    }

    async fn bound(&self, tx: &mut Tx, tenant: Uuid) -> Result<Arc<Vec<Bound>>> {
        if let Some(entry) = self.bindings.get(&tenant)
            && entry.0.elapsed() < BINDINGS_TTL
        {
            return Ok(entry.1.clone());
        }
        let rows: Vec<(String, String, Value, i64, i32)> =
            sqlx::query_as("select name, sha256, hooks, fuel_limit, memory_limit_mb from plugin where tenant_id = $1 and enabled order by name")
                .bind(tenant)
                .fetch_all(&mut **tx)
                .await?;
        let mut out = Vec::new();
        for (name, sha, hooks, fuel, mem) in rows {
            for h in serde_json::from_value::<Vec<HookBinding>>(hooks).unwrap_or_default() {
                out.push(Bound {
                    name: name.clone(),
                    sha: sha.clone(),
                    entity: h.entity,
                    stage: h.stage,
                    fuel: fuel.max(0) as u64,
                    memory_mb: mem.max(1) as u32,
                });
            }
        }
        let out = Arc::new(out);
        self.bindings.insert(tenant, (Instant::now(), out.clone()));
        Ok(out)
    }

    async fn module(&self, tx: &mut Tx, tenant: Uuid, sha: &str) -> Result<Module> {
        if let Some(m) = self.modules.get(sha) {
            return Ok(m.clone());
        }
        let wasm: Vec<u8> =
            sqlx::query_scalar("select wasm from plugin where tenant_id = $1 and sha256 = $2 limit 1").bind(tenant).bind(sha).fetch_one(&mut **tx).await?;
        let module = self.validate(&wasm).map_err(|e| Error::internal(format!("stored plugin no longer loads: {e}")))?;
        self.modules.insert(sha.to_string(), module.clone());
        Ok(module)
    }

    // ───────────────────────── administration ─────────────────────────

    fn require_admin(ctx: &Ctx) -> Result<()> {
        if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("only administrators manage plugins".into())) }
    }

    /// Installs a plugin, or replaces the module and bindings of one with the same name.
    pub async fn install(&self, k: &Kernel, ctx: &Ctx, p: NewPlugin, wasm: &[u8]) -> Result<Uuid> {
        Self::require_admin(ctx)?;
        let mut errors = Vec::new();
        let name_ok = p.name.len() >= 2 && p.name.len() <= 63 && p.name.starts_with(|c: char| c.is_ascii_lowercase());
        if !name_ok || !p.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            errors.push(FieldError::new("name", "use 2-63 lower-case letters, digits, '-' or '_', starting with a letter"));
        }
        if p.hooks.is_empty() {
            errors.push(FieldError::new("hooks", "bind the plugin to at least one entity and stage"));
        }
        for (i, h) in p.hooks.iter().enumerate() {
            if !STAGES.contains(&h.stage.as_str()) {
                errors.push(FieldError::new("stage", format!("must be one of: {}", STAGES.join(", "))).in_row("hooks", i));
            }
            if h.entity != "*" && ctx.schema.entity(&h.entity).is_none() {
                errors.push(FieldError::new("entity", format!("unknown entity {}", h.entity)).in_row("hooks", i));
            }
        }
        if let Err(m) = self.validate(wasm) {
            errors.push(FieldError::new("wasm", m));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        let sha = hex::encode(Sha256::digest(wasm));
        let hooks = serde_json::to_value(&p.hooks).map_err(Error::internal)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let id: Uuid = sqlx::query_scalar(
            "insert into plugin (tenant_id, id, name, version, description, sha256, wasm, hooks, fuel_limit, memory_limit_mb, created_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             on conflict (tenant_id, name) do update set version = excluded.version, description = excluded.description, sha256 = excluded.sha256, \
             wasm = excluded.wasm, hooks = excluded.hooks, fuel_limit = excluded.fuel_limit, memory_limit_mb = excluded.memory_limit_mb, updated_at = now() \
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
        .bind(p.fuel_limit.unwrap_or(50_000_000))
        .bind(p.memory_limit_mb.unwrap_or(32))
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .fetch_one(&mut *tx)
        .await?;
        ferrum_kernel::audit::record(&mut tx, ctx, "Plugin", id, "install", None, json!({"name": p.name, "version": p.version, "sha256": sha, "hooks": hooks}))
            .await?;
        tx.commit().await?;
        self.invalidate(ctx.tenant);
        tracing::info!(tenant = %ctx.tenant, plugin = %p.name, %sha, "plugin installed");
        Ok(id)
    }

    pub async fn list(&self, k: &Kernel, ctx: &Ctx) -> Result<Vec<Value>> {
        Self::require_admin(ctx)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String, Option<String>, String, Value, i64, i32, bool, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "select id, name, version, description, sha256, hooks, fuel_limit, memory_limit_mb, enabled, updated_at from plugin where tenant_id = $1 order by name",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, version, description, sha, hooks, fuel, mem, enabled, updated)| {
                json!({"id": id, "name": name, "version": version, "description": description, "sha256": sha, "hooks": hooks,
                    "fuel_limit": fuel, "memory_limit_mb": mem, "enabled": enabled, "updated_at": updated})
            })
            .collect())
    }

    pub async fn set_enabled(&self, k: &Kernel, ctx: &Ctx, id: Uuid, enabled: bool) -> Result<()> {
        Self::require_admin(ctx)?;
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

    pub async fn remove(&self, k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<()> {
        Self::require_admin(ctx)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from plugin where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("plugin".into()));
        }
        ferrum_kernel::audit::record(&mut tx, ctx, "Plugin", id, "remove", None, json!({})).await?;
        tx.commit().await?;
        self.invalidate(ctx.tenant);
        Ok(())
    }
}

/// Dispatches every document hook to the tenant's plugins bound to that entity and stage.
pub struct PluginHook(pub Arc<PluginHost>);

impl PluginHook {
    async fn run(&self, hc: &mut HookCtx<'_>, stage: &str, doc: &Doc) -> Result<Doc> {
        let entity = hc.entity.def.name.clone();
        let bound = self.0.bound(hc.tx, hc.ctx.tenant).await?;
        let mine: Vec<Bound> = bound.iter().filter(|b| b.stage == stage && (b.entity == entity || b.entity == "*")).cloned().collect();
        let mut patch = Doc::new();
        if mine.is_empty() {
            return Ok(patch);
        }
        let input = serde_json::to_vec(&json!({
            "entity": entity,
            "stage": stage,
            "doc": doc,
            "user": {"id": hc.ctx.user, "email": hc.ctx.email, "roles": hc.ctx.roles},
        }))
        .map_err(Error::internal)?;
        for b in mine {
            let module = self.0.module(hc.tx, hc.ctx.tenant, &b.sha).await?;
            let (host, input, fuel, mem) = (self.0.clone(), input.clone(), b.fuel, b.memory_mb);
            let out = tokio::task::spawn_blocking(move || host.execute(&module, fuel, mem, &input))
                .await
                .map_err(|e| Error::internal(format!("plugin task: {e}")))?
                .map_err(|m| {
                    tracing::warn!(plugin = %b.name, %stage, %entity, "plugin failed: {m}");
                    Error::Invalid(format!("plugin '{}' failed: {m}", b.name))
                })?;
            let reply: Value = if out.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&out).map_err(|e| Error::Invalid(format!("plugin '{}' returned invalid JSON: {e}", b.name)))?
            };
            if let Some(msg) = reply.get("error").and_then(Value::as_str) {
                return Err(Error::Invalid(msg.to_string()));
            }
            if let Some(list) = reply.get("errors").and_then(Value::as_array) {
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
                if !errs.is_empty() {
                    return Err(Error::Validation(errs));
                }
            }
            if let Some(p) = reply.get("patch").and_then(Value::as_object) {
                if stage != "before_validate" {
                    return Err(Error::Invalid(format!("plugin '{}' may only change documents in before_validate", b.name)));
                }
                for (k, v) in p {
                    let f =
                        hc.entity.field(k).filter(|f| f.of.is_none()).ok_or_else(|| Error::Invalid(format!("plugin '{}' set unknown field {k}", b.name)))?;
                    let value = ferrum_kernel::value::normalize(f, v).map_err(|m| Error::field(k.clone(), m))?;
                    patch.insert(k.clone(), value);
                }
            }
        }
        Ok(patch)
    }
}

#[async_trait]
impl DocHook for PluginHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let patch = self.run(hc, "before_validate", doc).await?;
        doc.extend(patch);
        Ok(())
    }
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.run(hc, "validate", doc).await.map(drop)
    }
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        self.run(hc, "before_submit", doc).await.map(drop)
    }
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.run(hc, "on_submit", doc).await.map(drop)
    }
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.run(hc, "before_cancel", doc).await.map(drop)
    }
    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.run(hc, "on_cancel", doc).await.map(drop)
    }
    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.run(hc, "before_delete", doc).await.map(drop)
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
        PluginHost::new().unwrap()
    }

    fn compile(h: &PluginHost, wat: &str) -> Module {
        h.validate(wat.as_bytes()).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn answers_through_linear_memory() {
        let h = host();
        let m = compile(&h, &constant_plugin_wat(r#"{"error":"blocked"}"#));
        let out = h.execute(&m, 1_000_000, 4, br#"{"doc":{}}"#).unwrap();
        assert_eq!(out, br#"{"error":"blocked"}"#);
    }

    #[test]
    fn infinite_loops_run_out_of_fuel() {
        let h = host();
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64) (loop $l (br $l)) (i64.const 0)))"#;
        let m = compile(&h, wat);
        let err = h.execute(&m, 100_000, 4, b"{}").unwrap_err();
        assert!(err.contains("CPU budget"), "{err}");
    }

    #[test]
    fn memory_is_capped() {
        let h = host();
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64)
              (if (i32.eq (memory.grow (i32.const 200)) (i32.const -1)) (then unreachable))
              (i64.const 0)))"#;
        let m = compile(&h, wat);
        let err = h.execute(&m, 1_000_000, 4, b"{}").unwrap_err();
        assert!(err.contains("trapped"), "{err}");
    }

    #[test]
    fn imports_and_missing_exports_are_refused() {
        let h = host();
        let with_import = r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1))"#;
        assert!(h.validate(with_import.as_bytes()).unwrap_err().contains("may not import"));
        assert!(h.validate(br#"(module (memory (export "memory") 1))"#).unwrap_err().contains("missing export"));
        assert!(h.validate(b"not wasm").is_err());
    }

    #[test]
    fn outputs_outside_memory_are_rejected() {
        let h = host();
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "ferrum_alloc") (param i32) (result i32) (i32.const 0))
            (func (export "ferrum_hook") (param i32 i32) (result i64) (i64.const 0x0000fff000000100)))"#;
        let m = compile(&h, wat);
        assert!(h.execute(&m, 1_000_000, 4, b"{}").unwrap_err().contains("outside memory"));
    }
}
