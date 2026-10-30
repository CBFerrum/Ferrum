//! Embeds the platform's metadata packages (`packages/<name>/...`) so the binary carries them and
//! apps built in other repositories get them from this crate.

use std::fmt::Write;
use std::path::{Path, PathBuf};

const PACKAGES: &[&str] = &["core", "contacts", "parties", "products", "accounting", "buying", "selling", "inventory", "manufacturing", "india"];

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if e.path().is_dir() {
            walk(&e.path(), &rel, out);
        } else {
            out.push((rel, e.path()));
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    let mut code = String::from("// (package, path inside the package, content)\npub static EMBEDDED: &[(&str, &str, &str)] = &[\n");
    for pkg in PACKAGES {
        let dir = root.join(pkg);
        // Relative to the crate, like the paths below: checkouts at other paths can share a target
        // directory (Cargo hashes path crates relative to their workspace) without mixing up files.
        println!("cargo:rerun-if-changed=../../packages/{pkg}");
        let mut files = Vec::new();
        walk(&dir, "", &mut files);
        assert!(files.iter().any(|(r, _)| r == "package.yaml"), "{} has no package.yaml", dir.display());
        for (rel, _) in files {
            let file = format!("/../../packages/{pkg}/{rel}");
            writeln!(code, "    ({pkg:?}, {rel:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {file:?}))),").expect("write");
        }
    }
    code.push_str("];\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("packages.rs"), code).expect("write packages.rs");
}
