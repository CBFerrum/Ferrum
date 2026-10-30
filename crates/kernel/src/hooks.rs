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
use uuid::Uuid;

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
    /// Validation that compares with the stored version (`old` is `None` for a new document), for
    /// edit guards such as "the time of a confirmed booking cannot change". Runs after `validate`.
    async fn validate_change(&self, hc: &mut HookCtx<'_>, old: Option<&Doc>, doc: &Doc) -> Result<()> {
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
    /// After any hook handled action `name`, in the same transaction, with the document as the
    /// action left it: lets an app extend a platform action (re-costing a BOM's operations when
    /// "Update Cost" re-costs its materials) and add to its `result`.
    async fn after_action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, result: &mut Value) -> Result<()> {
        Ok(())
    }
}

/// Consumes outbox events after webhooks and automations (e.g. plugin subscriptions). Delivery is
/// at least once: an error makes the event retry, so implementations must deduplicate.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn on_event(&self, kernel: &Kernel, event: &crate::outbox::Event) -> Result<()>;
}

/// An HTTP request to a package's own endpoints: `/api/app/{package}/{path}` for signed-in users
/// (`user` is set) or `/api/site/{tenant}/{package}/{path}` for the public — booking pages, web
/// forms, storefronts, calendar feeds, inbound webhooks. Public requests carry no user: the package
/// decides what an anonymous caller may do and acts with a system or service context itself.
#[derive(Debug, Clone)]
pub struct SiteRequest {
    pub tenant: uuid::Uuid,
    pub user: Option<Ctx>,
    pub method: String,
    /// Starts with `/`, relative to the package's prefix.
    pub path: String,
    pub query: serde_json::Map<String, Value>,
    /// Request headers without credentials (authorization, cookie).
    pub headers: serde_json::Map<String, Value>,
    pub body: Vec<u8>,
    /// The caller's address, for rate limits and audit.
    pub client_ip: String,
}

#[derive(Debug, Clone)]
pub struct SiteResponse {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl SiteResponse {
    pub fn json(status: u16, value: &Value) -> Self {
        Self { status, content_type: "application/json".into(), headers: vec![], body: value.to_string().into_bytes() }
    }
    pub fn text(status: u16, content_type: &str, body: impl Into<String>) -> Self {
        Self { status, content_type: content_type.into(), headers: vec![], body: body.into().into_bytes() }
    }
}

/// A package's HTTP endpoints (see `SiteRequest`). Errors map to the usual API error responses.
#[async_trait]
pub trait Site: Send + Sync {
    async fn handle(&self, kernel: &Kernel, req: SiteRequest) -> Result<SiteResponse>;
    /// Largest request body this package accepts (the server reads no further, and caps it at 1 GiB).
    fn max_body(&self) -> usize {
        1 << 20
    }
    /// Largest body for one route (`path` is relative to the package, e.g. `upload/abc`), so
    /// only the routes that take files accept large bodies. Defaults to [`Site::max_body`].
    fn max_body_for(&self, method: &str, path: &str) -> usize {
        let _ = (method, path);
        self.max_body()
    }
}

/// What a person may do with a co-edited document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DocAccess {
    None,
    Read,
    Write,
}

/// A package whose records are documents people edit together live, at `/ws/docs/{package}/{id}`
/// (docs/plan/collaboration.md, C3). The server keeps the document itself (a Yjs document, its
/// changes in PostgreSQL); the package says who may open it, gives its first content, and keeps
/// what the server hands it after a stretch of editing.
#[async_trait]
pub trait DocHost: Send + Sync {
    /// What `ctx` may do with document `doc`; [`DocAccess::None`] also for one that does not exist.
    async fn access(&self, kernel: &Kernel, ctx: &Ctx, doc: Uuid) -> Result<DocAccess>;

    /// The content a document starts from when it is first opened (a Yjs update), if any.
    async fn initial(&self, kernel: &Kernel, tenant: Uuid, doc: Uuid) -> Result<Option<Vec<u8>>> {
        let _ = (kernel, tenant, doc);
        Ok(None)
    }

    /// The document after a stretch of editing: its whole state (a Yjs update), its plain text, and
    /// who changed it since the last save. Called once per change, from one replica.
    async fn saved(&self, kernel: &Kernel, tenant: Uuid, doc: Uuid, state: &[u8], text: &str, editors: &[Uuid]) -> Result<()> {
        let _ = (kernel, tenant, doc, state, text, editors);
        Ok(())
    }
}

#[derive(Default, Clone)]
pub struct HookRegistry {
    by_entity: HashMap<String, Vec<ScopedHook>>,
    sinks: Vec<Arc<dyn EventSink>>,
    sites: HashMap<String, Arc<dyn Site>>,
    doc_hosts: HashMap<String, Arc<dyn DocHost>>,
    /// The packages owning what is being registered (see [`HookRegistry::scoped`]).
    scope: Option<Arc<[String]>>,
}

