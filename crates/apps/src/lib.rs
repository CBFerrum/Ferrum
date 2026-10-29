//! Ferrum apps: the `app.yaml` manifest, the signed `.fapp` bundle format, signature verification
//! and dependency resolution (docs/plan/apps.md). The server, the `ferrum-app` CLI and the store
//! all use this crate, so they accept exactly the same bundles.
//!
//! # Bundle format (`ferrum-app-v1`)
//!
//! A gzip-compressed tar (one gzip member) of regular files only, at these paths:
//!
//! | Path | Content |
//! |---|---|
//! | `app.yaml` | the manifest ([`AppManifest`]); required |
//! | `README.md`, `CHANGELOG.md`, `LICENSE` | documentation |
//! | `plugin.json`, `plugin.wasm` | wasm apps only: the plugin manifest and the core WebAssembly module |
//! | `packages/<name>/package.yaml`, `README.md`, `entities/*.yaml`, `sql/*.sql`, `print/*.html` | the app's metadata packages |
//! | `assets/**/*.{png,jpg,jpeg,gif,webp,svg}` | icon and screenshots |
//! | `ui/**` | wasm and meta apps: screens (`html`, `js`, `mjs`, `css`, `json`, `map`, `txt`, `svg`, and `wasm`, `woff2` and raster images up to [`MAX_UI_BINARY`] bytes) |
//! | `SIGNATURES.json` | signatures (not covered by the digest) |
//!
//! Limits: [`MAX_UNPACKED`] bytes unpacked, [`MAX_FILES`] files (not counting `SIGNATURES.json`),
//! [`MAX_WASM`] bytes of WebAssembly, [`MAX_TEXT`] bytes per text file. Everything except
//! `plugin.wasm`, raster images and the binary files of screens is UTF-8 text.
//!
//! **Digest** ([`Bundle::digest`]): lower-case hex SHA-256 over `ferrum-app-v1\n` followed by one
//! line per file except `SIGNATURES.json`, sorted by the path's bytes: `<path>\0<sha256 hex of
//! content>\n`. It depends only on paths and contents, never on tar order, timestamps or owners.
//!
//! **Signatures** (`SIGNATURES.json`): `{"digest": "<hex>", "signatures": [{"key_id", "role",
//! "sig"}]}` where `key_id` is [`key_id`] of an Ed25519 public key, `role` is `publisher` or
//! `store`, and `sig` is the standard-base64 Ed25519 signature of the **32 raw digest bytes** (the
//! hex string decoded, not the hex text itself).
//!
//! # Decisions where the specification leaves room
//!
//! The safest reasonable option was chosen each time:
//!
//! * **Exact layout.** Files outside the table above are refused rather than ignored, so a
//!   reviewed bundle holds nothing unexpected. Directory entries are refused too ("only regular
//!   files"); so are data after the gzip member and anything but zero padding after the end of
//!   the tar.
//! * **Paths** are at most 255 bytes of `/`-separated components made of ASCII letters, digits,
//!   `.`, `_` and `-`; no component is empty, `.`, `..` or starts with a dot (no hidden files). Two
//!   paths may not differ only in letter case, and no file may sit "below" another file.
//! * **Content checks.** `plugin.wasm` must be a binary core module (`\0asm`, version 1 — not
//!   WebAssembly text, not a component). Images must start with their format's signature and
//!   SVGs must be UTF-8 text containing `<svg` (servers must still serve them as images, never
//!   inline HTML). `plugin.json` must be exactly the plugin manifest the server installs (unknown
//!   fields refused), named after the app, with resource limits capped ([`MAX_PLUGIN_FUEL`],
//!   [`MAX_PLUGIN_MEMORY_MB`]).
//! * **Manifest.** `plugin` must be `plugin.json`; `description` names a markdown file in the
//!   bundle; `icon` and `screenshots` are images under `assets/`; `homepage` is an http(s) URL;
//!   `support` an http(s) URL or e-mail address; versions carry no build metadata; `publisher`
//!   follows the app id syntax; prefixes starting with `X` are refused (that namespace belongs to
//!   custom and plugin entities). [`AppManifest::parse`] also validates.
//! * **Third-party namespaces** (wasm and meta apps; native apps are reviewed platform code):
//!   package entities are named `<prefix><Capital or digit>…` (`EvChargePoint`, not `Event`),
//!   plugin entities `X<prefix>…`, fields added to other packages' entities start with
//!   `<snake prefix>_` (and a package's `field_prefix`, when set, is exactly that), and package
//!   roles may not grant `*`. Role names starting with `plugin:` are reserved for plugins'
//!   service users and refused in every app. A role may be named by several packages (each adds
//!   grants, as the platform's own packages do); whether it already exists on a cluster is only
//!   known there (see `ferrum-app check --platform-packages` and the server).
//! * **Verification** ([`Bundle::verify`]) fails when the bundle is unsigned, when the content
//!   digest differs from the signed one, when a trusted key's signature does not verify, or when
//!   no trusted key signed it. A signature counts only for a trusted key with the same key id
//!   *and* role; other entries (untrusted keys, mismatched roles) are ignored.
//! * **Permissions** ([`Permissions`]) list everything a consent screen must show; the digest is
//!   SHA-256 over a domain-separated canonical JSON form, and [`Permissions::increases`] is
//!   conservative: any entry that was not there before counts as new.
//! * **Resolution** ([`resolve`]) never changes an installed app that was not requested, never
//!   downgrades, and backtracks to older versions when the newest ones conflict; see its docs.

