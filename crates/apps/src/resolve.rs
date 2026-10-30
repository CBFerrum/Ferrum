//! Choosing what to install: the highest compatible versions, dependencies first.

use crate::AppError;
use crate::manifest::AppManifest;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

/// One available version of an app, as far as resolution is concerned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: String,
    pub version: Version,
    #[serde(default)]
    pub requires: BTreeMap<String, VersionReq>,
    pub platform: VersionReq,
}

impl From<&AppManifest> for Release {
    fn from(m: &AppManifest) -> Release {
        Release { id: m.id.clone(), version: m.version.clone(), requires: m.requires.clone(), platform: m.platform.clone() }
    }
}

/// Candidate choices tried before giving up (the search backtracks over older versions).
const MAX_STEPS: usize = 10_000;

fn fail(msg: String) -> AppError {
    AppError::Resolve(msg)
}

/// A requirement on an app and who has it.
#[derive(Debug, Clone)]
struct Need {
    req: VersionReq,
    by: String,
}

#[derive(Debug, Clone, Default)]
struct Plan {
    chosen: BTreeMap<String, Release>,
    needs: BTreeMap<String, Vec<Need>>,
    /// Apps to decide, in discovery order.
    queue: Vec<String>,
}

struct Solver<'a> {
    installed: &'a BTreeMap<String, Version>,
    available: &'a dyn Fn(&str) -> Vec<Release>,
    platform: &'a Version,
    requested: BTreeSet<String>,
    releases: RefCell<BTreeMap<String, Vec<Release>>>,
    steps: Cell<usize>,
}

impl Solver<'_> {
    /// Releases of `id`, newest first, one per version.
    fn releases(&self, id: &str) -> Vec<Release> {
        self.releases
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| {
                let mut list: Vec<Release> = (self.available)(id).into_iter().filter(|r| r.id == id).collect();
                list.sort_by(|a, b| b.version.cmp(&a.version));
                list.dedup_by(|a, b| a.version == b.version);
                list
            })
            .clone()
    }

    /// An installed app this plan leaves as it is.
    fn kept(&self, id: &str) -> Option<&Version> {
        if self.requested.contains(id) { None } else { self.installed.get(id) }
    }

    fn search(&self, plan: Plan) -> Result<Plan, AppError> {
        let Some(id) = plan.queue.iter().find(|id| !plan.chosen.contains_key(*id)).cloned() else {
            return Ok(plan);
        };
        let needs = plan.needs.get(&id).cloned().unwrap_or_default();
        let wanted = needs.iter().map(|n| format!("{} ({})", n.req, n.by)).collect::<Vec<_>>().join(", ");
        let all = self.releases(&id);
        if all.is_empty() {
            return Err(fail(format!("{id} is not available from the configured stores (needed: {wanted})")));
        }
        let matching: Vec<&Release> = all.iter().filter(|r| needs.iter().all(|n| n.req.matches(&r.version))).collect();
        if matching.is_empty() {
            let versions = all.iter().map(|r| r.version.to_string()).collect::<Vec<_>>().join(", ");
            return Err(fail(format!("no release of {id} satisfies {wanted} (available: {versions})")));
        }
        let fitting: Vec<&Release> = matching.iter().copied().filter(|r| r.platform.matches(self.platform)).collect();
        let Some(newest) = fitting.first() else {
            let r = matching[0];
            return Err(fail(format!(
                "{id} {} needs platform {}, but this server provides {} (and no older matching release fits)",
                r.version, r.platform, self.platform
            )));
        };
        let installed = self.installed.get(&id);
        let candidates: Vec<&Release> = fitting.iter().copied().filter(|r| installed.is_none_or(|v| r.version >= *v)).collect();
        if candidates.is_empty() {
            let v = installed.map(Version::to_string).unwrap_or_default();
            return Err(fail(format!("{id} {v} is installed; going back to {} is not supported", newest.version)));
        }
        let mut first_error = None;
        for release in candidates {
            self.steps.set(self.steps.get() + 1);
            if self.steps.get() > MAX_STEPS {
                return Err(fail("the requirements are too entangled to resolve; install the apps one at a time".into()));
            }
            match self.choose(&plan, &id, release).and_then(|next| self.search(next)) {
                Ok(done) => return Ok(done),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| fail(format!("no release of {id} can be installed"))))
    }

    /// `plan` with `release` chosen for `id`, or why its own requirements cannot hold.
    fn choose(&self, plan: &Plan, id: &str, release: &Release) -> Result<Plan, AppError> {
        let mut next = plan.clone();
        let by = format!("{id} {}", release.version);
        for (dep, req) in &release.requires {
            if let Some(other) = next.chosen.get(dep)
                && !req.matches(&other.version)
            {
                let others = next.needs.get(dep).map(|n| n.iter().map(|n| format!("{} ({})", n.req, n.by)).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                return Err(fail(format!("{by} requires {dep} {req}, but {dep} {} is already planned for {others}", other.version)));
            }
            if let Some(v) = self.kept(dep)
                && !req.matches(v)
            {
                return Err(fail(format!("{by} requires {dep} {req}, but {dep} {v} is installed; upgrade {dep} first")));
            }
            next.needs.entry(dep.clone()).or_default().push(Need { req: req.clone(), by: by.clone() });
            if self.kept(dep).is_none() && !next.queue.contains(dep) {
                next.queue.push(dep.clone());
            }
        }
        next.chosen.insert(id.to_string(), release.clone());
        Ok(next)
    }
}

