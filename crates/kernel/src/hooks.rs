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
    /// This hook's name, for others to order themselves by: `{app}.{what}`.
    fn name(&self) -> &'static str {
        ""
    }
    /// Hooks on the same entity that must run first, by name. A name nobody registered stops the
    /// server from starting, and a workspace from installing an app without what it waits for.
    fn after(&self) -> &'static [&'static str] {
        &[]
    }
    /// Fields `before_validate` fills. Two hooks on an entity filling the same field must be
    /// ordered, or the last one registered wins by accident: that too stops the server.
    fn fills(&self) -> &'static [&'static str] {
        &[]
    }
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
    job_handlers: HashMap<&'static str, Arc<dyn crate::jobs::JobHandler>>,
    connector_source: Option<Arc<dyn crate::connect::ConnectorSource>>,
    connectors: HashMap<String, Arc<dyn crate::connect::Connector>>,
    resources: Option<Arc<dyn crate::resources::Resources>>,
    restored: Vec<Arc<dyn crate::recovery::Restored>>,
    leaving: Vec<Arc<dyn crate::account::Leaving>>,
    /// The packages owning what is being registered (see [`HookRegistry::scoped`]).
    scope: Option<Arc<[String]>>,
}

/// The order an entity's hooks run in, as indices of `hooks`: each after what it waits for, and
/// otherwise as registered.
fn settle_entity(entity: &str, hooks: &[ScopedHook]) -> std::result::Result<Vec<usize>, Vec<String>> {
    let label = |i: usize| match hooks[i].hook.name() {
        "" => format!("hook {} (unnamed)", i + 1),
        name => name.to_string(),
    };
    let mut problems = Vec::new();
    // waits[i]: the hooks i runs after.
    let mut waits: Vec<Vec<usize>> = vec![Vec::new(); hooks.len()];
    for (i, h) in hooks.iter().enumerate() {
        for wanted in h.hook.after() {
            match hooks.iter().position(|o| o.hook.name() == *wanted) {
                Some(j) if j != i => waits[i].push(j),
                Some(_) => problems.push(format!("{entity}: {} waits for itself", label(i))),
                None => problems.push(format!("{entity}: {} waits for {wanted}, which is not registered on it", label(i))),
            }
        }
    }
    let mut order = Vec::with_capacity(hooks.len());
    let mut placed = vec![false; hooks.len()];
    while order.len() < hooks.len() {
        let Some(next) = (0..hooks.len()).find(|&i| !placed[i] && waits[i].iter().all(|&w| placed[w])) else {
            let stuck: Vec<String> = (0..hooks.len()).filter(|&i| !placed[i]).map(label).collect();
            problems.push(format!("{entity}: these hooks wait for each other: {}", stuck.join(", ")));
            return Err(problems);
        };
        placed[next] = true;
        order.push(next);
    }
    // Whether a runs (directly or not) after b.
    fn reaches(waits: &[Vec<usize>], a: usize, b: usize) -> bool {
        waits[a].iter().any(|&w| w == b || reaches(waits, w, b))
    }
    for i in 0..hooks.len() {
        for j in i + 1..hooks.len() {
            let shared: Vec<&str> = hooks[i].hook.fills().iter().filter(|f| hooks[j].hook.fills().contains(f)).copied().collect();
            if !shared.is_empty() && !reaches(&waits, i, j) && !reaches(&waits, j, i) {
                problems.push(format!("{entity}: {} and {} both fill {}, and neither says it goes first", label(i), label(j), shared.join(", ")));
            }
        }
    }
    if problems.is_empty() { Ok(order) } else { Err(problems) }
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

    /// Puts each entity's hooks in the order they declare (otherwise, registration order), or says
    /// everything that cannot work: a hook waiting for one that does not exist, a cycle, two hooks
    /// filling one field with neither declared to go first.
    pub fn settle(&mut self) -> std::result::Result<(), Vec<String>> {
        let mut problems = Vec::new();
        let names: HashMap<String, Vec<&'static str>> = self.by_entity.iter().map(|(e, hs)| (e.clone(), hs.iter().map(|h| h.hook.name()).collect())).collect();
        let mut entities: Vec<&String> = names.keys().collect();
        entities.sort();
        for entity in entities {
            let hooks = self.by_entity.get_mut(entity).expect("listed above");
            match settle_entity(entity, hooks) {
                Ok(order) => {
                    let mut taken: Vec<Option<ScopedHook>> = hooks.drain(..).map(Some).collect();
                    hooks.extend(order.into_iter().filter_map(|i| taken[i].take()));
                }
                Err(found) => problems.extend(found),
            }
        }
        if problems.is_empty() { Ok(()) } else { Err(problems) }
    }

    /// What a workspace with the `installed` packages would be missing: hooks that would run without
    /// one they wait for, which its package does not bring.
    pub fn unmet(&self, installed: &[String]) -> Vec<String> {
        let runs = |h: &ScopedHook| h.packages.as_ref().is_none_or(|ps| ps.iter().any(|p| installed.contains(p)));
        let mut out = Vec::new();
        for (entity, hooks) in &self.by_entity {
            let here: Vec<&'static str> = hooks.iter().filter(|h| runs(h)).map(|h| h.hook.name()).collect();
            for hook in hooks.iter().filter(|h| runs(h)) {
                for wanted in hook.hook.after().iter().filter(|w| !here.contains(w)) {
                    out.push(format!("{entity}: {} needs {wanted}, which these packages do not bring", hook.hook.name()));
                }
            }
        }
        out.sort();
        out
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

    /// Adds a kind of background work (crate::jobs). Kinds are `{app}.{what}`.
    pub fn add_job_handler(&mut self, handler: Arc<dyn crate::jobs::JobHandler>) -> &mut Self {
        self.job_handlers.insert(handler.kind(), handler);
        self
    }

    pub fn job_handler(&self, kind: &str) -> Option<&Arc<dyn crate::jobs::JobHandler>> {
        self.job_handlers.get(kind)
    }

    /// Adds a provider a workspace can connect to (crate::connect).
    pub fn add_connector(&mut self, connector: Arc<dyn crate::connect::Connector>) -> &mut Self {
        self.connectors.insert(connector.info().provider, connector);
        self
    }

    /// Where connectors a workspace installs come from, beside those compiled in.
    pub fn set_connector_source(&mut self, source: Arc<dyn crate::connect::ConnectorSource>) -> &mut Self {
        self.connector_source = Some(source);
        self
    }

    pub fn connector_source(&self) -> Option<&Arc<dyn crate::connect::ConnectorSource>> {
        self.connector_source.as_ref()
    }

    pub fn connector(&self, provider: &str) -> Option<&Arc<dyn crate::connect::Connector>> {
        self.connectors.get(provider)
    }

    pub fn connectors(&self) -> impl Iterator<Item = &Arc<dyn crate::connect::Connector>> {
        self.connectors.values()
    }

    pub fn entities(&self) -> impl Iterator<Item = &String> {
        self.by_entity.keys()
    }

    /// Mounts `site` under the package's name; it answers only tenants that installed the package.
    /// What provides declared resources on this server. There is at most one.
    /// What an app does about work it had in hand when a restore is noticed.
    pub fn add_restored(&mut self, app: Arc<dyn crate::recovery::Restored>) {
        self.restored.push(app);
    }

    pub fn restored(&self) -> &[Arc<dyn crate::recovery::Restored>] {
        &self.restored
    }

    pub fn add_leaving(&mut self, app: Arc<dyn crate::account::Leaving>) {
        self.leaving.push(app);
    }

    pub fn leaving(&self) -> &[Arc<dyn crate::account::Leaving>] {
        &self.leaving
    }

    pub fn set_resources(&mut self, resources: Arc<dyn crate::resources::Resources>) -> &mut Self {
        self.resources = Some(resources);
        self
    }

    pub fn resources(&self) -> Option<&Arc<dyn crate::resources::Resources>> {
        self.resources.as_ref()
    }

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

    struct Named(&'static str, &'static [&'static str], &'static [&'static str]);

    #[async_trait]
    impl DocHook for Named {
        fn name(&self) -> &'static str {
            self.0
        }
        fn after(&self) -> &'static [&'static str] {
            self.1
        }
        fn fills(&self) -> &'static [&'static str] {
            self.2
        }
    }

    fn registry(hooks: Vec<Named>) -> HookRegistry {
        let mut r = HookRegistry::default();
        for h in hooks {
            r.register("Invoice", Arc::new(h));
        }
        r
    }

    fn names(r: &HookRegistry) -> Vec<&'static str> {
        r.for_entity("Invoice").iter().map(|h| h.name()).collect()
    }

    #[test]
    fn hooks_run_in_the_order_they_declare_and_otherwise_as_registered() {
        let mut r = registry(vec![Named("tax", &["pricing"], &[]), Named("audit", &[], &[]), Named("pricing", &[], &[])]);
        r.settle().unwrap();
        assert_eq!(names(&r), ["audit", "pricing", "tax"]);
        let mut r = registry(vec![Named("a", &[], &[]), Named("b", &[], &[])]);
        r.settle().unwrap();
        assert_eq!(names(&r), ["a", "b"], "nothing declared: nothing moves");
    }

    #[test]
    fn what_cannot_work_stops_the_server() {
        let problem = |hooks| registry(hooks).settle().unwrap_err().join("; ");
        assert!(problem(vec![Named("a", &["gone"], &[])]).contains("gone"), "unresolved");
        assert!(problem(vec![Named("a", &["a"], &[])]).contains("itself"));
        assert!(problem(vec![Named("a", &["b"], &[]), Named("b", &["a"], &[])]).contains("wait for each other"), "a cycle");
        assert!(
            problem(vec![Named("a", &["c"], &[]), Named("b", &["a"], &[]), Named("c", &["b"], &[]), Named("d", &[], &[])]).contains("a, b, c"),
            "only those in it"
        );
        assert!(problem(vec![Named("a", &[], &["total"]), Named("b", &[], &["total", "tax"])]).contains("both fill total"), "a conflict");
        let mut ordered = registry(vec![Named("a", &[], &["total"]), Named("b", &["mid"], &["total"]), Named("mid", &["a"], &[])]);
        assert!(ordered.settle().is_ok(), "ordered, even through another hook, it is deliberate");
    }

    #[test]
    fn a_workspace_needs_what_its_hooks_wait_for() {
        let mut r = HookRegistry::default();
        r.scoped(&["pos".to_string()], |r| {
            r.register("Invoice", Arc::new(Named("pos.till", &["erp.pricing"], &[])));
        });
        r.scoped(&["erp".to_string()], |r| {
            r.register("Invoice", Arc::new(Named("erp.pricing", &[], &[])));
        });
        assert!(r.settle().is_ok(), "the binary has both");
        assert!(r.unmet(&["pos".to_string(), "erp".to_string()]).is_empty());
        assert!(r.unmet(&["erp".to_string()]).is_empty(), "its hook does not run there");
        assert!(r.unmet(&["pos".to_string()])[0].contains("erp.pricing"));
    }

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
