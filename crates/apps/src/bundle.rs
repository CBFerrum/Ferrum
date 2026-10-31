//! The `.fapp` bundle: reading (with every limit and path rule), writing, digest and contents.

use crate::manifest::{AppManifest, Runtime};
use crate::permissions::PluginManifest;
use crate::signing::Signatures;
use crate::{AppError, FORMAT, MAX_FILES, MAX_TEXT, MAX_UI_BINARY, MAX_UNPACKED, MAX_WASM};
use ferrum_meta::{OverlayOp, Package};
use flate2::{Compression, GzBuilder};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;

pub(crate) const MANIFEST_FILE: &str = "app.yaml";
pub(crate) const PLUGIN_JSON: &str = "plugin.json";
pub(crate) const PLUGIN_WASM: &str = "plugin.wasm";
pub(crate) const SIGNATURES_FILE: &str = "SIGNATURES.json";
const DOCS: &[&str] = &["README.md", "CHANGELOG.md", "LICENSE"];
const IMAGES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg"];
/// Files of screens under `ui/`: text, then binary.
const UI_TEXT: &[&str] = &["html", "js", "mjs", "css", "json", "map", "txt", "svg"];
const UI_BINARY: &[&str] = &["wasm", "woff2", "png", "jpg", "jpeg", "gif", "webp"];
const MAX_PATH: usize = 255;
/// Room for tar headers, long-name records and padding on top of the content.
const TAR_OVERHEAD: u64 = 8 << 20;
/// Archive entries of any kind: every content file plus `SIGNATURES.json`.
const MAX_ENTRIES: usize = MAX_FILES + 1;
/// A binary core WebAssembly module, version 1.
const WASM_MAGIC: &[u8] = b"\0asm\x01\0\0\0";

/// An app release as a file: the manifest, the files it holds and its signatures.
///
/// Every constructor enforces the whole format, so a `Bundle` value is always valid. `manifest`
/// is the parsed `app.yaml` and `files` never contain `SIGNATURES.json` (it is `signatures`).
#[derive(Clone, PartialEq)]
pub struct Bundle {
    pub manifest: AppManifest,
    files: BTreeMap<String, Vec<u8>>,
    pub signatures: Option<Signatures>,
    digest: String,
}

impl fmt::Debug for Bundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bundle")
            .field("id", &self.manifest.id)
            .field("version", &self.manifest.version.to_string())
            .field("files", &self.files.len())
            .field("digest", &self.digest)
            .field("signatures", &self.signatures.as_ref().map(|s| s.signatures.len()))
            .finish()
    }
}

fn bad<T>(msg: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Bundle(msg.into()))
}

fn unreadable(e: std::io::Error) -> AppError {
    AppError::Bundle(format!("not a readable tar archive: {e}"))
}

/// Path rules shared by archive entries and the paths `app.yaml` refers to.
pub(crate) fn check_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if path.starts_with('/') {
        return Err(format!("{path}: absolute paths are not allowed"));
    }
    if path.len() > MAX_PATH {
        let start: String = path.chars().take(40).collect();
        return Err(format!("{start}...: paths are limited to {MAX_PATH} bytes"));
    }
    for part in path.split('/') {
        if part == ".." {
            return Err(format!("{path}: '..' is not allowed in paths"));
        }
        if part.is_empty() || part == "." {
            return Err(format!("{path}: empty or '.' path components are not allowed"));
        }
        if part.starts_with('.') {
            return Err(format!("{path}: hidden files are not allowed"));
        }
        if !part.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            return Err(format!("{path}: use only letters, digits, '.', '_', '-' and '/' in paths"));
        }
    }
    Ok(())
}

fn image_ext(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    IMAGES.iter().copied().find(|e| *e == ext)
}

