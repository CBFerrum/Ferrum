//! Functional packages compiled into the server (cargo features): their hooks, reports, web
//! endpoints and periodic upkeep.

use ferrum_kernel::{HookRegistry, Kernel};
use ferrum_meta::{Catalog, Package};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How often package upkeep runs (expiring quotations, SLA clocks, mail polling...).
const EVERY: Duration = Duration::from_secs(60);
/// Session advisory lock: one replica runs upkeep at a time; the others skip that round.
const LOCK: i64 = 0x6665_7272_756d_0001;

/// Two kinds of compiled apps: `native` ones carry their packages and manifest (built in their
/// own repositories, docs/plan/apps.md); `legacy` ones still keep their YAML in `packages/`.
macro_rules! apps {
    (
        native { $($nfeature:literal => $nkrate:ident),* $(,)? }
        legacy { $($lfeature:literal => $lkrate:ident [$($yaml:literal),+]),* $(,)? }
    ) => {
        /// Registers every compiled app's hooks, reports and endpoints.
        pub fn register(hooks: &mut HookRegistry) {
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
            let _ = hooks;
        }

        /// Runs every compiled app's upkeep once; failures are logged and do not stop the others.
        pub async fn maintenance(k: &Kernel) {
            $(
                #[cfg(feature = $nfeature)]
                if let Err(e) = $nkrate::maintenance(k).await {
                    tracing::warn!(app = $nfeature, "app maintenance failed: {e}");
                }
            )*
            $(
                #[cfg(feature = $lfeature)]
                if let Err(e) = $lkrate::maintenance(k).await {
                    tracing::warn!(app = $lfeature, "app maintenance failed: {e}");
                }
            )*
            let _ = k;
        }

        /// Packages embedded in the compiled native apps.
        pub fn native_packages() -> Result<Vec<Package>, String> {
            #[allow(unused_mut)]
            let mut out = Vec::new();
            $(
                #[cfg(feature = $nfeature)]
                out.extend($nkrate::packages().map_err(|e| format!("app {}: {e}", $nfeature))?);
            )*
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
            // ERP and its add-on for India, over the platform's own packages (docs/plan/apps-ui.md).
            out.push(ERP_MANIFEST);
            out.push(INDIA_MANIFEST);
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
        "studio" => ferrum_studio,
        "assets" => ferrum_app_assets,
        "bookings" => ferrum_app_bookings,
        "field-service" => ferrum_app_field_service,
        "forms" => ferrum_app_forms,
        "lms" => ferrum_app_lms,
        "drive" => ferrum_app_drive,
        "docs" => ferrum_app_docs,
        "campaigns" => ferrum_app_campaigns,
        "sign" => ferrum_app_sign,
    }
    legacy {
    }
}

/// ERP's app manifest: its interface at /erp/ and the records it shows.
pub const ERP_MANIFEST: &str = include_str!("../apps/erp/app.yaml");

/// ERP's add-on for India.
pub const INDIA_MANIFEST: &str = include_str!("../apps/india/app.yaml");

/// The catalog this build starts from: the platform's embedded packages, the native apps'
/// embedded packages, and extra packages from `dir` (metadata-only packages an operator adds, and
/// legacy apps' YAML). An embedded package wins over a directory copy of the same name.
pub fn base_catalog(dir: &Path) -> Result<Catalog, String> {
    let mut pkgs = ferrum_domain::packages().map_err(|e| format!("platform packages: {e}"))?;
    pkgs.extend(native_packages()?);
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

/// Upkeep under the advisory lock, held on one pooled connection for the round.
async fn locked_round(k: &Kernel) -> Result<(), sqlx::Error> {
    let mut conn = k.db.pool.acquire().await?;
    let got: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(LOCK).fetch_one(&mut *conn).await?;
    if !got {
        return Ok(());
    }
    maintenance(k).await;
    sqlx::query("select pg_advisory_unlock($1)").bind(LOCK).execute(&mut *conn).await?;
    Ok(())
}

pub fn spawn(k: Arc<Kernel>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = locked_round(&k).await {
                tracing::warn!("package maintenance: {e}");
            }
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
}
