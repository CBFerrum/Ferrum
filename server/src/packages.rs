//! Functional packages compiled into the server (cargo features): their hooks, reports, web
//! endpoints and periodic upkeep.

use ferrum_kernel::{HookRegistry, Kernel};
use ferrum_meta::{Catalog, Package};
use futures_util::future::BoxFuture;
use sqlx::PgPool;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, watch};

/// How often package upkeep runs (expiring quotations, SLA clocks, mail polling...).
const EVERY: Duration = Duration::from_secs(60);
/// Upkeep's advisory locks are session locks of the two-key form (`pg_try_advisory_lock(int, int)`), a key space the
/// kernel's other locks (one 64-bit key each) never touch: this is the first key, the second comes from the app's id.
const UPKEEP_NAMESPACE: i32 = 0x6675_706b;
/// How many apps' upkeep one replica runs at once (`FERRUM_UPKEEP_CONCURRENCY`); each holds a pooled connection.
const DEFAULT_CONCURRENCY: usize = 2;

/// One piece of upkeep: an app's (or the domain checks') name and what it runs.
pub type Upkeep = (&'static str, Box<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>);

/// The lock of one piece of upkeep: stable across replicas and releases, because it is made from the name alone
/// (FNV-1a, which no library upgrade changes).
fn lock_key(name: &str) -> (i32, i32) {
    let h = name.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
    (UPKEEP_NAMESPACE, h as i32)
}

// EU member states other than Germany are apps of one crate on the EU's (localization.md L2).
#[cfg(feature = "be")]
use ferrum_eu_members::be as ferrum_be;
#[cfg(feature = "es")]
use ferrum_eu_members::es as ferrum_es;
#[cfg(feature = "fr")]
use ferrum_eu_members::fr as ferrum_fr;
#[cfg(feature = "it")]
use ferrum_eu_members::it as ferrum_it;
#[cfg(feature = "pl")]
use ferrum_eu_members::pl as ferrum_pl;
#[cfg(feature = "pt")]
use ferrum_eu_members::pt as ferrum_pt;
#[cfg(feature = "ro")]
use ferrum_eu_members::ro as ferrum_ro;
// The countries on ERP's VAT and GST engine are apps of one crate (localization.md L6, L7).
#[cfg(feature = "ae")]
use ferrum_countries::ae as ferrum_ae;
#[cfg(feature = "au")]
use ferrum_countries::au as ferrum_au;
#[cfg(feature = "cn")]
use ferrum_countries::cn as ferrum_cn;
#[cfg(feature = "id")]
use ferrum_countries::id as ferrum_id;
#[cfg(feature = "jp")]
use ferrum_countries::jp as ferrum_jp;
#[cfg(feature = "mx")]
use ferrum_countries::mx as ferrum_mx;
#[cfg(feature = "my")]
use ferrum_countries::my as ferrum_my;
#[cfg(feature = "nz")]
use ferrum_countries::nz as ferrum_nz;
#[cfg(feature = "ph")]
use ferrum_countries::ph as ferrum_ph;
#[cfg(feature = "sa")]
use ferrum_countries::sa as ferrum_sa;
#[cfg(feature = "sg")]
use ferrum_countries::sg as ferrum_sg;
#[cfg(feature = "th")]
use ferrum_countries::th as ferrum_th;
#[cfg(feature = "vn")]
use ferrum_countries::vn as ferrum_vn;

/// Two kinds of compiled apps: `native` ones carry their packages and manifest (built in their
/// own repositories, docs/plan/apps.md); `legacy` ones still keep their YAML in `packages/`.
macro_rules! apps {
    (
        native { $($nfeature:literal => $nkrate:ident),* $(,)? }
        legacy { $($lfeature:literal => $lkrate:ident [$($yaml:literal),+]),* $(,)? }
    ) => {
        /// Registers every compiled app's hooks, reports and endpoints: ERP's first, so the apps
        /// that build on its documents see their totals, postings and open items.
        pub fn register(hooks: &mut HookRegistry) {
            ferrum_app_erp::register(hooks);
            for source in ferrum_app_erp::reports() {
                ferrum_domain::reports::add_source(source);
            }
            $(
                #[cfg(feature = $nfeature)]
                {
                    // An app's document hooks run only for workspaces that installed one of its packages.
                    let packages: Vec<String> = $nkrate::packages().map(|ps| ps.iter().map(|p| p.name().to_string()).collect()).unwrap_or_default();
                    if packages.is_empty() {
                        $nkrate::register(hooks);
                    } else {
                        hooks.scoped(&packages, |h| $nkrate::register(h));
                    }
                    for source in $nkrate::reports() {
                        ferrum_domain::reports::add_source(source);
                    }
                }
            )*
            $(
                #[cfg(feature = $lfeature)]
                {
                    $lkrate::register(hooks);
                    for source in $lkrate::reports() {
                        ferrum_domain::reports::add_source(source);
                    }
                }
            )*
            // Apps built in elsewhere, scoped to their packages as native ones are.
            for app in crate::extensions::all() {
                let packages: Vec<String> = (app.packages)().map(|ps| ps.iter().map(|p| p.name().to_string()).collect()).unwrap_or_default();
                hooks.scoped(&packages, |h| (app.register)(h));
                for source in (app.reports)() {
                    ferrum_domain::reports::add_source(source);
                }
            }
            let _ = hooks;
        }

        /// Every compiled app's upkeep, one job each, ERP's first (`round` runs them).
        pub fn upkeep_jobs(k: &Arc<Kernel>) -> Vec<Upkeep> {
            let mut jobs: Vec<Upkeep> = Vec::new();
            let k1 = k.clone();
            jobs.push(("erp", Box::new(move || { let k = k1.clone(); Box::pin(async move { ferrum_app_erp::maintenance(&k).await.map_err(|e| e.to_string()) }) })));
            $(
                #[cfg(feature = $nfeature)]
                {
                    let k1 = k.clone();
                    jobs.push(($nfeature, Box::new(move || { let k = k1.clone(); Box::pin(async move { $nkrate::maintenance(&k).await.map_err(|e| e.to_string()) }) })));
                }
            )*
            $(
                #[cfg(feature = $lfeature)]
                {
                    let k1 = k.clone();
                    jobs.push(($lfeature, Box::new(move || { let k = k1.clone(); Box::pin(async move { $lkrate::maintenance(&k).await.map_err(|e| e.to_string()) }) })));
                }
            )*
            for app in crate::extensions::all() {
                let k1 = k.clone();
                jobs.push((app.id, Box::new(move || { let k = k1.clone(); Box::pin(async move { (app.maintenance)(&k).await.map_err(|e| e.to_string()) }) })));
            }
            jobs
        }

        /// Every compiled app's reports by name, with the app that gives each.
        #[cfg(test)]
        fn report_names() -> Vec<(String, &'static str)> {
            let names = |app: &'static str, sources: Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>>| {
                sources.iter().flat_map(|s| s.catalog()).filter_map(|e| e.get("name").and_then(serde_json::Value::as_str).map(|n| (n.to_string(), app))).collect::<Vec<_>>()
            };
            #[allow(unused_mut)]
            let mut out = names("erp", ferrum_app_erp::reports());
            $(
                #[cfg(feature = $nfeature)]
                out.extend(names($nfeature, $nkrate::reports()));
            )*
            $(
                #[cfg(feature = $lfeature)]
                out.extend(names($lfeature, $lkrate::reports()));
            )*
            for app in crate::extensions::all() {
                out.extend(names(app.id, (app.reports)()));
            }
            out
        }

        /// Packages embedded in the compiled native apps, ERP's first.
        pub fn native_packages() -> Result<Vec<Package>, String> {
            #[allow(unused_mut)]
            let mut out = ferrum_app_erp::packages().map_err(|e| format!("app erp: {e}"))?;
            $(
                #[cfg(feature = $nfeature)]
                out.extend($nkrate::packages().map_err(|e| format!("app {}: {e}", $nfeature))?);
            )*
            for app in crate::extensions::all() {
                out.extend((app.packages)().map_err(|e| format!("app {}: {e}", app.id))?);
            }
            Ok(out)
        }

        /// Manifests (`app.yaml`) of the compiled native apps.
        #[allow(clippy::vec_init_then_push)]
        pub fn native_manifests() -> Vec<&'static str> {
            #[allow(unused_mut)]
            let mut out = Vec::new();
            $(
                #[cfg(feature = $nfeature)]
                out.push($nkrate::MANIFEST);
            )*
            // Drive's add-on Browser editors: a manifest without code, shipped with Drive.
            #[cfg(feature = "drive")]
            out.push(ferrum_app_drive::EDITORS_MANIFEST);
            // Connectors with ERP that ship with their apps (their packages and code are the apps').
            #[cfg(feature = "crm")]
            out.push(ferrum_app_crm::CONNECTOR_MANIFEST);
            #[cfg(feature = "helpdesk")]
            out.push(ferrum_app_helpdesk::CONNECTOR_MANIFEST);
            #[cfg(feature = "hr")]
            out.push(ferrum_app_hr::CONNECTOR_MANIFEST);
            #[cfg(feature = "projects")]
            out.push(ferrum_app_projects::CONNECTOR_MANIFEST);
            #[cfg(all(feature = "assets", feature = "ops"))]
            out.push(ferrum_app_assets::CONNECTOR_MANIFEST);
            #[cfg(all(feature = "commerce", feature = "finance"))]
            out.push(ferrum_app_commerce::CONNECTOR_MANIFEST);
            #[cfg(all(feature = "bookings", feature = "commerce"))]
            out.push(ferrum_app_bookings::CONNECTOR_MANIFEST);
            #[cfg(all(feature = "lms", feature = "commerce"))]
            out.push(ferrum_app_lms::CONNECTOR_MANIFEST);
            #[cfg(all(feature = "itsm", feature = "assets"))]
            out.push(ferrum_app_itsm::CONNECTOR_MANIFEST);
            #[cfg(feature = "control")]
            out.push(ferrum_app_control::CONNECTOR_MANIFEST);
            #[cfg(all(feature = "objectives", feature = "metrics"))]
            out.push(ferrum_app_objectives::CONNECTOR_MANIFEST);
            // ERP, compiled into every build (docs/plan/erp-repo.md).
            out.push(ERP_MANIFEST);
            // Suites, where this build has their members.
            #[cfg(all(feature = "comms", feature = "drive", feature = "docs"))]
            out.push(include_str!("../apps/suites/workspace.yaml"));
            #[cfg(all(feature = "crm", feature = "helpdesk", feature = "projects"))]
            out.push(include_str!("../apps/suites/business.yaml"));
            #[cfg(all(feature = "commerce", feature = "pos"))]
            out.push(include_str!("../apps/suites/retail.yaml"));
            #[cfg(all(feature = "crm", feature = "projects", feature = "helpdesk", feature = "bookings", feature = "forms", feature = "sign"))]
            out.push(include_str!("../apps/suites/services.yaml"));
            #[cfg(all(feature = "field-service", feature = "assets", feature = "helpdesk"))]
            out.push(include_str!("../apps/suites/field-operations.yaml"));
            out.extend(crate::extensions::all().iter().map(|app| app.manifest));
            out
        }

        /// The interfaces compiled in with their apps (an app crate's `ui/dist`, as `UI`): (app id,
        /// its built files). Apps without an interface of their own are not listed.
        #[allow(clippy::vec_init_then_push)]
        pub fn native_uis() -> Vec<(&'static str, &'static [(&'static str, &'static [u8])])> {
            #[allow(unused_mut)]
            let mut out: Vec<(&'static str, &'static [(&'static str, &'static [u8])])> = Vec::new();
            out.push(("erp", ferrum_app_erp::UI));
            #[cfg(feature = "comms")]
            out.push(("comms", ferrum_app_comms::UI));
            #[cfg(feature = "drive")]
            out.push(("drive", ferrum_app_drive::UI));
            #[cfg(feature = "docs")]
            out.push(("docs", ferrum_app_docs::UI));
            #[cfg(feature = "crm")]
            out.push(("crm", ferrum_app_crm::UI));
            #[cfg(feature = "helpdesk")]
            out.push(("helpdesk", ferrum_app_helpdesk::UI));
            #[cfg(feature = "hr")]
            out.push(("hr", ferrum_app_hr::UI));
            #[cfg(feature = "projects")]
            out.push(("projects", ferrum_app_projects::UI));
            #[cfg(feature = "commerce")]
            out.push(("commerce", ferrum_app_commerce::UI));
            #[cfg(feature = "pos")]
            out.push(("pos", ferrum_app_pos::UI));
            #[cfg(feature = "bookings")]
            out.push(("bookings", ferrum_app_bookings::UI));
            #[cfg(feature = "hospitality")]
            out.push(("hospitality", ferrum_app_hospitality::UI));
            #[cfg(feature = "nonprofit")]
            out.push(("nonprofit", ferrum_app_nonprofit::UI));
            #[cfg(feature = "education")]
            out.push(("education", ferrum_app_education::UI));
            #[cfg(feature = "restaurant")]
            out.push(("restaurant", ferrum_app_restaurant::UI));
            #[cfg(feature = "property")]
            out.push(("property", ferrum_app_property::UI));
            #[cfg(feature = "construction")]
            out.push(("construction", ferrum_app_construction::UI));
            #[cfg(feature = "logistics")]
            out.push(("logistics", ferrum_app_logistics::UI));
            #[cfg(feature = "healthcare")]
            out.push(("healthcare", ferrum_app_healthcare::UI));
            #[cfg(feature = "government")]
            out.push(("government", ferrum_app_government::UI));
            #[cfg(feature = "retail")]
            out.push(("retail", ferrum_app_retail::UI));
            #[cfg(feature = "itsm")]
            out.push(("itsm", ferrum_app_itsm::UI));
            #[cfg(feature = "grc")]
            out.push(("grc", ferrum_app_grc::UI));
            #[cfg(feature = "contracts")]
            out.push(("contracts", ferrum_app_contracts::UI));
            #[cfg(feature = "objectives")]
            out.push(("objectives", ferrum_app_objectives::UI));
            #[cfg(feature = "dam")]
            out.push(("dam", ferrum_app_dam::UI));
            #[cfg(feature = "metrics")]
            out.push(("metrics", ferrum_app_metrics::UI));
            #[cfg(feature = "control")]
            out.push(("control", ferrum_app_control::UI));
            #[cfg(feature = "social")]
            out.push(("social", ferrum_app_social::UI));
            #[cfg(feature = "developers")]
            out.push(("developers", ferrum_app_developers::UI));
            #[cfg(feature = "intranet")]
            out.push(("intranet", ferrum_app_intranet::UI));
            #[cfg(feature = "website")]
            out.push(("website", ferrum_app_website::UI));
            #[cfg(feature = "field-service")]
            out.push(("field-service", ferrum_app_field_service::UI));
            #[cfg(feature = "lms")]
            out.push(("lms", ferrum_app_lms::UI));
            #[cfg(feature = "sign")]
            out.push(("sign", ferrum_app_sign::UI));
            #[cfg(feature = "forms")]
            out.push(("forms", ferrum_app_forms::UI));
            #[cfg(feature = "assets")]
            out.push(("assets", ferrum_app_assets::UI));
            #[cfg(feature = "ops")]
            out.push(("ops", ferrum_ops::UI));
            #[cfg(feature = "edge")]
            out.push(("edge", ferrum_app_edge::UI));
            #[cfg(feature = "wms")]
            out.push(("wms", ferrum_wms::UI));
            #[cfg(feature = "mes")]
            out.push(("mes", ferrum_mes::UI));
            #[cfg(feature = "campaigns")]
            out.push(("campaigns", ferrum_app_campaigns::UI));
            out.extend(crate::extensions::all().iter().filter(|app| !app.ui.is_empty()).map(|app| (app.id, app.ui)));
            out
        }

        /// YAML packages of legacy apps whose code is not compiled into this build.
        pub fn missing_logic(catalog: &Catalog) -> Vec<String> {
            #[allow(unused_mut)]
            let mut out: Vec<String> = Vec::new();
            $(
                if !cfg!(feature = $lfeature) {
                    out.extend([$($yaml),+].into_iter().filter(|p| catalog.package(p).is_some()).map(String::from));
                }
            )*
            let _ = catalog;
            out
        }
    };
}

