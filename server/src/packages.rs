//! Functional packages compiled into the server (cargo features): their hooks, reports, web
//! endpoints and periodic upkeep.

use ferrum_kernel::doctor::Finding;
use ferrum_kernel::{Ctx, HookRegistry, Kernel};
use ferrum_meta::{Catalog, MetaError, Package};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinSet;
use uuid::Uuid;

/// An app's upkeep for one workspace: its crate's `maintenance(k, tenant)` (docs/app-contract.md, Upkeep).
pub type Run = Arc<dyn for<'a> Fn(&'a Kernel, Uuid) -> BoxFuture<'a, ferrum_kernel::Result<()>> + Send + Sync>;

/// An app as the upkeep runner knows it: its id, the packages that make a workspace one of its own, and its upkeep.
pub struct UpkeepApp {
    pub id: &'static str,
    pub packages: Vec<String>,
    pub run: Run,
}

/// [`Run`] from a function or a closure (the bound lets the compiler see the borrow it returns).
pub fn run<F>(f: F) -> Run
where
    F: for<'a> Fn(&'a Kernel, Uuid) -> BoxFuture<'a, ferrum_kernel::Result<()>> + Send + Sync + 'static,
{
    Arc::new(f)
}

fn names(packages: Result<Vec<Package>, MetaError>) -> Vec<String> {
    packages.map(|ps| ps.iter().map(|p| p.name().to_string()).collect()).unwrap_or_default()
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

/// The compiled apps: each carries its packages and manifest (built in its own repository,
/// docs/plan/apps.md).
macro_rules! apps {
    (
        native { $($nfeature:literal => $nkrate:ident),* $(,)? }
    ) => {
        /// Registers every compiled app's hooks, reports and endpoints: ERP's first, so the apps
        /// that build on its documents see their totals, postings and open items.
        pub fn register(hooks: &mut HookRegistry) -> Result<(), String> {
            // Each app registers as itself, so that it can take its own vault handle (crates/kernel/src/vault), and owns its
            // packages: a secret field one of them declares is its own, which no other app reads.
            hooks.app("erp", &[], ferrum_app_erp::register);
            let mut sources: Vec<(String, std::sync::Arc<dyn ferrum_domain::reports::ReportSource>)> = Vec::new();
            // Which packages give each report (zones, level 2): its app's own.
            let mut owners: std::collections::HashMap<String, Vec<String>> = Default::default();
            let mut own = |packages: &[String], given: &[std::sync::Arc<dyn ferrum_domain::reports::ReportSource>]| {
                for name in given.iter().flat_map(|s| s.catalog()).filter_map(|e| e["name"].as_str().map(str::to_string)) {
                    owners.insert(name, packages.to_vec());
                }
            };
            let erp: Vec<String> = ferrum_app_erp::packages().map(|ps| ps.iter().map(|p| p.name().to_string()).collect()).unwrap_or_default();
            hooks.own("erp", &erp);
            own(&erp, &ferrum_app_erp::reports());
            sources.extend(ferrum_app_erp::reports().into_iter().map(|s| ("erp".to_string(), s)));
            $(
                #[cfg(feature = $nfeature)]
                {
                    // An app's document hooks run only for workspaces that installed one of its packages.
                    let packages: Vec<String> = $nkrate::packages().map(|ps| ps.iter().map(|p| p.name().to_string()).collect()).unwrap_or_default();
                    hooks.app($nfeature, &packages, |h| $nkrate::register(h));
                    hooks.own($nfeature, &packages);
                    own(&packages, &$nkrate::reports());
                    sources.extend($nkrate::reports().into_iter().map(|s| ($nfeature.to_string(), s)));
                }
            )*
            // Apps built in elsewhere, scoped to their packages as native ones are.
            for app in crate::extensions::all() {
                let packages: Vec<String> = (app.packages)().map(|ps| ps.iter().map(|p| p.name().to_string()).collect()).unwrap_or_default();
                hooks.app(app.id, &[], |h| {
                    h.scoped(&packages, |h| (app.register)(h));
                });
                hooks.own(app.id, &packages);
                own(&packages, &(app.reports)());
                sources.extend((app.reports)().into_iter().map(|s| (app.id.to_string(), s)));
            }
            let _ = hooks;
            ferrum_domain::reports::set_owners(owners);
            // Set, not added to: a process may set up more than once (a test's replicas).
            ferrum_domain::reports::set_sources(sources)
        }

        /// Every compiled app's upkeep, with the packages that make a workspace one of its own ([`spawn`] runs them).
        pub fn upkeep_apps() -> Vec<UpkeepApp> {
            #[allow(unused_mut)]
            let mut apps = vec![UpkeepApp { id: "erp", packages: names(ferrum_app_erp::packages()), run: run(|k, t| Box::pin(ferrum_app_erp::maintenance(k, t))) }];
            $(
                #[cfg(feature = $nfeature)]
                apps.push(UpkeepApp { id: $nfeature, packages: names($nkrate::packages()), run: run(|k, t| Box::pin($nkrate::maintenance(k, t))) });
            )*
            for app in crate::extensions::all() {
                apps.push(UpkeepApp { id: app.id, packages: names((app.packages)()), run: run(move |k, t| (app.maintenance)(k, t)) });
            }
            apps
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
            for app in crate::extensions::all() {
                out.extend(names(app.id, (app.reports)()));
            }
            out
        }

        /// Every compiled app's reports: (report, app, the entity it is gated on, the app's own packages).
        #[cfg(test)]
        fn report_gates() -> Vec<(String, &'static str, Option<String>, Vec<String>)> {
            let gates = |app: &'static str, packages: Result<Vec<Package>, String>, sources: Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>>| {
                let packages: Vec<String> = packages.unwrap_or_default().iter().map(|p| p.name().to_string()).collect();
                sources
                    .iter()
                    .flat_map(|s| s.catalog())
                    .map(|e| (e["name"].as_str().unwrap_or_default().to_string(), app, e["entity"].as_str().map(str::to_string), packages.clone()))
                    .collect::<Vec<_>>()
            };
            #[allow(unused_mut)]
            let mut out = gates("erp", ferrum_app_erp::packages().map_err(|e| e.to_string()), ferrum_app_erp::reports());
            $(
                #[cfg(feature = $nfeature)]
                out.extend(gates($nfeature, $nkrate::packages().map_err(|e| e.to_string()), $nkrate::reports()));
            )*
            for app in crate::extensions::all() {
                out.extend(gates(app.id, (app.packages)().map_err(|e| e.to_string()), (app.reports)()));
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
            #[cfg(feature = "live")]
            out.push(("live", ferrum_app_live::UI));
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
        "live" => ferrum_app_live,
        "developers" => ferrum_app_developers,
        "intranet" => ferrum_app_intranet,
        "website" => ferrum_app_website,
    }
}

/// ERP's app manifest: its interface at /erp/, its packages and the records it shows (its
/// repository, docs/plan/erp-repo.md).
pub const ERP_MANIFEST: &str = ferrum_app_erp::MANIFEST;

/// The catalog this build starts from: the shared embedded packages, the native apps' embedded
/// packages (ERP's among them), and extra packages from `dir` (metadata-only packages an operator adds). An embedded package wins over a directory copy of the same name.
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

/// How the upkeep runner paces itself: [`spawn`] takes the default, with the replica's slots; tests shorten it.
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    /// Workspaces' upkeep this replica runs at once (`FERRUM_UPKEEP_CONCURRENCY`, `DbConfig::upkeep`).
    pub slots: usize,
    /// How often it looks for upkeep fallen due while none of its runs ends.
    pub poll: Duration,
    /// How often the rows are brought in line with what workspaces have installed ([`sync`]).
    pub sync: Duration,
    /// The longest one app's upkeep of one workspace may take: past it, the run is dropped (its open transaction rolls
    /// back) and recorded as failed.
    pub limit: Duration,
    /// How long a claim holds its row: past it, a replica that is gone or late leaves the row to whichever is free.
    pub lease: Duration,
    /// The wait after a run, before the same app is due again in the same workspace.
    pub every: Duration,
    /// The longest wait after runs in a row that hit the limit: each doubles [`Pace::every`] up to it.
    pub most: Duration,
    /// How long a stopping replica lets its runs finish before it gives their rows back.
    pub grace: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        let s = Duration::from_secs;
        Pace {
            slots: ferrum_kernel::db::UPKEEP_AT_ONCE as usize,
            poll: s(5),
            sync: s(60),
            limit: s(270),
            lease: s(300),
            every: s(60),
            most: s(3600),
            grace: s(5),
        }
    }
}

