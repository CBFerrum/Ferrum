//! Rules that count and sum other records (docs/logic/rules.md, aggregates), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; reset first): on packaged entities and on one defined through customization,
//! read in the save's transaction with the saver's rights, exact to the last digit, and a release kept before
//! rules could aggregate still running.

use ferrum_kernel::registry::Registry;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp, PermissionDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

const PACKAGE: &str = "package: club\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Visit, read: true, write: true, create: true}\n      - {entity: Member, read: true}\n      - {entity: Charge, read: true}\n  - name: Limited\n    permissions:\n      - {entity: Visit, read: true, write: true, create: true}\n      - {entity: Member, read: true}\n      - {entity: Charge, read: true, create: true, if_owner: true}\n  - name: Blind\n    permissions:\n      - {entity: Visit, read: true, write: true, create: true}\n      - {entity: Member, read: true}\n";
const ENTITIES: &str = r#"
- name: Member
  label: Member
  kind: master
  title_field: member_name
  fields:
    - {name: member_name, type: data, required: true}
- name: Membership
  label: Membership
  kind: document
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: member, type: link, to: Member}
    - {name: status, type: data}
- name: Charge
  label: Charge
  kind: document
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: member, type: link, to: Member}
    - {name: membership, type: link, to: Membership}
    - {name: amount, type: decimal}
    - {name: status, type: data}
- name: Visit
  label: Visit
  kind: document
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: member, type: link, to: Member}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

/// Unpaid charges of the visit's member, as a rule names them.
fn unpaid(name: &str) -> Value {
    json!({"name": name, "entity": "Charge", "link": "member", "to": "member", "filter": [["status", "=", "Unpaid"]], "op": "count"})
}

fn solution(version: &str, extra: Value) -> String {
    let mut rules = json!([
        // `to` a link, with a filter, over a packaged entity and a custom one.
        {"id": "unpaid", "entity": "Visit", "phase": "validate", "when": "!coalesce(doc.x_waived, false)", "field": "member",
         "message": "This member has unpaid charges",
         "aggregates": [unpaid("unpaid"), {"name": "fees", "entity": "XFee", "link": "member", "to": "member", "filter": [["status", "=", "Unpaid"]], "op": "exists"}],
         "check": "aggregates.unpaid == 0 && !aggregates.fees"},
        // What the saver could count, written down.
        {"id": "seen", "entity": "Visit", "phase": "derive", "aggregates": [unpaid("count")], "writes": {"x_unpaid": "aggregates.count"}},
        // `to: self`: no second open membership for this member.
        {"id": "one_open", "entity": "Member", "phase": "validate", "message": "A member has one open membership",
         "aggregates": [{"name": "open", "entity": "Membership", "link": "member", "filter": [["status", "=", "Open"]], "op": "count"}],
         "check": "aggregates.open <= 1"},
        // A derivation writing a sum.
        {"id": "balance", "entity": "Membership", "phase": "derive",
         "aggregates": [{"name": "owed", "entity": "Charge", "link": "membership", "to": "self", "op": "sum", "field": "amount"}],
         "writes": {"x_balance": "aggregates.owed"}},
        // A rule on the custom entity, to the last digit.
        {"id": "fee_cap", "entity": "XFee", "phase": "validate", "message": "This member owes too much already",
         "aggregates": [{"name": "owed", "entity": "Charge", "link": "member", "to": "member", "filter": [["status", "=", "Unpaid"]], "op": "sum", "field": "amount"}],
         "check": "aggregates.owed + doc.amount <= dec(\"100.3\")"}
    ]);
    rules.as_array_mut().unwrap().extend(extra.as_array().cloned().unwrap_or_default());
    json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.club", "version": version, "label": "Club rules", "rules": rules}).to_string()
}

#[test]
fn rules_count_and_sum_other_records() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn user(k: &Kernel, admin: &Ctx, email: &str, role: &str, is_admin: bool) -> Uuid {
    k.create_user(admin, email, role, Some("Correct-Horse-9"), &[role.to_string()], is_admin).await.unwrap()
}

