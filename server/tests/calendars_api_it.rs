//! Business calendars through the administration API against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`,
//! reset first): bad hours refused with a field error, a calendar put, read back, listed and deleted, and
//! only by administrators.

mod common;

use axum::response::IntoResponse;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::controllers::admin::{calendars, put_calendar};
use ferrum_server::error::ApiError;
use serde_json::{Value, json};
use std::path::Path;

/// What the API answers with an error: its status and JSON.
async fn answer(e: ApiError) -> (u16, Value) {
    let r = e.into_response();
    let status = r.status().as_u16();
    (status, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap())
}

#[test]
fn administrators_keep_calendars_with_hours() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin: Ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let someone = k.create_user(&admin, "someone@acme.test", "Someone", None, &[], false).await.unwrap();
    let someone = k.load_ctx(tenant, someone, None, "it".into()).await.unwrap();

    let office = json!({"zone": "Asia/Kolkata", "weekends": ["sat", "sun"], "holidays": ["2026-11-10"],
        "hours": {"mon": [["09:00", "13:00"], ["14:00", "18:00"]], "fri": [["09:00", "13:00"]]}});

    // Bad hours: a field error that says what is wrong, and nothing kept.
    for (hours, says) in [
        (json!({"sat": [["09:00", "12:00"]]}), "weekend"),
        (json!({"mon": [["13:00", "09:00"]]}), "not a window"),
        (json!({"mon": [["09:00", "12:00"], ["11:00", "13:00"]]}), "overlap"),
    ] {
        let mut bad = office.clone();
        bad["hours"] = hours;
        let (status, body) = answer(put_calendar(&k, &admin, "office", &bad).await.unwrap_err()).await;
        assert_eq!((status, body["error"].as_str(), body["fields"][0]["field"].as_str()), (422, Some("validation_failed"), Some("hours")), "{body}");
        assert!(body["fields"][0]["message"].as_str().unwrap().contains(says), "{body}");
    }
    let (status, _) = answer(put_calendar(&k, &admin, "office", &json!({"zone": "Asia/Kolkata", "hours": "all day"})).await.unwrap_err()).await;
    assert_eq!(status, 422);
    assert_eq!(calendars(&k, &admin, None).await.unwrap(), json!({"calendars": []}));

    // Not an administrator: refused.
    assert_eq!(answer(put_calendar(&k, &someone, "office", &office).await.unwrap_err()).await.0, 403);
    assert_eq!(answer(calendars(&k, &someone, None).await.unwrap_err()).await.0, 403);
    assert_eq!(answer(k.delete_logic_calendar(&someone, "office").await.unwrap_err().into()).await.0, 403);

    // Put, read back, put again (a new revision), listed.
    assert_eq!(put_calendar(&k, &admin, "office", &office).await.unwrap(), json!({"revision": 1}));
    let read = calendars(&k, &admin, Some("office")).await.unwrap();
    for key in ["zone", "weekends", "holidays", "hours"] {
        assert_eq!(read[key], office[key], "{key}: {read}");
    }
    assert_eq!(put_calendar(&k, &admin, "office", &office).await.unwrap(), json!({"revision": 2}));
    let list = calendars(&k, &admin, None).await.unwrap();
    assert_eq!((list["calendars"].as_array().map(Vec::len), list["calendars"][0]["revision"].as_i64()), (Some(1), Some(2)), "{list}");
    assert_eq!(answer(calendars(&k, &admin, Some("mars")).await.unwrap_err()).await.0, 404);

    // Deleted: gone, and deleting it again is not found.
    k.delete_logic_calendar(&admin, "office").await.unwrap();
    assert_eq!(calendars(&k, &admin, None).await.unwrap(), json!({"calendars": []}));
    assert_eq!(answer(k.delete_logic_calendar(&admin, "office").await.unwrap_err().into()).await.0, 404);
}
