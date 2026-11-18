//! The Embedding record (docs/builder/decisions.md BD-80) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset): a page is a plain path, its origins are outside origins checked one by one and kept one per
//! line, ten at most; only an administrator keeps them.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};

const PASSWORD: &str = "Correct-Horse-9";

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn refused_on(e: Error, field: &str) {
    match e {
        Error::Validation(errs) => assert!(errs.iter().any(|f| f.field == field), "{field}: {errs:?}"),
        other => panic!("{field}: not a validation error: {other:?}"),
    }
}

#[test]
fn origins_validated_and_capped() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let catalog = Catalog::from_packages(ferrum_domain::packages().unwrap()).expect("catalog");
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();

    // Kept tidy: one origin per line, blanks and spaces dropped.
    let saved = k
        .insert(&admin, "Embedding", obj(json!({"page": "forms/f/contact", "origins": "  https://www.partner.example \n\nhttp://localhost:3000\n"})))
        .await
        .unwrap();
    assert_eq!(saved["origins"], "https://www.partner.example\nhttp://localhost:3000");

    // A page is a plain path of lower-case words: no capitals, empty parts, dots, underscores or wildcards, at most
    // eight parts.
    for page in ["Forms/f", "forms//f", "forms/f/", "/forms/f", "../x", "forms_f", "forms/%", "a/b/c/d/e/f/g/h/i", "forms/f?x=1"] {
        let e = k.insert(&admin, "Embedding", obj(json!({"page": page, "origins": "https://a.example"}))).await.unwrap_err();
        refused_on(e, "page");
    }
    // Origins: an origin, not an address with a path; https except on localhost; no scripts; ten at most.
    for origins in ["https://partner.example/path", "http://partner.example", "javascript:alert(1)", "https://a.example https://b.example", "*"] {
        let e = k.insert(&admin, "Embedding", obj(json!({"page": "bookings/book", "origins": origins}))).await.unwrap_err();
        refused_on(e, "origins");
    }
    let eleven: Vec<String> = (0..11).map(|i| format!("https://s{i}.example")).collect();
    refused_on(k.insert(&admin, "Embedding", obj(json!({"page": "bookings/book", "origins": eleven.join("\n")}))).await.unwrap_err(), "origins");
    k.insert(&admin, "Embedding", obj(json!({"page": "bookings/book", "origins": eleven[..10].join("\n")}))).await.expect("ten");

    // Only an administrator.
    let ann = k.create_user(&admin, "ann@acme.test", "Ann", Some(PASSWORD), &[], false).await.unwrap();
    let ann = k.load_ctx(tenant, ann, None, "it".into()).await.unwrap();
    let e = k.insert(&ann, "Embedding", obj(json!({"page": "sign/s", "origins": "https://a.example"}))).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "{e:?}");
}
