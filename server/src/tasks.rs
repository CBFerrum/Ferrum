//! CLI tasks: `cargo loco task <name> key:value ...` (or `ferrum task ...` in production).

use crate::state::app_state;
use async_trait::async_trait;
use ferrum_kernel::tenant::NewTenant;
use loco_rs::app::AppContext;
use loco_rs::task::{Task, TaskInfo, Vars};
use loco_rs::{Error, Result};
use std::path::Path;

fn kernel(ctx: &AppContext) -> Result<std::sync::Arc<ferrum_kernel::Kernel>> {
    app_state(ctx).map(|s| s.kernel).map_err(|e| Error::Message(format!("{:?}", e.0)))
}

/// `create_tenant slug:acme name:"Acme Ltd" email:admin@acme.com password:... packages:core,accounting,selling`
pub struct CreateTenant;

#[async_trait]
impl Task for CreateTenant {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "create_tenant".into(), detail: "Create a tenant with an administrator and installed packages".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let packages: Vec<String> = vars
            .cli_arg("packages")
            .unwrap_or("core,accounting,inventory,selling,buying,manufacturing")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let id = k
            .create_tenant(NewTenant {
                slug: vars.cli_arg("slug")?.to_string(),
                name: vars.cli_arg("name")?.to_string(),
                admin_email: vars.cli_arg("email")?.to_string(),
                admin_password: vars.cli_arg("password")?.to_string(),
                admin_name: vars.cli_arg("admin_name").ok().map(str::to_string),
                packages,
                settings: vars.cli_arg("fy_start_month").ok().and_then(|m| m.parse::<u32>().ok()).map(|m| serde_json::json!({"fiscal_year_start_month": m})),
            })
            .await
            .map_err(|e| Error::Message(e.to_string()))?;
        println!("tenant created: {id}");
        Ok(())
    }
}

/// `sync_schema` — force a full metadata → DDL sync (normally automatic at boot).
pub struct SyncSchema;

#[async_trait]
impl Task for SyncSchema {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "sync_schema".into(), detail: "Force the metadata-to-database schema sync".into() }
    }
    async fn run(&self, ctx: &AppContext, _vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let report = ferrum_kernel::ddl::sync(&k.db, &k.registry.catalog, true).await.map_err(|e| Error::Message(e.to_string()))?;
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
        Ok(())
    }
}

/// `import_erpnext src:../upstream/erpnext/erpnext out:packages/erpnext-import package:erpnext-import`
pub struct ImportErpnext;

#[async_trait]
impl Task for ImportErpnext {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "import_erpnext".into(), detail: "Convert Frappe/ERPNext DocType JSON into a Ferrum package".into() }
    }
    async fn run(&self, _ctx: &AppContext, vars: &Vars) -> Result<()> {
        let src = vars.cli_arg("src")?;
        let out = vars.cli_arg("out")?;
        let package = vars.cli_arg("package").unwrap_or("erpnext-import");
        let report = crate::importer::import(Path::new(src), Path::new(out), package).map_err(Error::Message)?;
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
        Ok(())
    }
}

/// `run_maintenance` — anchor audit trails and purge expired events (also runs hourly in-process).
pub struct RunMaintenance;

#[async_trait]
impl Task for RunMaintenance {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "run_maintenance".into(), detail: "Anchor audit trails, purge expired events, extend ledger partitions".into() }
    }
    async fn run(&self, ctx: &AppContext, _vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        k.maintenance().await.map_err(|e| Error::Message(e.to_string()))?;
        ferrum_domain::maintenance(&k).await.map_err(|e| Error::Message(e.to_string()))?;
        println!("maintenance done");
        Ok(())
    }
}

/// `seed_demo tenant:demo` — a small, realistic dataset to explore the system with.
pub struct SeedDemo;

#[async_trait]
impl Task for SeedDemo {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "seed_demo".into(), detail: "Fill a tenant with demo masters and transactions (idempotent)".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let (tenant, _) = k.tenant_by_slug(vars.cli_arg("tenant")?).await.map_err(|e| Error::Message(e.to_string()))?;
        let msg = crate::demo::seed(&k, tenant).await.map_err(|e| Error::Message(format!("{e:?}")))?;
        println!("{msg}");
        Ok(())
    }
}
