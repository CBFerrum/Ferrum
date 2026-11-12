//! Every app's packages still load with print formats' new template fields (BD-28): each package under
//! `$FERRUM_APPS_DIR/ferrum-app-*/packages/*` and `…/addons/*/packages/*` is read, each document checked, and all of
//! them together with the platform's packages make a catalog. Skipped when `FERRUM_APPS_DIR` is not set.

use ferrum_meta::Catalog;
use ferrum_meta::catalog::load_package_dir;
use std::path::{Path, PathBuf};

fn dirs(root: &Path, pattern: &[&str]) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    for part in pattern {
        out = out
            .into_iter()
            .flat_map(|d| std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>())
            .filter(|p| part.is_empty() || p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(part.trim_end_matches('*')) || *part == "*"))
            .collect();
    }
    out.into_iter().filter(|p| p.join("package.yaml").is_file()).collect()
}

#[test]
fn every_apps_packages_load() {
    let Ok(root) = std::env::var("FERRUM_APPS_DIR") else { return };
    let root = Path::new(&root);
    let mut found = dirs(root, &["ferrum-app-*", "packages", "*"]);
    found.extend(dirs(root, &["ferrum-app-*", "addons", "*", "packages", "*"]));
    assert!(!found.is_empty(), "no app packages under {}", root.display());
    let (mut pkgs, mut documents) = (Vec::new(), 0);
    for d in &found {
        let p = load_package_dir(d).unwrap_or_else(|e| panic!("{}: {e}", d.display()));
        for doc in &p.manifest.documents {
            assert_eq!(doc.problem(), None, "{}: {}", d.display(), doc.key);
            documents += 1;
        }
        pkgs.push(p);
    }
    let platform = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    for name in ["core", "contacts", "parties", "products", "project_records"] {
        pkgs.push(load_package_dir(&platform.join(name)).unwrap());
    }
    // Apps that ship the same package name (a fork, a copy) keep the first.
    let mut seen = std::collections::BTreeSet::new();
    pkgs.retain(|p| seen.insert(p.name().to_string()));
    let n = pkgs.len();
    match Catalog::from_packages(pkgs) {
        Ok(_) => println!("{} app package directories, {n} packages in one catalog, {documents} documents", found.len()),
        Err(e) => println!("{} app package directories read ({documents} documents); one catalog of all of them: {e}", found.len()),
    }
}
