//! The app shell's kernel side against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): browser sessions per device, host
//! names, kinds of people and interface releases (docs/plan/apps-ui.md).

use ferrum_kernel::shell::HostRoute;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

async fn kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    // Private addresses allowed: the domain check below reaches a server on this machine.
    let settings = KernelSettings { webhooks_allow_private: true, ..Default::default() };
    Kernel::boot(db, catalog, HookRegistry::default(), settings, Some(Arc::new(Quiet))).await.expect("boot")
}

/// A mailer that sends nothing: mail stays in the outbox, where the test reads it.
struct Quiet;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Quiet {
    async fn send(&self, _: &str, _: &str, _: &str) -> ferrum_kernel::Result<()> {
        Ok(())
    }
}

/// The sign-in codes queued for `to`, newest first (from the mail's subject).
async fn codes_for(k: &Kernel, to: &str) -> Vec<String> {
    // Past row-level security, as the dispatcher reads it.
    let mut tx = k.db.system_tx().await.unwrap();
    let subjects: Vec<String> = sqlx::query_scalar(
        "select payload->>'subject' from outbox where topic = 'ferrum.mail' and payload->>'to' = $1 and payload->>'subject' like '% is your code for %' order by id desc",
    )
    .bind(to)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    subjects.iter().map(|s| s.split(' ').next().unwrap().to_string()).collect()
}

fn tenant(slug: &str) -> NewTenant {
    NewTenant {
        slug: slug.into(),
        name: format!("{slug} Co"),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["core".into()],
        settings: None,
    }
}

