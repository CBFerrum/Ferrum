//! Agents as identities (gapclose/design/A2-A3-agents.md, AG-1) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`,
//! emptied first): who makes an agent and what it may hold, never more than its owner and never an administrator; the
//! switch, a pause and a revocation refusing the next request on any replica; its rates; its keys; its owner's erasure;
//! a sandbox's copy; and the build switch's decision.

use ferrum_kernel::ctx::{Access, Action, action_scope, transition_gate};
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Ctx, Kernel, KernelSettings, Made, Result, agent};
use ferrum_meta::Package;
use ferrum_testkit::{Kit, Workspace, at_screen, database, obj, refused, run, uid};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::OnceCell;
use uuid::Uuid;

const PACKAGE: &str = r#"package: agentest
version: 0.1.0
depends: [core]
roles:
  - name: North Approver
    permissions:
      - {entity: Quote, read: true, write: true, create: true}
      - {entity: Quote, read: true, actions: [approve], where: {region: North}}
  - name: Clerk
    permissions:
      - {entity: Quote, read: true, write: true, create: true, if_owner: true}
"#;

const ENTITY: &str = r#"name: Quote
label: Quote
kind: master
title_field: title
fields:
  - {name: title, label: Title, type: data}
  - {name: region, label: Region, type: data}
  - {name: margin, label: Margin, type: data, perm_group: pricing}
actions:
  - name: approve
    label: Approve
    permission: write
    contract: {effect: transactional_action, completion: The quote is approved}
"#;

fn package() -> Package {
    let files = BTreeMap::from([("package.yaml".to_string(), PACKAGE.to_string()), ("entities/quote.yaml".to_string(), ENTITY.to_string())]);
    ferrum_meta::catalog::load_package_files(&files).expect("package")
}

static KIT: OnceCell<Kit> = OnceCell::const_new();

async fn kit() -> &'static Kit {
    KIT.get_or_init(|| async { Kit::boot_with(&database("FERRUM_TEST_DATABASE_URL"), &[], vec![package()], KernelSettings::default()).await }).await
}

async fn workspace(slug: &str) -> (&'static Kit, Workspace, Ctx) {
    let kit = kit().await;
    let ws = kit.workspace(slug, &["agentest"], Value::Null).await;
    let admin = at_screen(&kit.k, &ws.admin).await;
    (kit, ws, admin)
}

/// `ctx`'s person in a request their agent's credential made, from `address`.
async fn request(k: &Kernel, ctx: &Ctx, address: &str) -> Ctx {
    let entry = Arc::new(Entry { zones: Arc::new(Zones::default()), placed: Placed::Nowhere, address: address.into(), credential: Credential::Agent });
    k.request_ctx(ctx.tenant, ctx.user, None, "it".into(), &entry).await.expect("the owner's request")
}

/// Agent `agent`'s context for a request from 10.1.2.3, its owner being `owner`.
async fn as_agent(k: &Kernel, owner: &Ctx, agent: Uuid) -> Result<Ctx> {
    k.agent_ctx(&request(k, owner, "10.1.2.3").await, agent).await
}

async fn make(k: &Kernel, ctx: &Ctx, v: Value) -> Uuid {
    uid(&k.create_agent(ctx, &v).await.unwrap_or_else(|e| panic!("an agent from {v}: {e}")))
}

async fn settings(k: &Kernel, admin: &Ctx, agents: Value) {
    let patch = json!({"agents": agents});
    k.update_tenant_settings(admin, patch.as_object().unwrap()).await.expect("the agents setting");
}

async fn scalar<T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin>(k: &Kernel, sql: &str, id: Uuid) -> T {
    let mut tx = k.db.system_tx().await.unwrap();
    let v: T = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(id).fetch_one(&mut *tx).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    tx.commit().await.unwrap();
    v
}

/// A counted minute starts well before its end: the rates are counted in windows of whole minutes.
async fn fresh_minute() {
    let into = chrono::Utc::now().timestamp() % 60;
    if into > 45 {
        tokio::time::sleep(std::time::Duration::from_secs((61 - into) as u64)).await;
    }
}

#[test]
fn agents_are_refused_while_the_build_is_closed() {
    refused(agent::admitted(false), "agents closed", "not available yet");
    assert!(agent::admitted(true).is_ok());
    assert!(agent::admitted(agent::OPEN).is_ok(), "the test kit builds with agents-preview");
}

