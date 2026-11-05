//! A site's answer that names a stored file (`SiteResponse::file`, FERRUM-OPS-006), against a real
//! PostgreSQL (`FERRUM_APPS_TEST_DATABASE_URL`, reset first) and files on a local directory: the
//! server streams that file from storage as the body, the header that named it goes, and a file of
//! another workspace is not found.

mod common;

use async_trait::async_trait;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result, SiteResponse};
use ferrum_meta::Catalog;
use ferrum_server::controllers::sites::named_file;
use ferrum_server::objects::Objects;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Files under a directory, at their keys, as the server's local storage keeps them.
struct Dir(PathBuf);

#[async_trait]
impl FileStore for Dir {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.0.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(Error::internal)?;
        std::fs::write(path, data).map_err(Error::internal)
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(|_| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        std::fs::remove_file(self.0.join(key)).map_err(Error::internal)
    }
}

#[test]
fn a_named_file_is_streamed_from_storage() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_APPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_apps_api_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let packages = ["core", "contacts", "parties"].map(|p| ferrum_meta::catalog::load_package_dir(&dir.join(p)).expect(p));
    let k = Kernel::boot(db, Catalog::from_packages(packages.to_vec()).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let root = std::env::temp_dir().join(format!("ferrum-streamed-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&root).unwrap();
    k.set_file_store(Arc::new(Dir(root.clone())));
    let objects = Objects::local(root.to_str().unwrap()).unwrap();
    let mut tenants = Vec::new();
    for slug in ["acme", "other"] {
        tenants.push(
            k.create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec!["core".into()],
                settings: None,
            })
            .await
            .unwrap(),
        );
    }
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenants[0], session.user, None, "it".into()).await.unwrap();
    let data: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
    let stored = k.store_file(&admin, None, "scan.bin", "application/octet-stream", &data).await.unwrap();
    let id: uuid::Uuid = stored["id"].as_str().unwrap().parse().unwrap();

    // Its workspace's file: streamed, with its length, and the header that named it gone.
    let mut r = SiteResponse::file(200, "application/octet-stream", vec![("content-disposition".into(), "attachment".into())], id);
    let (size, body) = named_file(&k, &objects, tenants[0], &mut r).await.unwrap().expect("a file was named");
    assert_eq!(size, data.len() as u64);
    assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap().as_ref(), data.as_slice());
    assert_eq!(r.headers, vec![("content-disposition".to_string(), "attachment".to_string())]);
    assert!(r.body.is_empty(), "nothing read whole");

    // Another workspace's answer naming it finds nothing.
    let mut theirs = SiteResponse::file(200, "application/octet-stream", vec![], id);
    assert!(matches!(named_file(&k, &objects, tenants[1], &mut theirs).await, Err(Error::NotFound(_))));

    // An answer that names no file is sent as it is.
    let mut plain = SiteResponse::json(200, &json!({"ok": true}));
    assert!(named_file(&k, &objects, tenants[0], &mut plain).await.unwrap().is_none());
    std::fs::remove_dir_all(&root).ok();
}
