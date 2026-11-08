//! CLI tasks: `cargo loco task <name> key:value ...` (or `ferrum task ...` in production).

use crate::state::app_state;
use async_trait::async_trait;
use ferrum_kernel::tenant::NewTenant;
use loco_rs::app::AppContext;
use loco_rs::task::{Task, TaskInfo, Vars};
use loco_rs::{Error, Result};
use std::path::Path;

/// A task's refusal as the operator reads it: a validation's reasons, field by field, not only
/// "validation failed".
fn refused<E: std::fmt::Display + 'static>(e: E) -> Error {
    match (&e as &dyn std::any::Any).downcast_ref::<ferrum_kernel::Error>() {
        Some(ferrum_kernel::Error::Validation(fields)) => {
            Error::Message(fields.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "))
        }
        _ => Error::Message(e.to_string()),
    }
}

fn kernel(ctx: &AppContext) -> Result<std::sync::Arc<ferrum_kernel::Kernel>> {
    app_state(ctx).map(|s| s.kernel).map_err(|e| Error::Message(format!("{:?}", e.0)))
}

/// `kms_wrap`: the configured secret keys as ciphertext of the Transit key `FERRUM_KMS_TRANSIT`
/// names, to configure in their place (crates/kernel/src/kms.rs). Keys given as ciphertext already
/// are wrapped again by the Transit key's newest version, after it was rotated. It prints no key.
pub struct KmsWrap;

#[async_trait]
impl Task for KmsWrap {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "kms_wrap".into(),
            detail: "Prints FERRUM_SECRET_KEY and FERRUM_RETIRED_SECRET_KEYS as ciphertext of the key-management service's Transit key".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, _vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let Some(transit) = crate::app::kms_transit(&st.settings)? else {
            return Err(Error::Message("set FERRUM_KMS_TRANSIT to the Transit key that is to wrap them (https://host:8200/<mount>/<key>)".into()));
        };
        let wrap = |value: String| {
            let transit = transit.clone();
            async move {
                let key = ferrum_kernel::kms::resolve(&value, Some(&transit)).await?;
                transit.wrap(&key.to_base64()).await
            }
        };
        let main = st.settings.secret_key.trim().to_string();
        if main.is_empty() {
            return Err(Error::Message("FERRUM_SECRET_KEY is not set: there is nothing to wrap".into()));
        }
        println!("FERRUM_SECRET_KEY={}", wrap(main).await.map_err(refused)?);
        let mut retired = Vec::new();
        for k in st.settings.retired_secret_keys.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            retired.push(wrap(k.to_string()).await.map_err(refused)?);
        }
        if !retired.is_empty() {
            println!("FERRUM_RETIRED_SECRET_KEYS={}", retired.join(","));
        }
        Ok(())
    }
}

/// `account_state slug:acme state:restricted note:"..."`: where a workspace's account stands.
pub struct AccountState;

#[async_trait]
impl Task for AccountState {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "account_state".into(),
            detail: "Moves a workspace between active, grace, restricted and closed (with retain_until:YYYY-MM-DD). Nothing here touches its data".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let slug = vars.cli_arg("slug").map_err(refused)?;
        let state = vars.cli_arg("state").map_err(refused)?;
        let mut tx = k.db.system_tx().await.map_err(refused)?;
        let tenant: Option<uuid::Uuid> =
            sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_optional(&mut *tx).await.map_err(refused)?;
        tx.commit().await.map_err(refused)?;
        let tenant = tenant.ok_or_else(|| Error::Message(format!("no workspace {slug}")))?;
        if state == "closed" {
            let until = vars.cli_arg("retain_until").map_err(|_| Error::Message("a closed workspace is kept: retain_until:YYYY-MM-DD".into()))?;
            let until = chrono::NaiveDate::parse_from_str(until, "%Y-%m-%d").map_err(|_| Error::Message("retain_until is a date: YYYY-MM-DD".into()))?;
            let note = vars.cli_arg("note").map_err(|_| Error::Message("say why: note:\"...\"".into()))?;
            k.close_account(tenant, until, note).await.map_err(refused)?;
            println!("{slug} is closed, and kept until {until}");
            return Ok(());
        }
        k.set_account_state(tenant, state, vars.cli_arg("note").ok()).await.map_err(refused)?;
        println!("{slug} is {state}");
        Ok(())
    }
}

