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
        let mut findings = crate::doctor::report(&st, ctx).await.map_err(refused)?;
        // `public:https://a,https://b`: the public addresses, checked through their gateway (zones).
        let public: Vec<String> = vars.cli_arg("public").map(|p| p.split(',').map(str::to_string).collect()).unwrap_or_default();
        findings.extend(crate::doctor::zone_findings(&public).await);
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
        TaskInfo {
            name: "reapply_erasures".into(),
            detail: "After a restore: erase again what the erasure journal names, and apply again who left or came back".into(),
        }
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
        // Who left or came back since the backup (FEO-014).
        let access = st.objects.list(ferrum_kernel::lifecycle::ACCESS_JOURNAL_PREFIX).await.map_err(refused)?;
        let changed = k.reapply_access(&access).await.map_err(|e| Error::Message(format!("access journal: {e}")))?;
        println!("{} changes of access replayed, {changed} people changed back to how they stand", access.len());
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

/// `migrate` — everything a server start does to the database, whatever `FERRUM_MIGRATE_AT_START` says: the kernel's
/// SQL migrations, Loco's, the schema sync for the packages of this build, and the one-time upgrade steps. It prints what
/// it did and fails when any step fails. Several at once are safe: each step holds its advisory lock.
pub struct Migrate;

#[async_trait]
impl Task for Migrate {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "migrate".into(),
            detail: "Migrate the database, sync the schema and run the upgrade steps; for a server that starts with FERRUM_MIGRATE_AT_START=false".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, _vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        // A newer build migrated the database (crates/kernel/src/window.rs): there is nothing for this one to do, and a
        // rollback that runs this task with the previous image goes on.
        if let Some(newer) = k.booted.guest {
            println!("nothing to do: a newer build (of {}) has migrated this database, and this build runs on it as it is", newer.build);
            return Ok(());
        }
        // The kernel did the first two as this task started.
        match &k.booted.migrations[..] {
            [] => println!("migrations: none to apply"),
            applied => println!("migrations: applied {}", applied.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")),
        }
        let loco = <migration::Migrator as migration::MigratorTrait>::up(&ctx.db, None).await;
        loco.map_err(|e| Error::Message(format!("Loco migrations: {e}")))?;
        let schema = &k.booted.schema;
        if schema.skipped {
            println!("schema: already current");
        } else {
            println!("schema: synced ({} statements, {} warnings)", schema.statements, schema.warnings.len());
        }
        // Held as a server's start holds it; None when a newer build has migrated since this task started.
        let Some(hold) = k.hold_start().await.map_err(refused)? else {
            println!("upgrade steps: left to the newer build that has migrated this database meanwhile");
            return Ok(());
        };
        let steps = crate::upgrade::run(&k).await.map_err(|e| Error::Message(format!("upgrade steps: {e}")));
        hold.release().await;
        let steps = steps?;
        for (step, n) in steps.iter().filter(|(_, n)| *n > 0) {
            println!("upgrade {step}: {n} workspaces");
        }
        println!("upgrades: {} steps checked, all done", steps.len());
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

/// A task's `zone:` (crate::zone): one of `FERRUM_ZONES`, or `none`.
fn zone_arg(st: &crate::state::AppState, vars: &Vars) -> Result<Option<String>> {
    match vars.cli_arg("zone").map_err(|_| Error::Message("say zone:<a zone of FERRUM_ZONES> or zone:none".into()))? {
        "none" => Ok(None),
        z if st.zones.has(z) => Ok(Some(z.to_string())),
        z => Err(Error::Message(format!("FERRUM_ZONES names no zone {z:?}: say one of them, or zone:none"))),
    }
}

/// `add_host tenant:<slug> host:<name> target:<shell|portal|site:app> zone:<zone|none>`: a host the
/// operator vouches for, served as verified and never checked (docs/operations.md, "Zones").
pub struct AddHost;

#[async_trait]
impl Task for AddHost {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "add_host".into(), detail: "Add a host only your own network resolves, served as verified, in a zone".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let zone = zone_arg(&st, vars)?;
        let host = vars.cli_arg("host")?;
        st.kernel.add_operator_host(vars.cli_arg("tenant")?, host, vars.cli_arg("target")?, zone.as_deref()).await.map_err(refused)?;
        println!("{host} added, in zone {}", zone.as_deref().unwrap_or("none"));
        Ok(())
    }
}

/// `set_host_zone host:<name> zone:<zone|none>`: the zone a host's requests are in.
pub struct SetHostZone;

#[async_trait]
impl Task for SetHostZone {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "set_host_zone".into(), detail: "Put a workspace's host in a zone, or in none".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let zone = zone_arg(&st, vars)?;
        let host = vars.cli_arg("host")?;
        st.kernel.set_host_zone(host, zone.as_deref()).await.map_err(refused)?;
        println!("{host} is in zone {} (other replicas see it within 20 seconds)", zone.as_deref().unwrap_or("none"));
        Ok(())
    }
}