#[test]
fn grants_are_checked() {
    run(async {
        let (kit, _ws, admin) = workspace("grants").await;
        let k = &kit.k;
        let with = |grants: Value| json!({"name": "Helper", "grants": grants});
        refused(k.create_agent(&admin, &with(json!({"entities": {"Nothing": ["read"]}}))).await, "invalid", "Nothing is not an entity");
        refused(k.create_agent(&admin, &with(json!({"entities": {"Quote": ["rule"]}}))).await, "invalid", "rule is not something");
        refused(k.create_agent(&admin, &with(json!({"entities": {"Quote": []}}))).await, "invalid", "name what it may do");
        refused(k.create_agent(&admin, &with(json!({"entities": {"Quote": ["read field group payroll"]}}))).await, "invalid", "is not something");
        refused(k.create_agent(&admin, &with(json!({"operations": ["selling quote!"]}))).await, "invalid", "is not a capability");
        let many: serde_json::Map<String, Value> = (0..201).map(|i| (format!("E{i}"), json!(["read"]))).collect();
        refused(k.create_agent(&admin, &with(json!({"entities": many}))).await, "invalid", "at most 200");
        refused(k.create_agent(&admin, &with(json!({"entities": {"*": ["read"]}, "reach": "all"}))).await, "invalid", "unknown field");
        refused(k.create_agent(&admin, &json!({"name": "  ", "grants": {"entities": {"*": ["read"]}}})).await, "invalid", "1 to 100");
        let id = make(
            k,
            &admin,
            with(json!({"entities": {"Quote": ["read", "write (own records)", "read field group pricing"]}, "operations": ["agentest.quote.*"]})),
        )
        .await;
        let a = k.agent(&admin, id).await.unwrap();
        assert_eq!(a["grants"]["operations"], json!(["agentest.quote.*"]));
        // None given: reading, nothing else.
        let plain = k.create_agent(&admin, &json!({"name": "Reader"})).await.unwrap();
        assert_eq!(plain["grants"], json!({"entities": {"*": ["read"]}, "operations": []}));
    });
}

#[test]
fn owners_set_only_stricter_classes() {
    run(async {
        let (kit, ws, admin) = workspace("classes").await;
        let k = &kit.k;
        let asha = at_screen(k, &ws.user("asha@classes.test", &["Clerk"]).await).await;
        refused(k.create_agent(&asha, &json!({"name": "Spender", "classes": {"money": "allowed"}})).await, "forbidden", "only make money stricter");
        refused(k.create_agent(&admin, &json!({"name": "Decider", "classes": {"decides": "allowed"}})).await, "invalid", "decides waits for a person");
        refused(k.create_agent(&asha, &json!({"name": "Shy", "classes": {"unclassified": "allowed"}})).await, "invalid", "cannot be allowed");
        let a = k.create_agent(&asha, &json!({"name": "Careful", "classes": {"external": "forbidden"}})).await.unwrap();
        assert_eq!(a["classes"]["effective"]["external"], "forbidden");
    });
}

#[test]
fn an_administrators_classes_stand_and_an_owners_never_undercut_the_workspace() {
    run(async {
        let (kit, ws, admin) = workspace("exceptions").await;
        let k = &kit.k;
        let asha = at_screen(k, &ws.user("asha@exceptions.test", &["Clerk"]).await).await;
        let own = make(k, &asha, json!({"name": "Careful", "classes": {"external": "forbidden", "transactional": "needs_person"}})).await;
        // An administrator may let one agent move money without a person.
        let trusted = make(k, &admin, json!({"name": "Trusted", "owner": asha.user, "classes": {"money": "allowed"}})).await;
        let a = k.agent(&asha, trusted).await.unwrap();
        assert_eq!(a["classes"]["effective"]["money"], "allowed");
        assert_eq!(a["classes"]["set_by_administrator"], json!(["money"]));
        // Its owner cannot undo or widen what an administrator set, but may add stricter classes of their own.
        let r = a["revision"].as_i64().unwrap();
        refused(k.update_agent(&asha, trusted, &json!({"revision": r, "classes": {}})).await, "forbidden", "set by an administrator");
        refused(k.update_agent(&asha, trusted, &json!({"revision": r, "classes": {"money": "needs_person"}})).await, "forbidden", "set by an administrator");
        let a = k.update_agent(&asha, trusted, &json!({"revision": r, "classes": {"money": "allowed", "destructive": "forbidden"}})).await.unwrap();
        assert_eq!((a["classes"]["effective"]["money"].clone(), a["classes"]["effective"]["destructive"].clone()), (json!("allowed"), json!("forbidden")));
        refused(k.update_agent(&asha, trusted, &json!({"revision": r, "name": "Late"})).await, "conflict", "changed since it was read");
        // The workspace made stricter later: an owner's earlier choice never undercuts it; an administrator's stands.
        settings(k, &admin, json!({"classes": {"transactional": "forbidden", "money": "forbidden"}})).await;
        let careful = as_agent(k, &asha, own).await.unwrap();
        assert_eq!(
            json!(careful.agent.as_ref().unwrap().table),
            json!({"transactional": "forbidden", "external": "forbidden", "money": "forbidden", "destructive": "needs_person", "decides": "needs_person", "unclassified": "needs_person"}),
            "an owner's earlier choice never undercuts the workspace's"
        );
        let trusted = as_agent(k, &asha, trusted).await.unwrap();
        assert_eq!(json!(trusted.agent.as_ref().unwrap().table)["money"], "allowed");
    });
}

