//! Actions an administrator sets up by configuration (docs/api.md, "Configured actions"), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first), and two faults found rebuilding
//! Governance and Risk as a pack (docs/builder/evidence/grc-as-a-pack-2026-11-10.md): a record's whole
//! numbers are signed in expressions, and `read_only_if` holds on the server.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::{Catalog, PermissionDef};
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
        "- name: Shop\n  label: Shop\n  kind: master\n  fields:\n    - {name: name, type: data}\n    - {name: status, type: data, read_only: true}\n  actions: [{name: lock, label: Lock}]\n",
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
    empty_copies(&workspace(&k, "copies").await).await;
    linked_from_automations(&w).await;
    configured(&k, &w).await;
    authored(&k, &w).await;
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
}

/// An effect whose value is only a path to a value copies it as stored, empty included, where an expression
/// reads an empty number as 0 and an empty tick as false (docs/api.md, "Configured actions"); found rebuilding
/// Government as a pack (docs/builder/evidence/government-as-a-pack-2026-11-11.md).
async fn empty_copies(w: &W) {
    let k = &w.k;
    let source = json!({"name": "XSource", "label": "Source", "kind": "master", "fields": [{"name": "owed", "type": "int"}, {"name": "twice", "type": "int"}]});
    let line = json!({"name": "XCopyLine", "label": "Copy Line", "kind": "child", "fields": [{"name": "a", "type": "int"}, {"name": "b", "type": "int"}]});
    let copy = json!({"name": "XCopy", "label": "Copy", "kind": "master", "fields": [
        {"name": "name", "type": "data"}, {"name": "from", "type": "link", "to": "XSource"},
        {"name": "n", "type": "currency"}, {"name": "d", "type": "date"}, {"name": "t", "type": "check"}, {"name": "i", "type": "int"},
        {"name": "n2", "type": "currency"}, {"name": "d2", "type": "date"}, {"name": "t2", "type": "check"}, {"name": "i2", "type": "int"},
        {"name": "plus", "type": "int"}, {"name": "lines", "type": "table", "of": "XCopyLine"}]});
    let copy_all = json!({"n2": "doc.n", "d2": "doc.d", "t2": "doc.t", "plus": "doc.i + 1"});
    w.batch(vec![
        define(source),
        define(line),
        define(copy),
        add_action("XCopy", json!({"name": "copy", "label": "Copy", "effects": [{"type": "set_fields", "values": copy_all}]})),
        add_action("XCopy", json!({"name": "copy_link", "label": "Copy link", "effects": [{"type": "set_fields", "values": {"i2": "links.from.owed"}}]})),
        add_action(
            "XCopy",
            json!({"name": "copy_line", "label": "Copy line", "effects": [{"type": "set_line", "table": "lines", "where": "true", "values": {"b": "row.a"}}]}),
        ),
    ])
    .await
    .unwrap();
    let admin = w.admin().await;
    automation::save(
        k,
        &admin,
        &obj(json!({"name": "Copy", "entity": "XCopy", "event": "created", "condition": "doc.name == 'auto'",
            "actions": [{"type": "set_fields", "values": copy_all}, {"type": "update_linked", "link": "from", "values": {"owed": "linked.twice"}}]})),
    )
    .await
    .unwrap();
    // A source whose `owed` and `twice` are empty.
    let thing = id_of(&k.insert(&admin, "XSource", obj(json!({}))).await.unwrap());
    let filled = json!({"n2": "5", "d2": "2026-01-01", "t2": true, "i2": 7, "plus": 9, "from": thing.to_string(), "lines": [{"b": 3}]});
    let empty = |doc: &Doc, what: &str| {
        assert_eq!((&doc["n2"], &doc["d2"], &doc["i2"]), (&Value::Null, &Value::Null, &Value::Null), "{what}: {doc:?}");
        assert_eq!(doc["t2"], doc["t"], "{what}: the tick as stored");
        assert_eq!(doc["plus"], json!(1), "{what}: doc.i + 1 on an empty i is still 1");
    };
    let mut button = filled.clone();
    button["name"] = json!("button");
    let id = id_of(&k.insert(&admin, "XCopy", obj(button)).await.unwrap());
    for a in ["copy", "copy_link", "copy_line"] {
        k.run_action(&admin, "XCopy", id, a, json!({})).await.unwrap_or_else(|e| panic!("{a}: {e:?}"));
    }
    let doc = w.get("XCopy", id).await;
    empty(&doc, "a button");
    assert_eq!(doc["lines"][0]["b"], Value::Null, "row.a, empty, copied: {doc:?}");
    // The same through an automation, and `linked`.
    k.update(&admin, "XSource", thing, obj(json!({"owed": 5})), None).await.unwrap();
    let mut auto = filled.clone();
    auto["name"] = json!("auto");
    auto["i2"] = Value::Null;
    let id = id_of(&k.insert(&admin, "XCopy", obj(auto)).await.unwrap());
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    empty(&w.get("XCopy", id).await, "an automation");
    assert_eq!(w.get("XSource", thing).await["owed"], Value::Null, "linked.twice, empty, copied");
}

