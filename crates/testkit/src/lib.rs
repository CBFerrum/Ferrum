//! The one way an app's integration suite starts (docs/testkit.md): a kernel booted as the server boots one, on a
//! database emptied first; a workspace with the packages named and its people signed in; and the helpers every suite
//! wrote for itself. Apps take it under `[dev-dependencies]` only, so nothing of it is in a release build.
//!
//! As the server boots (server/src/app.rs, server/src/packages.rs): the vault opened, with its credential slot, before
//! the kernel; the platform's packages and hooks (`ferrum_domain`); each app registered as itself
//! ([`HookRegistry::app`]): ERP first and for every workspace, every other app for the workspaces that installed one of
//! its packages, each owning its packages; their reports set at once with who gives each; a package needing one the
//! build lacks left out. Mail and files are kept in memory, and each app's upkeep runs once. Refused: a vault that does
//! not open.

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, Opener, Vault};
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, MailAttachment, Mailer, Result};
use ferrum_meta::{Catalog, MetaError, Package};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub use ferrum_kernel;

/// The password of everyone the kit makes, each workspace's administrator included.
pub const PASSWORD: &str = "Correct-Horse-9";

/// How long [`Workspace::drain`] waits for a workspace's events before it fails, naming what is left.
pub const DRAIN_LIMIT: Duration = Duration::from_secs(120);

/// The app every build compiles in, registered first and for every workspace (server/src/packages.rs): the apps that
/// build on its documents see its totals, postings and open items.
const FIRST: &str = "erp";

/// An app's upkeep, as [`App::maintenance`] gives it.
pub type Upkeep<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

/// An app as the server compiles it in: the functions its crate exports, by the names the server calls them. Made by
/// [`app!`].
#[derive(Clone, Copy)]
pub struct App {
    pub id: &'static str,
    pub packages: fn() -> std::result::Result<Vec<Package>, MetaError>,
    pub register: fn(&mut HookRegistry),
    pub reports: fn() -> Vec<Arc<dyn ReportSource>>,
    pub maintenance: for<'a> fn(&'a Kernel) -> Upkeep<'a>,
}

/// An [`App`] from its id and its crate, as the server lists it: `app!("hr", ferrum_app_hr)`.
#[macro_export]
macro_rules! app {
    ($id:literal, $krate:ident) => {{
        fn maintenance(k: &$crate::ferrum_kernel::Kernel) -> $crate::Upkeep<'_> {
            Box::pin($krate::maintenance(k))
        }
        $crate::App { id: $id, packages: $krate::packages, register: $krate::register, reports: $krate::reports, maintenance }
    }};
}

/// The database a suite runs on, which the kit empties: `var` from the environment, else `ferrum_<name>` on the local
/// server for `FERRUM_<NAME>_DATABASE_URL` (`FERRUM_HR_TEST_DATABASE_URL`: `ferrum_hr_test`), as the role `ferrum`. The
/// vault's credential slot is made from its password.
pub fn database(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| {
        let name = var.trim_start_matches("FERRUM_").trim_end_matches("_DATABASE_URL").to_lowercase();
        format!("postgres://ferrum:ferrum@localhost:5432/ferrum_{name}")
    })
}

static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().expect("a runtime"));

/// Runs a scenario as a task of one multi-threaded runtime per test binary, whose threads have the server's 16 MiB stacks
/// (an unoptimised build's hook chains need more than 2 MiB). A kernel booted under one test serves the next, and the
/// scenario's panic is the test's.
pub fn run(scenario: impl Future<Output = ()> + Send + 'static) {
    if let Err(e) = RUNTIME.block_on(RUNTIME.spawn(scenario)) {
        match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(e) => panic!("the scenario did not finish: {e}"),
        }
    }
}

/// A kernel booted as the server boots one, and what it keeps in memory for the suite to read.
pub struct Kit {
    pub k: Arc<Kernel>,
    /// What the kernel stored as files.
    pub files: Arc<MemFiles>,
    mail: Arc<Mailbox>,
    vaults: HashMap<&'static str, Vault>,
    url: String,
    apps: Vec<App>,
    packages: Vec<Package>,
    settings: KernelSettings,
}