apps! {
    native {
        // India first: its hooks run right after ERP's, ahead of the other add-ons' on ERP's documents.
        "india" => ferrum_india,
        // The EU's rules for companies in its member states (localization.md L1).
        "eu" => ferrum_eu,
        // Member states' rules after the EU's, which they adjust (localization.md L2).
        "de" => ferrum_de,
        "be" => ferrum_be,
        "es" => ferrum_es,
        "fr" => ferrum_fr,
        "it" => ferrum_it,
        "pl" => ferrum_pl,
        "pt" => ferrum_pt,
        "ro" => ferrum_ro,
        // The United Kingdom (localization.md L3).
        "gb" => ferrum_gb,
        // Canada (localization.md L5).
        "ca" => ferrum_ca,
        // The United States (localization.md L4).
        "us" => ferrum_us,
        // South East Asia, the Gulf, Australia, New Zealand, Japan, Mexico and China (localization.md L6, L7).
        "sg" => ferrum_sg,
        "my" => ferrum_my,
        "id" => ferrum_id,
        "th" => ferrum_th,
        "ph" => ferrum_ph,
        "vn" => ferrum_vn,
        "sa" => ferrum_sa,
        "ae" => ferrum_ae,
        "au" => ferrum_au,
        "nz" => ferrum_nz,
        "jp" => ferrum_jp,
        "mx" => ferrum_mx,
        "cn" => ferrum_cn,
        "crm" => ferrum_app_crm,
        "projects" => ferrum_app_projects,
        "helpdesk" => ferrum_app_helpdesk,
        "pos" => ferrum_app_pos,
        "commerce" => ferrum_app_commerce,
        "hr" => ferrum_app_hr,
        "comms" => ferrum_app_comms,
        "procurement" => ferrum_procurement,
        "stock-ops" => ferrum_stock_ops,
        "finance" => ferrum_finance,
        "mfg-ops" => ferrum_mfg_ops,
        "ops" => ferrum_ops,
        "edge" => ferrum_app_edge,
        "quality" => ferrum_quality,
        "wms" => ferrum_wms,
        "mes" => ferrum_mes,
        "studio" => ferrum_studio,
        "assets" => ferrum_app_assets,
        "bookings" => ferrum_app_bookings,
        "hospitality" => ferrum_app_hospitality,
        "nonprofit" => ferrum_app_nonprofit,
        "education" => ferrum_app_education,
        "restaurant" => ferrum_app_restaurant,
        "property" => ferrum_app_property,
        "construction" => ferrum_app_construction,
        "logistics" => ferrum_app_logistics,
        "healthcare" => ferrum_app_healthcare,
        "government" => ferrum_app_government,
        "retail" => ferrum_app_retail,
        "itsm" => ferrum_app_itsm,
        "grc" => ferrum_app_grc,
        "contracts" => ferrum_app_contracts,
        "objectives" => ferrum_app_objectives,
        "dam" => ferrum_app_dam,
        "metrics" => ferrum_app_metrics,
        "intelligence" => ferrum_app_intelligence,
        "control" => ferrum_app_control,
        "social" => ferrum_app_social,
        "field-service" => ferrum_app_field_service,
        "forms" => ferrum_app_forms,
        "lms" => ferrum_app_lms,
        "drive" => ferrum_app_drive,
        "docs" => ferrum_app_docs,
        "campaigns" => ferrum_app_campaigns,
        "sign" => ferrum_app_sign,
        "developers" => ferrum_app_developers,
        "intranet" => ferrum_app_intranet,
        "website" => ferrum_app_website,
    }
    legacy {
    }
}

