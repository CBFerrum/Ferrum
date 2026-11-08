//! FLC-CAP-001 to 003 and FLC-PROC-002: the business capability catalog and its gate, against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). Two workspaces, and in the
//! first an administrator, a clerk, a viewer and an approver.

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

const PACKAGE: &str = "package: stockcap\nversion: 0.3.1\ndepends: [core]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Hold, read: true, write: true, create: true, submit: true, read_groups: [costing], write_groups: [costing]}\n      - {entity: StockLine, read: true}\n  - name: Viewer\n    permissions:\n      - {entity: Hold, read: true}\n  - name: Approver\n    permissions:\n      - {entity: Hold, read: true, write: true}\nworkflows:\n  - name: Hold approval\n    entity: Hold\n    initial_state: Draft\n    states:\n      - {name: Draft}\n      - {name: Approved}\n    transitions:\n      - {action: approve, from: Draft, to: Approved, roles: [Approver], not_creator: true}\n";
const ENTITIES: &str = r#"
- name: Hold
  label: Hold
  kind: document
  submittable: true
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: qty, type: int}
    - {name: posted, type: int, read_only: true}
    - {name: note, type: data}
    - {name: cost, type: int, perm_group: costing}
  actions:
    - {name: place, label: Place, permission: write, contract: {completion: The stock is held, compensate_with: release}}
    - {name: release, label: Release, permission: write}
    - {name: peek, label: Peek, read_only: true, permission: read}
    - {name: fix_up, label: Fix up, hidden: true}
    - {name: restock, label: Restock, permission: write}
- name: StockLine
  label: Stock Line
  kind: ledger
  table: stockcap_line
  fields:
    - {name: qty, type: int}
"#;
/// The engine table a ledger is posted to: the app's own SQL, as a real ledger's is.
const LEDGER_SQL: &str = "create table stockcap_line (tenant_id uuid not null references tenant (id), id uuid not null, qty int, created_at timestamptz not null default now(), primary key (tenant_id, id));";

/// The owning app: its actions, and how often each really ran.
#[derive(Default)]
struct Stock {
    placed: AtomicUsize,
    /// While set, `restock` writes and then fails: a crash between the change and the commit.
    failing: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl DocHook for Stock {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        Ok(match name {
            "place" => {
                self.placed.fetch_add(1, Ordering::SeqCst);
                Some(json!({"held": doc["qty"]}))
            }
            "release" => Some(json!({"released": true})),
            "peek" => Some(json!({"qty": doc["qty"]})),
            "restock" => {
                let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
                sqlx::query("update e_hold set note = 'restocked' where id = $1").bind(id).execute(&mut **hc.tx).await?;
                if self.failing.load(Ordering::SeqCst) {
                    return Err(Error::Unavailable("the stock service went away mid-way".into()));
                }
                Some(json!({"restocked": true}))
            }
            _ => None,
        })
    }
}

fn code(e: &Error) -> &'static str {
    e.code()
}

fn ids(caps: &[ferrum_logic::capability::Capability]) -> Vec<&str> {
    caps.iter().map(|c| c.id.as_str()).collect()
}