impl Kit {
    /// The database at `url` emptied, and a kernel on it with the platform and `apps`.
    pub async fn boot(url: &str, apps: &[App]) -> Kit {
        Kit::boot_with(url, apps, Vec::new(), KernelSettings::default()).await
    }

    /// [`Kit::boot`], with `packages` that no app brings (a stand-in for another app's, a package as it was) and the
    /// kernel's `settings` (an address people reach, private addresses allowed).
    pub async fn boot_with(url: &str, apps: &[App], packages: Vec<Package>, settings: KernelSettings) -> Kit {
        let db = connect(url).await;
        reset(&db.pool).await;
        let (files, mail) = (Arc::new(MemFiles::default()), Arc::new(Mailbox::default()));
        let (k, vaults) = start(db, url, apps, &packages, &settings, &files, &mail).await;
        for app in ordered(apps) {
            (app.maintenance)(&k).await.unwrap_or_else(|e| panic!("{}'s upkeep: {e}", app.id));
        }
        Kit { k, files, mail, vaults, url: url.into(), apps: apps.to_vec(), packages, settings }
    }

    /// Another server on the same database, started as this one was, with nothing emptied: what a suite needs to ask
    /// whichever server, or one that started again. Its apps' handles are its own ([`Kit::vault`] gives this one's).
    pub async fn replica(&self) -> Arc<Kernel> {
        start(connect(&self.url).await, &self.url, &self.apps, &self.packages, &self.settings, &self.files, &self.mail).await.0
    }

    /// A workspace `slug` with `packages` (and what they depend on), and `settings` (`Value::Null` for none), its
    /// administrator `admin@{slug}.test` signed in.
    pub async fn workspace(&self, slug: &str, packages: &[&str], settings: Value) -> Workspace {
        let email = format!("admin@{slug}.test");
        let tenant = self
            .k
            .create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: email.clone(),
                admin_password: PASSWORD.into(),
                admin_name: None,
                packages: packages.iter().map(|p| p.to_string()).collect(),
                settings: (!settings.is_null()).then_some(settings),
            })
            .await
            .unwrap_or_else(|e| panic!("workspace {slug}: {}", describe(&e)));
        let admin = sign_in(&self.k, tenant, slug, &email).await;
        Workspace { k: self.k.clone(), tenant, slug: slug.into(), admin }
    }

    /// App `app`'s own vault handle, as its registration took it.
    pub fn vault(&self, app: &str) -> Vault {
        self.vaults.get(app).cloned().unwrap_or_else(|| panic!("no app {app} was booted"))
    }

    /// The mail sent so far, oldest first.
    pub fn mail(&self) -> Vec<Mail> {
        self.mail.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A workspace and its administrator, signed in.
#[derive(Clone)]
pub struct Workspace {
    pub k: Arc<Kernel>,
    pub tenant: Uuid,
    pub slug: String,
    pub admin: Ctx,
}

impl Workspace {
    /// A person of the workspace holding `roles`, signed in with [`PASSWORD`].
    pub async fn user(&self, email: &str, roles: &[&str]) -> Ctx {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        self.k.create_user(&self.admin, email, email, Some(PASSWORD), &roles, false).await.unwrap_or_else(|e| panic!("user {email}: {}", describe(&e)));
        sign_in(&self.k, self.tenant, &self.slug, email).await
    }

    /// Dispatches events until none of this workspace's is left undelivered. The dispatcher takes any workspace's
    /// events, as a server's replicas do, so those of a scenario running beside this one may be handling some of this
    /// one's: those are waited for, not taken for done. Fails after [`DRAIN_LIMIT`], naming what is left.
    pub async fn drain(&self) {
        self.drain_within(DRAIN_LIMIT).await
    }

    async fn drain_within(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let claimed = self.k.dispatch_outbox_once().await.unwrap_or(0);
            let mut tx = self.k.db.tenant_tx(self.tenant).await.expect("a transaction");
            let left: Option<String> = sqlx::query_scalar(
                "select string_agg(topic || coalesce(' (' || last_error || ')', ''), ', ' order by id) from outbox \
                 where tenant_id = $1 and processed_at is null and not dead",
            )
            .bind(self.tenant)
            .fetch_one(&mut *tx)
            .await
            .expect("what is left");
            tx.commit().await.expect("commit");
            let Some(left) = left else { return };
            assert!(Instant::now() < deadline, "{}'s events were not delivered within {} s: {left}", self.slug, limit.as_secs());
            if claimed == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

async fn connect(url: &str) -> Db {
    Db::connect(DbConfig { url: url.into(), ..Default::default() }).await.unwrap_or_else(|e| panic!("connect: {e}"))
}

/// Empties the database: its tables a few dozen to a statement, then the schemas. One `drop schema ... cascade` over
/// every app's tables, with their indexes, policies and guards, needs more locks than PostgreSQL has by default.
async fn reset(pool: &sqlx::PgPool) {
    let tables: Vec<String> =
        sqlx::query_scalar("select quote_ident(tablename) from pg_tables where schemaname = 'public'").fetch_all(pool).await.expect("reset: tables");
    for chunk in tables.chunks(50) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("drop table if exists {} cascade", chunk.join(", ")))).execute(pool).await.expect("reset");
    }
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(pool).await.expect("reset");
}

