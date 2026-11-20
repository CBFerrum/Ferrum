//! Two-factor sign-in against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): setup, the second step, replayed and
//! wrong codes, recovery codes, lockout, an administrator's reset and the workspace policy.

use ferrum_kernel::auth::SignIn;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::twofactor::code;
use ferrum_kernel::{Access, Action, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

async fn kernel() -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    boot(db, None).await
}

/// A mailer that sends nothing: mail stays in the outbox, where the test reads it.
struct Quiet;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Quiet {
    async fn send(&self, _: &str, _: &str, _: &str) -> ferrum_kernel::Result<()> {
        Ok(())
    }
}

/// Another server on the same database, with or without mail.
async fn boot(db: Db, mailer: Option<Arc<dyn ferrum_kernel::Mailer>>) -> Arc<Kernel> {
    let url = url();
    let opener = ferrum_kernel::vault::Opener {
        database_url: url.clone(),
        file: "/nonexistent/root.key".into(),
        kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
        ..Default::default()
    };
    ferrum_kernel::vault::open(&db, opener, false).await.expect("the vault");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings::default();
    Kernel::boot(db, catalog, HookRegistry::default(), settings, mailer).await.expect("boot")
}

/// The codes mailed to `to` to confirm a second factor, newest first.
async fn mailed(k: &Kernel, to: &str) -> Vec<String> {
    let mut tx = k.db.system_tx().await.unwrap();
    let subjects: Vec<String> = sqlx::query_scalar(
        "select payload->>'subject' from outbox where topic = 'ferrum.mail' and payload->>'to' = $1 and payload->>'subject' like '% to turn on two-factor sign-in' order by id desc",
    )
    .bind(to)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    subjects.iter().map(|s| s[..6].to_string()).collect()
}

/// What the audit log keeps of a person's turning on two-factor sign-in.
async fn enabled_audit(k: &Kernel, tenant: uuid::Uuid, user: uuid::Uuid) -> serde_json::Value {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let details: serde_json::Value = sqlx::query_scalar(
        "select coalesce(diff, 'null') from audit_event where tenant_id = $1 and record_id = $2 and action = 'enable_two_factor' order by id desc limit 1",
    )
    .bind(tenant)
    .bind(user)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    details
}

