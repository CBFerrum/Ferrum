//! Who is acting (S1-P0c, docs/builder/decisions.md BD-83), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! kit empties it): every place a context is made marks it as what it is (a person at a screen, a key, an automation, a
//! job, an approval completing, the assistant, the system), and `Ctx::at_screen()` holds only for a person at a screen in
//! this request. An approval policy with `people_at_screens` refuses anyone else, asking or deciding. Which credentials
//! make a screen is decided by the token's claims in `server/src/extract.rs` (`screens_are_decided_by_claims`), and
//! `scripts/check_contexts.py` holds that no other code makes one. What an app's call names: `app_uses_it.rs`.

use ferrum_kernel::controls::Proof;
use ferrum_kernel::ctx::Egress;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Ctx, Error, KernelSettings, Made};
use ferrum_testkit::{Kit, database, obj, run, uid};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const PACKAGE: &str = "package: till\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Cashier\n    permissions:\n      - {entity: Payout, read: true, write: true, create: true}\n\
approvals:\n  - entity: Payout\n    on: {action: release}\n    policy: {steps: [{label: A second administrator, approvers: {administrators: true}}], people_at_screens: true}\n";
const ENTITIES: &str = "name: Payout\nlabel: Payout\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n\
actions:\n  - {name: release, label: Release, permission: write}\n";

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
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/payout.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let till = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let kit = Kit::boot_with(&database("FERRUM_TEST_DATABASE_URL"), &[], vec![till], KernelSettings::default()).await;
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
        let files: BTreeMap<String, String> =
            [("package.yaml", package.as_str()), ("entities/payout.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let core = ferrum_meta::catalog::load_package_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).unwrap();
        let refused = match ferrum_meta::catalog::load_package_files(&files) {
            Err(e) => Some(e.to_string()),
            Ok(p) => ferrum_meta::Catalog::from_packages(vec![core, p]).err().map(|e| e.to_string()),
        };
        assert!(refused.as_deref().is_some_and(|e| e.contains("in person")), "{extra}: {refused:?}");
    }
    let _ = ben;
}
