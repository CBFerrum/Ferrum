//! A rule that does not show some fields (docs/builder/decisions.md BD-40) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). A client reaches a shared project through a rule
//! hiding its internal notes and its team; the same person reaching another project through a rule that shows
//! them reads them there. Staff whose rule hides a field lose it everywhere; staff holding a rule that shows it
//! keep it. Every surface a field reaches a person by is tried.

use ferrum_kernel::query::Filter;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Project
  label: Project
  kind: master
  title_field: name
  search_fields: [notes]
  fields:
    - {name: name, type: data, required: true}
    - {name: code, type: data, unique: true}
    - {name: customer_email, type: email}
    - {name: sponsor_email, type: email}
    - {name: stage, type: data}
    - {name: notes, type: data}
    - {name: team, type: table, of: ProjectMember}
- name: ProjectMember
  label: Project Member
  kind: child
  fields:
    - {name: person, type: data}
"#;

const PACKAGE: &str = "package: work\nversion: 0.1.0\nlabel: Work\ndepends: [core]\nroles:\n  - name: Work Client\n    portal: true\n    permissions:\n      - {entity: Project, read: true, via: customer_email, hide: [notes]}\n";

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-rule-hide-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("work/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("work/entities/work.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn rule(v: Value) -> PermissionDef {
    serde_json::from_value(v).expect("a rule")
}

async fn person(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str], portal: bool) -> Ctx {
    let user = k.create_user(admin, email, email, None, &[], false).await.unwrap();
    if portal {
        k.set_user_kind(admin, user, "portal").await.unwrap();
    }
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    k.set_user_roles_until(admin, user, &roles, &Default::default()).await.expect("roles given");
    k.load_ctx(admin.tenant, user, None, "it".into()).await.unwrap()
}