/// `grant_admin tenant:<slug> email:<address> reason:<text>` and `revoke_admin` (same arguments):
/// the operator's way back in when a workspace has lost its administrators, audited as the system's.
pub struct OperatorAdmin(pub bool);

#[async_trait]
impl Task for OperatorAdmin {
    fn task(&self) -> TaskInfo {
        if self.0 {
            TaskInfo { name: "grant_admin".into(), detail: "Make a member of a workspace an administrator, with a reason for the audit trail".into() }
        } else {
            TaskInfo { name: "revoke_admin".into(), detail: "Make an administrator of a workspace no longer one (never the last), with a reason".into() }
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let reason = vars.cli_arg("reason").map_err(|_| Error::Message("say why: reason:\"…\" (it goes in the workspace's audit trail)".into()))?;
        let email = vars.cli_arg("email")?;
        k.operator_admin(vars.cli_arg("tenant")?, email, self.0, reason).await.map_err(refused)?;
        println!("{email} {} an administrator", if self.0 { "is now" } else { "is no longer" });
        Ok(())
    }
}

/// `clear_staff_networks tenant:<slug> reason:<text>`: the operator takes a workspace's staff networks
/// away when its administrators are shut out (docs/operations.md, "Staff networks"), audited as the system's.
pub struct ClearStaffNetworks;

#[async_trait]
impl Task for ClearStaffNetworks {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "clear_staff_networks".into(), detail: "Take a workspace's staff networks away, with a reason for the audit trail".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let reason = vars.cli_arg("reason").map_err(|_| Error::Message("say why: reason:\"…\" (it goes in the workspace's audit trail)".into()))?;
        let tenant = vars.cli_arg("tenant")?;
        k.clear_staff_networks(tenant, reason).await.map_err(refused)?;
        println!("{tenant}'s staff may sign in from anywhere again; every replica knows at once");
        Ok(())
    }
}

/// `plan_move reason:"..." [valid:<hours, 24>] [wait:<seconds, 130>]`: before copying the whole installation elsewhere,
/// stops what reaches outside, waits for the work in hand, and fences the database (crates/kernel/src/moves.rs).
pub struct PlanMove;

