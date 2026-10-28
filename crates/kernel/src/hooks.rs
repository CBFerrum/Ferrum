//! Native extension points. First-party packages implement `DocHook` in Rust; plugin hosts adapt
//! sandboxed code to the same trait. Hooks run inside the command transaction and may veto by
//! returning an error. Order: registration order (deterministic, set at startup).

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::Result;
use crate::{Doc, Kernel};
use async_trait::async_trait;
use ferrum_meta::EffEntity;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct HookCtx<'a> {
    pub kernel: &'a Kernel,
    pub ctx: &'a Ctx,
    pub tx: &'a mut Tx,
    pub entity: &'a Arc<EffEntity>,
}

#[allow(unused_variables)]
#[async_trait]
pub trait DocHook: Send + Sync {
    /// Fill derived values (totals, defaults from linked records). May modify the document.
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        Ok(())
    }
    /// Domain validation beyond metadata rules.
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        Ok(())
    }
    /// After insert/update was written (same transaction).
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        Ok(())
    }
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        Ok(())
    }
    /// Post to ledgers, update linked documents. The document is already stored as submitted.
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        Ok(())
    }
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        Ok(())
    }
    /// Reverse postings. The document is already stored as cancelled.
    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        Ok(())
    }
    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        Ok(())
    }
    /// Custom actions declared in metadata. Return `Some` when handled.
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        Ok(None)
    }
}

/// Consumes outbox events after webhooks and automations (e.g. plugin subscriptions). Delivery is
/// at least once: an error makes the event retry, so implementations must deduplicate.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn on_event(&self, kernel: &Kernel, event: &crate::outbox::Event) -> Result<()>;
}

#[derive(Default, Clone)]
pub struct HookRegistry {
    by_entity: HashMap<String, Vec<Arc<dyn DocHook>>>,
    sinks: Vec<Arc<dyn EventSink>>,
}

impl HookRegistry {
    pub fn register(&mut self, entity: &str, hook: Arc<dyn DocHook>) -> &mut Self {
        self.by_entity.entry(entity.to_string()).or_default().push(hook);
        self
    }

    /// Hooks for `entity`, followed by hooks registered for every entity (`"*"`, e.g. plugins),
    /// so those see the values the entity's own hooks derived.
    pub fn for_entity(&self, entity: &str) -> Vec<Arc<dyn DocHook>> {
        let mut out = self.by_entity.get(entity).cloned().unwrap_or_default();
        if entity != "*" {
            out.extend(self.by_entity.get("*").cloned().unwrap_or_default());
        }
        out
    }

    pub fn add_event_sink(&mut self, sink: Arc<dyn EventSink>) -> &mut Self {
        self.sinks.push(sink);
        self
    }

    pub fn event_sinks(&self) -> &[Arc<dyn EventSink>] {
        &self.sinks
    }

    pub fn entities(&self) -> impl Iterator<Item = &String> {
        self.by_entity.keys()
    }
}