/// How many workspaces' upkeep one replica runs at once (`FERRUM_UPKEEP_CONCURRENCY`): each holds a connection of the
/// main pool while it runs.
pub fn upkeep_concurrency() -> usize {
    ferrum_kernel::env::var("FERRUM_UPKEEP_CONCURRENCY")
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(ferrum_kernel::db::UPKEEP_AT_ONCE as usize)
}

/// Every workspace that has the packages of `apps`, with each such app: a row of `upkeep` each (crates/kernel/migrations,
/// the baseline).
const WANTED: &str = "select distinct p.tenant_id, a.app from unnest($1::text[], $2::text[]) a(app, package) \
     join installed_package p on p.name = a.package join tenant t on t.id = p.tenant_id and t.status = 'active'";

/// Brings the rows in line with what workspaces have installed: one for each active workspace and each app of `apps`
/// with one of its packages there, due at once when new; every other row goes (the app's packages uninstalled, the
/// workspace suspended or gone, an app this build lacks). A workspace moving is left as it stands. One replica at a time:
/// `false` when another holds the step.
pub async fn sync(k: &Kernel, apps: &[UpkeepApp]) -> ferrum_kernel::Result<bool> {
    let (ids, packages): (Vec<String>, Vec<String>) = apps.iter().flat_map(|a| a.packages.iter().map(move |p| (a.id.to_string(), p.clone()))).unzip();
    let mut tx = k.db.system_tx().await?;
    if !sqlx::query_scalar::<_, bool>("select pg_try_advisory_xact_lock(hashtext('ferrum:upkeep'))").fetch_one(&mut *tx).await? {
        return Ok(false);
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "insert into upkeep (tenant_id, app) select w.tenant_id, w.app from ({WANTED}) w where not ferrum_workspace_held(w.tenant_id) on conflict do nothing"
    )))
    .bind(&ids)
    .bind(&packages)
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!("delete from upkeep u where not ferrum_workspace_held(u.tenant_id) and (u.tenant_id, u.app) not in ({WANTED})")))
        .bind(&ids)
        .bind(&packages)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// The custom domains due a check (docs/plan/apps-ui.md §7), checked once whichever replicas ask: under a transaction
