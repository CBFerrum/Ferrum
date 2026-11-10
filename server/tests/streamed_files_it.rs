//! A site's answer that names a stored file (`SiteResponse::file`, FERRUM-OPS-006), against a real
//! PostgreSQL (`FERRUM_APPS_TEST_DATABASE_URL`, reset first) and files on a local directory: the
//! server streams that file from storage as the body, or the range of it a request asks for, the
//! header that named it goes, and a file of another workspace is not found. Then an upload in parts: resumed from where it stands after a
//! part's answer is lost, joined in order when complete, and swept with its parts when left; with
//! `FERRUM_TEST_S3_ENDPOINT` (CI's S3 emulator), the same in parts joined by a multipart upload, and
//! a direct upload sent to its presigned URL and completed.

mod common;

use async_trait::async_trait;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result, SiteResponse};
use ferrum_meta::Catalog;
use ferrum_server::controllers::sites::named_file;
use ferrum_server::objects::{self as up, NewUpload, Objects, PART_BYTES};
use ferrum_server::state::Settings;
use serde_json::json;
use sha2::{Digest, Sha256};
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
    let (size, part, body) = named_file(&k, &objects, tenants[0], &mut r, None).await.unwrap().expect("a file was named");
    assert_eq!((size, part), (data.len() as u64, None));
    assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap().as_ref(), data.as_slice());
    assert_eq!(r.headers, vec![("content-disposition".to_string(), "attachment".to_string())]);
    assert!(r.body.is_empty(), "nothing read whole");
    // The range a player asks for when seeking, and only that, read from storage.
    let mut r = SiteResponse::file(200, "video/mp4", vec![], id);
    let (size, part, body) = named_file(&k, &objects, tenants[0], &mut r, Some("bytes=400000-400099")).await.unwrap().expect("a file was named");
    assert_eq!((size, part), (data.len() as u64, Some(400_000..400_100)));
    assert_eq!(axum::body::to_bytes(body, usize::MAX).await.unwrap().as_ref(), &data[400_000..400_100]);

    // Another workspace's answer naming it finds nothing.
    let mut theirs = SiteResponse::file(200, "application/octet-stream", vec![], id);
    assert!(matches!(named_file(&k, &objects, tenants[1], &mut theirs, None).await, Err(Error::NotFound(_))));

    // An answer that names no file is sent as it is.
    let mut plain = SiteResponse::json(200, &json!({"ok": true}));
    assert!(named_file(&k, &objects, tenants[0], &mut plain, None).await.unwrap().is_none());

    // ───────────── an upload in parts ─────────────
    let p = PART_BYTES as usize;
    let big: Vec<u8> = (0..2 * p + 123).map(|i| (i % 251) as u8).collect();
    let part = |from: usize, to: usize| bytes::Bytes::copy_from_slice(&big[from..to]);
    let new =
        |parts| NewUpload { file_name: "big.bin".into(), content_type: "application/octet-stream".into(), size: big.len() as u64, attach_to: None, parts };
    let parts_left = |dir: &Path| !dir.exists() || std::fs::read_dir(dir).is_ok_and(|mut e| e.next().is_none());
    // Straight to object storage needs S3; in parts goes on any store.
    assert!(matches!(up::start(&k, &objects, &admin, 3 * PART_BYTES, new(false)).await, Err(Error::Conflict(_))));
    let started = up::start(&k, &objects, &admin, 3 * PART_BYTES, new(true)).await.unwrap();
    assert_eq!((started["parts"].clone(), started["offset"].clone(), started["part_bytes"].clone()), (json!(true), json!(0), json!(PART_BYTES)));
    let upload: uuid::Uuid = started["upload_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(up::append(&k, &objects, &admin, upload, 0, part(0, p)).await.unwrap()["offset"], json!(PART_BYTES));
    // The first part again (its answer was lost): refused, and the upload says where it stands.
    assert!(matches!(up::append(&k, &objects, &admin, upload, 0, part(0, p)).await, Err(Error::Conflict(_))));
    assert_eq!(up::status(&k, &admin, upload).await.unwrap()["offset"], json!(PART_BYTES));
    assert!(matches!(up::complete(&k, &objects, &admin, upload).await, Err(Error::Conflict(_))), "not every byte has arrived");
    let oversized = up::append(&k, &objects, &admin, upload, PART_BYTES, bytes::Bytes::from(vec![0u8; p + 1])).await;
    assert!(matches!(oversized, Err(Error::Validation(_))), "a part is at most PART_BYTES: {oversized:?}");
    up::append(&k, &objects, &admin, upload, PART_BYTES, part(p, 2 * p)).await.unwrap();
    let past = up::append(&k, &objects, &admin, upload, 2 * PART_BYTES, bytes::Bytes::from(vec![0u8; 124])).await;
    assert!(matches!(past, Err(Error::Validation(_))), "past the size announced: {past:?}");
    // Another workspace's admin does not find it.
    let theirs = k.login("other", "admin@other.test", "Correct-Horse-9").await.unwrap();
    let other = k.load_ctx(tenants[1], theirs.user, None, "it".into()).await.unwrap();
    assert!(matches!(up::status(&k, &other, upload).await, Err(Error::NotFound(_))));
    assert!(matches!(up::append(&k, &objects, &other, upload, 2 * PART_BYTES, part(2 * p, big.len())).await, Err(Error::NotFound(_))));
    assert_eq!(up::append(&k, &objects, &admin, upload, 2 * PART_BYTES, part(2 * p, big.len())).await.unwrap()["offset"], json!(big.len()));
    let file = up::complete(&k, &objects, &admin, upload).await.unwrap();
    assert_eq!((file["size"].clone(), file["sha256"].clone()), (json!(big.len()), json!(hex::encode(Sha256::digest(&big)))));
    assert!(std::fs::read(root.join(format!("tenants/{}/{upload}", tenants[0]))).unwrap() == big, "the parts joined in order");
    assert!(parts_left(&root.join(format!("uploads/{}/{upload}.parts", tenants[0]))), "the parts go once the file is recorded");
    assert!(matches!(up::complete(&k, &objects, &admin, upload).await, Err(Error::NotFound(_))), "recorded once");

    // One left half-sent is swept with its parts once it expires.
    let left = up::start(&k, &objects, &admin, 3 * PART_BYTES, new(true)).await.unwrap();
    let left: uuid::Uuid = left["upload_id"].as_str().unwrap().parse().unwrap();
    up::append(&k, &objects, &admin, left, 0, part(0, p)).await.unwrap();
    let dir = root.join(format!("uploads/{}/{left}.parts", tenants[0]));
    assert!(!parts_left(&dir), "a part waits");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update file_upload set expires_at = now() - interval '1 hour' where id = $1").bind(left).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(up::sweep(&k, &objects).await.unwrap(), 1);
    assert!(parts_left(&dir), "swept with its parts");

    // ───────────── who ties an upload to a record ─────────────
    // Only whoever may change the record: a member or an outside person who may only read it may not.
    let rule = |v: serde_json::Value| serde_json::from_value::<ferrum_meta::PermissionDef>(v).unwrap();
    k.set_role_permission(&admin, "Reader", &rule(json!({"entity": "Uom", "read": true}))).await.unwrap();
    k.set_role_permission(&admin, "Writer", &rule(json!({"entity": "Uom", "read": true, "write": true}))).await.unwrap();
    k.set_role_permission(&admin, "Own writer", &rule(json!({"entity": "Uom", "read": true, "write": true, "create": true, "if_owner": true}))).await.unwrap();
    k.set_role_permission(&admin, "Visitors", &rule(json!({"entity": "Uom", "read": true}))).await.unwrap();
    k.set_role_portal(&admin, "Visitors", true).await.expect("units are reference data, read whole");
    let person = |email: &'static str, role: &'static str, portal: bool| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let id = k.create_user(&admin, email, email, None, &[], false).await.unwrap();
            if portal {
                k.set_user_kind(&admin, id, "portal").await.unwrap();
            }
            k.set_user_roles_until(&admin, id, &[role.to_string()], &Default::default()).await.unwrap();
            k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
        }
    };
    let (reader, writer, visitor) = (
        person("reader@acme.test", "Reader", false).await,
        person("writer@acme.test", "Writer", false).await,
        person("visitor@acme.test", "Visitors", true).await,
    );
    let uom: uuid::Uuid =
        k.insert(&admin, "Uom", json!({"uom_name": "Crate of twelve"}).as_object().cloned().unwrap()).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let onto = || NewUpload { attach_to: Some(("Uom".into(), uom)), ..new(true) };
    k.get(&reader, "Uom", uom, false).await.expect("the reader reads it");
    let refused = up::start(&k, &objects, &reader, 3 * PART_BYTES, onto()).await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "a member who may only read the record attaches to it: {refused:?}");
    let refused = up::start(&k, &objects, &visitor, 3 * PART_BYTES, onto()).await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "an outside person who may only read the record attaches to it: {refused:?}");
    up::start(&k, &objects, &writer, 3 * PART_BYTES, onto()).await.expect("a member who may change the record");
    // One who reads every unit and changes only their own: not this one.
    let own = person("own@acme.test", "Own writer", false).await;
    k.set_user_roles_until(&admin, own.user, &["Reader".into(), "Own writer".into()], &Default::default()).await.unwrap();
    let own = k.load_ctx(admin.tenant, own.user, None, "it".into()).await.unwrap();
    let refused = up::start(&k, &objects, &own, 3 * PART_BYTES, onto()).await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "a member who may change only their own units: {refused:?}");
    // An outside person's uploads tied to nothing are counted: 20 an hour.
    for _ in 0..20 {
        up::start(&k, &objects, &visitor, 3 * PART_BYTES, new(true)).await.expect("within the hour's uploads");
    }
    assert!(matches!(up::start(&k, &objects, &visitor, 3 * PART_BYTES, new(true)).await, Err(Error::TooManyRequests(_))), "past them");
    up::start(&k, &objects, &writer, 3 * PART_BYTES, new(true)).await.expect("a member's are not counted");

    // ───────────── through S3 ─────────────
    if let Ok(endpoint) = std::env::var("FERRUM_TEST_S3_ENDPOINT") {
        let bucket = std::env::var("FERRUM_TEST_S3_BUCKET").unwrap_or_else(|_| "ferrum-ci".into());
        let settings: Settings = serde_json::from_value(json!({"s3_bucket": bucket, "s3_region": "us-east-1", "s3_endpoint": endpoint})).unwrap();
        let s3 = Objects::new(&settings).unwrap();
        let sha = json!(hex::encode(Sha256::digest(&big)));
        // In parts, joined by a multipart upload of 8 MiB chunks.
        let started = up::start(&k, &s3, &admin, 3 * PART_BYTES, new(true)).await.unwrap();
        let id: uuid::Uuid = started["upload_id"].as_str().unwrap().parse().unwrap();
        for (from, to) in [(0, p), (p, 2 * p), (2 * p, big.len())] {
            up::append(&k, &s3, &admin, id, from as u64, part(from, to)).await.unwrap();
        }
        assert_eq!(up::complete(&k, &s3, &admin, id).await.unwrap()["sha256"], sha);
        let (size, _, body) = s3.stream(&format!("tenants/{}/{id}", tenants[0]), None).await.unwrap();
        assert_eq!(size, big.len() as u64);
        assert!(axum::body::to_bytes(body, usize::MAX).await.unwrap().as_ref() == big.as_slice(), "joined in order on S3");
        // Straight to the bucket with the presigned request, then completed.
        let direct = up::start(&k, &s3, &admin, 3 * PART_BYTES, new(false)).await.unwrap();
        let id: uuid::Uuid = direct["upload_id"].as_str().unwrap().parse().unwrap();
        let mut put = reqwest::Client::new()
            .request(reqwest::Method::from_bytes(direct["method"].as_str().unwrap().as_bytes()).unwrap(), direct["url"].as_str().unwrap());
        for (name, value) in direct["headers"].as_object().unwrap() {
            put = put.header(name.as_str(), value.as_str().unwrap());
        }
        let sent = put.body(big.clone()).send().await.unwrap();
        assert!(sent.status().is_success(), "the presigned PUT: {}", sent.status());
        assert_eq!(up::complete(&k, &s3, &admin, id).await.unwrap()["sha256"], sha);
        let (size, _, body) = s3.stream(&format!("tenants/{}/{id}", tenants[0]), None).await.unwrap();
        assert_eq!(size, big.len() as u64);
        assert!(axum::body::to_bytes(body, usize::MAX).await.unwrap().as_ref() == big.as_slice(), "copied to the file's key");
    } else {
        eprintln!("S3: skipped, FERRUM_TEST_S3_ENDPOINT is not set");
    }
    std::fs::remove_dir_all(&root).ok();
}