/// `apps` in the order the server registers them: ERP first.
fn ordered(apps: &[App]) -> Vec<App> {
    apps.iter().filter(|a| a.id == FIRST).chain(apps.iter().filter(|a| a.id != FIRST)).copied().collect()
}

/// A kernel on `db` as the server starts one: the vault opened first, the catalog, the platform's hooks, then each app.
async fn start(
    db: Db,
    url: &str,
    apps: &[App],
    extra: &[Package],
    settings: &KernelSettings,
    files: &Arc<MemFiles>,
    mail: &Arc<Mailbox>,
) -> (Arc<Kernel>, HashMap<&'static str, Vault>) {
    // Its credential slot, from the address's password; Argon2id kept cheap here.
    let opener = Opener { database_url: url.into(), file: "/nonexistent/ferrum/root.key".into(), kdf: Kdf { m_kib: 64, t: 1, p: 1 }, ..Default::default() };
    vault::open(&db, opener, false).await.unwrap_or_else(|e| panic!("the vault: {e}"));
    let mut packages = ferrum_domain::packages().expect("the platform's packages");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let (mut vaults, mut sources, mut owners) = (HashMap::new(), Vec::new(), HashMap::new());
    for app in ordered(apps) {
        let own = (app.packages)().unwrap_or_else(|e| panic!("{}'s packages: {e}", app.id));
        let names: Vec<String> = own.iter().map(|p| p.name().to_string()).collect();
        packages.extend(own);
        // Its hooks run for the workspaces with one of its packages; ERP's for every workspace.
        let scope = if app.id == FIRST { Vec::new() } else { names.clone() };
        hooks.app(app.id, &scope, |h| {
            (app.register)(h);
            vaults.insert(app.id, h.vault());
        });
        hooks.own(app.id, &names);
        for source in (app.reports)() {
            for name in source.catalog().iter().filter_map(|e| e["name"].as_str()) {
                owners.insert(name.to_string(), names.clone());
            }
            sources.push((app.id.to_string(), source));
        }
    }
    ferrum_domain::reports::set_owners(owners);
    ferrum_domain::reports::set_sources(sources).unwrap_or_else(|e| panic!("reports: {e}"));
    // A package given beside the apps never replaces one they bring.
    let given: HashSet<String> = packages.iter().map(|p| p.name().to_string()).collect();
    packages.extend(extra.iter().filter(|p| !given.contains(p.name())).cloned());
    let catalog = Catalog::from_packages(fitting(packages)).unwrap_or_else(|e| panic!("the catalog: {e}"));
    let k = Kernel::boot(db, catalog, hooks, settings.clone(), Some(mail.clone() as Arc<dyn Mailer>)).await.unwrap_or_else(|e| panic!("boot: {e}"));
    let status = vault::status(&k.db).await.expect("the vault's status");
    assert!(status.open, "the vault did not open: {}", status.closed_because.unwrap_or_default());
    k.set_file_store(files.clone());
    (k, vaults)
}