#[test]
fn limits_stay_within_the_workspace_caps() {
    run(async {
        let (kit, ws, admin) = workspace("limits").await;
        let k = &kit.k;
        let asha = at_screen(k, &ws.user("asha@limits.test", &["Clerk"]).await).await;
        let with = |limits: Value| json!({"name": "Limited", "limits": limits});
        refused(k.create_agent(&asha, &with(json!({"calls_per_minute": 301}))).await, "invalid", "1 to 300");
        refused(k.create_agent(&asha, &with(json!({"records_per_call": 0}))).await, "invalid", "records_per_call: 1 to 200");
        refused(k.create_agent(&asha, &with(json!({"records_per_call": 201}))).await, "invalid", "records_per_call: 1 to 200");
        refused(k.create_agent(&asha, &with(json!({"money": {"INR": {"per_call": "10", "per_day": "20"}}}))).await, "forbidden", "money ceilings");
        refused(k.create_agent(&admin, &with(json!({"money": {"Rupees": {"per_call": "10", "per_day": "20"}}}))).await, "invalid", "not a currency");
        refused(k.create_agent(&admin, &with(json!({"money": {"INR": {"per_call": "50", "per_day": "20"}}}))).await, "invalid", "per_day at least");
        let many: serde_json::Map<String, Value> =
            (0..21u8).map(|i| (format!("A{}{}", (b'A' + i / 10) as char, (b'A' + i % 10) as char), json!({"per_call": "1", "per_day": "1"}))).collect();
        refused(k.create_agent(&admin, &with(json!({"money": many}))).await, "invalid", "at most 20");
        let id = make(
            k,
            &admin,
            json!({"name": "Payer", "owner": asha.user, "limits": {"calls_per_minute": 300, "money": {"INR": {"per_call": "50000", "per_day": "200000"}}}}),
        )
        .await;
        // An owner's change of the limits leaves an administrator's ceilings as they are.
        let r = k.agent(&asha, id).await.unwrap()["revision"].as_i64().unwrap();
        refused(
            k.update_agent(&asha, id, &json!({"revision": r, "limits": {"money": {"INR": {"per_call": "1", "per_day": "1"}}}})).await,
            "forbidden",
            "money ceilings",
        );
        let a = k.update_agent(&asha, id, &json!({"revision": r, "limits": {"calls_per_minute": 100}})).await.unwrap();
        assert_eq!(a["limits"]["money"]["INR"]["per_call"], "50000");
        // Caps lowered later hold over what an agent was given.
        settings(k, &admin, json!({"caps": {"calls_per_minute": 60}})).await;
        let a = k.agent(&asha, id).await.unwrap();
        assert_eq!((a["limits"]["calls_per_minute"].clone(), a["limits"]["records_per_call"].clone()), (json!(60), json!(100)));
        refused(k.update_tenant_settings(&admin, json!({"agents": {"caps": {"calls_per_minute": 0}}}).as_object().unwrap()).await, "invalid", "1 or more");
    });
}

#[test]
fn a_member_makes_an_agent_for_themselves_only() {
    run(async {
        let (kit, ws, _admin) = workspace("members").await;
        let k = &kit.k;
        let asha = ws.user("asha@members.test", &["Clerk"]).await;
        let bo = ws.user("bo@members.test", &["Clerk"]).await;
        let screen = at_screen(k, &asha).await;
        refused(k.create_agent(&screen, &json!({"name": "Bo's", "owner": bo.user})).await, "forbidden", "for yourself only");
        // Never by a program holding the person's credential, nor from a job.
        refused(k.create_agent(&asha, &json!({"name": "Mine"})).await, "at screen", "by a person at a screen");
        let key = request(k, &asha, "10.1.2.3").await;
        assert_eq!(key.made, Made::Key);
        refused(k.create_agent(&key, &json!({"name": "Mine"})).await, "at screen", "by a person at a screen");
        let a = k.create_agent(&screen, &json!({"name": "Mine"})).await.unwrap();
        assert_eq!((a["owner"]["id"].clone(), a["created_by"].clone()), (json!(asha.user), json!(asha.user)));
        refused(k.create_agent(&screen, &json!({"name": " mine "})).await, "conflict", "already an agent named mine");
    });
}

#[test]
fn a_member_sees_only_their_own_agents() {
    run(async {
        let (kit, ws, _admin) = workspace("sees").await;
        let k = &kit.k;
        let asha = ws.user("asha@sees.test", &["Clerk"]).await;
        let bo = ws.user("bo@sees.test", &["Clerk"]).await;
        let id = make(k, &at_screen(k, &asha).await, json!({"name": "Mine"})).await;
        refused(k.agent(&bo, id).await, "not found", "agent");
        refused(k.agent_usage(&bo, id, 30).await, "not found", "agent");
        assert!(k.agents(&bo, None, None).await.unwrap().is_empty(), "Bo lists none of Asha's");
        refused(k.agents(&bo, Some(asha.user), None).await, "forbidden", "your own agents");
        assert_eq!(k.agents(&asha, None, None).await.unwrap().len(), 1);
    });
}

