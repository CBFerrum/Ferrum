//! Merging duplicates, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first): every link moves, whole or not at all; append-only history keeps the id it was posted
//! under and is still found; the old id answers with what it became.

use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: OrderLine
  label: Order Line
  kind: child
  fields:
    - {name: bill_to, type: link, to: Party}
    - {name: note, type: data}
- name: Party
  label: Party
  kind: master
  title_field: party_name
  mergeable: true
  duplicate_fields: [email, party_name]
  fields:
    - {name: party_name, type: data, required: true}
    - {name: email, type: email}
    - {name: parent, type: link, to: Party}
- name: Order
  label: Order
  kind: document
  submittable: true
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: party, type: link, to: Party}
    - {name: lines, type: table, of: OrderLine}
- name: Unique
  label: Unique
  kind: master
  title_field: code
  unique: [[party]]
  fields:
    - {name: code, type: data, required: true}
    - {name: party, type: link, to: Party}
"#;

const PACKAGE: &str = r#"package: cases
version: 0.1.0
label: Cases
depends: [core]
merge_links:
  - {table: party_balance, column: party_id, to: Party}
roles:
  - name: Clerk
    permissions:
      - {entity: Party, read: true, write: true, create: true}
      - {entity: Order, read: true, write: true, create: true}
  - name: Manager
    permissions:
      - {entity: Party, read: true, write: true, create: true, delete: true}
"#;

const ENGINE: &str = r#"
create table party_balance (tenant_id uuid not null, party_id uuid not null, amount numeric not null default 0, primary key (tenant_id, party_id, amount));
create table party_ledger (tenant_id uuid not null, seq bigint generated always as identity, party_id uuid not null, amount numeric not null,
  primary key (tenant_id, seq), foreign key (tenant_id, party_id) references e_party (tenant_id, id));