/// An automation's `update_linked` writes the record a link names, as the administrator who saved it (a
/// read-only field stays as it was), through that record's pipeline; a chain of them stops at the depth rule.
async fn linked_from_automations(w: &W) {
    let k = &w.k;
    let parent = json!({"name": "XParent", "label": "Parent", "kind": "master", "fields": [
        {"name": "note", "type": "data"}, {"name": "status", "type": "data", "read_only": true}, {"name": "pings", "type": "int"},
        {"name": "last_child", "type": "link", "to": "XChild"}]});
    let child = json!({"name": "XChild", "label": "Child", "kind": "master", "fields": [
        {"name": "parent", "type": "link", "to": "XParent"}, {"name": "note", "type": "data"}, {"name": "bounces", "type": "int"}]});
    // Each refers to the other: defined together, then the link back.
    w.batch(vec![define(json!({"name": "XChild", "label": "Child", "kind": "master", "fields": [{"name": "note", "type": "data"}]})), define(parent)])
        .await
        .unwrap();
    let redefine: NewCustomization = serde_json::from_value(json!({"entity": "XChild", "op": {"op": "redefine_entity", "entity": child}})).unwrap();
    w.batch(vec![redefine]).await.unwrap();
    let admin = w.admin().await;
    let save = |a: Value| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { automation::save(&k, &admin, &obj(a)).await }
    };
    let bad =
        save(json!({"name": "Bad", "entity": "XChild", "event": "created", "actions": [{"type": "update_linked", "link": "note", "values": {"note": "'x'"}}]}))
            .await;
    assert!(message(&bad.unwrap_err()).contains("note is not a link field of Child"), "a link field, checked when saved");
    let bad = save(
        json!({"name": "Bad", "entity": "XChild", "event": "created", "actions": [{"type": "update_linked", "link": "parent", "values": {"nope": "'x'"}}]}),
    )
    .await;
    assert!(message(&bad.unwrap_err()).contains("Parent has no field nope"));
    save(json!({"name": "Tell the parent", "entity": "XChild", "event": "created",
        "actions": [{"type": "update_linked", "link": "parent", "values": {"note": "doc.note + ' (was ' + linked.note + ')'", "status": "'Touched'"}}]}))
    .await
    .unwrap();
    let p = k.insert(&admin, "XParent", obj(json!({"note": "n0"}))).await.unwrap();
    let c = k.insert(&admin, "XChild", obj(json!({"parent": id_of(&p).to_string(), "note": "hello"}))).await.unwrap();
    k.insert(&admin, "XChild", obj(json!({"note": "orphan"}))).await.expect("an empty link writes nothing");
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    let now = w.get("XParent", id_of(&p)).await;
    assert_eq!(now["note"], "hello (was n0)", "written, seeing the linked record as it was");
    assert_eq!(now["status"], Value::Null, "the administrator's rights: a read-only field is not theirs to write");

    // A child pings its parent, the parent bounces it back: the chain stops at the depth rule.
    for a in [
        json!({"name": "Ping", "entity": "XChild", "event": "updated", "actions": [{"type": "update_linked", "link": "parent", "values": {"pings": "linked.pings + 1"}}]}),
        json!({"name": "Bounce", "entity": "XParent", "event": "updated", "actions": [{"type": "update_linked", "link": "last_child", "values": {"bounces": "linked.bounces + 1"}}]}),
    ] {
        save(a).await.unwrap();
    }
    k.update(&admin, "XParent", id_of(&p), obj(json!({"last_child": id_of(&c).to_string()})), None).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!((w.get("XParent", id_of(&p)).await["pings"].as_i64(), w.get("XChild", id_of(&c)).await["bounces"].as_i64()), (Some(1), Some(2)));
    let stopped = w
        .count("select count(*) from automation_run where tenant_id = $1 and status = 'failed' and error like 'not run: automations made this change after 3%'")
        .await;
    assert_eq!(stopped, 1, "the fourth link of the chain is not run, and says why");
}

