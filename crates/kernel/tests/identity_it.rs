//! One person, many workspaces (docs/plan/apps-ui.md §9.3, U6) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): the
//! first sign-in at accounts makes an identity from a workspace's password and the address's code;
//! other workspaces join it after a proof each; codes carry a sign-in to a workspace's host once;
//! workspace rules still apply; a code by email sets a password; signing out everywhere ends every
//! session; and a server without mail makes identities whose address proves nothing.

use ferrum_kernel::auth::Session;
use ferrum_kernel::identity::{AccountSignIn, Entry, Proof};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::twofactor::code;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

async fn boot(db: Db, mail: bool) -> Arc<Kernel> {
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let mailer: Option<Arc<dyn ferrum_kernel::Mailer>> = if mail { Some(Arc::new(Quiet)) } else { None };
    Kernel::boot(db, catalog, HookRegistry::default(), settings, mailer).await.expect("boot")
}

/// A mailer that sends nothing: mail stays in the outbox, where the test reads it.
struct Quiet;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Quiet {
    async fn send(&self, _: &str, _: &str, _: &str) -> ferrum_kernel::Result<()> {
        Ok(())
    }
}

/// The codes mailed to `to`, newest first.
async fn codes_for(k: &Kernel, to: &str) -> Vec<String> {
    let mut tx = k.db.system_tx().await.unwrap();
    let subjects: Vec<String> = sqlx::query_scalar(
        "select payload->>'subject' from outbox where topic = 'ferrum.mail' and payload->>'to' = $1 and payload->>'subject' like '% is your Ferrum code' order by id desc",
    )
    .bind(to)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    subjects.iter().map(|s| s.split(' ').next().unwrap().to_string()).collect()
}