/// lock held while they are checked, so a replica that comes after finds them checked (each check records it on the
/// domain) and one that comes meanwhile leaves them. The checks call outside, at most six domains of ten seconds each:
/// this transaction may wait that long between its statements.
async fn domain_checks(k: &Kernel) -> ferrum_kernel::Result<()> {
    let mut tx = k.db.system_tx().await?;
    sqlx::query("set local idle_in_transaction_session_timeout = '120s'").execute(&mut *tx).await?;
    if sqlx::query_scalar::<_, bool>("select pg_try_advisory_xact_lock(hashtext('ferrum:domain_checks'))").fetch_one(&mut *tx).await? {
        k.check_hosts().await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A row this replica holds, until its lease (the row's `due_at` while held) ends.
struct Claim {
    tenant: Uuid,
    app: &'static str,
    lease: chrono::DateTime<chrono::Utc>,
}

/// How a run ended, when it did not end well.
enum Ended {
    Failed(String),
    TimedOut,
}

/// A run's end to record: its claim and how it ended (`None`: it ended well).
type Finished<'a> = (&'a Claim, Option<&'a Ended>);

/// One statement, in one transaction, for both halves of the runner's bookkeeping. First it records how a run ended,
/// on its row: only while the row still holds this claim's lease, so a replica that finishes after its lease ended,
/// once another has run the pair again, changes nothing; nor while the workspace moves. Then it takes up to `free`
/// rows due, the longest waiting first, of the apps of `apps`, in workspaces that are not moving: each held until
/// `lease` from now. The rows of `full` (apps holding their share of this replica's slots) come after every other
/// app's, so that one app slow in many workspaces cannot hold every slot while others wait; with nothing else due they
/// still run. The row just recorded is not taken again by the same statement (one statement may change a row once).
async fn step(k: &Kernel, finished: Option<Finished<'_>>, apps: &[UpkeepApp], free: usize, full: &[&str], pace: &Pace) -> ferrum_kernel::Result<Vec<Claim>> {
    let ids: Vec<&str> = apps.iter().map(|a| a.id).collect();
    let (claim, error, timed_out) = match finished {
        None => (None, None, false),
        Some((c, None)) => (Some(c), None, false),
        Some((c, Some(Ended::Failed(e)))) => (Some(c), Some(e.clone()), false),
        Some((c, Some(Ended::TimedOut))) => (Some(c), Some(format!("took longer than {} s, and was stopped", pace.limit.as_secs())), true),
    };
    let mut tx = k.db.system_tx().await?;
    let rows: Vec<(Uuid, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "with recorded as (update upkeep set timeouts = case when $5 then timeouts + 1 else 0 end, \
           due_at = now() + make_interval(secs => case when $5 then least($6 * power(2::float8, least(timeouts + 1, 20)), $7) else $6 end), \
           ran_at = case when $4::text is null then now() else ran_at end, failed_at = case when $4::text is null then failed_at else now() end, \
           error = left($4, 2000) where tenant_id = $1 and app = $2 and due_at = $3 and not ferrum_workspace_held(tenant_id) returning 1), \
         due as (select tenant_id, app from upkeep where due_at <= now() and app = any($9) and not ferrum_workspace_held(tenant_id) \
           and (tenant_id, app) is distinct from ($1, $2) order by app = any($11), due_at limit $8 for update skip locked) \
         update upkeep u set due_at = now() + make_interval(secs => $10) from due where (u.tenant_id, u.app) = (due.tenant_id, due.app) \
         returning u.tenant_id, u.app, u.due_at",
    )
    .bind(claim.map(|c| c.tenant))
    .bind(claim.map(|c| c.app))
    .bind(claim.map(|c| c.lease))
    .bind(error)
    .bind(timed_out)
    .bind(pace.every.as_secs_f64())
    .bind(pace.most.as_secs_f64())
    .bind(free as i64)
    .bind(&ids)
    .bind(pace.lease.as_secs_f64())
    .bind(full)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().filter_map(|(tenant, app, lease)| ids.iter().find(|i| **i == app).map(|&app| Claim { tenant, app, lease })).collect())
}

/// Gives the rows of claims whose runs did not end back, due at once (a replica stopping).
async fn hand_back(k: &Kernel, claims: Vec<Claim>) -> ferrum_kernel::Result<()> {
    if claims.is_empty() {
        return Ok(());
    }
    let tenants: Vec<Uuid> = claims.iter().map(|c| c.tenant).collect();
    let apps: Vec<&str> = claims.iter().map(|c| c.app).collect();
    let leases: Vec<chrono::DateTime<chrono::Utc>> = claims.iter().map(|c| c.lease).collect();
    let mut tx = k.db.system_tx().await?;
    sqlx::query(
        "update upkeep u set due_at = now() from unnest($1::uuid[], $2::text[], $3::timestamptz[]) c(tenant, app, lease) \
         where (u.tenant_id, u.app, u.due_at) = (c.tenant, c.app, c.lease)",
    )
    .bind(&tenants)
    .bind(&apps)
    .bind(&leases)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Every compiled app's upkeep, workspace by workspace, on this replica as its slots allow (`FERRUM_UPKEEP_CONCURRENCY`).
pub fn spawn(k: Arc<Kernel>, shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    let pace = Pace { slots: k.db.config.upkeep.max(1) as usize, ..Pace::default() };
    spawn_with(k, upkeep_apps(), pace, shutdown)
}

/// The upkeep runner. Once a [`Pace::sync`] it brings the rows in line with what is installed ([`sync`], one replica at
/// a time) and checks the custom domains due ([`domain_checks`], each once). Whenever a slot is free it claims the rows due, the longest
/// waiting first (an app already holding half its slots after the others), and runs each app's upkeep for its workspace in a task of its own, as that workspace's work
/// (`sandbox::in_hand`), within [`Pace::limit`]; then records the outcome on the row ([`record`]). Stopping, it lets
/// its runs finish for [`Pace::grace`], stops the rest and gives their rows back.
pub fn spawn_with(k: Arc<Kernel>, apps: Vec<UpkeepApp>, pace: Pace, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let runs: HashMap<&'static str, Run> = apps.iter().map(|a| (a.id, a.run.clone())).collect();
        let mut running: JoinSet<Result<ferrum_kernel::Result<()>, tokio::time::error::Elapsed>> = JoinSet::new();
        let mut held: HashMap<tokio::task::Id, Claim> = HashMap::new();
        let mut poll = tokio::time::interval(pace.poll);
        let mut synced: Option<Instant> = None;
        let mut checks: Option<tokio::task::JoinHandle<()>> = None;
        // The last claim found fewer rows due than it asked for: none is asked for until the next poll.
        let mut drained = false;
        loop {
            if synced.is_none_or(|t| t.elapsed() >= pace.sync) {
                synced = Some(Instant::now());
                drained = false;
                if let Err(e) = sync(&k, &apps).await {
                    tracing::warn!("upkeep: bringing the rows in line with what is installed failed: {e}");
                }
                if checks.as_ref().is_none_or(|c| c.is_finished()) {
                    let k = k.clone();
                    checks = Some(tokio::spawn(async move {
                        if let Err(e) = domain_checks(&k).await {
                            tracing::warn!("checking custom domains failed: {e}");
                        }
                    }));
                }
            }
            // Only to find work when no run is ending: an ending run claims the next for its slot ([`step`]).
            if running.len() < pace.slots && !drained {
                drained = turn(&k, None, &apps, &runs, &mut running, &mut held, &pace).await;
            }
            tokio::select! {
                _ = shutdown.changed() => break,
                Some(done) = running.join_next_with_id(), if !running.is_empty() => {
                    let finished = ended(&mut held, done, &pace);
                    if let Some(f) = finished {
                        let claiming = !drained;
                        let more = turn(&k, Some(f), if claiming { &apps } else { &[] }, &runs, &mut running, &mut held, &pace).await;
                        drained = drained || more;
                    }
                }
                _ = poll.tick() => drained = false,
            }
        }
        let _ = tokio::time::timeout(pace.grace, async {
            while let Some(done) = running.join_next_with_id().await {
                if let Some(f) = ended(&mut held, done, &pace) {
                    turn(&k, Some(f), &[], &runs, &mut running, &mut held, &pace).await;
                }
            }
        })
        .await;
        running.abort_all();
        if let Some(c) = checks {
            c.abort();
        }
        if let Err(e) = hand_back(&k, held.into_values().collect()).await {
            tracing::warn!("upkeep: giving back the rows of runs stopped failed (each is due again when its lease ends): {e}");
        }
    })
}

/// The claim of the run that ended and how it ended, logged; `None` for a run this replica no longer holds.
fn ended(
    held: &mut HashMap<tokio::task::Id, Claim>,
    done: Result<(tokio::task::Id, Result<ferrum_kernel::Result<()>, tokio::time::error::Elapsed>), tokio::task::JoinError>,
    pace: &Pace,
) -> Option<(Claim, Option<Ended>)> {
    let (id, outcome) = match done {
        Ok((id, Ok(Ok(())))) => (id, None),
        Ok((id, Ok(Err(e)))) => (id, Some(Ended::Failed(e.to_string()))),
        Ok((id, Err(_))) => (id, Some(Ended::TimedOut)),
        Err(e) => (e.id(), Some(Ended::Failed(if e.is_panic() { "it panicked".to_string() } else { e.to_string() }))),
    };
    let c = held.remove(&id)?;
    if let Some(Ended::Failed(e)) = &outcome {
        tracing::warn!(app = c.app, tenant = %c.tenant, "app upkeep failed: {e}");
    } else if let Some(Ended::TimedOut) = &outcome {
        tracing::warn!(app = c.app, tenant = %c.tenant, "app upkeep took longer than {} s and was stopped", pace.limit.as_secs());
    }
    Some((c, outcome))
}

/// One [`step`]: records `finished`, if a run ended, and claims for this replica's free slots among `apps` (none: record
/// only), starting each claim's run in a task of its own, as that workspace's work (`sandbox::in_hand`), within
/// [`Pace::limit`]. True when it found fewer rows due than slots free (nothing more is asked for until the next poll).
async fn turn(
    k: &Arc<Kernel>,
    finished: Option<(Claim, Option<Ended>)>,
    apps: &[UpkeepApp],
    runs: &HashMap<&'static str, Run>,
    running: &mut JoinSet<Result<ferrum_kernel::Result<()>, tokio::time::error::Elapsed>>,
    held: &mut HashMap<tokio::task::Id, Claim>,
    pace: &Pace,
) -> bool {
    let free = if apps.is_empty() { 0 } else { pace.slots.saturating_sub(running.len()) };
    // An app's share of the slots: half of them (one, with one or two).
    let share = (pace.slots / 2).max(1);
    let mut busy: HashMap<&str, usize> = HashMap::new();
    for c in held.values() {
        *busy.entry(c.app).or_default() += 1;
    }
    let full: Vec<&str> = busy.into_iter().filter(|(_, n)| *n >= share).map(|(a, _)| a).collect();
    let app = finished.as_ref().map(|(c, _)| c.app);
    match step(k, finished.as_ref().map(|(c, e)| (c, e.as_ref())), apps, free, &full, pace).await {
        Ok(claims) => {
            let drained = claims.len() < free;
            for c in claims {
                let (k, run, tenant, limit) = (k.clone(), runs[c.app].clone(), c.tenant, pace.limit);
                let task = running.spawn(async move { tokio::time::timeout(limit, ferrum_kernel::sandbox::in_hand(tenant, run(&k, tenant))).await });
                held.insert(task.id(), c);
            }
            drained
        }
        Err(e) => {
            match app {
                Some(app) => tracing::warn!(app, "upkeep: recording a run and claiming what is due failed (the row is due again when its lease ends): {e}"),
                None => tracing::warn!("upkeep: claiming what is due failed: {e}"),
            }
            true
        }
    }
}

/// Across every workspace, for `/metrics` and the doctor: how long the longest-waiting row has been due (0 when none
/// is), how many rows failed their last run, and how many have been due for longer than a lease.
pub async fn gauges(k: &Kernel) -> ferrum_kernel::Result<(f64, i64, i64)> {
    let mut tx = k.db.system_tx().await?;
    let row = sqlx::query_as(
        "select coalesce(extract(epoch from now() - min(due_at) filter (where due_at <= now())), 0)::float8, \
         count(*) filter (where failed_at > coalesce(ran_at, '-infinity')), count(*) filter (where due_at < now() - make_interval(secs => $1)) from upkeep",
    )
    .bind(Pace::default().lease.as_secs_f64())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// The doctor's word on apps' upkeep: rows failing, and rows waiting longer than a lease (no slot free for them).
pub async fn findings(k: &Kernel) -> ferrum_kernel::Result<Vec<Finding>> {
    let (oldest, failing, late) = gauges(k).await?;
    Ok(vec![if failing == 0 && late == 0 {
        Finding::ok("upkeep", format!("no app's upkeep failed its last run in any workspace; the longest-waiting has been due {oldest:.0} s"))
    } else {
        Finding::warning(
            "upkeep",
            format!(
                "{failing} workspaces' app upkeep failed its last run, {late} have waited longer than five minutes; the longest-waiting has been due {oldest:.0} s"
            ),
            "A workspace's administrator sees which app and why at GET /api/admin/upkeep, and the log names them. Upkeep waiting long: raise FERRUM_UPKEEP_CONCURRENCY or run more replicas.",
        )
    }])
}

/// A workspace's apps' upkeep, for its administrator: each app's last run, last failure and why, and when it is next
/// due (while a replica runs it, the end of that replica's lease).
pub async fn state(k: &Kernel, ctx: &Ctx) -> ferrum_kernel::Result<Vec<Value>> {
    if !ctx.is_admin {
        return Err(ferrum_kernel::Error::Forbidden("upkeep is for administrators".into()));
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    type Row = (String, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>, Option<String>, i32);
    let rows: Vec<Row> = sqlx::query_as("select app, due_at, ran_at, failed_at, error, timeouts from upkeep where tenant_id = $1 order by app")
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(app, due_at, ran_at, failed_at, error, timeouts)| {
            let state = match (ran_at, failed_at) {
                (_, Some(f)) if ran_at.is_none_or(|r| f > r) => "failing",
                (Some(_), _) => "ok",
                _ => "waiting",
            };
            json!({"app": app, "state": state, "due_at": due_at, "ran_at": ran_at, "failed_at": failed_at, "error": error, "timeouts": timeouts})
        })
        .collect())
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
        // (the shell's own points are checked by the manifest).
        let platform = |p: &str| ferrum_apps::shell::SHELL_POINTS.contains(&p);
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

    /// Reports are narrowed by zones as records are (crate::zones, level 2): `/api/reports/{name}` runs a report only
    /// for whoever may read the entity it names, so that entity must be one of its own app's packages. A report gated
    /// on another app's entity (a shared one such as Customer) would run its app's own SQL for staff whose zone leaves
    /// that app out.
    #[test]
    fn every_report_is_gated_on_its_own_apps_entity() {
        let empty = std::env::temp_dir().join(format!("ferrum-no-packages-gates-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let catalog = super::base_catalog(&empty).unwrap_or_else(|e| panic!("catalog: {e}"));
        let _ = std::fs::remove_dir(&empty);
        let package_of = |entity: &str| catalog.packages().find(|p| p.entities.iter().any(|e| e.name == entity)).map(|p| p.name().to_string());
        let offenders: Vec<String> = super::report_gates()
            .into_iter()
            .filter_map(|(name, app, entity, packages)| {
                let Some(entity) = entity else { return Some(format!("{app}'s report {name} names no entity")) };
                match package_of(&entity) {
                    Some(p) if packages.contains(&p) => None,
                    Some(p) => Some(format!("{app}'s report {name} is gated on {entity}, of package {p}, not one of {app}'s ({})", packages.join(", "))),
                    None => Some(format!("{app}'s report {name} is gated on {entity}, which no package defines")),
                }
            })
            .collect();
        // Gated on an entity of a package their app depends on: each is also kept to its app's own packages where it
        // runs (ferrum_domain::reports::set_owners), so a zone that leaves the app out lists and runs none of them.
        // Moving their gate to an entity of their own app is the apps' to do; a new one here fails.
        const GATED_ELSEWHERE: &[&str] = &[
            "eu_ec_sales_list",
            "eu_oss_return",
            "fr_e_reporting",
            "ca_gst_hst_return",
            "ca_qst_return",
            "us_sales_tax",
            "us_economic_nexus",
            "us_1099",
            "sg_gst_f5",
            "my_sst02",
            "id_spt_masa_ppn",
            "th_pp30",
            "ph_2550q",
            "vn_01_gtgt",
            "sa_vat_return",
            "ae_vat201",
            "au_bas_gst",
            "nz_gst101a",
            "jp_consumption_tax",
            "mx_iva",
            "cn_vat_return",
            "project_profitability",
            "portfolio_overview",
            "web_sales",
            "top_products",
            "purchase_analytics",
            "variant_stock_matrix",
            "stock_ageing",
            "finance_overview",
            "cash_flow_forecast",
            "receivables_by_due_date",
            "work_order_operations",
            "edge_availability",
            "edge_readings",
            "retail_stock_by_store",
        ];
        let new: Vec<&String> = offenders.iter().filter(|o| !GATED_ELSEWHERE.iter().any(|n| o.contains(&format!("report {n} ")))).collect();
        assert!(new.is_empty(), "{new:#?}");
    }

    /// Document hooks run only where their package is (docs/plan/apps-ui.md §13): a workspace
    /// without ERP runs none of ERP's on the records it shares (companies, customers, items).
    #[test]
    fn hooks_run_only_with_their_packages() {
        let mut hooks = ferrum_kernel::HookRegistry::default();
        ferrum_domain::register(&mut hooks);
        super::register(&mut hooks).unwrap();
        let everywhere: Vec<String> = hooks.scopes().into_iter().filter(|(entity, scope)| scope.is_none() && entity != "*").map(|(e, _)| e).collect();
        assert!(everywhere.is_empty(), "hooks for every workspace on {everywhere:?}");
        // Each compiled app owns its packages, and no package is claimed by two (its secret fields are one app's).
        assert_eq!(hooks.settle(), Ok(()));
    }

    /// A process that sets up more than once (a test's replicas) registers the apps each time: their reports are still
    /// listed, and run, once.
    #[test]
    fn reports_are_listed_once_however_often_apps_register() {
        for _ in 0..2 {
            super::register(&mut ferrum_kernel::HookRegistry::default()).unwrap();
        }
        let names: Vec<String> = ferrum_domain::reports::catalog().as_array().unwrap().iter().filter_map(|e| e["name"].as_str().map(str::to_string)).collect();
        let distinct: std::collections::BTreeSet<&String> = names.iter().collect();
        assert!(!names.is_empty());
        assert_eq!(names.len(), distinct.len(), "a report listed twice");
    }
}