create function party_ledger_is_append_only() returns trigger language plpgsql as $$ begin raise exception 'the ledger is append-only'; end $$;
create trigger party_ledger_guard before update or delete on party_ledger for each row execute function party_ledger_is_append_only();
alter table party_balance enable row level security;
alter table party_balance force row level security;
create policy tenant_isolation on party_balance using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table party_ledger enable row level security;
alter table party_ledger force row level security;
create policy tenant_isolation on party_ledger using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
"#;

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-merge-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::fs::create_dir_all(dir.join("cases/sql")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
    std::fs::write(dir.join("cases/sql/001_engine.sql"), ENGINE).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

/// The tests reset one database: they take their turn.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

#[test]
fn a_customer_stays_one_customer() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["cases".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &["Clerk".to_string()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let manager = k.create_user(&admin, "manager@acme.test", "Manager", None, &["Manager".to_string()], false).await.unwrap();
    let manager = k.load_ctx(tenant, manager, None, "it".into()).await.unwrap();
    k.add_customization(
        &admin,
        "Order",
        serde_json::from_value::<OverlayOp>(json!({"op": "add_field", "field": {"name": "x_partner", "type": "link", "to": "Party"}})).unwrap(),
        None,
    )
    .await
    .unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();

    let party = |name: &str, email: &str| obj(json!({"party_name": name, "email": email}));
    let keep = id(&k.insert(&admin, "Party", party("Globex Ltd", "ap@globex.test")).await.unwrap());
    let dup = id(&k.insert(&admin, "Party", party("GLOBEX  LTD", "AP@globex.test")).await.unwrap());
    let other = id(&k.insert(&admin, "Party", party("Initech", "hi@initech.test")).await.unwrap());
    let child = id(&k.insert(&admin, "Party", obj(json!({"party_name": "Globex EU", "parent": dup}))).await.unwrap());

    // Found.
    let groups = k.duplicates(&manager, "Party", 50).await.unwrap();
    assert_eq!(groups.len(), 2, "by email and by name: {groups:?}");
    assert!(groups.iter().all(|g| g["ids"] == json!([keep, dup])), "{groups:?}");
    assert!(matches!(k.duplicates(&clerk, "Party", 50).await, Err(Error::Forbidden(_))));

    let order = k
        .insert(&admin, "Order", obj(json!({"reference": "SO-1", "party": dup, "x_partner": dup, "lines": [{"bill_to": dup, "note": "billed"}, {"bill_to": other, "note": "elsewhere"}]})))
        .await
        .unwrap();
    let order_id = id(&order);
    k.submit(&admin, "Order", order_id, None).await.expect("submitted");
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into party_balance (tenant_id, party_id, amount) values ($1, $2, 90)").bind(tenant).bind(dup).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into party_ledger (tenant_id, party_id, amount) values ($1, $2, 90), ($1, $3, 10)")
        .bind(tenant)
        .bind(dup)
        .bind(keep)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.add_comment(&admin, "Party", dup, "A note on the duplicate", &[]).await.expect("comment");
    k.set_tags(&admin, "Party", dup, &["vip".to_string(), "eu".to_string()]).await.unwrap();
    k.set_tags(&admin, "Party", keep, &["vip".to_string()]).await.unwrap();
    let file = k.store_file(&admin, Some(("Party", dup)), "contract.pdf", "application/pdf", b"signed").await.unwrap();
    let file: Uuid = file["id"].as_str().unwrap().parse().unwrap();

    // Who may, and what cannot be merged.
    // A record says what links to it, down to the rows of a child table, and whether it can be merged.
    let described = |ctx| ferrum_kernel::describe::describe(ctx, &k.entity(ctx, "Party").expect("Party"));
    let links = |ctx| {
        described(ctx)["linked_from"]
            .as_array()
            .expect("links")
            .iter()
            .map(|l| format!("{}.{}", l["entity"].as_str().unwrap(), l["field"].as_str().unwrap()))
            .collect::<Vec<_>>()
    };
    assert_eq!(described(&admin)["mergeable"], true);
    assert_eq!(links(&admin), ["Order.lines.bill_to", "Order.party", "Order.x_partner", "Party.parent", "Unique.party"], "a workspace's own fields too");
    assert_eq!(links(&manager), ["Party.parent"], "a list closed to them stays out of it");
    assert_eq!(ferrum_kernel::describe::describe(&admin, &k.entity(&admin, "Order").expect("Order"))["mergeable"], false);

    assert!(matches!(k.merge_records(&clerk, "Party", keep, dup, "same").await, Err(Error::Forbidden(_))), "needs delete");
    assert!(matches!(k.merge_records(&manager, "Party", keep, dup, " ").await, Err(Error::Validation(_))), "a reason");
    assert!(matches!(k.merge_records(&manager, "Party", keep, keep, "same").await, Err(Error::Invalid(_))));
    assert!(matches!(k.merge_records(&manager, "Order", order_id, order_id, "same").await, Err(Error::Invalid(_))), "an entity that did not opt in");
    assert!(matches!(k.merge_records(&manager, "Party", keep, Uuid::now_v7(), "same").await, Err(Error::NotFound(_))));
    let held = k.place_legal_hold(&admin, None, Some(("Party", dup)), "Litigation").await.unwrap();
    assert!(matches!(k.merge_records(&manager, "Party", keep, dup, "same").await, Err(Error::Conflict(_))), "a legal hold stops it");
    k.release_legal_hold(&admin, held).await.unwrap();

    // A collision: nothing moves at all.
    let u1 = id(&k.insert(&admin, "Unique", obj(json!({"code": "a", "party": keep}))).await.unwrap());
    let u2 = id(&k.insert(&admin, "Unique", obj(json!({"code": "b", "party": dup}))).await.unwrap());
    assert!(matches!(k.merge_records(&manager, "Party", keep, dup, "same").await, Err(Error::Conflict(_))), "both have one, and only one is allowed");
    assert_eq!(k.get(&admin, "Order", order_id, false).await.unwrap()["party"], json!(dup.to_string()), "whole or not at all");
    // The values chosen to stay are part of the merge: one that fails leaves none of them (FEO-006).
    let version = k.get(&admin, "Party", keep, false).await.unwrap()["version"].as_i64();
    let chosen = |version: Option<i64>| Some((obj(json!({"email": "billing@globex.test"})), version));
    assert!(matches!(k.merge_records_with(&manager, "Party", keep, dup, "same", chosen(version)).await, Err(Error::Conflict(_))));
    assert_eq!(k.get(&admin, "Party", keep, false).await.unwrap()["email"], json!("ap@globex.test"), "no chosen value is left behind");
    assert_eq!(k.get(&admin, "Party", dup, false).await.unwrap()["id"], json!(dup.to_string()));
    k.delete(&admin, "Unique", u2, None).await.unwrap();
    k.delete(&admin, "Unique", u1, None).await.unwrap();

    // Chosen against a version that has since moved on: nothing is merged, and nothing is written.
    let stale = version.map(|v| v - 1);
    assert!(
        matches!(k.merge_records_with(&manager, "Party", keep, dup, "same", chosen(stale)).await, Err(Error::Conflict(_))),
        "chosen against an older version"
    );
    assert_eq!(k.get(&admin, "Party", dup, false).await.unwrap()["id"], json!(dup.to_string()), "not merged");
    // Records at providers: the duplicate's alone in one stream, and both linked in another.
    let (crm, books) = (Uuid::now_v7(), Uuid::now_v7());
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    for (id, provider) in [(crm, "crm"), (books, "books")] {
        sqlx::query("insert into connection (tenant_id, id, provider, name) values ($1, $2, $3, $3)")
            .bind(tenant)
            .bind(id)
            .bind(provider)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for (connection, stream, external, record) in [(crm, "contacts", "crm-9", dup), (books, "customers", "bk-1", keep), (books, "customers", "bk-2", dup)] {
        sqlx::query("insert into connection_link (tenant_id, connection_id, stream, external_id, entity, record_id, local_version) values ($1, $2, $3, $4, 'Party', $5, 1)")
            .bind(tenant)
            .bind(connection)
            .bind(stream)
            .bind(external)
            .bind(record)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let done = k.merge_records_with(&manager, "Party", keep, dup, "Same company, entered twice", chosen(version)).await.expect("merged");
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    let links: Vec<(String, Uuid, Option<i32>)> =
        sqlx::query_as("select external_id, record_id, local_version from connection_link where tenant_id = $1 order by external_id")
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let drift: Vec<(String, String, Value)> =
        sqlx::query_as("select external_id, kind, detail from connection_drift where tenant_id = $1 and resolved_at is null")
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        links,
        [("bk-1".into(), keep, Some(1)), ("bk-2".into(), dup, Some(1)), ("crm-9".into(), keep, Some(-1))],
        "the provider's record now stands for the one that stays"
    );
    assert_eq!(drift.len(), 1, "{drift:?}");
    assert_eq!((drift[0].0.as_str(), drift[0].1.as_str(), &drift[0].2["record"]), ("bk-2", "conflict", &json!(keep)), "two there, one here: for a person");
    assert_eq!(k.get(&admin, "Party", keep, false).await.unwrap()["email"], json!("billing@globex.test"), "the chosen value, with the merge");
    assert!(done["links_moved"].as_u64().unwrap() >= 6, "{done}");

    let merged = k.get(&admin, "Order", order_id, false).await.unwrap();
    assert_eq!(merged["party"], json!(keep.to_string()), "a submitted document's link moves: it is identity, not content");
    assert_eq!(merged["x_partner"], json!(keep.to_string()), "custom fields too");
    assert_eq!(merged["lines"][0]["bill_to"], json!(keep.to_string()), "child rows too");
    assert_eq!(merged["lines"][1]["bill_to"], json!(other.to_string()), "others stay");
    assert_eq!(merged["version"], json!(2), "and the document did not change otherwise");
    assert_eq!(k.get(&admin, "Party", child, false).await.unwrap()["parent"], json!(keep.to_string()));

    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let balance: i64 = sqlx::query_scalar("select count(*) from party_balance where tenant_id = $1 and party_id = $2")
        .bind(tenant)
        .bind(keep)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(balance, 1, "declared engine tables move");
    let posted: Vec<Uuid> =
        sqlx::query_scalar("select party_id from party_ledger where tenant_id = $1 order by seq").bind(tenant).fetch_all(&mut *tx).await.unwrap();
    assert_eq!(posted, [dup, keep], "history keeps the id it was posted under");
    let ids = ferrum_kernel::merge::merged_ids(&mut tx, tenant, "Party", keep).await.unwrap();
    assert_eq!(ids, [keep, dup], "and is found");
    assert_eq!(ferrum_kernel::merge::merged_ids(&mut tx, tenant, "Party", dup).await.unwrap(), [keep, dup], "from either id");
    let total: rust_decimal::Decimal = sqlx::query_scalar("select sum(amount) from party_ledger where tenant_id = $1 and party_id = any($2)")
        .bind(tenant)
        .bind(&ids)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(total, rust_decimal::Decimal::from(100), "a statement for the one customer holds both");
    tx.commit().await.unwrap();

    assert_eq!(k.get(&admin, "Party", dup, false).await.unwrap()["id"], json!(keep.to_string()), "the old id answers with what it became");
    let listed = k.list(&admin, "Party", &ListQuery { limit: Some(50), ..Default::default() }).await.unwrap();
    assert!(listed.rows.iter().all(|r| r["id"] != json!(dup.to_string())), "lists no longer show it");
    assert!(matches!(k.update(&admin, "Party", dup, obj(json!({"party_name": "Ghost"})), None).await, Err(Error::Conflict(_))), "it cannot be changed");
    assert!(matches!(k.delete(&admin, "Party", dup, None).await, Err(Error::Conflict(_))));
    let stale = k.insert(&admin, "Order", obj(json!({"reference": "SO-2", "party": dup}))).await.expect("a form opened before the merge");
    assert_eq!(stale["party"], json!(keep.to_string()), "lands on the record that stayed");
    let tags = k.tags(&admin, "Party", keep).await.unwrap();
    assert_eq!(tags.len(), 2, "a tag both had stays once: {tags:?}");
    assert_eq!(k.comments(&admin, "Party", keep).await.unwrap().len(), 1, "comments come along");
    assert!(k.read_file(&admin, file).await.is_ok(), "files come along");
    assert!(k.duplicates(&manager, "Party", 50).await.unwrap().is_empty(), "nothing left to suggest");

    // Twice over: chains do not form.
    let third = id(&k.insert(&admin, "Party", party("Globex Limited", "")).await.unwrap());
    k.merge_records(&admin, "Party", third, keep, "It has a better name").await.expect("merged again");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(ferrum_kernel::merge::merged_ids(&mut tx, tenant, "Party", dup).await.unwrap().len(), 3, "the first duplicate still finds its way");
    tx.commit().await.unwrap();
    assert_eq!(k.get(&admin, "Party", dup, false).await.unwrap()["id"], json!(third.to_string()));
    assert!(matches!(k.merge_records(&admin, "Party", dup, other, "into a ghost").await, Err(Error::Invalid(_))), "not into what is gone");
    assert!(matches!(k.merge_records(&admin, "Party", other, dup, "again").await, Err(Error::Conflict(_))), "and not twice");
}

// ───────────── who may merge, and links that decide who reaches records (S9 K0-2, R19) ─────────────

const REACH_PACKAGE: &str = r#"package: reach
version: 0.1.0
label: Reach
depends: [core, parties]
roles:
  - name: Rep
    permissions:
      - {entity: RcLead, read: true, where: {territory: north}}
      - {entity: RcLead, read: true, write: true, create: true, delete: true, if_owner: true}
  - name: Keeper
    permissions:
      - {entity: Customer, read: true, write: true, create: true, delete: true}
      - {entity: RcProject, read: true, write: true, create: true}
      - {entity: RcInvoice, read: true, write: true, create: true}
      - {entity: RcAccount, read: true, write: true, create: true, delete: true}
      - {entity: RcTask, read: true, write: true, create: true}
  - name: Writer
    permissions:
      - {entity: RcLead, read: true, write: true}
      - {entity: RcLead, read: true, create: true, delete: true, if_owner: true}
  - name: Client
    permissions:
      - {entity: RcTask, read: true, via: account.contact_email}
"#;

const REACH_ENTITIES: &str = r#"
- name: RcLead
  label: Lead
  kind: master
  title_field: lead_name
  mergeable: true
  fields:
    - {name: lead_name, type: data, required: true}
    - {name: territory, type: data}
- name: RcProject
  label: Project
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: customer, type: link, to: Customer}
- name: RcInvoice
  label: Invoice
  kind: master
  title_field: title
  reach_links: [customer]
  fields:
    - {name: title, type: data, required: true}
    - {name: customer, type: link, to: Customer}
- name: RcAccount
  label: Account
  kind: master
  title_field: account_name
  mergeable: true
  fields:
    - {name: account_name, type: data, required: true}
    - {name: contact_email, type: email}
- name: RcTask
  label: Task
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: account, type: link, to: RcAccount}
"#;