fn add_action(entity: &str, action: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": {"op": "add_action", "action": action}})).unwrap()
}

/// Refused as `kind` saying `says`.
fn refused<V: std::fmt::Debug>(r: Result<V, Error>, kind: &str, says: &str) {
    let msg = match &r {
        Err(e) => message(e),
        Ok(v) => format!("{v:?}"),
    };
    let ok = matches!(
        (&r, kind),
        (Err(Error::Invalid(_) | Error::Validation(_)), "invalid") | (Err(Error::Forbidden(_)), "forbidden") | (Err(Error::NotFound(_)), "not found")
    );
    assert!(ok && msg.contains(says), "expected {kind} saying {says:?}, got {msg}");
}

/// Deciding a risk acceptance, as Governance and Risk does: a button for approvers, refused to the risk's owner,
/// writing the read-only status, a log, a notification, a mail, and the risk itself.
fn accept() -> Value {
    json!({"name": "accept", "label": "Accept It", "condition": "doc.status == 'Draft'", "confirm": "Accept taking this risk on?",
        "roles": ["Approver"],
        "params": [{"name": "note", "label": "Note", "type": "text"}],
        "refuse_if": [{"when": "doc.risk_owner == user.email", "message": "a risk's owner does not approve taking it on"}],
        "effects": [
            {"type": "set_fields", "values": {"status": "'Accepted'", "approver": "user.email", "note": "params.note"}},
            {"type": "create_document", "entity": "XLog", "values": {"what": "'acceptance ' + doc.status"}},
            {"type": "notify", "to": "owner", "title": "'Accepted: ' + params.note"},
            {"type": "email", "to": "'risk@acme.test'", "subject": "'Accepted ' + params.note", "body": "'Taken on'"},
            {"type": "update_linked", "link": "risk", "values": {"treatment": "'Accept'", "accepted_by": "user.email"}}]})
}