#[test]
fn an_administrator_binds_an_oauth_client_for_a_member() {
    run(async {
        let (kit, ws, admin) = workspace("binds").await;
        let k = &kit.k;
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("insert into oauth_client (id, name, redirect_uris) values ('fc_binds', 'Claude', '{https://claude.example/cb}')")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let asha = at_screen(k, &ws.user("asha@binds.test", &["Clerk"]).await).await;
        refused(k.create_agent(&asha, &json!({"name": "Mine", "oauth_client": "fc_binds"})).await, "forbidden", "set by an administrator");
        refused(k.create_agent(&admin, &json!({"name": "Hers", "owner": asha.user, "oauth_client": "fc_nobody"})).await, "invalid", "no such OAuth client");
        let a = k.create_agent(&admin, &json!({"name": "Hers", "owner": asha.user, "oauth_client": "fc_binds"})).await.unwrap();
        assert_eq!(a["connected"]["client_name"], "Claude");
        assert_eq!(a["owner"]["id"], json!(asha.user));
        refused(
            k.create_agent(&admin, &json!({"name": "Again", "owner": asha.user, "oauth_client": "fc_binds"})).await,
            "conflict",
            "already has an agent for that OAuth client",
        );
        // Only an active member owns one.
        let gone = ws.user("gone@binds.test", &["Clerk"]).await;
        k.set_user_active(&ws.admin, gone.user, false).await.unwrap();
        refused(k.create_agent(&admin, &json!({"name": "Ghost's", "owner": gone.user})).await, "invalid", "an active member");
        refused(k.create_agent(&admin, &json!({"name": "Nobody's", "owner": Uuid::now_v7()})).await, "invalid", "an active member");
    });
}

#[test]
fn members_may_not_when_the_workspace_says_so() {
    run(async {
        let (kit, ws, admin) = workspace("nomembers").await;
        let k = &kit.k;
        let asha = at_screen(k, &ws.user("asha@nomembers.test", &["Clerk"]).await).await;
        // The setting is an administrator's, at a screen, checked whole.
        let patch = json!({"agents": {"members_may_create": false}});
        refused(k.update_tenant_settings(&ws.admin, patch.as_object().unwrap()).await, "at screen", "by a person at a screen");
        refused(
            k.update_tenant_settings(&admin, json!({"agents": {"classes": {"unclassified": "allowed"}}}).as_object().unwrap()).await,
            "invalid",
            "cannot be allowed",
        );
        refused(k.update_tenant_settings(&admin, json!({"agents": {"members": false}}).as_object().unwrap()).await, "invalid", "unknown field");
        refused(k.update_tenant_settings(&admin, json!({"agents": {"approver_role": "Nobody"}}).as_object().unwrap()).await, "invalid", "not a role");
        settings(k, &admin, json!({"members_may_create": false, "approver_role": "Clerk"})).await;
        assert_eq!(k.tenant_settings(&admin).await.unwrap()["agents"]["members_may_create"], false);
        refused(k.create_agent(&asha, &json!({"name": "Mine"})).await, "forbidden", "administrators connect agents");
        let id = make(k, &admin, json!({"name": "Hers", "owner": asha.user})).await;
        assert_eq!(as_agent(k, &asha, id).await.unwrap().agent.as_ref().unwrap().approver_role.as_deref(), Some("Clerk"));
        // Agents off: none is made.
        settings(k, &admin, json!({"enabled": false})).await;
        refused(k.create_agent(&admin, &json!({"name": "Another", "owner": asha.user})).await, "agents off", "turned off");
    });
}

#[test]
fn an_agent_context_cannot_manage_agents() {
    run(async {
        let (kit, ws, admin) = workspace("selfmade").await;
        let k = &kit.k;
        let own = make(k, &admin, json!({"name": "Mine", "grants": {"entities": {"*": ["read", "write", "create"]}}})).await;
        let agent = as_agent(k, &admin, own).await.unwrap();
        assert!(!agent.at_screen());
        refused(k.create_agent(&agent, &json!({"name": "Child"})).await, "at screen", "making an agent");
        refused(k.update_agent(&agent, own, &json!({"revision": 1, "name": "Renamed"})).await, "at screen", "changing an agent");
        refused(k.set_agent_state(&agent, own, "resume").await, "at screen", "revoking an agent");
        refused(k.create_agent_key(&agent, own, "more", None).await, "at screen", "an agent's key");
        let (key, _) = k.create_agent_key(&admin, own, "its", None).await.unwrap();
        refused(k.revoke_agent_key(&agent, own, key).await, "at screen", "an agent's key");
        // Nor a key of its owner's, nor the workspace's settings.
        refused(k.create_api_key(&agent, agent.user, "escape", None, None).await, "at screen", "managing API keys");
        refused(k.update_tenant_settings(&agent, json!({"assistant": false}).as_object().unwrap()).await, "forbidden", "only administrators");
        // The owner's own credential in a program (a key, an app's token) cannot either (A2-A3 Q21).
        let keyed = request(k, &ws.admin, "10.1.2.3").await;
        refused(k.set_agent_state(&keyed, own, "pause").await, "at screen", "by a person at a screen");
    });
}

