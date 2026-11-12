//! Calls an app makes in its own transaction instead of writing the kernel's tables (docs/builder/decisions.md
//! BD-43), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first): tagging a record,
//! a working calendar made once by name, and files going out with a record. Each asks what its route asks.

use async_trait::async_trait;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_logic::calendar::Calendar;
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const PACKAGE: &str = "package: desk\nversion: 0.1.0\nlabel: Desk\ndepends: [core]\n";
const ENTITIES: &str =
    "- {name: Ticket, label: Ticket, kind: master, title_field: subject, fields: [{name: subject, type: data, required: true}, {name: email, type: email}]}\n";

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-app-calls-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("desk/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("desk/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("desk/entities/desk.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[derive(Default)]
struct MemFiles(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

#[async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn rule(v: Value) -> PermissionDef {
    serde_json::from_value(v).unwrap()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

async fn person(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str], portal: bool) -> Ctx {
    let user = k.create_user(admin, email, email, None, &[], false).await.unwrap();
    if portal {
        k.set_user_kind(admin, user, "portal").await.unwrap();
    }
    k.set_user_roles_until(admin, user, &strings(roles), &Default::default()).await.unwrap();
    k.load_ctx(admin.tenant, user, None, "it".into()).await.unwrap()
}

#[test]
fn an_apps_calls_ask_what_their_routes_ask() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let tenant = k
        .create_tenant(NewTenant {
            slug: "desk".into(),
            name: "desk".into(),
            admin_email: "admin@desk.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["desk".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("desk", "admin@desk.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let system = Ctx::system(tenant, admin.schema.clone());
    k.set_role_permission(&admin, "Agent", &rule(json!({"entity": "Ticket", "read": true, "write": true, "create": true}))).await.unwrap();
    k.set_role_permission(&admin, "Viewer", &rule(json!({"entity": "Ticket", "read": true}))).await.unwrap();
    k.set_role_permission(&admin, "Customer", &rule(json!({"entity": "Ticket", "read": true, "via": "email", "shared": "add"}))).await.unwrap();
    k.set_role_portal(&admin, "Customer", true).await.unwrap();
    let agent = person(&k, &admin, "agent@desk.test", &["Agent"], false).await;
    let viewer = person(&k, &admin, "viewer@desk.test", &["Viewer"], false).await;
    let customer = person(&k, &admin, "c@home.test", &["Customer"], true).await;
    let t = id_of(&k.insert(&agent, "Ticket", obj(json!({"subject": "Printer", "email": "c@home.test"}))).await.unwrap());
    let other = id_of(&k.insert(&agent, "Ticket", obj(json!({"subject": "Other"}))).await.unwrap());

    // Tags: added once whatever their case, removed, held to the record's limit; as the tags route asks.
    let tag = |ctx: &Ctx, add: &[&str], remove: &[&str]| {
        let (k, ctx, add, remove) = (&k, ctx.clone(), strings(add), strings(remove));
        async move {
            let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
            let r = k.tag_tx(&ctx, &mut tx, "Ticket", t, &add, &remove).await;
            tx.commit().await.unwrap();
            r
        }
    };
    assert_eq!(tag(&system, &["urgent", "Printer"], &[]).await.unwrap(), strings(&["Printer", "urgent"]));
    assert_eq!(tag(&agent, &["URGENT", "billing"], &["printer"]).await.unwrap(), strings(&["billing", "urgent"]));
    assert!(matches!(tag(&viewer, &["x"], &[]).await, Err(Error::Forbidden(_))), "reading is not tagging");
    assert!(matches!(tag(&customer, &["x"], &[]).await, Err(Error::Forbidden(_))), "tags are the members'");
    let many: Vec<String> = (0..20).map(|i| format!("t{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(tag(&system, &many, &[]).await.is_err(), "past the limit");
    assert_eq!(k.tags(&agent, "Ticket", t).await.unwrap(), strings(&["billing", "urgent"]), "a refusal adds nothing");

    // A working calendar made once by name; one already there is left as it is.
    let cal = |zone: &str| -> Calendar {
        serde_json::from_value(json!({"name": "desk_hours", "zone": zone, "weekends": ["sat", "sun"], "holidays": [], "revision": 0})).unwrap()
    };
    let ensure = |ctx: &Ctx, c: Calendar| {
        let (k, ctx) = (&k, ctx.clone());
        async move {
            let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
            let r = k.ensure_calendar_tx(&ctx, &mut tx, &c).await;
            tx.commit().await.unwrap();
            r
        }
    };
    assert!(matches!(ensure(&agent, cal("UTC")).await, Err(Error::Forbidden(_))), "calendars are the administrators'");
    assert!(ensure(&system, cal("Asia/Kolkata")).await.unwrap());
    assert!(!ensure(&system, cal("UTC")).await.unwrap());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let kept = ferrum_kernel::load_calendar(&mut tx, tenant, "desk_hours").await.unwrap().unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept.zone, "Asia/Kolkata");

    // Files going out with a ticket: the caller's own loose uploads are attached; ones already on it pass.
    let loose = |ctx: &Ctx, name: &str| {
        let (k, ctx, name) = (&k, ctx.clone(), name.to_string());
        async move { Uuid::parse_str(k.store_file(&ctx, None, &name, "text/plain", b"a log").await.unwrap()["id"].as_str().unwrap()).unwrap() }
    };
    let send = |ctx: &Ctx, ids: Vec<Uuid>, record: Uuid| {
        let (k, ctx) = (&k, ctx.clone());
        async move {
            let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
            let r = k.attach_files_tx(&ctx, &mut tx, &ids, "Ticket", record).await;
            tx.commit().await.unwrap();
            r
        }
    };
    let mine = loose(&agent, "log.txt").await;
    let out = send(&agent, vec![mine], t).await.unwrap();
    assert_eq!((out[0]["file_name"].clone(), out[0]["size"].clone()), (json!("log.txt"), json!(5)));
    assert!(k.attachments(&agent, "Ticket", t).await.unwrap().iter().any(|f| f["id"] == json!(mine)));
    assert_eq!(send(&agent, vec![mine], t).await.unwrap().len(), 1, "already on it");
    // Another's upload, a file of another record, one not there: refused, and nothing of the batch attached.
    let theirs = loose(&viewer, "theirs.txt").await;
    let fresh = loose(&agent, "fresh.txt").await;
    for bad in [theirs, Uuid::now_v7()] {
        assert!(send(&agent, vec![fresh, bad], t).await.is_err());
    }
    assert!(send(&agent, vec![mine], other).await.is_err(), "a file of another record");
    assert!(!k.attachments(&agent, "Ticket", t).await.unwrap().iter().any(|f| f["id"] == json!(fresh)), "nothing attached on a refusal");
    // Reading is not attaching; an outside person whose rule lets them add attaches their own, shared.
    let viewed = loose(&viewer, "v.txt").await;
    assert!(matches!(send(&viewer, vec![viewed], t).await, Err(Error::Forbidden(_))));
    let hers = loose(&customer, "hers.txt").await;
    send(&customer, vec![hers], t).await.unwrap();
    let listed = k.attachments(&agent, "Ticket", t).await.unwrap();
    assert!(listed.iter().any(|f| f["id"] == json!(hers) && f["shared"] == json!(true)), "{listed:?}");
}
