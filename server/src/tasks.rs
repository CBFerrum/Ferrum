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
        let packages: Vec<String> = vars.cli_arg("packages").unwrap_or("core").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
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
        // Shapes B and C: its subdomain at once.
        if let Some(base) = crate::shell::base_domain(&crate::state::Settings::from_ctx(ctx)?) {
            k.add_subdomain(id, &format!("{}.{base}", vars.cli_arg("slug")?)).await.map_err(|e| Error::Message(e.to_string()))?;
        }
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
        let report = ferrum_kernel::ddl::sync(&k.db, &k.registry.catalog(), true).await.map_err(|e| Error::Message(e.to_string()))?;
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
        Ok(())
    }
}

/// `assistant_check`: one short message to the assistant's model with the configured key
/// (`FERRUM_ASSISTANT_*`), to see that it is set up; it costs a few tokens.
pub struct AssistantCheck;

#[async_trait]
impl Task for AssistantCheck {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "assistant_check".into(), detail: "Send the assistant's model one short message: are its key, model and streaming right?".into() }
    }
    async fn run(&self, _ctx: &AppContext, _vars: &Vars) -> Result<()> {
        let Some(c) = crate::assistant::Config::from_env() else {
            return Err(Error::Message("FERRUM_ASSISTANT_API_KEY is not set: the assistant is off".into()));
        };
        match crate::assistant::Assistant::new(Some(c.clone())).check().await {
            Ok((model, said)) => {
                println!(
                    "ok: {model} answered {said:?} (configured model {}, effort {}, fallbacks {}, {} calls a person an hour)",
                    c.model, c.effort, c.fallbacks, c.calls_per_hour
                );
                Ok(())
            }
            Err(e) => Err(Error::Message(format!("the assistant's model cannot be used: {e}"))),
        }
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

/// `deps_report [schema:<file>]` — what ties packages together (docs/plan/apps-ui.md §13): every
/// entity a package names that another defines, and the document hooks that run on another
/// package's entities or in every workspace. `schema:<file>` also writes what the schema gives a
/// workspace with everything installed, without who defines it: the same file before and after
/// moving entities or fields between packages means no workspace sees or stores anything else.
pub struct DepsReport;

#[async_trait]
impl Task for DepsReport {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "deps_report".into(), detail: "Show what ties packages together (schema:<file> writes the canonical schema)".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let catalog = k.registry.catalog();
        let refs = catalog.references();
        let mut pairs: std::collections::BTreeMap<(&str, &str), Vec<&ferrum_meta::catalog::Reference>> = Default::default();
        for r in &refs {
            pairs.entry((r.from.as_str(), r.to.as_str())).or_default().push(r);
        }
        for ((from, to), rs) in &pairs {
            println!("{from} → {to} ({})", rs.len());
            for r in rs {
                println!("  {:<13} {}", r.kind, r.what);
            }
        }
        println!("\nDocument hooks on another package's entities, or in every workspace:");
        for (entity, scope) in k.hooks.scopes() {
            let owner = catalog.entity_package(&entity).unwrap_or("*");
            match scope {
                None => println!("  {entity} ({owner}): every workspace"),
                Some(ps) if !ps.iter().any(|p| p == owner) => println!("  {entity} ({owner}): {}", ps.join(", ")),
                Some(_) => {}
            }
        }
        if let Ok(path) = vars.cli_arg("schema") {
            let all: Vec<String> = catalog.packages().map(|p| p.name().to_string()).collect();
            let schema = ferrum_meta::effective::build(&catalog, &all, &[], 0).map_err(|e| Error::Message(e.to_string()))?;
            let text = serde_json::to_vec_pretty(&schema.canonical()).map_err(|e| Error::Message(e.to_string()))?;
            std::fs::write(path, text).map_err(|e| Error::Message(format!("{path}: {e}")))?;
            println!("\nThe canonical schema is in {path}");
        }
        Ok(())
    }
}

/// `upgrade_back step:core-split` — before going back to the release before a one-time upgrade
/// step: takes out the packages it installed in every workspace (the data stays).
pub struct UpgradeBack;

#[async_trait]
impl Task for UpgradeBack {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "upgrade_back".into(), detail: "Undo a one-time upgrade step before going back to the release before it".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let step = vars.cli_arg("step")?;
        let n = crate::upgrade::back(&k, step).await.map_err(|e| Error::Message(e.to_string()))?;
        println!("{step}: undone in {n} workspaces; stop every replica of this release before it starts again, or it runs the step again");
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

fn apps(ctx: &AppContext) -> Result<std::sync::Arc<crate::appmgr::AppManager>> {
    app_state(ctx).map(|s| s.apps).map_err(|e| Error::Message(format!("{:?}", e.0)))
}

/// `app_make_available file:ev-charging-1.4.0.fapp [allow_unsigned:true]` — make a bundle available to every workspace.
pub struct AppMakeAvailable;

#[async_trait]
impl Task for AppMakeAvailable {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "app_make_available".into(), detail: "Make an app bundle (.fapp) available on the cluster, verified against apps.trusted_keys".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let file = vars.cli_arg("file")?;
        let size = std::fs::metadata(file).map_err(|e| Error::Message(format!("{file}: {e}")))?.len();
        if size > crate::appmgr::MAX_BUNDLE as u64 {
            return Err(Error::Message(format!("{file}: app bundles are limited to 50 MB")));
        }
        let bytes = std::fs::read(file).map_err(|e| Error::Message(format!("{file}: {e}")))?;
        let allow_unsigned = vars.cli_arg("allow_unsigned").is_ok_and(|v| v == "true");
        let r = apps(ctx)?.make_available_file(bytes.into(), allow_unsigned).await.map_err(|e| Error::Message(e.to_string()))?;
        println!("{} {} available on the cluster (digest {})", r.app_id, r.version, r.digest);
        Ok(())
    }
}

/// `app_upgrade [app:<id>]` — make the newest compatible store releases of the cluster's store apps (or of one app) available.
pub struct AppUpgrade;

#[async_trait]
impl Task for AppUpgrade {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "app_upgrade".into(), detail: "Make the newest compatible store releases available on the cluster".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let lines = apps(ctx)?.upgrade_from_stores(vars.cli_arg("app").ok()).await.map_err(|e| Error::Message(e.to_string()))?;
        for l in lines {
            println!("{l}");
        }
        Ok(())
    }
}

/// `app_revoke app:<id> [version:<v>] reason:"..."` — take a release (or every release) out of service and disable it everywhere.
pub struct AppRevoke;

#[async_trait]
impl Task for AppRevoke {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "app_revoke".into(), detail: "Revoke an app release on the cluster and disable it in every workspace (data kept)".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let app = vars.cli_arg("app")?;
        let reason = vars.cli_arg("reason")?;
        let n = apps(ctx)?.revoke(app, vars.cli_arg("version").ok(), reason).await.map_err(|e| Error::Message(e.to_string()))?;
        println!("{n} release(s) of {app} revoked");
        Ok(())
    }
}

/// `app_releases [app:<id>]` — every release recorded on the cluster.
pub struct AppReleases;

#[async_trait]
impl Task for AppReleases {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "app_releases".into(), detail: "List the app releases recorded on the cluster".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        for l in apps(ctx)?.release_lines(vars.cli_arg("app").ok()).await.map_err(|e| Error::Message(e.to_string()))? {
            println!("{l}");
        }
        Ok(())
    }
}
