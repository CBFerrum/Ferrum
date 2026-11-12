//! Outside people signing themselves up to a portal role (docs/api.md, "Public sign-up"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): off by default and for a role that is not a portal
//! role; on for one role, with its domains and what a page may pass; nobody made until the code is confirmed; the
//! same answer for an address that is someone's; the limits per address and per network address; and the event.

use ferrum_kernel::auth::SignIn;
use ferrum_kernel::portal_signup::{PER_NETWORK_HOUR, PortalSignup, SIGNED_UP, SignupRequest};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

struct Mailer;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Mailer {
    async fn send(&self, _to: &str, _subject: &str, _html: &str) -> ferrum_kernel::Result<()> {
        Ok(())
    }
}

#[test]
fn outside_people_sign_themselves_up() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), Some(Arc::new(Mailer)))
        .await
        .expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "school".into(),
            name: "School".into(),
            admin_email: "admin@school.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let root = k.login("school", "admin@school.test", "Correct-Horse-9").await.unwrap().user;
    let admin = k.load_ctx(tenant, root, None, "it".into()).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into role (tenant_id, name) values ($1, 'Learners'), ($1, 'Tutors')").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.set_role_portal(&admin, "Learners", true).await.unwrap();

    let ask = |role: &str, email: &str, passes: Value, network: &str| {
        let k = k.clone();
        let req = SignupRequest {
            tenant: "school".into(),
            role: role.into(),
            name: "Ann Outside".into(),
            email: email.into(),
            passes: passes.as_object().cloned().unwrap_or_default(),
        };
        let network = network.to_string();
        async move { k.request_portal_signup(&req, &network).await }
    };
    // What the workspace holds: its people, and the mails and events waiting to go.
    let count = |sql: &'static str, arg: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar(sql).bind(tenant).bind(arg).fetch_one(&mut *tx).await.unwrap();
            n
        }
    };
    let people = || count("select count(*) from app_user where tenant_id = $1 and email::text like $2", "%");
    let mails_to = |a: &'static str| count("select count(*) from outbox where tenant_id = $1 and topic = 'ferrum.mail' and payload->>'to' = $2", a);
    let code_for = |a: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let html: String = sqlx::query_scalar(
                "select payload->>'html' from outbox where tenant_id = $1 and topic = 'ferrum.mail' and payload->>'to' = $2 order by id desc limit 1",
            )
            .bind(tenant)
            .bind(a)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            html.split(|c: char| !c.is_ascii_digit()).find(|w| w.len() == 6).unwrap().to_string()
        }
    };

    // ───────────── off by default; on only for a portal role ─────────────
    assert!(matches!(ask("Learners", "ann@learn.test", json!({}), "10.0.0.1").await, Err(Error::NotFound(_))), "off by default");
    let setting = PortalSignup { domains: vec!["learn.test".into()], passes: vec!["course".into()] };
    let e = k.set_portal_signup(&admin, "Tutors", Some(&setting)).await.unwrap_err();
    assert!(e.to_string().contains("not a portal role") || format!("{e:?}").contains("not a portal role"), "{e:?}");
    k.set_portal_signup(&admin, "Learners", Some(&setting)).await.expect("on for the portal role");
    assert!(matches!(ask("Tutors", "ann@learn.test", json!({}), "10.0.0.1").await, Err(Error::NotFound(_))), "not for another role");

    // ───────────── the domains it is for, and what a page may pass ─────────────
    let e = ask("Learners", "ann@elsewhere.test", json!({}), "10.0.0.1").await.unwrap_err();
    assert!(format!("{e:?}").contains("for addresses at learn.test"), "{e:?}");
    let e = ask("Learners", "ann@learn.test", json!({"colour": "red"}), "10.0.0.1").await.unwrap_err();
    assert!(format!("{e:?}").contains("colour is not something this sign-up takes"), "{e:?}");

    // ───────────── nobody is made until the code is confirmed ─────────────
    let before = people().await;
    ask("Learners", "ann@learn.test", json!({"course": "C-101"}), "10.0.0.1").await.expect("asked");
    assert_eq!(people().await, before, "an unconfirmed sign-up leaves no person behind");
    assert_eq!(mails_to("ann@learn.test").await, 1);
    let code = code_for("ann@learn.test").await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    assert!(k.sign_in_with_code("school", "ann@learn.test", wrong).await.is_err());
    assert_eq!(people().await, before, "a wrong code makes nobody");
    let SignIn::Session(s) = k.sign_in_with_code("school", "ann@learn.test", &code).await.expect("confirmed") else { panic!("a session") };
    let ann = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    assert_eq!((ann.roles.clone(), ann.outside), (vec!["Learners".to_string()], true), "a portal person holding the role");
    assert!(k.sign_in_with_code("school", "ann@learn.test", &code).await.is_err(), "a code works once");
    let mut tx = k.db.system_tx().await.unwrap();
    let event: Value =
        sqlx::query_scalar("select payload from outbox where tenant_id = $1 and topic = $2").bind(tenant).bind(SIGNED_UP).fetch_one(&mut *tx).await.unwrap();
    drop(tx);
    assert_eq!(
        (event["user"].as_str(), event["role"].as_str(), event["passed"].clone(), event["name"].as_str()),
        (Some(s.user.to_string().as_str()), Some("Learners"), json!({"course": "C-101"}), Some("Ann Outside")),
        "{event}"
    );

    // ───────────── the same answer for an address that is someone's ─────────────
    ask("Learners", "ann@learn.test", json!({}), "10.0.0.2").await.expect("a portal person: the same answer");
    let mut tx = k.db.system_tx().await.unwrap();
    let codes: i64 = sqlx::query_scalar("select count(*) from sign_in_code where tenant_id = $1 and user_id = $2")
        .bind(tenant)
        .bind(s.user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(codes, 1, "who is a portal person already is sent the usual sign-in code");
    k.set_portal_signup(&admin, "Learners", Some(&PortalSignup::default())).await.unwrap();
    ask("Learners", "admin@school.test", json!({}), "10.0.0.3").await.expect("a member: the same answer");
    assert_eq!(mails_to("admin@school.test").await, 0, "and nothing sent");
    assert_eq!(people().await, before + 1);

    // ───────────── limits: per address, per network address ─────────────
    ask("Learners", "ann@learn.test", json!({}), "10.0.0.4").await.expect("the third this hour");
    assert!(matches!(ask("Learners", "ann@learn.test", json!({}), "10.0.0.5").await, Err(Error::TooManyRequests(_))), "the fourth");
    for i in 0..PER_NETWORK_HOUR {
        let a: &'static str = Box::leak(format!("n{i}@learn.test").into_boxed_str());
        ask("Learners", a, json!({}), "10.9.9.9").await.expect("under the network's limit");
    }
    assert!(matches!(ask("Learners", "late@learn.test", json!({}), "10.9.9.9").await, Err(Error::TooManyRequests(_))), "over it");
    ask("Learners", "late@learn.test", json!({}), "10.9.9.8").await.expect("another network");

    // ───────────── off again ─────────────
    k.set_portal_signup(&admin, "Learners", None).await.unwrap();
    assert!(matches!(ask("Learners", "bob@learn.test", json!({}), "10.0.0.6").await, Err(Error::NotFound(_))));
    let late = code_for("late@learn.test").await;
    assert!(k.sign_in_with_code("school", "late@learn.test", &late).await.is_err(), "a code sent before it went off makes nobody");
}
