//! The Brand record (docs/builder/decisions.md BD-79) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset by each test, which take their turn): its files are this workspace's, uploaded for it; one brand
//! at most is the default however many saves race; a host shows only a brand of its own workspace; only an
//! administrator keeps brands; and a workspace made before brands existed (core 0.3.0) shows the platform's look after
//! the upgrade, until it makes one.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
const PASSWORD: &str = "Correct-Horse-9";

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

async fn db() -> Db {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 20, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db
}

async fn boot(db: Db, packages: Vec<ferrum_meta::Package>) -> Arc<Kernel> {
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    Kernel::boot(db, Catalog::from_packages(packages).expect("catalog"), hooks, KernelSettings::default(), None).await.expect("boot")
}

/// A workspace and its administrator.
async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .expect("workspace");
    let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
    k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap()
}

/// A stored file's row as an upload leaves it (its bytes are not needed here): `/files/{id}`.
async fn upload(k: &Kernel, by: &Ctx, ctype: &str, size: i64, tied_to: Option<(&str, Uuid)>) -> String {
    let id = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(by.tenant).await.unwrap();
    sqlx::query(
        "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by, is_public, shared) \
         values ($1, $2, $3, $4, 'f', $5, $6, $7, '00', $8, false, false)",
    )
    .bind(by.tenant)
    .bind(id)
    .bind(tied_to.map(|t| t.0))
    .bind(tied_to.map(|t| t.1))
    .bind(ctype)
    .bind(size)
    .bind(format!("tenants/{}/{id}", by.tenant))
    .bind(by.user)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    format!("/files/{id}")
}

fn refused_on(e: Error, field: &str) {
    match e {
        Error::Validation(errs) => assert!(errs.iter().any(|f| f.field == field), "{field}: {errs:?}"),
        other => panic!("{field}: not a validation error: {other:?}"),
    }
}

#[test]
fn files_must_be_this_workspaces_public_images() {
    run(|| async {
        let k = boot(db().await, ferrum_domain::packages().unwrap()).await;
        let acme = workspace(&k, "acme").await;
        let other = workspace(&k, "other").await;
        let brand = k.insert(&acme, "Brand", obj(json!({"brand_name": "Acme", "primary_color": "#FF0"}))).await.unwrap();
        let id = id_of(&brand);
        // The colours shown are saved with it: given in short form, nudged in light mode, as given in dark.
        assert_eq!(
            (brand["primary_color"].as_str(), brand["shown_color"].as_str(), brand["shown_color_dark"].as_str()),
            (Some("#ffff00"), Some("#707000"), Some("#ffff00"))
        );
        let patch = |field: &str, v: String| obj(json!({ field: v }));

        // Another workspace's file, another record's, one of a type the field does not take, one too large, an address
        // elsewhere: each refused.
        let theirs = upload(&k, &other, "image/png", 10, None).await;
        refused_on(k.update(&acme, "Brand", id, patch("logo", theirs), None).await.unwrap_err(), "logo");
        let elsewhere = upload(&k, &acme, "image/png", 10, Some(("Company", Uuid::now_v7()))).await;
        refused_on(k.update(&acme, "Brand", id, patch("logo", elsewhere), None).await.unwrap_err(), "logo");
        for (field, ctype) in
            [("logo", "image/svg+xml"), ("logo", "application/pdf"), ("favicon", "image/jpeg"), ("font_file", "image/png"), ("logo_dark", "text/html")]
        {
            let f = upload(&k, &acme, ctype, 10, None).await;
            refused_on(k.update(&acme, "Brand", id, patch(field, f), None).await.unwrap_err(), field);
        }
        let big = upload(&k, &acme, "image/png", 3 << 20, None).await;
        refused_on(k.update(&acme, "Brand", id, patch("logo", big), None).await.unwrap_err(), "logo");
        refused_on(k.update(&acme, "Brand", id, patch("logo", "https://evil.example/logo.png".into()), None).await.unwrap_err(), "logo");
        // Its own typeface is needed for Own; links must be safe on a page, eight at most.
        refused_on(k.update(&acme, "Brand", id, obj(json!({"font": "Own"})), None).await.unwrap_err(), "font_file");
        refused_on(k.update(&acme, "Brand", id, obj(json!({"links": [{"label": "x", "url": "javascript:alert(1)"}]})), None).await.unwrap_err(), "url");
        let nine: Vec<Value> = (0..9).map(|i| json!({"label": format!("l{i}"), "url": "/p"})).collect();
        refused_on(k.update(&acme, "Brand", id, obj(json!({ "links": nine })), None).await.unwrap_err(), "links");

        // Uploaded here: tied to the brand by the save, and kept on later saves.
        let (logo, favicon, font) =
            (upload(&k, &acme, "image/webp", 10, None).await, upload(&k, &acme, "image/png", 10, None).await, upload(&k, &acme, "font/woff2", 10, None).await);
        let saved = k
            .update(
                &acme,
                "Brand",
                id,
                obj(json!({"logo": logo, "favicon": favicon, "font": "Own", "font_file": font, "links": [{"label": "Privacy", "url": "/privacy"}, {"label": "Mail", "url": "mailto:hi@acme.test"}]})),
                None,
            )
            .await
            .unwrap();
        let mut tx = k.db.tenant_tx(acme.tenant).await.unwrap();
        let tied: Vec<(Option<String>, Option<Uuid>)> = sqlx::query_as("select entity, record_id from file_object where tenant_id = $1 and id = any($2)")
            .bind(acme.tenant)
            .bind([&logo, &favicon, &font].map(|f| f["/files/".len()..].parse::<Uuid>().unwrap()).to_vec())
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(tied.len() == 3 && tied.iter().all(|t| t == &(Some("Brand".into()), Some(id))), "{tied:?}");
        k.update(&acme, "Brand", id, obj(json!({"footer_text": "Acme Ltd"})), Some(saved["version"].as_i64().unwrap())).await.unwrap();

        // What pages get: no default yet, so only when named; then as the default.
        assert_eq!(k.public_brand(acme.tenant, None, None).await.unwrap(), None);
        let shown = k.public_brand(acme.tenant, Some(id), None).await.unwrap().expect("named");
        assert_eq!((shown.files.clone(), shown.footer_text.as_str(), shown.links.len()), (vec!["logo", "favicon", "font_file"], "Acme Ltd", 2));
        k.update(&acme, "Brand", id, obj(json!({"is_default": true})), None).await.unwrap();
        assert_eq!(k.public_brand(acme.tenant, None, None).await.unwrap().map(|b| b.id), Some(id));
        // Never another workspace's, by any way of naming it.
        assert_eq!(k.public_brand(other.tenant, Some(id), Some(id)).await.unwrap(), None);
        assert_eq!(k.brand_by_id(other.tenant, id).await.unwrap(), None);
        assert_eq!(k.brand_file(other.tenant, id, "logo").await.unwrap(), None);
    });
}

