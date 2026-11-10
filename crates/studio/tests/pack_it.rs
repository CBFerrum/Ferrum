//! Packs (docs/api.md, "Packs"; docs/builder/decisions.md BD-07, BF-PACK-001, BF-PACK-004), against a real
//! PostgreSQL (`FERRUM_STUDIO_TEST_DATABASE_URL`; the database is reset first). A gym is built by
//! configuration, with the calls an administrator makes: a membership with lines, a key, a total, a check and
//! a workflow; a coach's role and a members' portal role; an automation on each new membership and a daily
//! one with a mail template; Studio's report, chart, card, dashboard and space; a solution whose rule counts;
//! and three plans as reference records. It is exported, planned and installed in a fresh workspace, where
//! everything works as it did; installed again, changing nothing; changed and installed once more; and every
//! way a pack is refused is tried. It lives with Studio because Studio's items need Studio's hooks.

use chrono::{Duration, Utc};
use ferrum_kernel::pack::{ExportOptions, MAX_PACK};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, automation};
use ferrum_meta::{Catalog, OverlayOp, PermissionDef};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

fn membership_def() -> Value {
    json!({"name": "XMembership", "label": "Membership", "kind": "master", "title_field": "code", "fields": [
        {"name": "code", "type": "data", "unique": true},
        {"name": "member_email", "type": "email"},
        {"name": "plan", "type": "link", "to": "XPlan"},
        {"name": "status", "type": "data"},
        {"name": "total", "type": "currency", "rollup": {"table": "lines", "op": "sum", "field": "amount"}},
        {"name": "lines", "type": "table", "of": "XMembershipLine"}]})
}

fn solution() -> String {
    json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "gym.limits", "version": "1.0.0", "label": "Gym limits", "rules": [
        {"id": "plan_full", "entity": "XMembership", "phase": "validate", "field": "plan", "message": "This plan is full",
         "aggregates": [{"name": "active", "entity": "XMembership", "link": "plan", "to": "plan", "filter": [["status", "=", "Active"]], "op": "count"}],
         "check": "doc.status != \"Active\" || aggregates.active < 2"}]})
    .to_string()
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
    async fn ctx(&self, user: Uuid) -> Ctx {
        self.k.load_ctx(self.tenant, user, None, "it".into()).await.unwrap()
    }
    async fn count(&self, sql: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }
    async fn text(&self, sql: &str) -> Option<String> {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let v: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_optional(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        v
    }
    async fn mails(&self, subject: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar("select count(*) from outbox where tenant_id = $1 and topic = 'ferrum.mail' and payload->>'subject' = $2")
            .bind(self.tenant)
            .bind(subject)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    }
    /// What a second install would duplicate, counted.
    async fn counts(&self) -> Vec<i64> {
        let mut out = vec![];
        for sql in [
            "select count(*) from meta_overlay where tenant_id = $1",
            "select count(*) from role where tenant_id = $1",
            "select count(*) from role_permission where tenant_id = $1",
            "select count(*) from automation where tenant_id = $1",
            "select count(*) from e_report where tenant_id = $1",
            "select count(*) from e_dashboard_chart where tenant_id = $1",
            "select count(*) from e_number_card where tenant_id = $1",
            "select count(*) from e_dashboard where tenant_id = $1",
            "select count(*) from e_space where tenant_id = $1",
            "select count(*) from custom_record where tenant_id = $1 and entity = 'XPlan'",
            "select count(*) from custom_record where tenant_id = $1 and entity = 'XMembership'",
            "select count(*) from logic_draft where tenant_id = $1",
        ] {
            out.push(self.count(sql).await);
        }
        out
    }
    async fn customize(&self, entity: &str, op: Value) {
        let op: OverlayOp = serde_json::from_value(op).unwrap();
        self.k.add_customization(&self.admin().await, entity, op, None).await.unwrap_or_else(|e| panic!("{entity}: {e}"));
    }
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("{entity}: {}", message(&e)))
    }
}

