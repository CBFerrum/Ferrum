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
    /// Collection action `name` of this entity (A11 2.4): bound to no record, with its declared `params` only. Return
    /// `Some` when handled. What it writes goes through the kernel's record pipeline, in `hc.tx`.
    async fn collection(&self, hc: &mut HookCtx<'_>, name: &str, params: &Value) -> Result<Option<Value>> {
        Ok(None)
    }
    /// After any hook handled action `name`, in the same transaction, with the document as the
    /// action left it: lets an app extend a platform action (re-costing a BOM's operations when
    /// "Update Cost" re-costs its materials) and add to its `result`.
    async fn after_action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, result: &mut Value) -> Result<()> {
        Ok(())
    }
    /// An approval on a record of this entity reached its outcome (BD-66), once, in the deciding transaction, after the
    /// outcome took effect (the transition taken, the submit or action done or failed, the record moved on rejection):
    /// `doc` is the record as it now stands. An error here does not undo the decision: what the hook wrote is rolled
    /// back, the failure is logged and audited (`approval:hook_failed`), and the decision stands.
    async fn approval_decided(&self, hc: &mut HookCtx<'_>, doc: &Doc, decided: &ApprovalDecided<'_>) -> Result<()> {
        Ok(())
    }
}

/// What [`DocHook::approval_decided`] is told.
#[derive(Debug, Clone)]
pub struct ApprovalDecided<'a> {
    /// The approval request's id (`/api/approvals/…`).
    pub request: Uuid,
    /// What was asked: a transition's action, `submit`, `cancel`, or the entity's action.
    pub action: &'a str,
    /// `approved` (and done), `rejected`, or `failed` (approved, but doing it was refused: `failure` says why).
    pub outcome: &'a str,
    pub failure: Option<&'a str>,
    /// Who asked; who took the deciding step, and for whom when standing in.
    pub requester: Option<Uuid>,
    pub decided_by: Uuid,
    pub on_behalf_of: Option<Uuid>,
    /// The decider's comment (a rejection always has one).
    pub comment: Option<&'a str>,
}

/// Consumes outbox events after webhooks and automations (e.g. plugin subscriptions), each sink on its own: an error
/// makes the event retry for this sink alone, and a sink that handled an event is not called for it again when it is
/// retried or replayed (crate::outbox). A replica that dies while handling an event can still call a sink again for
/// it, so what must happen once is asked of [`crate::outbox::first_time`] in the transaction that does it.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Its name within its app (`what` of `{app}.{what}`: the registry adds the app), under which what it handled is
    /// recorded. Its type's name unless it says otherwise; two sinks of one app may not share one.
    fn name(&self) -> &'static str {
        let path = std::any::type_name::<Self>();
        let path = path.split('<').next().unwrap_or(path);
        path.rsplit("::").next().unwrap_or(path)
    }
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

/// The header a [`SiteResponse::file`] names its file in. The server streams that file as the body
/// and sends the header on to nobody.
pub const STREAM_FILE_HEADER: &str = "x-ferrum-stream-file";