fn reach_catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-merge-reach-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("reach/entities")).unwrap();
    for p in ["core", "contacts", "parties"] {
        std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../packages/{p}")).canonicalize().unwrap(), dir.join(p)).unwrap();
    }
    std::fs::write(dir.join("reach/package.yaml"), REACH_PACKAGE).unwrap();
    std::fs::write(dir.join("reach/entities/reach.yaml"), REACH_ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

struct Reach {
    k: Arc<Kernel>,
    admin: ferrum_kernel::Ctx,
}

impl Reach {
    async fn user(&self, email: &str, roles: &[&str]) -> ferrum_kernel::Ctx {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        let u = self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &roles, false).await.unwrap();
        self.k.load_ctx(self.admin.tenant, u, None, "it".into()).await.unwrap()
    }
    async fn insert(&self, ctx: &ferrum_kernel::Ctx, entity: &str, v: Value) -> Uuid {
        id(&self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("{entity}: {e}")))
    }
}

async fn reach_world() -> Reach {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, reach_catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "rc".into(),
            name: "Reach".into(),
            admin_email: "admin@rc.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["reach".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("rc", "admin@rc.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    Reach { k, admin }
}

fn reach_test<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

#[test]
fn a_reader_by_territory_cannot_merge_a_lead_into_their_own() {
    reach_test(|| async {
        let w = reach_world().await;
        let rep = w.user("rep@rc.test", &["Rep"]).await;
        let theirs = w.insert(&w.admin, "RcLead", json!({"lead_name": "Globex", "territory": "north"})).await;
        let mine = w.insert(&rep, "RcLead", json!({"lead_name": "Globex Ltd"})).await;
        w.k.get(&rep, "RcLead", theirs, false).await.expect("they read it through the territory");
        let e = w.k.merge_records(&rep, "RcLead", mine, theirs, "same company").await.unwrap_err();
        assert!(matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "{e:?}");
        w.k.get(&w.admin, "RcLead", theirs, false).await.expect("still there, its own");
    });
}

