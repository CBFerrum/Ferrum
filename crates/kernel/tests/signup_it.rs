//! Workspace sign-up and plans (FERRUM-CLOUD-002) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`): a
//! closed server takes no sign-ups; an open one mails a code to an address of a domain it takes,
//! and the code makes a workspace whose maker is its administrator, on the operator's plan; a plan's
//! people and storage quotas hold; an address makes a few workspaces at most.

use ferrum_kernel::plans;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::signup::{SignUp, SignupPolicy};
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

const PASSWORD: &str = "Correct-Horse-9";

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

async fn boot(db: Db, signup: SignupPolicy) -> Arc<Kernel> {
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let plans = json!({"free": {"people": 2, "storage_bytes": 1000, "ai_tokens_month": 50000}, "team": {"people": 10}});
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), plans: plans.as_object().unwrap().clone(), signup, ..Default::default() };
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

/// The newest code mailed to `to`.
async fn newest_code(k: &Kernel, to: &str) -> String {
    let mut tx = k.db.system_tx().await.unwrap();
    let subject: String = sqlx::query_scalar(
        "select payload->>'subject' from outbox where topic = 'ferrum.mail' and payload->>'to' = $1 and payload->>'subject' like '% is your Ferrum code' order by id desc limit 1",
    )
    .bind(to)
    .fetch_one(&mut *tx)
    .await
    .expect("a code was mailed");
    tx.commit().await.unwrap();
    subject.split(' ').next().unwrap().to_string()
}

fn form(email: &str, code: &str, slug: &str) -> SignUp {
    SignUp { email: email.into(), code: code.into(), slug: slug.into(), name: format!("{slug} Ltd"), password: PASSWORD.into(), full_name: Some("Ana Rao".into()) }
}

#[test]
fn sign_up_and_plans() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let db = Db::connect(DbConfig { url: url(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");

    // The operator's plans are checked before the server starts.
    assert!(plans::validate(json!({"free": {"people": 2}}).as_object().unwrap()).is_ok());
    assert!(plans::validate(json!({"free": {"seats": 2}}).as_object().unwrap()).is_err(), "not a quota");
    assert!(plans::validate(json!({"free": {"people": -1}}).as_object().unwrap()).is_err(), "not a whole number");
    assert!(plans::validate(json!({"Free Plan": {}}).as_object().unwrap()).is_err(), "not a plan name");

    // A server that takes no sign-ups mails nothing and makes nothing.
    let closed = boot(db.clone(), SignupPolicy::default()).await;
    assert!(matches!(closed.send_signup_code("ana@x.test").await, Err(Error::Forbidden(_))));
    assert!(matches!(closed.sign_up(form("ana@x.test", "123456", "anaco")).await, Err(Error::Forbidden(_))));

    let policy = SignupPolicy { open: true, domains: vec!["x.test".into()], packages: vec![], plan: Some("free".into()), per_address: 2 };
    let k = boot(db, policy).await;
    k.ensure_directory().await.unwrap();
    assert!(matches!(k.send_signup_code("eve@y.test").await, Err(Error::Validation(_))), "a domain it does not take");
    assert!(matches!(k.send_signup_code("not an address").await, Err(Error::Validation(_))));

    // Ana is a member nowhere, and still gets a code.
    k.send_signup_code("Ana@X.test").await.unwrap();
    let code = newest_code(&k, "ana@x.test").await;
    assert!(matches!(k.sign_up(form("ana@x.test", "000000", "anaco")).await, Err(Error::Unauthorized(_))), "a wrong code");
    let weak = SignUp { password: "short".into(), ..form("ana@x.test", &code, "anaco") };
    assert!(matches!(k.sign_up(weak).await, Err(Error::Validation(_))), "a weak password is refused before the code is spent");
    assert!(matches!(k.sign_up(form("ana@x.test", &code, "accounts")).await, Err(Error::Validation(_))), "a name the cluster keeps");
    let anaco = k.sign_up(form("ana@x.test", &code, "anaco")).await.unwrap();
    assert!(matches!(k.sign_up(form("ana@x.test", &code, "anaco-two")).await, Err(Error::Unauthorized(_))), "a code works once");

    // She is its administrator, and it is on the sign-up plan.
    let s = k.login("anaco", "ana@x.test", PASSWORD).await.unwrap();
    let ana = k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    assert!(ana.is_admin);
    assert_eq!(k.plan_of(anaco).await.unwrap().as_deref(), Some("free"));
    let usage = k.usage(&ana).await.unwrap();
    assert_eq!(usage["quotas"], json!({"people": 2, "storage_bytes": 1000, "ai_tokens_month": 50000}));

    // Two people fit the free plan; a third does not, until someone is turned off.
    let bo = k.create_user(&ana, "bo@x.test", "Bo", Some("Bo-Pass-12345"), &[], false).await.unwrap();
    let full = k.create_user(&ana, "cy@x.test", "Cy", Some("Cy-Pass-12345"), &[], false).await;
    assert!(matches!(&full, Err(Error::Invalid(m)) if m.contains("room for 2 people")), "{full:?}");
    k.set_user_active(&ana, bo, false).await.unwrap();
    let cy = k.create_user(&ana, "cy@x.test", "Cy", Some("Cy-Pass-12345"), &[], false).await.unwrap();
    assert!(matches!(k.set_user_active(&ana, bo, true).await, Err(Error::Invalid(_))), "turning someone back on needs room too");

    // A larger plan makes room; an unknown one is refused.
    k.set_plan(anaco, "team").await.unwrap();
    k.set_user_active(&ana, bo, true).await.unwrap();
    assert!(matches!(k.set_plan(anaco, "gold").await, Err(Error::Validation(_))));
    assert_eq!(k.plan_of(anaco).await.unwrap().as_deref(), Some("team"));

    // The free plan's storage holds files to its size.
    k.set_plan(anaco, "free").await.unwrap();
    assert!(k.check_storage_quota(anaco, 500).await.is_ok());
    assert!(k.check_storage_quota(anaco, 2000).await.is_err());
    k.set_user_active(&ana, cy, false).await.unwrap();

    // A taken name is refused without spending the code; an address makes two workspaces at most.
    k.send_signup_code("ana@x.test").await.unwrap();
    let code = newest_code(&k, "ana@x.test").await;
    assert!(matches!(k.sign_up(form("ana@x.test", &code, "anaco")).await, Err(Error::Validation(_))));
    k.sign_up(form("ana@x.test", &code, "anaco-two")).await.unwrap();
    k.send_signup_code("ana@x.test").await.unwrap();
    let code = newest_code(&k, "ana@x.test").await;
    assert!(matches!(k.sign_up(form("ana@x.test", &code, "anaco-three")).await, Err(Error::Forbidden(_))));
}