#[async_trait]
impl Task for PlanMove {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "plan_move".into(),
            detail: "Before moving the installation: stops outward work, waits for what is in hand, and fences the database so a copy carries on without the restore hold".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let reason = vars.cli_arg("reason").map_err(|_| Error::Message("say why: reason:\"...\"".into()))?;
        let number = |name: &str, default: u64| -> Result<u64> {
            vars.cli_arg(name).map_or(Ok(default), |v| v.trim().parse().map_err(|_| Error::Message(format!("{name} is a whole number"))))
        };
        let (valid, wait) = (number("valid", 24)?, number("wait", 130)?);
        // A region that does not hold the right to write is not the one to move from.
        if let Ok(witness) = std::env::var("FERRUM_AUTHORITY_WITNESS_URL")
            && !witness.trim().is_empty()
        {
            let region = std::env::var("FERRUM_REGION").unwrap_or_default();
            let authority =
                ferrum_kernel::recovery::Authority::connect(&witness, "ferrum", &region, std::time::Duration::from_secs(30)).await.map_err(refused)?;
            let holder = authority.holder().await.map_err(refused)?;
            if holder.as_deref() != Some(region.as_str()) {
                return Err(Error::Message(format!(
                    "this region ({region}) does not hold the right to write ({}): plan the move where the witness says writes happen",
                    holder.map_or("nobody does".to_string(), |h| format!("{h} does"))
                )));
            }
        }
        let m = k.plan_move(reason, std::time::Duration::from_secs(valid * 3600), std::time::Duration::from_secs(wait)).await.map_err(refused)?;
        println!("move {} planned: this database takes no changes from now on, and nothing here reaches outside.", m.id);
        println!("Copy it now (dump and restore, a physical copy, or a backup taken from now on), with its files and keys.");
        println!(
            "A copy that starts before {} carries on without the restore hold, once. Start ONE copy: every copy that starts in time goes on with what was waiting, and would send it again.",
            m.valid_until
        );
        println!("Every replica must run this build: one from before this release does not honour the fence, and would keep writing to this database.");
        println!("Changed your mind: ferrum task cancel_move reason:\"...\" here.");
        Ok(())
    }
}

/// `cancel_move reason:"..."`: the move does not happen; this database takes changes again.
pub struct CancelMove;

#[async_trait]
impl Task for CancelMove {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "cancel_move".into(), detail: "Lifts a planned move's fence: this database takes changes and sends again".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let reason = vars.cli_arg("reason").map_err(|_| Error::Message("say why: reason:\"...\"".into()))?;
        let m = k.cancel_move(reason).await.map_err(refused)?;
        println!("move {} cancelled: this database takes changes again.", m.id);
        println!("A copy made while it was fenced would still carry on as moved until {}: do not start one.", m.valid_until);
        Ok(())
    }
}

/// `allow_older_builds reason:"..." [feature:<name>] [force:yes]`: removes the floors that keep builds which would not
/// enforce a protection from starting (docs/operations.md, "Protections an older build would not enforce").
pub struct AllowOlderBuilds;

#[async_trait]
impl Task for AllowOlderBuilds {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "allow_older_builds".into(),
            detail: "Lets builds that do not enforce protections in use here start again (all, or one feature's), saying which, with a reason".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let reason = vars.cli_arg("reason").map_err(|_| Error::Message("say why: reason:\"...\" (it goes in the audit trail)".into()))?;
        let feature = vars.cli_arg("feature").ok().map(|f| f.trim()).filter(|f| !f.is_empty());
        let force = vars.cli_arg("force").is_ok_and(|v| v == "yes");
        let done = k.allow_older_builds(reason, feature, force).await.map_err(refused)?;
        println!("Builds from before these dates may start on this database again, and they will NOT enforce:");
        for line in ferrum_kernel::window::said(&done.removed) {
            println!("  {line}");
        }
        match done.compatible_from {
            Some(from) => println!("The oldest build that may start now is of {}.", from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            None => println!("No build has recorded itself here yet."),
        }
        println!("This lasts until the next start of a build that knows floors: it puts them back for whatever is still in use. Roll back now.");
        Ok(())
    }
}

/// `adopt_move reason:"..."`: on a physical copy (another timeline of the same database), says it is the copy.
pub struct AdoptMove;

#[async_trait]
impl Task for AdoptMove {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "adopt_move".into(),
            detail: "On a physical copy or a recovered backup of a database fenced for a move: says this is the copy, which then carries on".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let reason = vars.cli_arg("reason").map_err(|_| Error::Message("say how this copy was made: reason:\"...\"".into()))?;
        let m = k.adopt_move(reason).await.map_err(refused)?;
        println!("move {} adopted: this copy takes changes and sends what was waiting.", m.id);
        Ok(())
    }
}

/// `check_files [tenant:<slug>]`: every file the database names, looked for in storage (the doctor looks at the newest).
pub struct CheckFiles;