/// Changes the test makes behind the kernel's back, past row-level security.
async fn sql(k: &Kernel, q: &str) {
    let q = format!("begin; set local ferrum.system = 'on'; {q}; commit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(q.clone())).execute(&k.db.pool).await.expect(&q);
}

#[test]
fn sessions_hosts_kinds_and_releases() {
    // A thread with room for the whole scenario's future (a test thread's stack is not).
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = kernel().await;
    let acme = k.create_tenant(tenant("acme")).await.expect("tenant");
    let globex = k.create_tenant(tenant("globex")).await.expect("tenant");
    let s = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let ana_id = k.create_user(&admin, "ana@acme.test", "Ana", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let a = k.login("acme", "ana@acme.test", "Correct-Horse-9").await.unwrap();
    let ana = k.load_ctx(acme, ana_id, Some(a.token_version), "it".into()).await.unwrap();

    // ── Sessions: one per device, each signed out on its own. ──
    let laptop = k.open_session(&a, Duration::from_secs(3600), "Firefox\u{0}\nlaptop", "203.0.113.7", None).await.unwrap();
    let phone = k.open_session(&a, Duration::from_secs(3600), "Safari phone", "203.0.113.8", None).await.unwrap();
    let state = k.check_session(acme, ana_id, laptop).await.unwrap();
    assert_eq!(state.actor, None);
    assert!(state.stepped_up(), "signing in proves the person");
    assert!(matches!(k.check_session(acme, s.user, laptop).await, Err(Error::Unauthorized(_))), "another person's session");
    assert!(matches!(k.check_session(globex, ana_id, laptop).await, Err(Error::Unauthorized(_))), "another workspace");
    let devices = k.list_sessions(&ana, ana_id).await.unwrap();
    assert_eq!(devices.len(), 2);
    assert!(devices.iter().any(|d| d.user_agent == "Firefoxlaptop"), "control characters are dropped: {devices:?}");
    assert!(matches!(k.list_sessions(&ana, s.user).await, Err(Error::Forbidden(_))));
    assert_eq!(k.list_sessions(&admin, ana_id).await.unwrap().len(), 2, "administrators see everyone's devices");

    k.revoke_session(&ferrum_testkit::at_screen(&k, &ana).await, phone).await.unwrap();
    assert!(matches!(k.check_session(acme, ana_id, phone).await, Err(Error::Unauthorized(_))), "revoked: at once on this replica");
    assert!(k.check_session(acme, ana_id, laptop).await.is_ok(), "the other device stays signed in");
    assert!(matches!(k.revoke_session(&ferrum_testkit::at_screen(&k, &ana).await, phone).await, Err(Error::NotFound(_))), "already revoked");
    let theirs = k.open_session(&s, Duration::from_secs(3600), "Chrome", "198.51.100.1", None).await.unwrap();
    assert!(matches!(k.revoke_session(&ferrum_testkit::at_screen(&k, &ana).await, theirs).await, Err(Error::NotFound(_))), "not hers to revoke");
    k.revoke_session(&ferrum_testkit::at_screen(&k, &admin).await, laptop).await.unwrap();
    assert!(k.check_session(acme, ana_id, laptop).await.is_err(), "an administrator signs her device out");

    // Step-up ages; proving oneself again renews it.
    let tablet = k.open_session(&a, Duration::from_secs(3600), "tablet", "", Some(s.user)).await.unwrap();
    assert_eq!(k.check_session(acme, ana_id, tablet).await.unwrap().actor, Some(s.user), "who really acts");
    sql(&k, &format!("update user_session set step_up_at = now() - interval '11 minutes' where id = '{tablet}'")).await;
    k.step_up_session(acme, tablet).await.unwrap();
    assert!(k.check_session(acme, ana_id, tablet).await.unwrap().stepped_up());
    sql(&k, &format!("update user_session set step_up_at = now() - interval '11 minutes' where id = '{tablet}'")).await;
    k.end_session(acme, tablet).await.unwrap();
    assert!(k.check_session(acme, ana_id, tablet).await.is_err(), "signed out of this device");

    // Expired sessions fail, and maintenance forgets them a week on.
    let old = k.open_session(&a, Duration::from_secs(1), "old", "", None).await.unwrap();
    sql(&k, &format!("update user_session set expires_at = now() - interval '8 days' where id = '{old}'")).await;
    assert!(k.check_session(acme, ana_id, old).await.is_err());
    assert!(k.purge_sessions().await.unwrap() >= 1);

    // ── Hosts. ──
    k.add_subdomain(acme, "acme.ferrum.test").await.unwrap();
    k.add_subdomain(acme, "acme-old.ferrum.test").await.unwrap();
    assert_eq!(k.route_host("ACME.ferrum.test:443").await.unwrap(), HostRoute::Serve { tenant: acme, target: "shell".into() });
    assert_eq!(k.tenant_for_host("acme.ferrum.test").await.unwrap(), Some(acme));
    assert_eq!(k.route_host("nobody.ferrum.test").await.unwrap(), HostRoute::Unknown);
    assert_eq!(k.route_host("localhost").await.unwrap(), HostRoute::Unknown, "not a domain name");
    assert_eq!(k.primary_host(acme, "shell").await.unwrap().as_deref(), Some("acme.ferrum.test"), "the first is the primary");
    assert!(matches!(k.add_subdomain(globex, "acme.ferrum.test").await, Err(Error::Conflict(_))), "taken");

    let custom = k.add_host(&admin, "ERP.Acme.example", "shell").await.unwrap();
    assert_eq!((custom.host.as_str(), custom.kind.as_str(), custom.verified_at.is_none()), ("erp.acme.example", "custom", true));
    assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Unknown, "not before it is verified");
    assert_eq!(k.host_token("erp.acme.example").await.unwrap().as_deref(), Some(custom.verify_token.as_str()));
    assert!(k.host_may_have_certificate("erp.acme.example").await.unwrap(), "a pending domain is served so it can be checked");
    assert!(!k.host_may_have_certificate("evil.example").await.unwrap());
    assert!(matches!(k.add_host(&ana, "x.acme.example", "shell").await, Err(Error::Forbidden(_))));
    assert!(matches!(k.add_host(&admin, "x.acme.example", "site:NOPE").await, Err(Error::Validation(_))));
    assert!(k.hosts_to_check(10).await.unwrap().contains(&"erp.acme.example".to_string()));
    k.host_checked("erp.acme.example", true).await.unwrap();
    assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Serve { tenant: acme, target: "shell".into() });
    k.make_primary_host(&admin, "erp.acme.example").await.unwrap();
    assert_eq!(k.primary_host(acme, "shell").await.unwrap().as_deref(), Some("erp.acme.example"));
    let shop = k.add_host(&admin, "shop.acme.example", "site:commerce").await.unwrap();
    assert!(matches!(k.make_primary_host(&admin, &shop.host).await, Err(Error::Invalid(_))), "not verified yet");
    k.host_checked(&shop.host, true).await.unwrap();
    assert_eq!(k.route_host(&shop.host).await.unwrap(), HostRoute::Serve { tenant: acme, target: "site:commerce".into() });
    assert_eq!(k.tenant_for_host(&shop.host).await.unwrap(), None, "a site host serves no shell");

    // A domain is checked by fetching its token from it: it points at this cluster.
    let books = k.add_host(&admin, "books.acme.example", "shell").await.unwrap();
    let answering = answer_with(books.verify_token.clone()).await;
    assert!(k.host_answers("books.acme.example", &answering).await, "its own token");
    assert!(!k.host_answers("books.acme.example", &answer_with("another".into()).await).await, "another token");
    assert!(!k.host_answers("nobody.acme.example", &answering).await, "not a domain of any workspace");
    assert!(!k.host_answers("books.acme.example", "http://127.0.0.1:9/.well-known/ferrum-domain").await, "nothing listening");
    assert_eq!(k.remove_host(&admin, "books.acme.example").await.unwrap(), None, "never verified: dropped, with no leaving period");

    // Seven failed checks in a row unverify a domain.
    for _ in 0..6 {
        k.host_checked(&shop.host, false).await.unwrap();
    }
    assert!(matches!(k.route_host(&shop.host).await.unwrap(), HostRoute::Serve { .. }));
    k.host_checked(&shop.host, false).await.unwrap();
    assert_eq!(k.route_host(&shop.host).await.unwrap(), HostRoute::Unknown);

    // Removed after use: it is leaving for FERRUM_HOST_LEAVING_DAYS (those signed in there go on), then redirects, then
    // is quarantined; never claimable meanwhile. Being the main host, it hands that place on first (the oldest subdomain).
    // (Boxed, here and below: the test's future stays within a test thread's stack.)
    Box::pin(async {
        let until = k.remove_host(&admin, "erp.acme.example").await.unwrap().expect("a leaving period");
        let days = (until - chrono::Utc::now()).num_hours();
        assert!((13 * 24..=14 * 24).contains(&days), "the setting's 14 days: {days} hours");
        assert_eq!(k.primary_host(acme, "shell").await.unwrap().as_deref(), Some("acme.ferrum.test"), "the main host's place was handed on");
        assert_eq!(
            k.route_host("erp.acme.example").await.unwrap(),
            HostRoute::Leaving { tenant: acme, target: "shell".into(), to: Some("acme.ferrum.test".into()), until }
        );
        assert_eq!(k.host_zone("erp.acme.example").await.unwrap(), None, "its zone, as while it served");
        assert!(k.host_may_have_certificate("erp.acme.example").await.unwrap(), "still served");
        let listed = k.list_hosts(&admin).await.unwrap().into_iter().find(|h| h.host == "erp.acme.example").unwrap();
        assert_eq!((listed.released_at.is_some(), listed.leaving_until, listed.is_primary), (true, Some(until), false));
        assert!(matches!(k.remove_host(&admin, "erp.acme.example").await, Err(Error::NotFound(_))), "released already");
        sql(&k, "update tenant_host set leaving_until = now() - interval '1 second' where host = 'erp.acme.example'").await;
        k.release_hosts(globex).await.unwrap(); // clears the replica's cache
        assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Moved { tenant: acme, to: Some("acme.ferrum.test".into()) }, "then redirects");
        let audited: i64 = {
            let mut tx = k.db.system_tx().await.unwrap();
            let n = sqlx::query_scalar(
                "select count(*) from audit_event where entity = 'WorkspaceDomain' and action = 'remove' and diff->>'leaving_until' is not null",
            )
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
            n
        };
        assert_eq!(audited, 1, "the removal and its leaving period are in the audit trail");
    })
    .await;
    let g = k.login("globex", "admin@globex.test", "Correct-Horse-9").await.unwrap();
    let globex_admin = k.load_ctx(globex, g.user, Some(g.token_version), "it".into()).await.unwrap();
    assert!(matches!(k.add_host(&globex_admin, "erp.acme.example", "shell").await, Err(Error::Conflict(_))), "released recently");
    sql(&k, "update tenant_host set released_at = now() - interval '114 days', leaving_until = now() - interval '100 days' where host = 'erp.acme.example'")
        .await;
    k.release_hosts(globex).await.unwrap(); // clears the replica's cache
    assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Unknown, "past the redirect window");
    assert!(matches!(k.add_host(&globex_admin, "erp.acme.example", "shell").await, Err(Error::Conflict(_))), "still quarantined");
    sql(&k, "update tenant_host set released_at = now() - interval '360 days', leaving_until = now() - interval '346 days' where host = 'erp.acme.example'")
        .await;
    assert!(
        matches!(k.add_host(&globex_admin, "erp.acme.example", "shell").await, Err(Error::Conflict(_))),
        "a year counts from the end of its leaving period"
    );
    sql(&k, "update tenant_host set released_at = now() - interval '414 days', leaving_until = now() - interval '400 days' where host = 'erp.acme.example'")
        .await;
    k.add_host(&globex_admin, "erp.acme.example", "shell").await.expect("claimable a year on");
    // Never verified: removing it simply drops it.
    k.remove_host(&globex_admin, "erp.acme.example").await.unwrap();
    k.add_host(&admin, "erp.acme.example", "shell").await.expect("free again at once");

    Box::pin(async {
        // The operator's task releases any address, a subdomain too: with no leaving period it redirects at once.
        k.add_subdomain(acme, "acme-b.ferrum.test").await.unwrap();
        assert_eq!(k.release_host("ACME-B.ferrum.test", Some(0)).await.unwrap(), (acme, None));
        assert_eq!(k.route_host("acme-b.ferrum.test").await.unwrap(), HostRoute::Moved { tenant: acme, to: Some("acme.ferrum.test".into()) });
        assert!(matches!(k.release_host("acme-b.ferrum.test", None).await, Err(Error::NotFound(_))), "released already");
        assert!(matches!(k.release_host("acme.ferrum.test", Some(400)).await, Err(Error::Validation(_))));
        assert!(matches!(k.release_host("nobody.ferrum.test", None).await, Err(Error::NotFound(_))));
        // Two releases at once (two replicas): one waits for the other, so neither hands the main place to a host the
        // other is releasing, and nothing is left half done.
        k.add_subdomain(globex, "g1.ferrum.test").await.unwrap();
        k.add_subdomain(globex, "g2.ferrum.test").await.unwrap();
        k.add_subdomain(globex, "g3.ferrum.test").await.unwrap();
        let (r1, r2) = tokio::join!(k.release_host("g1.ferrum.test", None), k.release_host("g2.ferrum.test", None));
        assert!(r1.unwrap().1.is_some() && r2.unwrap().1.is_some());
        assert_eq!(k.primary_host(globex, "shell").await.unwrap().as_deref(), Some("g3.ferrum.test"), "the one left is the main host");
        for g in ["g1.ferrum.test", "g2.ferrum.test"] {
            assert!(matches!(k.route_host(g).await.unwrap(), HostRoute::Leaving { to: Some(ref to), .. } if to == "g3.ferrum.test"), "{g}");
        }
    })
    .await;

    // Hosts the operator vouches for (ferrum task add_host): the first of a target becomes its main host, so a
    // release hands that place on and the leaving notice points somewhere.
    Box::pin(async {
        let ops = Box::pin(k.create_tenant(tenant("ops"))).await.expect("tenant");
        k.add_operator_host("ops", "erp.ops.internal", "shell", None).await.unwrap();
        k.add_operator_host("ops", "erp2.ops.internal", "shell", None).await.unwrap();
        k.add_operator_host("ops", "portal.ops.internal", "portal", None).await.unwrap();
        assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp.ops.internal"), "the first is the main one");
        assert_eq!(k.primary_host(ops, "portal").await.unwrap().as_deref(), Some("portal.ops.internal"), "per target");
        assert_eq!(k.host_successor("erp.ops.internal").await.unwrap().as_deref(), Some("erp2.ops.internal"));
        assert!(matches!(k.host_successor("nobody.ops.internal").await, Err(Error::NotFound(_))));
        k.release_host("erp.ops.internal", None).await.unwrap();
        assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp2.ops.internal"), "handed on to an operator host");
        assert!(
            matches!(k.route_host("erp.ops.internal").await.unwrap(), HostRoute::Leaving { to: Some(ref to), .. } if to == "erp2.ops.internal"),
            "the notice points at the other"
        );
        // Chosen on purpose, by the operator: audited.
        k.add_operator_host("ops", "erp3.ops.internal", "shell", None).await.unwrap();
        assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp2.ops.internal"), "a later host is not the main one");
        k.set_primary_host("ERP3.ops.internal").await.unwrap();
        assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp3.ops.internal"));
        assert!(matches!(k.set_primary_host("erp.ops.internal").await, Err(Error::NotFound(_))), "a released host cannot be");
        assert!(matches!(k.set_primary_host("nobody.ops.internal").await, Err(Error::NotFound(_))));
        let audited: i64 = {
            let mut tx = k.db.system_tx().await.unwrap();
            let n = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and entity = 'WorkspaceDomain' and action in ('add', 'primary')")
                .bind(ops)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            n
        };
        assert_eq!(audited, 5, "four hosts added and one made the main one");
        // The last host of a target: nothing takes its place.
        assert_eq!(k.host_successor("portal.ops.internal").await.unwrap(), None);
        k.release_host("portal.ops.internal", None).await.unwrap();
        assert!(matches!(k.route_host("portal.ops.internal").await.unwrap(), HostRoute::Leaving { to: None, .. }));

        // An installation from before: hosts and no main one. The next start, or the next change of a host, picks one.
        let older = Box::pin(k.create_tenant(tenant("older"))).await.expect("tenant");
        k.add_operator_host("older", "a.older.internal", "shell", None).await.unwrap();
        k.add_operator_host("older", "b.older.internal", "shell", None).await.unwrap();
        sql(&k, &format!("update tenant_host set is_primary = false where tenant_id = '{older}'")).await;
        assert_eq!(k.primary_host(older, "shell").await.unwrap(), None);
        assert_eq!(k.host_successor("b.older.internal").await.unwrap().as_deref(), Some("a.older.internal"), "what a release would leave");
        assert!(k.ensure_primary_hosts().await.unwrap() >= 1);
        assert_eq!(k.primary_host(older, "shell").await.unwrap().as_deref(), Some("a.older.internal"), "the oldest");
        assert_eq!(k.ensure_primary_hosts().await.unwrap(), 0, "once");
        sql(&k, &format!("update tenant_host set is_primary = false where tenant_id = '{older}'")).await;
        k.release_host("a.older.internal", None).await.unwrap();
        assert!(
            matches!(k.route_host("a.older.internal").await.unwrap(), HostRoute::Leaving { to: Some(ref to), .. } if to == "b.older.internal"),
            "a release with no main host hands it to the one left"
        );
    })
    .await;

    // A deleted or renamed workspace releases every host.
    assert!(k.release_hosts(acme).await.unwrap() >= 3);
    assert!(matches!(k.route_host("acme.ferrum.test").await.unwrap(), HostRoute::Moved { to: None, .. }));

    // ── Kinds of people. ──
    assert_eq!(k.user_kind(acme, ana_id).await.unwrap(), "member");
    k.set_user_kind(&admin, ana_id, "portal").await.unwrap();
    assert_eq!(k.user_kind(acme, ana_id).await.unwrap(), "portal");
    assert!(k.load_ctx(acme, ana_id, Some(a.token_version), "it".into()).await.is_err(), "changing the kind signs her out everywhere");
    assert!(matches!(k.set_user_kind(&admin, ana_id, "robot").await, Err(Error::Validation(_))));
    assert!(matches!(k.set_user_kind(&admin, s.user, "guest").await, Err(Error::Invalid(_))), "not one's own");
    assert!(matches!(k.set_user_kind(&ana, s.user, "member").await, Err(Error::Forbidden(_))));

    // ── Portal people sign in with emailed codes, and hold no roles or rights. ──
    let pia_id = k.create_user(&admin, "pia@customer.test", "Pia", None, &[], true).await.unwrap();
    k.set_user_kind(&admin, pia_id, "portal").await.unwrap();
    k.send_sign_in_code("ACME", " Pia@Customer.test ").await.unwrap();
    k.send_sign_in_code("acme", "nobody@customer.test").await.expect("the same answer for nobody");
    k.send_sign_in_code("acme", "admin@acme.test").await.expect("and for members, who sign in with passwords");
    assert!(codes_for(&k, "nobody@customer.test").await.is_empty() && codes_for(&k, "admin@acme.test").await.is_empty());
    let first = codes_for(&k, "pia@customer.test").await;
    assert_eq!(first.len(), 1);
    assert!(first[0].len() == 6 && first[0].bytes().all(|b| b.is_ascii_digit()), "{first:?}");
    // A new code ends the one before.
    k.send_sign_in_code("acme", "pia@customer.test").await.unwrap();
    let code = codes_for(&k, "pia@customer.test").await[0].clone();
    if code != first[0] {
        assert!(matches!(k.sign_in_with_code("acme", "pia@customer.test", &first[0]).await, Err(Error::Unauthorized(_))), "the older code");
    }
    assert!(matches!(k.sign_in_with_code("globex", "pia@customer.test", &code).await, Err(Error::Unauthorized(_))), "another workspace");
    let wrong = if code == "000000" { "111111" } else { "000000" };
    assert!(matches!(k.sign_in_with_code("acme", "pia@customer.test", wrong).await, Err(Error::Unauthorized(_))));
    let signed = match k.sign_in_with_code("acme", "pia@customer.test", &format!(" {} {} ", &code[..3], &code[3..])).await.unwrap() {
        ferrum_kernel::auth::SignIn::Session(s) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!((signed.tenant, signed.user), (acme, pia_id));
    assert!(matches!(k.sign_in_with_code("acme", "pia@customer.test", &code).await, Err(Error::Unauthorized(_))), "a code works once");
    let pia = k.load_ctx(acme, pia_id, Some(signed.token_version), "it".into()).await.unwrap();
    assert!(!pia.is_admin && pia.roles.is_empty(), "a portal person holds no rights, whatever she was given");
    // The directory of people is the members': a portal person neither reads it nor is in it.
    assert!(matches!(k.user_directory(&pia).await, Err(Error::Forbidden(_))), "a portal person does not read the directory");
    let listed: Vec<String> = k.user_directory(&admin).await.unwrap().iter().map(|p| p["email"].as_str().unwrap_or_default().to_string()).collect();
    assert!(listed.iter().any(|e| e == "admin@acme.test"), "{listed:?}");
    assert!(!listed.iter().any(|e| e == "pia@customer.test" || e == "ana@acme.test"), "portal people are not listed: {listed:?}");
    // Five wrong tries end a code.
    k.send_sign_in_code("acme", "pia@customer.test").await.unwrap();
    let code = codes_for(&k, "pia@customer.test").await[0].clone();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        assert!(k.sign_in_with_code("acme", "pia@customer.test", wrong).await.is_err());
    }
    assert!(matches!(k.sign_in_with_code("acme", "pia@customer.test", &code).await, Err(Error::Unauthorized(_))), "spent by wrong tries");
    // At most five codes an hour; past that, silence.
    for _ in 0..4 {
        k.send_sign_in_code("acme", "pia@customer.test").await.unwrap();
    }
    assert_eq!(codes_for(&k, "pia@customer.test").await.len(), 5);
    // A disabled portal person gets none.
    sql(&k, &format!("update app_user set is_active = false where id = '{pia_id}'")).await;
    sql(&k, "delete from sign_in_code").await;
    k.send_sign_in_code("acme", "pia@customer.test").await.unwrap();
    assert_eq!(codes_for(&k, "pia@customer.test").await.len(), 5);

    // ── Interface releases: content-addressed, pinned per workspace, purged when unused. ──
    let (d1, d2, d3) = ("1".repeat(64), "2".repeat(64), "3".repeat(64));
    let files = json!({"index.html": {"size": 120, "type": "text/html"}});
    let entry = json!({"head": "<!doctype html><html><head>", "body": "</head><body>", "tail": "</body></html>"});
    k.register_ui_release("crm", &d1, "1.0.0", &files, &entry).await.unwrap();
    k.register_ui_release("crm", &d1, "1.0.0", &files, &entry).await.expect("idempotent");
    sql(&k, "update ui_release set created_at = now() - interval '2 days'; update ui_release_version set created_at = now() - interval '2 days'").await;
    k.register_ui_release("crm", &d2, "1.0.0", &files, &entry).await.unwrap();
    k.register_ui_release("crm", &d3, "1.1.0", &files, &entry).await.unwrap();
    assert_eq!(k.ui_release_for("crm", "1.0.0").await.unwrap().unwrap().digest, d2, "the newest build of a version");
    assert_eq!(k.ui_release("crm", &d1).await.unwrap().unwrap().entry, entry);
    assert_eq!(k.ui_releases("crm", 10).await.unwrap().len(), 3);
    assert!(k.ui_release_for("crm", "9.9.9").await.unwrap().is_none());
    // A version that did not change the interface runs the release of the one before.
    k.register_ui_release("crm", &d3, "1.1.1", &files, &entry).await.unwrap();
    assert_eq!(k.ui_release_for("crm", "1.1.1").await.unwrap().unwrap().digest, d3);
    let listed = k.ui_releases("crm", 10).await.unwrap();
    assert_eq!(listed.iter().find(|(r, _)| r.digest == d3).unwrap().1, vec!["1.1.0".to_string(), "1.1.1".to_string()]);

    assert_eq!(k.ui_pin(acme, "crm").await.unwrap(), None);
    k.pin_ui(&admin, "crm", Some(&d1)).await.unwrap();
    assert_eq!(k.ui_pin(acme, "crm").await.unwrap(), Some(d1.clone()));
    assert_eq!(k.ui_pin(globex, "crm").await.unwrap(), None, "per workspace");
    assert!(matches!(k.pin_ui(&admin, "crm", Some(&"9".repeat(64))).await, Err(Error::NotFound(_))));
    assert!(matches!(k.pin_ui(&ana, "crm", None).await, Err(Error::Forbidden(_))));

    sql(&k, "update ui_release set last_used_at = now() - interval '40 days'").await;
    assert!(k.purge_ui_releases().await.unwrap().is_empty(), "pinned, or the newest of its version");
    k.pin_ui(&admin, "crm", None).await.unwrap();
    k.touch_ui_releases(&[("crm".into(), d3.clone())]).await.unwrap();
    assert_eq!(k.purge_ui_releases().await.unwrap(), vec![("crm".to_string(), d1.clone())]);
    assert!(k.ui_release("crm", &d2).await.unwrap().is_some() && k.ui_release("crm", &d3).await.unwrap().is_some());

    // ───────────── a slug never names another workspace (apps' origins of their own) ─────────────
    let initech = k.create_tenant(tenant("initech")).await.expect("tenant");
    sql(&k, &format!("update tenant set slug = 'initech-labs' where id = '{initech}'")).await;
    let taken = k.create_tenant(tenant("initech")).await.unwrap_err();
    assert!(matches!(&taken, Error::Validation(f) if f[0].field == "slug"), "a renamed workspace's old slug stays retired: {taken}");
    // The workspace itself may take its old slug back.
    sql(&k, &format!("update tenant set slug = 'initech' where id = '{initech}'")).await;
    // A deleted workspace's slug is retired too (a bare row: nothing else refers to it).
    sql(&k, "insert into tenant (id, slug, name) values ('01a19000-0000-7000-8000-000000000001', 'hooli', 'Hooli')").await;
    sql(&k, "delete from tenant where slug = 'hooli'").await;
    assert!(k.create_tenant(tenant("hooli")).await.is_err(), "a deleted workspace's slug stays retired");
    // Unchanged slugs and other columns pass untouched.
    sql(&k, &format!("update tenant set name = 'Initech Inc' where id = '{initech}'")).await;
    assert!(k.create_tenant(tenant("umbrella")).await.is_ok());

    Box::pin(names_and_what_addresses_show(&k)).await;
    Box::pin(mail_domains(&k)).await;
}

