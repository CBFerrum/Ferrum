//! Workflows that cross systems against a real PostgreSQL (`FERRUM_SAGA_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_saga_test`; the database is reset), with an outside
//! system played here, which keeps what was asked of it and by which key: an order that reserves
//! stock, charges, then ships. What happens when each part fails, for good or for a while, when a
//! worker is lost after the other system did the work, when the tries run out, when undoing fails,
//! and when someone asks a workflow to stop.

use async_trait::async_trait;
use ferrum_kernel::saga::{Saga, Step, StepCx};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq)]
enum Fail {
    /// Not now: tried again.
    ForAWhile(u32),
    /// Always, but not for good.
    Always,
    /// It says it cannot be done.
    ForGood,
    /// It did the work, then the answer was lost.
    AfterDoing,
}

/// The outside system: what each workflow had done there, by key (a repeated key is the same act),
/// and how each of its parts fails, by order and step, so the cases below share it.
#[derive(Default)]
struct World {
    acts: Vec<(String, String)>,
    keys: HashMap<String, usize>,
    fail_run: HashMap<String, Fail>,
    fail_undo: HashMap<String, Fail>,
}

type Shared = Arc<Mutex<World>>;

struct Act {
    name: &'static str,
    world: Shared,
}

#[async_trait]
impl Step for Act {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, _k: &Kernel, cx: &StepCx<'_>) -> ferrum_kernel::Result<Value> {
        let order = cx.input["order"].as_str().unwrap_or_default().to_string();
        let mut w = self.world.lock().unwrap();
        let rule = format!("{order}/{}", self.name);
        match w.fail_run.get(&rule).copied() {
            Some(Fail::ForGood) => return Err(Error::Invalid(format!("{} refused", self.name))),
            Some(Fail::Always) => return Err(Error::Unavailable(format!("{} is down", self.name))),
            Some(Fail::ForAWhile(n)) if n > 0 => {
                w.fail_run.insert(rule, Fail::ForAWhile(n - 1));
                return Err(Error::Unavailable(format!("{} is busy", self.name)));
            }
            _ => {}
        }
        // The same key is the same act, done once.
        if !w.keys.contains_key(&cx.key) {
            let n = w.acts.len();
            w.keys.insert(cx.key.clone(), n);
            w.acts.push((order.clone(), format!("{}:{}", self.name, cx.key)));
        }
        if w.fail_run.get(&rule) == Some(&Fail::AfterDoing) {
            w.fail_run.remove(&rule);
            return Err(Error::Unavailable("the answer was lost on the way back".into()));
        }
        let before = cx.output("reserve").cloned().unwrap_or(Value::Null);
        Ok(json!({"did": self.name, "after": before}))
    }

    async fn undo(&self, _k: &Kernel, cx: &StepCx<'_>, done: &Value) -> ferrum_kernel::Result<()> {
        let order = cx.input["order"].as_str().unwrap_or_default().to_string();
        let mut w = self.world.lock().unwrap();
        let rule = format!("{order}/{}", self.name);
        match w.fail_undo.get(&rule).copied() {
            Some(Fail::ForGood) => return Err(Error::Invalid(format!("{} cannot be undone", self.name))),
            Some(Fail::ForAWhile(n)) if n > 0 => {
                w.fail_undo.insert(rule, Fail::ForAWhile(n - 1));
                return Err(Error::Unavailable(format!("undoing {} is busy", self.name)));
            }
            _ => {}
        }
        assert_eq!(done["did"], json!(self.name), "undo is given what the step gave back");
        w.acts.push((order, format!("undo {}", self.name)));
        Ok(())
    }
}

fn acts(w: &Shared, order: &str) -> Vec<String> {
    w.lock().unwrap().acts.iter().filter(|(o, _)| o == order).map(|(_, a)| a.split(':').next().unwrap().to_string()).collect()
}

async fn status(k: &Kernel, ctx: &Ctx, id: Uuid) -> (String, Value, Option<String>) {
    let j = k.job(ctx, id).await.unwrap();
    (j["status"].as_str().unwrap().to_string(), j["progress"].clone(), j["last_error"].as_str().map(str::to_string))
}

/// Runs whatever is due, making a waiting retry due first, until the workflow stops moving.
async fn settle(k: &Kernel, id: Uuid, at_most: usize) {
    for _ in 0..at_most {
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update job set run_at = now() where id = $1 and status = 'queued'").bind(id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        if k.run_jobs_once(1).await.unwrap() == 0 {
            return;
        }
    }
}