/// ERP's app manifest: its interface at /erp/, its packages and the records it shows (its
/// repository, docs/plan/erp-repo.md).
pub const ERP_MANIFEST: &str = ferrum_app_erp::MANIFEST;

/// The catalog this build starts from: the shared embedded packages, the native apps' embedded
/// packages (ERP's among them), and extra packages from `dir` (metadata-only packages an operator adds, and
/// legacy apps' YAML). An embedded package wins over a directory copy of the same name.
pub fn base_catalog(dir: &Path) -> Result<Catalog, String> {
    let mut pkgs = ferrum_domain::packages().map_err(|e| format!("shared packages: {e}"))?;
    pkgs.extend(native_packages()?);
    // A connector between two compiled apps ships with one of them: without the other in this
    // build it is left out.
    loop {
        let names: std::collections::HashSet<String> = pkgs.iter().map(|p| p.name().to_string()).collect();
        let before = pkgs.len();
        pkgs.retain(|p| {
            let ok = p.manifest.depends.iter().all(|d| names.contains(d));
            if !ok {
                tracing::warn!(package = p.name(), "left out: this build lacks a package it depends on");
            }
            ok
        });
        if pkgs.len() == before {
            break;
        }
    }
    if dir.is_dir() {
        let mut dirs: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("package.yaml").is_file())
            .collect();
        dirs.sort();
        for d in dirs {
            let p = ferrum_meta::catalog::load_package_dir(&d).map_err(|e| e.to_string())?;
            if !pkgs.iter().any(|x| x.name() == p.name()) {
                pkgs.push(p);
            }
        }
    }
    Catalog::from_packages(pkgs).map_err(|e| e.to_string())
}