/// `chosen` in install order (dependencies first, then by id), or the cycle that prevents one.
fn install_order(chosen: BTreeMap<String, Release>) -> Result<Vec<Release>, AppError> {
    let mut waiting: BTreeMap<String, BTreeSet<String>> =
        chosen.iter().map(|(id, r)| (id.clone(), r.requires.keys().filter(|d| chosen.contains_key(*d)).cloned().collect())).collect();
    let mut order = Vec::with_capacity(chosen.len());
    while !waiting.is_empty() {
        let ready: Vec<String> = waiting.iter().filter(|(_, deps)| deps.is_empty()).map(|(id, _)| id.clone()).collect();
        if ready.is_empty() {
            // Every app left waits for another one left: follow the first dependency around.
            let mut path: Vec<&str> = Vec::new();
            let mut at = waiting.keys().next().map(String::as_str).unwrap_or_default();
            while !path.contains(&at) {
                path.push(at);
                at = waiting.get(at).and_then(|d| d.iter().next()).map(String::as_str).unwrap_or(at);
            }
            let start = path.iter().position(|p| *p == at).unwrap_or(0);
            let cycle: Vec<String> = path[start..].iter().chain([&at]).map(|id| format!("{id} {}", chosen[*id].version)).collect();
            return Err(fail(format!("dependency cycle: {}", cycle.join(" -> "))));
        }
        for id in ready {
            waiting.remove(&id);
            for deps in waiting.values_mut() {
                deps.remove(&id);
            }
            order.push(chosen[&id].clone());
        }
    }
    Ok(order)
}

