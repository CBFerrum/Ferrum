//! Who is acting (S1-P0c, docs/builder/decisions.md BD-83), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! kit empties it): every place a context is made marks it as what it is (a person at a screen, a key, an automation, a
//! job, an approval completing, the assistant, the system), and `Ctx::at_screen()` holds only for a person at a screen in
//! this request. An approval policy with `people_at_screens` refuses anyone else, asking or deciding. Which credentials
//! make a screen is decided by the token's claims in `server/src/extract.rs` (`screens_are_decided_by_claims`), and
//! `scripts/check_contexts.py` holds that no other code makes one. What an app's call names: `app_uses_it.rs`.

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::controls::Proof;
use ferrum_kernel::ctx::Egress;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, HookRegistry, Made, automation};
use ferrum_meta::{MetaError, Package};
use ferrum_testkit::{App, Kit, database, obj, run, uid};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const PACKAGE: &str = "package: till\nversion: 0.1.0\ndepends: [core]\nuses:\n  - {capability: till.payout.stamp, version: '^0.1'}\nroles:\n  - name: Cashier\n    permissions:\n      - {entity: Payout, read: true, write: true, create: true}\n      - {entity: Note, read: true, create: true}\n\
approvals:\n  - entity: Payout\n    on: {action: release}\n    policy: {steps: [{label: A second administrator, approvers: {administrators: true}}], people_at_screens: true}\n  - entity: Payout\n    on: {action: settle}\n    policy: {steps: [{label: A second administrator, approvers: {administrators: true}}], people_at_screens: true, step_up_within: 600}\n";
const ENTITIES: &str = r#"
name: Payout
label: Payout
kind: master
title_field: title
fields:
  - {name: title, type: data, required: true}
actions:
  - {name: release, label: Release, permission: write}
  - {name: settle, label: Settle by Hand, permission: write}
  - name: stamp
    label: Stamp
    permission: write
    hidden: true
    contract: {completion: A note says it was stamped}
    effects: [{type: create_document, entity: Note, values: {title: "'stamped'"}}]
"#;
const NOTE: &str = "name: Note\nlabel: Note\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n";

/// What the till's code saw acting: (where, how the context was made, its app, whether at a screen).
static SEEN: std::sync::Mutex<Vec<(String, Made, Option<String>, bool)>> = std::sync::Mutex::new(Vec::new());

fn seen(at: &str) -> Vec<(Made, Option<String>, bool)> {
    SEEN.lock().unwrap().iter().filter(|s| s.0 == at).map(|s| (s.1, s.2.clone(), s.3)).collect()
}

/// Records the context of a note's save (titled after what made it) and of a payout's release.
struct Watch;

