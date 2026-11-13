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
    link_host(&k, acme).await;
    framing_hosts(&k, acme).await;
    connector_return(acme).await;
}

/// A connector's sign-in started on a second host of the workspace comes back to `PUBLIC_URL` (the
/// one return address registered with the provider), goes on to the host it started from, and
/// finishes there; a state for a host that no longer serves the workspace, or one used already, is
/// refused.
async fn connector_return(acme: uuid::Uuid) {
    use axum::routing::post;
    use ferrum_server::controllers::connections::{Back, callback_page, started_elsewhere};
    let token = axum::Router::new().route("/token", post(|| async { axum::Json(json!({"access_token": "at-Qq1", "expires_in": 3600})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, token).await.unwrap() });

    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 4, ..Default::default() }).await.expect("connect");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let settings = KernelSettings {
        secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])),
        public_url: Some("https://erp.acme.test".into()),
        connection_hosts: vec!["127.0.0.1".into()],
        ..Default::default()
    };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let sh = shell(&k, json!({}), "https://erp.acme.test");
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let desc = json!({"base": provider, "auth": {"type": "oauth", "provider": {"authorize_url": "https://id.example.test/authorize", "token_url": format!("{provider}/token"), "client_id": "ferrum-test"}},
        "operations": {"me": {"path": "/me"}}});
    let input: ferrum_kernel::connections::ConnectionInput =
        serde_json::from_value(json!({"name": "mail", "label": "Mail", "description": desc, "enabled": true})).unwrap();
    let conn = k.save_connection_def(&admin, None, &input).await.unwrap();
    let state_of = |url: String| reqwest::Url::parse(&url).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    let page = |k: &Arc<Kernel>, admin: &ferrum_kernel::Ctx, sid, state: &str| {
        let (k, admin, back) = (k.clone(), admin.clone(), Back { state: Some(state.to_string()), code: Some("CODE-1".into()), error: None });
        async move { callback_page(&k, &admin, Some(sid), &back).await.map(|r| r.status().as_u16()).unwrap_or(0) }
    };

    // Started on staff.acme.test (verified, not the main host): back at PUBLIC_URL, sent on there.
    let sid = uuid::Uuid::now_v7();
    let state = state_of(k.connect_start_on(&admin, conn, sid, Some("staff.acme.test")).await.unwrap());
    let query = format!("state={state}&code=CODE-1");
    let on = started_elsewhere(&k, &sh, &host("erp.acme.test"), Some(&query), Some(&state)).await.unwrap();
    assert_eq!(on, Some(format!("https://staff.acme.test/api/connections/callback?{query}")));
    // There, nothing to pass on: the person's session finishes it, once.
    assert_eq!(started_elsewhere(&k, &sh, &host("staff.acme.test"), Some(&query), Some(&state)).await.unwrap(), None);
    assert_eq!(page(&k, &admin, sid, &state).await, 200, "connected");
    assert_eq!(page(&k, &admin, sid, &state).await, 401, "a state replayed after use");

    // A host that no longer serves the workspace: answered where it landed, which has no session.
    k.add_host(&admin, "old.acme.test", "shell").await.unwrap();
    k.host_checked("old.acme.test", true).await.unwrap();
    let sid = uuid::Uuid::now_v7();
    let state = state_of(k.connect_start_on(&admin, conn, sid, Some("old.acme.test")).await.unwrap());
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant_host set released_at = now() - interval '200 days' where host = 'old.acme.test'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(started_elsewhere(&k, &sh, &host("erp.acme.test"), Some("x"), Some(&state)).await.unwrap(), None, "not sent on");
    // Nor to another workspace's host, nor with a state this server did not sign.
    let state = state_of(k.connect_start_on(&admin, conn, uuid::Uuid::now_v7(), Some("globex.erp.test")).await.unwrap());
    assert_eq!(started_elsewhere(&k, &sh, &host("erp.acme.test"), Some("x"), Some(&state)).await.unwrap(), None, "another workspace's host");
    assert_eq!(started_elsewhere(&k, &sh, &host("erp.acme.test"), Some("x"), Some("forged.state.mac")).await.unwrap(), None);
    // A person on another session cannot finish someone's sign-in, even on the right host.
    let state = state_of(k.connect_start_on(&admin, conn, uuid::Uuid::now_v7(), Some("staff.acme.test")).await.unwrap());
    assert_eq!(page(&k, &admin, uuid::Uuid::now_v7(), &state).await, 401, "another browser session");
}

/// An isolated app may be framed by every host that serves the workspace's shell, its main one first.
async fn framing_hosts(k: &Arc<Kernel>, acme: uuid::Uuid) {
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    assert_eq!(k.serving_hosts(acme, "shell").await.unwrap(), vec!["acme.erp.test".to_string()]);
    k.add_host(&admin, "staff.acme.test", "shell").await.unwrap();
    assert_eq!(k.serving_hosts(acme, "shell").await.unwrap(), vec!["acme.erp.test".to_string()], "not before it is verified");
    k.host_checked("staff.acme.test", true).await.unwrap();
    assert_eq!(k.serving_hosts(acme, "shell").await.unwrap(), vec!["acme.erp.test".to_string(), "staff.acme.test".to_string()]);
    assert_eq!(k.serving_hosts(acme, "portal").await.unwrap(), vec!["help.acme.test".to_string()]);
}