/// A domain claimed for mail (docs/operations.md, "Custom domains", Domains for mail), on a cluster that proves addresses
/// by their pointing here alone: proven by its DNS record alone (asked of a DNS server this test runs), never an address,
/// one workspace's, and released by the rules every domain of a workspace's own follows.
async fn mail_domains(k: &Kernel) {
    use ferrum_kernel::shell::{DomainProof, HostCheck};
    let zone: Zone = Arc::default();
    k.set_live(KernelSettings { dns_resolvers: vec![dns(zone.clone()).await], ..k.settings.clone() });
    assert_eq!(k.settings.domain_proof, DomainProof::Points, "addresses are proven by pointing here");
    let record = |host: &str, token: &str| {
        zone.lock().unwrap().insert(format!("_ferrum.{host}."), format!("ferrum-domain={token}"));
    };
    let ctx = |slug: &'static str| async move {
        let t = Box::pin(k.create_tenant(tenant(slug))).await.expect("tenant");
        let s = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
        (t, k.load_ctx(t, s.user, Some(s.token_version), "it".into()).await.unwrap())
    };
    let ((post, admin), (rival, rival_admin)) = (ctx("post").await, ctx("rival").await);

    // Claimed for mail, it waits for its record alone: nothing of it can point here.
    let claim = k.add_host(&admin, "Post.example", "mail").await.expect("a workspace claims a domain for mail");
    assert_eq!((claim.host.as_str(), claim.kind.as_str(), claim.target.as_str()), ("post.example", "custom", "mail"));
    assert!(!k.domain_proven(post, "post.example").await.unwrap(), "not before its record");
    assert_eq!(k.check_host("post.example").await.unwrap(), Some(HostCheck { works: false, points: true, record: Some(false) }), "its record asked");
    record("post.example", "fedcba9876543210fedcba9876543210");
    assert_eq!(k.check_host("post.example").await.unwrap().map(|c| c.works), Some(false), "another token");
    assert!(!k.domain_proven(post, "post.example").await.unwrap());
    record("post.example", &claim.verify_token);
    assert_eq!(k.check_host("post.example").await.unwrap(), Some(HostCheck { works: true, points: true, record: Some(true) }));
    assert!(k.domain_proven(post, "POST.example.").await.unwrap(), "its record proved it");
    assert!(!k.domain_proven(rival, "post.example").await.unwrap(), "for its workspace only");
    zone.lock().unwrap().clear();
    assert_eq!(k.check_host("post.example").await.unwrap(), Some(HostCheck { works: true, points: true, record: None }), "proven once, not asked again");

    // Never an address: no route, no proof answered, no certificate, never a main host (the database refuses it one).
    assert_eq!(k.route_host("post.example").await.unwrap(), HostRoute::Unknown);
    assert_eq!(k.host_token("post.example").await.unwrap(), None);
    assert!(!k.host_may_have_certificate("post.example").await.unwrap());
    let row = k.list_hosts(&admin).await.unwrap().into_iter().find(|h| h.host == "post.example").unwrap();
    assert!(row.verified_at.is_some() && row.proved_at.is_some() && !row.is_primary, "{row:?}");
    assert!(k.make_primary_host(&admin, "post.example").await.is_err() && k.set_primary_host("post.example").await.is_err());
    let mut tx = k.db.system_tx().await.unwrap();
    assert!(sqlx::query("update tenant_host set is_primary = true where host = 'post.example'").execute(&mut *tx).await.is_err());
    tx.rollback().await.unwrap();
    assert!(k.add_name(&admin, "names.test", "post", "mail", &Default::default()).await.is_err(), "a name under the base is the cluster's");
    assert!(k.add_operator_host("post", "mail.post.internal", "mail", None).await.is_err(), "only its record proves one");

    // One workspace's, whatever it is used for; one claim per domain. An address proven by pointing alone proves nothing
    // about mail.
    for target in ["mail", "shell"] {
        assert!(matches!(k.add_host(&rival_admin, "post.example", target).await, Err(Error::Conflict(_))), "{target}");
    }
    assert!(matches!(k.add_host(&admin, "post.example", "shell").await, Err(Error::Conflict(_))), "claimed once");
    k.add_host(&admin, "www.post.example", "shell").await.unwrap();
    k.host_checked("www.post.example", true).await.unwrap();
    assert!(!k.domain_proven(post, "www.post.example").await.unwrap(), "pointing here proves nothing about mail");

    // Removed: no longer proven, with no leaving period nor redirect, and kept from every other workspace for the
    // quarantine; a year on, another may claim it, and the old record proves nothing for it.
    assert_eq!(k.remove_host(&admin, "post.example").await.unwrap(), None);
    assert!(!k.domain_proven(post, "post.example").await.unwrap());
    assert_eq!(k.route_host("post.example").await.unwrap(), HostRoute::Unknown, "it never redirects");
    assert!(!k.host_may_have_certificate("post.example").await.unwrap());
    assert!(matches!(k.add_host(&rival_admin, "post.example", "mail").await, Err(Error::Conflict(m)) if m.contains("recently")));
    sql(k, "update tenant_host set released_at = now() - interval '366 days' where host = 'post.example'").await;
    let theirs = k.add_host(&rival_admin, "post.example", "mail").await.expect("free a year on");
    record("post.example", &claim.verify_token);
    assert_eq!(k.check_host("post.example").await.unwrap().map(|c| c.works), Some(false), "the old owner's record");
    record("post.example", &theirs.verify_token);
    assert!(k.check_host("post.example").await.unwrap().is_some_and(|c| c.works) && k.domain_proven(rival, "post.example").await.unwrap());
    // One never proven is simply dropped: free at once.
    k.add_host(&admin, "late.example", "mail").await.unwrap();
    assert_eq!(k.remove_host(&admin, "late.example").await.unwrap(), None);
    k.add_host(&rival_admin, "late.example", "mail").await.expect("free at once");
}