/// One round of upkeep. Each piece has an advisory lock of its own, tried on a connection held while it runs: a replica
/// that gets it runs it, one that does not leaves it to whoever has it. At most `concurrency` run at once here, in an
/// order that differs between replicas, so a slow app delays no other and replicas share the work. A failure or a panic
/// is logged with the app's name and stops nothing.
pub async fn round(pool: &PgPool, mut jobs: Vec<Upkeep>, concurrency: usize, every: Duration) {
    // Shuffled with one random key per job: a key that changed between comparisons would not be an order.
    jobs.sort_by_cached_key(|_| uuid::Uuid::new_v4());
    let slots = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut running = tokio::task::JoinSet::new();
    for (name, run) in jobs {
        let Ok(slot) = slots.clone().acquire_owned().await else { break };
        let pool = pool.clone();
        running.spawn(async move {
            let _slot = slot;
            let (a, b) = lock_key(name);
            let mut conn = match pool.acquire().await {
                Ok(c) => c,
                Err(e) => return tracing::warn!(app = name, "upkeep: no connection: {e}"),
            };
            match sqlx::query_scalar::<_, bool>("select pg_try_advisory_lock($1, $2)").bind(a).bind(b).fetch_one(&mut *conn).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => return tracing::warn!(app = name, "upkeep: lock: {e}"),
            }
            let started = Instant::now();
            // Its own task, so that a panic is caught here and the lock is released below.
            match tokio::spawn(run()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(app = name, "app maintenance failed: {e}"),
                Err(e) => tracing::error!(app = name, "app maintenance panicked: {e}"),
            }
            if started.elapsed() > every {
                tracing::warn!(app = name, seconds = started.elapsed().as_secs(), "app maintenance took longer than the round's interval");
            }
            if sqlx::query("select pg_advisory_unlock($1, $2)").bind(a).bind(b).execute(&mut *conn).await.is_err() {
                // The lock goes with the connection.
                let _ = conn.close().await;
            }
        });
    }
    while running.join_next().await.is_some() {}
}