/// `set_plan slug:acme plan:team`: puts a workspace on one of the operator's plans (`FERRUM_PLANS`).
pub struct SetPlan;

#[async_trait]
impl Task for SetPlan {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "set_plan".into(),
            detail: "Puts a workspace on one of FERRUM_PLANS: its quotas become the plan's. Nothing here charges anyone".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let slug = vars.cli_arg("slug").map_err(refused)?;
        let plan = vars.cli_arg("plan").map_err(refused)?;
        let mut tx = k.db.system_tx().await.map_err(refused)?;
        let tenant: Option<uuid::Uuid> =
            sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_optional(&mut *tx).await.map_err(refused)?;
        tx.commit().await.map_err(refused)?;
        let tenant = tenant.ok_or_else(|| Error::Message(format!("no workspace {slug}")))?;
        k.set_plan(tenant, plan).await.map_err(refused)?;
        println!("{slug} is on the {plan} plan");
        Ok(())
    }
}

/// `delete_tenant slug:acme confirm:acme note:"..." [apply:yes]`: deletes a closed workspace whose time to be kept has passed.
pub struct DeleteTenant;

#[async_trait]
impl Task for DeleteTenant {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "delete_tenant".into(), detail: "Deletes a closed workspace after its retention date. Says what would go unless apply:yes".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let arg = |name: &str| vars.cli_arg(name).map_err(refused);
        let (slug, confirm, note) = (arg("slug")?, arg("confirm")?, arg("note")?);
        let apply = vars.cli_arg("apply").is_ok_and(|v| v == "yes");
        // Everything that could stop it is asked first, so nothing goes from storage for a deletion that will not happen.
        let said = st.kernel.delete_tenant(slug, confirm, false, note).await.map_err(refused)?;
        if apply && said["held_outside"].as_array().is_some_and(|a| a.is_empty()) {
            // Through a system transaction: row-level security hides workspaces from the pool itself.
            let mut tx = st.kernel.db.system_tx().await.map_err(refused)?;
            let tenant: uuid::Uuid = sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_one(&mut *tx).await.map_err(refused)?;
            tx.commit().await.map_err(refused)?;
            let gone = st.apps.forget_private(tenant).await.map_err(refused)?;
            let done = st.kernel.delete_tenant(slug, confirm, true, note).await.map_err(refused)?;
            println!("{}", serde_json::to_string_pretty(&done).unwrap_or_default());
            println!("deleted, with {} private app bundles", gone.len());
            return Ok(());
        }
        println!("{}", serde_json::to_string_pretty(&said).unwrap_or_default());
        if apply {
            return Err(Error::Message("not deleted: something is still held outside this database".into()));
        }
        Ok(())
    }
}

/// A system identity, for what an operator does from a shell. Any workspace's will do: what these do is
/// the whole server's, and is on record against the first.
async fn operator(k: &ferrum_kernel::Kernel) -> Result<ferrum_kernel::Ctx> {
    let mut tx = k.db.system_tx().await.map_err(refused)?;
    let tenant: Option<uuid::Uuid> = sqlx::query_scalar("select id from tenant order by created_at limit 1").fetch_optional(&mut *tx).await.map_err(refused)?;
    tx.commit().await.map_err(refused)?;
    let tenant = tenant.ok_or_else(|| Error::Message("this server has no workspaces".into()))?;
    let schema = k.registry.schema(tenant).await.map_err(refused)?;
    Ok(ferrum_kernel::Ctx::system(tenant, schema))
}