#[derive(Clone)]
struct ScopedHook {
    /// `None`: runs for every tenant; otherwise only for tenants with one of these packages.
    packages: Option<Arc<[String]>>,
    hook: Arc<dyn DocHook>,
}

impl HookRegistry {
    pub fn register(&mut self, entity: &str, hook: Arc<dyn DocHook>) -> &mut Self {
        let packages = self.scope.clone();
        self.by_entity.entry(entity.to_string()).or_default().push(ScopedHook { packages, hook });
        self
    }

    /// Registers what `f` adds as belonging to `packages`: those document hooks run only for
    /// tenants that installed one of them, so an app's hooks on shared entities (Item, Sales
    /// Invoice) never fire — or fail — in workspaces without the app.
    pub fn scoped(&mut self, packages: &[String], f: impl FnOnce(&mut Self)) -> &mut Self {
        let outer = self.scope.replace(packages.iter().cloned().collect());
        f(self);
        self.scope = outer;
        self
    }

    /// Each document hook's entity and the packages it runs for (`None`: every tenant), in
    /// registration order (`ferrum task deps_report`).
    pub fn scopes(&self) -> Vec<(String, Option<Vec<String>>)> {
        let mut out: Vec<_> = self.by_entity.iter().flat_map(|(e, hs)| hs.iter().map(move |h| (e.clone(), h.packages.as_ref().map(|p| p.to_vec())))).collect();
        out.sort();
        out
    }

    /// Every hook for `entity`, followed by hooks registered for every entity (`"*"`, e.g.
    /// plugins), so those see the values the entity's own hooks derived.
    pub fn for_entity(&self, entity: &str) -> Vec<Arc<dyn DocHook>> {
        self.matching(entity, |_| true)
    }

    /// [`HookRegistry::for_entity`] for a tenant with the `installed` packages: hooks scoped to
    /// packages it lacks are left out.
    pub fn for_tenant(&self, entity: &str, installed: &[String]) -> Vec<Arc<dyn DocHook>> {
        self.matching(entity, |h| h.packages.as_ref().is_none_or(|ps| ps.iter().any(|p| installed.contains(p))))
    }

    fn matching(&self, entity: &str, keep: impl Fn(&ScopedHook) -> bool) -> Vec<Arc<dyn DocHook>> {
        let own = self.by_entity.get(entity).into_iter().flatten();
        let all = if entity != "*" { self.by_entity.get("*") } else { None };
        own.chain(all.into_iter().flatten()).filter(|h| keep(h)).map(|h| h.hook.clone()).collect()
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

    /// Mounts `site` under the package's name; it answers only tenants that installed the package.
    pub fn add_site(&mut self, package: &str, site: Arc<dyn Site>) -> &mut Self {
        self.sites.insert(package.to_string(), site);
        self
    }

    pub fn site(&self, package: &str) -> Option<&Arc<dyn Site>> {
        self.sites.get(package)
    }

    /// Lets the package's records be co-edited documents (see [`DocHost`]).
    pub fn add_doc_host(&mut self, package: &str, host: Arc<dyn DocHost>) -> &mut Self {
        self.doc_hosts.insert(package.to_string(), host);
        self
    }

    pub fn doc_host(&self, package: &str) -> Option<&Arc<dyn DocHost>> {
        self.doc_hosts.get(package)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;
    impl DocHook for Noop {}

    #[test]
    fn scoped_hooks_run_only_where_their_package_is_installed() {
        let mut r = HookRegistry::default();
        let platform: Arc<dyn DocHook> = Arc::new(Noop);
        let app: Arc<dyn DocHook> = Arc::new(Noop);
        let everywhere: Arc<dyn DocHook> = Arc::new(Noop);
        r.register("Item", platform.clone());
        r.scoped(&["procurement".to_string()], |h| {
            h.register("Item", app.clone());
        });
        r.register("*", everywhere.clone());
        let ids = |hooks: Vec<Arc<dyn DocHook>>| hooks.iter().map(|h| Arc::as_ptr(h) as *const () as usize).collect::<Vec<_>>();
        let id = |h: &Arc<dyn DocHook>| Arc::as_ptr(h) as *const () as usize;
        assert_eq!(ids(r.for_tenant("Item", &["core".into()])), [id(&platform), id(&everywhere)]);
        assert_eq!(ids(r.for_tenant("Item", &["core".into(), "procurement".into()])), [id(&platform), id(&app), id(&everywhere)]);
        assert_eq!(ids(r.for_entity("Item")).len(), 3, "for_entity still lists every hook");
        r.register("Item", platform.clone());
        assert_eq!(ids(r.for_tenant("Item", &[])).len(), 3, "the scope ends with the closure");
    }
}
