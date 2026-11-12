//! A package's rule rows follow that package's releases and no other's (docs/builder/decisions.md BD-40, corrected),
//! against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). Two packages declare a rule for
//! the same role and entity with different grants: in either install order the first row stays and the second package
//! changes nothing of it, again on every later install; the first package's own corrected rule still reaches the
//! workspace; an administrator's edit survives both; a row of unknown origin is claimed by neither.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, PermissionDef};
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

const NOTES: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\n";
const NOTE: &str = "- name: Note\n  label: Note\n  kind: master\n  fields:\n    - {name: title, type: data}\n    - {name: body, type: data}\n";
const ALPHA: &str = "package: alpha\nversion: 0.1.0\ndepends: [notes]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Note, read: true, create: true, hide: [body]}\n";
const BETA: &str =
    "package: beta\nversion: 0.1.0\ndepends: [notes]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Note, read: true, write: true}\n";

fn package(files: &[(&str, &str)]) -> ferrum_meta::Package {
    let files: BTreeMap<String, String> = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    ferrum_meta::catalog::load_package_files(&files).expect("package")
}

/// The Clerk's rule on Note: `(read, write, create, hide, packaged, package)`.
type Row = (bool, bool, bool, Vec<String>, Option<bool>, Option<String>);

async fn row(k: &Kernel, tenant: Uuid) -> Row {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let r = sqlx::query_as(
        "select can_read, can_write, can_create, hide, packaged, package from role_permission where tenant_id = $1 and role = 'Clerk' and entity = 'Note'",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    r
}

async fn exec(k: &Kernel, tenant: Uuid, sql: &'static str) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query(sql).bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

async fn workspace(k: &Kernel, slug: &str, first: &str) -> (Uuid, Ctx) {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![first.into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    (tenant, k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap())
}

#[test]
fn a_rule_follows_its_own_package() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let catalog = Catalog::from_packages(vec![
        core,
        package(&[("package.yaml", NOTES), ("entities/note.yaml", NOTE)]),
        package(&[("package.yaml", ALPHA)]),
        package(&[("package.yaml", BETA)]),
    ])
    .unwrap();
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let alpha_row: Row = (true, false, true, vec!["body".into()], Some(true), Some("alpha".into()));
    let beta_row: Row = (true, true, false, vec![], Some(true), Some("beta".into()));

    // Alpha first: its row stays when beta comes, and on every install after.
    let grants = |r: Row| (r.0, r.1, r.2, r.3);
    let (one, admin) = workspace(&k, "one", "alpha").await;
    k.install_packages(one, &["beta".into()]).await.unwrap();
    assert_eq!(grants(row(&k, one).await), grants(alpha_row.clone()), "beta's rule does not replace alpha's");
    assert_eq!(row(&k, one).await, alpha_row, "and the row says alpha wrote it");
    k.install_packages(one, &["alpha".into(), "beta".into()]).await.unwrap();
    k.install_packages(one, &["beta".into()]).await.unwrap();
    assert_eq!(row(&k, one).await, alpha_row, "nor on a later install, in either order");
    // Beta first: the same the other way round.
    let (two, _) = workspace(&k, "two", "beta").await;
    k.install_packages(two, &["alpha".into()]).await.unwrap();
    k.install_packages(two, &["beta".into(), "alpha".into()]).await.unwrap();
    assert_eq!(grants(row(&k, two).await), grants(beta_row.clone()), "alpha's rule does not replace beta's");
    assert_eq!(row(&k, two).await, beta_row);

    // Alpha's own release still reaches its row (the stored row stands for an older release of alpha).
    exec(&k, one, "update role_permission set can_create = false, can_delete = true, hide = '{}' where tenant_id = $1 and role = 'Clerk'").await;
    k.install_packages(one, &["beta".into()]).await.unwrap();
    assert!(!row(&k, one).await.2, "another package's install does not correct it");
    k.install_packages(one, &["alpha".into()]).await.unwrap();
    assert_eq!(row(&k, one).await, alpha_row, "its own does: a withdrawn grant and a new hide");

    // Of unknown origin (written packaged without a package): no package claims it; an empty `hide` is taken.
    exec(&k, one, "update role_permission set package = null, can_write = false, can_create = false, hide = '{}' where tenant_id = $1 and role = 'Clerk'")
        .await;
    k.install_packages(one, &["beta".into(), "alpha".into()]).await.unwrap();
    assert_eq!(row(&k, one).await, (true, false, false, vec!["body".into()], Some(true), None));

    // An administrator's edit is theirs, through both packages' installs.
    let rule: PermissionDef = serde_json::from_value(serde_json::json!({"entity": "Note", "read": true, "delete": true})).unwrap();
    k.set_role_permission(&admin, "Clerk", &rule).await.unwrap();
    k.install_packages(one, &["alpha".into(), "beta".into()]).await.unwrap();
    let edited = row(&k, one).await;
    assert_eq!((edited.0, edited.1, edited.2, edited.3, edited.4), (true, false, false, vec![], Some(false)), "the administrator's");
}
