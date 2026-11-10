//! Lines within lines (docs/builder/decisions.md BD-06, 11 November 2026), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). An administrator defines, through one customization
//! batch, an audit whose tests each hold several evidence rows: a record, its lines, their lines, two levels and no
//! more. The test walks everything that reads or writes lines at the second level: load and save (rows kept by id,
//! dropped with their line, never moved), compute and validate (row rules and computed fields with `parent` the
//! line, exact totals at both levels, errors that say where), deleting, the workspace's export and an import of
//! it, erasure's way of emptying a table, the definition a designer redefines from, configured `append_line` and
//! `set_line`, `links.<field>`, a pack's reference records, merging, a link's delete guard, an attach field's
//! file, and what is refused. A record of 200 lines of 10 rows each is saved and loaded in a counted number of
//! statements.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// ───────────── counting statements: every query sqlx runs is an event of target `sqlx::query` ─────────────

static COUNTING: AtomicBool = AtomicBool::new(false);
static STATEMENTS: AtomicUsize = AtomicUsize::new(0);

struct Statements;

impl tracing::Subscriber for Statements {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == "sqlx::query"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        if COUNTING.load(Ordering::SeqCst) {
            STATEMENTS.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The statements `f` runs, and how long it took.
async fn counted<T>(f: impl std::future::Future<Output = T>) -> (T, usize, std::time::Duration) {
    STATEMENTS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    let out = f.await;
    let took = start.elapsed();
    COUNTING.store(false, Ordering::SeqCst);
    (out, STATEMENTS.load(Ordering::SeqCst), took)
}

const ENTITIES: &str = r#"
- name: Basket
  label: Basket
  kind: master
  fields:
    - {name: name, type: data}
    - {name: items, type: table, of: BasketItem}
- name: BasketItem
  label: Basket Item
  kind: child
  fields:
    - {name: price, type: currency}
"#;

fn write_catalog(dir: &Path, entities: &str) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("shop/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shop/package.yaml"), "package: shop\nversion: 0.1.0\nlabel: Shop\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("shop/entities/shop.yaml"), entities).unwrap();
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-nested-lines-it-{}", std::process::id()));
    write_catalog(&dir, ENTITIES);
    let c = Catalog::load_dir(&dir).expect("catalog");
    // A package's line entity holds no table of its own: lines within lines are a workspace's own entities'.
    let nested = format!(
        "{ENTITIES}    - {{name: parts, type: table, of: BasketPart}}\n- name: BasketPart\n  label: Part\n  kind: child\n  fields:\n    - {{name: n, type: int}}\n"
    );
    write_catalog(&dir, &nested);
    let e = Catalog::load_dir(&dir).expect_err("a package's line holding lines");
    assert!(e.to_string().contains("child entities cannot nest tables"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
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

fn col(rows: &Value, field: &str) -> Vec<Value> {
    rows.as_array().unwrap().iter().map(|r| r[field].clone()).collect()
}

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

fn define(def: Value) -> NewCustomization {
    let name = def["name"].as_str().unwrap().to_string();
    op(&name, json!({"op": "define_entity", "entity": def}))
}

fn vendor_def() -> Value {
    json!({"name": "XVendor", "label": "Vendor", "kind": "master", "title_field": "name", "mergeable": true,
        "fields": [{"name": "name", "type": "data", "label": "Name", "unique": true}]})
}

fn evidence_def() -> Value {
    json!({"name": "XEvidence", "label": "Evidence", "kind": "child",
        "fields": [
            {"name": "title", "type": "data", "label": "Title", "required": true},
            {"name": "amount", "type": "decimal", "label": "Amount"},
            {"name": "file", "type": "attach", "label": "File"},
            {"name": "vendor", "type": "link", "to": "XVendor"},
            {"name": "email", "type": "data"},
            {"name": "tag", "type": "data", "computed": "doc.title == null ? parent.name : parent.name + '/' + doc.title"}
        ],
        "rules": [{"name": "frozen", "check": "parent.name != 'frozen' || doc.amount == 0.0", "message": "nothing is spent on a frozen test"}]})
}

fn test_def() -> Value {
    json!({"name": "XTest", "label": "Test", "kind": "child",
        "fields": [
            {"name": "name", "type": "data", "label": "Name", "required": true},
            {"name": "score", "type": "int"},
            {"name": "spent", "type": "decimal", "rollup": {"table": "evidence", "op": "sum", "field": "amount"}},
            {"name": "pieces", "type": "int", "rollup": {"table": "evidence", "op": "count"}},
            {"name": "evidence", "type": "table", "of": "XEvidence", "label": "Evidence"}
        ],
        "rules": [{"name": "few", "check": "doc.evidence.size() <= 20", "message": "at most 20 pieces of evidence"}]})
}

fn audit_def() -> Value {
    json!({"name": "XAudit", "label": "Audit", "kind": "master", "title_field": "code",
    "fields": [
        {"name": "code", "type": "data", "label": "Code", "unique": true},
        {"name": "total", "type": "decimal", "rollup": {"table": "tests", "op": "sum", "field": "spent"}},
        {"name": "test_count", "type": "int", "rollup": {"table": "tests", "op": "count"}},
        {"name": "tests", "type": "table", "of": "XTest", "label": "Tests"}
    ]})
}

fn defs() -> Vec<NewCustomization> {
    vec![define(vendor_def()), define(evidence_def()), define(test_def()), define(audit_def())]
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
    async fn scalar(&self, sql: &str, id: Option<Uuid>) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let mut q = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant);
        if let Some(id) = id {
            q = q.bind(id);
        }
        let n: i64 = q.fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }
    /// Each row of the workspace's custom records, by id: when it was last written.
    async fn written(&self) -> HashMap<Uuid, String> {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("select id, updated_at::text from custom_record where tenant_id = $1").bind(self.tenant).fetch_all(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        rows.into_iter().collect()
    }
    async fn batch(&self, items: Vec<NewCustomization>, dry_run: bool) -> std::result::Result<usize, (Option<usize>, Error)> {
        self.k.batch_customizations(&self.admin().await, items, dry_run).await
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

fn invalid(e: &Error) -> &[ferrum_kernel::FieldError] {
    match e {
        Error::Validation(f) => f,
        other => panic!("not a validation error: {other}"),
    }
}

#[test]
fn custom_lines_hold_lines_of_their_own() {
    tracing::subscriber::set_global_default(Statements).expect("the statement counter");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let w = workspace(&k, "acme").await;

    // ───────────── defining it in one batch; a third level and a key on a line's line are refused ─────────────
    assert_eq!(w.batch(defs(), false).await.expect("an audit, its tests, their evidence"), 4);
    let admin = w.admin().await;
    assert!(admin.schema.warnings.is_empty(), "{:?}", admin.schema.warnings);
    let three = vec![
        define(json!({"name": "XC3", "label": "C3", "kind": "child", "fields": [{"name": "n", "type": "int"}]})),
        define(json!({"name": "XC2", "label": "C2", "kind": "child", "fields": [{"name": "rows", "type": "table", "of": "XC3"}]})),
        define(json!({"name": "XC1", "label": "C1", "kind": "child", "fields": [{"name": "rows", "type": "table", "of": "XC2"}]})),
    ];
    let (at, e) = w.batch(three, true).await.expect_err("a third level");
    assert!(at == Some(2) && e.to_string().contains("lines go two levels deep (a record, its lines, their lines), and XC2 has lines of its own"), "{at:?} {e}");
    let mut keyed = evidence_def();
    keyed["name"] = json!("XKeyedEvidence");
    keyed["fields"][0]["unique"] = json!(true);
    let (_, e) = w.batch(vec![define(keyed)], true).await.expect_err("a unique key on a line's lines");
    assert!(e.to_string().contains("its lines cannot have unique keys"), "{e}");

    // ───────────── a record with lines with lines: saved, loaded, in order, totals exact at both levels ─────────────
    let input = json!({"code": "A-1", "total": "999", "tests": [
        {"name": "door", "score": 3, "spent": "7", "evidence": [
            {"title": "photo", "amount": "0.1", "email": "erin@acme.test"},
            {"title": "log", "amount": "0.2"}
        ]},
        {"name": "vault", "evidence": [
            {"title": "ship", "amount": "1234567890123456789"},
            {"title": "fee", "amount": "0.01"},
            {"title": "seal"}
        ]},
        {"name": "lobby"}
    ]});
    let a = k.insert(&admin, "XAudit", obj(input)).await.expect("an audit with tests with evidence");
    let id = id_of(&a);
    let tests = &a["tests"];
    assert_eq!(col(tests, "name"), [json!("door"), json!("vault"), json!("lobby")]);
    assert_eq!(col(tests, "idx"), [json!(0), json!(1), json!(2)]);
    assert_eq!(col(&tests[0]["evidence"], "title"), [json!("photo"), json!("log")]);
    assert_eq!(col(&tests[1]["evidence"], "idx"), [json!(0), json!(1), json!(2)], "idx by place at each level");
    assert_eq!(tests[2]["evidence"], json!([]));
    assert_eq!(col(tests, "spent"), [json!("0.3"), json!("1234567890123456789.01"), json!("0")], "a line's total over its own lines, exact");
    assert_eq!(col(tests, "pieces"), [json!(2), json!(3), json!(0)]);
    assert_eq!((&a["total"], &a["test_count"]), (&json!("1234567890123456789.31"), &json!(3)), "the record's total over its lines' totals, exact");
    assert_eq!(col(&tests[1]["evidence"], "tag"), [json!("vault/ship"), json!("vault/fee"), json!("vault/seal")], "computed with parent its line");
    let loaded = k.get(&admin, "XAudit", id, false).await.unwrap();
    assert_eq!(loaded["tests"], a["tests"], "loaded as saved, ids, order and values");
    assert_eq!(w.scalar("select count(*) from custom_record where tenant_id = $1 and entity = 'XEvidence'", None).await, 5);
    // What the API returns, for docs/api.md.
    let shown = k.get(&admin, "XAudit", id, false).await.unwrap();
    println!("RECORD {}", serde_json::to_string_pretty(&json!({"code": shown["code"], "total": shown["total"], "tests": [shown["tests"][0]]})).unwrap());
    // The draft preview works it out the same way.
    let draft = k
        .preview(
            &admin,
            "XAudit",
            obj(json!({"code": "P", "tests": [{"name": "t", "evidence": [{"title": "a", "amount": "0.1"}, {"title": "b", "amount": "0.2"}]}]})),
        )
        .await
        .unwrap();
    assert_eq!((&draft["total"], &draft["tests"][0]["spent"], &draft["tests"][0]["evidence"][1]["tag"]), (&json!("0.3"), &json!("0.3"), &json!("t/b")));

    // ───────────── replacing the lines: kept by id with their own lines, dropped with them ─────────────
    let (door, vault) = (tests[0]["id"].clone(), tests[1]["id"].clone());
    let vault_id: Uuid = vault.as_str().unwrap().parse().unwrap();
    let before = w.written().await;
    let edited = k
        .update(&admin, "XAudit", id, obj(json!({"tests": [{"id": door}, {"name": "roof", "evidence": [{"title": "drone"}]}, {"id": tests[2]["id"]}]})), None)
        .await
        .expect("keep two lines, drop one, add one");
    assert_eq!(col(&edited["tests"], "name"), [json!("door"), json!("roof"), json!("lobby")]);
    assert_eq!(edited["tests"][0]["evidence"], a["tests"][0]["evidence"], "a kept line keeps its own lines as they were");
    assert_eq!(
        w.scalar("select count(*) from custom_record where tenant_id = $1 and parent_id = $2", Some(vault_id)).await,
        0,
        "the dropped line's own lines went with it"
    );
    assert_eq!(w.scalar("select count(*) from custom_record where tenant_id = $1 and entity = 'XEvidence'", None).await, 3);
    assert_eq!(edited["total"], json!("0.3"));
    let after = w.written().await;
    let door_rows: Vec<Uuid> = a["tests"][0]["evidence"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().parse().unwrap()).collect();
    assert!(door_rows.iter().all(|r| before[r] == after[r]), "the kept line's own lines were not written");

    // ───────────── one line's line changed: one version, that row alone written ─────────────
    let e0 = edited["tests"][0]["evidence"][0]["id"].clone();
    let e1 = edited["tests"][0]["evidence"][1]["id"].clone();
    let before = w.written().await;
    let changed = k
        .update(&admin, "XAudit", id, obj(json!({"tests": [{"id": door, "evidence": [{"id": e0, "title": "photo, signed"}, {"id": e1}]}, {"id": edited["tests"][1]["id"]}, {"id": edited["tests"][2]["id"]}]})), Some(2))
        .await
        .expect("change one row of one line");
    assert_eq!(changed["version"], json!(3), "the record's version goes up once");
    assert_eq!(changed["tests"][0]["evidence"][0]["tag"], json!("door/photo, signed"));
    let after = w.written().await;
    let moved: Vec<&Uuid> = after.keys().filter(|r| before.get(*r) != after.get(*r)).collect();
    let e0_id: Uuid = e0.as_str().unwrap().parse().unwrap();
    assert_eq!(moved.into_iter().filter(|r| **r != id).collect::<Vec<_>>(), [&e0_id], "only the changed row (and the record) were written");

    // ───────────── a line's line stays under its line ─────────────
    let e = k
        .update(
            &admin,
            "XAudit",
            id,
            obj(json!({"tests": [{"id": door, "evidence": []}, {"id": edited["tests"][1]["id"], "evidence": [{"id": e1, "title": "stolen"}]}]})),
            None,
        )
        .await
        .expect_err("a row sent under another line");
    let f = &invalid(&e)[0];
    assert_eq!(
        (f.field.as_str(), f.row.clone(), f.path.clone()),
        ("evidence", Some(("tests".into(), 1)), Some(vec![("tests".into(), 1), ("evidence".into(), 0)]))
    );
    assert!(f.message.contains("stays under its line"), "{}", f.message);
    let other =
        k.insert(&admin, "XAudit", obj(json!({"code": "B-1", "tests": [{"name": "gate", "evidence": [{"id": e1, "title": "copied"}]}]}))).await.unwrap();
    assert_ne!(other["tests"][0]["evidence"][0]["id"], e1, "another record's row id makes a new row, as for lines");
    assert_eq!(k.get(&admin, "XAudit", id, false).await.unwrap()["tests"][0]["evidence"][1]["title"], json!("log"), "and the row stays where it was");

    // ───────────── rules, required fields and links on a line's lines say where ─────────────
    let e = k
        .insert(&admin, "XAudit", obj(json!({"code": "R-1", "tests": [{"name": "ok"}, {"name": "frozen", "evidence": [{"title": "free", "amount": "0"}, {"title": "paid", "amount": "2"}]}]})))
        .await
        .expect_err("a rule of a line's line reading its line");
    let errs = invalid(&e);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!((errs[0].field.as_str(), errs[0].path.clone()), ("frozen", Some(vec![("tests".into(), 1), ("evidence".into(), 1)])));
    let e = k.insert(&admin, "XAudit", obj(json!({"code": "R-2", "tests": [{"name": "t", "evidence": [{"amount": "1"}]}]}))).await.expect_err("no title");
    let f = &invalid(&e)[0];
    assert_eq!((f.field.as_str(), f.message.as_str()), ("title", "Title is required"));
    assert_eq!((f.row.clone(), f.path.clone()), (Some(("tests".into(), 0)), Some(vec![("tests".into(), 0), ("evidence".into(), 0)])));
    println!("FIELD ERROR {}", serde_json::to_string(f).unwrap());
    let one_level = k.insert(&admin, "XAudit", obj(json!({"code": "R-3", "tests": [{"evidence": []}]}))).await.expect_err("no name");
    assert_eq!(
        serde_json::to_value(&invalid(&one_level)[0]).unwrap(),
        json!({"field": "name", "message": "Name is required", "row": ["tests", 0]}),
        "a line's error as before"
    );
    let gone = Uuid::now_v7();
    let e = k
        .insert(&admin, "XAudit", obj(json!({"code": "R-4", "tests": [{"name": "t", "evidence": [{"title": "x", "vendor": gone.to_string()}]}]})))
        .await
        .expect_err("a link to nothing");
    assert_eq!(invalid(&e)[0].path, Some(vec![("tests".into(), 0), ("evidence".into(), 0)]));
    let many = Value::Array((0..21).map(|i| json!({"title": format!("e{i}")})).collect());
    let e = k
        .insert(&admin, "XAudit", obj(json!({"code": "R-5", "tests": [{"name": "t", "evidence": many}]})))
        .await
        .expect_err("a line's own rule over its lines");
    assert_eq!((invalid(&e)[0].field.as_str(), invalid(&e)[0].row.clone(), invalid(&e)[0].path.clone()), ("few", Some(("tests".into(), 0)), None));
    let e = k.insert(&admin, "XAudit", obj(json!({"code": "R-6", "tests": [{"name": "t", "evidence": "x"}]}))).await.expect_err("not a list");
    assert_eq!(invalid(&e)[0].message, "must be a list of rows");

    // ───────────── filters and keys on a line's lines ─────────────
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"tests.evidence.title": "log"})).unwrap(), ..Default::default() };
    let e = k.list(&admin, "XAudit", &q).await.expect_err("a filter on a line's line");
    assert!(e.to_string().contains("a filter reads a record's lines, not their own lines"), "{e}");
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"tests.name": "door"})).unwrap(), ..Default::default() };
    assert_eq!(k.list(&admin, "XAudit", &q).await.unwrap().rows.len(), 1, "a filter on a line's own field as before");

    // ───────────── the description and the definition a designer starts from ─────────────
    let ent = k.entity(&admin, "XAudit").unwrap();
    let d = ferrum_kernel::describe::describe(&admin, &ent);
    assert_eq!(d["children"]["tests"]["entity"], json!("XTest"));
    assert_eq!(d["children"]["tests"]["children"]["evidence"]["entity"], json!("XEvidence"));
    assert!(d["children"]["tests"]["children"]["evidence"]["fields"].as_array().unwrap().iter().any(|f| f["name"] == json!("amount")));
    let def = k.custom_definition(&admin, "XAudit").unwrap();
    let lines = def["lines"].as_object().unwrap();
    let mut names: Vec<&String> = lines.keys().collect();
    names.sort();
    assert_eq!(names, ["XEvidence", "XTest"], "its lines' entity and theirs");
    let stored_before = k.get(&admin, "XAudit", id, false).await.unwrap();
    let redefine: Vec<NewCustomization> = ["XEvidence", "XTest"]
        .iter()
        .map(|n| op(n, json!({"op": "redefine_entity", "entity": lines[*n]})))
        .chain([op("XAudit", json!({"op": "redefine_entity", "entity": def["entity"]}))])
        .collect();
    w.batch(redefine, false).await.expect("redefining it as it is");
    let admin = w.admin().await;
    assert_eq!(k.custom_definition(&admin, "XAudit").unwrap(), def, "the identity");
    assert_eq!(k.get(&admin, "XAudit", id, false).await.unwrap(), stored_before, "and its records as they were");
    // A line's lines cannot become another table's while there are any.
    let mut flat_test = test_def();
    flat_test["fields"] = json!([{"name": "name", "type": "data", "label": "Name", "required": true}, {"name": "score", "type": "int"}]);
    let mut loose = audit_def();
    loose["fields"] = json!([{"name": "code", "type": "data", "label": "Code", "unique": true}, {"name": "tests", "type": "table", "of": "XTest"},
        {"name": "loose", "type": "table", "of": "XEvidence"}]);
    let (_, e) = w
        .batch(
            vec![
                op(
                    "XAudit",
                    json!({"op": "redefine_entity", "entity": {"name": "XAudit", "label": "Audit", "kind": "master", "title_field": "code", "fields": [
                    {"name": "code", "type": "data", "label": "Code", "unique": true}, {"name": "tests", "type": "table", "of": "XTest"}]}}),
                ),
                op("XTest", json!({"op": "redefine_entity", "entity": flat_test})),
                op("XAudit", json!({"op": "redefine_entity", "entity": loose})),
            ],
            true,
        )
        .await
        .expect_err("evidence rows becoming the audit's own lines");
    assert!(e.to_string().contains("Evidence: its rows are lines of XTest records; they cannot become the lines of XAudit.loose while there are any"), "{e}");

    // ───────────── configured append_line and set_line leave the other lines' own lines alone ─────────────
    w.batch(
        vec![
            op(
                "XAudit",
                json!({"op": "add_action", "action": {"name": "x_add", "label": "Add Test", "params": [{"name": "name", "label": "Name", "type": "data"}],
                "effects": [{"type": "append_line", "table": "tests", "values": {"name": "params.name"}}]}}),
            ),
            op(
                "XAudit",
                json!({"op": "add_action", "action": {"name": "x_score", "label": "Score", "params": [{"name": "name", "label": "Name", "type": "data"}],
                "effects": [{"type": "set_line", "table": "tests", "where": "row.name == params.name", "values": {"score": "5"}}]}}),
            ),
        ],
        false,
    )
    .await
    .expect("two buttons");
    let admin = w.admin().await;
    let before = k.get(&admin, "XAudit", id, false).await.unwrap();
    k.run_action(&admin, "XAudit", id, "x_add", json!({"name": "attic"})).await.expect("append a line");
    k.run_action(&admin, "XAudit", id, "x_score", json!({"name": "roof"})).await.expect("set a line");
    let after = k.get(&admin, "XAudit", id, false).await.unwrap();
    assert_eq!(col(&after["tests"], "name"), [json!("door"), json!("roof"), json!("lobby"), json!("attic")]);
    assert_eq!(after["tests"][0], before["tests"][0], "an untouched line with its own lines");
    assert_eq!((&after["tests"][1]["score"], &after["tests"][1]["evidence"]), (&json!(5), &before["tests"][1]["evidence"]), "the line set keeps its own lines");

    // ───────────── links.<field> shows a linked record's lines' lines ─────────────
    w.batch(
        vec![define(json!({"name": "XReview", "label": "Review", "kind": "master", "fields": [{"name": "audit", "type": "link", "to": "XAudit"}],
            "rules": [{"name": "unsigned", "check": "links.audit == null || links.audit.tests.all(t, t.evidence.all(e, e.title != 'log'))",
                "message": "the audit holds unsigned evidence"}]}))],
        false,
    )
    .await
    .unwrap();
    let admin = w.admin().await;
    let e = k.insert(&admin, "XReview", obj(json!({"audit": id.to_string()}))).await.expect_err("a rule over the linked record's lines' lines");
    assert!(e.to_string().contains("the audit holds unsigned evidence"), "{e}");
    k.insert(&admin, "XReview", obj(json!({"audit": other["id"]}))).await.expect("one without");

    // ───────────── erasure: the history's copies of a line's line, and emptying a table ─────────────
    k.create_user(&admin, "erin@acme.test", "Erin", None, &[], false).await.unwrap();
    let e_tmp = k
        .insert(&admin, "XAudit", obj(json!({"code": "E-1", "tests": [{"name": "t", "evidence": [{"title": "x", "email": "erin@acme.test"}]}]})))
        .await
        .unwrap();
    let held = "select count(*) from audit_event where tenant_id = $1 and record_id = $2 and diff::text ilike '%erin@acme.test%'";
    assert_eq!(w.scalar(held, Some(id_of(&e_tmp))).await, 1, "a new record's history holds its rows' values");
    let done = k.erase_personal_data(&admin, "erin@acme.test", "Request 7").await.expect("erased");
    assert_eq!(done["account_erased"], json!(true));
    assert_eq!(w.scalar(held, Some(id_of(&e_tmp))).await, 0, "the address went from the history, a line's line's included");
    assert_eq!(w.scalar(held, Some(id)).await, 0);
    // No declaration names a workspace's own entity (packages declare personal data), so the stored value stays.
    assert_eq!(w.scalar("select count(*) from custom_record where tenant_id = $1 and ext->>'email' = 'erin@acme.test'", None).await, 2);
    // A declared table is erased by emptying it (privacy.rs, erase_record): its lines' lines go with its lines.
    let engine = Ctx::system(w.tenant, admin.schema.clone());
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    k.set_child_rows_tx(&engine, &mut tx, "XAudit", "tests", id_of(&e_tmp), vec![]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        w.scalar("select count(*) from custom_record where tenant_id = $1 and ext->>'email' = 'erin@acme.test'", None).await,
        1,
        "only A-1's row is left"
    );
    k.delete(&admin, "XAudit", id_of(&e_tmp), None).await.unwrap();

    // ───────────── the workspace's export, and importing what it holds ─────────────
    let job = k.export_workspace(&admin).await.unwrap();
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
    let v = k.job(&admin, job).await.unwrap();
    assert_eq!(v["status"], json!("done"), "{v}");
    let part: Uuid = v["result"]["parts"][0]["id"].as_str().unwrap().parse().unwrap();
    let (_, bytes) = k.export_part(&admin, job, part).await.unwrap();
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut audits = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let name = f.name().unwrap().to_string();
        assert!(!name.starts_with("records/XTest") && !name.starts_with("records/XEvidence"), "lines are no records of their own: {name}");
        if name.starts_with("records/XAudit/") {
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            audits.extend(s.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()));
        }
    }
    let mut exported = audits.into_iter().find(|o| o["code"] == json!("A-1")).expect("A-1 exported");
    let stored = k.get(&admin, "XAudit", id, false).await.unwrap();
    assert_eq!(exported["tests"], stored["tests"], "with its lines' lines");
    exported["code"] = json!("A-1 again");
    exported.as_object_mut().unwrap().remove("id");
    let (made, failed) = k.import_rows(&admin, "XAudit", &[exported]).await.unwrap();
    assert!(failed.is_empty(), "{failed:?}");
    let copy = k.get(&admin, "XAudit", made[0].as_str().unwrap().parse().unwrap(), false).await.unwrap();
    let strip = |rows: &Value| -> Value {
        Value::Array(
            rows.as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    let mut t = t.clone();
                    t.as_object_mut().unwrap().remove("id");
                    for e in t["evidence"].as_array_mut().unwrap() {
                        e.as_object_mut().unwrap().remove("id");
                    }
                    t
                })
                .collect(),
        )
    };
    assert_eq!(strip(&copy["tests"]), strip(&stored["tests"]), "imported with its lines' lines, as new rows");
    assert_eq!(copy["total"], stored["total"]);

    // ───────────── deleting a record takes its lines and theirs ─────────────
    let copy_id = id_of(&copy);
    let rows = w.scalar("select count(*) from custom_record where tenant_id = $1 and entity in ('XTest', 'XEvidence')", None).await;
    let copy_rows = w
        .scalar(
            "select count(*) from custom_record l where l.tenant_id = $1 and (l.parent_id = $2 or l.parent_id in (select id from custom_record where tenant_id = $1 and parent_id = $2))",
            Some(copy_id),
        )
        .await;
    assert!(copy_rows > 4, "{copy_rows}");
    k.delete(&admin, "XAudit", copy_id, None).await.unwrap();
    assert_eq!(
        w.scalar("select count(*) from custom_record where tenant_id = $1 and entity in ('XTest', 'XEvidence')", None).await,
        rows - copy_rows,
        "no row left behind"
    );

    // ───────────── a file named on a line's line is the record's ─────────────
    let file = k.store_file(&admin, None, "proof.txt", "text/plain", b"proof").await.unwrap();
    let file_id: Uuid = file["id"].as_str().unwrap().parse().unwrap();
    let mut tests = after["tests"].as_array().unwrap().clone();
    tests[3]["evidence"] = json!([{"title": "scan", "file": file["url"]}]);
    k.update(&admin, "XAudit", id, obj(json!({"tests": tests})), None).await.expect("a file on a line's line");
    assert_eq!(
        w.scalar("select count(*) from file_object where tenant_id = $1 and record_id = $2 and entity = 'XAudit'", Some(id)).await,
        1,
        "tied to the record"
    );
    let _ = file_id;

    // ───────────── links to a vendor from a line's line: merged, and guarding the vendor's delete ─────────────
    let v1 = k.insert(&admin, "XVendor", obj(json!({"name": "Acme"}))).await.unwrap();
    let v2 = k.insert(&admin, "XVendor", obj(json!({"name": "Acme Ltd"}))).await.unwrap();
    let c = k
        .insert(&admin, "XAudit", obj(json!({"code": "V-1", "tests": [{"name": "t", "evidence": [{"title": "invoice", "vendor": v2["id"]}]}]})))
        .await
        .unwrap();
    let e = k.delete(&admin, "XVendor", id_of(&v2), None).await.expect_err("a vendor a line's line links to");
    assert!(matches!(e, Error::Conflict(_)), "{e}");
    k.merge_records(&admin, "XVendor", id_of(&v1), id_of(&v2), "the same vendor").await.expect("merged");
    assert_eq!(k.get(&admin, "XAudit", id_of(&c), false).await.unwrap()["tests"][0]["evidence"][0]["vendor"], v1["id"], "the link follows the merge");
    k.delete(&admin, "XAudit", id_of(&c), None).await.unwrap();
    k.delete(&admin, "XVendor", id_of(&v1), None).await.expect("nothing links to it now");

    // ───────────── a pack's reference records carry their lines' lines ─────────────
    let src = workspace(&k, "src").await;
    src.batch(defs(), false).await.unwrap();
    let sa = src.admin().await;
    k.insert(
        &sa,
        "XAudit",
        obj(json!({"code": "STD", "tests": [{"name": "door", "evidence": [{"title": "photo", "amount": "1.5"}, {"title": "log"}]}, {"name": "gate"}]})),
    )
    .await
    .unwrap();
    let pack =
        k.export_pack(&sa, &ExportOptions { name: "audits".into(), label: "Audits".into(), version: 1, records: vec!["XAudit".into()] }).await.expect("a pack");
    let rows = &pack["records"][0]["rows"][0];
    assert_eq!(
        rows["tests"][0]["evidence"],
        json!([{"amount": "1.5", "email": null, "title": "photo", "vendor": null}, {"amount": null, "email": null, "title": "log", "vendor": null}])
    );
    let dst = workspace(&k, "dst").await;
    let da = dst.admin().await;
    let plan = k.plan_pack(&da, &pack).await.expect("its plan");
    k.install_pack(&da, &pack, plan["digest"].as_str().unwrap()).await.expect("installed");
    let da = dst.admin().await;
    let there = k.list(&da, "XAudit", &ListQuery::default()).await.unwrap();
    let there = k.get(&da, "XAudit", there.rows[0]["id"].as_str().unwrap().parse().unwrap(), false).await.unwrap();
    assert_eq!(col(&there["tests"][0]["evidence"], "title"), [json!("photo"), json!("log")]);
    assert_eq!((&there["total"], &there["tests"][0]["evidence"][1]["tag"]), (&json!("1.5"), &json!("door/log")));

    // ───────────── 200 lines of 10 rows each: statements per save and load, whatever the rows ─────────────
    let big = |tag: &str| {
        Value::Array(
            (0..200)
                .map(|i| json!({"name": format!("t{i}"), "evidence": (0..10).map(|j| json!({"title": format!("{tag}{i}.{j}"), "amount": "0.01"})).collect::<Vec<_>>()}))
                .collect(),
        )
    };
    let (made, inserted, t_insert) = counted(k.insert(&admin, "XAudit", obj(json!({"code": "BIG", "tests": big("e")})))).await;
    let made = made.expect("200 by 10");
    assert_eq!(made["total"], json!("20"), "2,000 cents, exact");
    let big_id = id_of(&made);
    let (loaded, loads, t_load) = counted(k.get(&admin, "XAudit", big_id, false)).await;
    let loaded = loaded.unwrap();
    assert_eq!((loaded["tests"].as_array().unwrap().len(), loaded["tests"][199]["evidence"][9]["title"].clone()), (200, json!("e199.9")));
    let mut tests = loaded["tests"].as_array().unwrap().clone();
    tests[100]["evidence"][5]["title"] = json!("changed");
    let (changed, updated, t_update) = counted(k.update(&admin, "XAudit", big_id, obj(json!({"tests": tests})), None)).await;
    changed.expect("one row of 2,200 changed");
    let (replaced, replaced_n, t_replace) = counted(k.update(&admin, "XAudit", big_id, obj(json!({"tests": big("r")})), None)).await;
    replaced.expect("every row new");
    println!(
        "STATEMENTS insert {inserted} ({t_insert:?}), load {loads} ({t_load:?}), change one {updated} ({t_update:?}), replace all {replaced_n} ({t_replace:?})"
    );
    assert!(loads <= 6, "a load is one statement in its transaction: {loads}");
    for (what, n) in [("insert", inserted), ("change one", updated), ("replace all", replaced_n)] {
        assert!(n <= 40, "{what}: {n} statements for 2,200 rows");
    }
    assert_eq!(w.scalar("select count(*) from custom_record where tenant_id = $1 and entity = 'XEvidence' and ext->>'title' like 'e%'", None).await, 0);
}
