//! The app shell's kernel side against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): browser sessions per device, host
//! names, kinds of people and interface releases (docs/plan/apps-ui.md).

use ferrum_kernel::secret::SecretKey;
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
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    // Private addresses allowed: the domain check below reaches a server on this machine.
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), webhooks_allow_private: true, ..Default::default() };
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

#[tokio::test]
async fn sessions_hosts_kinds_and_releases() {
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

    k.revoke_session(&ana, phone).await.unwrap();
    assert!(matches!(k.check_session(acme, ana_id, phone).await, Err(Error::Unauthorized(_))), "revoked: at once on this replica");
    assert!(k.check_session(acme, ana_id, laptop).await.is_ok(), "the other device stays signed in");
    assert!(matches!(k.revoke_session(&ana, phone).await, Err(Error::NotFound(_))), "already revoked");
    let theirs = k.open_session(&s, Duration::from_secs(3600), "Chrome", "198.51.100.1", None).await.unwrap();
    assert!(matches!(k.revoke_session(&ana, theirs).await, Err(Error::NotFound(_))), "not hers to revoke");
    k.revoke_session(&admin, laptop).await.unwrap();
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
    k.remove_host(&admin, "books.acme.example").await.unwrap();

    // Seven failed checks in a row unverify a domain.
    for _ in 0..6 {
        k.host_checked(&shop.host, false).await.unwrap();
    }
    assert!(matches!(k.route_host(&shop.host).await.unwrap(), HostRoute::Serve { .. }));
    k.host_checked(&shop.host, false).await.unwrap();
    assert_eq!(k.route_host(&shop.host).await.unwrap(), HostRoute::Unknown);

    // Removed after use: it redirects, then is quarantined; never claimable meanwhile.
    k.remove_host(&admin, "erp.acme.example").await.unwrap();
    assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Moved { tenant: acme, to: None }, "no primary left for shell");
    k.make_primary_host(&admin, "acme.ferrum.test").await.unwrap();
    assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Moved { tenant: acme, to: Some("acme.ferrum.test".into()) });
    let g = k.login("globex", "admin@globex.test", "Correct-Horse-9").await.unwrap();
    let globex_admin = k.load_ctx(globex, g.user, Some(g.token_version), "it".into()).await.unwrap();
    assert!(matches!(k.add_host(&globex_admin, "erp.acme.example", "shell").await, Err(Error::Conflict(_))), "released recently");
    sql(&k, "update tenant_host set released_at = now() - interval '100 days' where host = 'erp.acme.example'").await;
    k.release_hosts(globex).await.unwrap(); // clears the replica's cache
    assert_eq!(k.route_host("erp.acme.example").await.unwrap(), HostRoute::Unknown, "past the redirect window");
    assert!(matches!(k.add_host(&globex_admin, "erp.acme.example", "shell").await, Err(Error::Conflict(_))), "still quarantined");
    sql(&k, "update tenant_host set released_at = now() - interval '400 days' where host = 'erp.acme.example'").await;
    k.add_host(&globex_admin, "erp.acme.example", "shell").await.expect("claimable a year on");
    // Never verified: removing it simply drops it.
    k.remove_host(&globex_admin, "erp.acme.example").await.unwrap();
    k.add_host(&admin, "erp.acme.example", "shell").await.expect("free again at once");

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