#[tokio::test]
async fn two_factor_sign_in() {
    let k = kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "twofa".into(),
            name: "Two Factor Co".into(),
            admin_email: "admin@twofa.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let s = k.login("twofa", "admin@twofa.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let pat_id = k.create_user(&admin, "pat@twofa.test", "Pat", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let s = k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.unwrap();
    let pat = k.load_ctx(tenant, pat_id, Some(s.token_version), "it".into()).await.unwrap();

    // Setup: a secret for the app, on only once a code from it confirms it.
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["enabled"], json!(false));
    let setup = k.begin_two_factor(&ferrum_testkit::at_screen(&k, &pat).await).await.unwrap();
    let secret = setup["secret"].as_str().unwrap().to_string();
    assert!(setup["uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    assert!(setup["qr_svg"].as_str().unwrap().contains("<svg"));
    assert_eq!(setup["mailed"], json!(false), "this server sends no mail");
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["pending"], json!(true));
    assert_eq!(
        k.begin_two_factor(&ferrum_testkit::at_screen(&k, &pat).await).await.unwrap()["secret"],
        json!(secret),
        "asking again while pending gives the same secret"
    );
    assert!(k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.is_ok(), "not on until confirmed");
    let wrong = if code(&secret, now()).unwrap() == "000000" { "111111" } else { "000000" };
    assert!(matches!(k.enable_two_factor(&ferrum_testkit::at_screen(&k, &pat).await, wrong).await, Err(Error::Validation(_))));
    let recovery = k.enable_two_factor(&ferrum_testkit::at_screen(&k, &pat).await, &code(&secret, now()).unwrap()).await.unwrap();
    assert_eq!(recovery.len(), 10);
    assert_eq!(enabled_audit(&k, tenant, pat_id).await["mailed"], json!(false), "audited as set up without a mailed code");
    assert!(
        k.doctor(10).await.unwrap().iter().any(|f| f.check == "two_factor.mail" && f.detail.contains("without a code mailed")),
        "the doctor warns on a server without mail"
    );
    let status = k.two_factor_status(&pat).await.unwrap();
    assert_eq!((status["enabled"].clone(), status["recovery_codes_left"].clone()), (json!(true), json!(10)));
    assert!(matches!(k.begin_two_factor(&ferrum_testkit::at_screen(&k, &pat).await).await, Err(Error::Invalid(_))), "on already");

    // The password alone no longer signs in.
    assert!(matches!(k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await, Err(Error::Unauthorized(_))));
    let SignIn::SecondFactor(ticket) = k.sign_in("twofa", "pat@twofa.test", "Correct-Horse-9").await.unwrap() else { panic!("a second step") };
    assert!(k.sign_in("twofa", "pat@twofa.test", "wrong-password").await.is_err());

    // The next step's code signs in; the same code a second time does not (nor the one that
    // turned it on).
    let next = code(&secret, now() + 30).unwrap();
    let session = k.second_factor(tenant, pat_id, ticket.token_version, &next).await.unwrap();
    assert_eq!(session.user, pat_id);
    assert!(matches!(k.second_factor(tenant, pat_id, ticket.token_version, &next).await, Err(Error::Unauthorized(_))), "replayed");

    // A recovery code works once, however it is typed.
    k.second_factor(tenant, pat_id, ticket.token_version, &recovery[0]).await.unwrap();
    assert!(k.second_factor(tenant, pat_id, ticket.token_version, &recovery[0]).await.is_err(), "used");
    k.second_factor(tenant, pat_id, ticket.token_version, &format!(" {} ", recovery[1].to_uppercase().replace('-', " "))).await.unwrap();
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["recovery_codes_left"], json!(8));

    // A ticket from before a password change is void.
    assert!(matches!(k.second_factor(tenant, pat_id, ticket.token_version + 1, &recovery[2]).await, Err(Error::Unauthorized(_))));

    // Wrong codes lock the account like wrong passwords, password step included.
    for _ in 0..5 {
        let _ = k.second_factor(tenant, pat_id, ticket.token_version, wrong).await;
    }
    assert!(matches!(k.second_factor(tenant, pat_id, ticket.token_version, &recovery[2]).await, Err(Error::TooManyRequests(_))));
    assert!(matches!(k.sign_in("twofa", "pat@twofa.test", "Correct-Horse-9").await, Err(Error::TooManyRequests(_))));

    // Only an administrator resets it: off, unlocked, and every session ended.
    assert!(matches!(k.reset_two_factor(&pat, pat_id).await, Err(Error::Forbidden(_))));
    k.reset_two_factor(&ferrum_testkit::at_screen(&k, &admin).await, pat_id).await.unwrap();
    assert!(k.load_ctx(tenant, pat_id, Some(session.token_version), "it".into()).await.is_err(), "signed out");
    let s = k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.expect("a password alone again");
    let pat = k.load_ctx(tenant, pat_id, Some(s.token_version), "it".into()).await.unwrap();
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["recovery_codes_left"], json!(0));

    // The offline policy (docs/offline.md) is a workspace setting like this one: allowed, and hours.
    k.update_tenant_settings(&admin, json!({"offline": {"allowed": false, "hours": 8}}).as_object().unwrap()).await.unwrap();
    assert_eq!(k.tenant_settings(&admin).await.unwrap()["offline"], json!({"allowed": false, "hours": 8}));
    for bad in [json!({"hours": 0}), json!({"hours": 721}), json!({"allowed": "no"}), json!({"keep": true}), json!(true)] {
        assert!(k.update_tenant_settings(&admin, json!({"offline": bad}).as_object().unwrap()).await.is_err(), "{bad}");
    }

    // A workspace that requires it: those without it may only set it up.
    assert!(k.update_tenant_settings(&pat, json!({"require_two_factor": true}).as_object().unwrap()).await.is_err());
    k.update_tenant_settings(&admin, json!({"require_two_factor": true}).as_object().unwrap()).await.unwrap();
    assert_eq!(k.tenant_settings(&admin).await.unwrap()["require_two_factor"], json!(true));
    assert!(matches!(k.sign_in("twofa", "pat@twofa.test", "Correct-Horse-9").await.unwrap(), SignIn::SetupRequired(_)));
    assert!(k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.is_err());
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["required"], json!(true));

    // Turning it off takes the password (or a code from the app).
    let secret = k.begin_two_factor(&ferrum_testkit::at_screen(&k, &pat).await).await.unwrap()["secret"].as_str().unwrap().to_string();
    k.enable_two_factor(&ferrum_testkit::at_screen(&k, &pat).await, &code(&secret, now()).unwrap()).await.unwrap();
    assert!(matches!(k.new_recovery_codes(&ferrum_testkit::at_screen(&k, &pat).await, wrong).await, Err(Error::Validation(_))));
    assert_eq!(k.new_recovery_codes(&ferrum_testkit::at_screen(&k, &pat).await, &code(&secret, now() + 30).unwrap()).await.unwrap().len(), 10);
    assert!(matches!(k.disable_two_factor(&ferrum_testkit::at_screen(&k, &pat).await, Some("not-my-password"), None).await, Err(Error::Validation(_))));
    k.disable_two_factor(&ferrum_testkit::at_screen(&k, &pat).await, Some("Correct-Horse-9"), None).await.unwrap();
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["enabled"], json!(false));
    assert!(matches!(k.sign_in("twofa", "pat@twofa.test", "Correct-Horse-9").await.unwrap(), SignIn::SetupRequired(_)));

    // API keys: one to the whole API, and an app password for calendar and contacts apps, which
    // says so. Neither is a sign-in, so neither asks for the second factor.
    let (_, full) = k.create_api_key(&pat, pat_id, "Script", None, None).await.unwrap();
    let (dav_id, dav) = k.create_api_key(&pat, pat_id, "Phone", None, Some("dav")).await.unwrap();
    assert_eq!(k.verify_api_key(&full).await.unwrap().1, None);
    let (session, scope) = k.verify_api_key(&dav).await.unwrap();
    assert_eq!((session.user, scope.as_deref()), (pat_id, Some("dav")));
    assert!(matches!(k.create_api_key(&pat, pat_id, "Odd", None, Some("mail")).await, Err(Error::Validation(_))));
    assert!(matches!(k.create_api_key(&pat, admin.user, "Theirs", None, Some("dav")).await, Err(Error::Forbidden(_))), "keys are for oneself");
    k.revoke_api_key(&pat, dav_id).await.unwrap();
    assert!(matches!(k.verify_api_key(&dav).await, Err(Error::Unauthorized(_))));
    // A records key names the entities it reaches and what it may do there, and is kept in order;
    // in use, its owner's rights are narrowed to that, never to an administrator's.
    for bad in ["records:Nope=read", "records:Currency=fly", "records:Currency"] {
        assert!(matches!(k.create_api_key(&admin, admin.user, "Bad", None, Some(bad)).await, Err(Error::Validation(_))), "{bad}");
    }
    let (_, reader) = k.create_api_key(&admin, admin.user, "Rates", None, Some("records:Currency=read;ExchangeRate=write+read")).await.unwrap();
    let (_, scope) = k.verify_api_key(&reader).await.unwrap();
    assert_eq!(scope.as_deref(), Some("records:Currency=read;ExchangeRate=read+write"));
    let grants = ferrum_kernel::auth::records_scope(scope.as_deref().unwrap()).unwrap();
    let narrowed = admin.perms.limited_to(&grants, &Default::default());
    assert!(!narrowed.admin);
    assert_eq!((narrowed.access("Currency", Action::Read), narrowed.access("Currency", Action::Write)), (Access::All, Access::None));
    assert_eq!((narrowed.access("ExchangeRate", Action::Write), narrowed.access("Company", Action::Read)), (Access::All, Access::None));
    // A key one character off (never the same character: random keys end in x now and then).
    let tampered = format!("{}{}", &full[..full.len() - 1], if full.ends_with('x') { 'y' } else { 'x' });
    assert!(matches!(k.verify_api_key(&tampered).await, Err(Error::Unauthorized(_))));

    a_first_second_factor_needs_the_code_mailed_to_the_person(&k, tenant, &admin).await;
}