#[test]
fn merge_needs_delete_on_the_duplicate_and_write_on_the_kept() {
    reach_test(|| async {
        let w = reach_world().await;
        let rep = w.user("rep@rc.test", &["Rep"]).await;
        let writer = w.user("writer@rc.test", &["Writer"]).await;
        let theirs = w.insert(&w.admin, "RcLead", json!({"lead_name": "Initech", "territory": "north"})).await;
        let mine = w.insert(&rep, "RcLead", json!({"lead_name": "Initech Inc"})).await;
        // Rep deletes their own, but may not change the other's: their own does not go into it.
        let e = w.k.merge_records(&rep, "RcLead", theirs, mine, "same").await.unwrap_err();
        assert!(matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "{e:?}");
        // Writer changes every lead but deletes only their own: the administrator's does not go.
        let writers = w.insert(&writer, "RcLead", json!({"lead_name": "Initech Group"})).await;
        let e = w.k.merge_records(&writer, "RcLead", writers, theirs, "same").await.unwrap_err();
        assert!(matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "{e:?}");
        // Their own into one they may change: merged.
        w.k.merge_records(&writer, "RcLead", theirs, writers, "same").await.expect("delete on the duplicate, write on the kept");
    });
}

#[test]
fn merging_customers_that_decide_portal_reach_is_for_administrators() {
    reach_test(|| async {
        let w = reach_world().await;
        let keeper = w.user("keeper@rc.test", &["Keeper"]).await;
        let a = w.insert(&keeper, "Customer", json!({"customer_name": "Acme"})).await;
        let b = w.insert(&keeper, "Customer", json!({"customer_name": "ACME Inc"})).await;
        w.insert(&keeper, "RcProject", json!({"title": "Roof", "customer": a.to_string()})).await;
        // A customer belongs to the parties package: any link it moves decides someone's reach.
        let e = w.k.merge_records(&keeper, "Customer", b, a, "same").await.unwrap_err();
        assert!(matches!(&e, Error::Forbidden(m) if m.contains("administrator")), "{e:?}");
        // Nothing moved.
        let p = w.k.list(&w.admin, "RcProject", &ListQuery::default()).await.unwrap();
        assert_eq!(p.rows[0]["customer"], json!(a.to_string()));
        // An account (not a party) whose tasks a portal rule reaches through it: the same.
        let x = w.insert(&keeper, "RcAccount", json!({"account_name": "X", "contact_email": "x@client.test"})).await;
        let y = w.insert(&keeper, "RcAccount", json!({"account_name": "X Co", "contact_email": "y@client.test"})).await;
        w.insert(&keeper, "RcTask", json!({"title": "Fix", "account": x.to_string()})).await;
        let e = w.k.merge_records(&keeper, "RcAccount", y, x, "same").await.unwrap_err();
        assert!(matches!(&e, Error::Forbidden(m) if m.contains("administrator")), "{e:?}");
        // Accounts nothing links to merge for whoever may.
        let z = w.insert(&keeper, "RcAccount", json!({"account_name": "Z"})).await;
        w.k.merge_records(&keeper, "RcAccount", y, z, "same").await.expect("no link that decides reach moves");
    });
}

