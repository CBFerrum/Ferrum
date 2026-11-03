//! What declaring resources does to installing an app: a real App Manager on a real PostgreSQL
//! (`FERRUM_APP_RESOURCES_TEST_DATABASE_URL`), with what provides resources stood in for. The real
//! provider is exercised in CBFerrum/ferrum-app-cloud; here it is what the platform asks of one.

use async_trait::async_trait;
use ferrum_apps::AppManifest;
use ferrum_kernel::resources::Resources;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use ferrum_plugins::PluginHost;
use ferrum_server::appmgr::AppManager;
use ferrum_server::state::{AppsSettings, Uploads};
use loco_rs::storage::{Storage, drivers::mem};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const LEDGER_YAML: &str = "id: ledger\nname: Ledger\nversion: 0.2.0\npublisher: cbferrum\nsummary: A ledger\ndescription: README.md\ncategories: [finance]\n\
runtime: native\nplatform: \">=0.3, <0.4\"\npackages: [ledgered]\nuninstall: ask\n\
resources:\n  - {name: books, class: relational, isolation: database, authority: primary, required: true, capacity: {storage_gb: 10}}\n  - {name: cache, class: cache, isolation: prefix, authority: disposable}\n";

fn package() -> ferrum_meta::Package {
    let f: BTreeMap<String, String> = [
        (
            "package.yaml",
            "package: ledgered\nversion: 0.2.0\ndepends: [core]\nroles:\n  - name: Ledger User\n    permissions:\n      - {entity: LedgerNote, read: true}\n",
        ),
        ("entities/note.yaml", "name: LedgerNote\nlabel: Note\nkind: master\nfields:\n  - {name: title, type: data}\n"),
    ]
    .into_iter()
    .map(|(p, d)| (p.to_string(), d.to_string()))
    .collect();
    ferrum_meta::catalog::load_package_files(&f).expect("package")
}

/// What provides resources, doing nothing but remembering what it was told.
#[derive(Default)]
struct Provider {
    declared: Mutex<Vec<(String, Vec<Value>)>>,
    withdrawn: Mutex<Vec<(String, bool)>>,
    missing: Mutex<Vec<String>>,
}

#[async_trait]
impl Resources for Provider {
    async fn declare(&self, _: &Kernel, _: &Ctx, app: &str, declared: &[Value]) -> Result<()> {
        self.declared.lock().unwrap().push((app.to_string(), declared.to_vec()));
        Ok(())
    }

    async fn missing(&self, _: &Kernel, _: Uuid, _: &str, required: &[String]) -> Result<Vec<String>> {
        let missing = self.missing.lock().unwrap();
        Ok(required.iter().filter(|r| missing.contains(r)).cloned().collect())
    }

    async fn withdraw(&self, _: &Kernel, _: &Ctx, app: &str, uninstalled: bool) -> Result<()> {
        self.withdrawn.lock().unwrap().push((app.to_string(), uninstalled));
        Ok(())
    }

    async fn query(&self, _: &Kernel, _: Uuid, _: &str, _: &str, _: &str, _: &[Value]) -> Result<Value> {
        Err(Error::Unavailable("not here".into()))
    }

    async fn execute(&self, _: &Kernel, _: Uuid, _: &str, _: &str, _: &str, _: &[Value]) -> Result<u64> {
        Err(Error::Unavailable("not here".into()))
    }
}

async fn stage(hooks: HookRegistry) -> (Arc<Kernel>, AppManager, Ctx) {
    let url =
        std::env::var("FERRUM_APP_RESOURCES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_app_resources_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let dir = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let core = ferrum_meta::catalog::load_package_dir(&dir.join("core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, package()]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let settings = AppsSettings { registries: String::new(), trusted_keys: String::new(), uploads: Uploads::Off, allow_unsigned_uploads: false };
    let manager = AppManager::new(
        k.clone(),
        PluginHost::new(false).unwrap(),
        Arc::new(Storage::single(mem::new())),
        &settings,
        vec![AppManifest::parse(LEDGER_YAML).unwrap()],
    )
    .expect("manager");
    (k, manager, admin)
}

#[test]
fn declared_resources_decide_whether_an_app_can_run() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn enabled(manager: &AppManager, admin: &Ctx) -> Option<bool> {
    let o = manager.overview(admin).await.unwrap();
    o["installed"].as_array().unwrap().iter().find(|a| a["id"] == json!("ledger")).map(|a| a["enabled"] == json!(true))
}

async fn scenario() {
    // A server with nothing to provide them refuses an app that cannot run without them.
    let (_, manager, admin) = stage(HookRegistry::default()).await;
    let refused = manager.plan(&admin, "ledger", None).await.expect_err("refused");
    assert!(refused.to_string().contains("nothing to provide"), "{refused}");

    let provider = Arc::new(Provider::default());
    let mut hooks = HookRegistry::default();
    hooks.set_resources(provider.clone());
    provider.missing.lock().unwrap().push("books".into());
    let (k, manager, admin) = stage(hooks).await;

    // AT-EXT-003: what it needs is part of what is agreed to.
    let plan = manager.plan(&admin, "ledger", None).await.expect("a plan");
    assert!(plan.permissions.resources.contains("books: relational primary, required, 10 GB"), "{:?}", plan.permissions.resources);
    assert!(plan.permissions.resources.contains("cache: cache disposable"));
    assert_eq!(manager.install(&admin, "ledger", None, "not the digest", false).await.err().map(|_| ()), Some(()));

    // AT-RES-003: installed, and off until what it requires is there.
    let installed = manager.install(&admin, "ledger", None, &plan.digest, false).await;
    assert!(installed.is_ok(), "{:?}", installed.err().map(|_| ()));
    assert_eq!(enabled(&manager, &admin).await, Some(false), "it does not run short of what it depends on");
    assert_eq!(provider.declared.lock().unwrap()[0].0, "ledger");
    assert_eq!(provider.declared.lock().unwrap()[0].1.len(), 2);
    let r = manager.set_enabled(&admin, "ledger", true).await;
    assert!(matches!(&r, Err(Error::Invalid(why)) if why.contains("books")), "nor can anyone turn it on: {r:?}");
    assert_eq!(manager.activate_waiting().await.unwrap(), 0, "and looking changes nothing yet");
    assert_eq!(k.apps_waiting().await.unwrap().len(), 1);

    provider.missing.lock().unwrap().clear();
    assert_eq!(manager.activate_waiting().await.unwrap(), 1);
    assert_eq!(enabled(&manager, &admin).await, Some(true), "when it is there, it runs");
    assert!(k.apps_waiting().await.unwrap().is_empty());
    assert_eq!(manager.activate_waiting().await.unwrap(), 0);

    // AT-EXT-007: stopping it ends new access. Removing it does too, and says so.
    manager.set_enabled(&admin, "ledger", false).await.expect("off");
    assert_eq!(provider.withdrawn.lock().unwrap().last().cloned(), Some(("ledger".to_string(), false)));
    manager.set_enabled(&admin, "ledger", true).await.expect("on");
    assert!(provider.declared.lock().unwrap().len() >= 2, "and turning it on again says it needs them again");
    manager.uninstall(&admin, "ledger", false).await.expect("removed");
    assert_eq!(provider.withdrawn.lock().unwrap().last().cloned(), Some(("ledger".to_string(), true)));
}
