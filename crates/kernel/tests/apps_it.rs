//! Apps made available at runtime against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): packages join the catalog with their
//! tables, workspaces install and remove them, revocation takes them out without losing data.

use ferrum_kernel::apps::NewRelease;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

async fn kernel(reset: bool) -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    }
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let mut hooks = HookRegistry::default();
    hooks.register("FleetVehicle", Arc::new(Peek));
    Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot")
}

/// A read-only action: answers from the record, and tries to write when asked to.
struct Peek;

#[async_trait::async_trait]
impl ferrum_kernel::DocHook for Peek {
    async fn action(&self, hc: &mut ferrum_kernel::HookCtx<'_>, name: &str, doc: &ferrum_kernel::Doc, params: &Value) -> ferrum_kernel::Result<Option<Value>> {
        if name != "peek" {
            return Ok(None);
        }
        if params.get("write").is_some() {
            sqlx::query("create temp table fleet_peek_write (x int)").execute(&mut **hc.tx).await?;
        }
        Ok(Some(json!({"plate": doc.get("plate")})))
    }

    async fn validate_change(
        &self,
        _hc: &mut ferrum_kernel::HookCtx<'_>,
        old: Option<&ferrum_kernel::Doc>,
        doc: &ferrum_kernel::Doc,
    ) -> ferrum_kernel::Result<()> {
        if let Some(old) = old
            && old.get("plate") == Some(&json!("FROZEN-1"))
            && doc.get("plate") != old.get("plate")
        {
            return Err(Error::field("plate", "a frozen plate cannot change"));
        }
        Ok(())
    }
}

fn release(version: &str, extra_field: bool) -> NewRelease {
    let mut files = BTreeMap::new();
    // From 1.1 the app also adds a child table to core's Customer (a package overlay).
    let overlay = if extra_field {
        "field_prefix: fleet_\noverlays:\n  - entity: Customer\n    ops:\n      - {op: add_field, field: {name: fleet_drivers, type: table, of: FleetDriver}}\n"
    } else {
        ""
    };
    files.insert(
        "package.yaml".to_string(),
        format!(
            "package: fleet\nversion: '1'\nlabel: Fleet\ndepends: [core]\n{overlay}roles:\n  - name: Fleet Manager\n    permissions:\n      - {{entity: FleetVehicle, read: true, write: true, create: true, delete: true}}\n"
        ),
    );
    let extra = if extra_field { "    - {name: odometer, type: int}\n    - {name: services, type: table, of: FleetService}\n" } else { "" };
    let child = if extra_field {
        "- name: FleetService\n  label: Service\n  kind: child\n  fields:\n    - {name: note, type: data}\n    - {name: active, type: check, default: true}\n\
         - name: FleetTrip\n  label: Trip\n  kind: document\n  submittable: true\n  fields:\n    - {name: purpose, type: data, required: true}\n    - {name: ticket, type: data, unique: true}\n\
         - name: FleetDriver\n  label: Driver\n  kind: child\n  fields:\n    - {name: driver_name, type: data, required: true}\n"
    } else {
        ""
    };
    files.insert(
        "entities/fleet.yaml".to_string(),
        format!("{child}- name: FleetVehicle\n  label: Vehicle\n  kind: master\n  title_field: plate\n  actions:\n    - {{name: peek, label: Peek, permission: read, read_only: true}}\n  fields:\n    - {{name: plate, type: data, required: true}}\n{extra}"),
    );
    NewRelease {
        app_id: "fleet".into(),
        version: version.into(),
        runtime: "meta".into(),
        manifest: json!({"id": "fleet", "version": version}),
        digest: format!("digest-{version}-{extra_field}"),
        signatures: None,
        source: "upload".into(),
        bundle_key: None,
        packages: BTreeMap::from([("fleet".to_string(), files)]),
        plugin_sha256: None,
        private_tenant: None,
        created_by: None,
    }
}

