//! `/api/ready` (controllers/system.rs), on the server set up as the binary sets it up, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::json;
use tower::ServiceExt;

/// A replica told to stop (`SIGTERM`) says it is not ready at once, so a load balancer with no `preStop` pause stops
/// sending to it; until then it is ready.
#[test]
fn not_ready_while_stopping() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let pool = sqlx::PgPool::connect(&url).await.expect("database");
    common::reset(&pool).await;
    pool.close().await;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    config.database.uri = url;
    config.settings.as_mut().unwrap()["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    let ctx = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("set-up");
    let routes = <App as Hooks>::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    let app = <App as Hooks>::after_routes(routes, &ctx).await.expect("layers");
    let st = ctx.shared_store.get::<AppState>().expect("state");
    let ready = || async {
        let req = Request::builder().uri("/api/ready").header(header::HOST, "localhost").body(Body::empty()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        (res.status(), res.headers().contains_key(header::RETRY_AFTER))
    };
    // The object store's first write and read happen in the background: ready within a few seconds.
    let mut said = ready().await;
    for _ in 0..50 {
        if said.0 == StatusCode::OK {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        said = ready().await;
    }
    assert_eq!(said.0, StatusCode::OK, "ready while running");
    // The loops a server runs hold receivers; a watch with none keeps its old value.
    let _loop = st.shutdown.subscribe();
    let _ = st.shutdown.send(true);
    assert_eq!(ready().await, (StatusCode::SERVICE_UNAVAILABLE, true), "not ready once told to stop, and asked again later");
    let handles: Vec<_> = st.background.lock().await.drain(..).collect();
    futures_util::future::join_all(handles).await;
    st.kernel.db.close().await;
}