#[test]
fn revoking_ends_keys() {
    run(async {
        let (kit, _ws, admin) = workspace("keys").await;
        let k = &kit.k;
        let id = make(k, &admin, json!({"name": "Keyed"})).await;
        let (first, secret) = k.create_agent_key(&admin, id, "laptop", None).await.unwrap();
        assert!(secret.starts_with("fk_"), "shown once, as a key is");
        // An agent's key is never its owner's key: nothing opens with it until agents sign in (AG-3).
        refused(k.verify_api_key(&secret).await, "unauthorized", "invalid API key");
        let listed = k.agent_keys(&admin, id).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!json!(listed).to_string().contains(&secret[3..]), "no secret is shown again");
        for n in 2..=5 {
            k.create_agent_key(&admin, id, &format!("key {n}"), None).await.unwrap();
        }
        refused(k.create_agent_key(&admin, id, "sixth", None).await, "conflict", "at most 5 live keys");
        k.revoke_agent_key(&admin, id, first).await.unwrap();
        k.create_agent_key(&admin, id, "fifth again", None).await.unwrap();
        k.set_agent_state(&admin, id, "revoke").await.unwrap();
        let live: i64 = scalar(k, "select count(*) from api_key where agent_id = $1 and revoked_at is null", id).await;
        assert_eq!(live, 0, "revoking the agent revoked every key");
        refused(k.create_agent_key(&admin, id, "after", None).await, "conflict", "takes no new key");
        refused(k.set_agent_state(&admin, id, "resume").await, "conflict", "revoked for good");
        refused(k.update_agent(&admin, id, &json!({"revision": 1, "name": "Back"})).await, "conflict", "changes no more");
    });
}

#[test]
fn keys_and_agents_are_counted_under_a_lock() {
    run(async {
        let (kit, ws, admin) = workspace("counted").await;
        let k = kit.k.clone();
        let asha = ws.user("asha@counted.test", &["Clerk"]).await;
        for i in 0..19 {
            make(&k, &admin, json!({"name": format!("Agent {i}"), "owner": asha.user})).await;
        }
        // Another transaction makes the 20th under the owner's lock: the 21st waits for it, then counts it.
        let mut held = k.db.system_tx().await.unwrap();
        sqlx::query("select pg_advisory_xact_lock(hashtext('ferrum.agent'), hashtext($1::text))").bind(asha.user).execute(&mut *held).await.unwrap();
        sqlx::query("insert into agent (tenant_id, id, owner_id, name, grants, created_by) values ($1, $2, $3, 'Agent 19', '{}', $3)")
            .bind(ws.tenant)
            .bind(Uuid::now_v7())
            .bind(asha.user)
            .execute(&mut *held)
            .await
            .unwrap();
        let (k2, admin2, owner) = (k.clone(), admin.clone(), asha.user);
        let late = tokio::spawn(async move { k2.create_agent(&admin2, &json!({"name": "Agent 20", "owner": owner})).await });
        waits_for_a_lock(&k, &["advisory"]).await;
        held.commit().await.unwrap();
        refused(late.await.unwrap(), "conflict", "at most 20 agents");
        // An agent's keys alike, under its row's lock.
        let id = make(&k, &admin, json!({"name": "Keyed"})).await;
        for n in 0..4 {
            k.create_agent_key(&admin, id, &format!("key {n}"), None).await.unwrap();
        }
        let mut held = k.db.system_tx().await.unwrap();
        sqlx::query("select 1 from agent where id = $1 for update").bind(id).execute(&mut *held).await.unwrap();
        sqlx::query("insert into api_key (tenant_id, id, user_id, agent_id, name, prefix, secret_hash) values ($1, $2, $3, $4, 'fifth', $5, 'x')")
            .bind(ws.tenant)
            .bind(Uuid::now_v7())
            .bind(admin.user)
            .bind(id)
            .bind(format!("p{}", &Uuid::now_v7().simple().to_string()[20..]))
            .execute(&mut *held)
            .await
            .unwrap();
        let (k2, admin2) = (k.clone(), admin.clone());
        let late = tokio::spawn(async move { k2.create_agent_key(&admin2, id, "sixth", None).await });
        waits_for_a_lock(&k, &["transactionid", "tuple"]).await;
        held.commit().await.unwrap();
        refused(late.await.unwrap(), "conflict", "at most 5 live keys");
    });
}

