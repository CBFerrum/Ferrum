//! Documents about a query (docs/builder/decisions.md BD-21), against a real PostgreSQL
//! (`FERRUM_STUDIO_TEST_DATABASE_URL`; the database is reset first): a statement per customer from a saved report over
//! custom entities, one customer's mailed with its file, the bound a person meets, the effect's run going on after its
//! cursor in a second job, and a person who may not read the report's entity refused.

use ferrum_kernel::documents::{MAX_GROUPS, QueryRun};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, DocumentDef, EntityDef, MessageDef, OverlayOp};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

#[derive(Default)]
struct MemFiles(Mutex<std::collections::HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
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

/// A mailer that takes attachments, so a mail with a file may be queued.
struct Mailer;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Mailer {
    async fn send(&self, _to: &str, _subject: &str, _html: &str) -> ferrum_kernel::Result<()> {
        Ok(())
    }
    fn can_attach(&self) -> bool {
        true
    }
}

#[test]
fn statements_per_customer_from_a_report() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_studio::register(&mut hooks);
    let k = Kernel::boot(db, Catalog::from_packages(packages).expect("catalog"), hooks, KernelSettings::default(), Some(Arc::new(Mailer))).await.expect("boot");
    let store = Arc::new(MemFiles::default());
    k.set_file_store(store.clone());
    let tenant = k
        .create_tenant(NewTenant {
            slug: "stmt".into(),
            name: "Statements".into(),
            admin_email: "admin@stmt.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "studio".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("stmt", "admin@stmt.test", "Correct-Horse-9").await.expect("login");
    let admin = || {
        let k = k.clone();
        async move { k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap() }
    };
    let a = admin().await;
    for def in [
        json!({"name": "XCustomer", "label": "Customer", "kind": "master", "title_field": "name", "fields": [
            {"name": "name", "type": "data"}, {"name": "email", "type": "email"}]}),
        json!({"name": "XCharge", "label": "Charge", "kind": "master", "fields": [
            {"name": "customer", "type": "link", "to": "XCustomer"}, {"name": "amount", "type": "decimal"}]}),
    ] {
        let entity: EntityDef = serde_json::from_value(def).unwrap();
        let name = entity.name.clone();
        k.add_customization(&a, &name, OverlayOp::DefineEntity { entity: Box::new(entity) }, None).await.expect("defined");
    }
    let a = admin().await;
    k.insert(
        &a,
        "Report",
        obj(json!({"label": "Charges", "kind": "table", "sharing": "everyone",
        "spec": {"entity": "XCharge", "fields": [{"field": "customer"}, {"field": "amount"}], "sort": [{"by": "amount"}]}})),
    )
    .await
    .expect("report");
    let statement = |group_by: Option<&str>, body: &str| -> DocumentDef {
        serde_json::from_value(json!({"key": if group_by.is_some() { "x.statement" } else { "x.charges" }, "label": "Statement", "entity": "XCharge",
            "output": "text", "query": {"report": "Charges", "group_by": group_by}, "languages": {"en": {"body": body}}}))
        .unwrap()
    };
    // Checked when saved against the report's columns and the grouped record.
    for (def, says) in [
        (statement(Some("customer"), "{{ group.colour }}"), "undefined"),
        (statement(Some("amount"), "x"), "links to no record"),
        (statement(Some("nope"), "x"), "has no column nope"),
        (statement(None, "{% for r in rows %}{{ r.nope }}{% endfor %}"), "undefined"),
        (
            serde_json::from_value(
                json!({"key": "x.other", "entity": "XCustomer", "output": "text", "query": {"report": "Charges"}, "languages": {"en": {"body": "x"}}}),
            )
            .unwrap(),
            "reads XCharge, not XCustomer",
        ),
        (
            serde_json::from_value(
                json!({"key": "x.gone", "entity": "XCharge", "output": "text", "query": {"report": "Gone"}, "languages": {"en": {"body": "x"}}}),
            )
            .unwrap(),
            "there is no report Gone",
        ),
    ] {
        let e = k.save_document_template(&a, &def).await.expect_err(says);
        assert!(format!("{e:?}").contains(says), "{says}: {e:?}");
    }
    k.save_document_template(&a, &statement(Some("customer"), "{{ group.name }}|{% for r in rows %}{{ r.amount }};{% endfor %}|{{ rows|total('amount') }}"))
        .await
        .expect("statement");
    k.save_document_template(&a, &statement(None, "{% for r in rows %}{{ r.amount }};{% endfor %}")).await.expect("whole result");
    let mail: MessageDef = serde_json::from_value(json!({"key": "x.statement_mail", "entity": "XCustomer",
        "languages": {"en": {"email": {"subject": "Your statement", "body": "<p>Dear {{ doc.name }}</p>"}}}}))
    .unwrap();
    k.save_message_template(&a, &mail).await.expect("message");

    let customer = |name: &str| obj(json!({"name": name, "email": format!("{}@stmt.test", name.to_lowercase())}));
    let ann = k.insert(&a, "XCustomer", customer("Ann")).await.unwrap();
    let bob = k.insert(&a, "XCustomer", customer("Bob")).await.unwrap();
    for (who, amount) in [(&ann, "10.50"), (&ann, "2.25"), (&bob, "7")] {
        k.insert(&a, "XCharge", obj(json!({"customer": who["id"], "amount": amount}))).await.unwrap();
    }
    k.insert(&a, "XCharge", obj(json!({"amount": "99"}))).await.unwrap();
    // Checked when saved however many rows the report has: it compiles, and the report is there to be run.
    let mut again = statement(None, "{% for r in rows %}{{ r.amount }}|{% endfor %}");
    again.key = "x.again".into();
    k.save_document_template(&a, &again).await.expect("saved over a report with rows");
    let text = |file: Uuid| String::from_utf8(store.0.lock().unwrap().get(&format!("tenants/{tenant}/{file}")).cloned().unwrap()).unwrap();

    // ───────────── one file for the whole result; one per customer, kept on each ─────────────
    let whole = k.render_query_document(&a, &QueryRun { template: "x.charges".into(), ..Default::default() }).await.expect("one file");
    assert_eq!(String::from_utf8(whole.bytes).unwrap(), "2.25;7;10.5;99;");
    let run = QueryRun { template: "x.statement".into(), ..Default::default() };
    let made = k.generate_query_documents(&a, &run).await.expect("statements");
    assert_eq!(made.next, None);
    let by: Vec<(Value, String)> = made.files.iter().map(|f| (json!(f.record.to_string()), text(f.file))).collect();
    let want = [(ann["id"].clone(), "Ann|2.25;10.5;|12.75".to_string()), (bob["id"].clone(), "Bob|7;|7".to_string())];
    let mut by = by;
    by.sort_by_key(|p| p.1.clone());
    assert_eq!(by, want, "each customer's own rows and total, the charge with no customer in none");
    assert!(made.files.iter().all(|f| f.entity == "XCustomer" && f.template == "x.statement"));
    // Again: each customer's statement replaced, not added.
    k.generate_query_documents(&a, &run).await.unwrap();
    let kept = |n: i64| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let c: i64 = sqlx::query_scalar("select count(*) from file_object where document = 'x.statement'").fetch_one(&mut *tx).await.unwrap();
            assert_eq!(c, n, "statements kept");
        }
    };
    kept(2).await;

    // ───────────── a person who may not read the report's entity ─────────────
    let clerk = k.create_user(&a, "clerk@stmt.test", "Clerk", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let e = k.generate_query_documents(&clerk, &run).await.expect_err("not theirs to read");
    assert!(matches!(e, Error::Forbidden(_)), "{e:?}");

    // ───────────── each customer sent their statement: one configured action, in turns ─────────────
    k.add_customization(
        &a,
        "XCustomer",
        OverlayOp::AddAction {
            action: serde_json::from_value(json!({"name": "x_send_statements", "label": "Send statements", "effects": [
                {"type": "report_documents", "template": "x.statement", "message": {"template": "x.statement_mail", "to": "field:email", "channels": ["email"]}}]}))
            .unwrap(),
        },
        None,
    )
    .await
    .expect("the button");
    let a = admin().await;
    for i in 0..(MAX_GROUPS - 1) {
        let c = k.insert(&a, "XCustomer", customer(&format!("C{i:03}"))).await.unwrap();
        k.insert(&a, "XCharge", obj(json!({"customer": c["id"], "amount": "1"}))).await.unwrap();
    }
    // 101 customers now: more than a person's one run makes.
    let e = k.generate_query_documents(&a, &run).await.expect_err("over the bound");
    assert!(e.to_string().contains(&format!("rows for {} records of Customer, more than the {MAX_GROUPS}", MAX_GROUPS + 1)), "{e}");
    kept(2).await;
    let out = k.run_action(&a, "XCustomer", ann["id"].as_str().unwrap().parse().unwrap(), "x_send_statements", Value::Null).await.expect("queued");
    assert!(out.is_object());
    let sent = || {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let n: i64 = sqlx::query_scalar("select count(*) from message_send where template = 'x.statement_mail' and state = 'queued'")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            n
        }
    };
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
    assert_eq!(sent().await, MAX_GROUPS as i64, "the first job did as many as one does");
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
    assert_eq!(sent().await, MAX_GROUPS as i64 + 1, "the second went on after the first's last record");
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 0, "and queued no third");
    kept(MAX_GROUPS as i64 + 1).await;
    // Ann's mail carries her statement.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let (file, payload): (Uuid, Value) = sqlx::query_as(
        "select f.id, o.payload from file_object f join message_send s on s.record_id = f.record_id and s.template = 'x.statement_mail' \
         join outbox o on o.topic = 'ferrum.mail' and (o.payload->>'send')::uuid = s.id where f.document = 'x.statement' and f.record_id = $1",
    )
    .bind(ann["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .fetch_one(&mut *tx)
    .await
    .expect("her mail");
    assert_eq!(payload["to"], json!("ann@stmt.test"), "{payload}");
    assert_eq!(payload["files"], json!([file.to_string()]), "{payload}");
    assert_eq!(text(file), "Ann|2.25;10.5;|12.75");
    drop(tx);

    // ───────────── a button that asks for the period; one document per linked record ─────────────
    let doc: DocumentDef =
        serde_json::from_value(json!({"key": "x.charge_note", "entity": "XCharge", "output": "text", "languages": {"en": {"body": "{{ doc.amount }}"}}}))
            .unwrap();
    k.save_document_template(&a, &doc).await.expect("a document per charge");
    let button = |name: &str, effects: Value| OverlayOp::AddAction {
        action: serde_json::from_value(json!({"name": name, "label": name, "params": [
            {"name": "from", "label": "From", "type": "date"}, {"name": "to", "label": "To", "type": "date"}, {"name": "all", "label": "All", "type": "check"}],
            "effects": effects}))
        .unwrap(),
    };
    k.add_customization(
        &a,
        "XCustomer",
        button(
            "x_statements_for",
            json!([{"type": "report_documents", "template": "x.statement", "params": {"filters": []},
            "params_from": {"from": "params.from", "to": "params.to"}}]),
        ),
        None,
    )
    .await
    .expect("the period asked for");
    k.add_customization(
        &a,
        "XCustomer",
        button(
            "x_notes",
            json!([{"type": "for_each", "entity": "XCharge", "link": "customer", "effects": [
            {"type": "generate", "template": "x.charge_note", "record": "item.id"}]}]),
        ),
        None,
    )
    .await
    .expect("a document per charge of the customer");
    let a = admin().await;
    let ann_id: Uuid = ann["id"].as_str().unwrap().parse().unwrap();
    k.run_action(&a, "XCustomer", ann_id, "x_statements_for", json!({"from": "2026-01-01", "to": "2026-03-31", "all": true})).await.expect("queued");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let run: Value = sqlx::query_scalar("select payload->'run'->'params' from job where kind = 'platform.document.groups' order by created_at desc limit 1")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(run, json!({"filters": [], "from": "2026-01-01", "to": "2026-03-31"}));
    let made = k.run_action(&a, "XCustomer", ann_id, "x_notes", json!({})).await.expect("made");
    let notes: Vec<String> = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let ids: Vec<Uuid> =
            sqlx::query_scalar("select id from file_object where document = 'x.charge_note' order by created_at").fetch_all(&mut *tx).await.unwrap();
        ids.into_iter().map(text).collect()
    };
    assert_eq!(notes.len(), 2, "{made}");
    assert!(notes.contains(&"10.5".to_string()) && notes.contains(&"2.25".to_string()), "{notes:?}");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let bobs = k.find_tx(&a, &mut tx, "XCharge", &[ferrum_kernel::query::Filter::eq("customer", bob["id"].as_str().unwrap())], None, 1).await.unwrap();
    drop(tx);
    let bad = json!([{"type": "generate", "template": "x.charge_note", "record": format!("'{}'", bobs[0]["id"].as_str().unwrap())}]);
    k.add_customization(&a, "XCustomer", button("x_bad", bad), None).await.expect("saved: what it names is known when it runs");
    let a = admin().await;
    let e = k.run_action(&a, "XCustomer", ann_id, "x_bad", json!({})).await.expect_err("not a record this run reaches");
    assert!(e.to_string().contains("neither this record nor"), "{e}");

    // ───────────── a report shared with a role, in one pack; the document about it in a second ─────────────
    // The report is read as the person who renders, by its label, with the roles it is shared with.
    let pack_of = |extra: Value| {
        let mut p = json!({"format": "ferrum.pack/1", "name": "tidy", "label": "Tidy", "version": 1});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        p
    };
    let first = pack_of(json!({"roles": [{"name": "Charge Clerk", "permissions": [{"entity": "XCharge", "read": true, "export": true}]},
        {"name": "Charge Reader", "permissions": [{"entity": "XCharge", "read": true, "export": true}]}],
        "studio": [{"entity": "Report", "name": "Clerk charges", "record": {"label": "Clerk charges", "kind": "table", "sharing": "roles",
            "roles": [{"role": "Charge Clerk"}], "spec": {"entity": "XCharge", "fields": [{"field": "amount"}], "sort": [{"by": "amount"}]}}}]}));
    let second = pack_of(json!({"documents": [{"key": "x.clerk", "label": "Clerk", "entity": "XCharge", "output": "text", "roles": ["Charge Clerk"],
        "query": {"report": "Clerk charges"}, "languages": {"en": {"body": "{% for r in rows %}{{ r.amount }};{% endfor %}"}}},
        {"key": "x.clerk_open", "label": "Clerk, for anyone", "entity": "XCharge", "output": "text",
        "query": {"report": "Clerk charges"}, "languages": {"en": {"body": "{% for r in rows %}{{ r.amount }};{% endfor %}"}}}]}));
    for pack in [&first, &second] {
        let plan = k.plan_pack(&a, pack).await.expect("planned");
        k.install_pack(&a, pack, plan["digest"].as_str().unwrap()).await.expect("installed, the second over the first's committed report");
    }
    let clerk = k.create_user(&a, "charges@stmt.test", "Clerk", Some("Correct-Horse-9"), &["Charge Clerk".to_string()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    // Someone who may export the charges but holds no role the report is shared with is told what a missing report says.
    let reader = k.create_user(&a, "reader@stmt.test", "Reader", Some("Correct-Horse-9"), &["Charge Reader".to_string()], false).await.unwrap();
    let reader = k.load_ctx(tenant, reader, None, "it".into()).await.unwrap();
    let e = k.render_query_document(&reader, &QueryRun { template: "x.clerk_open".into(), ..Default::default() }).await.expect_err("not shared with them");
    assert!(e.to_string().contains("there is no report Clerk charges"), "{e}");
    k.render_query_document(&clerk, &QueryRun { template: "x.clerk_open".into(), ..Default::default() }).await.expect("their role is the report's");
    let made = k.render_query_document(&clerk, &QueryRun { template: "x.clerk".into(), ..Default::default() }).await.expect("a report shared with their role");
    let text = String::from_utf8(made.bytes).unwrap();
    assert_eq!(text.matches(';').count(), MAX_GROUPS + 3, "every row, not the one the save looked at: {text}");
    assert!(text.ends_with("2.25;7;10.5;99;"), "{text}");

    // ───────────── a pack with the report and the documents about it installs in one go ─────────────
    let opts = ferrum_kernel::pack::ExportOptions { name: "statements".into(), label: "Statements".into(), version: 1, records: vec![] };
    let pack = k.export_pack(&a, &opts).await.expect("exported");
    assert!(pack["documents"].as_array().is_some_and(|d| d.len() == 6), "{}", pack["documents"]);
    let other = k
        .create_tenant(NewTenant {
            slug: "stmt2".into(),
            name: "Statements Too".into(),
            admin_email: "admin@stmt2.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "studio".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("stmt2", "admin@stmt2.test", "Correct-Horse-9").await.expect("login");
    let b = k.load_ctx(other, session.user, None, "it".into()).await.unwrap();
    let plan = k.plan_pack(&b, &pack).await.expect("planned");
    k.install_pack(&b, &pack, plan["digest"].as_str().unwrap()).await.expect("the report and its documents in one install");
    let b = k.load_ctx(other, session.user, None, "it".into()).await.unwrap();
    assert!(k.document_template(&b, "x.statement").await.is_ok());
}
