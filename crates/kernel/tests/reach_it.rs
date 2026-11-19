//! The fields that decide who reaches a record (`ferrum_kernel::reach::fields`, S9 §2.4), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first).

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::Path;

const PACKAGE: &str = r#"package: rk
version: 0.1.0
label: Reach fields
depends: [core]
approvals:
  - {entity: RkCase, on: submit, policy: {steps: [{label: Review, approvers: {field: reviewer}}]}}
roles:
  - name: Desk
    permissions:
      - {entity: RkCase, read: true, where: {territory: north}}
      - {entity: RkCase, read: true, via: account.contact_email}
      - {entity: RkNote, read: true, via: case.watcher}
"#;

const ENTITIES: &str = r#"
- name: RkAccount
  label: Account
  kind: master
  fields:
    - {name: account_name, type: data}
    - {name: contact_email, type: email}
- name: RkRegion
  label: Region
  kind: master
  fields:
    - {name: region_name, type: data}
- name: RkPartner
  label: Partner
  kind: master
  fields:
    - {name: partner_name, type: data}
- name: RkCase
  label: Case
  kind: document
  submittable: true
  owner_fields: [owner_email]
  reach_links: [partner]
  fields:
    - {name: owner_email, type: email}
    - {name: territory, type: data}
    - {name: account, type: link, to: RkAccount}
    - {name: company, type: link, to: Company}
    - {name: region, type: link, to: RkRegion}
    - {name: reviewer, type: email}
    - {name: watcher, type: email}
    - {name: partner, type: link, to: RkPartner}
    - {name: note, type: data}
- name: RkNote
  label: Note
  kind: master
  fields:
    - {name: case, type: link, to: RkCase}
    - {name: text, type: data}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-reach-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("rk/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("rk/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("rk/entities/rk.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[test]
fn fields_cover_owner_where_via_scope_approval_routing_and_declared_links() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "rk".into(),
            name: "Reach".into(),
            admin_email: "admin@rk.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["rk".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("rk", "admin@rk.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    let schema = k.registry.schema(tenant).await.unwrap();
    let case = schema.entity("RkCase").unwrap();

    let fields = |region_scoped: bool| {
        let (k, admin, case) = (k.clone(), admin.clone(), case.clone());
        async move {
            let mut tx = k.db.system_tenant_tx(admin.tenant).await.unwrap();
            if region_scoped {
                sqlx::query("insert into user_scope (tenant_id, user_id, entity, value) values ($1, $2, 'RkRegion', gen_random_uuid())")
                    .bind(admin.tenant)
                    .bind(admin.user)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            let f = ferrum_kernel::reach::fields(&admin, &mut tx, &case).await.unwrap();
            tx.commit().await.unwrap();
            f
        }
    };
    let before = fields(false).await;
    for want in ["owner_email", "territory", "account", "company", "reviewer", "watcher", "partner"] {
        assert!(before.contains(want), "{want} decides who reaches a case: {before:?}");
    }
    assert!(!before.contains("note") && !before.contains("region"), "{before:?}");
    // Once people are scoped by region, the region link decides too.
    assert!(fields(true).await.contains("region"));
    // The note's link to its case is crossed by a rule's path: it decides who reaches notes.
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    let notes = ferrum_kernel::reach::fields(&admin, &mut tx, schema.entity("RkNote").unwrap()).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(notes.into_iter().collect::<Vec<_>>(), ["case"]);
}