/// The packages whose dependencies are all there, as the server keeps them: a connector between two apps ships with
/// one of them, and is left out where the other is not.
fn fitting(mut packages: Vec<Package>) -> Vec<Package> {
    loop {
        let names: HashSet<String> = packages.iter().map(|p| p.name().to_string()).collect();
        let before = packages.len();
        packages.retain(|p| {
            let fits = p.manifest.depends.iter().all(|d| names.contains(d));
            if !fits {
                eprintln!("left out {}: this build lacks a package it depends on", p.name());
            }
            fits
        });
        if packages.len() == before {
            return packages;
        }
    }
}

async fn sign_in(k: &Kernel, tenant: Uuid, slug: &str, email: &str) -> Ctx {
    let s = k.login(slug, email, PASSWORD).await.unwrap_or_else(|e| panic!("{email} signs in: {}", describe(&e)));
    k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap_or_else(|e| panic!("{email}'s context: {}", describe(&e)))
}

/// Files kept in memory, where a server keeps them in object storage.
#[derive(Default)]
pub struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
        Ok(())
    }
}

/// A mail as the server's mailer was asked to send it.
#[derive(Debug, Clone)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub attachments: Vec<MailAttachment>,
}

/// A mailer that keeps what it is asked to send, files attached included.
#[derive(Default)]
struct Mailbox(Mutex<Vec<Mail>>);

#[async_trait::async_trait]
impl Mailer for Mailbox {
    async fn send(&self, to: &str, subject: &str, html: &str) -> Result<()> {
        self.send_with(to, subject, html, &[]).await
    }
    fn can_attach(&self) -> bool {
        true
    }
    async fn send_with(&self, to: &str, subject: &str, html: &str, attachments: &[MailAttachment]) -> Result<()> {
        let mail = Mail { to: to.into(), subject: subject.into(), html: html.into(), attachments: attachments.to_vec() };
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push(mail);
        Ok(())
    }
}

/// A record from its JSON object.
#[track_caller]
pub fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_else(|| panic!("not an object: {v}"))
}

/// What carries a record's id: the record, its JSON, or the id itself as JSON text.
pub trait Id: std::fmt::Debug {
    fn id(&self) -> Option<&str>;
}

impl Id for Doc {
    fn id(&self) -> Option<&str> {
        self.get("id").and_then(Value::as_str)
    }
}

impl Id for Value {
    fn id(&self) -> Option<&str> {
        self.as_str().or_else(|| self.get("id").and_then(Value::as_str))
    }
}

/// The id of a record: `uid(&doc)`, `uid(&doc["customer"])`.
#[track_caller]
pub fn uid(v: &impl Id) -> Uuid {
    v.id().and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("not a record or an id: {v:?}"))
}

/// A decimal as the kernel gives one (text, or a number), none for null.
#[track_caller]
pub fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap_or_else(|_| panic!("not a decimal: {s}")),
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap_or_else(|_| panic!("not a decimal: {n}")),
        Value::Null => Decimal::ZERO,
        other => panic!("not a decimal: {other}"),
    }
}

/// Asserts `r` was refused as `kind`, saying `says`: `invalid` (a validation error too), or the error's code with spaces
/// for underscores (`forbidden`, `not found`, `conflict`, `unauthorized`, ...).
#[track_caller]
pub fn refused<V: std::fmt::Debug>(r: Result<V>, kind: &str, says: &str) {
    let (code, said) = match &r {
        Err(e @ (Error::Invalid(_) | Error::Validation(_))) => ("invalid".to_string(), describe(e)),
        Err(e) => (e.code().replace('_', " "), describe(e)),
        Ok(v) => ("done".to_string(), format!("{v:?}")),
    };
    assert!(code == kind && said.contains(says), "expected {kind} saying {says:?}, got {code}: {said}");
}

