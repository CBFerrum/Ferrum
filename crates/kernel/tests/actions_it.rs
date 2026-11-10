//! Actions an administrator sets up by configuration (docs/api.md, "Configured actions"), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first), and two faults found rebuilding
//! Governance and Risk as a pack (docs/builder/evidence/grc-as-a-pack-2026-11-10.md): a record's whole
//! numbers are signed in expressions, and `read_only_if` holds on the server.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-actions-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shop/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shop/package.yaml"), "package: shop\nversion: 0.1.0\nlabel: Shop\ndepends: [core]\n").unwrap();
    std::fs::write(
        dir.join("shop/entities/shop.yaml"),
        "- name: Shop\n  label: Shop\n  kind: master\n  fields:\n    - {name: name, type: data}\n    - {name: status, type: data, read_only: true}\n",
    )
    .unwrap();
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

fn define(entity: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity["name"], "op": {"op": "define_entity", "entity": entity}})).unwrap()
}

fn check(entity: &str, name: &str, check: &str, message: &str) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": {"op": "add_rule", "rule": {"name": name, "check": check, "message": message}}})).unwrap()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

struct W {
    k: Arc<Kernel>,
    tenant: Uuid,
    user: Uuid,
}

impl W {
    async fn admin(&self) -> Ctx {
        self.k.load_ctx(self.tenant, self.user, None, "it".into()).await.unwrap()
    }
    async fn batch(&self, items: Vec<NewCustomization>) -> Result<usize, String> {
        self.k.batch_customizations(&self.admin().await, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin().await, entity, id, false).await.unwrap()
    }
    async fn count(&self, sql: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }
}

async fn workspace(k: &Arc<Kernel>, slug: &str) -> W {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shop".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user }
}

#[test]
fn actions_set_up_by_configuration() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let w = workspace(&k, "acme").await;
    faults(&w).await;
}

/// Whole numbers from a record are signed in checks, computed fields and automations; `read_only_if` ignores
/// input as `read_only` does, judged on the record as stored.
async fn faults(w: &W) {
    let k = &w.k;
    let thing = json!({"name": "XThing", "label": "Thing", "kind": "master", "fields": [
        {"name": "days", "type": "int"},
        {"name": "hours", "type": "int", "computed": "doc.days * 24"},
        {"name": "owed", "type": "int"},
        {"name": "twice", "type": "int"},
        {"name": "locked", "type": "check"},
        {"name": "note", "type": "data", "read_only_if": "doc.locked"}]});
    w.batch(vec![define(thing), check("XThing", "short", "doc.days * 24 <= 2400 && doc.owed > -100", "at most a hundred days, owing under a hundred")])
        .await
        .unwrap();
    let admin = w.admin().await;
    automation::save(
        k,
        &admin,
        &obj(json!({"name": "Twice", "entity": "XThing", "event": "created", "condition": "doc.days * 2 > 0",
            "actions": [{"type": "set_fields", "values": {"twice": "doc.days * 2"}}]})),
    )
    .await
    .unwrap();
    // A multiplication, a comparison and a negative number.
    let t = k.insert(&admin, "XThing", obj(json!({"days": 90, "owed": -5, "note": "a"}))).await.expect("90 days, owing -5");
    assert_eq!(t["hours"], json!(2160), "doc.days * 24 in a computed field");
    let refused = k.insert(&admin, "XThing", obj(json!({"days": 101, "owed": 0}))).await.unwrap_err();
    assert!(message(&refused).contains("at most a hundred days"), "{refused:?}");
    assert!(k.insert(&admin, "XThing", obj(json!({"days": 1, "owed": -100}))).await.is_err(), "a negative bound");
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!(w.get("XThing", id_of(&t)).await["twice"], json!(180), "doc.days * 2 in an automation");

    // `read_only_if`: the field ignores input while the stored record says so, as a read-only field does.
    let id = id_of(&t);
    k.update(&admin, "XThing", id, obj(json!({"note": "b"})), None).await.unwrap();
    assert_eq!(w.get("XThing", id).await["note"], "b", "not locked: changed");
    k.update(&admin, "XThing", id, obj(json!({"note": "c", "locked": true})), None).await.unwrap();
    assert_eq!(w.get("XThing", id).await["note"], "c", "judged on the record as stored: not locked yet");
    k.update(&admin, "XThing", id, obj(json!({"note": "d"})), None).await.unwrap();
    assert_eq!(w.get("XThing", id).await["note"], "c", "locked: the input is ignored, as for a read-only field");
    let fresh = k.insert(&admin, "XThing", obj(json!({"days": 1, "locked": true, "note": "new"}))).await.unwrap();
    assert_eq!(fresh["note"], "new", "a new record has nothing stored to be read-only by");
    let _ = w.count("select count(*) from custom_record where tenant_id = $1").await;
}