mod bundle;
pub mod contract;
mod manifest;
mod permissions;
mod resolve;
mod signing;
#[cfg(test)]
mod testutil;

pub use bundle::Bundle;
pub use manifest::{AppManifest, CATEGORIES, ExtensionPoint, Runtime, SETTING_TYPES, SettingDef, UI_POINTS, Uninstall, contribution_body};
pub use permissions::Permissions;
pub use resolve::{Release, resolve};
pub use signing::{Role, SignatureEntry, Signatures, Signer, TrustedKey, key_id};

/// Bundle format identifier; the first line hashed into every digest.
pub const FORMAT: &str = "ferrum-app-v1";
/// The app API version this platform build provides; apps declare `platform` requirements against it.
pub const PLATFORM_VERSION: &str = "0.3.0";

/// Most bytes a bundle may hold unpacked (50 MiB).
pub const MAX_UNPACKED: u64 = 50 << 20;
/// Most files a bundle may hold (`SIGNATURES.json` not counted).
pub const MAX_FILES: usize = 2_000;
/// Largest `plugin.wasm` (8 MiB, the plugin host's own limit).
pub const MAX_WASM: usize = 8 << 20;
/// Largest text file (2 MiB).
pub const MAX_TEXT: usize = 2 << 20;
/// Largest binary file of a screen under `ui/` (16 MiB): WebAssembly, fonts, images.
pub const MAX_UI_BINARY: usize = 16 << 20;
/// Most signatures `SIGNATURES.json` may carry.
pub const MAX_SIGNATURES: usize = 16;
/// Highest `fuel_limit` an app's plugin may ask for (20 times the host's default).
pub const MAX_PLUGIN_FUEL: i64 = 1_000_000_000;
/// Highest `memory_limit_mb` an app's plugin may ask for (8 times the host's default).
pub const MAX_PLUGIN_MEMORY_MB: i32 = 256;

/// Why an app, bundle, key or installation plan was refused. Messages are written for the
/// administrators and developers who read them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppError {
    /// `app.yaml` does not parse or breaks a rule.
    #[error("app.yaml: {0}")]
    Manifest(String),
    /// The archive, its paths, limits or layout.
    #[error("bundle: {0}")]
    Bundle(String),
    /// One of the app's packages.
    #[error("package {package}: {message}")]
    Package { package: String, message: String },
    /// `plugin.json` or `plugin.wasm`.
    #[error("plugin: {0}")]
    Plugin(String),
    /// Missing, malformed, untrusted or failing signatures.
    #[error("signature: {0}")]
    Signature(String),
    /// A signing key or trusted key that cannot be used.
    #[error("key: {0}")]
    Key(String),
    /// No installation plan satisfies the requirements.
    #[error("cannot install: {0}")]
    Resolve(String),
}
