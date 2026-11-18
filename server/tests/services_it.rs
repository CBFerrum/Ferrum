//! Optional services and what an app wants or needs of them (docs/operations.md, "Optional services"): a real App Manager
//! on a real PostgreSQL (`FERRUM_SERVICES_TEST_DATABASE_URL`). A `needs` the installation has not set up refuses the plan
//! with a sentence an operator can act on; one it has installs; a `wants` installs either way and says so; the table the
//! operator reads and the doctor's lines list every service, with the apps that declared it.

mod common;

use ferrum_apps::AppManifest;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_plugins::PluginHost;
use ferrum_server::appmgr::AppManager;
use ferrum_server::services;
use ferrum_server::state::{AppsSettings, Uploads};
use loco_rs::storage::{Storage, drivers::mem};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path as FsPath;
use std::sync::Arc;

/// An app of one package that says what it wants and needs, in the manifest's own words.
fn app(id: &str, services: &str) -> AppManifest {
    AppManifest::parse(&format!(
        "id: {id}\nname: {id}\nversion: 0.2.0\npublisher: cbferrum\nsummary: An app\ndescription: README.md\ncategories: [finance]\n\
         runtime: native\nplatform: \">=0.4, <0.5\"\npackages: [served]\nuninstall: ask\n{services}"
    ))
    .expect("manifest")
}

fn package() -> ferrum_meta::Package {
    let f: BTreeMap<String, String> = [
        (
            "package.yaml",
            "package: served\nversion: 0.2.0\ndepends: [core]\nroles:\n  - name: Served User\n    permissions:\n      - {entity: ServedNote, read: true}\n",
        ),
        ("entities/note.yaml", "name: ServedNote\nlabel: Note\nkind: master\nfields:\n  - {name: title, type: data}\n"),
    ]
    .into_iter()
    .map(|(p, d)| (p.to_string(), d.to_string()))
    .collect();
    ferrum_meta::catalog::load_package_files(&f).expect("package")
}

async fn stage() -> (Arc<Kernel>, AppManager, Ctx) {
    let url = std::env::var("FERRUM_SERVICES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_services_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let dir = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let core = ferrum_meta::catalog::load_package_dir(&dir.join("core")).expect("core");
    let k =
        Kernel::boot(db, Catalog::from_packages(vec![core, package()]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
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
    let natives = vec![
        app("needs-keys", "needs: [keys.external]\n"),
        app("needs-search", "needs: [search]\n"),
        app("wants-keys", "wants: [keys.external]\n"),
        app("plain", ""),
    ];
    let manager = AppManager::new(k.clone(), PluginHost::new(false).unwrap(), Arc::new(Storage::single(mem::new())), &settings, natives).expect("manager");
    (k, manager, admin)
}

#[test]
fn what_an_app_needs_refuses_it_and_what_it_wants_never_does() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let (k, manager, admin) = stage().await;

    // Nothing is set up: a need is refused, as a conflict, naming the app, the service, what to set and where it is written.
    let Err(refused) = manager.plan(&admin, "needs-keys", None).await else { panic!("planned an app whose service is not set up") };
    assert!(matches!(refused, Error::Conflict(_)), "{refused:?}");
    let text = refused.to_string();
    assert!(
        text.contains("needs-keys needs keys.external") && text.contains("an operator sets kms_transit") && text.contains("\"Optional services\""),
        "{text}"
    );
    // One this build does not have at all says that instead of a setting.
    let text = manager.plan(&admin, "needs-search", None).await.expect_err("refused").to_string();
    assert!(text.contains("needs-search needs search") && text.contains("does not have yet"), "{text}");
    // The refusal comes before any agreement: installing it is refused the same way, whatever consent is given.
    assert!(manager.install(&admin, "needs-keys", None, "any", false).await.is_err());

    // A want installs either way, and the dialog says whether it is there.
    let plan = manager.plan(&admin, "wants-keys", None).await.expect("a want does not refuse");
    assert_eq!(plan.app.services.len(), 1);
    assert_eq!((plan.app.services[0].id.as_str(), plan.app.services[0].configured), ("keys.external", false));
    assert!(plan.app.services[0].note.contains("works without keys.external"), "{:?}", plan.app.services);
    assert!(manager.plan(&admin, "plain", None).await.unwrap().app.services.is_empty(), "an app that names none says nothing");

    assert!(manager.declared_services().await.unwrap().is_empty(), "nobody has installed anything that names one yet");

    // Set up, the same plans pass; the dialog says it is there.
    manager.set_services(|| BTreeSet::from(["keys.external"]));
    let plan = manager.plan(&admin, "wants-keys", None).await.unwrap();
    assert!(plan.app.services[0].configured && plan.app.services[0].note.starts_with("wants-keys uses keys.external"), "{:?}", plan.app.services);
    let plan = manager.plan(&admin, "needs-keys", None).await.expect("its need is met");
    manager.install(&admin, "needs-keys", None, &plan.digest, false).await.unwrap_or_else(|e| panic!("{e:?}"));
    // The one package they share is in the workspace now, which counts every compiled-in app of it as installed.

    // What the operator reads: five services, one set up, and who declared what; the doctor has a line for each.
    let declared = manager.declared_services().await.unwrap();
    assert_eq!(
        declared,
        vec![
            ("needs-keys".to_string(), vec![], vec!["keys.external".to_string()]),
            ("needs-search".to_string(), vec![], vec!["search".to_string()]),
            ("wants-keys".to_string(), vec!["keys.external".to_string()], vec![])
        ]
    );
    let rows = services::table(&BTreeSet::from(["keys.external"]), &declared);
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), ferrum_apps::SERVICES);
    assert_eq!(rows.iter().filter(|r| r.configured).count(), 1);
    assert_eq!((rows[0].wanted_by.clone(), rows[0].needed_by.clone()), (vec!["wants-keys".to_string()], vec!["needs-keys".to_string()]));
    assert_eq!(services::findings(&rows).len(), 5);
    // Unset later: the apps keep running, and the doctor warns, naming the app that needs it.
    let after = services::findings(&services::table(&BTreeSet::new(), &declared));
    let line = after.iter().find(|f| f.check == "services.keys.external").unwrap();
    assert!(line.level == ferrum_kernel::doctor::Level::Warning && line.detail.contains("needs-keys"), "{line:?}");

    // The age of the oldest event a consumer has not taken: young at first, then a 90 s old one, then none once it took them all.
    assert!(services::lag(&k.db, "search").await.unwrap().is_none_or(|s| s < 60), "only what the workspace's making wrote, seconds old");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("insert into outbox (tenant_id, topic, payload, created_at) select id, 'x.y', '{}', now() - interval '90 seconds' from tenant limit 1")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(services::lag(&k.db, "search").await.unwrap().is_some_and(|s| s >= 90), "an event nobody took");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("insert into event_receipt (tenant_id, consumer, event_id) select tenant_id, 'search', id from outbox").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(services::lag(&k.db, "search").await.unwrap(), None, "taken by that consumer");
    assert!(services::lag(&k.db, "other").await.unwrap().is_some(), "not by another");
}