/// What the test's DNS holds: a name (lower case, with its final dot) and its TXT record.
type Zone = Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>;

/// A DNS server for one test, as the kernel's own tests run one (crates/kernel/src/shell.rs): a question for a name `zone`
/// holds is answered with its TXT record; any other name does not exist.
async fn dns(zone: Zone) -> std::net::SocketAddr {
    use hickory_resolver::proto::op::{Message, OpCode, ResponseCode};
    use hickory_resolver::proto::rr::rdata::TXT;
    use hickory_resolver::proto::rr::{Name, RData, Record};
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = socket.recv_from(&mut buf).await {
            let Ok(asked) = Message::from_vec(&buf[..n]) else { continue };
            let Some(q) = asked.queries.first().cloned() else { continue };
            let name = q.name().to_ascii().to_ascii_lowercase();
            let mut answer = Message::response(asked.metadata.id, OpCode::Query);
            answer.metadata.recursion_desired = true;
            answer.metadata.recursion_available = true;
            answer.add_query(q);
            let found = zone.lock().unwrap().get(&name).cloned();
            match found {
                Some(value) => {
                    answer.add_answers(vec![Record::from_rdata(Name::from_ascii(&name).unwrap(), 60, RData::TXT(TXT::new(vec![value])))]);
                }
                None => answer.metadata.response_code = ResponseCode::NXDomain,
            }
            let _ = socket.send_to(&answer.to_vec().unwrap(), from).await;
        }
    });
    addr
}

