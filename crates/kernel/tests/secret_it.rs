//! Secret fields against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): sealed apart from the record, never
//! shown, logged or published, bound to their record and key.

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
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

async fn kernel(key: Option<SecretKey>, reset: bool) -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    }
    Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings { secret_key: key, ..Default::default() }, None).await.expect("boot")
}

async fn reveal(k: &Kernel, tenant: Uuid, id: Uuid) -> ferrum_kernel::Result<Option<String>> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    let v = k.secret(&mut tx, tenant, "Integration", id, "api_key").await;
    tx.commit().await?;
    v
}

async fn count(k: &Kernel, tenant: Uuid, sql: &str) -> i64 {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[tokio::test]
async fn secrets_are_write_only_sealed_and_bound() {
    let key = SecretKey::new([42u8; 32]);
    let k = kernel(Some(key.clone()), true).await;
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
    assert_eq!(reveal(&k, tenant, id).await.unwrap().as_deref(), Some("s3cret-value"));
    let listed = k.list(&ctx, "Integration", &ListQuery::default()).await.unwrap();
    assert!(!listed.rows[0].get("api_key").is_some_and(|v| v.is_string()), "lists never carry the value");
    assert!(k.insert(&ctx, "Integration", obj(json!({"title": "Bad", "api_key": 7}))).await.is_err());

    // Echoing the marker back changes nothing; text replaces; null clears.
    k.update(&ctx, "Integration", id, obj(json!({"title": "Mail (EU)", "api_key": true})), None).await.unwrap();
    assert_eq!(reveal(&k, tenant, id).await.unwrap().as_deref(), Some("s3cret-value"));
    k.update(&ctx, "Integration", id, obj(json!({"title": "Mail (EU)"})), None).await.unwrap();
    assert_eq!(reveal(&k, tenant, id).await.unwrap().as_deref(), Some("s3cret-value"), "absent means unchanged");
    k.update(&ctx, "Integration", id, obj(json!({"api_key": "rotated-value"})), None).await.unwrap();
    assert_eq!(reveal(&k, tenant, id).await.unwrap().as_deref(), Some("rotated-value"));

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

    // Bound to the key: another key cannot open it.
    let other = kernel(Some(SecretKey::new([7u8; 32])), false).await;
    assert!(matches!(reveal(&other, tenant, id).await, Err(Error::Unavailable(_))));
    let keyless = kernel(None, false).await;
    assert!(matches!(keyless.insert(&ctx, "Integration", obj(json!({"title": "No key", "api_key": "x"}))).await, Err(Error::Unavailable(_))));

    // Clearing and deleting remove the sealed value.
    k.update(&ctx, "Integration", id, obj(json!({"api_key": null})), None).await.unwrap();
    assert_eq!(k.get(&ctx, "Integration", id, false).await.unwrap()["api_key"], Value::Null);
    assert_eq!(reveal(&k, tenant, id).await.unwrap(), None);
    k.update(&ctx, "Integration", id, obj(json!({"api_key": "again"})), None).await.unwrap();
    k.delete(&ctx, "Integration", id, None).await.unwrap();
    assert_eq!(count(&k, tenant, "select count(*) from record_secret where tenant_id = $1").await, 0);
}
