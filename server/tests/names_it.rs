//! The names that should never change (docs/operations.md, "Names that should never change"),
//! against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the accounts host
//! without a base domain, and a one-workspace server turning it on and off.

mod common;

use axum::http::{HeaderMap, HeaderValue, header};
use ferrum_kernel::identity::{AccountSignIn, Entry};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::shell::{Place, Shell};
use ferrum_server::state::Settings;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

const PASSWORD: &str = "Correct-Horse-9";

fn shell(k: &Arc<Kernel>, settings: Value, public_url: &str) -> Shell {
    let settings: Settings = serde_json::from_value(settings).expect("settings");
    let storage = Arc::new(loco_rs::storage::Storage::single(loco_rs::storage::drivers::mem::new()));
    Shell::new(k.clone(), storage, &settings, public_url)
}

fn host(h: &str) -> HeaderMap {
    let mut m = HeaderMap::new();
    m.insert(header::HOST, HeaderValue::from_str(h).unwrap());
    m
}

#[test]
fn names_stay_put() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let acme = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();

    accounts_host(&k, acme).await;
    hooks_host(&k, acme).await;
}

/// `FERRUM_HOOKS_HOST` without a base domain: a place of its own (no pages, no sign-in), named in
/// the bootstrap as where outside callers go.
async fn hooks_host(k: &Arc<Kernel>, acme: uuid::Uuid) {
    let off = shell(k, json!({"default_workspace": "acme"}), "https://erp.acme.test");
    assert_eq!(off.hooks_origin(&host("erp.acme.test")), None, "the public address, as before");
    assert_eq!(off.place(&host("cb.acme.test")).await.unwrap(), Place::Workspace(acme));
    let on = shell(k, json!({"default_workspace": "acme", "hooks_host": "cb.acme.test"}), "https://erp.acme.test");
    assert_eq!(on.hooks_origin(&host("erp.acme.test")).as_deref(), Some("https://cb.acme.test"));
    assert_eq!(on.place(&host("cb.acme.test")).await.unwrap(), Place::Hooks);
    assert_eq!(on.place(&host("erp.acme.test")).await.unwrap(), Place::Workspace(acme));
    assert!(on.is_own_host("cb.acme.test"));
    assert_eq!(on.workspace_at("cb.acme.test").await.unwrap(), None, "no sign-in is handed to it");
}

/// `FERRUM_ACCOUNTS_HOST` without a base domain, on a one-workspace server (shape A).
async fn accounts_host(k: &Arc<Kernel>, acme: uuid::Uuid) {
    // Off: as today. No accounts host, and every host is the one workspace.
    let off = shell(k, json!({"default_workspace": "acme"}), "https://erp.acme.test");
    assert_eq!(off.accounts_origin(&host("erp.acme.test")), None);
    assert_eq!(off.place(&host("id.acme.test")).await.unwrap(), Place::Workspace(acme));

    // On: that host is the accounts place; the workspace keeps its address.
    let on = shell(k, json!({"default_workspace": "acme", "accounts_host": "ID.acme.test."}), "https://erp.acme.test");
    assert_eq!(on.accounts_origin(&host("erp.acme.test")).as_deref(), Some("https://id.acme.test"));
    assert_eq!(on.place(&host("id.acme.test")).await.unwrap(), Place::Accounts);
    assert_eq!(on.place(&host("erp.acme.test")).await.unwrap(), Place::Workspace(acme));
    assert!(on.is_own_host("id.acme.test") && !on.is_own_host("erp.acme.test"));
    // A sign-in is handed only to the workspace's own address (the server's public one here), never
    // to a host someone names.
    assert_eq!(on.workspace_at("erp.acme.test").await.unwrap(), Some(acme));
    for other in ["evil.test", "id.acme.test", "erp.acme.test.evil.test"] {
        assert_eq!(on.workspace_at(other).await.unwrap(), None, "{other}");
    }

    // With a base domain and no setting: exactly as before.
    let base = shell(k, json!({"base_domain": "ferrum.test", "default_workspace": "acme"}), "https://ferrum.test");
    assert_eq!(base.accounts_origin(&host("acme.ferrum.test")).as_deref(), Some("https://accounts.ferrum.test"));
    assert_eq!(base.place(&host("accounts.ferrum.test")).await.unwrap(), Place::Accounts);
    assert_eq!(base.place(&host("hooks.ferrum.test")).await.unwrap(), Place::Hooks);
    assert_eq!(base.place(&host("ferrum.test")).await.unwrap(), Place::Nowhere);
    assert_eq!(base.workspace_at("ferrum.test").await.unwrap(), None, "no default workspace under a base domain");
    let both = shell(k, json!({"base_domain": "ferrum.test", "accounts_host": "login.ferrum.test"}), "https://ferrum.test");
    assert_eq!(both.place(&host("login.ferrum.test")).await.unwrap(), Place::Accounts, "the setting wins");
    assert_eq!(both.place(&host("accounts.ferrum.test")).await.unwrap(), Place::Nowhere);

    // The first sign-in at the accounts host makes the identity from the workspace's password, and
    // hands it to the workspace's host with a one-use code bound to that host.
    k.ensure_directory().await.unwrap();
    let AccountSignIn::Signed(s) = k.account_sign_in("admin@acme.test", PASSWORD).await.unwrap() else { panic!("signed in") };
    let me = k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let Entry::Ready(m) = k.enter(&me, acme).await.unwrap() else { panic!("ready to enter") };
    let code = k.mint_host_code(&m, Some(me.user), "erp.acme.test").await.unwrap();
    assert!(k.redeem_host_code(&code, "evil.test").await.is_err(), "not on another host");
    assert!(k.redeem_host_code(&code, "erp.acme.test").await.is_err(), "and spent by the try");
    let code = k.mint_host_code(&m, Some(me.user), "erp.acme.test").await.unwrap();
    let back = k.redeem_host_code(&code, "erp.acme.test").await.unwrap();
    assert_eq!((back.tenant, back.user), (m.tenant, m.user));
    assert!(k.redeem_host_code(&code, "erp.acme.test").await.is_err(), "once");
    // Turned off again, the workspace's own password still signs in there (it was copied, not moved).
    assert!(k.login("acme", "admin@acme.test", PASSWORD).await.is_ok());
}