/// `release_restore note:"..."`: a person has looked at what came back from a backup.
pub struct ReleaseRestore;

#[async_trait]
impl Task for ReleaseRestore {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "release_restore".into(), detail: "After a restore: says what was checked, and lets work that reaches outside carry on".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let note = vars.cli_arg("note").map_err(refused)?;
        k.release_hold(&operator(&k).await?, note).await.map_err(refused)?;
        println!("released. What was set aside stays set aside: replay what should still happen");
        Ok(())
    }
}

/// `extract_tenant from:"postgres://..." tenant:acme note:"..." [apply:yes]`: takes one workspace back to
/// what a restored copy holds. Without `apply:yes` it only compares.
pub struct ExtractTenant;

#[async_trait]
impl Task for ExtractTenant {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "extract_tenant".into(), detail: "Takes one workspace back to what a restored copy holds. Compares unless apply:yes".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let arg = |name: &str| vars.cli_arg(name).map_err(refused);
        let mut tx = k.db.system_tx().await.map_err(refused)?;
        let tenant: Option<uuid::Uuid> =
            sqlx::query_scalar("select id from tenant where slug = $1").bind(arg("tenant")?).fetch_optional(&mut *tx).await.map_err(refused)?;
        tx.commit().await.map_err(refused)?;
        let tenant = tenant.ok_or_else(|| Error::Message("no such workspace".into()))?;
        let schema = k.registry.schema(tenant).await.map_err(refused)?;
        let apply = vars.cli_arg("apply").is_ok_and(|v| v == "yes");
        let done = k.extract_tenant(&ferrum_kernel::Ctx::system(tenant, schema), arg("from")?, apply, arg("note")?).await.map_err(refused)?;
        println!("{}", serde_json::to_string_pretty(&done).unwrap_or_default());
        if apply {
            println!("done. Every session and API key of this workspace has stopped working, and what was waiting is set aside");
        }
        Ok(())
    }
}

/// `accept_authority note:"..."`: this region, which lost the right to write, may have it again.
pub struct AcceptAuthority;

#[async_trait]
impl Task for AcceptAuthority {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "accept_authority".into(),
            detail: "After a failover: says this region's data is good, so it may take the right to write again".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let note = vars.cli_arg("note").map_err(refused)?;
        k.accept_authority(&operator(&k).await?, note).await.map_err(refused)?;
        println!("accepted. This region writes once the witness gives it the authority");
        Ok(())
    }
}

/// `doctor`: a redacted report on this deployment; `format:json` for a machine. Fails when something
/// must be fixed before it is safe to run.
pub struct Doctor;

#[async_trait]
impl Task for Doctor {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "doctor".into(), detail: "A redacted report on this deployment: what must be fixed, what should be, and how".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let findings = crate::doctor::report(&st, ctx).await.map_err(refused)?;
        if vars.cli_arg("format").is_ok_and(|f| f == "json") {
            println!("{}", serde_json::to_string_pretty(&findings).map_err(refused)?);
        } else {
            print!("{}", crate::doctor::render(&findings));
        }
        let blockers = findings.iter().filter(|f| f.level == ferrum_kernel::doctor::Level::Blocker).count();
        if blockers > 0 { Err(Error::Message(format!("{blockers} blockers"))) } else { Ok(()) }
    }
}

/// `reapply_erasures`: after restoring a database, erases again what was erased since that backup.
pub struct ReapplyErasures;