#[async_trait]
impl Task for CheckFiles {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "check_files".into(), detail: "Looks for every file the database names (and app bundles) in storage, and lists those missing".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let tenant = match vars.cli_arg("tenant") {
            Ok(slug) => {
                let mut tx = k.db.system_tx().await.map_err(refused)?;
                let id: Option<uuid::Uuid> =
                    sqlx::query_scalar("select id from tenant where slug = $1").bind(slug).fetch_optional(&mut *tx).await.map_err(refused)?;
                tx.commit().await.map_err(refused)?;
                Some(id.ok_or_else(|| Error::Message(format!("no workspace {slug}")))?)
            }
            Err(_) => None,
        };
        let mut missing = 0u64;
        let checked = k
            .check_files(tenant, ferrum_kernel::recovery::Files::All, |f| {
                missing += 1;
                match f.tenant {
                    Some(t) => println!("missing: workspace {t}, file {} ({})", f.id, f.name),
                    None => println!("missing: {} ({})", f.name, f.id),
                }
            })
            .await
            .map_err(refused)?;
        println!("{checked} checked, {missing} missing");
        if missing > 0 { Err(Error::Message(format!("{missing} files are not in storage"))) } else { Ok(()) }
    }
}

/// What `move_workspace` is told about the source, from the environment (never the command line, never printed):
/// `FERRUM_MOVE_SOURCE_DATABASE_URL`, `FERRUM_MOVE_SOURCE_SECRET_KEYS` (its `FERRUM_SECRET_KEY`, then its retired keys,
/// comma-separated), its files (`FERRUM_MOVE_SOURCE_FILES=same`, or `FERRUM_MOVE_SOURCE_FILES_DIR`, or
/// `FERRUM_MOVE_SOURCE_S3_BUCKET` with `FERRUM_MOVE_SOURCE_S3_REGION` and `FERRUM_MOVE_SOURCE_S3_ENDPOINT`), and
/// `FERRUM_MOVE_SOURCE_REGION`.
async fn move_source(st: &crate::state::AppState, vars: &Vars) -> Result<ferrum_kernel::workspace_move::MoveSource> {
    let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let url = env("FERRUM_MOVE_SOURCE_DATABASE_URL").ok_or_else(|| Error::Message("set FERRUM_MOVE_SOURCE_DATABASE_URL to the source's database".into()))?;
    let transit = crate::app::kms_transit(&st.settings)?;
    let mut keys = Vec::new();
    for k in env("FERRUM_MOVE_SOURCE_SECRET_KEYS").unwrap_or_default().split(',').map(str::trim).filter(|k| !k.is_empty()) {
        keys.push(ferrum_kernel::kms::resolve(k, transit.as_ref()).await.map_err(|e| Error::Message(format!("FERRUM_MOVE_SOURCE_SECRET_KEYS: {e}")))?);
    }
    let files = match (env("FERRUM_MOVE_SOURCE_FILES").as_deref(), env("FERRUM_MOVE_SOURCE_FILES_DIR"), env("FERRUM_MOVE_SOURCE_S3_BUCKET")) {
        (Some("same"), None, None) => None,
        (None, dir, bucket) if dir.is_some() != bucket.is_some() => {
            let mut s = st.settings.clone();
            s.files_dir = dir.unwrap_or_default();
            s.s3_bucket = bucket.unwrap_or_default();
            s.s3_region = env("FERRUM_MOVE_SOURCE_S3_REGION").unwrap_or_default();
            s.s3_endpoint = env("FERRUM_MOVE_SOURCE_S3_ENDPOINT").unwrap_or_default();
            s.require_object_storage = false;
            let storage = std::sync::Arc::new(crate::app::build_storage(&s)?);
            let objects = std::sync::Arc::new(crate::objects::Objects::new(&s).map_err(Error::Message)?);
            Some(std::sync::Arc::new(crate::mailer::StorageFiles(storage, objects)) as std::sync::Arc<dyn ferrum_kernel::files::FileStore>)
        }
        _ => {
            return Err(Error::Message(
                "say where the source keeps its files: FERRUM_MOVE_SOURCE_FILES=same when both installations use one storage, else \
                 FERRUM_MOVE_SOURCE_FILES_DIR or FERRUM_MOVE_SOURCE_S3_BUCKET (one of them)"
                    .into(),
            ));
        }
    };
    // People keep their Ferrum account only between regions that trust each other.
    let regions = match env("FERRUM_MOVE_SOURCE_REGION") {
        Some(r) if !st.shell.region.is_empty() && st.shell.regions.iter().any(|(name, _)| *name == r) => Some((r, st.shell.region.clone())),
        Some(r) => {
            return Err(Error::Message(format!("FERRUM_MOVE_SOURCE_REGION is {r}, which FERRUM_REGIONS here does not list (or FERRUM_REGION is not set)")));
        }
        None => None,
    };
    let wait = vars.cli_arg("wait").map_or(Ok(130), |v| v.trim().parse::<u64>().map_err(|_| Error::Message("wait is a whole number of seconds".into())))?;
    Ok(ferrum_kernel::workspace_move::MoveSource {
        url,
        keys,
        files,
        regions,
        base_domain: crate::shell::base_domain(&st.settings),
        not_sealed: vars.cli_arg("not_sealed").map(|v| v.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()).unwrap_or_default(),
        wait: std::time::Duration::from_secs(wait),
        stop_after: None,
    })
}