async fn start(k: &Kernel, tenant: Uuid, user: Uuid, order: &str) -> Uuid {
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    let id = Saga::start(&mut tx, tenant, Some(user), "order", json!({"order": order}), None).await.unwrap();
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
async fn workflows_undo_what_they_did_when_they_cannot_finish() {
    let url = std::env::var("FERRUM_SAGA_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_saga_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let world: Shared = Arc::default();
    let steps: Vec<Arc<dyn Step>> =
        ["reserve", "charge", "ship"].into_iter().map(|name| Arc::new(Act { name, world: world.clone() }) as Arc<dyn Step>).collect();
    let mut hooks = HookRegistry::default();
    hooks.add_job_handler(Arc::new(Saga::new("order", "Order", steps)));
    let settings = KernelSettings { secret_key: Some(SecretKey::new([4u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "sagaco".into(),
            name: "Saga Co".into(),
            admin_email: "admin@sagaco.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let s = k.login("sagaco", "admin@sagaco.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let set = |order: &str, step: &str, f: Fail| world.lock().unwrap().fail_run.insert(format!("{order}/{step}"), f);
    let set_undo = |order: &str, step: &str, f: Fail| world.lock().unwrap().fail_undo.insert(format!("{order}/{step}"), f);

    // Everything goes: each step once, under a key of its own the same every time.
    let id = start(&k, tenant, s.user, "o1").await;
    settle(&k, id, 3).await;
    let (st, progress, _) = status(&k, &admin, id).await;
    assert_eq!(st, "done");
    assert_eq!(acts(&world, "o1"), ["reserve", "charge", "ship"]);
    assert_eq!(progress["done"][2]["output"]["after"]["did"], json!("reserve"), "a later step reads what an earlier one gave back");
    assert!(world.lock().unwrap().keys.contains_key(&Saga::key(id, "charge")));

    // Shipping cannot be done: what was done is undone, newest first, and it says so.
    set("o2", "ship", Fail::ForGood);
    let id = start(&k, tenant, s.user, "o2").await;
    settle(&k, id, 3).await;
    let (st, progress, error) = status(&k, &admin, id).await;
    assert_eq!(st, "failed");
    assert_eq!(acts(&world, "o2"), ["reserve", "charge", "undo charge", "undo reserve"]);
    assert_eq!(progress["undone"], json!(["charge", "reserve"]));
    let error = error.unwrap();
    assert!(error.contains("ship failed") && error.contains("undone: charge, reserve"), "{error}");

    // Charging is busy for a while: tried again from where it got, reserving not done twice.
    set("o3", "charge", Fail::ForAWhile(2));
    let id = start(&k, tenant, s.user, "o3").await;
    settle(&k, id, 5).await;
    assert_eq!(status(&k, &admin, id).await.0, "done");
    assert_eq!(acts(&world, "o3"), ["reserve", "charge", "ship"]);

    // The charge went through and the answer was lost: tried again under the same key, which the
    // other system takes as the same charge.
    set("o4", "charge", Fail::AfterDoing);
    let id = start(&k, tenant, s.user, "o4").await;
    settle(&k, id, 5).await;
    assert_eq!(status(&k, &admin, id).await.0, "done");
    assert_eq!(acts(&world, "o4"), ["reserve", "charge", "ship"], "one charge");

    // Shipping is down and stays down: past its forward tries, it turns around and undoes.
    set("o5", "ship", Fail::Always);
    let id = start(&k, tenant, s.user, "o5").await;
    settle(&k, id, 30).await;
    let (st, _, error) = status(&k, &admin, id).await;
    assert_eq!(st, "failed", "{error:?}");
    assert_eq!(acts(&world, "o5"), ["reserve", "charge", "undo charge", "undo reserve"]);
    assert_eq!(k.job(&admin, id).await.unwrap()["attempts"], json!(10), "its tenth try turned it around");

    // An undo that is busy is tried again; one that cannot be done stops it there for a person.
    set("o6", "ship", Fail::ForGood);
    set_undo("o6", "charge", Fail::ForAWhile(1));
    set_undo("o6", "reserve", Fail::ForGood);
    let id = start(&k, tenant, s.user, "o6").await;
    settle(&k, id, 5).await;
    let (st, progress, error) = status(&k, &admin, id).await;
    assert_eq!(st, "failed");
    assert_eq!(progress["undone"], json!(["charge"]));
    let error = error.unwrap();
    assert!(error.contains("undoing reserve failed") && error.contains("a person must still undo reserve"), "{error}");

    // Asked to stop between tries: it is run once more, to undo what it did, and ends cancelled.
    set("o7", "charge", Fail::ForAWhile(5));
    let id = start(&k, tenant, s.user, "o7").await;
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
    assert_eq!(status(&k, &admin, id).await.0, "queued", "waiting to charge again");
    k.cancel_job(&admin, id).await.unwrap();
    assert_eq!(status(&k, &admin, id).await.0, "queued", "not dropped: it has something to undo");
    settle(&k, id, 3).await;
    let (st, _, error) = status(&k, &admin, id).await;
    assert_eq!(st, "cancelled", "{error:?}");
    assert_eq!(acts(&world, "o7"), ["reserve", "undo reserve"]);
    assert!(error.unwrap().contains("asked to stop"));

    // Asked to stop before it began: nothing to undo.
    let id = start(&k, tenant, s.user, "o8").await;
    k.cancel_job(&admin, id).await.unwrap();
    settle(&k, id, 2).await;
    let (st, _, error) = status(&k, &admin, id).await;
    assert_eq!(st, "cancelled");
    assert!(acts(&world, "o8").is_empty() && error.unwrap().contains("nothing had been done"));
}
