//! Packs an app directory into an unsigned `.fapp` bundle, checked as a server would check it:
//! `cargo run -p ferrum-apps --example pack -- <app dir> <out.fapp>`. For platform tests and spikes;
//! apps use `ferrum-app pack` from ferrum-sdk.

use std::collections::BTreeMap;
use std::path::Path;

fn files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        if e.file_type()?.is_dir() {
            files(root, &path, out)?;
        } else {
            let rel = path.strip_prefix(root).expect("under root").to_string_lossy().replace('\\', "/");
            out.insert(rel, std::fs::read(&path)?);
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(dir), Some(out)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: pack <app dir> <out.fapp>");
        std::process::exit(2);
    };
    let mut all = BTreeMap::new();
    if let Err(e) = files(Path::new(dir), Path::new(dir), &mut all) {
        eprintln!("{dir}: {e}");
        std::process::exit(1);
    }
    let bundle = match ferrum_apps::Bundle::from_files(all) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if let Some(ui) = &bundle.manifest.ui {
        let prefix = format!("{}/", ui.dist.trim_end_matches('/'));
        let dist: BTreeMap<String, Vec<u8>> =
            bundle.paths().filter_map(|p| p.strip_prefix(prefix.as_str()).map(|r| (r.to_string(), bundle.file(p).unwrap_or_default().to_vec()))).collect();
        if let Err(e) = ferrum_apps::ui::prepare(&bundle.manifest.id, dist) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    let bytes = bundle.write().expect("archive");
    std::fs::write(out, bytes).expect("write");
    println!("{} {} {}", bundle.manifest.id, bundle.manifest.version, bundle.digest());
}