async fn count(k: &Kernel, ctx: &Ctx, sql: &'static str) -> i64 {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[test]
fn the_gate_runs_what_the_catalog_promises() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn workspace(k: &Kernel, slug: &str) -> (Ctx, Ctx) {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["stockcap".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, &format!("clerk@{slug}.test"), "Clerk", Some("Correct-Horse-9"), &["Clerk".to_string()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    (admin, clerk)
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> = [("package.yaml", PACKAGE), ("entities/stock.yaml", ENTITIES), ("sql/001_ledger.sql", LEDGER_SQL)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let stock = Arc::new(Stock::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Hold", stock.clone());
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let (admin, clerk) = workspace(&k, "acme").await;
    let viewer = k.create_user(&admin, "viewer@acme.test", "Viewer", Some("Correct-Horse-9"), &["Viewer".to_string()], false).await.unwrap();
    let viewer = k.load_ctx(admin.tenant, viewer, None, "it".into()).await.unwrap();
    let approver = k.create_user(&admin, "approver@acme.test", "Approver", Some("Correct-Horse-9"), &["Approver".to_string()], false).await.unwrap();
    let approver = k.load_ctx(admin.tenant, approver, None, "it".into()).await.unwrap();

    // What each may use, read from what is installed and their rights; nothing typed in twice.
    let mine = k.catalog(&clerk);
    let theirs = k.catalog(&viewer);
    for id in [
        "stockcap.hold.create",
        "stockcap.hold.update",
        "stockcap.hold.submit",
        "stockcap.hold.place",
        "stockcap.hold.peek",
        "stockcap.hold.read",
        "stockcap.hold.list",
    ] {
        assert!(ids(&mine).contains(&id), "the clerk has {id}: {:?}", ids(&mine));
    }
    for id in ["stockcap.hold.delete", "stockcap.hold.fix_up", "ferrum.mail", "stockcap.hold.transition.approve"] {
        assert!(!ids(&mine).contains(&id), "the clerk does not have {id}");
    }
    assert!(!ids(&mine).iter().any(|id| id.starts_with("stockcap.stock_line.")), "a ledger offers no capability, not even to read");
    assert_eq!(
        ids(&theirs).into_iter().filter(|id| id.starts_with("stockcap.")).collect::<Vec<_>>(),
        ["stockcap.hold.list", "stockcap.hold.peek", "stockcap.hold.read"],
        "a viewer only reads"
    );
    let place = k.capability(&clerk, "stockcap.hold.place").unwrap();
    assert_eq!((place.version.as_str(), place.compensate_with.as_deref()), ("0.3.1", Some("stockcap.hold.release")));
    assert_eq!(place.completion, "The stock is held");
    assert_eq!(place.digest, place.compute_digest().unwrap(), "what is shown is what was sealed");
    assert_eq!(k.capability(&admin, "stockcap.hold.place").unwrap().digest, place.digest, "one capability, one digest, whoever looks");
    let events: Vec<String> = k.events(&clerk).iter().map(|e| e["id"].as_str().unwrap().to_string()).collect();
    assert!(events.contains(&"stockcap.hold.submitted".to_string()) && events.contains(&"stockcap.hold.transitioned".to_string()), "{events:?}");

    // A change needs a key; the same key and input is one change, however often it is asked for.
    let create =
        |reference: &str, qty: i64| Invocation::new("stockcap.hold.create", json!({"values": {"reference": reference, "qty": qty, "posted": 99, "cost": 40}}));
    let refused = k.invoke(&clerk, create("A", 5)).await.unwrap_err();
    assert_eq!(code(&refused), "schema_invalid", "{refused}");
    let first = k.invoke(&clerk, create("A", 5).key("order-7/hold")).await.unwrap();
    assert!(!first.repeated);
    let hold: Uuid = first.result["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(first.result["posted"], Value::Null, "a read-only field is the app's to set, not the caller's");
    let again = k.invoke(&clerk, create("A", 5).key("order-7/hold")).await.unwrap();
    assert!(again.repeated, "the receipt answers");
    assert_eq!(again.result["id"], first.result["id"]);
    let changed = k.invoke(&clerk, create("A", 6).key("order-7/hold")).await.unwrap_err();
    assert_eq!(code(&changed), "input_changed", "{changed}");
    assert_eq!(count(&k, &clerk, "select count(*) from e_hold").await, 1, "one hold, not three");

    // Two at once with one key: one change, and the other gets its receipt.
    let (a, b) = tokio::join!(k.invoke(&clerk, create("B", 1).key("race")), k.invoke(&clerk, create("B", 1).key("race")));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.result["id"], b.result["id"]);
    assert!(a.repeated != b.repeated, "exactly one did it");
    assert_eq!(count(&k, &clerk, "select count(*) from e_hold where reference = 'B'").await, 1);

    // The owning app's action, through the gate: done once under its key.
    let act = || Invocation::new("stockcap.hold.place", json!({"id": hold.to_string(), "params": {}})).key("order-7/place");
    assert_eq!(k.invoke(&clerk, act()).await.unwrap().result, json!({"held": 5}));
    assert!(k.invoke(&clerk, act()).await.unwrap().repeated);
    assert_eq!(stock.placed.load(Ordering::SeqCst), 1, "the app's action ran once");
    // A failure after the change, before the commit: neither the change nor a receipt survives, and the
    // same key then does it once (FLC-PROC-002).
    let restock = || Invocation::new("stockcap.hold.restock", json!({"id": hold.to_string(), "params": {}})).key("order-7/restock");
    stock.failing.store(true, Ordering::SeqCst);
    assert_eq!(code(&k.invoke(&clerk, restock()).await.unwrap_err()), "unavailable");
    assert_eq!(count(&k, &clerk, "select count(*) from e_hold where note = 'restocked'").await, 0, "the change rolled back");
    assert_eq!(count(&k, &clerk, "select count(*) from logic_effect where effect_key = 'order-7/restock'").await, 0, "and so did its receipt");
    stock.failing.store(false, Ordering::SeqCst);
    assert!(!k.invoke(&clerk, restock()).await.unwrap().repeated, "the retry does it");
    assert!(k.invoke(&clerk, restock()).await.unwrap().repeated, "once");

    // A field in a group the viewer may not read stays hidden however it is asked for.
    let listed = k.invoke(&viewer, Invocation::new("stockcap.hold.list", json!({"filters": [["reference", "=", "A"]]}))).await.unwrap();
    assert_eq!(listed.result.as_array().map(Vec::len), Some(1));
    assert!(listed.result[0].get("cost").is_none_or(|c| c != &json!(40)), "listed: {}", listed.result);
    let read = k.invoke(&viewer, Invocation::new("stockcap.hold.read", json!({"id": hold.to_string()}))).await.unwrap();
    assert!(read.result.get("cost").is_none_or(|c| c != &json!(40)), "read: {}", read.result);
    let theirs = k.invoke(&clerk, Invocation::new("stockcap.hold.list", json!({"filters": [["reference", "=", "A"]]}))).await.unwrap();
    assert_eq!(theirs.result[0]["cost"], json!(40), "the clerk's role reads the group");
    let peek = k.invoke(&viewer, Invocation::new("stockcap.hold.peek", json!({"id": hold.to_string(), "params": {}}))).await.unwrap();
    assert_eq!(peek.result, json!({"qty": 5}), "a read needs no key");

    // Who is asking comes from their sign-in, never from the input.
    let forged = Invocation::new("stockcap.hold.create", json!({"values": {"reference": "C"}, "tenant_id": Uuid::now_v7().to_string()})).key("forged");
    assert_eq!(code(&k.invoke(&clerk, forged).await.unwrap_err()), "forbidden");
    let elevated =
        Invocation::new("stockcap.hold.update", json!({"id": hold.to_string(), "values": {"note": "x"}, "roles": ["System Manager"]})).key("elevated");
    assert_eq!(code(&k.invoke(&clerk, elevated).await.unwrap_err()), "forbidden");
    assert_eq!(code(&k.invoke(&viewer, create("V", 1).key("viewer")).await.unwrap_err()), "forbidden", "a viewer's rights, not the input's");
    assert_eq!(
        code(&k.invoke(&clerk, Invocation::new("stockcap.stock_line.create", json!({"values": {"qty": 9}})).key("ledger")).await.unwrap_err()),
        "capability_unavailable"
    );
    assert_eq!(code(&k.invoke(&clerk, Invocation::new("stockcap.hold.vanish", json!({}))).await.unwrap_err()), "capability_unavailable");

    // A process bound to one version does not silently run another.
    let stale = create("D", 1).key("stale").version("0.3.0");
    assert_eq!(code(&k.invoke(&clerk, stale).await.unwrap_err()), "capability_unavailable");
    assert!(k.invoke(&clerk, create("D", 1).key("current").version("0.3.1")).await.is_ok());

    // The workflow's transition: the role it names, and still not the record's creator.
    assert_eq!(
        code(&k.invoke(&clerk, Invocation::new("stockcap.hold.transition.approve", json!({"id": hold.to_string()})).key("self")).await.unwrap_err()),
        "forbidden"
    );
    let approved = k.invoke(&approver, Invocation::new("stockcap.hold.transition.approve", json!({"id": hold.to_string()})).key("approve-7")).await.unwrap();
    assert_eq!(approved.result["workflow_state"], json!("Approved"));

    // The platform's own: a notification once per key; mail is queued with the outbox, an administrator's.
    let notify = || Invocation::new("ferrum.notify", json!({"user": clerk.user.to_string(), "title": "Your hold is placed"})).key("notify-7");
    k.invoke(&admin, notify()).await.unwrap();
    assert!(k.invoke(&admin, notify()).await.unwrap().repeated);
    assert_eq!(count(&k, &clerk, "select count(*) from notification where title = 'Your hold is placed'").await, 1);
    let mail = || Invocation::new("ferrum.mail", json!({"to": "buyer@example.com", "subject": "Held", "body": "Your stock is held"})).key("mail-7");
    assert_eq!(code(&k.invoke(&clerk, mail()).await.unwrap_err()), "forbidden");
    k.invoke(&admin, mail()).await.unwrap();
    k.invoke(&admin, mail()).await.unwrap();
    let queued = count(&k, &admin, "select count(*) from outbox where topic = 'ferrum.mail' and payload->>'subject' = 'Held'").await;
    assert_eq!(queued, 1, "queued once, in the transaction of its receipt");

    // Another workspace: its own receipts and records, whatever keys it chooses.
    let (_, beta) = workspace(&k, "beta").await;
    let theirs = k.invoke(&beta, create("A", 5).key("order-7/hold")).await.unwrap();
    assert!(!theirs.repeated, "a key is a workspace's own");
    assert_ne!(theirs.result["id"], first.result["id"]);
    let across = Invocation::new("stockcap.hold.read", json!({"id": hold.to_string()}));
    assert_eq!(code(&k.invoke(&beta, across).await.unwrap_err()), "not_found", "another workspace's record does not exist here");
    assert_eq!(count(&k, &beta, "select count(*) from logic_effect").await, 1, "and none of its receipts are visible");
}