#[test]
fn one_default_under_concurrent_saves() {
    run(|| async {
        let k = boot(db().await, ferrum_domain::packages().unwrap()).await;
        let acme = workspace(&k, "acme").await;
        let mut ids = Vec::new();
        for i in 0..8 {
            ids.push(id_of(&k.insert(&acme, "Brand", obj(json!({"brand_name": format!("B{i}"), "is_default": i == 0}))).await.unwrap()));
        }
        // Eight saves at once, each making its brand the default.
        let saves: Vec<_> = ids
            .iter()
            .map(|id| {
                let (k, acme, id) = (k.clone(), acme.clone(), *id);
                tokio::spawn(async move { k.update(&acme, "Brand", id, obj(json!({"is_default": true, "footer_text": "x"})), None).await })
            })
            .collect();
        for s in saves {
            s.await.unwrap().unwrap();
        }
        let mut tx = k.db.tenant_tx(acme.tenant).await.unwrap();
        let defaults: i64 =
            sqlx::query_scalar("select count(*) from e_brand where tenant_id = $1 and is_default").bind(acme.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(defaults, 1);
        // A new default takes the place of the old one.
        let newer = id_of(&k.insert(&acme, "Brand", obj(json!({"brand_name": "Newer", "is_default": true}))).await.unwrap());
        assert_eq!(k.public_brand(acme.tenant, None, None).await.unwrap().map(|b| b.id), Some(newer));
    });
}

#[test]
fn host_brand_is_this_workspaces() {
    run(|| async {
        let k = boot(db().await, ferrum_domain::packages().unwrap()).await;
        let acme = workspace(&k, "acme").await;
        let other = workspace(&k, "other").await;
        let ours = id_of(&k.insert(&acme, "Brand", obj(json!({"brand_name": "Shop"}))).await.unwrap());
        let theirs = id_of(&k.insert(&other, "Brand", obj(json!({"brand_name": "Theirs"}))).await.unwrap());
        k.add_subdomain(acme.tenant, "acme.ferrum.test").await.unwrap();
        k.add_subdomain(other.tenant, "other.ferrum.test").await.unwrap();
        k.add_host(&acme, "my.acme.test", "portal").await.unwrap();
        k.host_checked("my.acme.test", true).await.unwrap();

        // Another workspace's brand, or a host of another workspace: refused, and nothing changes.
        refused_on(k.set_host_brand(&acme, "my.acme.test", Some(theirs)).await.unwrap_err(), "brand");
        refused_on(k.set_host_brand(&acme, "my.acme.test", Some(Uuid::now_v7())).await.unwrap_err(), "brand");
        assert!(matches!(k.set_host_brand(&acme, "other.ferrum.test", Some(ours)).await.unwrap_err(), Error::NotFound(_)));
        assert_eq!(k.host_brand("other.ferrum.test").await.unwrap(), None);

        // Its own: the host shows it, and a page there reads it as the host's.
        k.set_host_brand(&acme, "MY.acme.test", Some(ours)).await.unwrap();
        assert_eq!(k.host_brand("my.acme.test").await.unwrap(), Some(ours));
        assert_eq!(k.list_hosts(&acme).await.unwrap().iter().find(|h| h.host == "my.acme.test").unwrap().brand, Some(ours));
        assert_eq!(k.public_brand(acme.tenant, None, Some(ours)).await.unwrap().map(|b| b.id), Some(ours));
        // A host brand that is not the workspace's (a stale id) is never shown: the default, here none.
        assert_eq!(k.public_brand(acme.tenant, None, Some(theirs)).await.unwrap(), None);
        // Cleared: the default again.
        k.set_host_brand(&acme, "my.acme.test", None).await.unwrap();
        assert_eq!(k.host_brand("my.acme.test").await.unwrap(), None);
    });
}

#[test]
fn a_person_without_the_right_cannot_create_or_change_a_brand() {
    run(|| async {
        let k = boot(db().await, ferrum_domain::packages().unwrap()).await;
        let acme = workspace(&k, "acme").await;
        let brand = id_of(&k.insert(&acme, "Brand", obj(json!({"brand_name": "Acme"}))).await.unwrap());
        let ann = k.create_user(&acme, "ann@acme.test", "Ann", Some(PASSWORD), &[], false).await.unwrap();
        let ann = k.load_ctx(acme.tenant, ann, None, "it".into()).await.unwrap();
        let forbidden = |e: Error| assert!(matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "{e:?}");
        forbidden(k.insert(&ann, "Brand", obj(json!({"brand_name": "Mine"}))).await.unwrap_err());
        forbidden(k.update(&ann, "Brand", brand, obj(json!({"primary_color": "#000000"})), None).await.unwrap_err());
        forbidden(k.delete(&ann, "Brand", brand, None).await.unwrap_err());
        k.add_subdomain(acme.tenant, "acme.ferrum.test").await.unwrap();
        forbidden(k.set_host_brand(&ann, "acme.ferrum.test", Some(brand)).await.unwrap_err());
    });
}

/// Core as it was before brands (0.3.0): the files of `packages/core` but `brand.yaml` and `embedding.yaml`.
fn core_0_3_0() -> ferrum_meta::Package {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core");
    let mut files = BTreeMap::new();
    let package = std::fs::read_to_string(dir.join("package.yaml")).unwrap();
    let package: String = package.lines().map(|l| if l.starts_with("version: ") { "version: 0.3.0\n".to_string() } else { format!("{l}\n") }).collect();
    files.insert("package.yaml".to_string(), package);
    for e in std::fs::read_dir(dir.join("entities")).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().to_string();
        if !["brand.yaml", "embedding.yaml"].contains(&name.as_str()) {
            files.insert(format!("entities/{name}"), std::fs::read_to_string(e.path()).unwrap());
        }
    }
    ferrum_meta::catalog::load_package_files(&files).expect("core 0.3.0")
}

