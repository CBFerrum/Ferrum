//! Who may show a workspace's public page in a frame (docs/builder/decisions.md BD-80), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset by each test, which take their turn): the server as the binary
//! boots it. A public page is framed by its own workspace's live addresses and by the origins of the longest
//! Embedding that is its path or starts it; never by another workspace's addresses or Embeddings; a site page by
//! nobody.

mod common;

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Doc, Kernel};
use ferrum_server::app::App;
use ferrum_server::controllers::shell::{framed_by, public_frames, site_csp};
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

struct World {
    st: AppState,
    k: Arc<Kernel>,
    acme: Ctx,
    globex: Ctx,
    /// `http` or `https`, as the server serves.
    scheme: &'static str,
}

async fn world() -> World {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, url.clone());
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let mut admins = Vec::new();
    for slug in ["acme", "globex"] {
        let t = k
            .create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: PASSWORD.into(),
                admin_name: None,
                packages: vec![],
                settings: None,
            })
            .await
            .unwrap();
        k.add_subdomain(t, &format!("{slug}.ferrum.test")).await.unwrap();
        let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
        admins.push(k.load_ctx(t, s.user, None, "it".into()).await.unwrap());
    }
    let globex = admins.pop().unwrap();
    let acme = admins.pop().unwrap();
    let scheme = if st.shell.secure { "https" } else { "http" };
    World { st: st.clone(), k, acme, globex, scheme }
}

async fn embedding(k: &Kernel, ctx: &Ctx, page: &str, origins: &[&str]) {
    k.insert(ctx, "Embedding", obj(json!({"page": page, "origins": origins.join("\n")}))).await.unwrap();
}

#[test]
#[serial]
fn unlisted_origin_gets_frame_ancestors_without_it() {
    run(|| async {
        let w = world().await;
        embedding(&w.k, &w.acme, "forms/f/contact", &["https://partner.example"]).await;
        let listed = public_frames(&w.st, w.acme.tenant, "forms/f/contact").await.unwrap();
        let other = public_frames(&w.st, w.acme.tenant, "forms/f/survey").await.unwrap();
        assert!(listed.contains(&"https://partner.example".to_string()), "{listed:?}");
        assert!(!other.contains(&"https://partner.example".to_string()), "another form is not the listed one: {other:?}");
        let csp = framed_by("default-src 'self'; frame-ancestors 'self'".into(), &other);
        assert!(!csp.contains("partner.example") && csp.contains("frame-ancestors 'self'"), "{csp}");
        let csp = framed_by("default-src 'self'; frame-ancestors 'self'".into(), &listed);
        assert!(csp.contains("frame-ancestors 'self' ") && csp.contains(" https://partner.example"), "{csp}");
    });
}

#[test]
#[serial]
fn longest_prefix_wins_and_underscores_are_literal() {
    run(|| async {
        let w = world().await;
        embedding(&w.k, &w.acme, "forms/f", &["https://all-forms.example"]).await;
        embedding(&w.k, &w.acme, "forms/f/contact", &["https://contact.example"]).await;
        let frames = |p: &'static str| {
            let (st, t) = (w.st.clone(), w.acme.tenant);
            async move { public_frames(&st, t, p).await.unwrap() }
        };
        let contact = frames("forms/f/contact").await;
        assert!(contact.contains(&"https://contact.example".into()) && !contact.contains(&"https://all-forms.example".into()), "{contact:?}");
        assert!(frames("forms/f/contact/thanks").await.contains(&"https://contact.example".into()));
        assert!(frames("forms/f/survey").await.contains(&"https://all-forms.example".into()));
        // A start only at a `/`: forms/f does not start forms/fx.
        assert!(!frames("forms/fx/a").await.contains(&"https://all-forms.example".into()));
        // A row whose page has an underscore (none can be saved so; written here as a stale row would be) matches only
        // itself: no character is a wildcard.
        let mut tx = w.k.db.tenant_tx(w.acme.tenant).await.unwrap();
        sqlx::query("insert into e_embedding (tenant_id, id, page, origins) values ($1, $2, 'sign_s', 'https://wild.example')")
            .bind(w.acme.tenant)
            .bind(Uuid::now_v7())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(!frames("signxs").await.contains(&"https://wild.example".into()));
        assert!(!frames("sign/s/abc").await.contains(&"https://wild.example".into()));
    });
}

#[test]
#[serial]
fn own_hosts_may_frame() {
    run(|| async {
        let w = world().await;
        let k = &w.k;
        k.add_host(&w.acme, "my.acme.test", "portal").await.unwrap();
        k.host_checked("my.acme.test", true).await.unwrap();
        k.add_host(&w.acme, "pending.acme.test", "shell").await.unwrap();
        let frames = public_frames(&w.st, w.acme.tenant, "bookings/book").await.unwrap();
        let s = w.scheme;
        assert!(frames.contains(&format!("{s}://acme.ferrum.test")) && frames.contains(&format!("{s}://my.acme.test")), "{frames:?}");
        // Not one that is not proven, or another workspace's address.
        for not in ["pending.acme.test", "globex.ferrum.test"] {
            assert!(!frames.iter().any(|f| f.ends_with(not)), "{not}: {frames:?}");
        }
    });
}

#[test]
#[serial]
fn another_workspaces_embeddings_and_hosts_never_frame_this_one() {
    run(|| async {
        let w = world().await;
        embedding(&w.k, &w.globex, "forms/f", &["https://globex-partner.example"]).await;
        let frames = public_frames(&w.st, w.acme.tenant, "forms/f/contact").await.unwrap();
        assert!(frames.iter().all(|f| !f.contains("globex")), "{frames:?}");
        let theirs = public_frames(&w.st, w.globex.tenant, "forms/f/contact").await.unwrap();
        assert!(theirs.contains(&"https://globex-partner.example".into()) && theirs.iter().all(|f| !f.contains("acme")), "{theirs:?}");
    });
}

#[test]
#[serial]
fn site_pages_never_frameable() {
    let own = vec!["https://acme.example".to_string(), "https://shop.acme.example".to_string()];
    let csp = site_csp(&own);
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(csp.contains("frame-src 'self' https://acme.example https://shop.acme.example;"), "may show its own forms: {csp}");
    assert!(!csp.contains("frame-ancestors 'self'"), "{csp}");
    assert_eq!(site_csp(&[]).matches("frame-ancestors 'none'").count(), 1);
}