async fn newest_code(k: &Kernel, to: &str) -> String {
    codes_for(k, to).await.into_iter().next().expect("a code was mailed")
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

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn admin(k: &Kernel, slug: &str) -> Ctx {
    let s = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await.unwrap()
}

async fn ctx(k: &Kernel, s: &Session) -> Ctx {
    k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await.unwrap()
}

/// Turns on an authenticator app for a person: its secret and recovery codes.
async fn two_factor(k: &Kernel, person: &Ctx) -> (String, Vec<String>) {
    let secret = k.begin_two_factor(person).await.unwrap()["secret"].as_str().unwrap().to_string();
    let recovery = k.enable_two_factor(person, &code(&secret, now()).unwrap()).await.unwrap();
    (secret, recovery)
}

/// A workspace's sign-in rules (`require_two_factor`, `require_sso`), set as administration does.
async fn set_policy(k: &Kernel, tenant: uuid::Uuid, policy: serde_json::Value) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set settings = (settings - 'require_two_factor' - 'require_sso') || $2 where id = $1")
        .bind(tenant)
        .bind(policy)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Lets a minute pass for every code carrying a sign-in.
async fn expire_host_codes(k: &Kernel) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update host_code set expires_at = now() - interval '1 second' where used_at is null").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

fn signed(r: AccountSignIn) -> Session {
    match r {
        AccountSignIn::Signed(s) => s,
        other => panic!("expected to be signed in, got {other:?}"),
    }
}

#[tokio::test]
async fn one_person_many_workspaces() {
    let db = Db::connect(DbConfig { url: url(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = boot(db.clone(), true).await;
    let acme = k.create_tenant(tenant("acme")).await.unwrap();
    let globex = k.create_tenant(tenant("globex")).await.unwrap();
    let initech = k.create_tenant(tenant("initech")).await.unwrap();
    assert!(matches!(k.create_tenant(tenant("accounts")).await, Err(Error::Validation(_))), "the accounts host's name is kept");
    assert!(k.directory().await.unwrap().is_none(), "no directory until the cluster signs in through accounts");
    let dir = k.ensure_directory().await.unwrap();
    assert_eq!(k.ensure_directory().await.unwrap(), dir);
    let (a_admin, g_admin, i_admin) = (admin(&k, "acme").await, admin(&k, "globex").await, admin(&k, "initech").await);

    // Ana: a password of her own in Acme and another in Globex (with an authenticator app), and
    // none in Initech, whose administrator added her without one.
    let ana_acme = k.create_user(&a_admin, "ana@x.test", "Ana Rao", Some("Acme-Pass-1"), &[], false).await.unwrap();
    let ana_globex = k.create_user(&g_admin, "ana@x.test", "Ana Rao", Some("Globex-Pass-1"), &[], false).await.unwrap();
    let ana_initech = k.create_user(&i_admin, "ana@x.test", "Ana Rao", None, &[], false).await.unwrap();
    let (globex_secret, _) = two_factor(&k, &k.load_ctx(globex, ana_globex, None, "it".into()).await.unwrap()).await;

    // ── The first sign-in at accounts: Acme's password, then the address's code ──
    assert!(matches!(k.account_sign_in("ana@x.test", "wrong-one-1").await, Err(Error::Unauthorized(_))));
    let AccountSignIn::Verify(m) = k.account_sign_in(" Ana@X.test ", "Acme-Pass-1").await.unwrap() else { panic!("a code to the address comes next") };
    assert_eq!((m.tenant, m.user), (acme, ana_acme));
    assert!(k.account_verify(&m, "000000").await.is_err());
    let ana = k.account_verify(&m, &newest_code(&k, "ana@x.test").await).await.unwrap();
    assert_eq!(ana.tenant, dir);
    let me = ctx(&k, &ana).await;
    let id = k.identity(&me).await.unwrap();
    assert!(id.verified && !id.two_factor);
    assert_eq!((id.email.as_str(), id.name.as_str()), ("ana@x.test", "Ana Rao"));
    assert!(k.account_verify(&m, &newest_code(&k, "ana@x.test").await).await.is_err(), "a code works once");
    assert_eq!(k.identity_of(acme, ana_acme).await.unwrap(), Some((id.id, "ana@x.test".to_string())));
    // The identity has Acme's password now; Globex's is not it.
    assert_eq!(signed(k.account_sign_in("ana@x.test", "Acme-Pass-1").await.unwrap()).user, ana.user);
    assert!(k.account_sign_in("ana@x.test", "Globex-Pass-1").await.is_err());
    let list = k.identity_workspaces(&me).await.unwrap();
    let names: Vec<(String, bool)> = list.iter().map(|w| (w["slug"].as_str().unwrap().to_string(), w["joined"].as_bool().unwrap())).collect();
    assert_eq!(names, vec![("acme".into(), true), ("globex".into(), false), ("initech".into(), false)]);

    // ── Entering workspaces, and joining them ──
    assert_eq!(k.enter(&me, acme).await.unwrap(), Entry::Ready(Session { tenant: acme, user: ana_acme, token_version: m.token_version }));
    assert_eq!(k.enter(&me, globex).await.unwrap(), Entry::Join { click: false, password: true, two_factor: true });
    assert_eq!(k.enter(&me, initech).await.unwrap(), Entry::Join { click: true, password: false, two_factor: false });
    assert_eq!(k.enter(&me, dir).await.unwrap(), Entry::NoAccess, "the directory is no workspace");
    // Initech: her address is proven and the membership has no app of its own, so confirming joins it.
    k.join(&me, initech, Proof::default()).await.unwrap();
    assert!(matches!(k.enter(&me, initech).await.unwrap(), Entry::Ready(s) if s.user == ana_initech));
    // Globex: its password and its app's code.
    assert!(k.join(&me, globex, Proof::default()).await.is_err(), "confirming is not enough with an app of its own");
    assert!(matches!(k.join(&me, globex, Proof { password: Some("Globex-Pass-1"), ..Default::default() }).await, Err(Error::Validation(_))));
    assert!(k.join(&me, globex, Proof { password: Some("wrong-one-1"), totp: Some("123456"), ..Default::default() }).await.is_err());
    let totp = code(&globex_secret, now() + 30).unwrap();
    k.join(&me, globex, Proof { password: Some("Globex-Pass-1"), totp: Some(&totp), ..Default::default() }).await.unwrap();
    assert!(matches!(k.enter(&me, globex).await.unwrap(), Entry::Ready(s) if s.user == ana_globex));
    assert!(k.identity_workspaces(&me).await.unwrap().iter().all(|w| w["joined"] == json!(true)));

    // ── Carrying a sign-in to a workspace's host ──
    let Entry::Ready(into_acme) = k.enter(&me, acme).await.unwrap() else { panic!() };
    let c = k.mint_host_code(&into_acme, Some(id.id), "acme.ferrum.test").await.unwrap();
    assert!(k.redeem_host_code(&c, "globex.ferrum.test").await.is_err(), "another host");
    assert!(k.redeem_host_code(&c, "acme.ferrum.test").await.is_err(), "spent by the wrong host's try");
    let c = k.mint_host_code(&into_acme, Some(id.id), "acme.ferrum.test").await.unwrap();
    assert_eq!(k.redeem_host_code(&c, "ACME.ferrum.test:443").await.unwrap(), into_acme);
    assert!(k.redeem_host_code(&c, "acme.ferrum.test").await.is_err(), "once");
    let c = k.mint_host_code(&into_acme, Some(id.id), "acme.ferrum.test").await.unwrap();
    expire_host_codes(&k).await;
    assert!(k.redeem_host_code(&c, "acme.ferrum.test").await.is_err(), "a minute at most");
    // Someone turned off meanwhile does not get in.
    let c = k.mint_host_code(&into_acme, Some(id.id), "acme.ferrum.test").await.unwrap();
    k.set_user_active(&a_admin, ana_acme, false).await.unwrap();
    assert!(k.redeem_host_code(&c, "acme.ferrum.test").await.is_err());
    assert_eq!(k.enter(&me, acme).await.unwrap(), Entry::NoAccess);
    k.set_user_active(&a_admin, ana_acme, true).await.unwrap();

    // ── Each workspace's rules still apply ──
    set_policy(&k, acme, json!({"require_two_factor": true})).await;
    assert_eq!(k.enter(&me, acme).await.unwrap(), Entry::TwoFactor, "her identity has no app yet");
    set_policy(&k, acme, json!({"require_sso": true})).await;
    assert_eq!(k.enter(&me, acme).await.unwrap(), Entry::Sso);
    set_policy(&k, acme, json!({})).await;
    assert!(matches!(k.enter(&me, acme).await.unwrap(), Entry::Ready(_)));

    // ── A password by a code to the address ──
    // Bob was added to Initech without a password and has none anywhere.
    k.create_user(&i_admin, "bob@x.test", "Bob Ito", None, &[], false).await.unwrap();
    assert!(k.account_sign_in("bob@x.test", "Anything-1").await.is_err());
    k.send_email_code("nobody@x.test", "set your password").await.unwrap();
    assert!(codes_for(&k, "nobody@x.test").await.is_empty(), "nobody of this cluster: no mail, and the same answer");
    k.send_email_code("bob@x.test", "set your password").await.unwrap();
    let bob = signed(k.account_password("bob@x.test", &newest_code(&k, "bob@x.test").await, "Bob-Pass-12").await.unwrap());
    let bob_ctx = ctx(&k, &bob).await;
    let bob_id = k.identity(&bob_ctx).await.unwrap();
    assert!(bob_id.verified);
    assert_eq!(bob_id.name, "Bob Ito", "the name his workspace knows");
    assert_eq!(k.enter(&bob_ctx, initech).await.unwrap(), Entry::Join { click: true, password: false, two_factor: false });
    assert_eq!(k.enter(&bob_ctx, acme).await.unwrap(), Entry::NoAccess, "a proven address may hear it has no account there");
    // Ana forgot hers: a new one ends her other sessions.
    let sid = k.open_session(&ana, Duration::from_secs(3600), "it", "127.0.0.1", None).await.unwrap();
    k.send_email_code("ana@x.test", "set a new password").await.unwrap();
    let again = signed(k.account_password("ana@x.test", &newest_code(&k, "ana@x.test").await, "Ana-New-Pass-1").await.unwrap());
    assert!(k.check_session(dir, ana.user, sid).await.is_err());
    assert!(k.load_ctx(dir, ana.user, Some(ana.token_version), "it".into()).await.is_err(), "older tokens end too");
    assert!(k.account_sign_in("ana@x.test", "Acme-Pass-1").await.is_err());
    let ana = signed(k.account_sign_in("ana@x.test", "Ana-New-Pass-1").await.unwrap());
    assert_eq!(again.user, ana.user);
    let me = ctx(&k, &ana).await;
    // A code takes five wrong tries.
    k.send_email_code("ana@x.test", "set a new password").await.unwrap();
    let good = newest_code(&k, "ana@x.test").await;
    for _ in 0..5 {
        let _ = k.account_password("ana@x.test", "999999", "Ana-New-Pass-2").await;
    }
    assert!(k.account_password("ana@x.test", &good, "Ana-New-Pass-2").await.is_err(), "spent");

    // ── An identity made from a membership with an authenticator app takes the app ──
    let dana_globex = k.create_user(&g_admin, "dana@x.test", "Dana Wu", Some("Dana-Pass-12"), &[], false).await.unwrap();
    let (dana_secret, dana_recovery) = two_factor(&k, &k.load_ctx(globex, dana_globex, None, "it".into()).await.unwrap()).await;
    let AccountSignIn::SecondFactor { session, found: true } = k.account_sign_in("dana@x.test", "Dana-Pass-12").await.unwrap() else { panic!() };
    assert!(k.account_found_second_factor(session.tenant, session.user, session.token_version, "000000").await.is_err());
    let AccountSignIn::Verify(m) =
        k.account_found_second_factor(session.tenant, session.user, session.token_version, &code(&dana_secret, now() + 30).unwrap()).await.unwrap()
    else {
        panic!()
    };
    let dana = k.account_verify(&m, &newest_code(&k, "dana@x.test").await).await.unwrap();
    assert!(k.identity(&ctx(&k, &dana).await).await.unwrap().two_factor);
    let AccountSignIn::SecondFactor { session, found: false } = k.account_sign_in("dana@x.test", "Dana-Pass-12").await.unwrap() else { panic!() };
    assert_eq!(session.tenant, dir);
    // The app's codes go on from where the membership's were; its recovery codes came along too.
    assert!(k.second_factor(session.tenant, session.user, session.token_version, &code(&dana_secret, now() + 30).unwrap()).await.is_err(), "used already");
    k.second_factor(session.tenant, session.user, session.token_version, &dana_recovery[0]).await.unwrap();

    // ── A workspace's single sign-on joins its membership to the identity signed in ──
    let eve_globex = k.create_user(&g_admin, "eve@x.test", "Eve", None, &[], false).await.unwrap();
    let eve_other = k.create_user(&a_admin, "eve@y.test", "Eve", None, &[], false).await.unwrap();
    k.create_user(&a_admin, "eve@x.test", "Eve", None, &[], false).await.unwrap();
    k.send_email_code("eve@x.test", "set your password").await.unwrap();
    let eve = signed(k.account_password("eve@x.test", &newest_code(&k, "eve@x.test").await, "Eve-Pass-123").await.unwrap());
    let eve_ctx = ctx(&k, &eve).await;
    let proven = Session { tenant: globex, user: eve_globex, token_version: 1 };
    assert!(k.join_proven(&eve_ctx, &proven).await.unwrap());
    assert!(!k.join_proven(&eve_ctx, &proven).await.unwrap(), "once");
    assert!(!k.join_proven(&eve_ctx, &Session { tenant: acme, user: eve_other, token_version: 1 }).await.unwrap(), "another address");

    // ── Signing out everywhere ──
    let Entry::Ready(into_acme) = k.enter(&me, acme).await.unwrap() else { panic!() };
    let here = k.open_session(&ana, Duration::from_secs(3600), "it", "127.0.0.1", None).await.unwrap();
    let there = k.open_session(&into_acme, Duration::from_secs(3600), "it", "127.0.0.1", None).await.unwrap();
    k.identity_sign_out_everywhere(&me).await.unwrap();
    assert!(k.check_session(dir, ana.user, here).await.is_err());
    assert!(k.check_session(acme, ana_acme, there).await.is_err());
    assert!(k.load_ctx(acme, ana_acme, Some(into_acme.token_version), "it".into()).await.is_err());
    assert!(k.identity(&me).await.is_ok(), "the identity itself stays");

    // ── A server that sends no mail: identities whose address proves nothing ──
    let quiet = boot(db, false).await;
    quiet.create_user(&a_admin, "carl@x.test", "Carl", Some("Carl-Pass-12"), &[], false).await.unwrap();
    let carl = signed(quiet.account_sign_in("carl@x.test", "Carl-Pass-12").await.unwrap());
    let carl_ctx = ctx(&quiet, &carl).await;
    assert!(!quiet.identity(&carl_ctx).await.unwrap().verified);
    assert_eq!(quiet.identity_workspaces(&carl_ctx).await.unwrap().len(), 1, "only what it joined");
    // Whether Carl has an account in Globex is not said: the same page, and the proof fails.
    assert_eq!(quiet.enter(&carl_ctx, globex).await.unwrap(), Entry::Join { click: false, password: true, two_factor: false });
    assert!(quiet.join(&carl_ctx, globex, Proof { password: Some("Carl-Pass-12"), ..Default::default() }).await.is_err());
    assert!(quiet.join(&carl_ctx, globex, Proof::default()).await.is_err());
    assert!(matches!(quiet.send_email_code("carl@x.test", "x").await, Err(Error::Unavailable(_))));
}