#[test]
fn a_workspace_from_core_0_3_0_shows_its_pages_unchanged_after_the_upgrade() {
    run(|| async {
        let db = db().await;
        let old: Vec<_> = ferrum_domain::packages().unwrap().into_iter().map(|p| if p.name() == "core" { core_0_3_0() } else { p }).collect();
        let before = boot(db.clone(), old).await;
        let acme = workspace(&before, "acme").await;
        assert!(acme.schema.entity("Brand").is_none(), "no brands at 0.3.0");
        assert_eq!(before.public_brand(acme.tenant, None, None).await.unwrap(), None);
        drop(before);

        // The new build starts on the same database: brands exist, the workspace shows the platform's look until it
        // makes one, and then the one it makes.
        let k = boot(db, ferrum_domain::packages().unwrap()).await;
        let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let admin = k.load_ctx(acme.tenant, s.user, None, "it".into()).await.unwrap();
        assert!(admin.schema.entity("Brand").is_some());
        assert_eq!(k.public_brand(acme.tenant, None, None).await.unwrap(), None);
        let brand = id_of(&k.insert(&admin, "Brand", obj(json!({"brand_name": "Acme", "is_default": true}))).await.unwrap());
        assert_eq!(k.public_brand(acme.tenant, None, None).await.unwrap().map(|b| b.id), Some(brand));
    });
}