async fn configured(k: &Arc<Kernel>, w: &W) {
    let risk = json!({"name": "XRisk", "label": "Risk", "kind": "master", "title_field": "title", "fields": [
        {"name": "title", "type": "data"}, {"name": "owner", "type": "email"}, {"name": "score", "type": "int"},
        {"name": "treatment", "type": "select", "options": ["Open", "Accept"], "default": "Open", "read_only": true},
        {"name": "accepted_by", "type": "email", "read_only": true}]});
    let acceptance = json!({"name": "XAcceptance", "label": "Acceptance", "kind": "master", "fields": [
        {"name": "risk", "type": "link", "to": "XRisk"}, {"name": "risk_owner", "type": "email", "read_only": true, "fetch_from": "risk.owner"},
        {"name": "requester", "type": "email"},
        {"name": "status", "type": "select", "options": ["Draft", "Accepted", "Withdrawn"], "default": "Draft", "read_only": true},
        {"name": "approver", "type": "email", "read_only": true}, {"name": "note", "type": "data"}]});
    let log = json!({"name": "XLog", "label": "Log", "kind": "master", "fields": [{"name": "what", "type": "data"}]});
    w.batch(vec![
        define(risk),
        define(acceptance),
        define(log),
        check("XRisk", "acceptable", "doc.treatment != 'Accept' || doc.score <= 20", "too risky to accept"),
    ])
    .await
    .unwrap();

    // ───────────── saved: checked, named and bounded ─────────────
    let refusal = |r: Result<usize, String>, says: &str| {
        let e = r.expect_err("refused");
        assert!(e.contains(says), "expected {says:?}, got {e}");
    };
    let effects = |e: Value| json!({"name": "x_try", "label": "Try", "effects": e});
    refusal(
        w.batch(vec![add_action("XAcceptance", effects(json!([{"type": "set_fields", "values": {"docstatus": "2"}}])))]).await,
        "docstatus is the platform's: no effect writes it",
    );
    refusal(
        w.batch(vec![add_action("XAcceptance", effects(json!([{"type": "update_linked", "link": "risk", "values": {"id": "doc.id"}}])))]).await,
        "id is the platform's: no effect writes it",
    );
    refusal(w.batch(vec![add_action("XAcceptance", effects(json!([{"type": "set_fields", "values": {"status": "doc.status +"}}])))]).await, "effect 1");
    refusal(
        w.batch(vec![add_action("XAcceptance", effects(json!([{"type": "update_linked", "link": "note", "values": {"title": "'x'"}}])))]).await,
        "not a link field",
    );
    refusal(w.batch(vec![add_action("XAcceptance", json!({"name": "x_bare", "label": "Bare"}))]).await, "needs code behind it");
    let eleven = vec![json!({"type": "notify", "to": "owner", "title": "'t'"}); 11];
    refusal(w.batch(vec![add_action("XAcceptance", effects(json!(eleven)))]).await, "at most 10 effects");
    // On a packaged entity: named x_…, and never its package's read-only fields.
    refusal(
        w.batch(vec![add_action("Shop", json!({"name": "close", "label": "Close", "effects": [{"type": "set_fields", "values": {"name": "'x'"}}]}))]).await,
        "is named x_",
    );
    refusal(
        w.batch(vec![add_action("Shop", json!({"name": "x_close", "label": "Close", "effects": [{"type": "set_fields", "values": {"status": "'Closed'"}}]}))])
            .await,
        "Shop.status is its package's to keep",
    );
    w.batch(vec![add_action(
        "Shop",
        json!({"name": "x_rename", "label": "Rename", "effects": [{"type": "set_fields", "values": {"name": "doc.name + '!'"}}]}),
    )])
    .await
    .expect("a field a person may type");
    let withdraw = json!({"name": "withdraw", "label": "Withdraw", "condition": "doc.status == 'Draft'",
        "effects": [{"type": "set_fields", "values": {"status": "'Withdrawn'"}}]});
    w.batch(vec![add_action("XAcceptance", accept()), add_action("XAcceptance", withdraw)]).await.unwrap();

    // ───────────── described as any action ─────────────
    let admin = w.admin().await;
    let described = ferrum_kernel::describe::describe(&admin, admin.schema.entity("XAcceptance").unwrap());
    let a = described["actions"].as_array().unwrap().iter().find(|a| a["name"] == "accept").cloned().unwrap();
    assert_eq!(
        (a["label"].as_str(), a["condition"].as_str(), a["params"][0]["name"].as_str()),
        (Some("Accept It"), Some("doc.status == 'Draft'"), Some("note"))
    );

    // ───────────── people ─────────────
    let person = |email: &'static str, role: &'static str| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let id = k.create_user(&admin, email, email, None, &[role.to_string()], false).await.unwrap();
            (id, k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap())
        }
    };
    for (role, entity, write) in [("Approver", "XAcceptance", true), ("Approver", "XRisk", false), ("Manager", "XAcceptance", true), ("Manager", "XRisk", true)]
    {
        let rule = PermissionDef { entity: entity.into(), read: true, write, create: role == "Manager", ..Default::default() };
        k.set_role_permission(&admin, role, &rule).await.unwrap();
    }
    let (manager_id, manager) = person("manager@acme.test", "Manager").await;
    let (approver_id, approver) = person("approver@acme.test", "Approver").await;
    let (_, owner) = person("owner@acme.test", "Approver").await;
    // Events and automations follow, after commit.
    automation::save(
        k,
        &admin,
        &obj(json!({"name": "Accepted risks", "entity": "XRisk", "event": "updated", "condition": "doc.treatment == 'Accept'",
        "actions": [{"type": "create_document", "entity": "XLog", "values": {"what": "'risk accepted'"}}]})),
    )
    .await
    .unwrap();

    let new_risk = |title: &str, score: i64| {
        let (k, manager) = (k.clone(), manager.clone());
        let v = json!({"title": title, "owner": "owner@acme.test", "score": score});
        async move { id_of(&k.insert(&manager, "XRisk", obj(v)).await.unwrap()) }
    };
    let new_acceptance = |risk: Option<Uuid>| {
        let (k, manager) = (k.clone(), manager.clone());
        let v = json!({"risk": risk.map(|r| r.to_string()), "requester": "pia@home.test", "status": "Accepted"});
        async move { id_of(&k.insert(&manager, "XAcceptance", obj(v)).await.unwrap()) }
    };
    let ransom = new_risk("Ransomware", 16).await;
    let acc = new_acceptance(Some(ransom)).await;
    assert_eq!(w.get("XAcceptance", acc).await["status"], "Draft", "read-only: what a person sends is ignored");
    let note = json!({"note": "insured"});
    let mails = "select count(*) from outbox where tenant_id = $1 and topic = 'ferrum.mail' and payload->>'subject' like 'Accepted%'";
    let (logs, mailed) = (w.count("select count(*) from custom_record where tenant_id = $1 and entity = 'XLog'").await, w.count(mails).await);

    // ───────────── who may run it ─────────────
    refused(k.run_action(&manager, "XAcceptance", acc, "accept", note.clone()).await, "forbidden", "'Accept It' is for Approver");
    refused(k.run_action(&owner, "XAcceptance", acc, "accept", note.clone()).await, "forbidden", "a risk's owner does not approve taking it on");
    refused(k.run_action(&approver, "XAcceptance", acc, "nope", note.clone()).await, "not found", "action nope");
    // The form learns who may press it, as pressing asks; what it writes is for administrators.
    let accept_as = |ctx: &Ctx| {
        let d = ferrum_kernel::describe::describe(ctx, ctx.schema.entity("XAcceptance").unwrap());
        d["actions"].as_array().unwrap().iter().find(|a| a["name"] == "accept").cloned().unwrap()
    };
    for (who, ctx, may) in [("manager", &manager, false), ("approver", &approver, true), ("admin", &admin, true)] {
        let a = accept_as(ctx);
        assert_eq!(a["may_run"], json!(may), "{who}");
        assert_eq!(a.get("effects").is_some(), who == "admin", "{who}: effects for an administrator only");
        let kept = (a["refuse_if"][0]["message"].as_str(), a["roles"][0].as_str(), a["confirm"].is_string(), a["params"][0]["name"].as_str());
        assert_eq!(kept, (Some("a risk's owner does not approve taking it on"), Some("Approver"), true, Some("note")), "{who}");
    }

    // ───────────── a check the effects break aborts everything ─────────────
    let huge = new_risk("Meteor", 25).await;
    let doomed = new_acceptance(Some(huge)).await;
    refused(k.run_action(&approver, "XAcceptance", doomed, "accept", note.clone()).await, "invalid", "too risky to accept");
    assert_eq!(w.get("XAcceptance", doomed).await["status"], "Draft", "nothing changed");
    assert_eq!(w.count("select count(*) from custom_record where tenant_id = $1 and entity = 'XLog'").await, logs, "no record made");
    assert_eq!(w.count(mails).await, mailed, "no mail queued");
    assert_eq!(w.count("select count(*) from notification where tenant_id = $1 and title like 'Accepted%'").await, 0, "no one told");

    // ───────────── run by an approver ─────────────
    let done = k.run_action(&approver, "XAcceptance", acc, "accept", note.clone()).await.expect("accepted");
    assert_eq!(done["status"], "Accepted", "{done}");
    assert_eq!((done["made"][0]["entity"].as_str(), done["made"].as_array().map(Vec::len)), (Some("XLog"), Some(1)), "{done}");
    let a = w.get("XAcceptance", acc).await;
    assert_eq!((a["status"].as_str(), a["approver"].as_str(), a["note"].as_str()), (Some("Accepted"), Some("approver@acme.test"), Some("insured")));
    assert_eq!(a["updated_by"].as_str(), Some(approver_id.to_string().as_str()), "the records say who ran it");
    let r = w.get("XRisk", ransom).await;
    assert_eq!((r["treatment"].as_str(), r["accepted_by"].as_str()), (Some("Accept"), Some("approver@acme.test")), "the linked risk, read-only fields and all");
    let made = id_of(done["made"][0].as_object().unwrap());
    assert_eq!(w.get("XLog", made).await["what"], "acceptance Accepted", "each effect sees what the ones before it did");
    assert_eq!(w.count(mails).await, mailed + 1, "a mail queued");
    let told = format!("select count(*) from notification where tenant_id = $1 and user_id = '{manager_id}' and title = 'Accepted: insured'");
    assert_eq!(w.count(&told).await, 1, "its owner told");
    let audited =
        format!("select count(*) from audit_event where tenant_id = $1 and action = 'action:accept' and actor = '{approver_id}' and record_id = '{acc}'");
    assert_eq!(w.count(&audited).await, 1, "the audit names who ran it");
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!(
        w.count("select count(*) from custom_record where tenant_id = $1 and entity = 'XLog' and ext->>'what' = 'risk accepted'").await,
        1,
        "the risk's automation ran"
    );
    refused(k.run_action(&approver, "XAcceptance", acc, "accept", note.clone()).await, "invalid", "is not available for this document");

    // An empty link writes nothing; an administrator may run it whatever the roles.
    let alone = new_acceptance(None).await;
    let done = k.run_action(&admin, "XAcceptance", alone, "accept", note.clone()).await.expect("no risk to write");
    assert_eq!(done["status"], "Accepted");

    // A Logic rule the effects break aborts it too.
    let frozen = new_risk("Frozen", 1).await;
    let rule = json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.frozen", "version": "1.0.0", "label": "Frozen",
        "rules": [{"id": "frozen", "entity": "XRisk", "phase": "validate", "check": "doc.title != \"Frozen\"", "message": "a frozen risk stays as it is"}]});
    let saved = k.save_release(&admin, &rule.to_string()).await.unwrap();
    let second = k.create_user(&admin, "admin2@acme.test", "Second", None, &[], true).await.unwrap();
    let second = k.load_ctx(admin.tenant, second, None, "it".into()).await.unwrap();
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&second, release, saved["artifact_digest"].as_str().unwrap(), true, "Read").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    let cold = new_acceptance(Some(frozen)).await;
    refused(k.run_action(&approver, "XAcceptance", cold, "accept", note.clone()).await, "invalid", "a frozen risk stays as it is");
    assert_eq!(w.get("XAcceptance", cold).await["status"], "Draft");

    // ───────────── a portal role allowed it by name, on her own record ─────────────
    let rule = json!({"entity": "XAcceptance", "read": true, "via": "requester", "actions": ["withdraw"]});
    k.set_role_permission(&admin, "Requesters", &serde_json::from_value(rule).unwrap()).await.unwrap();
    k.set_role_portal(&admin, "Requesters", true).await.unwrap();
    let pia = k.create_user(&admin, "pia@home.test", "Pia", None, &[], false).await.unwrap();
    k.set_user_kind(&admin, pia, "portal").await.unwrap();
    k.set_user_roles_until(&admin, pia, &["Requesters".into()], &Default::default()).await.unwrap();
    let pia = k.load_ctx(admin.tenant, pia, None, "it".into()).await.unwrap();
    let hers = new_acceptance(Some(ransom)).await;
    let theirs = id_of(&k.insert(&manager, "XAcceptance", obj(json!({"requester": "ned@home.test"}))).await.unwrap());
    refused(k.run_action(&pia, "XAcceptance", theirs, "withdraw", json!({})).await, "not found", "Acceptance");
    refused(k.run_action(&pia, "XAcceptance", hers, "accept", json!({})).await, "forbidden", "");
    k.run_action(&pia, "XAcceptance", hers, "withdraw", json!({})).await.expect("her own");
    assert_eq!(w.get("XAcceptance", hers).await["status"], "Withdrawn");
    let mine = ferrum_kernel::describe::describe(&pia, pia.schema.entity("XAcceptance").unwrap());
    let may: Vec<(&str, bool)> = mine["actions"].as_array().unwrap().iter().map(|a| (a["name"].as_str().unwrap(), a["may_run"] == json!(true))).collect();
    assert_eq!(may, [("accept", false), ("withdraw", true)], "allowed by name on the entity");

    // ───────────── a packaged entity's x_ action ─────────────
    let shop = id_of(&k.insert(&admin, "Shop", obj(json!({"name": "Corner"}))).await.unwrap());
    k.run_action(&admin, "Shop", shop, "x_rename", json!({})).await.unwrap();
    assert_eq!(w.get("Shop", shop).await["name"], "Corner!");

    // ───────────── carried by a change set and a pack, and run there ─────────────
    let bundle = k.export_customizations(&admin).await.unwrap();
    let ops: Vec<&str> = bundle["customizations"].as_array().unwrap().iter().filter_map(|c| c["op"]["action"]["name"].as_str()).collect();
    assert_eq!(ops, ["x_rename", "accept", "withdraw"]);
    let there = workspace(k, "copy").await;
    k.import_customizations(&there.admin().await, &bundle).await.expect("imported");
    let pack = k.export_pack(&admin, &ExportOptions { name: "risk".into(), label: "Risk".into(), version: 1, records: vec![] }).await.expect("export");
    let packed = workspace(k, "packed").await;
    let padmin = packed.admin().await;
    let plan = k.plan_pack(&padmin, &pack).await.expect("plan");
    k.install_pack(&padmin, &pack, plan["digest"].as_str().unwrap()).await.expect("install");
    for target in [&there, &packed] {
        let a = target.admin().await;
        let r = id_of(&k.insert(&a, "XRisk", obj(json!({"title": "Flood", "owner": "owner@acme.test", "score": 4}))).await.unwrap());
        let x = id_of(&k.insert(&a, "XAcceptance", obj(json!({"risk": r.to_string()}))).await.unwrap());
        k.run_action(&a, "XAcceptance", x, "accept", json!({"note": "there"})).await.expect("runs in the target workspace");
        assert_eq!(target.get("XRisk", r).await["treatment"], "Accept");
    }
}

