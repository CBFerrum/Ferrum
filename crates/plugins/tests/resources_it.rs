//! What a plugin can say when it reaches a resource (`FERRUM_PLUGIN_RESOURCES_TEST_DATABASE_URL`, the
//! database is reset first): a handle, a statement and its parameters. Which workspace and which app
//! are asking are the host's to say, so nothing a plugin writes can make it someone else.
//! The provider here only remembers what reached it; a real one is in CBFerrum/ferrum-app-cloud.

use async_trait::async_trait;
use ferrum_kernel::resources::Resources;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use ferrum_plugins::host::{Host, Scope};
use ferrum_plugins::runtime::HostApi;
use ferrum_plugins::{Loaded, Manifest, PluginHost};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
struct Remembers {
    asked: Mutex<Vec<(Uuid, String, String, String, Vec<Value>)>>,
}

#[async_trait]
impl Resources for Remembers {
    async fn declare(&self, _: &Kernel, _: &Ctx, _: &str, _: &[Value]) -> Result<()> {
        Ok(())
    }

    async fn missing(&self, _: &Kernel, _: Uuid, _: &str, _: &[String]) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn withdraw(&self, _: &Kernel, _: &Ctx, _: &str, _: bool) -> Result<()> {
        Ok(())
    }

    async fn query(&self, _: &Kernel, tenant: Uuid, app: &str, handle: &str, sql: &str, params: &[Value]) -> Result<Value> {
        self.asked.lock().unwrap().push((tenant, app.into(), handle.into(), sql.into(), params.to_vec()));
        Ok(json!([]))
    }

    async fn execute(&self, _: &Kernel, tenant: Uuid, app: &str, handle: &str, sql: &str, params: &[Value]) -> Result<u64> {
        self.asked.lock().unwrap().push((tenant, app.into(), handle.into(), sql.into(), params.to_vec()));
        Ok(3)
    }
}

fn plugin(name: &str) -> Loaded {
    Loaded {
        id: Uuid::now_v7(),
        name: name.into(),
        sha: String::new(),
        manifest: Arc::new(serde_json::from_value::<Manifest>(json!({"name": name})).unwrap()),
        config: Arc::new(json!({})),
        credentials: Arc::new(Map::new()),
        user_id: None,
        fuel: 1,
        memory_mb: 1,
    }
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .expect("workspace");
    Ctx::system(tenant, k.registry.schema(tenant).await.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn who_is_asking_comes_from_the_host() {
    let url = std::env::var("FERRUM_PLUGIN_RESOURCES_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_plugin_resources_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 5, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let provider = Arc::new(Remembers::default());
    let mut hooks = HookRegistry::default();
    hooks.set_resources(provider.clone());
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let (ours, theirs) = (workspace(&k, "ours").await, workspace(&k, "theirs").await);
    let (host, ledger) = (PluginHost::new(false).unwrap(), plugin("ledger"));

    let mut api = Host { host: &host, plugin: &ledger, scope: Scope::Own { k: &k, ctx: ours.clone() }, limit_to_manifest: false };
    assert_eq!(api.call("resource.query", json!({"handle": "books", "sql": "select $1", "params": [1]})).await.unwrap(), json!([]));
    assert_eq!(api.call("resource.execute", json!({"handle": "books", "sql": "delete from a"})).await.unwrap(), json!({"changed": 3}));
    let asked = provider.asked.lock().unwrap().clone();
    assert_eq!(asked.iter().map(|a| (a.0, a.1.as_str(), a.2.as_str())).collect::<Vec<_>>(), [(ours.tenant, "ledger", "books"); 2]);
    assert_eq!(asked[0].4, [json!(1)]);

    // AT-EXT-004: another workspace or another app is nothing a plugin can claim. The attempt stays on record.
    for tried in ["tenant", "tenant_id", "app", "plugin", "consumer", "workspace"] {
        for call in ["resource.query", "resource.execute"] {
            let mut args = json!({"handle": "books", "sql": "select 1"});
            args[tried] = json!(theirs.tenant.to_string());
            let refused = api.call(call, args).await.expect_err("refused");
            assert_eq!(refused["code"], json!("validation_failed"), "{refused}");
            assert_eq!(refused["fields"][0]["field"], json!(tried), "{refused}");
        }
    }
    assert_eq!(provider.asked.lock().unwrap().len(), 2, "none of it reached a provider");
    let mut tx = k.db.tenant_tx(ours.tenant).await.unwrap();
    let kept: Vec<Value> =
        sqlx::query_scalar("select diff from audit_event where entity = 'Plugin' and record_id = $1 and action = 'resource.refused' order by id")
            .bind(ledger.id)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept.len(), 12, "{kept:?}");
    assert_eq!(kept[0], json!({"call": "resource.query", "tried": "tenant"}));
    let mut tx = k.db.tenant_tx(theirs.tenant).await.unwrap();
    let elsewhere: i64 = sqlx::query_scalar("select count(*) from audit_event where entity = 'Plugin'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(elsewhere, 0, "and on the record of whoever did it");

    // The other workspace's run of the same plugin is asked about as that workspace.
    let mut api = Host { host: &host, plugin: &ledger, scope: Scope::Own { k: &k, ctx: theirs.clone() }, limit_to_manifest: false };
    api.call("resource.query", json!({"handle": "books", "sql": "select 1"})).await.unwrap();
    assert_eq!(provider.asked.lock().unwrap().last().unwrap().0, theirs.tenant);

    // A statement in a hook would wait on something that the document's transaction cannot take back.
    let entity = ours.schema.entities.values().next().expect("core brings some").clone();
    let mut tx = k.db.tenant_tx(ours.tenant).await.unwrap();
    let mut hc = HookCtx { kernel: &k, ctx: &ours, tx: &mut tx, entity: &entity };
    let mut api = Host { host: &host, plugin: &ledger, scope: Scope::Hook(&mut hc), limit_to_manifest: false };
    let refused = api.call("resource.execute", json!({"handle": "books", "sql": "delete from a"})).await.expect_err("refused");
    assert!(refused["message"].as_str().unwrap().contains("cannot run in a hook"), "{refused}");
    tx.rollback().await.unwrap();
    assert_eq!(provider.asked.lock().unwrap().len(), 3);
}
