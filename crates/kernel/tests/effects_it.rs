//! Effects that branch, find and loop (docs/builder/decisions.md BD-18), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). One flow an administrator could not configure before: a
//! button on a case finds or makes its party by a key, copies a template's checklist with due dates from the case's
//! start, and flags only the lines that need a sign-off; then the bounds, the refusals and automations alike.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-effects-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
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

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

fn define(entity: Value) -> NewCustomization {
    op(entity["name"].as_str().unwrap(), json!({"op": "define_entity", "entity": entity}))
}

fn action(entity: &str, name: &str, effects: Value) -> NewCustomization {
    op(entity, json!({"op": "add_action", "action": {"name": name, "label": name, "effects": effects}}))
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

#[track_caller]
fn refused<T: std::fmt::Debug>(r: Result<T, Error>, says: &str) {
    let e = r.expect_err(says);
    assert!(message(&e).contains(says), "{says}: {}", message(&e));
}

#[test]
fn effects_that_branch_find_and_loop() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

/// The case's button: find or make the party by its code, keep it on the case, copy the template's steps with due
/// dates from the start, flag the ones needing a sign-off, and tell nobody (a `when` that never holds).
fn open_case() -> Value {
    json!([
        {"type": "find", "entity": "XParty", "by": {"code": "doc.code"}, "create": {"name": "doc.title"}},
        {"type": "set_fields", "values": {"party": "results[0].id"}},
        {"type": "copy_lines", "table": "checklist", "link": "template", "from_table": "steps", "mode": "replace",
         "values": {"title": "item.title", "due": "addDays(doc.start, item.offset_days)", "needs_signoff": "item.needs_signoff"}},
        {"type": "for_each", "table": "checklist", "effects": [
            {"type": "set_line", "table": "checklist", "where": "row.id == item.id", "values": {"flagged": "true"}, "when": "item.needs_signoff"}]},
        {"type": "notify", "to": "owner", "title": "'never'", "when": "false"}
    ])
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "firm".into(),
            name: "Firm".into(),
            admin_email: "admin@firm.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .expect("workspace");
    let user = k.login("firm", "admin@firm.test", "Correct-Horse-9").await.unwrap().user;
    let admin = || {
        let k = k.clone();
        async move { k.load_ctx(tenant, user, None, "it".into()).await.unwrap() }
    };
    let batch = |items: Vec<NewCustomization>| {
        let k = k.clone();
        async move {
            let a = k.load_ctx(tenant, user, None, "it".into()).await.unwrap();
            k.batch_customizations(&a, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
        }
    };
    let child = |name: &str, fields: Value| define(json!({"name": name, "label": name, "kind": "child", "fields": fields}));
    batch(vec![
        child("XTemplateStep", json!([{"name": "title", "type": "data"}, {"name": "offset_days", "type": "int"}, {"name": "needs_signoff", "type": "check"}])),
        define(json!({"name": "XTemplate", "label": "Template", "kind": "master", "title_field": "name", "fields": [
            {"name": "name", "type": "data", "unique": true}, {"name": "steps", "type": "table", "of": "XTemplateStep"}]})),
        define(json!({"name": "XParty", "label": "Party", "kind": "master", "title_field": "name", "fields": [
            {"name": "code", "type": "data", "unique": true}, {"name": "name", "type": "data"}]})),
        define(json!({"name": "XLoose", "label": "Loose", "kind": "master", "fields": [{"name": "code", "type": "data"}, {"name": "hits", "type": "int"}]})),
        child("XCaseStep", json!([{"name": "title", "type": "data"}, {"name": "due", "type": "date"}, {"name": "needs_signoff", "type": "check"}, {"name": "flagged", "type": "check"}])),
        define(json!({"name": "XCase", "label": "Case", "kind": "master", "title_field": "title", "fields": [
            {"name": "title", "type": "data", "required": true}, {"name": "code", "type": "data"}, {"name": "start", "type": "date"},
            {"name": "party", "type": "link", "to": "XParty"}, {"name": "template", "type": "link", "to": "XTemplate"},
            {"name": "checklist", "type": "table", "of": "XCaseStep"}]})),
        define(json!({"name": "XTask", "label": "Task", "kind": "master", "fields": [
            {"name": "case", "type": "link", "to": "XCase"}, {"name": "status", "type": "data"}, {"name": "priority", "type": "int"}]})),
    ])
    .await
    .expect("entities");

    // ───────────── refused when saved, saying which effect ─────────────
    for (effects, says) in [
        (json!([{"type": "find", "entity": "XNope", "by": {"code": "doc.code"}}]), "effect 1: find: unknown entity XNope"),
        (json!([{"type": "find", "entity": "XParty", "by": {"colour": "doc.code"}}]), "find: Party has no field colour"),
        (
            json!([{"type": "set_fields", "values": {"code": "'x'"}}, {"type": "for_each", "table": "code", "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}]),
            "effect 2: for_each: code is not a table of lines of Case",
        ),
        (
            json!([{"type": "for_each", "table": "checklist", "effects": [{"type": "for_each", "table": "checklist", "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}]}]),
            "a loop does not hold another loop",
        ),
        (
            json!([{"type": "for_each", "entity": "XTask", "link": "status", "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}]),
            "status is not a link field of Task to Case",
        ),
        (
            json!([{"type": "copy_lines", "table": "checklist", "link": "template", "from_table": "name", "values": {"title": "item.title"}}]),
            "name is not a table of lines of Template",
        ),
        (
            json!([{"type": "copy_lines", "table": "checklist", "result": 0, "from_table": "steps", "values": {"title": "item.title"}}]),
            "result 0 is not an earlier find",
        ),
        (
            json!([{"type": "copy_lines", "table": "checklist", "link": "template", "from_table": "steps", "values": {"colour": "item.title"}}]),
            "has no field colour",
        ),
        (json!([{"type": "notify", "to": "owner", "title": "'t'", "when": "doc.code =="}]), "when 'doc.code =='"),
        // The templates and recipients a message or a document names are there.
        (
            json!([{"type": "set_fields", "values": {}}, {"type": "message", "template": "wk.nothing", "to": "owner"}]),
            "effect 2: there is no message template wk.nothing",
        ),
        (json!([{"type": "message", "template": "wk.nothing", "to": ["field:nofield"]}]), "message.to: Case has no field nofield"),
        (json!([{"type": "generate", "template": "wk.nothing"}]), "effect 1: there is no document template wk.nothing"),
        (
            json!([{"type": "for_each", "table": "checklist", "effects": [{"type": "notify", "to": "owner", "message": "wk.nothing"}]}]),
            "there is no message template wk.nothing",
        ),
    ] {
        let e = batch(vec![action("XCase", "x_bad", effects)]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    batch(vec![
        action("XCase", "x_open", open_case()),
        action(
            "XCase",
            "x_close_tasks",
            json!([{"type": "for_each", "entity": "XTask", "link": "case", "where": "item.status != 'Done'",
            "effects": [{"type": "find", "entity": "XTask", "by": {"id": "item.id"}, "update": {"status": "'Done'"}}]}]),
        ),
        action(
            "XCase",
            "x_party_then_fail",
            json!([{"type": "find", "entity": "XParty", "by": {"code": "'ZZ'"}, "create": {"name": "'Gone'"}},
            {"type": "set_fields", "values": {"title": "''"}}]),
        ),
        action(
            "XCase",
            "x_flag_all",
            json!([{"type": "for_each", "table": "checklist",
            "effects": [{"type": "set_line", "table": "checklist", "where": "row.id == item.id", "values": {"flagged": "true"}}]}]),
        ),
        action("XCase", "x_loose", json!([{"type": "find", "entity": "XLoose", "by": {"code": "doc.code"}, "create": {"hits": "1"}}])),
        action(
            "XCase",
            "x_copy_found",
            json!([{"type": "find", "entity": "XTemplate", "by": {"name": "'Standard'"}},
            {"type": "copy_lines", "table": "checklist", "result": 0, "from_table": "steps", "values": {"title": "'again: ' + item.title"}}]),
        ),
    ])
    .await
    .expect("the buttons");

    // Described as saved, so an authoring screen can round-trip them.
    let def = k.custom_definition(&admin().await, "XCase").expect("definition");
    let saved = def["entity"]["actions"].as_array().unwrap().iter().find(|a| a["name"] == "x_open").unwrap()["effects"].clone();
    assert_eq!(saved, open_case(), "the effects come back as written");

    // ───────────── the flow ─────────────
    let a = admin().await;
    let steps =
        json!([{"title": "Intake", "offset_days": 0}, {"title": "Review", "offset_days": 7, "needs_signoff": true}, {"title": "Close", "offset_days": 30}]);
    let template = k.insert(&a, "XTemplate", obj(json!({"name": "Standard", "steps": steps}))).await.unwrap();
    let case = |title: &str, code: &str| obj(json!({"title": title, "code": code, "start": "2026-11-01", "template": template["id"]}));
    let c1 = k.insert(&a, "XCase", case("First", "P-1")).await.unwrap();
    k.run_action(&a, "XCase", id_of(&c1), "x_open", Value::Null).await.expect("opened");
    let c1 = k.get(&a, "XCase", id_of(&c1), false).await.unwrap();
    let parties = k.list(&a, "XParty", &Default::default()).await.unwrap().rows;
    assert_eq!(parties.len(), 1, "made, as none had the code");
    assert_eq!((&parties[0]["code"], &parties[0]["name"], &c1["party"]), (&json!("P-1"), &json!("First"), &parties[0]["id"]));
    let lines: Vec<(Value, Value, Value)> =
        c1["checklist"].as_array().unwrap().iter().map(|r| (r["title"].clone(), r["due"].clone(), r["flagged"].clone())).collect();
    assert_eq!(
        lines,
        [
            (json!("Intake"), json!("2026-11-01"), json!(false)),
            (json!("Review"), json!("2026-11-08"), json!(true)),
            (json!("Close"), json!("2026-12-01"), json!(false))
        ],
        "copied with dates from the start, flagged only where a sign-off is needed"
    );
    // Again, and on a second case with the same code: found, not made again; `replace` leaves three lines.
    k.run_action(&a, "XCase", id_of(&c1), "x_open", Value::Null).await.expect("again");
    let c2 = k.insert(&a, "XCase", case("Second", "P-1")).await.unwrap();
    k.run_action(&a, "XCase", id_of(&c2), "x_open", Value::Null).await.expect("second case");
    assert_eq!(k.list(&a, "XParty", &Default::default()).await.unwrap().rows.len(), 1, "found by its key");
    assert_eq!(k.get(&a, "XCase", id_of(&c1), false).await.unwrap()["checklist"].as_array().unwrap().len(), 3);
    assert_eq!(k.get(&a, "XCase", id_of(&c2), false).await.unwrap()["party"], c1["party"]);

    // From what an earlier effect found, appended.
    k.run_action(&a, "XCase", id_of(&c2), "x_copy_found", Value::Null).await.expect("copied from a find");
    let titles: Vec<Value> = k.get(&a, "XCase", id_of(&c2), false).await.unwrap()["checklist"].as_array().unwrap().iter().map(|r| r["title"].clone()).collect();
    assert_eq!(titles.len(), 6);
    assert_eq!(titles[3], json!("again: Intake"));

    // ───────────── for each record linking here ─────────────
    for (status, priority) in [("Open", 1), ("Done", 2), ("Open", 3)] {
        k.insert(&a, "XTask", obj(json!({"case": c1["id"], "status": status, "priority": priority}))).await.unwrap();
    }
    k.insert(&a, "XTask", obj(json!({"case": c2["id"], "status": "Open"}))).await.unwrap();
    let out = k.run_action(&a, "XCase", id_of(&c1), "x_close_tasks", Value::Null).await.expect("closed");
    assert!(out.is_object());
    let tasks = k.list(&a, "XTask", &Default::default()).await.unwrap().rows;
    let open: Vec<&Value> = tasks.iter().filter(|t| t["status"] == "Open").map(|t| &t["case"]).collect();
    assert_eq!(open, [&c2["id"]], "every task of the first case closed, the second's left alone");

    // ───────────── a refusal anywhere keeps nothing ─────────────
    refused(k.run_action(&a, "XCase", id_of(&c1), "x_party_then_fail", Value::Null).await, "title: effect 2 (set_fields): ");
    let codes: Vec<Value> = k.list(&a, "XParty", &Default::default()).await.unwrap().rows.iter().map(|p| p["code"].clone()).collect();
    assert_eq!(codes, [json!("P-1")], "the party the first effect made went with the refusal");

    // ───────────── two that match: a refusal, never a guess ─────────────
    for _ in 0..2 {
        k.insert(&a, "XLoose", obj(json!({"code": "DUP"}))).await.unwrap();
    }
    let dup = k.insert(&a, "XCase", case("Dup", "DUP")).await.unwrap();
    refused(k.run_action(&a, "XCase", id_of(&dup), "x_loose", Value::Null).await, "effect 1 (find): more than one Loose has code DUP");
    let blank = k.insert(&a, "XCase", obj(json!({"title": "Blank"}))).await.unwrap();
    refused(k.run_action(&a, "XCase", id_of(&blank), "x_loose", Value::Null).await, "code is empty");

    // ───────────── two runs at once make one record, with no unique key ─────────────
    // Two cases, so that nothing but the find makes them wait.
    let race1 = k.insert(&a, "XCase", case("Race", "RACE")).await.unwrap();
    let race2 = k.insert(&a, "XCase", case("Race too", "RACE")).await.unwrap();
    let (r1, r2) =
        tokio::join!(k.run_action(&a, "XCase", id_of(&race1), "x_loose", Value::Null), k.run_action(&a, "XCase", id_of(&race2), "x_loose", Value::Null));
    r1.expect("first");
    r2.expect("second");
    let loose = k.list(&a, "XLoose", &Default::default()).await.unwrap().rows;
    assert_eq!(loose.iter().filter(|l| l["code"] == "RACE").count(), 1, "the second waited and found the first's");

    // ───────────── bounded ─────────────
    let many: Vec<Value> = (0..=automation::MAX_ITEMS).map(|i| json!({"title": format!("S{i}"), "offset_days": 0})).collect();
    let big = k.insert(&a, "XTemplate", obj(json!({"name": "Big", "steps": many}))).await.unwrap();
    let c3 = k.insert(&a, "XCase", obj(json!({"title": "Big", "code": "B", "start": "2026-11-01", "template": big["id"]}))).await.unwrap();
    refused(k.run_action(&a, "XCase", id_of(&c3), "x_open", Value::Null).await, "more than the 100 one copy takes");
    assert!(k.get(&a, "XCase", id_of(&c3), false).await.unwrap()["party"].is_null(), "nothing kept");
    let lines: Vec<Value> = (0..=automation::MAX_ITEMS).map(|i| json!({"title": format!("L{i}")})).collect();
    let c4 = k.insert(&a, "XCase", obj(json!({"title": "Long", "checklist": lines}))).await.unwrap();
    refused(k.run_action(&a, "XCase", id_of(&c4), "x_flag_all", Value::Null).await, "effect 1 (for_each): more than 100 lines of checklist");

    // ───────────── automations alike ─────────────
    automation::save(
        &k,
        &a,
        &obj(json!({"name": "Seen", "entity": "XTask", "event": "created",
            "actions": [{"type": "set_fields", "values": {"status": "'Urgent'"}, "when": "doc.priority > 1"}]})),
    )
    .await
    .expect("an automation with a when");
    let e = automation::save(
        &k,
        &a,
        &obj(json!({"name": "Late", "entity": "XTask", "event": "created",
        "actions": [{"type": "set_fields", "values": {"status": "'x'"}}, {"type": "generate", "template": "wk.nothing"}]})),
    )
    .await
    .unwrap_err();
    assert!(matches!(&e, Error::Validation(f) if f[0].row == Some(("actions".into(), 1)) && f[0].message.contains("no document template wk.nothing")), "{e:?}");
    let low = k.insert(&a, "XTask", obj(json!({"status": "New", "priority": 1}))).await.unwrap();
    let high = k.insert(&a, "XTask", obj(json!({"status": "New", "priority": 2}))).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!(k.get(&a, "XTask", id_of(&low), false).await.unwrap()["status"], "New");
    assert_eq!(k.get(&a, "XTask", id_of(&high), false).await.unwrap()["status"], "Urgent");

    // ───────────── the lines of a linked record: a team's members ─────────────
    batch(vec![
        child("XMember", json!([{"name": "email", "type": "email"}, {"name": "note", "type": "int"}])),
        define(json!({"name": "XTeam", "label": "Team", "kind": "master", "fields": [
            {"name": "name", "type": "data"}, {"name": "members", "type": "table", "of": "XMember"}]})),
        define(json!({"name": "XTicket", "label": "Ticket", "kind": "master", "fields": [
            {"name": "title", "type": "data"}, {"name": "team", "type": "link", "to": "XTeam"}]})),
        define(json!({"name": "XSeen", "label": "Seen", "kind": "master", "fields": [{"name": "who", "type": "data"}, {"name": "ticket", "type": "data"}]})),
    ])
    .await
    .expect("teams");
    let tell = |to: &str| json!([{"type": "notify", "to": to, "title": "'t'"}]);
    for (effects, says) in [
        (tell("links:title.members.email"), "title is not a link field of Ticket"),
        (tell("links:team.nope.email"), "nope is not a table of lines of Team"),
        (tell("links:team.members.colour"), "the lines of members have no field colour"),
        (tell("links:team.members.note"), "note holds no address"),
        (tell("links:team.members"), "notify.to must be"),
        (json!([{"type": "message", "template": "x", "to": "links:team.nope.email"}]), "nope is not a table of lines of Team"),
        (json!([{"type": "for_each", "links": "team.nope", "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}]), "nope is not a table of lines"),
    ] {
        let e = batch(vec![action("XTicket", "x_bad", effects)]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    let a = admin().await;
    let mut people = Vec::new();
    for n in 1..=3 {
        people.push(k.create_user(&a, &format!("m{n}@firm.test"), "M", Some("Correct-Horse-9"), &[], false).await.unwrap());
    }
    let member = |n: u32| json!({"email": format!("M{n}@firm.test")});
    // The first twice: told once all the same.
    let team = k.insert(&a, "XTeam", obj(json!({"name": "Desk", "members": [member(1), member(2), member(1)]}))).await.unwrap();
    automation::save(
        &k,
        &a,
        &obj(json!({"name": "Assigned", "entity": "XTicket", "event": "updated", "condition": "doc.team != old.team",
            "actions": [{"type": "notify", "to": "links:team.members.email", "title": "'Assigned to your team'"},
                {"type": "for_each", "links": "team.members", "where": "item.email != ''",
                 "effects": [{"type": "create_document", "entity": "XSeen", "values": {"who": "item.email", "ticket": "doc.id"}}]}]})),
    )
    .await
    .expect("a notice to a group's members");
    let told = |ticket: Value| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let rows: Vec<(Uuid,)> =
                sqlx::query_as("select user_id from notification where record_id = $1 and title = 'Assigned to your team' order by user_id")
                    .bind(ticket.as_str().unwrap().parse::<Uuid>().unwrap())
                    .fetch_all(&mut *tx)
                    .await
                    .unwrap();
            rows.into_iter().map(|r| r.0).collect::<Vec<_>>()
        }
    };
    let t1 = k.insert(&a, "XTicket", obj(json!({"title": "Printer"}))).await.unwrap();
    k.update(&a, "XTicket", id_of(&t1), obj(json!({"team": team["id"]})), None).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    let mut want = vec![people[0], people[1]];
    want.sort();
    assert_eq!(told(t1["id"].clone()).await, want, "each member once, nobody else");
    let seen = k.list(&a, "XSeen", &Default::default()).await.unwrap().rows;
    assert_eq!(seen.len(), 3, "the loop ran once per line");
    // The second member leaves; the third joins.
    k.update(&a, "XTeam", id_of(&team), obj(json!({"members": [member(1), member(3)]})), None).await.unwrap();
    let t2 = k.insert(&a, "XTicket", obj(json!({"title": "Screen"}))).await.unwrap();
    k.update(&a, "XTicket", id_of(&t2), obj(json!({"team": team["id"]})), None).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    let mut want = vec![people[0], people[2]];
    want.sort();
    assert_eq!(told(t2["id"].clone()).await, want, "a member removed is not told");
    // Over the bound: refused, nobody told.
    batch(vec![action("XTicket", "x_tell", json!([{"type": "notify", "to": "links:team.members.email", "title": "'Big'"}]))]).await.expect("button");
    let a = admin().await;
    let crowd: Vec<Value> = (0..=automation::MAX_ITEMS).map(|i| json!({"email": format!("c{i}@firm.test")})).collect();
    let big = k.insert(&a, "XTeam", obj(json!({"name": "Crowd", "members": crowd}))).await.unwrap();
    let t3 = k.insert(&a, "XTicket", obj(json!({"title": "All", "team": big["id"]}))).await.unwrap();
    refused(k.run_action(&a, "XTicket", id_of(&t3), "x_tell", Value::Null).await, "more than the 100 one message goes to");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let none: i64 = sqlx::query_scalar("select count(*) from notification where title = 'Big'").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(none, 0, "nobody told when over the bound");
}