#[async_trait]
impl DocHook for Watch {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> ferrum_kernel::Result<()> {
        if let Some(title) = doc.get("title").and_then(Value::as_str).filter(|_| hc.entity.def.name == "Note") {
            SEEN.lock().unwrap().push((format!("note:{title}"), hc.ctx.made, hc.ctx.app.clone(), hc.ctx.at_screen()));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, _: &Doc, _: &Value) -> ferrum_kernel::Result<Option<Value>> {
        SEEN.lock().unwrap().push((format!("action:{name}"), hc.ctx.made, hc.ctx.app.clone(), hc.ctx.at_screen()));
        Ok(None)
    }
}

fn till_packages() -> Result<Vec<Package>, MetaError> {
    let files: BTreeMap<String, String> = [("package.yaml", PACKAGE), ("entities/payout.yaml", ENTITIES), ("entities/note.yaml", NOTE)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

fn till_register(h: &mut HookRegistry) {
    h.register("Payout", Arc::new(Watch));
    h.register("Note", Arc::new(Watch));
}

#[test]
fn who_acts() {
    run(scenario());
}

/// A request's context, as the server builds it: at a screen or with a key.
async fn request(kit: &Kit, of: &Ctx, screen: bool) -> Ctx {
    let entry = Arc::new(Entry { zones: Arc::new(Zones::default()), placed: Placed::Nowhere, address: "10.1.2.3".into(), credential: Credential::Token });
    kit.k.request_ctx_as(of.tenant, of.user, None, "it".into(), &entry, screen).await.expect("a request's context")
}

async fn scenario() {
    let till = App { id: "till", packages: till_packages, register: till_register, reports: Vec::new };
    let kit = Kit::boot(&database("FERRUM_TEST_DATABASE_URL"), &[till]).await;
    let ws = kit.workspace("till", &["till"], Value::Null).await;
    let k = kit.k.clone();
    let (asha, ben) = (ws.user("asha@till.test", &["Cashier"]).await, ws.user("ben@till.test", &["Cashier"]).await);

    // ── a_request_and_a_job_name_no_app; no_constructor_but_request_ctx_makes_screen ──
    let screen = request(&kit, &asha, true).await;
    assert!(screen.made == Made::Screen && screen.app.is_none() && screen.at_screen(), "a person at a screen");
    let key = request(&kit, &asha, false).await;
    assert!(key.made == Made::Key && !key.at_screen(), "a key, a bare token, an app's or an OAuth token: not a screen");
    let loaded = k.load_ctx(ws.tenant, asha.user, None, "job".into()).await.unwrap();
    assert!(loaded.made == Made::Job && loaded.app.is_none() && !loaded.at_screen(), "loaded outside a request: queued work");
    let system = Ctx::system(ws.tenant, asha.schema.clone());
    assert!(system.made == Made::System && !system.at_screen());

    // ── an_elevated_context_is_made_system_and_not_at_a_screen ──
    let elevated = screen.clone().elevated();
    assert!(elevated.is_system && elevated.made == Made::System && !elevated.at_screen());

    // ── a_context_with_an_app_or_egress_is_not_at_a_screen; the_assistant_and_mcp_are_made_assistant ──
    assert!(!Ctx { app: Some("shop".into()), ..screen.clone() }.at_screen(), "an app's call");
    let assistant = screen.for_provider(Egress::default());
    assert!(assistant.made == Made::Assistant && !assistant.at_screen(), "the assistant and MCP");

    // ── stepped_up_within_reads_the_sessions_step_up ──
    let proved = |secs: i64| Ctx { proof: Proof::Person { at: Some(chrono::Utc::now() - chrono::Duration::seconds(secs)), staff: true }, ..screen.clone() };
    assert!(proved(60).stepped_up_within(600) && !proved(900).stepped_up_within(600));
    assert!(!Ctx { proof: Proof::Person { at: None, staff: true }, ..screen.clone() }.stepped_up_within(600), "never stepped up");
    assert!(!Ctx { proof: Proof::Machine, ..screen.clone() }.stepped_up_within(600));

    // ── a_job_runs_made_job ──
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    let job = ferrum_kernel::jobs::enqueue(&mut tx, ws.tenant, Some(asha.user), ferrum_kernel::jobs::NewJob::new("it.noop", json!({}))).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    let job: ferrum_kernel::jobs::Job = sqlx::query_as("select tenant_id, id, kind, payload, priority, attempts, max_attempts, created_by, correlation_id, depth, lease_token, progress as resumed, undo_on_cancel, zone, step_up from job where id = $1").bind(job).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let (creator, sys) = (job.creator(&k).await.unwrap(), job.system(&k).await.unwrap());
    assert!(creator.made == Made::Job && !creator.at_screen() && sys.made == Made::Job && !sys.at_screen(), "queued work, as its creator or the system");

    // ── people_at_screens_refuses_an_asker_or_decider_not_at_a_screen ──
    let payout = |title: &'static str| {
        let k = k.clone();
        let asha = asha.clone();
        async move { uid(&k.insert(&asha, "Payout", obj(json!({"title": title}))).await.unwrap()) }
    };
    let first = payout("Float to bank").await;
    for (who, ctx) in [
        ("a key", key.clone()),
        ("a job", loaded.clone()),
        ("the assistant", assistant.clone()),
        ("an app", Ctx { app: Some("shop".into()), ..screen.clone() }),
    ] {
        assert!(matches!(k.run_action(&ctx, "Payout", first, "release", json!({})).await, Err(Error::Forbidden(_))), "{who} asks");
    }

    // The administrators: the founder (made with the workspace), Carla promoted today, Dan a member who will stand in.
    let promote = |email: &'static str, admin: bool| {
        let (k, ws) = (k.clone(), ws.clone());
        async move {
            let id = k.create_user(&ws.admin, email, email, Some("Correct-Horse-9"), &[], admin).await.unwrap();
            k.load_ctx(ws.tenant, id, None, "it".into()).await.unwrap()
        }
    };
    let (carla, dan) = (promote("carla@till.test", true).await, promote("dan@till.test", false).await);
    let since = |who: uuid::Uuid| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let at: Option<chrono::DateTime<chrono::Utc>> =
                sqlx::query_scalar("select admin_since from app_user where id = $1").bind(who).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            at
        }
    };
    // ── admin_since_is_set_by_every_way_of_making_an_administrator: the workspace's founder, create_user, the operator ──
    assert!(since(ws.admin.user).await.is_some() && since(carla.user).await.is_some() && since(dan.user).await.is_none());
    assert!(carla.admin_since.is_some() && dan.admin_since.is_none(), "loaded beside is_admin");
    k.operator_admin("till", "dan@till.test", true, "covering the desk").await.unwrap();
    assert!(since(dan.user).await.is_some(), "the operator's");
    k.operator_admin("till", "dan@till.test", false, "back to the desk").await.unwrap();
    assert!(since(dan.user).await.is_none(), "cleared when it turns off");
    // ── every_administrator_is_told_of_a_new_one ──
    let mut tx = k.db.system_tx().await.unwrap();
    let told: i64 =
        sqlx::query_scalar("select count(*) from notification where tenant_id = $1 and user_id = $2 and title = 'carla@till.test is now an administrator'")
            .bind(ws.tenant)
            .bind(ws.admin.user)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(told, 1, "the founder is told");

    // Moving the clock: whoever's administrator flag is `days` old.
    let aged = |who: uuid::Uuid, days: i32| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            sqlx::query("update app_user set admin_since = now() - make_interval(days => $2) where id = $1")
                .bind(who)
                .bind(days)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    aged(ws.admin.user, 30).await;
    let asked = k.run_action(&screen, "Payout", first, "release", json!({})).await.expect("asked at a screen");
    assert!(asked.get("_approval").is_some(), "it waits for an administrator: {asked}");
    // ── a_new_administrator_cannot_approve_where_money_goes_for_seven_days ──
    let carla_screen = request(&kit, &carla, true).await;
    assert!(matches!(k.decide_approval(&carla_screen, "Payout", first, true, None).await, Err(Error::Forbidden(_))), "promoted today");
    // an automation, an API key and the assistant acting for an administrator are refused (people_at_screens)
    let admin_key = request(&kit, &ws.admin, false).await;
    let admin_auto = Ctx { made: Made::Automation, ..k.load_ctx(ws.tenant, ws.admin.user, None, "auto".into()).await.unwrap() };
    let admin_screen = request(&kit, &ws.admin, true).await;
    for (who, ctx) in [("a key", admin_key), ("the assistant", admin_screen.for_provider(Egress::default())), ("an automation", admin_auto)] {
        let e = k.decide_approval(&ctx, "Payout", first, true, None).await.unwrap_err();
        assert!(matches!(e, Error::Forbidden(_)) && e.to_string().contains("at a screen"), "{who}: {e}");
    }
    k.decide_approval(&admin_screen, "Payout", first, true, None).await.expect("an administrator of a month, at a screen");
    // A week on, Carla counts.
    aged(carla.user, 8).await;
    let second = payout("Float to bank, again").await;
    k.run_action(&screen, "Payout", second, "release", json!({})).await.expect("asked");
    // ── an_administrators_step_is_never_decided_by_a_stand_in ──
    let today = chrono::Utc::now().date_naive();
    k.set_away(&ws.admin, carla.user, today, today + chrono::Duration::days(3), dan.user, "on leave").await.unwrap();
    let dan_screen = request(&kit, &dan, true).await;
    assert!(matches!(k.decide_approval(&dan_screen, "Payout", second, true, None).await, Err(Error::Forbidden(_))), "no stand-in is offered");
    let carla_screen = request(&kit, &carla, true).await;
    k.decide_approval(&carla_screen, "Payout", second, true, None).await.expect("an administrator of a week, in person");

    // ── an_administrators_step_with_escalation_or_the_asker_is_refused ──
    let third = payout("Petty cash").await;
    k.run_action(&admin_screen, "Payout", third, "release", json!({})).await.expect("asked by the founder");
    assert!(matches!(k.decide_approval(&admin_screen, "Payout", third, true, None).await, Err(Error::Forbidden(_))), "not by the asker");
    k.decide_approval(&carla_screen, "Payout", third, true, None).await.expect("the other administrator decides");
    for extra in ["allow_requester: true", "steps: [{label: A, approvers: {administrators: true}, escalate_to: {role: Cashier}, escalate_after: PT2H}]"] {
        let policy = if extra.starts_with("steps") {
            format!("{{{extra}, people_at_screens: true}}")
        } else {
            format!("{{steps: [{{label: A, approvers: {{administrators: true}}}}], people_at_screens: true, {extra}}}")
        };
        let package = PACKAGE.replace("{steps: [{label: A second administrator, approvers: {administrators: true}}], people_at_screens: true}", &policy);
        assert_ne!(package, PACKAGE);
        let files: BTreeMap<String, String> = [("package.yaml", package.as_str()), ("entities/payout.yaml", ENTITIES), ("entities/note.yaml", NOTE)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let core = ferrum_meta::catalog::load_package_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).unwrap();
        let refused = match ferrum_meta::catalog::load_package_files(&files) {
            Err(e) => Some(e.to_string()),
            Ok(p) => ferrum_meta::Catalog::from_packages(vec![core, p]).err().map(|e| e.to_string()),
        };
        assert!(refused.as_deref().is_some_and(|e| e.contains("in person")), "{extra}: {refused:?}");
    }
    // ── an_approved_action_completes_made_approval: the release Carla approved ran as Asha, made Approval ──
    let released = seen("action:release");
    assert!(released.contains(&(Made::Approval, None, false)), "completed as the requester, not at a screen: {released:?}");

    // ── an_automation_is_made_automation_even_in_a_zone ──
    automation::save(
        &k,
        &ws.admin,
        &obj(json!({"name": "note it", "entity": "Payout", "event": "created",
        "actions": [{"type": "create_document", "entity": "Note", "values": {"title": "'by automation'"}}]})),
    )
    .await
    .unwrap();
    payout("Change for the till").await;
    ws.drain().await;
    let s = seen("note:by automation");
    assert!(!s.is_empty() && s.iter().all(|x| *x == (Made::Automation, None, false)), "{}: {s:?}", "an automation, never a screen");

    // ── effects_inside_an_apps_call_name_no_app: the till's call of its own stamp; the effect's note names no app ──
    let stamped = payout("Stamp me").await;
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    k.call_tx(&kit.vault("till"), &asha, &mut tx, &Invocation::new("till.payout.stamp", json!({"id": stamped, "params": {}})).key("stamp-1"))
        .await
        .expect("stamped");
    tx.commit().await.unwrap();
    let s = seen("note:stamped");
    assert!(!s.is_empty() && s.iter().all(|x| *x == (Made::System, None, false)), "{}: {s:?}", "the effect runs as the system, not as the app");

    // ── a_mapping_run_as_the_system_is_not_at_a_screen ──
    let desc = json!({"test": true, "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "token"},
        "map": {"entity": "Note", "by": {"title": "item.title"}, "as": "system"}}});
    let input: ferrum_kernel::connections::ConnectionInput =
        serde_json::from_value(json!({"name": "feed", "label": "Feed", "description": desc, "secrets": {"token": "tok-12345678"}, "enabled": true})).unwrap();
    let feed = k.save_connection_def(&ws.admin, None, &input).await.unwrap();
    let headers = BTreeMap::from([("x-token".to_string(), "tok-12345678".to_string())]);
    k.connection_inbound(feed, &headers, &BTreeMap::new(), br#"{"title": "by mapping"}"#).await.unwrap();
    while k.run_jobs_once(10).await.unwrap() > 0 {}
    let s = seen("note:by mapping");
    assert!(!s.is_empty() && s.iter().all(|x| *x == (Made::System, None, false)), "{}: {s:?}", "a mapping run as the system");
    let _ = ben;

    // ── approval-step-up: a policy's step_up_within asks the asker and each decider for a sign-in check that recent ──
    let fifth = payout("Settle the float").await;
    let asha_screen = request(&kit, &asha, true).await;
    let said = |r: ferrum_kernel::Result<Value>| match r {
        Err(Error::Forbidden(m)) => m,
        other => panic!("refused, not {other:?}"),
    };
    assert!(said(k.run_action(&asha_screen, "Payout", fifth, "settle", json!({})).await).contains("sign-in check"), "an asker without one");
    let asked = k.run_action(&asha_screen.clone().proved(Some(chrono::Utc::now())), "Payout", fifth, "settle", json!({})).await.expect("asked");
    assert!(asked.get("_approval").is_some(), "{asked}");
    let carla_now = request(&kit, &carla, true).await;
    assert!(
        matches!(k.decide_approval(&carla_now, "Payout", fifth, true, None).await, Err(Error::Forbidden(m)) if m.contains("sign-in check")),
        "a decider without one"
    );
    let stale = carla_now.clone().proved(Some(chrono::Utc::now() - chrono::Duration::seconds(601)));
    assert!(matches!(k.decide_approval(&stale, "Payout", fifth, true, None).await, Err(Error::Forbidden(_))), "nor with one older than the policy says");
    k.decide_approval(&carla_now.proved(Some(chrono::Utc::now())), "Payout", fifth, true, None).await.expect("decided with a fresh one");
    // ── a_policy_with_step_up_within_and_without_people_at_screens_is_refused_at_load ──
    let package = PACKAGE.replace("people_at_screens: true, step_up_within: 600", "step_up_within: 600");
    assert_ne!(package, PACKAGE);
    let files: BTreeMap<String, String> = [("package.yaml", package.as_str()), ("entities/payout.yaml", ENTITIES), ("entities/note.yaml", NOTE)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let core = ferrum_meta::catalog::load_package_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).unwrap();
    let refused = match ferrum_meta::catalog::load_package_files(&files) {
        Err(e) => Some(e.to_string()),
        Ok(p) => ferrum_meta::Catalog::from_packages(vec![core, p]).err().map(|e| e.to_string()),
    };
    assert!(refused.as_deref().is_some_and(|e| e.contains("people_at_screens")), "{refused:?}");
}
