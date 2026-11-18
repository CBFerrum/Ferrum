//! Workspaces with the same packages and no customizations share what is built for them (K6, registry.rs), against a real
//! PostgreSQL (`FERRUM_REGISTRY_SHARE_TEST_DATABASE_URL`; the database is reset first): the entities of their schemas are
//! the same allocations, while each has its own version and settings; a customization makes one's own at once, and its
//! removal shares again; other packages do not share.

use ferrum_kernel::KernelSettings;
use ferrum_meta::OverlayOp;
use ferrum_testkit::{Kit, database, run};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

fn package(name: &str, entity: &str) -> ferrum_meta::Package {
    let package = format!("package: {name}\nversion: 0.1.0\ndepends: [core]\n");
    let entity = format!("name: {entity}\nlabel: {entity}\nkind: master\ntitle_field: title\nfields:\n  - {{name: title, type: data}}\n");
    let files: BTreeMap<String, String> = [("package.yaml".to_string(), package), (format!("entities/{}.yaml", name), entity)].into_iter().collect();
    ferrum_meta::catalog::load_package_files(&files).expect("a package")
}

#[test]
fn identical_workspaces_share() {
    run(async {
        let kit = Kit::boot_with(
            &database("FERRUM_REGISTRY_SHARE_TEST_DATABASE_URL"),
            &[],
            vec![package("alpha", "Lead"), package("beta", "Deal")],
            KernelSettings::default(),
        )
        .await;
        let registry = kit.k.registry.clone();
        let one = kit.workspace("one", &["alpha"], Value::Null).await;
        let two = kit.workspace("two", &["alpha"], json!({"timezone": "Asia/Kolkata", "fiscal_year_start_month": 4})).await;
        let other = kit.workspace("other", &["beta"], Value::Null).await;
        let (s1, s2, s3) =
            (registry.schema(one.tenant).await.unwrap(), registry.schema(two.tenant).await.unwrap(), registry.schema(other.tenant).await.unwrap());
        let shared = |a: &ferrum_meta::Schema, b: &ferrum_meta::Schema, entity: &str| Arc::ptr_eq(&a.entities[entity], &b.entities[entity]);
        // Same packages, no customizations: the entities are one allocation, whatever the package; the rest is each one's.
        assert!(shared(&s1, &s2, "Lead") && s1.entities.keys().all(|e| shared(&s1, &s2, e)), "one allocation per entity");
        assert!(!Arc::ptr_eq(&s1, &s2));
        assert_eq!((s1.timezone.as_str(), s2.timezone.as_str(), s1.fy_start_month, s2.fy_start_month), ("UTC", "Asia/Kolkata", 1, 4));
        assert_ne!(s1.version, 0);
        // Other packages: nothing shared, not even what both depend on.
        let core = s1.entities.iter().find(|(_, e)| e.package == "core").map(|(n, _)| n.clone()).expect("an entity of core");
        assert!(!shared(&s1, &s3, &core), "a different list of packages is its own key");
        // A custom field on one: its own at the next look, and the other is the same schema it was.
        let op: OverlayOp = serde_json::from_value(json!({"op": "add_field", "field": {"name": "x_note", "type": "data"}})).unwrap();
        let id = one.k.add_customization(&one.admin, "Lead", op, None).await.unwrap();
        let (c1, c2) = (registry.schema(one.tenant).await.unwrap(), registry.schema(two.tenant).await.unwrap());
        assert!(c1.entities["Lead"].field("x_note").is_some() && c2.entities["Lead"].field("x_note").is_none());
        assert!(!shared(&c1, &c2, "Lead") && !shared(&c1, &c2, &core), "a workspace with a customization builds its own");
        assert!(Arc::ptr_eq(&s2, &c2), "the other is untouched");
        // Taken out, they share again.
        one.k.set_customization_active(&one.admin, id, false).await.unwrap();
        let back = registry.schema(one.tenant).await.unwrap();
        assert!(back.entities["Lead"].field("x_note").is_none() && back.entities.keys().all(|e| shared(&back, &c2, e)), "shared again");
        assert!(back.version > s1.version, "and its own version moved on");
    });
}