/// `move_workspace tenant:<slug> [resume:yes] [abort:yes] [status:yes] [wait:<seconds>] [not_sealed:<table.column,...>]`, run where the workspace is going: moves one
/// workspace here from another installation (docs/operations.md, "Moving one workspace").
pub struct MoveWorkspace;

#[async_trait]
impl Task for MoveWorkspace {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "move_workspace".into(),
            detail: "Run where the workspace goes: moves it here from the installation FERRUM_MOVE_SOURCE_DATABASE_URL names. resume carries on, abort undoes"
                .into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let slug = vars.cli_arg("tenant").map_err(|_| Error::Message("which workspace: tenant:<slug>".into()))?;
        // Every argument is `key:value` here: resume:yes, abort:yes.
        let flag = |name: &str| vars.cli_arg(name).is_ok_and(|v| v == "yes");
        if flag("status") {
            let k = kernel(ctx)?;
            let st = k.move_status(slug).await.map_err(refused)?;
            println!("{}", serde_json::to_string_pretty(&st).unwrap_or_default());
            return Ok(());
        }
        let run = match (flag("resume"), flag("abort")) {
            (false, false) => ferrum_kernel::workspace_move::Run::Start,
            (true, false) => ferrum_kernel::workspace_move::Run::Resume,
            (false, true) => ferrum_kernel::workspace_move::Run::Abort,
            (true, true) => return Err(Error::Message("resume or abort, not both".into())),
        };
        let from = move_source(&st, vars).await?;
        let say = |line: &str| println!("{line}");
        st.kernel.move_workspace(slug, &from, run, &say).await.map_err(refused)?;
        Ok(())
    }
}

/// `copy_workspace tenant:<slug> as:<new slug> until:<date> [from:here] [resume:yes] [not_sealed:<table.column,...>]`, run
/// where the sandbox is to be: a copy of a live workspace under a new id and name that reaches nothing outside
/// (docs/operations.md, "A sandbox of a workspace"). Without `from:here` the source is another installation, given as
/// for `move_workspace` (FERRUM_MOVE_SOURCE_*).
pub struct CopyWorkspace;