/// Until some statement of this database waits for a lock of kind `kind` (`advisory`, or a row's: `transactionid`, `tuple`)
/// another holds, at most ten seconds.
async fn waits_for_a_lock(k: &Kernel, kind: &[&str]) {
    for _ in 0..200 {
        let waiting: bool = sqlx::query_scalar(
            "select exists (select 1 from pg_stat_activity where wait_event_type = 'Lock' and wait_event = any($1) and datname = current_database())",
        )
        .bind(kind)
        .fetch_one(&k.db.pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("nothing waited for the lock: the count was not made under it");
}

#[test]
fn the_owner_never_changes() {
    run(async {
        let (kit, ws, admin) = workspace("owner").await;
        let k = &kit.k;
        let bo = ws.user("bo@owner.test", &["Clerk"]).await;
        let id = make(k, &admin, json!({"name": "Fixed"})).await;
        for sql in ["update agent set owner_id = $2 where id = $1", "update agent set created_by = $2 where id = $1"] {
            let mut tx = k.db.system_tx().await.unwrap();
            let r = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).bind(bo.user).execute(&mut *tx).await;
            assert!(r.is_err(), "{sql} was let through");
        }
        // Any other change goes as before.
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update agent set name = 'Renamed' where id = $1").bind(id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    });
}

#[test]
fn a_credential_names_its_agents_owner() {
    run(async {
        let (kit, ws, admin) = workspace("credential").await;
        let k = &kit.k;
        let bo = ws.user("bo@credential.test", &["Clerk"]).await;
        let id = make(k, &admin, json!({"name": "Admin's"})).await;
        let key = |user: Uuid, scope: Option<&'static str>| {
            let (k, tenant) = (k.clone(), ws.tenant);
            async move {
                let mut tx = k.db.system_tx().await.unwrap();
                let r = sqlx::query(
                    "insert into api_key (tenant_id, id, user_id, agent_id, name, prefix, secret_hash, scope) values ($1, $2, $3, $4, 'k', $5, 'x', $6)",
                )
                .bind(tenant)
                .bind(Uuid::now_v7())
                .bind(user)
                .bind(id)
                .bind(format!("q{}", &Uuid::now_v7().simple().to_string()[20..]))
                .bind(scope)
                .execute(&mut *tx)
                .await;
                let _ = tx.commit().await;
                r.is_ok()
            }
        };
        assert!(!key(bo.user, None).await, "a key of the agent's is its owner's, never another's");
        assert!(!key(admin.user, Some("dav")).await, "an agent's key has no single use");
        assert!(key(admin.user, None).await);
    });
}

#[test]
fn narrowing_never_exceeds_the_owner() {
    run(async {
        let (kit, ws, admin) = workspace("narrow").await;
        let k = &kit.k;
        let clerk = ws.user("clerk@narrow.test", &["Clerk"]).await;
        let all = json!(["read", "write", "create", "delete", "submit", "cancel", "export", "import", "read field group pricing", "write field group pricing"]);
        let id = make(k, &admin, json!({"name": "Everything", "owner": clerk.user, "grants": {"entities": {"*": all, "Quote": all}}})).await;
        let agent = as_agent(k, &clerk, id).await.unwrap();
        let owner = request(k, &clerk, "10.1.2.3").await;
        for entity in owner.schema.entities.keys() {
            for action in [Action::Read, Action::Write, Action::Create, Action::Delete, Action::Submit, Action::Cancel, Action::Export, Action::Import] {
                assert!(agent.perms.access(entity, action) <= owner.perms.access(entity, action), "never more than its owner: {entity} {action:?}");
            }
        }
        assert_eq!(agent.perms.access("Quote", Action::Write), Access::Own, "the clerk's own quotes, no more");
        assert!(agent.perms.grants("Quote", Action::Write).unwrap().iter().all(|g| g.owner));
        let margin = owner.schema.entity("Quote").unwrap().field("margin").unwrap().clone();
        assert!(!agent.perms.can_read_field("Quote", &margin), "a field group its owner lacks");
        // What it reads is narrowed as what goes to an AI provider is.
        assert!(agent.egress.is_some());
    });
}

#[test]
fn a_named_action_rule_stays_narrow() {
    run(async {
        let (kit, ws, admin) = workspace("named").await;
        let k = &kit.k;
        let nia = ws.user("nia@named.test", &["North Approver"]).await;
        let owner = request(k, &nia, "10.1.2.3").await;
        let ent = owner.schema.entity("Quote").unwrap().clone();
        let quote = |region: &str, by: Uuid| obj(json!({"id": Uuid::now_v7(), "region": region, "created_by": by}));
        let (north, south, own_north) = (quote("North", admin.user), quote("South", admin.user), quote("North", nia.user));
        let reaches = |ctx: &Ctx| -> Vec<bool> {
            match action_scope(ctx, &ent, "approve", Action::Write) {
                Ok(scope) => vec![scope.matches(&north), scope.matches(&south), scope.matches(&own_north)],
                Err(_) => vec![],
            }
        };
        // Her rule names approve for northern quotes only, though she may change every quote.
        assert_eq!(reaches(&owner), vec![true, false, true]);
        let narrowed = |grants: &[(&str, &[&str])]| -> Ctx {
            let declared: BTreeMap<String, BTreeSet<String>> = grants.iter().map(|(e, v)| (e.to_string(), v.iter().map(|s| s.to_string()).collect())).collect();
            Ctx { perms: Arc::new(owner.perms.limited_to(&declared, &BTreeSet::new(), &owner.schema)), is_admin: false, ..owner.clone() }
        };
        // A records key, an app's screen: as narrow as her rule.
        let key = ferrum_kernel::auth::records_scope("records:Quote=read+write").unwrap();
        let keyed = Ctx { perms: Arc::new(owner.perms.limited_to(&key, &BTreeSet::new(), &owner.schema)), is_admin: false, ..owner.clone() };
        assert_eq!(reaches(&keyed), vec![true, false, true], "a records key");
        assert_eq!(reaches(&narrowed(&[("Quote", &["read", "write"])])), vec![true, false, true], "an app's screen");
        assert_eq!(reaches(&narrowed(&[("Quote", &["read", "write (own records)"])])), vec![false, false, true], "own records only");
        assert!(reaches(&narrowed(&[("Quote", &["read"])])).is_empty(), "no leave to change: refused");
        // An agent.
        let id = make(k, &admin, json!({"name": "Approver's", "owner": nia.user, "grants": {"entities": {"Quote": ["read", "write"]}}})).await;
        assert_eq!(reaches(&as_agent(k, &nia, id).await.unwrap()), vec![true, false, true], "an agent");
    });
}

#[test]
fn a_narrowed_context_takes_a_role_gated_transition_only_with_leave_to_change() {
    run(async {
        let (kit, ws, admin) = workspace("transition").await;
        let k = &kit.k;
        let nia = ws.user("nia@transition.test", &["North Approver"]).await;
        let owner = request(k, &nia, "10.1.2.3").await;
        let ent = owner.schema.entity("Quote").unwrap().clone();
        let t: ferrum_meta::WorkflowTransition =
            serde_json::from_value(json!({"action": "Close", "from": "Open", "to": "Closed", "roles": ["North Approver"]})).unwrap();
        assert!(transition_gate(&owner, &ent, &t).is_ok());
        let reader = make(k, &admin, json!({"name": "Reader", "owner": nia.user, "grants": {"entities": {"Quote": ["read"]}}})).await;
        refused(transition_gate(&as_agent(k, &nia, reader).await.unwrap(), &ent, &t), "forbidden", "not permitted to write quote");
        let key = ferrum_kernel::auth::records_scope("records:Quote=read").unwrap();
        let keyed = Ctx { perms: Arc::new(owner.perms.limited_to(&key, &BTreeSet::new(), &owner.schema)), is_admin: false, ..owner.clone() };
        refused(transition_gate(&keyed, &ent, &t), "forbidden", "not permitted to write quote");
        let writer = make(k, &admin, json!({"name": "Writer", "owner": nia.user, "grants": {"entities": {"Quote": ["read", "write"]}}})).await;
        assert!(transition_gate(&as_agent(k, &nia, writer).await.unwrap(), &ent, &t).is_ok());
    });
}

#[test]
fn an_admins_agent_is_no_admin() {
    run(async {
        let (kit, _ws, admin) = workspace("adminagent").await;
        let k = &kit.k;
        let id = make(k, &admin, json!({"name": "Admin's"})).await;
        let agent = as_agent(k, &admin, id).await.unwrap();
        assert!(!agent.is_admin && !agent.perms.admin && agent.admin_since.is_none());
        assert_eq!((agent.made, agent.proof), (Made::Agent, ferrum_kernel::controls::Proof::Machine));
        assert_eq!(agent.perms.access("Quote", Action::Read), Access::All);
        refused(agent.require("Quote", Action::Write), "forbidden", "not permitted to write quote");
        refused(k.tenant_settings(&agent).await, "forbidden", "only administrators");
        // The system acting inside an agent's call is not the agent: what it does is checked where it passes (AG-6).
        assert!(agent.clone().elevated().agent.is_none());
        assert_eq!(agent.user_value()["agent"], json!(id.to_string()));
    });
}

#[test]
fn the_switch_and_a_pause_refuse_the_next_request() {
    run(async {
        let (kit, ws, admin) = workspace("switch").await;
        let k = &kit.k;
        let other = kit.replica().await;
        let asha = ws.user("asha@switch.test", &["Clerk"]).await;
        let screen = at_screen(k, &asha).await;
        let id = make(k, &screen, json!({"name": "Mine"})).await;
        as_agent(k, &asha, id).await.unwrap();
        settings(k, &admin, json!({"enabled": false})).await;
        refused(as_agent(&other, &asha, id).await, "agents off", "turned off");
        settings(k, &admin, json!({"enabled": true})).await;
        k.set_agent_state(&screen, id, "pause").await.unwrap();
        refused(as_agent(&other, &asha, id).await, "agent paused", "paused");
        assert_eq!(k.set_agent_state(&screen, id, "pause").await.unwrap()["state"], "paused", "the same state again does nothing");
        k.set_agent_state(&screen, id, "resume").await.unwrap();
        as_agent(&other, &asha, id).await.unwrap();
    });
}

#[test]
fn only_its_owners_live_credential_opens_it() {
    run(async {
        let (kit, ws, admin) = workspace("live").await;
        let k = &kit.k;
        let asha = ws.user("asha@live.test", &["Clerk"]).await;
        let screen = at_screen(k, &asha).await;
        let id = make(k, &screen, json!({"name": "Mine"})).await;
        as_agent(k, &asha, id).await.unwrap();
        // Nobody else's credential opens it.
        refused(as_agent(k, &ws.admin, id).await, "unauthorized", "not valid");
        // Its owner no longer an active member: refused.
        let owner = request(k, &asha, "10.1.2.3").await;
        k.set_user_active(&ws.admin, asha.user, false).await.unwrap();
        refused(k.agent_ctx(&owner, id).await, "forbidden", "active member");
        k.set_user_active(&ws.admin, asha.user, true).await.unwrap();
        // Expired, or revoked: refused.
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update agent set expires_at = now() - interval '1 second' where id = $1").bind(id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        refused(as_agent(k, &asha, id).await, "unauthorized", "not valid");
        let second = make(k, &screen, json!({"name": "Second"})).await;
        k.set_agent_state(&admin, second, "revoke").await.unwrap();
        refused(as_agent(k, &asha, second).await, "unauthorized", "not valid");
    });
}

#[test]
fn its_own_networks_or_the_staff_networks_hold_it() {
    run(async {
        let (kit, ws, _admin) = workspace("nets").await;
        let k = &kit.k;
        let asha = ws.user("asha@nets.test", &["Clerk"]).await;
        let screen = at_screen(k, &asha).await;
        let id = make(k, &screen, json!({"name": "Mine", "networks": ["10.9.0.0/16"]})).await;
        refused(as_agent(k, &asha, id).await, "network", "cannot be used from this network");
        k.agent_ctx(&request(k, &asha, "10.9.1.1").await, id).await.unwrap();
        k.update_agent(&screen, id, &json!({"revision": 1, "networks": null})).await.unwrap();
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update tenant set settings = settings || '{\"staff_networks\": {\"networks\": [\"10.1.0.0/16\"], \"mode\": \"enforce\"}}' where id = $1")
            .bind(ws.tenant)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        as_agent(k, &asha, id).await.unwrap();
        refused(k.agent_ctx(&request(k, &asha, "192.0.2.7").await, id).await, "network", "cannot be used from this network");
    });
}

#[test]
fn owner_and_agent_rates_are_counted() {
    run(async {
        let (kit, ws, admin) = workspace("rates").await;
        let k = &kit.k;
        let asha = ws.user("asha@rates.test", &["Clerk"]).await;
        settings(k, &admin, json!({"caps": {"owner_calls_per_minute": 5}})).await;
        let one = make(k, &admin, json!({"name": "One", "owner": asha.user, "limits": {"calls_per_minute": 3}})).await;
        let two = make(k, &admin, json!({"name": "Two", "owner": asha.user})).await;
        fresh_minute().await;
        for _ in 0..3 {
            as_agent(k, &asha, one).await.unwrap();
        }
        refused(as_agent(k, &asha, one).await, "too many requests", "too many calls this minute");
        // Its owner's agents together: four counted so far, the fifth passes, the sixth does not.
        as_agent(k, &asha, two).await.unwrap();
        refused(as_agent(k, &asha, two).await, "too many requests", "too many calls this minute");
        let usage = k.agent_usage(&admin, one, 30).await.unwrap();
        assert_eq!((usage["days"][0]["calls"].clone(), usage["days"][0]["refused"].clone()), (json!(3), json!(1)));
        let a = k.agent(&admin, one).await.unwrap();
        assert!(a["last_used_at"].is_string());
        assert_eq!(a["usage_30d"]["calls"], 3);
    });
}

#[test]
fn erasing_the_owner_revokes_and_blanks_their_agents() {
    run(async {
        let (kit, ws, admin) = workspace("erase").await;
        let k = &kit.k;
        let asha = ws.user("asha@erase.test", &["Clerk"]).await;
        let id = make(k, &at_screen(k, &asha).await, json!({"name": "Asha's helper"})).await;
        k.create_agent_key(&admin, id, "laptop", None).await.unwrap();
        let held = k.personal_data(&ws.admin, "asha@erase.test").await.unwrap();
        assert_eq!(held["agents"][0]["name"], "Asha's helper", "the person's data lists their agents");
        k.erase_personal_data(&ws.admin, "asha@erase.test", "asked to be forgotten").await.unwrap();
        let (state, name): (String, String) = {
            let mut tx = k.db.system_tx().await.unwrap();
            let row = sqlx::query_as("select state, name from agent where id = $1").bind(id).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            row
        };
        assert_eq!(state, "revoked");
        assert!(name.starts_with("Erased agent ") && !name.contains("Asha"), "{name}");
        let keys: i64 = scalar(k, "select count(*) from api_key where agent_id = $1", id).await;
        assert_eq!(keys, 0);
    });
}

#[test]
fn a_sandbox_copies_agents_revoked() {
    run(async {
        let (kit, ws, admin) = workspace("sandboxed").await;
        let k = &kit.k;
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("insert into oauth_client (id, name, redirect_uris) values ('fc_sandboxed', 'Claude', '{https://claude.example/cb}')")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let asha = ws.user("asha@sandboxed.test", &["Clerk"]).await;
        let id = make(k, &admin, json!({"name": "Live", "owner": asha.user, "oauth_client": "fc_sandboxed"})).await;
        k.create_agent_key(&admin, id, "laptop", None).await.unwrap();
        as_agent(k, &asha, id).await.unwrap();
        let until = chrono::Utc::now().date_naive() + chrono::Duration::days(10);
        let copy = ferrum_kernel::sandbox::Copy { from: None, until, base_domain: None, not_sealed: vec![], resume: false, halt: None };
        k.copy_workspace("sandboxed", "sandboxed-try", &copy, &|_: &str| {}).await.expect("a sandbox");
        let mut tx = k.db.system_tx().await.unwrap();
        let (state, client, revoked): (String, Option<String>, bool) = sqlx::query_as(
            "select a.state, a.oauth_client, a.revoked_at is not null from agent a join tenant t on t.id = a.tenant_id where t.slug = 'sandboxed-try'",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let (days, keys): (i64, i64) = sqlx::query_as(
            "select (select count(*) from agent_day d join tenant t on t.id = d.tenant_id where t.slug = 'sandboxed-try'), \
                    (select count(*) from api_key k join tenant t on t.id = k.tenant_id where t.slug = 'sandboxed-try')",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!((state.as_str(), client, revoked), ("revoked", None, true));
        assert_eq!((days, keys), (0, 0), "its days and keys are the live workspace's");
        // The live one is untouched.
        assert_eq!(k.agent(&admin, id).await.unwrap()["state"], "active");
    });
}