pub fn spawn(k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    let concurrency = std::env::var("FERRUM_UPKEEP_CONCURRENCY").ok().and_then(|v| v.trim().parse().ok()).filter(|n| *n > 0).unwrap_or(DEFAULT_CONCURRENCY);
    tokio::spawn(async move {
        loop {
            let mut jobs = upkeep_jobs(&k);
            // Custom domains due a check (docs/plan/apps-ui.md §7): verified when they answer their token.
            let k1 = k.clone();
            jobs.push((
                "domain-checks",
                Box::new(move || {
                    let k = k1.clone();
                    Box::pin(async move { k.check_hosts().await.map(|_| ()).map_err(|e| e.to_string()) })
                }),
            ));
            round(&k.db.pool, jobs, concurrency, EVERY).await;
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(EVERY) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    /// Every compiled app's manifest is valid, and the platform's packages and every compiled
    /// app's load together as one catalog (no clashing entity names, valid overlays).
    #[test]
    fn compiled_apps_fit_together() {
        let natives: Vec<_> =
            super::native_manifests().into_iter().map(|m| ferrum_apps::AppManifest::parse(m).unwrap_or_else(|e| panic!("{e}\n{m}"))).collect();
        // What a compiled app contributes to another's extension point meets that point's schema
        // (the platform's own points, screens and the shell's, are checked by the manifest).
        let platform = |p: &str| ferrum_apps::UI_POINTS.contains(&p) || ferrum_apps::shell::SHELL_POINTS.contains(&p);
        for m in &natives {
            for (point, items) in m.contributes.iter().filter(|(p, _)| !platform(p)) {
                let owner =
                    natives.iter().find(|o| o.extension_points.contains_key(point)).unwrap_or_else(|| panic!("{}: no compiled app offers {point}", m.id));
                let schema = owner.extension_points[point].schema.clone().unwrap_or_default();
                for item in items {
                    ferrum_apps::contract::conforms(&ferrum_apps::contribution_body(item), &schema).unwrap_or_else(|e| panic!("{} on {point}: {e}", m.id));
                }
            }
        }
        // Every launcher tile has its picture: a name the shell cannot draw falls back to a letter.
        for m in &natives {
            for item in m.contributes.get("shell.app").into_iter().flatten() {
                if let Some(icon) = item.get("icon").and_then(serde_json::Value::as_str) {
                    assert!(crate::shell::lucide(icon).is_some(), "{}: the shell has no icon {icon}", m.id);
                }
            }
        }
        // What a compiled app requires of another compiled app, the release compiled with it meets:
        // a range left behind would leave the app impossible to install (Campaigns asked for Comms
        // 0.7 beside Comms 0.10).
        let versions: std::collections::BTreeMap<_, _> = natives.iter().map(|m| (m.id.as_str(), &m.version)).collect();
        let unmet: Vec<String> = natives
            .iter()
            .flat_map(|m| m.requires.iter().map(move |(dep, req)| (m, dep, req)))
            .filter_map(|(m, dep, req)| {
                versions.get(dep.as_str()).filter(|v| !req.matches(v)).map(|v| format!("{} requires {dep} {req}, compiled with {dep} {v}", m.id))
            })
            .collect();
        assert!(unmet.is_empty(), "{unmet:#?}");
        // No two compiled apps give a report one name: /api/reports/{name} runs the first source
        // registered (CRM's Campaign Performance answered for Campaigns').
        let mut reports = std::collections::BTreeMap::new();
        for (name, app) in super::report_names() {
            if let Some(other) = reports.insert(name.clone(), app) {
                panic!("both {other} and {app} give the report {name}");
            }
        }
        // No two compiled apps share an address.
        let mut routes = std::collections::BTreeMap::new();
        for m in &natives {
            for (route, aliases) in m.routes() {
                for r in std::iter::once(route).chain(aliases) {
                    if let Some(other) = routes.insert(r.clone(), m.id.clone()) {
                        panic!("/{r}/ belongs to both {other} and {}", m.id);
                    }
                }
            }
        }
        let empty = std::env::temp_dir().join(format!("ferrum-no-packages-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let catalog = super::base_catalog(&empty).unwrap_or_else(|e| panic!("catalog: {e}"));
        assert!(catalog.package("core").is_some());
        assert_eq!(catalog.unknown_form_fields(), Vec::<String>::new(), "forms place fields nothing defines");
        let _ = std::fs::remove_dir(&empty);
    }

    /// Document hooks run only where their package is (docs/plan/apps-ui.md §13): a workspace
    /// without ERP runs none of ERP's on the records it shares (companies, customers, items).
    #[test]
    fn hooks_run_only_with_their_packages() {
        let mut hooks = ferrum_kernel::HookRegistry::default();
        ferrum_domain::register(&mut hooks);
        super::register(&mut hooks);
        let everywhere: Vec<String> = hooks.scopes().into_iter().filter(|(entity, scope)| scope.is_none() && entity != "*").map(|(e, _)| e).collect();
        assert!(everywhere.is_empty(), "hooks for every workspace on {everywhere:?}");
    }

    /// Every piece of upkeep has its own lock key.
    #[test]
    fn upkeep_locks_differ() {
        assert_eq!(super::lock_key("erp"), super::lock_key("erp"));
        let ids = ["erp", "crm", "hr", "domain-checks", "control", "field-service", "social", "forms"];
        let keys: std::collections::HashSet<_> = ids.iter().map(|i| super::lock_key(i)).collect();
        assert_eq!(keys.len(), ids.len());
    }

    use super::{Upkeep, round};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    type Spans = Arc<Mutex<Vec<(&'static str, Instant, Instant)>>>;

    /// An app whose upkeep takes 150 ms and is recorded; `Err` and panics for the ones that fail.
    fn fake(name: &'static str, spans: &Spans, how: &'static str) -> Upkeep {
        let spans = spans.clone();
        (
            name,
            Box::new(move || {
                let spans = spans.clone();
                Box::pin(async move {
                    let start = Instant::now();
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    spans.lock().unwrap().push((name, start, Instant::now()));
                    match how {
                        "err" => Err("it failed".to_string()),
                        "panic" => panic!("it panicked"),
                        _ => Ok(()),
                    }
                })
            }),
        )
    }

    fn overlap(a: &(&str, Instant, Instant), b: &(&str, Instant, Instant)) -> bool {
        a.1 < b.2 && b.1 < a.2
    }

    /// Upkeep with a lock per app (`FERRUM_TEST_DATABASE_URL`, nothing in it is touched): two replicas never run one app
    /// at once, run different apps at once, are not stopped by an app that fails or panics (whose lock is let go), and
    /// with a limit of one run one app after another.
    #[tokio::test]
    async fn replicas_share_upkeep_app_by_app() {
        let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
        let pool = || sqlx::postgres::PgPoolOptions::new().max_connections(8).connect(&url);
        let (one, two) = (pool().await.expect("connect"), pool().await.expect("connect"));
        let spans: Spans = Default::default();
        let jobs = || vec![fake("fake-a", &spans, "ok"), fake("fake-b", &spans, "ok"), fake("fake-err", &spans, "err"), fake("fake-panic", &spans, "panic")];
        let every = Duration::from_secs(60);
        tokio::join!(round(&one, jobs(), 4, every), round(&two, jobs(), 4, every));
        let seen = spans.lock().unwrap().clone();
        for name in ["fake-a", "fake-b", "fake-err", "fake-panic"] {
            let runs: Vec<_> = seen.iter().filter(|s| s.0 == name).collect();
            assert!(!runs.is_empty(), "{name} never ran");
            for (i, x) in runs.iter().enumerate() {
                assert!(runs[i + 1..].iter().all(|y| !overlap(x, y)), "{name} ran twice at once");
            }
        }
        let a: Vec<_> = seen.iter().filter(|s| s.0 == "fake-a").collect();
        let b: Vec<_> = seen.iter().filter(|s| s.0 == "fake-b").collect();
        assert!(a.iter().any(|x| b.iter().any(|y| overlap(x, y))), "different apps did not run at once");
        // The failed and the panicked apps let their locks go.
        let mut conn = one.acquire().await.unwrap();
        for name in ["fake-err", "fake-panic"] {
            let (k1, k2) = super::lock_key(name);
            let free: bool = sqlx::query_scalar("select pg_try_advisory_lock($1, $2)").bind(k1).bind(k2).fetch_one(&mut *conn).await.unwrap();
            assert!(free, "{name} kept its lock");
        }
        drop(conn.detach());
        // One at a time on a replica set to one.
        spans.lock().unwrap().clear();
        round(&one, jobs(), 1, every).await;
        let seen = spans.lock().unwrap().clone();
        assert_eq!(seen.len(), 4);
        for (i, x) in seen.iter().enumerate() {
            assert!(seen[i + 1..].iter().all(|y| !overlap(x, y)), "two apps ran at once with a limit of one");
        }
    }
}