fn remove_action(entity: &str, name: &str) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": {"op": "remove_action", "name": name}})).unwrap()
}

/// An entity's actions as an administrator sees them: name and label, in order.
fn buttons(ctx: &Ctx, entity: &str) -> Vec<String> {
    let d = ferrum_kernel::describe::describe(ctx, ctx.schema.entity(entity).unwrap());
    d["actions"].as_array().unwrap().iter().map(|a| format!("{}:{}", a["name"].as_str().unwrap(), a["label"].as_str().unwrap())).collect()
}

/// Changing and removing buttons: a replacement keeps its place and runs its own effects, a removal goes and comes
/// back with its switch, a package's stay as they are, and a change set and a pack carry both.
async fn authored(k: &Arc<Kernel>, w: &W) {
    let task = json!({"name": "XTask", "label": "Task", "kind": "master", "fields": [{"name": "title", "type": "data"}, {"name": "state", "type": "data"}]});
    let mark = |name: &str, label: &str, state: &str| json!({"name": name, "label": label, "roles": ["Doer"], "effects": [{"type": "set_fields", "values": {"state": format!("'{state}'")}}]});
    w.batch(vec![define(task), add_action("XTask", mark("x_start", "Start", "Started")), add_action("XTask", mark("x_stop", "Stop", "Stopped"))])
        .await
        .unwrap();
    let admin = w.admin().await;
    let t = id_of(&k.insert(&admin, "XTask", obj(json!({"title": "Paint"}))).await.unwrap());

    // ───────────── replaced where it stands ─────────────
    let begin = json!({"name": "x_start", "label": "Begin", "roles": ["Doer"], "params": [{"name": "why", "label": "Why", "type": "data"}], "effects": [
        {"type": "set_fields", "values": {"title": "doc.title + ' (begun)'"}},
        {"type": "email", "to": "'crew@acme.test'", "subject": "'Begun'", "body": "''", "template": "<p>Begun because {{ params.why }}</p>"}]});
    w.batch(vec![add_action("XTask", begin.clone())]).await.expect("a workspace's action is replaced");
    let refusal = w.batch(vec![add_action("XTask", begin)]).await.expect_err("the same again");
    assert!(refusal.contains("already active"), "{refusal}");
    let admin = w.admin().await;
    assert_eq!(buttons(&admin, "XTask"), ["x_start:Begin", "x_stop:Stop"], "in its place");
    k.run_action(&admin, "XTask", t, "x_start", json!({"why": "spring"})).await.unwrap();
    let now = w.get("XTask", t).await;
    assert_eq!((now["title"].as_str(), now["state"].as_str()), (Some("Paint (begun)"), None), "the new effects ran, the old did not");
    let mailed = "select count(*) from outbox where tenant_id = $1 and topic = 'ferrum.mail' and payload->>'html' like '%Begun because spring%'";
    assert_eq!(w.count(mailed).await, 1, "the mail's template sees params");
    let list = k.list_customizations(&admin).await.unwrap();
    let starts: Vec<_> = list.iter().filter(|r| r.op["action"]["name"] == "x_start").collect();
    assert_eq!(starts.len(), 2);
    assert_eq!((starts[0].superseded_by, starts[1].superseded_by), (Some(starts[1].id), None), "the change list says which applies");

    // ───────────── a package's stays ─────────────
    let lock = json!({"name": "lock", "label": "Mine", "effects": [{"type": "set_fields", "values": {"name": "'x'"}}]});
    for (op, says) in [
        (add_action("Shop", lock), "comes with the package"),
        (remove_action("Shop", "lock"), "comes with the package"),
        (remove_action("XTask", "x_nope"), "no action x_nope"),
    ] {
        let e = w.batch(vec![op]).await.expect_err("refused");
        assert!(e.contains(says), "expected {says:?}, got {e}");
    }

    // ───────────── removed, and back with its switch ─────────────
    w.batch(vec![remove_action("XTask", "x_stop")]).await.unwrap();
    let admin = w.admin().await;
    assert_eq!(buttons(&admin, "XTask"), ["x_start:Begin"]);
    refused(k.run_action(&admin, "XTask", t, "x_stop", json!({})).await, "not found", "action x_stop");
    let removal = k.list_customizations(&admin).await.unwrap().into_iter().find(|r| r.op["op"] == "remove_action").unwrap();
    k.set_customization_active(&admin, removal.id, false).await.unwrap();
    assert_eq!(buttons(&w.admin().await, "XTask"), ["x_start:Begin", "x_stop:Stop"], "switching the removal off brings it back");
    k.set_customization_active(&admin, removal.id, true).await.unwrap();
    let admin = w.admin().await;
    assert_eq!(buttons(&admin, "XTask"), ["x_start:Begin"]);

    // ───────────── carried by a change set and a pack ─────────────
    let bundle = k.export_customizations(&admin).await.unwrap();
    let there = workspace(k, "copy2").await;
    k.import_customizations(&there.admin().await, &bundle).await.expect("imported");
    let pack = k.export_pack(&admin, &ExportOptions { name: "tasks".into(), label: "Tasks".into(), version: 1, records: vec![] }).await.expect("export");
    let packed = workspace(k, "packed2").await;
    let padmin = packed.admin().await;
    let plan = k.plan_pack(&padmin, &pack).await.expect("plan");
    k.install_pack(&padmin, &pack, plan["digest"].as_str().unwrap()).await.expect("install");
    for target in [&there, &packed] {
        let a = target.admin().await;
        assert_eq!(buttons(&a, "XTask"), buttons(&admin, "XTask"));
        assert_eq!(buttons(&a, "Shop"), buttons(&admin, "Shop"));
    }
}