impl SiteResponse {
    /// A stored file as the body (FERRUM-OPS-006): the server streams it from storage, so a
    /// download of any size never sits whole in memory. The app has decided who may read it; the
    /// server sends it only when it is this workspace's.
    pub fn file(status: u16, content_type: &str, mut headers: Vec<(String, String)>, file: uuid::Uuid) -> Self {
        headers.push((STREAM_FILE_HEADER.into(), file.to_string()));
        Self { status, content_type: content_type.into(), headers, body: Vec::new() }
    }

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
    /// Whether it serves pages at the root of a `site:{app}` host (`/_site/…`), as a website or a
    /// storefront does. Only such apps are offered as a domain's target.
    fn serves_pages(&self) -> bool {
        false
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

    /// The entity its documents are records of, if any: a workspace's step-up rule on it, or a zone's, is asked of each
    /// change a person sends (crates/kernel/src/controls.rs). None: documents of no entity, asked nothing more.
    fn entity(&self) -> Option<&str> {
        None
    }

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
    /// Event sinks by name (`{app}.{what}`; `platform.{what}` outside any app's registration), each with the packages it
    /// is called for (as [`ScopedHook::packages`]), in registration order.
    sinks: Vec<(String, Option<Arc<[String]>>, Arc<dyn EventSink>)>,
    sites: HashMap<String, Arc<dyn Site>>,
    doc_hosts: HashMap<String, Arc<dyn DocHost>>,
    job_handlers: HashMap<&'static str, Arc<dyn crate::jobs::JobHandler>>,
    inbound_handlers: HashMap<String, Arc<dyn crate::connections::inbound::InboundHandler>>,
    connector_source: Option<Arc<dyn crate::connect::ConnectorSource>>,
    connectors: HashMap<String, Arc<dyn crate::connect::Connector>>,
    resources: Option<Arc<dyn crate::resources::Resources>>,
    restored: Vec<Arc<dyn crate::recovery::Restored>>,
    leaving: Vec<Arc<dyn crate::account::Leaving>>,
    personal_held: Vec<Arc<dyn crate::privacy::PersonalHeld>>,
    query_rows: Option<Arc<dyn crate::documents::QueryRows>>,
    health: Vec<(String, Arc<dyn crate::support::Health>)>,
    /// The packages owning what is being registered (see [`HookRegistry::scoped`]).
    scope: Option<Arc<[String]>>,
    /// Apps registered with [`HookRegistry::app`], each with what its vault handles carry (crate::vault).
    vault_apps: HashMap<String, u128>,
    /// What the platform's handle carries, once the server took it ([`HookRegistry::platform_vault`]).
    platform_mint: Option<u128>,
    /// Which app owns each package compiled in ([`HookRegistry::own`]): whose the package's secret fields are.
    package_owners: HashMap<String, String>,
    /// The programs apps run beside Ferrum that sign people in through it, by app and name
    /// ([`HookRegistry::openid_companion`]).
    openid_companions: HashMap<(String, String), Arc<dyn crate::oidc::Companion>>,
    /// The app whose registration is running.
    app: Option<String>,
    /// Kinds of file apps keep for their own work ([`HookRegistry::file_kind`]): the app that declared each.
    file_kinds: HashMap<String, String>,
    /// The programs beside the server whose files their owners render ([`HookRegistry::add_companion`]), by name:
    /// their owner (an app's id, or `platform`) and what renders them.
    companions: std::collections::BTreeMap<String, (String, Arc<dyn crate::companion::Companion>)>,
    /// What registering apps went wrong, said by [`HookRegistry::settle`], which stops the server.
    problems: Vec<String>,
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
    /// The app whose registration added it ([`HookRegistry::app`]); none: the platform's.
    app: Option<String>,
    hook: Arc<dyn DocHook>,
}

/// A document hook an app runs on records another package defines, or on every record (`*`)
/// ([`HookRegistry::foreign_hooks`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct ForeignHook {
    pub app: String,
    pub entity: String,
    /// Its name (`{app}.{what}`), empty when it gives none.
    pub hook: &'static str,
}

impl HookRegistry {
    pub fn register(&mut self, entity: &str, hook: Arc<dyn DocHook>) -> &mut Self {
        let (packages, app) = (self.scope.clone(), self.app.clone());
        self.by_entity.entry(entity.to_string()).or_default().push(ScopedHook { packages, app, hook });
        self
    }

