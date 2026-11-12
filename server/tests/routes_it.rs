//! The whole application's router, built as the binary builds it (`AppRoutes::to_router` over every controller `App`
//! mounts), with no database or kernel behind it: no path and method may be mounted twice (axum panics on that at
//! boot), and the connector hub's and the connections' paths are each mounted, once. (A request cannot tell them
//! apart here: the sites' catch-all answers any path while the server has no state.)

use axum::http::Method;
use ferrum_server::app::App;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use std::panic::AssertUnwindSafe;

#[tokio::test]
async fn every_controller_mounts_once_and_is_reached() {
    let env = Environment::Test;
    let config = env.load_from_folder(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config")).expect("config/test.yaml");
    let ctx = AppContext::builder(env, Default::default(), config).build();
    let routes = App::routes(&ctx);
    let listed: Vec<(Method, String)> = routes.collect().iter().flat_map(|r| r.actions.iter().map(|m| (m.clone(), r.uri.clone()))).collect();
    let built = std::panic::catch_unwind(AssertUnwindSafe(|| routes.to_router::<App>(ctx.clone(), axum::Router::new())));
    let _router = built.expect("the routes build into one router: no path and method is mounted twice").expect("the router");
    let id = "{id}";
    for (method, path) in [
        // The connector hub (connect.rs).
        (Method::GET, "/api/admin/connectors".to_string()),
        (Method::GET, "/api/admin/connector-instances".to_string()),
        (Method::POST, "/api/admin/connector-instances".to_string()),
        (Method::PUT, format!("/api/admin/connector-instances/{id}")),
        (Method::DELETE, format!("/api/admin/connector-instances/{id}")),
        (Method::POST, format!("/api/admin/connector-instances/{id}/test")),
        (Method::GET, format!("/api/admin/connector-instances/{id}/schema")),
        (Method::POST, format!("/api/admin/connector-instances/{id}/sync")),
        (Method::POST, format!("/api/admin/connector-instances/{id}/reconcile")),
        (Method::GET, format!("/api/admin/connector-instances/{id}/drift")),
        (Method::GET, format!("/api/admin/connector-instances/{id}/activity")),
        (Method::POST, "/api/admin/connection-drift/{id}/resolve".to_string()),
        // Connections (connections.rs).
        (Method::GET, "/api/admin/connections".to_string()),
        (Method::POST, "/api/admin/connections".to_string()),
        (Method::PUT, format!("/api/admin/connections/{id}")),
        (Method::DELETE, format!("/api/admin/connections/{id}")),
        (Method::POST, format!("/api/admin/connections/{id}/try")),
        (Method::GET, format!("/api/admin/connections/{id}/pulls")),
    ] {
        assert_eq!(listed.iter().filter(|(m, p)| *m == method && *p == path).count(), 1, "{method} {path} mounted once");
    }
    assert!(
        !listed.iter().any(|(_, p)| p.starts_with("/api/admin/connections/{id}/")
            && ["test", "schema", "sync", "reconcile", "drift", "activity"].iter().any(|t| p.ends_with(t))),
        "nothing of the hub under /api/admin/connections"
    );
}