/// An error as a person reads it: a validation error's fields with what each says.
fn describe(e: &Error) -> String {
    match e {
        Error::Validation(f) => f.iter().map(|x| format!("{}: {}", x.field, x.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_helpers_read_what_the_kernel_gives() {
        let id = Uuid::now_v7();
        let doc = obj(json!({"id": id.to_string(), "customer": {"id": id.to_string()}}));
        assert_eq!((uid(&doc), uid(&doc["id"]), uid(&doc["customer"])), (id, id, id));
        assert_eq!((dec(&json!("12.50")), dec(&json!(3)), dec(&Value::Null)), (Decimal::new(1250, 2), Decimal::from(3), Decimal::ZERO));
        refused::<()>(Err(Error::Validation(vec![ferrum_kernel::error::FieldError::new("total", "must be positive")])), "invalid", "total: must be positive");
        refused::<()>(Err(Error::NotFound("the invoice".into())), "not found", "invoice");
        assert!(std::panic::catch_unwind(|| refused::<()>(Ok(()), "forbidden", "")).is_err(), "done is not refused");
        assert_eq!(database("FERRUM_KIT_SELF_TEST_DATABASE_URL"), "postgres://ferrum:ferrum@localhost:5432/ferrum_kit_self_test");
        // Scenarios side by side share one kit, kept in a static.
        fn shared<T: Send + Sync>() {}
        shared::<Kit>();
        shared::<Workspace>();
    }

    /// As the server boots one: an app's hooks only where it is installed, the vault open and the app's handle working
    /// in it, people signed in with their roles, and a drain that waits for the workspace's own events and fails, naming
    /// them, when they cannot go.
    #[test]
    fn a_kernel_as_the_server_boots_one() {
        run(async {
            let kit = Kit::boot(&database("FERRUM_TESTKIT_TEST_DATABASE_URL"), &[app!("ops", ferrum_ops)]).await;
            let ops: Vec<String> = ferrum_ops::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
            let (without, with) = (kit.k.hooks.for_tenant("Device", &["core".into()]).len(), kit.k.hooks.for_tenant("Device", &ops).len());
            assert!(without < with, "Operations's hook on devices runs only where it is installed: {without} and {with}");

            let ws = kit.workspace("kit", &["ops"], json!({"timezone": "Asia/Kolkata"})).await;
            assert!(ws.admin.is_admin);
            let viewer = ws.user("viewer@kit.test", &["Ops Viewer"]).await;
            assert!(viewer.has_role("Ops Viewer") && !viewer.is_admin);
            refused(kit.k.insert(&viewer, "Site", obj(json!({"site_name": "North", "site_code": "N"}))).await, "forbidden", "");
            kit.vault("ops").derive(&kit.k, vault::Scope::Workspace(ws.tenant), "kit-check").await.expect("the app's own handle, in the open vault");

            let site = kit.k.insert(&ws.admin, "Site", obj(json!({"site_name": "North", "site_code": "N"}))).await.expect("a site");
            ws.drain().await;
            let other = kit.workspace("other", &["ops"], Value::Null).await;
            // A newer build's event waits for a replica of it: this build never takes it.
            let mut tx = kit.k.db.tenant_tx(other.tenant).await.unwrap();
            sqlx::query("insert into outbox (tenant_id, topic, payload, build_date) values ($1, 'kit.newer', '{}', now() + interval '400 days')")
                .bind(other.tenant)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            ws.drain().await;
            let waiting = other.clone();
            let failed = tokio::spawn(async move { waiting.drain_within(Duration::from_millis(300)).await }).await.expect_err("it cannot go");
            let said = failed.into_panic();
            let said = said.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(said.contains("other's events were not delivered") && said.contains("kit.newer"), "{said}");
            assert_eq!(kit.k.get(&ws.admin, "Site", uid(&site), false).await.unwrap()["site_name"], "North");
            assert!(kit.mail().is_empty(), "nothing was sent");
        });
    }
}
