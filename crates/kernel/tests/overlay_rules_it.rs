//! Role rules and personal-data declarations over fields a package adds to another package's entity, against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). The test package adds a table of crew and a
//! private mark to core's Company: its role reaches the companies whose crew names the person and that are not
//! private, and its declaration erases the contact address it added. A package that does not require it cannot name
//! those fields.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, Package};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

const CREW: &str = r#"package: crew
version: 0.1.0
depends: [core]
field_prefix: crew_
roles:
  - name: Crewman
    permissions:
      - {entity: Company, read: true, via: crew_members.email, where: {crew_private: false}}
personal_data:
  - {entity: Company, match: crew_contact, erase: [crew_contact]}
overlays:
  - entity: Company
    ops:
      - {op: add_field, field: {name: crew_members, label: Crew, type: table, of: CrewMember}}
      - {op: add_field, field: {name: crew_private, label: Private, type: check}}
      - {op: add_field, field: {name: crew_contact, label: Contact, type: email}}
"#;
const MEMBER: &str = "- name: CrewMember\n  label: Crew Member\n  kind: child\n  fields:\n    - {name: email, type: email}\n";
const OTHER: &str = r#"package: other
version: 0.1.0
depends: [core]
roles:
  - name: Snoop
    permissions:
      - {entity: Company, read: true, via: crew_members.email}
"#;

fn package(manifest: &str, entities: Option<&str>) -> Package {
    let mut files: BTreeMap<String, String> = BTreeMap::from([("package.yaml".to_string(), manifest.to_string())]);
    if let Some(e) = entities {
        files.insert("entities/crew.yaml".into(), e.into());
    }
    ferrum_meta::catalog::load_package_files(&files).expect("package")
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

#[test]
fn rules_over_fields_a_package_adds() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let crew = package(CREW, Some(MEMBER));
    // A package that does not require crew does not see its fields.
    let e = Catalog::from_packages(vec![core.clone(), crew.clone(), package(OTHER, None)]).unwrap_err().to_string();
    assert!(e.contains("other: role Snoop on Company: Company has no field crew_members"), "{e}");
    let catalog = Catalog::from_packages(vec![core, crew]).expect("crew's rules and declarations name its own fields");

    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "crew".into(),
            name: "crew".into(),
            admin_email: "admin@crew.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["crew".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("crew", "admin@crew.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let usd = k.find_tx(&admin, &mut tx, "Currency", &[ferrum_kernel::query::Filter::eq("code", "USD")], None, 1).await.unwrap()[0]["id"].clone();
    drop(tx);
    let company = |name: &str, members: &[&str], private: bool| {
        obj(
            json!({"company_name": name, "abbr": &name[..3], "default_currency": usd, "crew_private": private, "crew_contact": format!("{}@contact.test", name.to_lowercase()),
            "crew_members": members.iter().map(|m| json!({"email": m})).collect::<Vec<_>>()}),
        )
    };
    let mut ids = Vec::new();
    for (name, members, private) in [("Alpha", vec!["bob@crew.test"], false), ("Bravo", vec!["eve@crew.test"], false), ("Charlie", vec!["bob@crew.test"], true)]
    {
        let d = k.insert(&admin, "Company", company(name, &members, private)).await.expect("a company");
        ids.push(d["id"].as_str().unwrap().parse::<Uuid>().unwrap());
    }
    let bob = k.create_user(&admin, "bob@crew.test", "Bob", Some("Correct-Horse-9"), &["Crewman".into()], false).await.unwrap();
    let bob = k.load_ctx(tenant, bob, None, "it".into()).await.unwrap();
    let seen = k.list(&bob, "Company", &Default::default()).await.expect("listed");
    let names: Vec<&str> = seen.rows.iter().filter_map(|d| d["company_name"].as_str()).collect();
    assert_eq!(names, ["Alpha"], "the crew he is on, not another's, not a private one");
    assert!(k.get(&bob, "Company", ids[1], false).await.is_err(), "Bravo is not his");

    // The address the package added is found and erased for its person.
    let found = k.personal_data(&admin, "alpha@contact.test").await.expect("found");
    assert!(found.to_string().contains(&ids[0].to_string()), "{found}");
    k.erase_personal_data(&admin, "alpha@contact.test", "asked").await.expect("erased");
    let alpha = k.get(&admin, "Company", ids[0], false).await.unwrap();
    assert!(alpha["crew_contact"].as_str().is_none_or(|c| !c.contains("alpha@contact.test")), "{alpha:?}");
    assert_eq!(k.get(&admin, "Company", ids[1], false).await.unwrap()["crew_contact"], "bravo@contact.test", "and nobody else's");
}