async fn workspace(k: &Arc<Kernel>, slug: &str, packages: &[&str]) -> W {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: packages.iter().map(|p| p.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user }
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

fn export(records: &[&str]) -> ExportOptions {
    ExportOptions { name: "gym".into(), label: "Gym".into(), version: 1, records: records.iter().map(|r| r.to_string()).collect() }
}

fn names(plan: &Value, section: &str, status: &str) -> Vec<String> {
    let mut v: Vec<String> =
        plan["sections"][section][status].as_array().cloned().unwrap_or_default().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    v.sort();
    v
}

/// Every section of a plan or summary leaves everything as it is.
fn nothing_to_do(plan: &Value) {
    for (section, s) in plan.as_object().unwrap() {
        assert!(s["created"].as_array().unwrap().is_empty() && s["changed"].as_array().unwrap().is_empty(), "{section}: {s}");
    }
}

#[test]
fn a_gym_travels_as_a_pack() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let mut packages = vec![core];
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let mut hooks = HookRegistry::default();
    ferrum_studio::register(&mut hooks);
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");

    // ───────────── a gym, built by configuration ─────────────
    let src = workspace(&k, "source", &["core", "studio"]).await;
    src.customize(
        "XPlan",
        json!({"op": "define_entity", "entity": {"name": "XPlan", "label": "Plan", "kind": "master", "title_field": "name", "fields": [
        {"name": "name", "type": "data", "unique": true}, {"name": "price", "type": "currency"}, {"name": "sessions", "type": "int"}]}}),
    )
    .await;
    src.customize(
        "XMembershipLine",
        json!({"op": "define_entity", "entity": {"name": "XMembershipLine", "label": "Membership Line", "kind": "child", "fields": [
        {"name": "item", "type": "data", "required": true}, {"name": "amount", "type": "currency"}]}}),
    )
    .await;
    src.customize("XMembership", json!({"op": "define_entity", "entity": membership_def()})).await;
    src.customize(
        "XMembership",
        json!({"op": "add_rule", "rule": {"name": "affordable", "check": "doc.total <= 1000.0", "message": "A membership costs at most 1000"}}),
    )
    .await;
    src.customize(
        "XMembership",
        json!({"op": "set_workflow", "workflow": {"name": "Membership", "entity": "XMembership", "initial_state": "Draft",
        "states": [{"name": "Draft"}, {"name": "Active"}], "transitions": [{"action": "Activate", "from": "Draft", "to": "Active", "roles": ["Coach"]}]}}),
    )
    .await;
    let admin = src.admin().await;
    let rule = |v: Value| -> PermissionDef { serde_json::from_value(v).unwrap() };
    k.set_role_permission(&admin, "Coach", &rule(json!({"entity": "XMembership", "read": true, "write": true, "create": true}))).await.unwrap();
    k.set_role_permission(&admin, "Coach", &rule(json!({"entity": "XPlan", "read": true}))).await.unwrap();
    k.set_role_portal(&admin, "Members", true).await.expect_err("no such role yet");
    k.set_role_permission(&admin, "Members", &rule(json!({"entity": "XPlan", "read": true}))).await.unwrap();
    k.set_role_portal(&admin, "Members", true).await.unwrap();
    k.set_role_permission(&admin, "Members", &rule(json!({"entity": "XMembership", "read": true, "via": "member_email"}))).await.unwrap();
    let welcome = |condition: Option<&str>| {
        obj(json!({"name": "Welcome", "entity": "XMembership", "event": "created", "condition": condition,
            "actions": [{"type": "email", "to": "'desk@gym.test'", "subject": "'Welcome ' + doc.code", "body": "doc.code"}]}))
    };
    automation::save(&k, &admin, &welcome(None)).await.unwrap();
    automation::save(
        &k,
        &admin,
        &obj(json!({"name": "Daily plan", "entity": "XPlan", "event": "scheduled", "schedule": {"every": {"period": "day", "at": "06:00"}},
        "actions": [{"type": "email", "to": "'desk@gym.test'", "subject": "'Plan ' + doc.name", "body": "''", "template": "<p>{{ doc.name }} today</p>"}]})),
    )
    .await
    .unwrap();
    for (name, price, sessions) in [("Basic", "20", 4), ("Gold", "50", 12), ("Platinum", "90", 30)] {
        src.insert(&admin, "XPlan", json!({"name": name, "price": price, "sessions": sessions})).await;
    }
    let gold = id_of(
        &k.list(&admin, "XPlan", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"name": "Gold"})).unwrap(), ..Default::default() })
            .await
            .unwrap()
            .rows[0],
    );
    src.insert(
        &admin,
        "XMembership",
        json!({"code": "S-1", "member_email": "sam@home.test", "plan": gold.to_string(), "lines": [{"item": "Fee", "amount": "50"}]}),
    )
    .await;
    // Studio: a space, a report pinned to it, a chart of the report, a card, a dashboard with both, and the
    // space's blocks naming all of them.
    let space = src.insert(&admin, "Space", json!({"label": "Gym home", "module": "gym", "sharing": "everyone"})).await;
    let report = src
        .insert(
            &admin,
            "Report",
            json!({"label": "Takings by plan", "kind": "group_by", "sharing": "everyone", "space": space["id"],
            "spec": {"entity": "XMembership", "group_by": [{"field": "plan"}], "measures": [{"fn": "sum", "field": "total"}]}}),
        )
        .await;
    let chart = src
        .insert(
            &admin,
            "DashboardChart",
            json!({"label": "Takings chart", "chart_type": "bar", "source": "report", "report": report["id"], "sharing": "everyone"}),
        )
        .await;
    let card = src
        .insert(
            &admin,
            "NumberCard",
            json!({"label": "Memberships", "format": "int", "sharing": "everyone", "spec": {"entity": "XMembership", "measures": [{"fn": "count"}]}}),
        )
        .await;
    src.insert(
        &admin,
        "Dashboard",
        json!({"label": "Gym", "sharing": "everyone", "layout": [
        {"i": "takings", "type": "chart", "chart": chart["id"], "x": 0, "y": 0, "w": 8, "h": 4},
        {"i": "count", "type": "card", "card": card["id"], "x": 8, "y": 0, "w": 4, "h": 2}]}),
    )
    .await;
    let dashboard = k.list(&admin, "Dashboard", &ListQuery::default()).await.unwrap().rows[0]["id"].clone();
    k.update(
        &admin,
        "Space",
        id_of(&space),
        obj(json!({"blocks": [{"type": "chart", "chart": chart["id"]},
        {"type": "links", "label": "Go to", "items": [{"label": "Takings", "report": report["id"]}, {"label": "Gym", "dashboard": dashboard}]}]})),
        None,
    )
    .await
    .expect("the space's blocks");
    // A solution whose rule counts, released, reviewed by a second administrator and activated.
    let admin2 = k.create_user(&admin, "admin2@source.test", "Second", Some("Correct-Horse-9"), &[], true).await.unwrap();
    let saved = k.save_release(&admin, &solution()).await.unwrap();
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&src.ctx(admin2).await, release, saved["artifact_digest"].as_str().unwrap(), true, "Counts look right").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();

    // ───────────── export: each of those, and nothing else ─────────────
    let admin = src.admin().await;
    let pack = k.export_pack(&admin, &export(&["XPlan"])).await.expect("export");
    assert_eq!(pack["format"], json!("ferrum.pack/1"));
    let ops: Vec<String> =
        pack["customizations"].as_array().unwrap().iter().map(|c| format!("{} {}", c["entity"].as_str().unwrap(), c["op"]["op"].as_str().unwrap())).collect();
    assert_eq!(ops, ["XPlan define_entity", "XMembershipLine define_entity", "XMembership define_entity", "XMembership add_rule", "XMembership set_workflow"]);
    let roles: Vec<&Value> = pack["roles"].as_array().unwrap().iter().map(|r| &r["name"]).collect();
    assert_eq!(roles, [&json!("Coach"), &json!("Members")], "the workspace's own roles, not Studio's");
    assert_eq!(pack["roles"][1]["portal"], json!(true));
    assert!(pack["roles"][1]["permissions"].as_array().unwrap().iter().any(|p| p["via"] == json!("member_email")));
    let autos: Vec<&Value> = pack["automations"].as_array().unwrap().iter().map(|a| &a["name"]).collect();
    assert_eq!(autos, [&json!("Daily plan"), &json!("Welcome")]);
    let studio: Vec<String> =
        pack["studio"].as_array().unwrap().iter().map(|s| format!("{} {}", s["entity"].as_str().unwrap(), s["name"].as_str().unwrap())).collect();
    assert_eq!(studio, ["Space Gym home", "Report Takings by plan", "DashboardChart Takings chart", "NumberCard Memberships", "Dashboard Gym"]);
    let item = |name: &str| pack["studio"].as_array().unwrap().iter().find(|s| s["name"] == json!(name)).unwrap()["record"].clone();
    assert_eq!(item("Takings by plan")["space"], json!("Gym home"), "a link by the key of what it reaches");
    assert_eq!(item("Takings chart")["report"], json!("Takings by plan"));
    assert_eq!(item("Gym")["layout"][0]["chart"], json!("Takings chart"));
    assert_eq!(item("Gym home")["blocks"][1]["items"][1]["dashboard"], json!("Gym"));
    assert_eq!(pack["solutions"].as_array().unwrap().len(), 1);
    assert_eq!(pack["solutions"][0]["solution"], json!("gym.limits"));
    assert_eq!(pack["records"].as_array().unwrap().len(), 1);
    assert_eq!(pack["records"][0]["key"], json!(["name"]));
    let plans: Vec<&Value> = pack["records"][0]["rows"].as_array().unwrap().iter().map(|r| &r["name"]).collect();
    assert_eq!(plans, [&json!("Basic"), &json!("Gold"), &json!("Platinum")], "the three rows, by key");
    assert_eq!(pack["requires"], json!({"packages": {"studio": ferrum_studio::packages().unwrap()[0].manifest.version}}));
    let text = pack.to_string();
    for id in [src.user, admin2, src.tenant, gold, id_of(&space), release] {
        assert!(!text.contains(&id.to_string()), "no id travels: {id}");
    }
    for word in ["created_by", "sam@home.test", "S-1", "password", "secret"] {
        assert!(!text.contains(word), "{word} travels");
    }
    let mut again = k.export_pack(&admin, &export(&["XPlan"])).await.unwrap();
    assert_ne!(again["exported_at"], Value::Null);
    again["exported_at"] = pack["exported_at"].clone();
    assert_eq!(again, pack, "two exports differ only in when they were made");
    // Reference records never hold people's details.
    let e = k.export_pack(&admin, &export(&["XMembership"])).await.unwrap_err();
    assert!(message(&e).contains("records of XMembership cannot travel in a pack: it keeps an email or a phone number"), "{}", message(&e));
    let member = k.create_user(&admin, "coach@source.test", "Coach", None, &["Coach".into()], false).await.unwrap();
    assert!(matches!(k.export_pack(&src.ctx(member).await, &export(&[])).await, Err(Error::Forbidden(_))), "administrators only");

    // ───────────── plan: a workspace lacking Studio, one with an older Studio, a fresh one ─────────────
    let bare = workspace(&k, "bare", &["core"]).await;
    let plan = k.plan_pack(&bare.admin().await, &pack).await.unwrap();
    assert_eq!(plan["can_install"], json!(false));
    assert!(plan["missing"][0].as_str().unwrap().contains("the package studio"), "{plan}");
    let refused = k.install_pack(&bare.admin().await, &pack, plan["digest"].as_str().unwrap()).await.unwrap_err();
    assert!(matches!(&refused, Error::Conflict(m) if m.contains("studio")), "{refused}");
    let old = workspace(&k, "old", &["core", "studio"]).await;
    let mut tx = k.db.tenant_tx(old.tenant).await.unwrap();
    sqlx::query("update installed_package set version = '0.0.1' where tenant_id = $1 and name = 'studio'").bind(old.tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let plan = k.plan_pack(&old.admin().await, &pack).await.unwrap();
    assert!(plan["missing"][0].as_str().unwrap().contains("studio is at 0.0.1 here"), "{plan}");

    let dst = workspace(&k, "target", &["core", "studio"]).await;
    let target = dst.admin().await;
    let before = dst.counts().await;
    let plan = k.plan_pack(&target, &pack).await.expect("plan");
    assert_eq!(dst.counts().await, before, "a plan does nothing");
    assert_eq!(plan["can_install"], json!(true));
    assert_eq!(names(&plan, "customizations", "created").len(), 5);
    assert_eq!(names(&plan, "roles", "created"), ["Coach", "Members"]);
    assert_eq!(names(&plan, "automations", "created"), ["Daily plan", "Welcome"]);
    assert_eq!(names(&plan, "studio", "created").len(), 5);
    assert_eq!(names(&plan, "records", "created"), ["XPlan Basic", "XPlan Gold", "XPlan Platinum"]);
    assert_eq!(names(&plan, "solutions", "created"), ["gym.limits"]);
    let warnings = plan["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings.iter().any(|w| w.as_str().unwrap().contains("Daily plan runs on a schedule from now")));
    assert!(warnings.iter().any(|w| w.as_str().unwrap().contains("gym.limits arrives as a draft") && w.as_str().unwrap().contains("second administrator")));
    let digest = plan["digest"].as_str().unwrap().to_string();

    // ───────────── refusals: each says why, and nothing is kept ─────────────
    let e = k.install_pack(&target, &pack, "sha256:00").await.unwrap_err();
    assert!(message(&e).contains("digest: is not this pack's"), "{}", message(&e));
    let mut odd = pack.clone();
    odd["extra"] = json!(1);
    assert!(message(&k.plan_pack(&target, &odd).await.unwrap_err()).contains("unknown field `extra`"));
    let mut big = pack.clone();
    big["customizations"][0]["note"] = json!("x".repeat(MAX_PACK));
    assert!(message(&k.plan_pack(&target, &big).await.unwrap_err()).contains("a pack is at most 5242880 bytes"));
    let coach_there = k.create_user(&target, "coach@target.test", "Coach", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let not_admin = dst.ctx(coach_there).await;
    assert!(matches!(k.plan_pack(&not_admin, &pack).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.install_pack(&not_admin, &pack, &digest).await, Err(Error::Forbidden(_))));
    let spare = workspace(&k, "spare", &["core", "studio"]).await;
    let spare_admin = spare.admin().await;
    let spare_before = spare.counts().await;
    let mut halfway = pack.clone();
    halfway["customizations"]
        .as_array_mut()
        .unwrap()
        .insert(3, json!({"entity": "XNowhere", "op": {"op": "add_options", "field": "status", "options": ["x"]}}));
    let e = k.plan_pack(&spare_admin, &halfway).await.unwrap_err();
    assert!(message(&e).contains("customizations: customization rejected"), "{}", message(&e));
    let hd = ferrum_kernel::pack::digest(&ferrum_kernel::pack::parse(&halfway).unwrap()).unwrap();
    assert_ne!(hd, digest);
    assert!(message(&k.install_pack(&spare_admin, &halfway, &hd).await.unwrap_err()).contains("customization rejected"));
    let mut late = pack.clone();
    late["records"][0]["rows"][2]["price"] = json!("not money");
    let ld = ferrum_kernel::pack::digest(&ferrum_kernel::pack::parse(&late).unwrap()).unwrap();
    let e = k.install_pack(&spare_admin, &late, &ld).await.unwrap_err();
    assert!(message(&e).contains("XPlan Platinum"), "the row refused is named: {}", message(&e));
    assert_eq!(spare.counts().await, spare_before, "a refusal anywhere leaves nothing: no customization, role, automation, item or record");
    assert!(spare.admin().await.schema.entity("XMembership").is_none());

    // ───────────── install in the fresh workspace ─────────────
    let done = k.install_pack(&target, &pack, &digest).await.expect("install");
    assert_eq!(done["failed"], json!([]));
    assert_eq!(done["summary"]["solutions"]["created"], json!(["gym.limits"]));
    let listed = k.installed_packs(&target).await.unwrap();
    assert_eq!((listed.len(), &listed[0]["name"], &listed[0]["digest"]), (1, &json!("gym"), &json!(digest)));
    assert_eq!(dst.count("select count(*) from audit_event where tenant_id = $1 and entity = 'Pack' and action = 'install'").await, 1);
    let target = dst.admin().await;
    // The entity: lines and their total; the key and the check refuse what they should.
    let gold_here = id_of(
        &k.list(&target, "XPlan", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"name": "Gold"})).unwrap(), ..Default::default() })
            .await
            .unwrap()
            .rows[0],
    );
    let m1 = dst
        .insert(&target, "XMembership", json!({"code": "M-1", "member_email": "lena@home.test", "plan": gold_here.to_string(), "lines": [{"item": "Fee", "amount": "0.1"}, {"item": "Towel", "amount": "0.2"}]}))
        .await;
    assert_eq!(m1["total"], json!("0.3"));
    let e = k.insert(&target, "XMembership", obj(json!({"code": "M-1"}))).await.unwrap_err();
    assert!(message(&e).contains("already has this Code"), "the key: {}", message(&e));
    let e = k.insert(&target, "XMembership", obj(json!({"code": "M-9", "lines": [{"item": "Year", "amount": "1200"}]}))).await.unwrap_err();
    assert!(message(&e).contains("A membership costs at most 1000"), "the check: {}", message(&e));
    // The workflow, for a user holding the role.
    k.set_user_roles_until(&target, coach_there, &["Coach".into()], &Default::default()).await.unwrap();
    let coach = dst.ctx(coach_there).await;
    assert_eq!(k.transition(&coach, "XMembership", id_of(&m1), "Activate", None).await.expect("the coach activates")["workflow_state"], json!("Active"));
    // The portal role sees its holder's own.
    dst.insert(&target, "XMembership", json!({"code": "M-2", "member_email": "nora@home.test"})).await;
    let lena = k.create_user(&target, "lena@home.test", "Lena", None, &[], false).await.unwrap();
    k.set_user_kind(&target, lena, "portal").await.unwrap();
    k.set_user_roles_until(&target, lena, &["Members".into()], &Default::default()).await.unwrap();
    let seen: Vec<Value> = k.list(&dst.ctx(lena).await, "XMembership", &ListQuery::default()).await.unwrap().rows.iter().map(|r| r["code"].clone()).collect();
    assert_eq!(seen, [json!("M-1")], "the portal role sees its own only");
    // The event automation fires; the scheduled one at its moment.
    drain(&k).await;
    assert_eq!((dst.mails("Welcome M-1").await, dst.mails("Welcome M-2").await), (1, 1));
    let tomorrow = Utc::now() + Duration::hours(24);
    while k.scan_schedules_at(tomorrow, ferrum_kernel::schedule::PAGE).await.unwrap() {}
    drain(&k).await;
    for p in ["Basic", "Gold", "Platinum"] {
        assert_eq!(dst.mails(&format!("Plan {p}")).await, 1, "{p}, once, at 06:00");
    }
    // The report runs, and the dashboard shows both its items.
    let report_here: Uuid = dst.text("select id::text from e_report where tenant_id = $1").await.unwrap().parse().unwrap();
    let ran = ferrum_studio::builder::run_report(&k, &target, report_here, &ferrum_studio::builder::Runtime::parse(&json!({})).unwrap())
        .await
        .expect("the report runs");
    assert_eq!(ran["data"]["totals"]["sum(total)"], json!("0.3"), "{ran}");
    let dashboard_here: Uuid = dst.text("select id::text from e_dashboard where tenant_id = $1").await.unwrap().parse().unwrap();
    let board = ferrum_studio::builder::dashboard_data(&k, &target, dashboard_here, &Map::new()).await.expect("the dashboard");
    let items: Vec<&Value> = board["items"].as_array().unwrap().iter().map(|i| &i["i"]).collect();
    assert_eq!(items, [&json!("takings"), &json!("count")], "{board}");
    for i in board["items"].as_array().unwrap() {
        assert!(i.get("error").is_none() && i["widget"].get("error").is_none(), "{i}");
    }
    let space_blocks: String = dst.text("select blocks::text from e_space where tenant_id = $1").await.unwrap();
    for id in [dst.text("select id::text from e_dashboard_chart where tenant_id = $1").await.unwrap(), report_here.to_string(), dashboard_here.to_string()] {
        assert!(space_blocks.contains(&id), "the space's blocks name what is here: {space_blocks}");
    }
    // The three rows; the solution a draft, which runs once released, reviewed and activated.
    assert_eq!(dst.count("select count(*) from custom_record where tenant_id = $1 and entity = 'XPlan'").await, 3);
    assert_eq!(dst.count("select count(*) from logic_activation where tenant_id = $1").await, 0, "nothing runs from a draft");
    let drafts = k.logic_drafts(&target).await.unwrap();
    assert_eq!(drafts.len(), 1);
    let released = k.release_logic_draft(&target, drafts[0]["id"].as_str().unwrap().parse().unwrap(), 1).await.unwrap();
    let rel: Uuid = released["id"].as_str().unwrap().parse().unwrap();
    let second = k.create_user(&target, "admin2@target.test", "Second", Some("Correct-Horse-9"), &[], true).await.unwrap();
    assert!(k.review_release(&target, rel, released["artifact_digest"].as_str().unwrap(), true, "mine").await.is_err(), "nobody reviews their own");
    k.review_release(&dst.ctx(second).await, rel, released["artifact_digest"].as_str().unwrap(), true, "Counts look right").await.unwrap();
    k.activate_release(&target, rel, None).await.unwrap();
    let target = dst.admin().await;
    dst.insert(&target, "XMembership", json!({"code": "M-3", "plan": gold_here.to_string(), "status": "Active"})).await;
    dst.insert(&target, "XMembership", json!({"code": "M-4", "plan": gold_here.to_string(), "status": "Active"})).await;
    let e = k.insert(&target, "XMembership", obj(json!({"code": "M-5", "plan": gold_here.to_string(), "status": "Active"}))).await.unwrap_err();
    assert!(message(&e).contains("This plan is full"), "the rule's aggregate counts: {}", message(&e));

    // ───────────── installed again: nothing changes ─────────────
    let before = dst.counts().await;
    let plan = k.plan_pack(&target, &pack).await.unwrap();
    nothing_to_do(&plan["sections"]);
    assert_eq!(plan["warnings"], json!([]), "{plan}");
    let again = k.install_pack(&target, &pack, &digest).await.unwrap();
    nothing_to_do(&again["summary"]);
    assert_eq!(dst.counts().await, before, "no duplicates");
    let solutions_after_second = dst.count("select count(*) from logic_draft where tenant_id = $1").await;

    // ───────────── the source changes, and the target follows ─────────────
    src.customize("XMembership", json!({"op": "add_field", "field": {"name": "x_locker", "type": "int"}})).await;
    let admin = src.admin().await;
    let welcome_id: String = src.text("select id::text from automation where tenant_id = $1 and name = 'Welcome'").await.unwrap();
    let mut changed = welcome(Some("doc.code != 'GUEST'"));
    changed.insert("id".into(), json!(welcome_id));
    automation::save(&k, &admin, &changed).await.unwrap();
    let gold_doc = k
        .list(&admin, "XPlan", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"name": "Gold"})).unwrap(), ..Default::default() })
        .await
        .unwrap()
        .rows[0]
        .clone();
    k.update(&admin, "XPlan", id_of(&gold_doc), obj(json!({"price": "55"})), None).await.unwrap();
    let pack2 = k.export_pack(&admin, &ExportOptions { version: 2, ..export(&["XPlan"]) }).await.unwrap();
    let plan = k.plan_pack(&target, &pack2).await.unwrap();
    assert_eq!(names(&plan, "customizations", "created"), ["XMembership: add_field x_locker"]);
    assert_eq!(names(&plan, "automations", "changed"), ["Welcome"]);
    assert_eq!(names(&plan, "records", "changed"), ["XPlan Gold"]);
    let before = dst.counts().await;
    let done = k.install_pack(&target, &pack2, plan["digest"].as_str().unwrap()).await.unwrap();
    assert_eq!(done["failed"], json!([]));
    let after = dst.counts().await;
    assert_eq!(after[0], before[0] + 1, "one customization more");
    assert_eq!(after[1..], before[1..], "nothing duplicated, no record lost");
    let target = dst.admin().await;
    assert!(target.schema.entity("XMembership").unwrap().field("x_locker").is_some(), "the field came");
    let m1_now = k.update(&target, "XMembership", id_of(&m1), obj(json!({"x_locker": 7})), None).await.unwrap();
    assert_eq!((&m1_now["x_locker"], &m1_now["total"]), (&json!(7), &json!("0.3")), "the record kept");
    assert_eq!(dst.text("select condition from automation where tenant_id = $1 and name = 'Welcome'").await.as_deref(), Some("doc.code != 'GUEST'"));
    assert_eq!(
        dst.text("select ext->>'price' from custom_record where tenant_id = $1 and entity = 'XPlan' and ext->>'name' = 'Gold'").await.as_deref(),
        Some("55")
    );
    assert_eq!(dst.count("select count(*) from logic_draft where tenant_id = $1").await, solutions_after_second, "the solution is there already");
    assert_eq!(k.installed_packs(&target).await.unwrap().len(), 3);

    // ───────────── a pack that redefines an entity says what of the workspace's own stops applying ─────────────
    dst.customize("XMembership", json!({"op": "add_rule", "rule": {"name": "end_after_start", "check": "doc.code != ''", "message": "a code"}})).await;
    let mut v3 = membership_def();
    v3["fields"].as_array_mut().unwrap().push(json!({"name": "notes", "type": "text"}));
    src.customize("XMembership", json!({"op": "redefine_entity", "entity": v3})).await;
    let pack3 = k.export_pack(&src.admin().await, &ExportOptions { version: 3, ..export(&["XPlan"]) }).await.unwrap();
    let target = dst.admin().await;
    let plan = k.plan_pack(&target, &pack3).await.unwrap();
    let stops: Vec<&str> = plan["warnings"].as_array().unwrap().iter().filter_map(Value::as_str).filter(|w| w.contains("stops applying")).collect();
    assert_eq!(stops, ["XMembership: add_rule end_after_start stops applying: the pack redefines XMembership"], "the pack's own x_locker is not named: {plan}");
    let done = k.install_pack(&target, &pack3, plan["digest"].as_str().unwrap()).await.unwrap();
    assert_eq!(done["warnings"], json!(stops), "the install says it too");
}