#[tokio::test]
async fn apps_join_the_catalog_at_runtime() {
    let k = kernel(true).await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "fleetco".into(),
            name: "Fleet Co".into(),
            admin_email: "admin@fleet.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("fleetco", "admin@fleet.test", "Correct-Horse-9").await.unwrap();
    let ctx = || async { k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap() };

    // Unknown before; available (with a table) after, without a restart.
    assert!(k.registry.catalog().package("fleet").is_none());
    let r = k.make_available(release("1.0.0", false)).await.unwrap();
    assert_eq!(r.package_names(), vec!["fleet".to_string()]);
    assert!(k.registry.catalog().package("fleet").is_some());

    // Another replica booting now sees it too.
    let replica = kernel(false).await;
    assert!(replica.registry.catalog().package("fleet").is_some());

    // A workspace installs it and uses it.
    k.install_packages(tenant, &["fleet".into()]).await.unwrap();
    let c = ctx().await;
    let v = k.insert(&c, "FleetVehicle", obj(json!({"plate": "KA-01-1234"}))).await.unwrap();
    assert_eq!(v["plate"], json!("KA-01-1234"));

    // Same version, different content: refused. A new version adds a column, keeps the data.
    assert!(matches!(k.make_available(release("1.0.0", true)).await, Err(Error::Conflict(_))));
    k.make_available(release("1.1.0", true)).await.unwrap();
    let c = ctx().await;
    let id: uuid::Uuid = v["id"].as_str().unwrap().parse().unwrap();
    let upd = k.update(&c, "FleetVehicle", id, obj(json!({"odometer": 1200})), None).await.unwrap();
    assert_eq!(upd["odometer"], json!(1200));
    // Child rows added while editing get their defaults too.
    let upd = k.update(&c, "FleetVehicle", id, obj(json!({"services": [{"note": "oil change"}]})), None).await.unwrap();
    assert_eq!(upd["services"][0]["active"], json!(true));
    // validate_change sees the stored version: new records pass, edits are checked against it.
    let frozen = k.insert(&c, "FleetVehicle", obj(json!({"plate": "FROZEN-1"}))).await.unwrap();
    let frozen_id: uuid::Uuid = frozen["id"].as_str().unwrap().parse().unwrap();
    k.update(&c, "FleetVehicle", frozen_id, obj(json!({"odometer": 5})), None).await.expect("other fields may change");
    assert!(matches!(k.update(&c, "FleetVehicle", frozen_id, obj(json!({"plate": "NEW-2"})), None).await, Err(Error::Validation(_))));
    k.delete(&c, "FleetVehicle", frozen_id, None).await.unwrap();
    // A read-only action answers without locking or auditing, and cannot write.
    assert_eq!(k.run_action(&c, "FleetVehicle", id, "peek", json!({})).await.unwrap(), json!({"plate": upd["plate"]}));
    assert!(k.run_action(&c, "FleetVehicle", id, "peek", json!({"write": true})).await.is_err(), "a read-only transaction refuses writes");
    let trail = k.audit_trail(&c, "FleetVehicle", id).await.unwrap();
    assert!(!trail["events"].as_array().unwrap().iter().any(|e| e["action"] == json!("action:peek")), "no audit entry for a read");
    // A locked read that had to wait for another writer sees that writer's child rows.
    let mut writer = k.db.tenant_tx(tenant).await.unwrap();
    k.update_tx(&c, &mut writer, "FleetVehicle", id, obj(json!({"services": [{"note": "tyres"}]})), None).await.unwrap();
    let (k2, c2) = (k.clone(), c.clone());
    let waiter = tokio::spawn(async move {
        let mut tx = k2.db.tenant_tx(tenant).await.unwrap();
        let seen = k2.get_tx(&c2, &mut tx, "FleetVehicle", id, true).await.unwrap();
        tx.commit().await.unwrap();
        seen
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // the waiter is blocked on the row lock
    writer.commit().await.unwrap();
    let seen = waiter.await.unwrap();
    assert_eq!(seen["services"].as_array().unwrap().len(), 1);
    assert_eq!(seen["services"][0]["note"], json!("tyres"), "not the child rows from before the wait");
    // Created and submitted in one transaction; an idempotent replay returns the same document.
    let trip = k.insert_submit(&c, "FleetTrip", obj(json!({"purpose": "delivery"}))).await.unwrap();
    assert_eq!(trip["docstatus"], json!(1));
    assert!(k.insert_submit(&c, "FleetTrip", obj(json!({}))).await.is_err(), "a failing document leaves nothing behind");
    let (first, replayed) = k.insert_idempotent_with(&c, "FleetTrip", obj(json!({"purpose": "pickup"})), "trip-key-0001", "hash", true).await.unwrap();
    let (again, replayed_again) = k.insert_idempotent_with(&c, "FleetTrip", obj(json!({"purpose": "pickup"})), "trip-key-0001", "hash", true).await.unwrap();
    assert_eq!((first["docstatus"].clone(), replayed, replayed_again), (json!(1), false, true));
    assert_eq!(first["id"], again["id"]);
    assert_eq!(k.list(&c, "FleetTrip", &ListQuery::default()).await.unwrap().rows.len(), 2);
    // Unique values ignore cancelled documents (they are void), so a cancelled trip's ticket is free again.
    let t1 = k.insert_submit(&c, "FleetTrip", obj(json!({"purpose": "audit", "ticket": "T-1"}))).await.unwrap();
    assert!(k.insert(&c, "FleetTrip", obj(json!({"purpose": "audit", "ticket": "T-1"}))).await.is_err(), "unique while submitted");
    k.cancel(&c, "FleetTrip", t1["id"].as_str().unwrap().parse().unwrap(), None).await.unwrap();
    k.insert(&c, "FleetTrip", obj(json!({"purpose": "audit again", "ticket": "T-1"}))).await.expect("free once cancelled");
    // The child table the app added to Customer works like any other.
    let cust = k.insert(&c, "Customer", obj(json!({"customer_name": "Hauliers", "fleet_drivers": [{"driver_name": "Ravi"}]}))).await.unwrap();
    let cid: uuid::Uuid = cust["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k.get(&c, "Customer", cid, false).await.unwrap()["fleet_drivers"][0]["driver_name"], json!("Ravi"));
    let history = k.release_history("fleet").await.unwrap();
    assert_eq!(history.iter().map(|r| (r.version.as_str(), r.status.as_str())).collect::<Vec<_>>(), vec![("1.1.0", "active"), ("1.0.0", "superseded")]);

    // Private releases cannot bring tables.
    let mut private = release("2.0.0", true);
    private.private_tenant = Some(tenant);
    assert!(k.make_available(private).await.is_err());

    // Revocation takes the app out everywhere; the workspace keeps working without it, data kept.
    assert!(k.revoke_release("fleet", "1.1.0", "security issue").await.unwrap());
    assert!(k.registry.catalog().package("fleet").is_none());
    let c = ctx().await;
    assert!(c.schema.entity("FleetVehicle").is_none());
    assert!(c.schema.warnings.iter().any(|w| w.message.contains("no longer available")));
    assert!(k.list(&c, "Customer", &ListQuery::default()).await.is_ok(), "the rest of the workspace is unaffected");
    let mut tx = k.db.system_tx().await.unwrap();
    let kept: i64 = sqlx::query_scalar("select count(*) from e_fleet_vehicle").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept, 1, "revocation never deletes data");

    // A fixed release brings it back with the data; uninstalling with delete_data removes the rows.
    k.make_available(release("1.2.0", true)).await.unwrap();
    let c = ctx().await;
    assert_eq!(k.list(&c, "FleetVehicle", &ListQuery::default()).await.unwrap().rows.len(), 1);
    assert!(c.schema.roles.iter().any(|r| r.name == "Fleet Manager"));
    let blocked = k.uninstall_packages(&c, &["core".into()], false).await;
    assert!(blocked.is_err(), "core is needed by fleet");
    k.uninstall_packages(&c, &["fleet".into()], true).await.unwrap();
    let c = ctx().await;
    assert!(c.schema.entity("FleetVehicle").is_none());
    assert!(!c.schema.packages.contains(&"fleet".to_string()));
    let mut tx = k.db.system_tx().await.unwrap();
    let left: i64 = sqlx::query_scalar("select count(*) from e_fleet_vehicle").fetch_one(&mut *tx).await.unwrap();
    let roles: i64 = sqlx::query_scalar("select count(*) from role where package = 'fleet'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((left, roles), (0, 0));
}