/// The addresses a workspace gives itself (docs/operations.md, "Addresses of a workspace"): names under the base domain,
/// what each `shell` address shows, the limit, short paths, a renamed and a closed workspace.
async fn names_and_what_addresses_show(k: &Kernel) {
    use ferrum_kernel::shell::{HostPatch, HostShape, HostShown, ShortPath};
    const BASE: &str = "names.test";
    let nova = Box::pin(k.create_tenant(tenant("nova"))).await.expect("tenant");
    let other = Box::pin(k.create_tenant(tenant("other"))).await.expect("tenant");
    let ctx = |t, slug: &'static str| async move {
        let s = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
        k.load_ctx(t, s.user, Some(s.token_version), "it".into()).await.unwrap()
    };
    let (admin, other_admin) = (ctx(nova, "nova").await, ctx(other, "other").await);
    k.add_subdomain(nova, "nova.names.test").await.unwrap();
    let shape = |home: Option<&str>, apps: Option<&[&str]>, label: Option<&str>| HostShape {
        home: home.map(str::to_string),
        apps: apps.map(|a| a.iter().map(|s| s.to_string()).collect()),
        label: label.map(str::to_string),
        forward: false,
    };

    // A name under the base: its own form, live at once, a certificate on demand, what it shows kept with it.
    let sales = k.add_name(&admin, BASE, "sales", "shell", &shape(Some("crm"), Some(&["crm"]), Some("Sales"))).await.unwrap();
    assert_eq!((sales.host.as_str(), sales.kind.as_str(), sales.base_name.as_deref()), ("sales--nova.names.test", "subdomain", Some("sales")));
    assert!(sales.verified_at.is_some() && !sales.is_primary, "live at once; the subdomain stays the main address");
    assert_eq!(k.route_host("sales--nova.names.test").await.unwrap(), HostRoute::Serve { tenant: nova, target: "shell".into() });
    assert!(k.host_may_have_certificate("sales--nova.names.test").await.unwrap());
    assert_eq!(
        k.host_shown("SALES--nova.names.test:443").await.unwrap(),
        Some(HostShown { shape: shape(Some("crm"), Some(&["crm"]), Some("Sales")), primary: false, brand: None })
    );
    assert_eq!(k.host_shown("nova.names.test").await.unwrap().map(|s| s.primary), Some(true));
    assert!(matches!(k.add_name(&admin, BASE, "sales", "shell", &HostShape::default()).await, Err(Error::Conflict(_))), "once");
    for (name, why) in [("accounts", "kept"), ("sa--les", "two hyphens"), ("Sales!", "not a name")] {
        assert!(matches!(k.add_name(&admin, BASE, name, "shell", &HostShape::default()).await, Err(Error::Validation(_))), "{why}");
    }
    assert!(matches!(k.add_name(&admin, BASE, "help", "portal", &shape(Some("crm"), None, None)).await, Err(Error::Validation(_))), "a portal shows no apps");
    let portal = k.add_name(&admin, BASE, "help", "portal", &HostShape::default()).await.unwrap();
    assert!(portal.is_primary, "the first address of a target is its main one");
    // Another workspace's names end in its own slug; it cannot take this one by any way in.
    assert_eq!(k.add_name(&other_admin, BASE, "sales", "shell", &HostShape::default()).await.unwrap().host, "sales--other.names.test");
    assert!(matches!(k.add_host(&other_admin, "sales--nova.names.test", "shell").await, Err(Error::Conflict(_))));
    let taken = k.create_tenant(tenant("sales--nova")).await.unwrap_err();
    assert!(matches!(&taken, Error::Validation(f) if f[0].field == "slug"), "no new slug has two hyphens: {taken}");
    let ana = k.create_user(&admin, "ana@nova.test", "Ana", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let a = k.login("nova", "ana@nova.test", "Correct-Horse-9").await.unwrap();
    let ana = k.load_ctx(nova, ana, Some(a.token_version), "it".into()).await.unwrap();
    assert!(matches!(k.add_name(&ana, BASE, "mine", "shell", &HostShape::default()).await, Err(Error::Forbidden(_))), "administrators only");

    // What an address shows changes part by part, never on another target, never on an operator's address. Two changes
    // at once each build on the other (the row is locked while it changes).
    let label = HostPatch { label: Some(Some("Sales team".into())), ..Default::default() };
    let home = HostPatch { home: Some(None), ..Default::default() };
    let (a, b) = tokio::join!(k.shape_host(&admin, "sales--nova.names.test", &label), k.shape_host(&admin, "sales--nova.names.test", &home));
    a.unwrap();
    b.unwrap();
    let row = k.list_hosts(&admin).await.unwrap().into_iter().find(|h| h.host == "sales--nova.names.test").unwrap();
    assert_eq!((row.home, row.apps, row.label), (None, Some(vec!["crm".to_string()]), Some("Sales team".to_string())), "neither change lost");
    assert!(matches!(k.shape_host(&admin, "help--nova.names.test", &label).await, Err(Error::Validation(_))), "a portal shows no apps");
    k.add_operator_host("nova", "erp.nova.internal", "shell", Some("office")).await.unwrap();
    assert!(matches!(k.shape_host(&admin, "erp.nova.internal", &label).await, Err(Error::Forbidden(m)) if m.contains("operator")));
    assert!(matches!(k.remove_host(&admin, "erp.nova.internal").await, Err(Error::Forbidden(m)) if m.contains("operator")));
    assert!(matches!(k.remove_host(&admin, "nova.names.test").await, Err(Error::NotFound(_))), "the workspace's own subdomain stays");
    let listed = k.list_hosts(&admin).await.unwrap();
    assert_eq!(
        listed.iter().find(|h| h.host == "erp.nova.internal").and_then(|h| h.zone.clone()).as_deref(),
        Some("office"),
        "its zone, as the operator set it"
    );

    // Removing a name: its leaving period, as any address; it comes back to the same workspace at once.
    let until = k.remove_host(&admin, "sales--nova.names.test").await.unwrap();
    assert!(until.is_some());
    assert!(matches!(k.route_host("sales--nova.names.test").await.unwrap(), HostRoute::Leaving { .. }));
    k.add_name(&admin, BASE, "sales", "shell", &HostShape::default()).await.expect("its own name, again");
    assert_eq!(k.route_host("sales--nova.names.test").await.unwrap(), HostRoute::Serve { tenant: nova, target: "shell".into() });

    // The limit: the plan's `addresses` quota (20 without one), held when two adds race for the last place.
    let (limit, used) = k.address_room(nova).await.unwrap();
    assert_eq!((limit, used), (20, 4), "the subdomain, two names (one the portal's), the operator's");
    sql(k, &format!("update tenant set quotas = '{{\"addresses\": 5}}' where id = '{nova}'")).await;
    let plain = HostShape::default();
    let (x, y) = tokio::join!(k.add_name(&admin, BASE, "x1", "shell", &plain), k.add_name(&admin, BASE, "x2", "shell", &plain));
    assert_eq!([x.is_ok(), y.is_ok()].iter().filter(|ok| **ok).count(), 1, "one place, one name");
    let refused = k.add_host(&admin, "erp.nova.example", "shell").await.unwrap_err();
    assert!(refused.to_string().contains("at most 5 addresses"), "{refused}");

    // Short paths, kept in the workspace's settings, checked together.
    let p = |alias: &str, target: &str| ShortPath { alias: alias.into(), target: target.into() };
    k.set_short_paths(&admin, &[p("deals", "/crm/"), p("leads", "/crm/e/Lead")]).await.unwrap();
    assert_eq!(k.short_paths(nova).await.unwrap(), vec![p("deals", "/crm/"), p("leads", "/crm/e/Lead")]);
    for (bad, says) in [
        (vec![p("deals", "/crm/"), p("deals", "/x/")], "/deals is given twice"),
        (vec![p("ok", "/crm/"), p("x", "/crm/")], "/x:"),
        (vec![p("away", "//evil.example/")], "/away must lead"),
        (vec![p("away", "https://evil.example/")], "/away must lead"),
    ] {
        let e = k.set_short_paths(&admin, &bad).await.unwrap_err();
        assert!(e.to_string().contains(says), "{e}");
    }
    let many: Vec<ShortPath> = (0..51).map(|i| p(&format!("p{i}"), "/crm/")).collect();
    assert!(k.set_short_paths(&admin, &many).await.unwrap_err().to_string().contains("at most 50"));
    assert!(matches!(k.set_short_paths(&ana, &[]).await, Err(Error::Forbidden(_))));
    assert_eq!(k.short_paths(nova).await.unwrap().len(), 2, "a refused list changes nothing");

    // Every change is the administrator's, on the audit trail.
    let audited: Vec<(String, String, Option<uuid::Uuid>)> = {
        let mut tx = k.db.system_tx().await.unwrap();
        let rows = sqlx::query_as(
            "select entity, action, actor from audit_event where tenant_id = $1 and entity in ('WorkspaceDomain', 'WorkspacePaths') order by id",
        )
        .bind(nova)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        rows
    };
    for action in ["add", "shape", "remove", "set"] {
        assert!(audited.iter().any(|(_, a, by)| a == action && *by == Some(admin.user)), "{action}: {audited:?}");
    }

    // A renamed workspace (its slug changed by hand: there is no task for it): its names keep leading to it, and no other
    // workspace can ever form them, its old slug being retired; new names take the new slug.
    sql(k, &format!("update tenant set slug = 'nova-co', quotas = '{{}}' where id = '{nova}'")).await;
    k.release_hosts(uuid::Uuid::nil()).await.unwrap(); // clears this replica's host cache
    assert_eq!(k.route_host("sales--nova.names.test").await.unwrap(), HostRoute::Serve { tenant: nova, target: "shell".into() });
    let admin = k.load_ctx(nova, admin.user, None, "it".into()).await.unwrap();
    assert_eq!(k.add_name(&admin, BASE, "eu", "shell", &HostShape::default()).await.unwrap_or_else(|e| panic!("{e}")).host, "eu--nova-co.names.test");
    assert!(k.create_tenant(tenant("nova")).await.is_err(), "its old slug, and so its names, are never another's");

    // A closed workspace is read-only: its addresses keep answering, and stay as they are.
    k.close_account(nova, chrono::Utc::now().date_naive() + chrono::Days::new(30), "the contract ended").await.unwrap();
    let closed = k.load_ctx(nova, admin.user, None, "it".into()).await.unwrap();
    for refused in [
        k.add_name(&closed, BASE, "late", "shell", &HostShape::default()).await.map(|_| ()),
        k.shape_host(&closed, "sales--nova.names.test", &label).await.map(|_| ()),
        k.remove_host(&closed, "sales--nova.names.test").await.map(|_| ()),
        k.set_short_paths(&closed, &[]).await.map(|_| ()),
    ] {
        assert!(matches!(refused, Err(Error::Forbidden(ref m)) if m.contains("read-only")), "{refused:?}");
    }
    assert_eq!(k.route_host("sales--nova.names.test").await.unwrap(), HostRoute::Serve { tenant: nova, target: "shell".into() });
}

/// A server on this machine answering every request with `body`; its proof address.
async fn answer_with(body: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
        }
    });
    format!("http://{addr}/.well-known/ferrum-domain")
}
