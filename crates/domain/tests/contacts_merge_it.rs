//! Two contacts for one person, merged, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): they are found as likely duplicates, records linking to the duplicate move
//! to the contact that stays, and the duplicate's own emails and phones stay with it unless the merge
//! carries them in its chosen values.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// A customer's main contact, as an app's package links to contacts.
const LINKS: &str = "{package: links, version: '1', depends: [parties], overlays: [{entity: Customer, ops: [{op: add_field, column: true, \
     field: {name: main_contact, type: link, to: Contact}}]}]}";

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn emails(d: &Doc) -> Vec<String> {
    let mut v: Vec<String> = d["emails"].as_array().unwrap().iter().map(|r| r["email"].as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn contacts_merge() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("packages");
    packages.push(ferrum_meta::catalog::load_package_files(&BTreeMap::from([("package.yaml".to_string(), LINKS.to_string())])).expect("links"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let k = Kernel::boot(db, Catalog::from_packages(packages).expect("catalog"), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "people".into(),
            name: "People".into(),
            admin_email: "admin@people.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["parties".into(), "links".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let s = k.login("people", "admin@people.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();

    let keep = k
        .insert(
            &admin,
            "Contact",
            obj(json!({"first_name": "Mira", "last_name": "Sen", "emails": [{"email": "mira@globex.test"}], "phones": [{"phone": "+91 98100 00001"}]})),
        )
        .await
        .unwrap();
    let dup = k
        .insert(
            &admin,
            "Contact",
            obj(json!({"first_name": "Mira", "last_name": "Sen", "emails": [{"email": "MIRA@globex.test"}, {"email": "m.sen@home.test"}]})),
        )
        .await
        .unwrap();
    let (keep_id, dup_id) = (id_of(&keep), id_of(&dup));
    let customer = k.insert(&admin, "Customer", obj(json!({"customer_name": "Globex", "main_contact": dup_id.to_string()}))).await.unwrap();

    // Found by their primary email and by name.
    let groups = k.duplicates(&admin, "Contact", 50).await.unwrap();
    let found: Vec<&str> = groups.iter().filter_map(|g| g["field"].as_str()).collect();
    assert!(found.contains(&"email") && found.contains(&"full_name"), "{groups:?}");

    // The duplicate's other email comes along as a chosen value; the primary stays the kept one's.
    let mut lines = keep["emails"].as_array().unwrap().clone();
    lines.push(json!({"email": "m.sen@home.test"}));
    let out = k.merge_records_with(&admin, "Contact", keep_id, dup_id, "the same person", Some((obj(json!({"emails": lines})), None))).await.expect("merged");
    assert_eq!(out["links_moved"].as_u64(), Some(1), "{out}");

    let now = k.get(&admin, "Contact", keep_id, false).await.unwrap();
    assert_eq!(emails(&now), vec!["m.sen@home.test", "mira@globex.test"]);
    assert_eq!(now["email"], "mira@globex.test", "the primary is the kept one's");
    assert_eq!(now["phones"].as_array().unwrap().len(), 1, "the kept contact's own lines stay");
    assert_eq!(k.get(&admin, "Customer", id_of(&customer), false).await.unwrap()["main_contact"], json!(keep_id.to_string()), "a link moves");
    assert_eq!(k.get(&admin, "Contact", dup_id, false).await.unwrap()["id"], json!(keep_id.to_string()), "the old id answers with what it became");
    assert!(k.duplicates(&admin, "Contact", 50).await.unwrap().is_empty(), "no longer a duplicate");

    // Without chosen values, the duplicate's own lines stay with it (an alias, out of lists): nothing is added.
    let a = k.insert(&admin, "Contact", obj(json!({"first_name": "Ola", "phones": [{"phone": "+44 20 7946 0001"}]}))).await.unwrap();
    let b = k.insert(&admin, "Contact", obj(json!({"first_name": "Ola", "phones": [{"phone": "+44 20 7946 0002"}]}))).await.unwrap();
    k.merge_records(&admin, "Contact", id_of(&a), id_of(&b), "same").await.unwrap();
    let a = k.get(&admin, "Contact", id_of(&a), false).await.unwrap();
    assert_eq!(a["phones"].as_array().unwrap().len(), 1);
}