/// An image under `assets/` (what `icon` and `screenshots` may name).
pub(crate) fn is_image_path(path: &str) -> bool {
    path.starts_with("assets/") && image_ext(path).is_some()
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Wasm,
    Image(&'static str),
    /// A file of a screen, by extension.
    Screen(&'static str),
}

fn is_package_file(file: &str) -> bool {
    let one_level = |dir: &str, ext: &str| file.strip_prefix(dir).is_some_and(|n| !n.contains('/') && n.len() > ext.len() && n.ends_with(ext));
    file == "package.yaml" || file == "README.md" || one_level("entities/", ".yaml") || one_level("sql/", ".sql") || one_level("print/", ".html")
}

/// What a path may hold, or why it has no place in a bundle.
fn kind(path: &str) -> Result<Kind, AppError> {
    if path == PLUGIN_WASM {
        return Ok(Kind::Wasm);
    }
    if [MANIFEST_FILE, PLUGIN_JSON, SIGNATURES_FILE].contains(&path) || DOCS.contains(&path) {
        return Ok(Kind::Text);
    }
    if let Some((name, file)) = path.strip_prefix("packages/").and_then(|rest| rest.split_once('/'))
        && ferrum_meta::is_package_name(name)
        && is_package_file(file)
    {
        return Ok(Kind::Text);
    }
    if path.starts_with("ui/") {
        let ext = path.rsplit('/').next().and_then(|n| n.rsplit_once('.')).filter(|(stem, _)| !stem.is_empty()).map_or("", |(_, e)| e);
        return UI_TEXT
            .iter()
            .chain(UI_BINARY)
            .copied()
            .find(|e| *e == ext)
            .map(Kind::Screen)
            .ok_or_else(|| AppError::Bundle(format!("{path}: ui/ holds {} and {} files", UI_TEXT.join(", "), UI_BINARY.join(", "))));
    }
    if path.starts_with("assets/") {
        return image_ext(path)
            .map(Kind::Image)
            .ok_or_else(|| AppError::Bundle(format!("{path}: assets/ holds only png, jpg, jpeg, gif, webp and svg images")));
    }
    bad(format!(
        "unexpected file {path}: a bundle holds app.yaml, README.md, CHANGELOG.md, LICENSE, plugin.json, plugin.wasm, \
         packages/<name>/ (package.yaml, README.md, entities/*.yaml, sql/*.sql, print/*.html), images under assets/ and screens under ui/"
    ))
}

fn image_ok(ext: &str, data: &[u8]) -> bool {
    match ext {
        "png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" | "jpeg" => data.starts_with(&[0xff, 0xd8, 0xff]),
        "gif" => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
        "webp" => data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP",
        "svg" => data.len() <= MAX_TEXT && std::str::from_utf8(data).is_ok_and(|t| t.contains("<svg")),
        _ => false,
    }
}

/// Size, encoding and format of one file.
fn check_file(path: &str, data: &[u8]) -> Result<(), AppError> {
    match kind(path)? {
        Kind::Wasm => {
            if data.len() > MAX_WASM {
                return Err(AppError::Plugin(format!("plugin.wasm is larger than {} MiB", MAX_WASM >> 20)));
            }
            if !data.starts_with(WASM_MAGIC) {
                return Err(AppError::Plugin("plugin.wasm is not a binary WebAssembly module (\\0asm, version 1)".into()));
            }
        }
        Kind::Image(ext) => {
            if !image_ok(ext, data) {
                return bad(format!("{path} is not a valid {ext} image"));
            }
        }
        Kind::Screen(ext) => {
            if data.len() > MAX_UI_BINARY {
                return bad(format!("{path} is larger than {} MiB, the limit for files of screens", MAX_UI_BINARY >> 20));
            }
            let ok = match ext {
                "wasm" => data.starts_with(WASM_MAGIC),
                "woff2" => data.starts_with(b"wOF2"),
                "svg" => std::str::from_utf8(data).is_ok_and(|t| t.contains("<svg")),
                e if UI_TEXT.contains(&e) => std::str::from_utf8(data).is_ok(),
                e => image_ok(e, data),
            };
            if !ok {
                return bad(format!("{path} is not a valid {ext} file"));
            }
        }
        Kind::Text => {
            if data.len() > MAX_TEXT {
                return bad(format!("{path} is larger than {} MiB, the limit for text files", MAX_TEXT >> 20));
            }
            if std::str::from_utf8(data).is_err() {
                return bad(format!("{path} is not UTF-8 text"));
            }
        }
    }
    Ok(())
}

/// Decompresses one gzip member, refusing more than the bundle limit or trailing data.
fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, AppError> {
    let limit = MAX_UNPACKED + TAR_OVERHEAD;
    let mut decoder = flate2::bufread::GzDecoder::new(bytes);
    let mut tar = Vec::new();
    (&mut decoder).take(limit + 1).read_to_end(&mut tar).map_err(|e| AppError::Bundle(format!("not a gzip-compressed bundle: {e}")))?;
    if tar.len() as u64 > limit {
        return bad(format!("larger than {} MiB unpacked", MAX_UNPACKED >> 20));
    }
    if !decoder.into_inner().is_empty() {
        return bad("unexpected data after the compressed archive");
    }
    Ok(tar)
}

/// `X` followed by a capital letter or digit: `prefix` is the start of a name, not of a word.
pub(crate) fn namespaced(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).and_then(|rest| rest.chars().next()).is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn package_error<T>(pkg: &Package, message: String) -> Result<T, AppError> {
    Err(AppError::Package { package: pkg.name().to_string(), message })
}

/// Role rules for every app, and the namespace rules for third-party (wasm and meta) apps.
/// Several packages may name the same role (each adds grants for its own entities), as the
/// platform's packages do.
fn check_namespaces(m: &AppManifest, packages: &[Package]) -> Result<(), AppError> {
    for pkg in packages {
        for role in &pkg.manifest.roles {
            if role.name.starts_with("plugin:") {
                return package_error(pkg, format!("role '{}': names starting with 'plugin:' are reserved for plugins' service users", role.name));
            }
        }
    }
    if m.runtime == Runtime::Native {
        return Ok(());
    }
    let prefix = m.prefix.as_deref();
    let field_prefix = prefix.map(|p| format!("{}_", ferrum_meta::snake(p)));
    for pkg in packages {
        for e in &pkg.entities {
            match prefix {
                None => return package_error(pkg, format!("entity {} needs a namespace: set prefix in app.yaml and start entity names with it", e.name)),
                Some(p) if !namespaced(&e.name, p) => {
                    return package_error(
                        pkg,
                        format!("entity {} must start with the app prefix {p} followed by a capital letter or digit (such as {p}Record)", e.name),
                    );
                }
                Some(_) => {}
            }
        }
        if let Some(fp) = &pkg.manifest.field_prefix
            && Some(fp) != field_prefix.as_ref()
        {
            return package_error(
                pkg,
                match &field_prefix {
                    Some(want) => format!("field_prefix must be {want} (the app prefix in snake_case)"),
                    None => "field_prefix needs the app prefix: set prefix in app.yaml".into(),
                },
            );
        }
        for ov in &pkg.manifest.overlays {
            for op in &ov.ops {
                if let OverlayOp::AddField { field, .. } = op
                    && !field_prefix.as_deref().is_some_and(|fp| field.name.starts_with(fp))
                {
                    let want = field_prefix.as_deref().unwrap_or("the app prefix in snake_case and '_' (set prefix in app.yaml)");
                    return package_error(pkg, format!("field {} added to {} must start with {want}", field.name, ov.entity));
                }
            }
        }
        for role in &pkg.manifest.roles {
            if role.permissions.iter().any(|p| p.entity == "*") {
                return package_error(pkg, format!("role '{}' grants access to every entity ('*'); third-party apps must name the entities", role.name));
            }
        }
    }
    Ok(())
}

impl Bundle {
    /// Reads a `.fapp` (gzip-compressed tar), enforcing every limit, path and layout rule, and
    /// validates the manifest, the packages and the plugin manifest.
    pub fn read(bytes: &[u8]) -> Result<Bundle, AppError> {
        let tar = gunzip(bytes)?;
        let mut archive = tar::Archive::new(&tar[..]);
        let mut files = BTreeMap::new();
        for (n, entry) in archive.entries().map_err(unreadable)?.enumerate() {
            if n >= MAX_ENTRIES {
                return bad(format!("more than {MAX_FILES} files"));
            }
            let mut entry = entry.map_err(unreadable)?;
            let path = String::from_utf8(entry.path_bytes().into_owned()).map_err(|_| AppError::Bundle("a file name is not UTF-8".into()))?;
            match entry.header().entry_type() {
                tar::EntryType::Regular => {}
                tar::EntryType::Symlink | tar::EntryType::Link => return bad(format!("{path} is a link; bundles hold only regular files")),
                tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo => {
                    return bad(format!("{path} is a device or pipe; bundles hold only regular files"));
                }
                tar::EntryType::Directory => {
                    return bad(format!("{path} is a directory entry; bundles hold only regular files (build them with ferrum-app pack)"));
                }
                other => return bad(format!("{path} has entry type {other:?}; bundles hold only regular files")),
            }
            check_path(&path).map_err(AppError::Bundle)?;
            let size = entry.size();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(unreadable)?;
            if data.len() as u64 != size {
                return bad(format!("the archive is truncated (in {path})"));
            }
            if files.insert(path.clone(), data).is_some() {
                return bad(format!("{path} appears twice in the archive"));
            }
        }
        if archive.into_inner().iter().any(|b| *b != 0) {
            return bad("unexpected data after the end of the archive");
        }
        Bundle::build(files)
    }

    /// The same checks as [`Bundle::read`], for files already unpacked (paths relative to the
    /// bundle root, `/`-separated). A `SIGNATURES.json` entry becomes `signatures`.
    pub fn from_files(files: BTreeMap<String, Vec<u8>>) -> Result<Bundle, AppError> {
        Bundle::build(files)
    }

    fn build(mut files: BTreeMap<String, Vec<u8>>) -> Result<Bundle, AppError> {
        let signatures = match files.remove(SIGNATURES_FILE) {
            Some(raw) => {
                check_file(SIGNATURES_FILE, &raw)?;
                Some(Signatures::parse(&raw)?)
            }
            None => None,
        };
        if files.len() > MAX_FILES {
            return bad(format!("{} files; the limit is {MAX_FILES}", files.len()));
        }
        let mut total = 0u64;
        let mut folded = BTreeSet::new();
        for (path, data) in &files {
            check_path(path).map_err(AppError::Bundle)?;
            if !folded.insert(path.to_ascii_lowercase()) {
                return bad(format!("{path} differs from another file only in letter case"));
            }
            check_file(path, data)?;
            total += data.len() as u64;
        }
        if total > MAX_UNPACKED {
            return bad(format!("larger than {} MiB unpacked", MAX_UNPACKED >> 20));
        }
        for path in files.keys() {
            if let Some((i, _)) = path.match_indices('/').find(|(i, _)| files.contains_key(&path[..*i])) {
                return bad(format!("{} is a file, so {path} cannot be inside it", &path[..i]));
            }
        }
        let yaml = files.get(MANIFEST_FILE).ok_or_else(|| AppError::Bundle("app.yaml is missing".into()))?;
        let manifest = AppManifest::parse(std::str::from_utf8(yaml).map_err(|_| AppError::Bundle("app.yaml is not UTF-8 text".into()))?)?;
        let mut bundle = Bundle { manifest, files, signatures, digest: String::new() };
        bundle.check_contents()?;
        bundle.digest = bundle.compute_digest();
        Ok(bundle)
    }

    /// Cross-checks the manifest against the files.
    fn check_contents(&self) -> Result<(), AppError> {
        let m = &self.manifest;
        let has = |p: &str| self.files.contains_key(p);
        for p in m.description.iter().chain(&m.icon).chain(&m.screenshots) {
            if !has(p) {
                return bad(format!("app.yaml refers to {p}, which is not in the bundle"));
            }
        }
        match m.runtime {
            Runtime::Wasm => {
                if !has(PLUGIN_JSON) {
                    return Err(AppError::Plugin("plugin.json is missing (wasm apps ship their plugin manifest as plugin.json)".into()));
                }
                if !has(PLUGIN_WASM) {
                    return Err(AppError::Plugin("plugin.wasm is missing (build the plugin and ship the module as plugin.wasm)".into()));
                }
            }
            rt => {
                if let Some(p) = [PLUGIN_JSON, PLUGIN_WASM].into_iter().find(|p| has(p)) {
                    return Err(AppError::Plugin(format!("{p} found, but {rt} apps carry no plugin code")));
                }
            }
        }
        // A native app's own interface (`ui.dist`, built) may ride in its bundle for the store to
        // review; its server embeds its own copy. Nothing else of ui/: screens are wasm and meta
        // apps', and an interface's sources stay out.
        if m.runtime == Runtime::Native {
            let dist = m.ui.as_ref().map(|u| format!("{}/", u.dist.trim_end_matches('/')));
            if let Some(p) = self.files.keys().find(|p| p.starts_with("ui/") && !dist.as_deref().is_some_and(|d| p.starts_with(d))) {
                return bad(format!("{p}: native apps carry only their built interface under ui/ (ui.dist), not screens or sources"));
            }
        }
        if let Some(e) = m.screen_entries().into_iter().find(|e| !has(e)) {
            return bad(format!("app.yaml opens {e}, which is not in the bundle"));
        }
        let dirs: BTreeSet<&str> = self.files.keys().filter_map(|p| Some(p.strip_prefix("packages/")?.split_once('/')?.0)).collect();
        if let Some(dir) = dirs.iter().find(|d| !m.packages.iter().any(|p| p == *d)) {
            return bad(format!("packages/{dir}/ is not listed in app.yaml (packages)"));
        }
        for p in &m.packages {
            if !has(&format!("packages/{p}/package.yaml")) {
                return Err(AppError::Package { package: p.clone(), message: format!("listed in app.yaml, but packages/{p}/package.yaml is missing") });
            }
        }
        if m.runtime == Runtime::Meta
            && let Some(sql) = self.files.keys().find(|p| p.starts_with("packages/") && p.contains("/sql/"))
        {
            return bad(format!("{sql}: meta apps carry no code and no SQL"));
        }
        let packages = self.packages()?;
        check_namespaces(m, &packages)?;
        if m.runtime == Runtime::Wasm {
            PluginManifest::parse(self.file(PLUGIN_JSON).unwrap_or_default())?.validate(m)?;
        }
        Ok(())
    }

    /// The deterministic `.fapp`: sorted paths, zero mtimes, owners and fixed modes, with
    /// `SIGNATURES.json` when the bundle is signed. Identical bundles give identical bytes.
    pub fn write(&self) -> Result<Vec<u8>, AppError> {
        let io = |e: std::io::Error| AppError::Bundle(format!("cannot write the archive: {e}"));
        let signatures = match &self.signatures {
            Some(s) => {
                let mut json = serde_json::to_vec_pretty(s).map_err(|e| AppError::Bundle(format!("cannot write SIGNATURES.json: {e}")))?;
                json.push(b'\n');
                Some(json)
            }
            None => None,
        };
        let mut entries: Vec<(&str, &[u8])> = self.files.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
        if let Some(json) = &signatures {
            entries.push((SIGNATURES_FILE, json));
            entries.sort_by(|a, b| a.0.cmp(b.0));
        }
        let mut tar = tar::Builder::new(GzBuilder::new().write(Vec::new(), Compression::best()));
        for (path, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_size(data.len() as u64);
            tar.append_data(&mut header, path, data).map_err(io)?;
        }
        tar.into_inner().map_err(io)?.finish().map_err(io)
    }

    /// Lower-case hex SHA-256 of the content, as the format defines it (crate docs).
    pub fn digest(&self) -> String {
        self.digest.clone()
    }

    fn compute_digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(FORMAT.as_bytes());
        h.update(b"\n");
        for (path, data) in &self.files {
            h.update(path.as_bytes());
            h.update([0u8]);
            h.update(hex::encode(Sha256::digest(data)).as_bytes());
            h.update(b"\n");
        }
        hex::encode(h.finalize())
    }

    /// A file's content (`SIGNATURES.json` is [`Bundle::signatures`], not a file).
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    /// Every file path, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The packages the manifest lists, parsed (in manifest order).
    pub fn packages(&self) -> Result<Vec<Package>, AppError> {
        self.manifest
            .packages
            .iter()
            .map(|name| {
                let pkg = ferrum_meta::catalog::load_package_files(&self.package_texts(name))
                    .map_err(|e| AppError::Package { package: name.clone(), message: e.to_string() })?;
                if pkg.name() != name {
                    return package_error(
                        &pkg,
                        format!("packages/{name}/package.yaml declares package '{}'; the directory must have the package's name", pkg.name()),
                    );
                }
                Ok(pkg)
            })
            .collect()
    }

    /// Package name → (path inside the package → text), as the database stores them.
    pub fn package_files(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.manifest.packages.iter().map(|name| (name.clone(), self.package_texts(name))).collect()
    }

    fn package_texts(&self, name: &str) -> BTreeMap<String, String> {
        let dir = format!("packages/{name}/");
        self.files
            .range(dir.clone()..)
            .take_while(|(p, _)| p.starts_with(&dir))
            .filter_map(|(p, d)| Some((p[dir.len()..].to_string(), String::from_utf8(d.clone()).ok()?)))
            .collect()
    }

    /// For wasm apps: the module and `plugin.json` (the server installs them as the app's plugin).
    pub fn plugin(&self) -> Result<Option<(Vec<u8>, serde_json::Value)>, AppError> {
        if self.manifest.runtime != Runtime::Wasm {
            return Ok(None);
        }
        let wasm = self.file(PLUGIN_WASM).ok_or_else(|| AppError::Plugin("plugin.wasm is missing".into()))?;
        let json = self.file(PLUGIN_JSON).ok_or_else(|| AppError::Plugin("plugin.json is missing".into()))?;
        let json = serde_json::from_slice(json).map_err(|e| AppError::Plugin(format!("plugin.json: {e}")))?;
        Ok(Some((wasm.to_vec(), json)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use tar::EntryType;

    #[test]
    fn screens_under_ui() {
        let mut files = meta_app();
        let yaml = String::from_utf8(files["app.yaml"].clone()).unwrap() + "contributes:\n  ui.page:\n    - {id: board, title: Board, entry: ui/board.html}\n";
        files.insert("app.yaml".into(), yaml.into_bytes());
        refused(files.clone(), "app.yaml opens ui/board.html, which is not in the bundle");
        files.insert("ui/board.html".into(), b"<!doctype html><script src=app.js></script>".to_vec());
        files.insert("ui/app.js".into(), b"console.log(1)".to_vec());
        files.insert("ui/editor.wasm".into(), WASM_MAGIC.to_vec());
        files.insert("ui/fonts/inter.woff2".into(), b"wOF2....".to_vec());
        files.insert("ui/logo.svg".into(), b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec());
        let b = Bundle::from_files(files.clone()).unwrap();
        assert_eq!(Bundle::read(&b.write().unwrap()).unwrap(), b);
        assert_eq!(b.file("ui/editor.wasm"), Some(WASM_MAGIC));
        let big = [WASM_MAGIC, &vec![0; MAX_UI_BINARY]].concat();
        for (path, data, needle) in [
            ("ui/setup.exe", b"MZ".to_vec(), "ui/ holds html"),
            ("ui/.hidden.js", b"1".to_vec(), "hidden files are not allowed"),
            ("ui/bad.wasm", b"nope".to_vec(), "ui/bad.wasm is not a valid wasm file"),
            ("ui/bad.js", vec![0xff, 0xfe], "ui/bad.js is not a valid js file"),
            ("ui/big.wasm", big, "larger than 16 MiB"),
        ] {
            let mut f = files.clone();
            f.insert(path.into(), data);
            refused(f, needle);
        }
        let mut native = files.clone();
        let yaml = String::from_utf8(native["app.yaml"].clone())
            .unwrap()
            .replace("runtime: meta", "runtime: native")
            .replace("contributes:\n  ui.page:\n    - {id: board, title: Board, entry: ui/board.html}\n", "");
        native.insert("app.yaml".into(), yaml.clone().into_bytes());
        refused(native.clone(), "native apps carry only their built interface under ui/");
        // With an interface of its own, its built files come along, and nothing else of ui/.
        let own = format!("{yaml}ui:\n  dist: ui/dist\ncontributes:\n  shell.app:\n    - {{id: board, route: board, title: Board}}\n");
        let mut built: BTreeMap<String, Vec<u8>> = native.into_iter().filter(|(p, _)| !p.starts_with("ui/")).collect();
        built.insert("app.yaml".into(), own.into_bytes());
        built.insert("ui/dist/index.html".into(), b"<!doctype html><html><head><title>Board</title></head><body></body></html>".to_vec());
        built.insert("ui/dist/assets/index-a1.js".into(), b"export {};".to_vec());
        Bundle::from_files(built.clone()).expect("a native app with its built interface");
        let mut sources = built;
        sources.insert("ui/src/main.js".into(), b"export {};".to_vec());
        refused(sources, "ui/src/main.js: native apps carry only their built interface");
    }

    fn refused(files: BTreeMap<String, Vec<u8>>, needle: &str) {
        let err = Bundle::from_files(files).expect_err(&format!("should be refused: {needle}")).to_string();
        assert!(err.contains(needle), "expected '{needle}' in: {err}");
    }

    fn refused_archive(bytes: &[u8], needle: &str) {
        let err = Bundle::read(bytes).expect_err(&format!("should be refused: {needle}")).to_string();
        assert!(err.contains(needle), "expected '{needle}' in: {err}");
    }

    #[test]
    fn round_trip_is_lossless_and_deterministic() {
        for files in [meta_app(), wasm_app()] {
            let b = Bundle::from_files(files.clone()).unwrap();
            let bytes = b.write().unwrap();
            let back = Bundle::read(&bytes).unwrap();
            assert_eq!(back, b);
            assert_eq!(back.write().unwrap(), bytes, "writing is deterministic");
            assert_eq!(b.paths().collect::<Vec<_>>(), files.keys().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(b.file("app.yaml").unwrap(), files["app.yaml"].as_slice());
        }
    }

    #[test]
    fn digest_follows_the_specification() {
        let files = meta_app();
        let mut text = String::from("ferrum-app-v1\n");
        for (p, d) in &files {
            text.push_str(&format!("{p}\0{}\n", hex::encode(Sha256::digest(d))));
        }
        let b = Bundle::from_files(files).unwrap();
        assert_eq!(b.digest(), hex::encode(Sha256::digest(text.as_bytes())));
        assert_eq!(b.digest().len(), 64);
    }

    #[test]
    fn digest_ignores_tar_order_timestamps_and_signatures() {
        let files = meta_app();
        let expected = Bundle::from_files(files.clone()).unwrap().digest();
        // Reverse order, real timestamps, uid/gid, other modes.
        let mut entries: Vec<RawEntry> = files.iter().map(|(p, d)| RawEntry::file(p, d)).collect();
        entries.reverse();
        for (i, e) in entries.iter_mut().enumerate() {
            e.mtime = 1_700_000_000 + i as u64;
            e.mode = 0o600;
        }
        let shuffled = Bundle::read(&gz(&raw_tar(&entries))).unwrap();
        assert_eq!(shuffled.digest(), expected);
        // Signing adds SIGNATURES.json without changing the digest.
        let mut signed = shuffled.clone();
        signed.sign(&signer(), crate::Role::Publisher).unwrap();
        assert_eq!(Bundle::read(&signed.write().unwrap()).unwrap().digest(), expected);
        // Different content, different digest.
        let mut other = files.clone();
        other.insert("README.md".into(), b"# Kit 2\n".to_vec());
        assert_ne!(Bundle::from_files(other).unwrap().digest(), expected);
    }

    #[test]
    fn path_attacks_are_refused() {
        let base = meta_app();
        for (path, needle) in [
            ("../evil.md", "'..'"),
            ("packages/kit/../../etc/passwd", "'..'"),
            ("/etc/passwd", "absolute"),
            ("assets//icon.png", "empty or '.'"),
            ("assets/./icon.png", "empty or '.'"),
            ("assets/.hidden.png", "hidden"),
            ("assets\\icon.png", "only letters"),
            ("C:/icon.png", "only letters"),
            ("assets/ icon.png", "only letters"),
            ("assets/ïcon.png", "only letters"),
        ] {
            let mut files = base.clone();
            files.insert(path.into(), PNG.to_vec());
            refused(files, needle);
        }
        let mut files = base.clone();
        files.insert(format!("assets/{}.png", "a".repeat(250)), PNG.to_vec());
        refused(files, "limited to 255 bytes");
        // Archive entries go through the same rules.
        let mut entries: Vec<RawEntry> = base.iter().map(|(p, d)| RawEntry::file(p, d)).collect();
        entries.push(RawEntry::file("../escape.md", b"x"));
        refused_archive(&gz(&raw_tar(&entries)), "'..'");
    }

    #[test]
    fn only_regular_files() {
        let app = meta_app();
        let base: Vec<RawEntry> = app.iter().map(|(p, d)| RawEntry::file(p, d)).collect();
        for (entry, needle) in [
            (RawEntry { kind: EntryType::Symlink, link: Some("/etc/passwd"), ..RawEntry::file("assets/link.png", b"") }, "is a link"),
            (RawEntry { kind: EntryType::Link, link: Some("app.yaml"), ..RawEntry::file("assets/hard.png", b"") }, "is a link"),
            (RawEntry { kind: EntryType::Char, ..RawEntry::file("assets/tty.png", b"") }, "device"),
            (RawEntry { kind: EntryType::Block, ..RawEntry::file("assets/disk.png", b"") }, "device"),
            (RawEntry { kind: EntryType::Fifo, ..RawEntry::file("assets/pipe.png", b"") }, "device or pipe"),
            (RawEntry { kind: EntryType::Directory, ..RawEntry::file("assets/", b"") }, "directory entry"),
            (RawEntry { kind: EntryType::XGlobalHeader, ..RawEntry::file("pax_global_header", b"") }, "entry type"),
        ] {
            let mut entries = base.clone();
            entries.push(entry);
            refused_archive(&gz(&raw_tar(&entries)), needle);
        }
        let mut entries = base.clone();
        entries.push(RawEntry::file("README.md", b"# again\n"));
        refused_archive(&gz(&raw_tar(&entries)), "appears twice");
        let mut entries = base.clone();
        entries.push(RawEntry { name: b"assets/\xff.png", ..RawEntry::file("x", PNG) });
        refused_archive(&gz(&raw_tar(&entries)), "not UTF-8");
    }

    #[test]
    fn archive_framing() {
        let good = Bundle::from_files(meta_app()).unwrap().write().unwrap();
        refused_archive(b"", "not a gzip-compressed bundle");
        refused_archive(b"PK\x03\x04 a zip file", "not a gzip-compressed bundle");
        let mut trailing = good.clone();
        trailing.extend_from_slice(b"appended");
        refused_archive(&trailing, "after the compressed archive");
        let mut tar = raw_tar(&meta_app().iter().map(|(p, d)| RawEntry::file(p, d)).collect::<Vec<_>>());
        tar.extend_from_slice(&[7u8; 512]);
        refused_archive(&gz(&tar), "after the end of the archive");
        let bomb = gz(&vec![0u8; (MAX_UNPACKED + TAR_OVERHEAD + 1) as usize]);
        refused_archive(&bomb, "larger than 50 MiB unpacked");
        let tar = raw_tar(&[RawEntry::file("app.yaml", b"id: x\n")]);
        refused_archive(&gz(&tar[..tar.len() - 1024 - 512 + 3]), "truncated");
    }

    #[test]
    fn limits() {
        let mut files = meta_app();
        files.insert("README.md".into(), vec![b'a'; MAX_TEXT + 1]);
        refused(files, "larger than 2 MiB");

        let mut files = meta_app();
        for i in 0..MAX_FILES {
            files.insert(format!("assets/s{i}.png"), PNG.to_vec());
        }
        refused(files, "the limit is 2000");

        let mut files = meta_app();
        let big = [PNG, &vec![0u8; 10 << 20][..]].concat();
        for i in 0..6 {
            files.insert(format!("assets/big{i}.png"), big.clone());
        }
        refused(files, "larger than 50 MiB unpacked");

        let mut files = wasm_app();
        files.insert("plugin.wasm".into(), [WASM_MAGIC, &vec![0u8; MAX_WASM]].concat());
        refused(files, "plugin.wasm is larger than 8 MiB");

        // Many entries in one archive are refused while reading.
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("assets/s{i}.png")).collect();
        let entries: Vec<RawEntry> = names.iter().map(|n| RawEntry::file(n, PNG)).collect();
        refused_archive(&gz(&raw_tar(&entries)), "more than 2000 files");
    }

    #[test]
    fn encodings_and_formats() {
        let mut files = meta_app();
        files.insert("README.md".into(), b"caf\xe9".to_vec());
        refused(files, "README.md is not UTF-8");
        let mut files = meta_app();
        files.insert("packages/kit/entities/bad.yaml".into(), vec![0xff, 0xfe]);
        refused(files, "not UTF-8");
        let mut files = meta_app();
        files.insert("assets/fake.png".into(), b"MZ executable".to_vec());
        refused(files, "not a valid png image");
        let mut files = meta_app();
        files.insert("assets/fake.svg".into(), b"<html><script>".to_vec());
        refused(files, "not a valid svg image");
        let mut files = meta_app();
        files.insert("assets/notes.txt".into(), b"hi".to_vec());
        refused(files, "only png, jpg");
        for ok in [&b"\xff\xd8\xff\xe0jpeg"[..], b"GIF89a....", b"RIFF\0\0\0\0WEBPVP8 "] {
            let ext = match ok[0] {
                0xff => "jpg",
                b'G' => "gif",
                _ => "webp",
            };
            let mut files = meta_app();
            files.insert(format!("assets/x.{ext}"), ok.to_vec());
            Bundle::from_files(files).unwrap_or_else(|e| panic!("{ext}: {e}"));
        }
        let mut files = wasm_app();
        files.insert("plugin.wasm".into(), b"(module)".to_vec());
        refused(files, "not a binary WebAssembly module");
        let mut files = wasm_app();
        files.insert("plugin.wasm".into(), b"\0asm\x0d\0\x01\0component".to_vec());
        refused(files, "not a binary WebAssembly module");
    }

    #[test]
    fn layout_rules() {
        let mut files = meta_app();
        files.insert("install.sh".into(), b"rm -rf /".to_vec());
        refused(files, "unexpected file install.sh");
        let mut files = meta_app();
        files.insert("packages/kit/hooks/x.yaml".into(), b"a: 1".to_vec());
        refused(files, "unexpected file packages/kit/hooks/x.yaml");
        let mut files = meta_app();
        files.insert("packages/kit/entities/sub/x.yaml".into(), b"a: 1".to_vec());
        refused(files, "unexpected file");
        let mut files = meta_app();
        files.insert("packages/other/package.yaml".into(), b"package: other\nversion: '1'\n".to_vec());
        refused(files, "packages/other/ is not listed");
        let mut files = meta_app();
        files.remove("packages/kit/package.yaml");
        files.remove("packages/kit/entities/thing.yaml");
        refused(files, "packages/kit/package.yaml is missing");
        let mut files = meta_app();
        files.remove("app.yaml");
        refused(files, "app.yaml is missing");
        let mut files = meta_app();
        files.remove("README.md");
        refused(files, "refers to README.md");
        let mut files = meta_app();
        files.insert("readme.md".into(), b"x".to_vec());
        refused(files, "only in letter case");
        let mut files = meta_app();
        files.insert("assets/icon.png".into(), PNG.to_vec());
        files.insert("assets/icon.png/x.png".into(), PNG.to_vec());
        refused(files, "cannot be inside it");
        let mut files = meta_app();
        files.insert("packages/kit/package.yaml".into(), b"package: other\nversion: 1.0.0\n".to_vec());
        refused(files, "declares package 'other'");
        let mut files = meta_app();
        files.insert("packages/kit/entities/thing.yaml".into(), b"name: [unclosed".to_vec());
        refused(files, "package kit");
        // A package README is fine and kept with the package's files.
        let mut files = meta_app();
        files.insert("packages/kit/README.md".into(), b"# Kit package\n".to_vec());
        let b = Bundle::from_files(files).unwrap();
        assert!(b.package_files()["kit"].contains_key("README.md"));
        assert_eq!(b.packages().unwrap()[0].entities[0].name, "KtThing");
    }

    #[test]
    fn code_rules() {
        let mut files = meta_app();
        files.insert("packages/kit/sql/001_init.sql".into(), b"create table x ();".to_vec());
        refused(files, "meta apps carry no code and no SQL");
        let mut files = meta_app();
        files.insert("plugin.wasm".into(), WASM_MAGIC.to_vec());
        refused(files, "meta apps carry no plugin code");
        let mut files = wasm_app();
        files.remove("plugin.wasm");
        refused(files, "plugin.wasm is missing");
        let mut files = wasm_app();
        files.remove("plugin.json");
        refused(files, "plugin.json is missing");
        // wasm and native apps may ship SQL (the store limits it to verified publishers).
        assert!(Bundle::from_files(wasm_app()).unwrap().package_files()["gw"].contains_key("sql/001_init.sql"));
        let (wasm, json) = Bundle::from_files(wasm_app()).unwrap().plugin().unwrap().unwrap();
        assert!(wasm.starts_with(WASM_MAGIC) && json["name"] == "gw");
        assert!(Bundle::from_files(meta_app()).unwrap().plugin().unwrap().is_none());
    }

    #[test]
    fn third_party_namespaces() {
        let set = |path: &str, from: &str, to: &str| {
            let mut files = meta_app();
            let text = String::from_utf8(files[path].clone()).unwrap();
            assert!(text.contains(from), "{from}");
            files.insert(path.into(), text.replacen(from, to, 1).into_bytes());
            files
        };
        refused(set("packages/kit/entities/thing.yaml", "name: KtThing", "name: Thing"), "must start with the app prefix Kt");
        refused(set("packages/kit/entities/thing.yaml", "name: KtThing", "name: Ktthing"), "must start with the app prefix Kt");
        refused(set("app.yaml", "prefix: Kt\n", ""), "needs a namespace");
        refused(set("packages/kit/package.yaml", "field_prefix: kt_", "field_prefix: in_"), "field_prefix must be kt_");
        refused(set("packages/kit/package.yaml", "name: kt_level", "name: level"), "must start with kt_");
        refused(set("packages/kit/package.yaml", "entity: KtThing, read", "entity: '*', read"), "every entity");
        refused(set("packages/kit/package.yaml", "name: Kit User", "name: 'plugin:gw'"), "reserved");
        // Native apps are platform code: no prefix needed, and they may grant '*'.
        let mut files = set("app.yaml", "runtime: meta", "runtime: native");
        let pkg = String::from_utf8(files["packages/kit/package.yaml"].clone()).unwrap().replace("entity: KtThing, read", "entity: '*', read");
        files.insert("packages/kit/package.yaml".into(), pkg.into_bytes());
        let entity = String::from_utf8(files["packages/kit/entities/thing.yaml"].clone()).unwrap().replace("KtThing", "Thing");
        files.insert("packages/kit/entities/thing.yaml".into(), entity.into_bytes());
        Bundle::from_files(files).unwrap();
    }

    #[test]
    fn signatures_file_is_parsed_and_checked() {
        let mut files = meta_app();
        files.insert("SIGNATURES.json".into(), b"{\"digest\": \"abc\", \"signatures\": []}".to_vec());
        refused(files, "digest");
        let mut files = meta_app();
        files.insert("SIGNATURES.json".into(), b"not json".to_vec());
        refused(files, "SIGNATURES.json");
        let mut b = Bundle::from_files(meta_app()).unwrap();
        b.sign(&signer(), crate::Role::Store).unwrap();
        let mut files = meta_app();
        files.insert("SIGNATURES.json".into(), serde_json::to_vec(b.signatures.as_ref().unwrap()).unwrap());
        let again = Bundle::from_files(files).unwrap();
        assert_eq!(again.signatures, b.signatures);
        assert!(again.paths().all(|p| p != "SIGNATURES.json"));
    }
}
