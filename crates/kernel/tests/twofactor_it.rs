//! Two-factor sign-in against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): setup, the second step, replayed and
//! wrong codes, recovery codes, lockout, an administrator's reset and the workspace policy.

use ferrum_kernel::auth::SignIn;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::twofactor::code;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    Kernel::boot(db, catalog, HookRegistry::default(), settings, None).await.expect("boot")
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
    let setup = k.begin_two_factor(&pat).await.unwrap();
    let secret = setup["secret"].as_str().unwrap().to_string();
    assert!(setup["uri"].as_str().unwrap().starts_with("otpauth://totp/"));
    assert!(setup["qr_svg"].as_str().unwrap().contains("<svg"));
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["pending"], json!(true));
    assert_eq!(k.begin_two_factor(&pat).await.unwrap()["secret"], json!(secret), "asking again while pending gives the same secret");
    assert!(k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.is_ok(), "not on until confirmed");
    let wrong = if code(&secret, now()).unwrap() == "000000" { "111111" } else { "000000" };
    assert!(matches!(k.enable_two_factor(&pat, wrong).await, Err(Error::Validation(_))));
    let recovery = k.enable_two_factor(&pat, &code(&secret, now()).unwrap()).await.unwrap();
    assert_eq!(recovery.len(), 10);
    let status = k.two_factor_status(&pat).await.unwrap();
    assert_eq!((status["enabled"].clone(), status["recovery_codes_left"].clone()), (json!(true), json!(10)));
    assert!(matches!(k.begin_two_factor(&pat).await, Err(Error::Invalid(_))), "on already");

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
    k.reset_two_factor(&admin, pat_id).await.unwrap();
    assert!(k.load_ctx(tenant, pat_id, Some(session.token_version), "it".into()).await.is_err(), "signed out");
    let s = k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.expect("a password alone again");
    let pat = k.load_ctx(tenant, pat_id, Some(s.token_version), "it".into()).await.unwrap();
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["recovery_codes_left"], json!(0));

    // A workspace that requires it: those without it may only set it up.
    assert!(k.update_tenant_settings(&pat, json!({"require_two_factor": true}).as_object().unwrap()).await.is_err());
    k.update_tenant_settings(&admin, json!({"require_two_factor": true}).as_object().unwrap()).await.unwrap();
    assert_eq!(k.tenant_settings(&admin).await.unwrap()["require_two_factor"], json!(true));
    assert!(matches!(k.sign_in("twofa", "pat@twofa.test", "Correct-Horse-9").await.unwrap(), SignIn::SetupRequired(_)));
    assert!(k.login("twofa", "pat@twofa.test", "Correct-Horse-9").await.is_err());
    assert_eq!(k.two_factor_status(&pat).await.unwrap()["required"], json!(true));

    // Turning it off takes the password (or a code from the app).
    let secret = k.begin_two_factor(&pat).await.unwrap()["secret"].as_str().unwrap().to_string();
    k.enable_two_factor(&pat, &code(&secret, now()).unwrap()).await.unwrap();
    assert!(matches!(k.new_recovery_codes(&pat, wrong).await, Err(Error::Validation(_))));
    assert_eq!(k.new_recovery_codes(&pat, &code(&secret, now() + 30).unwrap()).await.unwrap().len(), 10);
    assert!(matches!(k.disable_two_factor(&pat, Some("not-my-password"), None).await, Err(Error::Validation(_))));
    k.disable_two_factor(&pat, Some("Correct-Horse-9"), None).await.unwrap();
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
    // A key one character off (never the same character: random keys end in x now and then).
    let tampered = format!("{}{}", &full[..full.len() - 1], if full.ends_with('x') { 'y' } else { 'x' });
    assert!(matches!(k.verify_api_key(&tampered).await, Err(Error::Unauthorized(_))));
}