/// `FERRUM_LINK_HOST`: off, links are the workspace's host and the path, as before; on, they go
/// through `/go/{workspace}/{s|p}`, which leads on to the workspace's current host and is never an
/// open redirect.
async fn link_host(k: &Arc<Kernel>, acme: uuid::Uuid) {
    use ferrum_kernel::notify::Audience::{Outside, Staff};
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    k.add_subdomain(acme, "acme.erp.test").await.unwrap();
    k.add_host(&admin, "help.acme.test", "portal").await.unwrap();
    k.host_checked("help.acme.test", true).await.unwrap();
    k.make_primary_host(&admin, "help.acme.test").await.unwrap();
    let globex = k
        .create_tenant(NewTenant {
            slug: "globex".into(),
            name: "Globex".into(),
            admin_email: "admin@globex.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    k.add_subdomain(globex, "globex.erp.test").await.unwrap();

    // Off: exactly the workspace's host and the path.
    assert_eq!(k.person_link(acme, Staff, "/open/Note/1").await.as_deref(), Some("https://acme.erp.test/open/Note/1"));
    assert_eq!(k.person_link(acme, Outside, "/open/Note/1").await, k.link_base(acme).await.map(|b| format!("{b}/open/Note/1")));

    // On.
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 4, ..Default::default() }).await.expect("connect");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let settings = KernelSettings { link_host: Some("go.acme.test".into()), public_url: Some("https://erp.acme.test".into()), ..Default::default() };
    let on = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    assert_eq!(on.person_link(acme, Staff, "/open/Note/1").await.as_deref(), Some("https://go.acme.test/go/acme/s/open/Note/1"));
    assert_eq!(
        on.person_link(acme, Outside, "/api/messages/opt-out?token=t").await.as_deref(),
        Some("https://go.acme.test/go/acme/p/api/messages/opt-out?token=t")
    );

    // `/go/`, answered on the link host whatever else it is (here the accounts host too).
    let sh = shell(&on, json!({"link_host": "go.acme.test", "accounts_host": "go.acme.test"}), "https://erp.acme.test");
    assert!(sh.on_link_host(&host("go.acme.test")) && sh.on_link_host(&host("GO.acme.test:443")));
    assert!(!sh.on_link_host(&host("acme.erp.test")));
    assert!(!shell(&on, json!({}), "https://erp.acme.test").on_link_host(&host("go.acme.test")), "off");
    let go = |rest: &'static str, q: Option<&'static str>| {
        let sh = &sh;
        async move { sh.go_target(rest, q).await.unwrap() }
    };
    assert_eq!(go("acme/s/open/Note/1", Some("tab=2")).await.as_deref(), Some("https://acme.erp.test/open/Note/1?tab=2"));
    assert_eq!(go("acme/p/open/Note/1", None).await.as_deref(), Some("https://help.acme.test/open/Note/1"), "the portal's host");
    assert_eq!(go("globex/p/open/Note/1", None).await.as_deref(), Some("https://globex.erp.test/open/Note/1"), "no portal host: the main one");
    assert_eq!(go("acme/s", None).await.as_deref(), Some("https://acme.erp.test/"));
    for (rest, why) in [
        ("nobody/s/x", "unknown workspace"),
        ("acme/x/open", "no such audience"),
        ("accounts/s/", "the directory"),
        ("acme/s//evil.example", "scheme-relative"),
        ("acme/s/\\evil.example", "backslash"),
        ("acme/s/x\\y", "backslash inside"),
    ] {
        assert_eq!(go(rest, None).await, None, "{why}");
    }
    // What the path or query says never moves the host.
    for (rest, q) in [
        ("acme/s/%2F%2Fevil.example", None),
        ("acme/s/%5Cevil.example", None),
        ("acme/s/open/Note/1", Some("host=evil.example&next=https://evil.example")),
        ("acme/s/@evil.example", None),
    ] {
        let to = go(rest, q).await.expect(rest);
        assert_eq!(reqwest::Url::parse(&to).unwrap().host_str(), Some("acme.erp.test"), "{to}");
    }
    // A closed workspace: 404.
    let mut tx = on.db.system_tx().await.unwrap();
    sqlx::query("update tenant set status = 'suspended' where id = $1").bind(globex).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let fresh = shell(&on, json!({"link_host": "go.acme.test"}), "https://erp.acme.test");
    assert_eq!(fresh.go_target("globex/s/", None).await.unwrap(), None, "closed");

    // The route order on a link host that is also the accounts host: `/go/…` first, the accounts
    // host's own pages as before.
    let both = host("go.acme.test");
    assert_eq!(sh.place(&both).await.unwrap(), Place::Accounts);
    assert!(sh.is_go(&both, "go/acme/s/") && sh.is_go(&both, "go"));
    assert_eq!(sh.go_target("acme/s/", None).await.unwrap().as_deref(), Some("https://acme.erp.test/"), "/go/acme/s/ there redirects");
    for other in ["authorize", "login", "gone", "goals/x", "api/accounts/me"] {
        assert!(!sh.is_go(&both, other), "{other} stays the accounts host's");
    }
    assert!(!sh.is_go(&host("acme.erp.test"), "go/acme/s/"), "a workspace's own /go/ stays its own");
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