async fn ctx(k: &Kernel, tenant: Uuid, user: Uuid) -> Ctx {
    k.load_ctx(tenant, user, None, "it".into()).await.unwrap()
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/club.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "club".into(),
            name: "club".into(),
            admin_email: "admin@club.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["club".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("club", "admin@club.test", "Correct-Horse-9").await.unwrap();
    let admin = ctx(&k, tenant, session.user).await;

    // A release kept before rules could aggregate: its stored form, written by hand, has no such key.
    let legacy_rule = json!({"id": "legacy", "entity": "Visit", "phase": "validate", "check": "doc.reference != \"LEGACY\"", "message": "The legacy rule ran"});
    let legacy_source = json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.legacy", "version": "1.0.0", "label": "Legacy",
        "requires": [], "parameters": {}, "rules": [legacy_rule], "decisions": [], "processes": [], "tests": [], "ui": {"layout": {}}});
    let legacy_release = json!({"solution": "acme.legacy", "version": "1.0.0", "compiler": "ferrum-logic/0.1.0", "profile": "core",
        "source_digest": "sha256:before", "artifact_digest": "sha256:before", "lock": [], "fields": {"Visit.reference": "Text"},
        "ir": {"parameters": {}, "rules": [legacy_rule], "decisions": [], "processes": [], "types": {}}, "notes": []});
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let legacy: Uuid = sqlx::query_scalar(
        "insert into logic_release (tenant_id, solution, version, source_digest, artifact_digest, compiler, profile, source, release, created_by) \
         values ($1, 'acme.legacy', '1.0.0', 'sha256:before', 'sha256:before', 'ferrum-logic/0.1.0', 'core', $2, $3, $4) returning id",
    )
    .bind(tenant)
    .bind(&legacy_source)
    .bind(&legacy_release)
    .bind(admin.user)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("insert into logic_activation (tenant_id, solution, release_id, activated_by) values ($1, 'acme.legacy', $2, $3)")
        .bind(tenant)
        .bind(legacy)
        .bind(admin.user)
        .execute(&mut *tx)
        .await
        .unwrap();
    Registry::bump_version(&mut tx, tenant).await.unwrap();
    tx.commit().await.unwrap();
    k.registry.invalidate(tenant);

    // The customer's own fields, and an entity of their own.
    for (entity, name, ty) in [("Visit", "x_unpaid", "int"), ("Visit", "x_waived", "check"), ("Membership", "x_balance", "decimal")] {
        let op: OverlayOp = serde_json::from_value(json!({"op": "add_field", "field": {"name": name, "type": ty}})).unwrap();
        k.add_customization(&admin, entity, op, None).await.unwrap();
    }
    let fee: OverlayOp = serde_json::from_value(json!({"op": "define_entity", "entity": {"name": "XFee", "label": "Fee", "kind": "document",
        "fields": [{"name": "member", "type": "link", "to": "Member"}, {"name": "amount", "type": "decimal"}, {"name": "status", "type": "data"}]}}))
    .unwrap();
    k.add_customization(&admin, "XFee", fee, None).await.unwrap();
    let reviewer = user(&k, &admin, "reviewer@club.test", "Clerk", true).await;
    let clerk = user(&k, &admin, "clerk@club.test", "Clerk", false).await;
    let limited = user(&k, &admin, "limited@club.test", "Limited", false).await;
    let blind = user(&k, &admin, "blind@club.test", "Blind", false).await;
    let scoped = user(&k, &admin, "scoped@club.test", "Limited", false).await;
    let admin = ctx(&k, tenant, admin.user).await;
    k.set_role_permission(&admin, "Clerk", &PermissionDef { entity: "XFee".into(), read: true, ..Default::default() }).await.unwrap();
    let (admin, reviewer, clerk, limited, blind, scoped) = (
        ctx(&k, tenant, admin.user).await,
        ctx(&k, tenant, reviewer).await,
        ctx(&k, tenant, clerk).await,
        ctx(&k, tenant, limited).await,
        ctx(&k, tenant, blind).await,
        ctx(&k, tenant, scoped).await,
    );
    let e = k.insert(&clerk, "Visit", obj(json!({"reference": "LEGACY"}))).await.unwrap_err();
    assert!(message(&e).contains("The legacy rule ran"), "a release stored before aggregates still loads and runs: {e}");

    // Compiled against this workspace: a fifth aggregate is refused, with a pointer.
    let five: Vec<Value> = (0..5).map(|i| unpaid(&format!("u{i}"))).collect();
    let bad = solution("0.9.0", json!([{"id": "five", "entity": "Visit", "phase": "validate", "message": "m", "check": "true", "aggregates": five}]));
    let faults = k.compile_solution(&admin, &bad).await.unwrap().unwrap_err();
    assert!(faults.iter().any(|d| d.pointer == "/rules/5/aggregates" && d.message.contains("at most 4")), "{faults:?}");
    let wrong = solution(
        "0.9.0",
        json!([{"id": "wrong", "entity": "Visit", "phase": "validate", "message": "m", "check": "true",
        "aggregates": [{"name": "x", "entity": "Charge", "link": "membership", "to": "member", "op": "count"}]}]),
    );
    let faults = k.compile_solution(&admin, &wrong).await.unwrap().unwrap_err();
    assert!(faults.iter().any(|d| d.message.contains("Charge.membership is not a link to Member")), "{faults:?}");

    let saved = k.save_release(&admin, &solution("1.0.0", json!([]))).await.unwrap();
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&reviewer, release, saved["artifact_digest"].as_str().unwrap(), true, "Counts look right").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();

    let member = |name: &'static str| {
        let (k, admin) = (&k, &admin);
        async move { k.insert(admin, "Member", obj(json!({"member_name": name}))).await.unwrap()["id"].as_str().unwrap().to_string() }
    };
    let (asha, bala, chen) = (member("Asha").await, member("Bala").await, member("Chen").await);

    // A membership being created has nothing pointing at it: its balance is zero. Once charged, saving it
    // again sums them exactly: 0.1 + 0.2 is 0.3, not 0.30000000000000004.
    let m = k.insert(&admin, "Membership", obj(json!({"reference": "M-1", "member": asha, "status": "Open"}))).await.unwrap();
    assert_eq!(m["x_balance"], json!("0"), "{m:?}");
    let m_id: Uuid = m["id"].as_str().unwrap().parse().unwrap();
    for (r, amount, status) in [("C-1", "0.1", "Unpaid"), ("C-2", "0.2", "Unpaid"), ("C-3", "7", "Paid")] {
        k.insert(&admin, "Charge", obj(json!({"reference": r, "member": asha, "membership": m_id.to_string(), "amount": amount, "status": status})))
            .await
            .unwrap();
    }
    let m = k.update(&admin, "Membership", m_id, obj(json!({"status": "Open"})), None).await.unwrap();
    assert_eq!(m["x_balance"], json!("7.3"), "{m:?}");

    // `to` a link with a filter: Asha's unpaid charges refuse her visit, whoever counts them all.
    let e = k.insert(&clerk, "Visit", obj(json!({"reference": "V-1", "member": asha}))).await.unwrap_err();
    assert!(message(&e).starts_with("member: This member has unpaid charges [acme.club 1.0.0, rule unpaid]"), "{}", message(&e));
    let waived = k.insert(&clerk, "Visit", obj(json!({"reference": "V-2", "member": asha, "x_waived": true}))).await.unwrap();
    assert_eq!(waived["x_unpaid"], json!(2));
    // Bala owes nothing; a visit without a member points at nobody.
    assert_eq!(k.insert(&clerk, "Visit", obj(json!({"reference": "V-3", "member": bala}))).await.unwrap()["x_unpaid"], json!(0));
    assert_eq!(k.insert(&clerk, "Visit", obj(json!({"reference": "V-4"}))).await.unwrap()["x_unpaid"], json!(0), "an empty link counts nothing");

    // An aggregate reads as the workspace: every matching charge counts whoever saves. Limited sees only the
    // charge they made, Blind may read no charge at all, Scoped (row-scoped to their own, and they made none)
    // sees none of them: all count both, and none of them passes the check.
    k.insert(&limited, "Charge", obj(json!({"reference": "C-4", "member": bala, "amount": "1", "status": "Unpaid"}))).await.unwrap();
    k.insert(&admin, "Charge", obj(json!({"reference": "C-5", "member": bala, "amount": "1", "status": "Unpaid"}))).await.unwrap();
    let seen = |c: &Ctx, r: &'static str| {
        let (k, c, bala) = (&k, c.clone(), bala.clone());
        async move { k.insert(&c, "Visit", obj(json!({"reference": r, "member": bala, "x_waived": true}))).await.unwrap()["x_unpaid"].clone() }
    };
    assert_eq!(seen(&clerk, "V-5").await, json!(2));
    assert_eq!(seen(&limited, "V-6").await, json!(2), "the charge Limited may not see counts too");
    assert_eq!(seen(&blind, "V-7").await, json!(2), "and for Blind, who may read no charge");
    assert_eq!(k.list(&scoped, "Charge", &Default::default()).await.unwrap().rows.len(), 0, "Scoped sees no charge");
    for (who, r) in [(&limited, "V-8"), (&blind, "V-9"), (&scoped, "V-9b")] {
        let e = k.insert(who, "Visit", obj(json!({"reference": r, "member": bala}))).await.unwrap_err();
        assert!(message(&e).contains("unpaid charges"), "{} is refused like anyone: {}", who.email, message(&e));
    }

    // `to: self`: a second open membership refuses the member's next save; a closed one does not count.
    k.insert(&admin, "Membership", obj(json!({"reference": "M-2", "member": chen, "status": "Open"}))).await.unwrap();
    k.insert(&admin, "Membership", obj(json!({"reference": "M-3", "member": chen, "status": "Closed"}))).await.unwrap();
    let chen_id: Uuid = chen.parse().unwrap();
    k.update(&admin, "Member", chen_id, obj(json!({"member_name": "Chen L"})), None).await.expect("one open");
    let second = k.insert(&admin, "Membership", obj(json!({"reference": "M-4", "member": chen, "status": "Open"}))).await.unwrap();
    let e = k.update(&admin, "Member", chen_id, obj(json!({"member_name": "Chen Li"})), None).await.unwrap_err();
    assert!(message(&e).contains("A member has one open membership"), "{}", message(&e));
    k.update(&admin, "Membership", second["id"].as_str().unwrap().parse().unwrap(), obj(json!({"status": "Closed"})), None).await.unwrap();
    k.update(&admin, "Member", chen_id, obj(json!({"member_name": "Chen Li"})), None).await.expect("one open again");

    // On the custom entity, to the last digit: Asha owes 0.1 + 0.2, so a fee of 100 reaches 100.3 exactly.
    k.insert(&admin, "XFee", obj(json!({"member": asha, "amount": "100", "status": "Paid"}))).await.expect("100.3 is within 100.3");
    let e = k.insert(&admin, "XFee", obj(json!({"member": asha, "amount": "100.01", "status": "Paid"}))).await.unwrap_err();
    assert!(message(&e).contains("owes too much"), "{}", message(&e));
    // And counted from the other side: an unpaid fee refuses Chen's visit, whose charges are none.
    k.insert(&clerk, "Visit", obj(json!({"reference": "V-10", "member": chen}))).await.expect("Chen owes nothing yet");
    k.insert(&admin, "XFee", obj(json!({"member": chen, "amount": "5", "status": "Unpaid"}))).await.unwrap();
    let e = k.insert(&clerk, "Visit", obj(json!({"reference": "V-11", "member": chen}))).await.unwrap_err();
    assert!(message(&e).contains("unpaid charges"), "an unpaid fee of the custom entity counts: {}", message(&e));
}