#[test]
fn merging_customers_with_portal_invoices_is_for_administrators() {
    reach_test(|| async {
        let w = reach_world().await;
        let keeper = w.user("keeper@rc.test", &["Keeper"]).await;
        let x = w.insert(&keeper, "RcAccount", json!({"account_name": "Inv"})).await;
        let _ = x;
        // RcInvoice declares `customer` in reach_links (its app's code reaches invoices by it): reach decides.
        let schema = w.k.registry.schema(w.admin.tenant).await.unwrap();
        let ent = schema.entity("RcInvoice").unwrap();
        let mut tx = w.k.db.tenant_tx(w.admin.tenant).await.unwrap();
        let fields = ferrum_kernel::reach::fields(&w.admin, &mut tx, ent).await.unwrap();
        tx.rollback().await.unwrap();
        assert!(fields.contains("customer"), "{fields:?}");
        let a = w.insert(&keeper, "Customer", json!({"customer_name": "Hooli"})).await;
        let b = w.insert(&keeper, "Customer", json!({"customer_name": "Hooli Corp"})).await;
        w.insert(&keeper, "RcInvoice", json!({"title": "INV-1", "customer": a.to_string()})).await;
        let e = w.k.merge_records(&keeper, "Customer", b, a, "same").await.unwrap_err();
        assert!(matches!(&e, Error::Forbidden(m) if m.contains("administrator")), "{e:?}");
    });
}