/// Where the server mails, a first second factor takes effect only with the code mailed to the person (API-K2): a password
/// alone cannot put someone else's authenticator on the account.
async fn a_first_second_factor_needs_the_code_mailed_to_the_person(k: &Kernel, tenant: uuid::Uuid, admin: &ferrum_kernel::Ctx) {
    let m = boot(k.db.clone(), Some(Arc::new(Quiet))).await;
    let sam_id = m.create_user(admin, "sam@twofa.test", "Sam", Some("Correct-Horse-9"), &[], false).await.unwrap();
    // The workspace requires a second factor by now: Sam's password gives only the session that sets one up.
    let SignIn::SetupRequired(s) = m.sign_in("twofa", "sam@twofa.test", "Correct-Horse-9").await.unwrap() else { panic!("setup required") };
    let sam = m.load_ctx(tenant, sam_id, Some(s.token_version), "it".into()).await.unwrap();
    let screen = || ferrum_testkit::at_screen(&m, &sam);
    let setup = m.begin_two_factor(&screen().await).await.unwrap();
    assert_eq!(setup["mailed"], json!(true));
    let secret = setup["secret"].as_str().unwrap().to_string();
    let sent = mailed(&m, "sam@twofa.test").await;
    assert_eq!(sent.len(), 1, "one code mailed: {sent:?}");
    assert!(sent[0].bytes().all(|b| b.is_ascii_digit()), "{sent:?}");
    m.begin_two_factor(&screen().await).await.unwrap();
    assert_eq!(mailed(&m, "sam@twofa.test").await.len(), 1, "the page shown again mails no second code while the first works");

    // The authenticator's code alone is not enough, nor a wrong mailed code; the error never carries the code.
    let refused = m.enable_two_factor(&screen().await, &code(&secret, now()).unwrap()).await;
    assert!(matches!(&refused, Err(Error::Validation(f)) if f.iter().any(|f| f.field == "mailed_code")), "{refused:?}");
    let wrong = if sent[0] == "000000" { "111111" } else { "000000" };
    let refused = m.enable_two_factor_mailed(&screen().await, &code(&secret, now()).unwrap(), Some(wrong)).await;
    assert!(matches!(&refused, Err(Error::Validation(_))) && !format!("{refused:?}").contains(&sent[0]), "{refused:?}");
    assert_eq!(m.two_factor_status(&sam).await.unwrap()["enabled"], json!(false));
    // A mailed code is not a sign-in code: it does not sign anyone in.
    assert!(m.sign_in_with_code("twofa", "sam@twofa.test", &sent[0]).await.is_err());

    // Five wrong codes use it up: the right one no longer works, and starting again mails a new one.
    for _ in 0..4 {
        assert!(m.enable_two_factor_mailed(&screen().await, &code(&secret, now()).unwrap(), Some(wrong)).await.is_err());
    }
    assert!(m.enable_two_factor_mailed(&screen().await, &code(&secret, now()).unwrap(), Some(&sent[0])).await.is_err(), "used up");
    m.begin_two_factor(&screen().await).await.unwrap();
    let again = mailed(&m, "sam@twofa.test").await;
    assert_eq!(again.len(), 2, "a new code once the first is used up");

    let recovery = m.enable_two_factor_mailed(&screen().await, &code(&secret, now()).unwrap(), Some(&again[0])).await.unwrap();
    assert_eq!(recovery.len(), 10);
    assert_eq!(m.two_factor_status(&sam).await.unwrap()["enabled"], json!(true));
    let audited = enabled_audit(&m, tenant, sam_id).await;
    assert_eq!(audited["mailed"], json!(true));
    assert!(!audited.to_string().contains(&again[0]), "no code in the audit log: {audited}");
    assert!(!m.doctor(10).await.unwrap().iter().any(|f| f.check == "two_factor.mail" && f.detail.contains("without")), "a server that mails is not warned");
}
