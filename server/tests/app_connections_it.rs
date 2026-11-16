//! What an administrator sees of how the workspace's apps use each other (`GET /api/admin/apps/connections`), against a
//! real PostgreSQL (`FERRUM_APP_CONNECTIONS_TEST_DATABASE_URL`, reset first): a carrier offers shipping, a shop declares
//! that it uses it, calls it once and runs a hook on the carrier's parcels. The three lists name each.

mod common;

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use ferrum_server::controllers::admin::connections;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const CARRIER: &str = "package: carrier\nversion: 1.2.0\ndepends: [core]\n";
const PARCEL: &str = r#"
- name: Parcel
  label: Parcel
  kind: master
  fields:
    - {name: title, type: data, required: true}
  actions:
    - {name: ship, label: Ship, permission: write, hidden: true, contract: {completion: The parcel is shipped}}
"#;
const SHOP: &str = "package: shop\nversion: 0.1.0\ndepends: [carrier]\nuses:\n  - {capability: carrier.parcel.ship, version: '^1.2'}\n";

/// The carrier's code: shipping says so.
struct Carrier;

#[async_trait]
impl DocHook for Carrier {
    async fn action(&self, _: &mut HookCtx<'_>, name: &str, _: &Doc, _: &Value) -> Result<Option<Value>> {
        Ok((name == "ship").then(|| json!({"shipped": true})))
    }
}

/// The shop's hook on the carrier's parcels.
struct Watch;

impl DocHook for Watch {
    fn name(&self) -> &'static str {
        "shop.watch"
    }
}

fn package(files: &[(&str, &str)]) -> ferrum_meta::Package {
    let files: BTreeMap<String, String> = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    ferrum_meta::catalog::load_package_files(&files).expect("package")
}

#[test]
fn an_administrator_sees_what_apps_use_hook_and_call() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_APP_CONNECTIONS_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_app_connections_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    common::open_vault(&db, &url).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let catalog =
        Catalog::from_packages(vec![core, package(&[("package.yaml", CARRIER), ("entities/parcel.yaml", PARCEL)]), package(&[("package.yaml", SHOP)])])
            .expect("catalog");
    let mut hooks = HookRegistry::default();
    let mut shop = None;
    hooks.app("carrier", &["carrier".to_string()], |h| {
        h.register("Parcel", Arc::new(Carrier));
    });
    hooks.app("shop", &["shop".to_string()], |h| {
        h.register("Parcel", Arc::new(Watch));
        shop = Some(h.vault());
    });
    hooks.own("carrier", &["carrier".to_string()]).own("shop", &["shop".to_string()]);
    let shop = shop.expect("the shop's handle");
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shop".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin: Ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let parcel = k.insert(&admin, "Parcel", json!({"title": "Box 1"}).as_object().cloned().unwrap()).await.unwrap();

    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let ship = Invocation::new("carrier.parcel.ship", json!({"id": parcel["id"], "params": {}})).key("order-1");
    k.call_tx(&shop, &admin, &mut tx, &ship).await.expect("the shop ships");
    tx.commit().await.unwrap();

    let seen = connections(&k, &admin).await.unwrap();
    assert_eq!(seen["uses"], json!([{"app": "shop", "package": "shop", "capability": "carrier.parcel.ship", "range": "^1.2", "installed": "1.2.0"}]), "{seen}");
    assert_eq!(seen["hooks"], json!([{"app": "shop", "entity": "Parcel", "hook": "shop.watch"}]), "the carrier's own hook is not listed: {seen}");
    let calls = seen["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{seen}");
    assert_eq!((calls[0]["app"].as_str(), calls[0]["capability"].as_str(), calls[0]["count"].as_i64()), (Some("shop"), Some("carrier.parcel.ship"), Some(1)));
    assert!(calls[0]["latest"].is_string(), "{seen}");

    // Administrators only.
    let clerk = k.create_user(&admin, "clerk@acme.test", "clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(connections(&k, &clerk).await.is_err(), "a clerk sees none of it");
}