#[test]
fn a_link_written_during_a_merge_is_counted() {
    reach_test(|| async {
        let w = reach_world().await;
        let keeper = w.user("keeper@rc.test", &["Keeper"]).await;
        let x = w.insert(&keeper, "RcAccount", json!({"account_name": "Late"})).await;
        let y = w.insert(&keeper, "RcAccount", json!({"account_name": "Late Co"})).await;
        // Nothing links to the duplicate when the merge is asked for; a task linking it comes before the moves run.
        let groups = w.k.duplicates(&keeper, "RcAccount", 50).await.unwrap_or_default();
        let _ = groups;
        w.insert(&keeper, "RcTask", json!({"title": "Late task", "account": x.to_string()})).await;
        let e = w.k.merge_records(&keeper, "RcAccount", y, x, "same").await.unwrap_err();
        assert!(matches!(&e, Error::Forbidden(m) if m.contains("administrator")), "the moves themselves are counted: {e:?}");
    });
}

#[test]
fn an_administrator_s_merge_moves_portal_reach_and_says_so() {
    reach_test(|| async {
        let w = reach_world().await;
        let keeper = w.user("keeper@rc.test", &["Keeper"]).await;
        let client = w.user("x@client.test", &["Client"]).await;
        let x = w.insert(&keeper, "RcAccount", json!({"account_name": "Move", "contact_email": "x@client.test"})).await;
        let y = w.insert(&keeper, "RcAccount", json!({"account_name": "Move Co", "contact_email": "y@client.test"})).await;
        let task = w.insert(&keeper, "RcTask", json!({"title": "Moved", "account": x.to_string()})).await;
        w.k.get(&client, "RcTask", task, false).await.expect("theirs through the account");
        let done = w.k.merge_records(&w.admin, "RcAccount", y, x, "same").await.unwrap();
        assert_eq!(done["reach_moved"], json!(1), "{done}");
        // The task now reaches the kept account's people, and the client's reach went with the merge.
        assert!(w.k.get(&client, "RcTask", task, false).await.is_err());
        let mut tx = w.k.db.tenant_tx(w.admin.tenant).await.unwrap();
        let said: Value = sqlx::query_scalar("select diff from audit_event where entity = 'RcAccount' and record_id = $1 and action = 'merge'")
            .bind(y)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(said["reach_moved"], json!(1), "{said}");
    });
}
