//! What a rule lets someone see, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database
//! is reset first). The rule set drives two paths, SQL for lists and a check in memory for one
//! record, and a list must never show what a direct read refuses, so every rule here is run through
//! both and compared with what the test works out for itself.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Action, Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, OverlayOp, PermissionDef};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Payee
  label: Payee
  kind: master
  title_field: name
  reference: restricted
  fields:
    - {name: name, type: data, required: true}
    - {name: region, type: data}
- name: Agent
  label: Agent
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
- name: Claim
  label: Claim
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: amount, type: decimal}
    - {name: level, type: int}
    - {name: due, type: date}
    - {name: filed_at, type: datetime}
    - {name: status, type: select, options: [Open, Closed]}
    - {name: payee, type: link, to: Payee}
    - {name: agent, type: link, to: Agent}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-rules-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("claims/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("claims/package.yaml"), "package: claims\nversion: 0.1.0\nlabel: Claims\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("claims/entities/claims.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

struct World {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
    clerk: Uuid,
}

impl World {
    /// The clerk holds exactly these rules on Claim, as a fresh context sees them.
    async fn rules(&self, rules: &[Value]) -> Ctx {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query("delete from role_permission where tenant_id = $1 and role = 'Clerk' and entity = 'Claim'")
            .bind(self.tenant)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        for rule in rules {
            let mut def = json!({"entity": "Claim", "read": true});
            def.as_object_mut().unwrap().extend(rule.as_object().unwrap().clone());
            let def: PermissionDef = serde_json::from_value(def).expect("a rule");
            self.k.set_role_permission(&self.admin, "Clerk", &def).await.expect("accepted");
        }
        self.k.invalidate_tenant_users(self.tenant);
        self.k.load_ctx(self.tenant, self.clerk, None, "it".into()).await.unwrap()
    }

    /// The titles `ctx` can reach, through the list and record by record: they must agree.
    async fn reach(&self, ctx: &Ctx, all: &[(Uuid, String)]) -> BTreeSet<String> {
        let listed: BTreeSet<String> = self
            .k
            .list(ctx, "Claim", &ListQuery { limit: Some(500), ..Default::default() })
            .await
            .unwrap()
            .rows
            .iter()
            .map(|r| r["title"].as_str().unwrap().to_string())
            .collect();
        let mut opened = BTreeSet::new();
        for (id, title) in all {
            match self.k.get(ctx, "Claim", *id, false).await {
                Ok(_) => {
                    opened.insert(title.clone());
                }
                Err(Error::NotFound(_) | Error::Forbidden(_)) => {}
                Err(e) => panic!("{title}: {e}"),
            }
        }
        assert_eq!(listed, opened, "the list and the record check disagree");
        listed
    }
}

fn set(titles: &[&str]) -> BTreeSet<String> {
    titles.iter().map(|t| t.to_string()).collect()
}

#[test]
fn rules_reach_the_same_records_however_they_are_asked() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["claims".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into role (tenant_id, name) values ($1, 'Clerk'), ($1, 'Viewer')").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &["Clerk".to_string()], false).await.unwrap();
    let world = World { k: k.clone(), tenant, admin: admin.clone(), clerk };

    let today = admin.today();
    let day = |n: i64| (today + chrono::Duration::days(n)).to_string();
    let rows = [
        json!({"title": "low", "amount": "99.99", "level": 1, "due": day(-1), "status": "Open", "filed_at": "2026-01-01T10:00:00Z"}),
        json!({"title": "edge", "amount": "100", "level": 2, "due": day(0), "status": "Closed", "filed_at": "2026-06-30T23:59:59Z"}),
        json!({"title": "over", "amount": "100.01", "level": 3, "due": day(7), "status": "Open", "filed_at": "2026-07-01T00:00:00Z"}),
        json!({"title": "far", "amount": "-5", "level": 0, "due": day(8), "status": "Open"}),
        json!({"title": "blank"}),
    ];
    let mut all = Vec::new();
    for row in rows {
        let doc = k.insert(&admin, "Claim", obj(row.clone())).await.expect("a claim");
        all.push((doc["id"].as_str().unwrap().parse::<Uuid>().unwrap(), row["title"].as_str().unwrap().to_string()));
    }

    // ───────────── ranges, both ends included, on both paths ─────────────
    let cases: &[(Value, &[&str])] = &[
        (json!({"where": {"amount": {"lte": 100}}}), &["low", "edge", "far"]),
        (json!({"where": {"amount": {"gte": "100"}}}), &["edge", "over"]),
        (json!({"where": {"amount": {"gte": 99.99, "lte": 100}}}), &["low", "edge"]),
        (json!({"where": {"amount": {"lte": -5}}}), &["far"]),
        (json!({"where": {"level": {"gte": 2}}}), &["edge", "over"]),
        (json!({"where": {"due": {"gte": day(0), "lte": day(7)}}}), &["edge", "over"]),
        (json!({"where": {"due": {"from_today": 0, "to_today": 7}}}), &["edge", "over"]),
        (json!({"where": {"due": {"to_today": -1}}}), &["low"]),
        (json!({"where": {"due": {"from_today": 8}}}), &["far"]),
        (json!({"where": {"filed_at": {"lte": "2026-06-30"}}}), &["low", "edge"]),
        (json!({"where": {"filed_at": {"gte": "2026-07-01"}}}), &["over"]),
        (json!({"where": {"status": "Open", "amount": {"lte": 100}}}), &["low", "far"]),
        (json!({"where": {"amount": {"gte": 1000}}}), &[]),
    ];
    for (rule, expected) in cases {
        let ctx = world.rules(std::slice::from_ref(rule)).await;
        assert_eq!(world.reach(&ctx, &all).await, set(expected), "{rule}");
    }

    // A record qualifies through any rule, and two rules differing only by range are two rules.
    let ctx = world.rules(&[json!({"where": {"amount": {"lte": -5}}}), json!({"where": {"amount": {"gte": 100.01}}})]).await;
    assert_eq!(world.reach(&ctx, &all).await, set(&["far", "over"]));

    // A range narrows someone's own records: both must hold.
    let owner = k.create_user(&admin, "owner@acme.test", "Owner", None, &["Clerk".to_string()], false).await.unwrap();
    world.rules(&[json!({"read": false, "create": true}), json!({"if_owner": true, "where": {"amount": {"lte": 100}}})]).await;
    let mine = k.load_ctx(tenant, owner, None, "it".into()).await.unwrap();
    let small = k.insert(&mine, "Claim", obj(json!({"title": "my small", "amount": "50"}))).await.expect("theirs");
    let large = k.insert(&mine, "Claim", obj(json!({"title": "my large", "amount": "500"}))).await.expect("theirs");
    all.push((small["id"].as_str().unwrap().parse().unwrap(), "my small".to_string()));
    all.push((large["id"].as_str().unwrap().parse().unwrap(), "my large".to_string()));
    assert_eq!(world.reach(&mine, &all).await, set(&["my small"]), "their own, and within the range");

    // Agreement across a spread of values and bounds, against arithmetic done here.
    let mut x = 20_260_101u64;
    let mut next = |n: u64| {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (x >> 33) % n
    };
    let mut spread = Vec::new();
    for i in 0..60 {
        let cents = next(40_000) as i64 - 20_000;
        let title = format!("spread-{i}");
        let amount = format!("{}.{:02}", cents / 100, (cents % 100).abs());
        let amount = if cents < 0 && cents / 100 == 0 { format!("-{amount}") } else { amount };
        let doc = k.insert(&admin, "Claim", obj(json!({"title": title, "amount": amount}))).await.unwrap();
        spread.push((cents, doc["id"].as_str().unwrap().parse::<Uuid>().unwrap(), title));
    }
    let every: Vec<(Uuid, String)> = all.iter().cloned().chain(spread.iter().map(|(_, id, t)| (*id, t.clone()))).collect();
    for _ in 0..12 {
        let (a, b) = (next(40_000) as i64 - 20_000, next(40_000) as i64 - 20_000);
        let (low, high) = (a.min(b), a.max(b));
        let bound = |c: i64| format!("{}{}.{:02}", if c < 0 { "-" } else { "" }, c.abs() / 100, c.abs() % 100);
        let ctx = world.rules(&[json!({"where": {"amount": {"gte": bound(low), "lte": bound(high)}}})]).await;
        let expected: BTreeSet<String> = spread.iter().filter(|(c, _, _)| *c >= low && *c <= high).map(|(_, _, t)| t.clone()).collect();
        let reached: BTreeSet<String> = world.reach(&ctx, &every).await.into_iter().filter(|t| t.starts_with("spread-")).collect();
        assert_eq!(reached, expected, "{low}..{high}");
    }

    // ───────────── rules that cannot mean anything are refused ─────────────
    let bad = [
        json!({"title": {"lte": "z"}}),
        json!({"amount": {"from_today": 1}}),
        json!({"due": {"gte": day(0), "from_today": 1}}),
        json!({"amount": {"lte": "many"}}),
        json!({"due": {"gte": "next week"}}),
        json!({"amount": {}}),
        json!({"amount": {"above": 1}}),
    ];
    for condition in bad {
        let def: PermissionDef = serde_json::from_value(json!({"entity": "Claim", "read": true, "where": condition})).unwrap();
        assert!(matches!(k.set_role_permission(&admin, "Clerk", &def).await, Err(Error::Validation(_))), "{condition}");
    }

    // ───────────── what a record shows of what it links to ─────────────
    let eu = k.insert(&admin, "Payee", obj(json!({"name": "Secret Payee EU", "region": "EU"}))).await.unwrap();
    let us = k.insert(&admin, "Payee", obj(json!({"name": "Secret Payee US", "region": "US"}))).await.unwrap();
    let agent = k.insert(&admin, "Agent", obj(json!({"name": "Open Agent"}))).await.unwrap();
    let linked = |payee: &Doc| json!({"title": "linked", "amount": "1", "payee": payee["id"], "agent": agent["id"]});
    let a = k.insert(&admin, "Claim", obj(linked(&eu))).await.unwrap();
    let b = k.insert(&admin, "Claim", obj(linked(&us))).await.unwrap();
    let (a, b): (Uuid, Uuid) = (a["id"].as_str().unwrap().parse().unwrap(), b["id"].as_str().unwrap().parse().unwrap());
    let titles = |d: &Doc, f: &str| d.get("_titles").and_then(|t| t.get(f)).and_then(Value::as_str).map(str::to_string);

    let ctx = world.rules(&[json!({})]).await;
    let seen = k.get(&ctx, "Claim", a, true).await.unwrap();
    assert_eq!(titles(&seen, "agent").as_deref(), Some("Open Agent"), "an open entity names itself");
    assert_eq!(titles(&seen, "payee"), None, "a restricted one, not to someone who cannot read it");
    assert!(seen.get("payee").is_some(), "the link itself is the record's own field");
    let listed = k.list(&ctx, "Claim", &ListQuery { with_titles: true, limit: Some(500), ..Default::default() }).await.unwrap();
    assert!(!serde_json::to_string(&listed.rows).unwrap().contains("Secret Payee"), "nor in a list");

    // Reading only some payees shows only those.
    let eu_only: PermissionDef = serde_json::from_value(json!({"entity": "Payee", "read": true, "where": {"region": "EU"}})).unwrap();
    k.set_role_permission(&admin, "Clerk", &eu_only).await.unwrap();
    k.invalidate_tenant_users(tenant);
    let ctx = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert_eq!(titles(&k.get(&ctx, "Claim", a, true).await.unwrap(), "payee").as_deref(), Some("Secret Payee EU"));
    assert_eq!(titles(&k.get(&ctx, "Claim", b, true).await.unwrap(), "payee"), None, "row by row, not entity by entity");
    assert_eq!(titles(&k.get(&admin, "Claim", b, true).await.unwrap(), "payee").as_deref(), Some("Secret Payee US"), "administrators see all");

    // A workspace can restrict an entity its packages left open.
    let restrict: OverlayOp = serde_json::from_value(json!({"op": "set_entity_property", "property": "reference", "value": "restricted"})).unwrap();
    k.add_customization(&admin, "Agent", restrict, None).await.expect("a workspace's choice");
    k.invalidate_tenant_users(tenant);
    let ctx = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert_eq!(titles(&k.get(&ctx, "Claim", a, true).await.unwrap(), "agent"), None);

    // ───────────── why someone may or may not ─────────────
    let ctx = world.rules(&[json!({"where": {"amount": {"lte": 100}}})]).await;
    let (low, over) = (all.iter().find(|r| r.1 == "low").unwrap().0, all.iter().find(|r| r.1 == "over").unwrap().0);
    let why = k.explain_access(&admin, clerk, "Claim", Action::Read, None, None).await.unwrap();
    assert_eq!(why["allowed"], json!(true));
    assert_eq!(why["rules"][0]["role"], json!("Clerk"));
    assert_eq!(why["rules"][0]["where"]["amount"]["lte"], json!(100));
    let yes = k.explain_access(&admin, clerk, "Claim", Action::Read, Some(low), None).await.unwrap();
    assert_eq!(yes["allowed"], json!(true), "{yes}");
    let no = k.explain_access(&admin, clerk, "Claim", Action::Read, Some(over), None).await.unwrap();
    assert_eq!(no["allowed"], json!(false), "{no}");
    assert_eq!(no["record"]["reached"], json!(false));
    assert!(k.get(&ctx, "Claim", over, false).await.is_err(), "the explanation says what enforcement does");
    let write = k.explain_access(&admin, clerk, "Claim", Action::Write, None, None).await.unwrap();
    assert_eq!(write["allowed"], json!(false), "{write}");
    assert!(write["reason"].as_str().unwrap().contains("none of their roles"));

    // What if: another role's rules, without changing anything.
    let view: PermissionDef = serde_json::from_value(json!({"entity": "Claim", "read": true})).unwrap();
    k.set_role_permission(&admin, "Viewer", &view).await.unwrap();
    let what_if = k.explain_access(&admin, clerk, "Claim", Action::Read, Some(over), Some(vec!["Viewer".to_string()])).await.unwrap();
    assert_eq!(what_if["allowed"], json!(true), "{what_if}");
    assert_eq!(what_if["simulated"], json!(true));
    assert!(k.get(&ctx, "Claim", over, false).await.is_err(), "and nothing changed");
    let none = k.explain_access(&admin, clerk, "Claim", Action::Read, None, Some(vec![])).await.unwrap();
    assert_eq!(none["allowed"], json!(false), "no roles, no access: {none}");

    let of_admin = k.explain_access(&admin, admin.user, "Claim", Action::Delete, None, None).await.unwrap();
    assert_eq!(of_admin["allowed"], json!(true));
    assert!(matches!(k.explain_access(&ctx, clerk, "Claim", Action::Read, None, None).await, Err(Error::Forbidden(_))), "administrators only");
    k.set_user_active(&admin, clerk, false).await.unwrap();
    let inactive = k.explain_access(&admin, clerk, "Claim", Action::Read, Some(low), None).await.unwrap();
    assert_eq!(inactive["allowed"], json!(false), "{inactive}");
    let stranger = k.explain_access(&admin, Uuid::now_v7(), "Claim", Action::Read, None, None).await.unwrap();
    assert_eq!(stranger["allowed"], json!(false));

    // ───────────── a role given until a moment ─────────────
    // Held until then and not after, though the context was cached a moment before; never ending
    // before it starts, nor set on a role not given; and removed by the upkeep once it has ended.
    let stand_in = k.create_user(&admin, "stand-in@acme.test", "Stand-in", None, &[], false).await.unwrap();
    let viewer = vec!["Viewer".to_string()];
    let soon = chrono::Utc::now() + chrono::Duration::milliseconds(1500);
    let ended = BTreeMap::from([("Viewer".to_string(), chrono::Utc::now() - chrono::Duration::minutes(1))]);
    assert!(matches!(k.set_user_roles_until(&admin, stand_in, &viewer, &ended).await, Err(Error::Validation(_))), "an end already past");
    let stray = BTreeMap::from([("Clerk".to_string(), soon)]);
    assert!(matches!(k.set_user_roles_until(&admin, stand_in, &viewer, &stray).await, Err(Error::Validation(_))), "an end on a role not given");
    assert!(matches!(k.set_user_roles_until(&ctx, stand_in, &viewer, &BTreeMap::new()).await, Err(Error::Forbidden(_))), "administrators only");
    k.set_user_roles_until(&admin, stand_in, &viewer, &BTreeMap::from([("Viewer".to_string(), soon)])).await.unwrap();
    let held = k.load_ctx(tenant, stand_in, None, "it".into()).await.unwrap();
    assert_eq!(held.roles, viewer);
    assert!(k.get(&held, "Claim", over, false).await.is_ok(), "held until its end");
    tokio::time::sleep((soon - chrono::Utc::now()).to_std().unwrap_or_default() + std::time::Duration::from_millis(200)).await;
    let after = k.load_ctx(tenant, stand_in, None, "it".into()).await.unwrap();
    assert!(after.roles.is_empty(), "the cached context lapsed at the end, well inside its {:?}", k.settings.ctx_ttl);
    assert!(k.get(&after, "Claim", over, false).await.is_err(), "and with it the access");
    let grants = || async {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar("select count(*) from user_role where tenant_id = $1 and user_id = $2")
            .bind(tenant)
            .bind(stand_in)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    };
    assert_eq!(grants().await, 1, "the ended grant waits for the upkeep");
    k.maintenance().await.unwrap();
    assert_eq!(grants().await, 0, "and the upkeep removes it");
    // A role for good beside one until a moment: only the second ends.
    let later = chrono::Utc::now() + chrono::Duration::days(30);
    let both = vec!["Clerk".to_string(), "Viewer".to_string()];
    k.set_user_roles_until(&admin, stand_in, &both, &BTreeMap::from([("Viewer".to_string(), later)])).await.unwrap();
    let ctx = k.load_ctx(tenant, stand_in, None, "it".into()).await.unwrap();
    assert_eq!(ctx.roles, both);
    k.maintenance().await.unwrap();
    assert_eq!(grants().await, 2, "nothing ended yet");

    // ───────────── a person limited to a branch ─────────────
    // A department or a branch is an entity like any other, and a scope on it limits someone to the
    // records that name theirs, or name none, on both paths.
    let north = k.insert(&admin, "Agent", obj(json!({"name": "North"}))).await.unwrap();
    let south = k.insert(&admin, "Agent", obj(json!({"name": "South"}))).await.unwrap();
    let north_claim = k.insert(&admin, "Claim", obj(json!({"title": "north's", "agent": north["id"]}))).await.unwrap();
    let south_claim = k.insert(&admin, "Claim", obj(json!({"title": "south's", "agent": south["id"]}))).await.unwrap();
    let id = |d: &Doc| d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    k.set_user_scopes(&admin, stand_in, "Agent", &[id(&north)], None).await.unwrap();
    let ctx = k.load_ctx(tenant, stand_in, None, "it".into()).await.unwrap();
    let seen: BTreeSet<String> = k
        .list(&ctx, "Claim", &ListQuery { limit: Some(500), ..Default::default() })
        .await
        .unwrap()
        .rows
        .iter()
        .map(|r| r["title"].as_str().unwrap().to_string())
        .collect();
    assert!(seen.contains("north's") && !seen.contains("south's"), "{seen:?}");
    assert!(seen.contains("blank"), "a claim that names no agent is not limited by one: {seen:?}");
    assert!(k.get(&ctx, "Claim", id(&north_claim), false).await.is_ok());
    assert!(matches!(k.get(&ctx, "Claim", id(&south_claim), false).await, Err(Error::NotFound(_) | Error::Forbidden(_))), "nor opened directly");

    // ───────────── roles one person may not hold together ─────────────
    // Declared by an administrator with a reason, refused from then on whichever way a grant comes,
    // saying why; whoever held both before is named beside the pair until one goes.
    let invalid = |r: ferrum_kernel::Result<()>| matches!(r, Err(Error::Validation(_)));
    assert!(invalid(k.set_role_conflict(&admin, "Clerk", "Clerk", "the same").await), "not with itself");
    assert!(invalid(k.set_role_conflict(&admin, "Clerk", "Nobody", "no such role").await), "both must exist");
    assert!(invalid(k.set_role_conflict(&admin, "Viewer", "Clerk", " ").await), "a reason");
    assert!(matches!(k.set_role_conflict(&ctx, "Viewer", "Clerk", "x").await, Err(Error::Forbidden(_))), "administrators only");
    k.set_role_conflict(&admin, "Viewer", "Clerk", "whoever files claims does not review them").await.unwrap();
    let pairs = k.role_conflicts(&admin).await.unwrap();
    assert_eq!(pairs.len(), 1, "{pairs:?}");
    assert_eq!((pairs[0]["role_a"].as_str(), pairs[0]["role_b"].as_str()), (Some("Clerk"), Some("Viewer")), "kept in order");
    assert_eq!(pairs[0]["holders"], json!([{"id": stand_in, "email": "stand-in@acme.test", "full_name": "Stand-in"}]), "held both before: named");
    let pack = k.evidence_pack(&admin).await.unwrap();
    assert_eq!(pack["access"]["role_conflicts"][0]["holders"][0]["email"], json!("stand-in@acme.test"), "and in the evidence for a review");
    let refused = k.set_user_roles(&admin, stand_in, &both).await;
    assert!(
        matches!(&refused, Err(Error::Validation(e)) if e[0].field == "roles" && e[0].message == "Clerk and Viewer are not held by one person: whoever files claims does not review them"),
        "{refused:?}"
    );
    k.invalidate_user(tenant, stand_in);
    assert_eq!(k.load_ctx(tenant, stand_in, None, "it".into()).await.unwrap().roles, both, "a refused change changes nothing");
    assert!(invalid(k.create_user(&admin, "both@acme.test", "Both", None, &both, false).await.map(|_| ())), "nor for someone new");
    // Any path a grant takes, an identity provider's groups and apps included, meets the same refusal.
    k.set_user_roles(&admin, stand_in, &viewer).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let direct = sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, 'Clerk')").bind(tenant).bind(stand_in).execute(&mut *tx).await;
    assert!(direct.is_err_and(|e| e.to_string().contains("are not held by one person")));
    drop(tx);
    assert_eq!(k.role_conflicts(&admin).await.unwrap()[0]["holders"], json!([]), "no one holds both now");
    k.delete_role_conflict(&admin, "Viewer", "Clerk").await.unwrap();
    assert!(matches!(k.delete_role_conflict(&admin, "Viewer", "Clerk").await, Err(Error::NotFound(_))));
    k.set_user_roles(&admin, stand_in, &both).await.expect("the pair is no longer declared");
}