/// Picks the highest compatible version of each requested app and its dependencies; returns them
/// in install order (dependencies first). Errors name the conflict.
///
/// * Requested apps always resolve to their newest release that fits (an upgrade when they are
///   installed; the installed version itself when nothing newer fits — callers treat that as a
///   no-op). Going back to an older version is refused.
/// * Installed apps that were not requested are never changed: a requirement they do not meet
///   is an error ("upgrade it first"). Their own requirements, found by looking their installed
///   release up in `available`, keep applying to anything this plan installs or upgrades — so an
///   upgrade cannot break an installed app that depends on it. Include installed releases in
///   `available` (and native or platform apps in `installed`) for this protection.
/// * Releases whose `platform` does not match `platform` are skipped; when the newest matching
///   releases conflict, older ones are tried (bounded search).
/// * A dependency cycle among the releases to install is refused.
pub fn resolve(
    requested: &[(String, VersionReq)],
    installed: &BTreeMap<String, Version>,
    available: &dyn Fn(&str) -> Vec<Release>,
    platform: &Version,
) -> Result<Vec<Release>, AppError> {
    let solver = Solver {
        installed,
        available,
        platform,
        requested: requested.iter().map(|(id, _)| id.clone()).collect(),
        releases: RefCell::new(BTreeMap::new()),
        steps: Cell::new(0),
    };
    let mut plan = Plan::default();
    for (id, req) in requested {
        plan.needs.entry(id.clone()).or_default().push(Need { req: req.clone(), by: "requested".into() });
        if !plan.queue.contains(id) {
            plan.queue.push(id.clone());
        }
    }
    for (id, version) in installed {
        if solver.requested.contains(id) {
            continue;
        }
        if let Some(release) = solver.releases(id).into_iter().find(|r| &r.version == version) {
            for (dep, req) in &release.requires {
                plan.needs.entry(dep.clone()).or_default().push(Need { req: req.clone(), by: format!("{id} {version}, installed") });
            }
        }
    }
    install_order(solver.search(plan)?.chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(id: &str, version: &str, requires: &[(&str, &str)], platform: &str) -> Release {
        Release {
            id: id.into(),
            version: Version::parse(version).unwrap(),
            requires: requires.iter().map(|(d, r)| (d.to_string(), VersionReq::parse(r).unwrap())).collect(),
            platform: VersionReq::parse(platform).unwrap(),
        }
    }

    fn platform() -> Version {
        Version::parse(crate::PLATFORM_VERSION).unwrap()
    }

    fn run(store: &[Release], installed: &[(&str, &str)], requested: &[(&str, &str)]) -> Result<Vec<String>, String> {
        let available = |id: &str| store.iter().filter(|r| r.id == id).cloned().collect::<Vec<_>>();
        let installed: BTreeMap<String, Version> = installed.iter().map(|(id, v)| (id.to_string(), Version::parse(v).unwrap())).collect();
        let requested: Vec<(String, VersionReq)> = requested.iter().map(|(id, r)| (id.to_string(), VersionReq::parse(r).unwrap())).collect();
        resolve(&requested, &installed, &available, &platform())
            .map(|plan| plan.iter().map(|r| format!("{} {}", r.id, r.version)).collect())
            .map_err(|e| e.to_string())
    }

    const P: &str = ">=0.3, <0.4";

    #[test]
    fn chooses_the_highest_compatible_version() {
        let store = [rel("crm", "1.0.0", &[], P), rel("crm", "1.5.0", &[], P), rel("crm", "2.0.0", &[], ">=0.4"), rel("crm", "1.6.0-beta.1", &[], P)];
        assert_eq!(run(&store, &[], &[("crm", "*")]).unwrap(), ["crm 1.5.0"]);
        assert_eq!(run(&store, &[], &[("crm", "~1.0")]).unwrap(), ["crm 1.0.0"]);
        assert_eq!(run(&store, &[], &[("crm", ">=1.6.0-beta.1")]).unwrap(), ["crm 1.6.0-beta.1"], "pre-releases only on request");
    }

    #[test]
    fn dependencies_come_first() {
        let store = [
            rel("a", "1.0.0", &[("b", "^1")], P),
            rel("b", "1.2.0", &[("c", "^1"), ("d", "^1")], P),
            rel("c", "1.0.0", &[], P),
            rel("d", "1.0.0", &[("c", "^1")], P),
        ];
        assert_eq!(run(&store, &[], &[("a", "*")]).unwrap(), ["c 1.0.0", "d 1.0.0", "b 1.2.0", "a 1.0.0"]);
    }

    #[test]
    fn respects_installed_versions() {
        let store = [rel("a", "1.0.0", &[("b", "^1")], P), rel("b", "1.0.0", &[], P), rel("b", "1.9.0", &[], P), rel("b", "2.0.0", &[], P)];
        // b stays at the installed 1.0.0 (it satisfies ^1) and is not reinstalled.
        assert_eq!(run(&store, &[("b", "1.0.0")], &[("a", "*")]).unwrap(), ["a 1.0.0"]);
        // An installed version that does not fit is an error, not a silent upgrade.
        let err = run(&store, &[("b", "0.9.0")], &[("a", "*")]).unwrap_err();
        assert!(err.contains("a 1.0.0 requires b ^1, but b 0.9.0 is installed; upgrade b first"), "{err}");
        // Requested installed apps upgrade to the newest fitting release; never back.
        assert_eq!(run(&store, &[("b", "1.0.0")], &[("b", "*")]).unwrap(), ["b 2.0.0"]);
        let err = run(&store, &[("b", "2.0.0")], &[("b", "^1")]).unwrap_err();
        assert!(err.contains("b 2.0.0 is installed; going back to 1.9.0 is not supported"), "{err}");
    }

    #[test]
    fn upgrades_do_not_break_installed_dependents() {
        let store = [rel("crm", "1.0.0", &[], P), rel("crm", "2.0.0", &[], P), rel("helpdesk", "1.0.0", &[("crm", "^1")], P)];
        let err = run(&store, &[("crm", "1.0.0"), ("helpdesk", "1.0.0")], &[("crm", "^2")]).unwrap_err();
        assert!(err.contains("helpdesk 1.0.0, installed"), "{err}");
        // Without a version pin the newest release that keeps helpdesk working is chosen.
        assert_eq!(run(&store, &[("crm", "1.0.0"), ("helpdesk", "1.0.0")], &[("crm", "*")]).unwrap(), ["crm 1.0.0"]);
    }

    #[test]
    fn platform_incompatibility() {
        let store = [rel("iot", "1.0.0", &[], ">=0.4"), rel("iot", "0.9.0", &[], "<0.3")];
        let err = run(&store, &[], &[("iot", "*")]).unwrap_err();
        assert!(err.contains("iot 1.0.0 needs platform >=0.4, but this server provides 0.3.1"), "{err}");
        // A dependency that does not fit the platform stops its dependent.
        let store = [rel("app", "1.0.0", &[("iot", "*")], P), rel("iot", "1.0.0", &[], ">=0.4")];
        assert!(run(&store, &[], &[("app", "*")]).unwrap_err().contains("needs platform"));
    }

    #[test]
    fn missing_dependency() {
        let store = [rel("app", "1.0.0", &[("ghost", "^1")], P)];
        let err = run(&store, &[], &[("app", "*")]).unwrap_err();
        assert!(err.contains("ghost is not available") && err.contains("^1 (app 1.0.0)"), "{err}");
        assert!(run(&store, &[], &[("nothing", "*")]).unwrap_err().contains("nothing is not available"));
        let err = run(&store, &[], &[("app", "^2")]).unwrap_err();
        assert!(err.contains("no release of app satisfies ^2 (requested) (available: 1.0.0)"), "{err}");
    }

    #[test]
    fn version_conflict_names_both_sides() {
        let store = [rel("a", "1.0.0", &[("c", "^1")], P), rel("b", "1.0.0", &[("c", "^2")], P), rel("c", "1.0.0", &[], P), rel("c", "2.0.0", &[], P)];
        let err = run(&store, &[], &[("a", "*"), ("b", "*")]).unwrap_err();
        assert!(err.contains("a 1.0.0") && err.contains("b 1.0.0") && err.contains("c"), "{err}");
    }

    #[test]
    fn backtracks_to_older_versions() {
        // The newest b needs c ^2, which a cannot use; b 1.0.0 works with c ^1.
        let store = [
            rel("a", "1.0.0", &[("b", ">=1"), ("c", "^1")], P),
            rel("b", "1.0.0", &[("c", "^1")], P),
            rel("b", "2.0.0", &[("c", "^2")], P),
            rel("c", "1.4.0", &[], P),
            rel("c", "2.0.0", &[], P),
        ];
        assert_eq!(run(&store, &[], &[("a", "*")]).unwrap(), ["c 1.4.0", "b 1.0.0", "a 1.0.0"]);
    }

    #[test]
    fn cycles_are_refused() {
        let store = [rel("a", "1.0.0", &[("b", "^1")], P), rel("b", "1.0.0", &[("a", "^1")], P)];
        let err = run(&store, &[], &[("a", "*")]).unwrap_err();
        assert!(err.contains("dependency cycle: a 1.0.0 -> b 1.0.0 -> a 1.0.0"), "{err}");
        let selfish = [rel("me", "1.0.0", &[("me", "*")], P)];
        assert!(run(&selfish, &[], &[("me", "*")]).unwrap_err().contains("dependency cycle: me 1.0.0 -> me 1.0.0"));
        // A dependency already installed closes no cycle in the plan.
        assert_eq!(run(&store, &[("a", "1.0.0")], &[("b", "*")]).unwrap(), ["b 1.0.0"]);
    }

    #[test]
    fn duplicates_and_foreign_releases_are_ignored() {
        let store = [rel("a", "1.0.0", &[], P), rel("a", "1.0.0", &[], P)];
        let foreign = |_: &str| vec![rel("other", "9.0.0", &[], P)];
        assert_eq!(run(&store, &[], &[("a", "*")]).unwrap(), ["a 1.0.0"]);
        let err = resolve(&[("a".into(), VersionReq::STAR)], &BTreeMap::new(), &foreign, &platform()).unwrap_err();
        assert!(err.to_string().contains("a is not available"));
    }

    #[test]
    fn release_from_manifest() {
        let m = AppManifest::parse("id: kit\nname: Kit\nversion: 1.2.3\npublisher: acme\nsummary: Kit\ncategories: [industry]\nruntime: meta\nplatform: '^0.3'\nrequires: {crm: '^1'}\npackages: [kit]\n").unwrap();
        let r = Release::from(&m);
        assert_eq!((r.id.as_str(), r.version.to_string(), r.requires.len()), ("kit", "1.2.3".to_string(), 1));
    }
}