#[async_trait]
impl Task for CopyWorkspace {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "copy_workspace".into(),
            detail: "Makes a sandbox of a workspace here: a copy under a new name, until a date, that reaches nothing outside. from:here copies one of this installation's".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let st = app_state(ctx).map_err(|e| Error::Message(format!("{:?}", e.0)))?;
        let slug = vars.cli_arg("tenant").map_err(|_| Error::Message("which workspace to copy: tenant:<slug>".into()))?;
        let new_slug = vars.cli_arg("as").map_err(|_| Error::Message("the sandbox's own name: as:<slug>".into()))?;
        let until = vars
            .cli_arg("until")
            .ok()
            .and_then(|d| chrono::NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok())
            .ok_or_else(|| Error::Message("the sandbox's last day: until:<YYYY-MM-DD>, at most 90 days ahead".into()))?;
        let here = vars.cli_arg("from").is_ok_and(|v| v == "here");
        let from = if here { None } else { Some(move_source(&st, vars).await?) };
        let copy = ferrum_kernel::sandbox::Copy {
            from,
            until,
            base_domain: crate::shell::base_domain(&st.settings),
            not_sealed: vars.cli_arg("not_sealed").map(|v| v.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()).unwrap_or_default(),
            resume: vars.cli_arg("resume").is_ok_and(|v| v == "yes"),
            halt: None,
        };
        let say = |line: &str| println!("{line}");
        st.kernel.copy_workspace(slug, new_slug, &copy, &say).await.map_err(refused)?;
        Ok(())
    }
}

/// `delete_sandbox tenant:<slug>`: deletes a sandbox, its rows and its files; refuses a live workspace.
pub struct DeleteSandbox;

#[async_trait]
impl Task for DeleteSandbox {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "delete_sandbox".into(), detail: "Deletes a sandbox (its rows and files). Refuses a live workspace".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let slug = vars.cli_arg("tenant").map_err(|_| Error::Message("which sandbox: tenant:<slug>".into()))?;
        let done = k.delete_sandbox(slug).await.map_err(refused)?;
        println!("{slug}: deleted, {} rows and {} files", done["rows"], done["files"]);
        Ok(())
    }
}

/// `finish_move tenant:<slug> note:"..."`, run where the workspace was: deletes what it left behind when it moved.
pub struct FinishMove;

#[async_trait]
impl Task for FinishMove {
    fn task(&self) -> TaskInfo {
        TaskInfo {
            name: "finish_move".into(),
            detail: "Run where a workspace moved from: deletes the frozen copy it left here (its rows, and its files unless the storage is shared)".into(),
        }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let slug = vars.cli_arg("tenant").map_err(|_| Error::Message("which workspace: tenant:<slug>".into()))?;
        let note = vars.cli_arg("note").map_err(|_| Error::Message("say what you checked at its new place: note:\"...\"".into()))?;
        let done = k.finish_move(slug, note).await.map_err(refused)?;
        println!(
            "{slug}: {} rows and {} files deleted here; it lives at {} now",
            done["rows"],
            done["files"],
            done["moved_to"].as_str().unwrap_or("its new place")
        );
        Ok(())
    }
}

/// `release_host host:<name> [leaving_days:<n>]`: an address of a workspace (a subdomain too) is going away. It keeps
/// serving the people signed in there for its leaving period (`FERRUM_HOST_LEAVING_DAYS` unless given; 0: none),
/// then redirects to the workspace's main address for 90 days, and is quarantined for a year. A main address hands
/// its place to another of the workspace's first. Audited as the system's.
pub struct ReleaseHost;

#[async_trait]
impl Task for ReleaseHost {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "release_host".into(), detail: "Release an address of a workspace, with a leaving period for those signed in there".into() }
    }
    async fn run(&self, ctx: &AppContext, vars: &Vars) -> Result<()> {
        let k = kernel(ctx)?;
        let host = vars.cli_arg("host")?;
        let days = match vars.cli_arg("leaving_days") {
            Ok(d) => Some(d.trim().parse::<i64>().map_err(|_| Error::Message("leaving_days: a number of days, 0 for none".into()))?),
            Err(_) => None,
        };
        match k.release_host(host, days).await.map_err(refused)?.1 {
            Some(until) => println!("{host} is leaving: it serves those signed in there until {until}, then redirects"),
            None => println!("{host} is released with no leaving period"),
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