#[async_trait]
impl Task for ReapplyErasures {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "reapply_erasures".into(), detail: "After a restore: erase again what the erasure journal names".into() }
    }
    async fn run(&self, ctx: &AppContext, _vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let keys = st.objects.list(ferrum_kernel::privacy::JOURNAL_PREFIX).await.map_err(refused)?;
        let mut records = 0;
        for key in &keys {
            records += k.reapply_erasure(key).await.map_err(|e| Error::Message(format!("{key}: {e}")))?;
        }
        println!("{} erasures replayed, {records} records and accounts erased again", keys.len());
        Ok(())
    }
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
            .map_err(refused)?;
        // Shapes B and C: its subdomain at once.
        if let Some(base) = crate::shell::base_domain(&crate::state::Settings::from_ctx(ctx)?) {
            k.add_subdomain(id, &format!("{}.{base}", vars.cli_arg("slug")?)).await.map_err(refused)?;
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
        let report = ferrum_kernel::ddl::sync(&k.db, &k.registry.catalog(), true).await.map_err(refused)?;
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

/// `assistant_eval slug:<workspace> email:<person> scratch:yes [file:<dataset.yaml>]`: the grounded-answer
/// cases (`server/assistant-evals/grounded.yaml` unless `file:` names others) against the configured
/// model, as that person, adding each case's records to that workspace (one set aside for it, as
/// `scratch:yes` says). Prints each outcome with its prompt version; fails when one fails.
pub struct AssistantEval;

#[async_trait]
impl Task for AssistantEval {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "assistant_eval".into(), detail: "Run the assistant's grounded-answer cases against its model in a workspace set aside for it".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let msg = |e: ferrum_kernel::Error| Error::Message(e.to_string());
        let arg = |name: &str| vars.cli_arg(name).map_err(refused);
        if !vars.cli_arg("scratch").is_ok_and(|v| v == "yes") {
            return Err(Error::Message("the cases add records to the workspace: name one set aside for it, and say scratch:yes".into()));
        }
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let (slug, email) = (arg("slug")?, arg("email")?);
        let cases = match vars.cli_arg("file") {
            Ok(path) => std::fs::read_to_string(path).map_err(|e| Error::Message(format!("{path}: {e}")))?,
            Err(_) => include_str!("../assistant-evals/grounded.yaml").to_string(),
        };
        let cases = crate::assistant_eval::parse(&cases).map_err(msg)?;
        let mut tx = st.kernel.db.system_tx().await.map_err(msg)?;
        let person: Option<(uuid::Uuid, uuid::Uuid)> =
            sqlx::query_as("select u.tenant_id, u.id from app_user u join tenant t on t.id = u.tenant_id where t.slug = $1 and u.email = $2 and u.is_active")
                .bind(slug)
                .bind(email)
                .fetch_optional(&mut *tx)
                .await
                .map_err(refused)?;
        tx.commit().await.map_err(refused)?;
        let (tenant, user) = person.ok_or_else(|| Error::Message(format!("no active {email} in {slug}")))?;
        let person = st.kernel.load_ctx(tenant, user, None, "assistant-eval".into()).await.map_err(msg)?;
        let outcomes = crate::assistant_eval::run(&crate::assistant::Env::of(&st), &person, &cases).await.map_err(msg)?;
        println!("{}", serde_json::to_string_pretty(&outcomes).unwrap_or_default());
        let failed = outcomes.iter().filter(|o| !o.passed).count();
        if failed > 0 {
            return Err(Error::Message(format!("{failed} of {} cases failed", outcomes.len())));
        }
        println!("all {} cases passed", outcomes.len());
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
        k.maintenance().await.map_err(refused)?;
        ferrum_app_erp::maintenance(&k).await.map_err(refused)?;
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
            let schema = ferrum_meta::effective::build(&catalog, &all, &[], 0).map_err(refused)?;
            let text = serde_json::to_vec_pretty(&schema.canonical()).map_err(refused)?;
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
        let n = crate::upgrade::back(&k, step).await.map_err(refused)?;
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
        let (tenant, _) = k.tenant_by_slug(vars.cli_arg("tenant")?).await.map_err(refused)?;
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
        let r = apps(ctx)?.make_available_file(bytes.into(), allow_unsigned).await.map_err(refused)?;
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
        let lines = apps(ctx)?.upgrade_from_stores(vars.cli_arg("app").ok()).await.map_err(refused)?;
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
        let n = apps(ctx)?.revoke(app, vars.cli_arg("version").ok(), reason).await.map_err(refused)?;
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
        for l in apps(ctx)?.release_lines(vars.cli_arg("app").ok()).await.map_err(refused)? {
            println!("{l}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_gives_each_fields_reason() {
        assert_eq!(refused(ferrum_kernel::Error::field("retain_until", "a date after today")).to_string(), "retain_until: a date after today");
        assert_eq!(refused(ferrum_kernel::Error::Invalid("no workspace acme".into())).to_string(), "no workspace acme");
    }
}

/// `logic_replay` (built with `process`, with `TEMPORAL_ADDRESS` set): fetches the history of every run that is
/// not finished (or those named, `workflows:id1,id2`) and replays it against this build's process workflow with
/// the workspace's keys; or, with `files:a.json,b.json`, histories saved by the SDK (`fetch_history().to_json()`). Run it with the new build
/// before it takes over open runs: a history that does not replay means that build would read those runs
/// differently, so the build that runs them keeps running them (docs/operations.md, "Upgrading with processes
/// open").
#[cfg(feature = "process")]
pub struct LogicReplay;

#[cfg(feature = "process")]
#[async_trait]
impl Task for LogicReplay {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "logic_replay".into(), detail: "Replay open runs' histories against this build before it takes over".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let Some(current) = k.settings.secret_key.clone() else { return Err(Error::Message("no secret key: runs are sealed with FERRUM_SECRET_KEY".into())) };
        let keys = std::sync::Arc::new(ferrum_process::Keys { current, retired: k.settings.retired_secret_keys.clone() });
        let mut results: Vec<(String, std::result::Result<(), String>)> = Vec::new();
        if let Ok(files) = vars.cli_arg("files") {
            for path in files.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let history = std::fs::read(path).map_err(|e| Error::Message(format!("{path}: {e}")))?;
                results.push((path.to_string(), ferrum_process::replay(history, Some(keys.clone())).await));
            }
        } else {
            let address = std::env::var("TEMPORAL_ADDRESS").map_err(|_| Error::Message("TEMPORAL_ADDRESS is not set".into()))?;
            let settings = ferrum_process::Settings {
                address,
                namespace: std::env::var("TEMPORAL_NAMESPACE").unwrap_or_else(|_| "ferrum".into()),
                queue: std::env::var("TEMPORAL_TASK_QUEUE").unwrap_or_else(|_| ferrum_process::QUEUE.into()),
                keys,
            };
            // Named runs, or every one not finished.
            let named: Vec<String> =
                vars.cli_arg("workflows").map(|w| w.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default();
            if !named.is_empty() {
                results = ferrum_process::replay_runs(&settings, &named).await.map_err(Error::Message)?;
            } else {
                let mut tx = k.db.system_tx().await.map_err(|e| Error::Message(e.to_string()))?;
                let open: Vec<String> = sqlx::query_scalar(
                "select workflow_id from logic_run where status in ('starting', 'running', 'needs_attention', 'cancelling') order by accepted_at limit 100000",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| Error::Message(e.to_string()))?;
                tx.commit().await.map_err(|e| Error::Message(e.to_string()))?;
                results = ferrum_process::replay_runs(&settings, &open).await.map_err(Error::Message)?;
            }
        }
        let failed = results.iter().filter(|(_, r)| r.is_err()).count();
        for (what, r) in &results {
            match r {
                Ok(()) => println!("{what}: replays on this build"),
                Err(e) => println!("{what}: does not replay on this build: {e}"),
            }
        }
        println!("{} replayed, {failed} did not", results.len() - failed);
        if failed > 0 {
            return Err(Error::Message(format!("{failed} histories do not replay on this build: keep the build that runs them")));
        }
        Ok(())
    }
}