    /// The document hooks apps registered on an entity of a package their app does not own (the platform's, another
    /// app's, or every entity), among those that run in a workspace with the `installed` packages; `package_of` says
    /// which package defines an entity there. Sorted by app, entity and name; the platform's own hooks are no app's.
    pub fn foreign_hooks(&self, installed: &[String], package_of: impl Fn(&str) -> Option<String>) -> Vec<ForeignHook> {
        let mut out: Vec<ForeignHook> = self
            .by_entity
            .iter()
            .flat_map(|(entity, hooks)| hooks.iter().map(move |h| (entity, h)))
            .filter(|(_, h)| h.packages.as_ref().is_none_or(|ps| ps.iter().any(|p| installed.contains(p))))
            .filter_map(|(entity, h)| {
                let app = h.app.as_deref()?;
                let own = package_of(entity).is_some_and(|p| self.package_owner(&p) == Some(app));
                (!own).then(|| ForeignHook { app: app.to_string(), entity: entity.clone(), hook: h.hook.name() })
            })
            .collect();
        out.sort();
        out
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

    /// Registers what `f` adds as app `id`'s: for the workspaces that installed one of `packages`, as
    /// [`HookRegistry::scoped`] does (none: for every workspace), and with `id` known to the vault, so that `f` may take
    /// the app's own handle ([`HookRegistry::vault`]). Once per app, and never inside another app's registration:
    /// either stops the server ([`HookRegistry::settle`]).
    pub fn app(&mut self, id: &str, packages: &[String], f: impl FnOnce(&mut Self)) -> &mut Self {
        let problem = if let Some(outer) = &self.app {
            Some(format!("app {id} was registered inside app {outer}'s registration"))
        } else if !crate::vault::app_id_ok(id) {
            Some(format!("{id:?} is not an app's id (lowercase letters, digits and dashes, and not platform)"))
        } else if self.vault_apps.contains_key(id) {
            Some(format!("app {id} was registered twice"))
        } else {
            None
        };
        if let Some(p) = problem {
            self.problems.push(p);
            return self;
        }
        self.vault_apps.insert(id.to_string(), crate::vault::mint());
        self.app = Some(id.to_string());
        if packages.is_empty() {
            f(self);
        } else {
            self.scoped(packages, f);
        }
        self.app = None;
        self
    }

    /// The vault handle of the app whose registration is running ([`HookRegistry::app`]): its own keys and secrets,
    /// and nobody else's. Keep it in what you register: it works with the kernel this registry boots, and no other.
    /// Asked for outside an app's registration, it is a handle nothing accepts, and the server does not start.
    pub fn vault(&mut self) -> crate::vault::Vault {
        let found = self.app.as_ref().and_then(|a| self.vault_apps.get(a).map(|m| (a.clone(), *m)));
        match found {
            Some((app, mint)) => crate::vault::Vault::app(&app, mint),
            None => {
                self.problems.push("a vault handle was asked for outside an app's registration".into());
                crate::vault::Vault::none()
            }
        }
    }

    /// The platform's own vault handle, for the server's code (what signs its sessions, the tokens it compares, the
    /// passwords it hands its mail and storage clients), as an app is given its own: taken once, by the server, before
    /// any app registers. Asked again, inside an app's registration or after one, it is a handle nothing accepts, and the
    /// server does not start.
    pub fn platform_vault(&mut self) -> crate::vault::Vault {
        if self.platform_mint.is_some() || self.app.is_some() || !self.vault_apps.is_empty() {
            self.problems.push("the platform's vault handle was asked for again, or after an app registered".into());
            return crate::vault::Vault::none();
        }
        let mint = crate::vault::mint();
        self.platform_mint = Some(mint);
        crate::vault::Vault::app("platform", mint)
    }

    /// Registers `name` (lowercase letters, digits and dashes), a program the app whose registration is running runs
    /// beside Ferrum and that signs people in through it (crate::oidc): the right that lets a person in, and the claims
    /// that place them there. The app then declares its client for each workspace with its own vault handle
    /// (`Vault::companion`). Outside an app's registration, or twice, the server does not start. (A program whose files
    /// the server renders is declared with [`HookRegistry::add_companion`]; one program can be both.)
    pub fn openid_companion(&mut self, name: &str, companion: Arc<dyn crate::oidc::Companion>) -> &mut Self {
        let Some(app) = self.app.clone() else {
            self.problems.push(format!("companion {name} was registered outside an app's registration"));
            return self;
        };
        if !crate::oidc::name_ok(name) {
            self.problems.push(format!("app {app}'s companion {name:?}: lowercase letters, digits and dashes, up to 40"));
        } else if self.openid_companions.insert((app.clone(), name.to_string()), companion).is_some() {
            self.problems.push(format!("app {app} registered its companion {name} twice"));
        }
        self
    }

    /// App `app`'s companion `name` that signs people in, when it registered one.
    pub(crate) fn openid_companion_of(&self, app: &str, name: &str) -> Option<&Arc<dyn crate::oidc::Companion>> {
        self.openid_companions.get(&(app.to_string(), name.to_string()))
    }

    /// Declares a kind of file the app registering keeps for its own work (`<app id>.<name>`: `name` lowercase letters,
    /// digits and `_`), and gives its handle (crate::files::FileKind): what the app keeps, reads and lets go of such files
    /// with, as nobody else can. Once per kind, inside the app's registration: otherwise the server does not start
    /// ([`HookRegistry::settle`]).
    pub fn file_kind(&mut self, name: &str) -> crate::files::FileKind {
        let Some((app, mint)) = self.app.as_ref().and_then(|a| self.vault_apps.get(a).map(|m| (a.clone(), *m))) else {
            self.problems.push(format!("a kind of file ({name}) was declared outside an app's registration"));
            return crate::files::FileKind::new(name.to_string(), "", 0);
        };
        let kind = format!("{app}.{name}");
        let named = name.len() <= 40
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !named {
            self.problems.push(format!("{kind:?} is not a kind of file's name (lowercase letters, digits and _ after the app's id)"));
        } else if self.file_kinds.insert(kind.clone(), app.clone()).is_some() {
            self.problems.push(format!("the kind of file {kind} is declared twice"));
        }
        crate::files::FileKind::new(kind, &app, if named { mint } else { 0 })
    }

    /// The app that declared the kind of file `kind` ([`HookRegistry::file_kind`]).
    pub(crate) fn file_kind_app(&self, kind: &str) -> Option<&str> {
        self.file_kinds.get(kind).map(String::as_str)
    }

    /// What app `id`'s vault handles carry, when this registry registered it (the platform's, once the server took it).
    pub(crate) fn vault_mint(&self, id: &str) -> Option<u128> {
        if id == "platform" { self.platform_mint } else { self.vault_apps.get(id).copied() }
    }

    /// Says that app `id` owns `packages`: a secret field one of them declares is that app's, read only with its own
    /// handle (crates/kernel/src/vault, "Who owns what"). The server's to say, for every app it compiles in, outside any
    /// app's registration; a package two apps claim, or a claim made inside an app's registration, stops the server
    /// ([`HookRegistry::settle`]). A package no app claims is the platform's, or that of the app installed with it.
    pub fn own(&mut self, id: &str, packages: &[String]) -> &mut Self {
        if let Some(inside) = &self.app {
            self.problems.push(format!("app {inside} claimed packages for {id} inside its registration"));
            return self;
        }
        if !crate::vault::app_id_ok(id) {
            self.problems.push(format!("{id:?} is not an app's id, and owns no package"));
            return self;
        }
        for p in packages {
            match self.package_owners.get(p) {
                Some(other) if other != id => self.problems.push(format!("package {p} is claimed by both {other} and {id}")),
                _ => {
                    self.package_owners.insert(p.clone(), id.to_string());
                }
            }
        }
        self
    }

    /// The app that owns `package`, when one claimed it ([`HookRegistry::own`]).
    pub fn package_owner(&self, package: &str) -> Option<&str> {
        self.package_owners.get(package).map(String::as_str)
    }

    /// Declares the service `name`, a program that runs beside the server, whose files `companion` renders
    /// (crate::companion; `ferrum task render`): app `id`'s inside its registration ([`HookRegistry::app`]), given its
    /// own vault handle as it renders; the platform's outside any. A name is lowercase letters, digits and dashes, and
    /// unique across the installation: one declared twice, or a declaration that cannot work, stops the server
    /// ([`HookRegistry::settle`]), naming both owners.
    pub fn add_companion(&mut self, name: &str, companion: Arc<dyn crate::companion::Companion>) -> &mut Self {
        let owner = self.app.clone().unwrap_or_else(|| "platform".to_string());
        let problems = crate::companion::declaration_problems(name, companion.as_ref());
        if !problems.is_empty() {
            self.problems.extend(problems);
        } else if let Some((other, _)) = self.companions.get(name) {
            self.problems.push(format!("the service {name} is declared twice: by {other} and by {owner}"));
        } else {
            self.companions.insert(name.to_string(), (owner, companion));
        }
        self
    }

    /// The service `name`: its owner and what renders it.
    pub(crate) fn companion(&self, name: &str) -> Option<(&str, &dyn crate::companion::Companion)> {
        self.companions.get(name).map(|(owner, c)| (owner.as_str(), c.as_ref()))
    }

    /// Every service, by name, with its owner.
    pub(crate) fn companions(&self) -> impl Iterator<Item = (&str, &str, &dyn crate::companion::Companion)> {
        self.companions.iter().map(|(name, (owner, c))| (name.as_str(), owner.as_str(), c.as_ref()))
    }

    /// Each document hook's entity and the packages it runs for (`None`: every tenant), in
    /// registration order (`ferrum task deps_report`).
    pub fn scopes(&self) -> Vec<(String, Option<Vec<String>>)> {
        let mut out: Vec<_> = self.by_entity.iter().flat_map(|(e, hs)| hs.iter().map(move |h| (e.clone(), h.packages.as_ref().map(|p| p.to_vec())))).collect();
        out.sort();
        out
    }

    /// The hooks for `entity` in a tenant with the `installed` packages (those scoped to packages it lacks are left
    /// out), followed by hooks registered for every entity (`"*"`, e.g. plugins), so those see the values the entity's
    /// own hooks derived.
    pub fn for_tenant(&self, entity: &str, installed: &[String]) -> Vec<Arc<dyn DocHook>> {
        let own = self.by_entity.get(entity).into_iter().flatten();
        let all = if entity != "*" { self.by_entity.get("*") } else { None };
        own.chain(all.into_iter().flatten())
            .filter(|h| h.packages.as_ref().is_none_or(|ps| ps.iter().any(|p| installed.contains(p))))
            .map(|h| h.hook.clone())
            .collect()
    }

    /// Puts each entity's hooks in the order they declare (otherwise, registration order), or says
    /// everything that cannot work: a hook waiting for one that does not exist, a cycle, two hooks
    /// filling one field with neither declared to go first.
    pub fn settle(&mut self) -> std::result::Result<(), Vec<String>> {
        let mut problems = self.problems.clone();
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

    /// Adds an event sink, named `{app}.{name}` after the app whose registration is running ([`EventSink::name`]), and
    /// called only for the workspaces that installed one of that app's packages, as its document hooks are
    /// ([`HookRegistry::scoped`]). A name two sinks share stops the server ([`HookRegistry::settle`]): what one handled
    /// would pass for the other's.
    pub fn add_event_sink(&mut self, sink: Arc<dyn EventSink>) -> &mut Self {
        let name = format!("{}.{}", self.app.as_deref().unwrap_or("platform"), sink.name());
        if self.sinks.iter().any(|(n, _, _)| *n == name) {
            self.problems.push(format!("two event sinks are named {name}: give one another name (EventSink::name)"));
        }
        self.sinks.push((name, self.scope.clone(), sink));
        self
    }

    /// The event sinks of a workspace with the `installed` packages, by name, in registration order: an app's only where
    /// one of its packages is (as [`HookRegistry::for_tenant`] gives document hooks).
    pub fn event_sinks(&self, installed: &[String]) -> impl Iterator<Item = (&str, &Arc<dyn EventSink>)> {
        self.sinks
            .iter()
            .filter(move |(_, packages, _)| packages.as_ref().is_none_or(|ps| ps.iter().any(|p| installed.contains(p))))
            .map(|(name, _, sink)| (name.as_str(), sink))
    }

    /// Adds a kind of background work (crate::jobs). Kinds are `{app}.{what}`.
    pub fn add_job_handler(&mut self, handler: Arc<dyn crate::jobs::JobHandler>) -> &mut Self {
        self.job_handlers.insert(handler.kind(), handler);
        self
    }

    pub fn job_handler(&self, kind: &str) -> Option<&Arc<dyn crate::jobs::JobHandler>> {
        self.job_handlers.get(kind)
    }

    /// Registers `app` (its package's name) for calls that connections listing it as their inbound `handler` accept
    /// (crate::connections::inbound).
    pub fn add_inbound_handler(&mut self, app: &str, handler: Arc<dyn crate::connections::inbound::InboundHandler>) -> &mut Self {
        self.inbound_handlers.insert(app.to_string(), handler);
        self
    }

    pub fn inbound_handler(&self, app: &str) -> Option<&Arc<dyn crate::connections::inbound::InboundHandler>> {
        self.inbound_handlers.get(app)
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

    /// An app that keeps people's data in tables of its own (see [`crate::privacy::PersonalHeld`]).
    pub fn add_personal_held(&mut self, app: Arc<dyn crate::privacy::PersonalHeld>) {
        self.personal_held.push(app);
    }

    pub fn personal_held(&self) -> &[Arc<dyn crate::privacy::PersonalHeld>] {
        &self.personal_held
    }

    /// Adds how `package`'s part of a workspace is doing to the support report (see
    /// [`crate::support::Health`]); asked only of workspaces that installed the package.
    pub fn add_health(&mut self, package: &str, source: Arc<dyn crate::support::Health>) -> &mut Self {
        self.health.push((package.to_string(), source));
        self
    }

    pub fn health(&self) -> &[(String, Arc<dyn crate::support::Health>)] {
        &self.health
    }

    /// Where documents about a query get their rows (the analytics engine). There is at most one.
    pub fn set_query_rows(&mut self, source: Arc<dyn crate::documents::QueryRows>) -> &mut Self {
        self.query_rows = Some(source);
        self
    }

    pub fn query_rows(&self) -> Option<&Arc<dyn crate::documents::QueryRows>> {
        self.query_rows.as_ref()
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
        r.for_tenant("Invoice", &[]).iter().map(|h| h.name()).collect()
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
    fn the_server_says_which_app_owns_each_package_once() {
        let mut r = HookRegistry::default();
        r.own("crm", &["selling".to_string(), "crm".to_string()]).own("crm", &["crm".to_string()]);
        assert_eq!((r.package_owner("selling"), r.package_owner("hr")), (Some("crm"), None));
        assert!(r.settle().is_ok(), "claiming its own again is no problem");
        r.own("hr", &["crm".to_string()]);
        assert!(r.settle().unwrap_err().join("; ").contains("claimed by both crm and hr"));
        let mut r = HookRegistry::default();
        r.app("hr", &[], |h| {
            h.own("hr", &["selling".to_string()]);
        });
        r.own("platform", &["core".to_string()]);
        let problems = r.settle().unwrap_err().join("; ");
        assert!(problems.contains("inside its registration") && problems.contains("not an app's id"), "{problems}");
        assert_eq!(r.package_owner("selling"), None);
    }

    #[test]
    fn an_app_gets_its_own_vault_handle_and_nothing_else_does() {
        let mut r = HookRegistry::default();
        let mut crm = None;
        r.app("crm", &["selling".to_string()], |h| {
            h.register("Lead", Arc::new(Noop));
            crm = Some(h.vault());
            // Inside its registration an app cannot register as another, nor take another's handle.
            h.app("hr", &[], |h| {
                h.vault();
            });
        });
        assert_eq!(crm.as_ref().map(|v| v.owner()), Some("crm"));
        assert_eq!(r.scopes(), [("Lead".to_string(), Some(vec!["selling".to_string()]))], "scoped to its packages, as scoped() does");
        assert!(r.vault_mint("crm").is_some() && r.vault_mint("hr").is_none());
        r.app("crm", &[], |_| {});
        r.app("platform", &[], |_| {});
        r.app("plugin:x", &[], |_| {});
        r.vault();
        let problems = r.settle().unwrap_err().join("; ");
        for said in [
            "app hr was registered inside app crm's registration",
            "app crm was registered twice",
            "\"platform\" is not an app's id",
            "\"plugin:x\" is not an app's id",
            "outside an app's registration",
        ] {
            assert!(problems.contains(said), "{said}: {problems}");
        }
        let mut plain = HookRegistry::default();
        plain.app("erp", &[], |h| {
            h.register("Invoice", Arc::new(Noop));
        });
        assert_eq!(plain.scopes(), [("Invoice".to_string(), None)], "no packages: every workspace, as before");
        assert!(plain.settle().is_ok());
    }

    #[test]
    fn the_hooks_an_app_runs_on_others_records_are_listed() {
        let mut r = HookRegistry::default();
        r.register("Item", Arc::new(Named("platform.stock", &[], &[])));
        r.app("crm", &["crm".to_string()], |h| {
            h.register("Lead", Arc::new(Named("crm.lead", &[], &[])));
            h.register("Customer", Arc::new(Named("crm.customer", &[], &[])));
            h.register("*", Arc::new(Named("crm.every", &[], &[])));
        });
        r.app("erp", &[], |h| {
            h.register("Invoice", Arc::new(Named("erp.invoice", &[], &[])));
            h.register("Lead", Arc::new(Named("erp.lead", &[], &[])));
        });
        r.own("crm", &["crm".to_string()]).own("erp", &["selling".to_string()]);
        let package_of = |e: &str| match e {
            "Lead" => Some("crm".to_string()),
            "Customer" => Some("parties".to_string()),
            "Invoice" => Some("selling".to_string()),
            "Item" => Some("products".to_string()),
            _ => None,
        };
        let listed = |installed: &[&str]| {
            let installed: Vec<String> = installed.iter().map(|p| p.to_string()).collect();
            r.foreign_hooks(&installed, package_of).into_iter().map(|f| (f.app, f.entity, f.hook)).collect::<Vec<_>>()
        };
        let row = |app: &str, entity: &str, hook: &'static str| (app.to_string(), entity.to_string(), hook);
        // Each by the app that registered it: on a record another package defines, or on every one; never its own, nor
        // the platform's hooks.
        assert_eq!(listed(&["crm", "selling"]), [row("crm", "*", "crm.every"), row("crm", "Customer", "crm.customer"), row("erp", "Lead", "erp.lead")]);
        assert_eq!(listed(&["selling"]), [row("erp", "Lead", "erp.lead")], "CRM's run only where one of its packages is");
    }

    #[test]
    fn a_kind_of_file_is_declared_once_by_its_app() {
        let mut r = HookRegistry::default();
        let mut kinds = Vec::new();
        r.app("cloud", &[], |h| {
            kinds.push(h.file_kind("backup"));
            kinds.push(h.file_kind("artifact"));
        });
        assert_eq!(kinds.iter().map(|k| k.kind()).collect::<Vec<_>>(), ["cloud.backup", "cloud.artifact"]);
        assert_eq!((r.file_kind_app("cloud.backup"), r.file_kind_app("hr.backup")), (Some("cloud"), None));
        assert!(r.settle().is_ok());
        r.app("hr", &[], |h| {
            h.file_kind("payslip");
            h.file_kind("payslip");
            h.file_kind("Pay Slip");
        });
        r.file_kind("loose");
        let problems = r.settle().unwrap_err().join("; ");
        for said in
            ["the kind of file hr.payslip is declared twice", "\"hr.Pay Slip\" is not a kind of file's name", "a kind of file (loose) was declared outside"]
        {
            assert!(problems.contains(said), "{said}: {problems}");
        }
    }

    struct Program(&'static [&'static str], &'static [&'static str]);

    #[async_trait]
    impl crate::companion::Companion for Program {
        fn files(&self) -> &'static [&'static str] {
            self.0
        }
        fn made(&self) -> &'static [&'static str] {
            self.1
        }
        async fn render(&self, _: &crate::companion::Rendering<'_>) -> Result<Vec<crate::companion::File>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn a_service_is_its_declarers_and_one_name_with_two_owners_stops_the_server() {
        let mut r = HookRegistry::default();
        r.add_companion("object-store", Arc::new(Program(&["s3.json"], &["aws_secret_access_key"])));
        r.app("comms", &[], |h| {
            h.add_companion("meet", Arc::new(Program(&["livekit.yaml"], &["meet.secret"])));
        });
        assert_eq!(r.companion("object-store").map(|c| c.0), Some("platform"), "outside any app's registration: the platform's");
        assert_eq!(r.companion("meet").map(|c| c.0), Some("comms"), "inside one: the app's");
        assert_eq!(r.companions().map(|c| c.0).collect::<Vec<_>>(), ["meet", "object-store"]);
        assert!(r.settle().is_ok());
        r.app("live", &[], |h| {
            h.add_companion("meet", Arc::new(Program(&["relay.toml"], &[])));
        });
        let problems = r.settle().unwrap_err().join("; ");
        assert!(problems.contains("the service meet is declared twice: by comms and by live"), "{problems}");
        assert_eq!(r.companion("meet").map(|c| c.0), Some("comms"), "the first stays; the server does not start anyway");
        let mut bad = HookRegistry::default();
        bad.add_companion("Meet", Arc::new(Program(&["a"], &[])));
        bad.add_companion("none", Arc::new(Program(&[], &[])));
        bad.add_companion("dots", Arc::new(Program(&[".hidden", "../up", "a", "a"], &["Not A Name", "field.0123"])));
        let problems = bad.settle().unwrap_err().join("; ");
        for said in [
            "\"Meet\" is not a service's name",
            "the service none declares no file",
            "the file \".hidden\"",
            "the file \"../up\"",
            "the file a twice",
            "the secret \"Not A Name\"",
            "the secret \"field.0123\"",
        ] {
            assert!(problems.contains(said), "{said}: {problems}");
        }
        assert!(bad.companions().next().is_none(), "none of them is declared");
    }

    struct Quiet;

    #[async_trait]
    impl EventSink for Quiet {
        async fn on_event(&self, _: &Kernel, _: &crate::outbox::Event) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_event_sink_is_named_after_its_app_and_type_and_one_name_is_one_sinks() {
        let mut r = HookRegistry::default();
        r.add_event_sink(Arc::new(Quiet));
        r.app("crm", &[], |h| {
            h.add_event_sink(Arc::new(Quiet));
        });
        assert_eq!(r.event_sinks(&[]).map(|(name, _)| name).collect::<Vec<_>>(), ["platform.Quiet", "crm.Quiet"]);
        assert!(r.settle().is_ok());
        r.app("hr", &[], |h| {
            h.add_event_sink(Arc::new(Quiet));
            h.add_event_sink(Arc::new(Quiet));
        });
        let problems = r.settle().unwrap_err().join("; ");
        assert!(problems.contains("two event sinks are named hr.Quiet"), "what one handled would pass for the other's: {problems}");
    }

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
        r.register("Item", platform.clone());
        assert_eq!(ids(r.for_tenant("Item", &[])).len(), 3, "the scope ends with the closure");
    }
}