async fn exec(k: &Kernel, tenant: Uuid, sql: &'static str) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query(sql).bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

async fn hide_of(k: &Kernel, tenant: Uuid, role: &str) -> Vec<String> {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query_scalar("select hide from role_permission where tenant_id = $1 and role = $2").bind(tenant).bind(role).fetch_one(&mut *tx).await.unwrap()
}

fn has_notes(d: &Doc) -> bool {
    d.contains_key("notes")
}

#[test]
fn a_rule_hides_fields_from_its_holders_only() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "hide".into(),
            name: "hide".into(),
            admin_email: "admin@hide.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["work".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("hide", "admin@hide.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();

    // Checked when saved.
    for (bad, field, why) in [
        (json!({"entity": "Project", "read": true, "hide": ["nothing"]}), "hide", "no field"),
        (json!({"entity": "Project", "read": true, "hide": ["name"]}), "hide", "names or keys"),
        (json!({"entity": "Project", "read": true, "hide": ["code"]}), "hide", "names or keys"),
        (json!({"entity": "Project", "read": true, "via": "customer_email", "hide": ["customer_email"]}), "hide", "reaches"),
        (json!({"entity": "Project", "read": true, "where": {"stage": "Open"}, "hide": ["stage"]}), "hide", "reaches"),
        (json!({"entity": "*", "read": true, "hide": ["notes"]}), "hide", "every entity"),
    ] {
        let err = k.set_role_permission(&admin, "Bad", &rule(bad.clone())).await.expect_err("refused");
        assert!(err.to_string().contains(why) && format!("{err:?}").contains(field), "{bad}: {err}");
    }

    // Two portal roles: a client's rule hides the notes and the team; a sponsor's shows them.
    k.set_role_permission(
        &admin,
        "Client",
        &rule(json!({"entity": "Project", "read": true, "write": true, "via": "customer_email", "hide": ["notes", "team"]})),
    )
    .await
    .unwrap();
    k.set_role_portal(&admin, "Client", true).await.unwrap();
    k.set_role_permission(&admin, "Sponsor", &rule(json!({"entity": "Project", "read": true, "via": "sponsor_email"}))).await.unwrap();
    k.set_role_portal(&admin, "Sponsor", true).await.unwrap();
    // Kept as given, for the role editor's listing to give back.
    assert_eq!(hide_of(&k, tenant, "Client").await, vec!["notes".to_string(), "team".to_string()]);
    // Staff: one role reads everything but the notes, another everything.
    k.set_role_permission(&admin, "Junior", &rule(json!({"entity": "Project", "read": true, "write": true, "hide": ["notes"]}))).await.unwrap();
    k.set_role_permission(&admin, "Senior", &rule(json!({"entity": "Project", "read": true, "write": true}))).await.unwrap();

    let p = |name: &str, customer: &str, sponsor: &str, notes: &str| {
        obj(json!({"name": name, "customer_email": customer, "sponsor_email": sponsor, "stage": "Open", "notes": notes, "team": [{"person": "Asha"}]}))
    };
    let only_client = k.insert(&admin, "Project", p("Only client", "c@x.test", "", "internal-alpha")).await.unwrap();
    let only_sponsor = k.insert(&admin, "Project", p("Only sponsor", "", "c@x.test", "internal-beta")).await.unwrap();
    let both = k.insert(&admin, "Project", p("Both", "c@x.test", "c@x.test", "internal-gamma")).await.unwrap();

    let c = person(&k, &admin, "c@x.test", &["Client", "Sponsor"], true).await;
    // Read: hidden only where every rule reaching the record hides it.
    let got = k.get(&c, "Project", id_of(&only_client), false).await.unwrap();
    assert!(!has_notes(&got) && !got.contains_key("team"), "{got:?}");
    assert_eq!(got["stage"], "Open");
    assert!(has_notes(&k.get(&c, "Project", id_of(&only_sponsor), false).await.unwrap()));
    assert!(has_notes(&k.get(&c, "Project", id_of(&both), false).await.unwrap()));
    // List, and fields= asking for it.
    for fields in [None, Some(vec!["name".to_string(), "notes".to_string()])] {
        let rows = k.list(&c, "Project", &ListQuery { fields, limit: Some(50), ..Default::default() }).await.unwrap().rows;
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert_eq!(has_notes(r), r["name"] != "Only client", "{r:?}");
        }
    }
    // Filter and sort on it are refused as on a field they cannot read; search does not look in it.
    let filter = ListQuery { filters: vec![Filter::eq("notes", "internal-alpha")], ..Default::default() };
    assert!(matches!(k.list(&c, "Project", &filter).await, Err(Error::Forbidden(_))));
    let sort = ListQuery { sort: Some("notes".into()), ..Default::default() };
    assert!(matches!(k.list(&c, "Project", &sort).await, Err(Error::Forbidden(_))));
    let search = ListQuery { search: Some("internal-alpha".into()), ..Default::default() };
    assert!(k.list(&c, "Project", &search).await.unwrap().rows.is_empty());
    // A record page rendered as the person.
    let page = k.render_print(&c, "Project", id_of(&only_client)).await.unwrap();
    assert!(!page.contains("internal-alpha") && !page.contains("Asha"), "{page}");
    // A write naming it is refused, whatever the value; one leaving it alone goes through.
    for v in [json!("changed"), json!("internal-alpha")] {
        let err = k.update(&c, "Project", id_of(&only_client), obj(json!({"notes": v})), None).await.expect_err("refused");
        assert!(format!("{err:?}").contains("notes"), "{err:?}");
    }
    let err = k.update(&c, "Project", id_of(&only_client), obj(json!({"team": []})), None).await.expect_err("refused");
    assert!(format!("{err:?}").contains("team"), "{err:?}");
    k.update(&c, "Project", id_of(&only_client), obj(json!({"stage": "Review"})), None).await.unwrap();
    let kept = k.get(&admin, "Project", id_of(&only_client), false).await.unwrap();
    assert_eq!(kept["notes"], "internal-alpha");
    assert_eq!(kept["team"].as_array().unwrap().len(), 1);

    // Staff whose only rule hides it: nowhere, history included.
    let junior = person(&k, &admin, "junior@hide.test", &["Junior"], false).await;
    assert!(!has_notes(&k.get(&junior, "Project", id_of(&both), false).await.unwrap()));
    assert!(matches!(k.list(&junior, "Project", &filter).await, Err(Error::Forbidden(_))));
    k.update(&admin, "Project", id_of(&both), obj(json!({"notes": "internal-delta"})), None).await.unwrap();
    let trail = k.audit_trail(&junior, "Project", id_of(&both)).await.unwrap().to_string();
    assert!(!trail.contains("internal-delta") && !trail.contains("internal-gamma") && trail.contains("hidden"), "{trail}");
    assert!(k.update(&junior, "Project", id_of(&both), obj(json!({"notes": "x"})), None).await.is_err());
    // Staff also holding a rule that shows it: everywhere.
    let senior = person(&k, &admin, "senior@hide.test", &["Junior", "Senior"], false).await;
    assert!(has_notes(&k.get(&senior, "Project", id_of(&both), false).await.unwrap()));
    assert_eq!(k.list(&senior, "Project", &filter).await.unwrap().rows.len(), 1);
    let trail = k.audit_trail(&senior, "Project", id_of(&both)).await.unwrap().to_string();
    assert!(trail.contains("internal-delta"), "{trail}");
    k.update(&senior, "Project", id_of(&both), obj(json!({"notes": "by senior"})), None).await.unwrap();

    // A package declares it; a later release reaches a workspace installed earlier, never over an administrator's edit.
    assert_eq!(hide_of(&k, tenant, "Work Client").await, vec!["notes".to_string()]);
    // Written by the package: a release changing it rewrites it (here the stored row stands for the older release).
    exec(&k, tenant, "update role_permission set hide = '{team}' where tenant_id = $1 and role = 'Work Client'").await;
    k.install_packages(tenant, &["work".into()]).await.unwrap();
    assert_eq!(hide_of(&k, tenant, "Work Client").await, vec!["notes".to_string()]);
    // Written before this release (whose it is unknown): only an empty `hide` follows the package.
    exec(&k, tenant, "update role_permission set hide = '{}', packaged = null where tenant_id = $1 and role = 'Work Client'").await;
    k.install_packages(tenant, &["work".into()]).await.unwrap();
    assert_eq!(hide_of(&k, tenant, "Work Client").await, vec!["notes".to_string()]);
    // An administrator's edit is theirs.
    k.set_role_permission(&admin, "Work Client", &rule(json!({"entity": "Project", "read": true, "via": "customer_email", "hide": ["team"]}))).await.unwrap();
    k.install_packages(tenant, &["work".into()]).await.unwrap();
    assert_eq!(hide_of(&k, tenant, "Work Client").await, vec!["team".to_string()]);
}
