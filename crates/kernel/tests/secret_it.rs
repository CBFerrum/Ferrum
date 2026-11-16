//! Secret fields against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`), the vault opened as a server opens it: kept in the vault
//! apart from the record, never shown, logged or published, read by their owner's handle alone, refused with the vault
//! closed, and gone from the vault with the value.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{Scope, Vault};
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

/// core plus a small package with a secret field.
fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-secret-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("vault/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("vault/package.yaml"), "package: vault\nversion: 0.1.0\nlabel: Vault\ndepends: [core]\n").unwrap();
    std::fs::write(
        dir.join("vault/entities/vault.yaml"),
        "- name: Integration\n  label: Integration\n  kind: master\n  title_field: title\n  fields:\n    - {name: title, type: data, required: true}\n    - {name: api_key, type: secret}\n",
    )
    .unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

/// A server on the test database, its vault opened (`open`) or not, and the platform's handle on it (whose the
/// package's field is: it is the platform's own).
async fn kernel(open: bool, reset: bool) -> (Arc<Kernel>, Vault) {
    let db = Db::connect(DbConfig { url: url(), max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
            .execute(&db.pool)
            .await
            .expect("reset");
    }
    if open {
        let opener = ferrum_kernel::vault::Opener {
            database_url: url(),
            file: "/nonexistent/root.key".into(),
            kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
            ..Default::default()
        };
        ferrum_kernel::vault::open(&db, opener, false).await.expect("the vault");
    }
    let mut hooks = HookRegistry::default();
    let platform = hooks.platform_vault();
    (Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot"), platform)
}

/// The field's value, as its owner reads it.
async fn reveal(k: &Kernel, platform: &Vault, tenant: Uuid, id: Uuid) -> ferrum_kernel::Result<Option<String>> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    let v = platform.field(k, &mut tx, tenant, "Integration", id, "api_key").await;
    tx.commit().await?;
    v.map(|v| v.map(|s| s.to_string()))
}

async fn count(k: &Kernel, tenant: Uuid, sql: &str) -> i64 {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// How many secret fields' values the vault keeps in `tenant`'s workspace.
async fn in_the_vault(k: &Kernel, tenant: Uuid) -> i64 {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let n: i64 =
        sqlx::query_scalar("select count(*) from vault.secret where scope = $1 and name like 'field.%'").bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[tokio::test]
async fn secrets_are_write_only_kept_in_the_vault_and_bound() {
    let (k, platform) = kernel(true, true).await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "vault".into(),
            name: "Vault Co".into(),
            admin_email: "admin@vault.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "vault".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let s = k.login("vault", "admin@vault.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();

    // Set on create: the document only says it is set.
    let created = k.insert(&ctx, "Integration", obj(json!({"title": "Mail", "api_key": "s3cret-value"}))).await.unwrap();
    assert_eq!(created["api_key"], json!(true));
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k.get(&ctx, "Integration", id, false).await.unwrap()["api_key"], json!(true));
    assert_eq!(reveal(&k, &platform, tenant, id).await.unwrap().as_deref(), Some("s3cret-value"));
    assert_eq!(in_the_vault(&k, tenant).await, 1);
    let listed = k.list(&ctx, "Integration", &ListQuery::default()).await.unwrap();
    assert!(!listed.rows[0].get("api_key").is_some_and(|v| v.is_string()), "lists never carry the value");
    assert!(k.insert(&ctx, "Integration", obj(json!({"title": "Bad", "api_key": 7}))).await.is_err());

    // Echoing the marker back changes nothing; text replaces; null clears.
    k.update(&ctx, "Integration", id, obj(json!({"title": "Mail (EU)", "api_key": true})), None).await.unwrap();
    assert_eq!(reveal(&k, &platform, tenant, id).await.unwrap().as_deref(), Some("s3cret-value"));
    k.update(&ctx, "Integration", id, obj(json!({"title": "Mail (EU)"})), None).await.unwrap();
    assert_eq!(reveal(&k, &platform, tenant, id).await.unwrap().as_deref(), Some("s3cret-value"), "absent means unchanged");
    k.update(&ctx, "Integration", id, obj(json!({"api_key": "rotated-value"})), None).await.unwrap();
    assert_eq!(reveal(&k, &platform, tenant, id).await.unwrap().as_deref(), Some("rotated-value"));

    // Never stored in the clear, nor in the audit trail or published events.
    let leaks = "select (select count(*) from record_secret where tenant_id = $1 and (sealed like '%s3cret%' or sealed like '%rotated%')) \
                 + (select count(*) from outbox where tenant_id = $1 and (payload::text like '%s3cret%' or payload::text like '%rotated%'))";
    assert_eq!(count(&k, tenant, leaks).await, 0);
    let trail = k.audit_trail(&ctx, "Integration", id).await.unwrap().to_string();
    assert!(!trail.contains("s3cret") && !trail.contains("rotated"), "{trail}");
    assert!(trail.contains("\"secrets\""), "the trail records that a secret changed");

    // Copies (duplicate/amend) never carry secrets.
    let copy = ferrum_kernel::doc::copy_for_new(ctx.schema.entity("Integration").unwrap(), &ctx.schema, &k.get(&ctx, "Integration", id, false).await.unwrap());
    assert!(copy.get("api_key").is_none());

    // Read by its owner's handle alone, on any server whose vault opens, also after the keys keeping it rotate.
    let (other, other_platform) = kernel(true, false).await;
    assert!(matches!(reveal(&other, &platform, tenant, id).await, Err(Error::Forbidden(_))), "a handle another server gave out");
    k.rotate_owner_key(Scope::Workspace(tenant), "platform").await.unwrap();
    assert_eq!(reveal(&other, &other_platform, tenant, id).await.unwrap().as_deref(), Some("rotated-value"), "another server, after a rotation");
    // With the vault closed: neither read nor set.
    let (closed, closed_platform) = kernel(false, false).await;
    assert!(matches!(reveal(&closed, &closed_platform, tenant, id).await, Err(Error::Unavailable(_))));
    assert!(matches!(closed.insert(&ctx, "Integration", obj(json!({"title": "No vault", "api_key": "x"}))).await, Err(Error::Unavailable(_))));

    // Clearing and deleting remove the value, from the vault too.
    k.update(&ctx, "Integration", id, obj(json!({"api_key": null})), None).await.unwrap();
    assert_eq!(k.get(&ctx, "Integration", id, false).await.unwrap()["api_key"], Value::Null);
    assert_eq!(reveal(&k, &platform, tenant, id).await.unwrap(), None);
    assert_eq!(in_the_vault(&k, tenant).await, 0);
    k.update(&ctx, "Integration", id, obj(json!({"api_key": "again"})), None).await.unwrap();
    k.delete(&ctx, "Integration", id, None).await.unwrap();
    assert_eq!(count(&k, tenant, "select count(*) from record_secret where tenant_id = $1").await, 0);
    assert_eq!(in_the_vault(&k, tenant).await, 0);
}
