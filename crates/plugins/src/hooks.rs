//! Document hooks: every plugin bound to an entity and stage runs inside the document's
//! transaction, as the user, limited to the permissions it declared.

use crate::host::{Host, MAX_DEPTH, Scope};
use crate::{PluginHost, reply_error, user_json};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct PluginHook(pub Arc<PluginHost>);

impl PluginHook {
    async fn run(&self, hc: &mut HookCtx<'_>, stage: &str, doc: &Doc) -> Result<Doc> {
        let mut patch = Doc::new();
        if hc.ctx.depth > MAX_DEPTH {
            return Err(Error::invalid("plugin hooks nest too deeply"));
        }
        let entity = hc.entity.def.name.clone();
        let (db, tenant) = (hc.kernel.db.clone(), hc.ctx.tenant);
        let plugins = self.0.plugins(&db, tenant).await?;
        for p in plugins.iter().filter(|p| p.manifest.hooks.iter().any(|h| h.stage == stage && (h.entity == entity || h.entity == "*"))) {
            let input = json!({"kind": "hook", "entity": entity, "stage": stage, "doc": doc, "user": user_json(hc.ctx)});
            let reply = {
                let mut api = Host { host: &self.0, plugin: p, scope: Scope::Hook(&mut *hc), limit_to_manifest: true };
                self.0.invoke(&db, tenant, p, input, &mut api).await?
            };
            if let Some(e) = reply_error(&reply) {
                return Err(e);
            }
            if let Some(changes) = reply.get("patch").and_then(Value::as_object) {
                if stage != "before_validate" {
                    return Err(Error::Invalid(format!("plugin '{}' may only change documents in before_validate", p.name)));
                }
                for (k, v) in changes {
                    let f =
                        hc.entity.field(k).filter(|f| f.of.is_none()).ok_or_else(|| Error::Invalid(format!("plugin '{}' set unknown field {k}", p.name)))?;
                    patch.insert(k.clone(), ferrum_kernel::value::normalize(f, v).map_err(|m| Error::field(k.clone(), m))?);
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
