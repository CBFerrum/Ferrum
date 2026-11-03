//! Apps compiled into a server that is built somewhere else. A private app's repository can make its own binary of
//! this server with the app inside, through [`extend`] and [`crate::run`], without this repository naming the app or
//! fetching its code: a public CI cannot fetch a private dependency, and an app's code is its own repository's.

use crate::state::AppState;
use ferrum_kernel::{HookRegistry, Kernel};
use ferrum_meta::{MetaError, Package};
use futures_util::future::BoxFuture;
use std::sync::{Arc, OnceLock};

/// What such an app gives the server: what a compiled-in app gives through the `apps!` list, and its tasks and metrics.
pub struct Extension {
    /// Its id, as its interface is served under.
    pub id: &'static str,
    pub packages: fn() -> Result<Vec<Package>, MetaError>,
    pub register: fn(&mut HookRegistry),
    pub reports: fn() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>>,
    pub maintenance: for<'a> fn(&'a Kernel) -> BoxFuture<'a, ferrum_kernel::Result<()>>,
    /// Its `app.yaml`.
    pub manifest: &'static str,
    /// Its built interface, as an app crate's `UI`. Empty: it has none.
    pub ui: &'static [(&'static str, &'static [u8])],
    /// What it adds to `ferrum task`.
    pub tasks: fn(&mut loco_rs::task::Tasks),
    /// Lines it adds to `/metrics`, with nothing about any workspace in them (D-17).
    pub metrics: for<'a> fn(&'a AppState) -> BoxFuture<'a, ferrum_kernel::Result<String>>,
}

static EXTENSIONS: OnceLock<Vec<Extension>> = OnceLock::new();

/// Adds apps to this server, once, before [`crate::run`].
pub fn extend(extensions: Vec<Extension>) {
    assert!(EXTENSIONS.set(extensions).is_ok(), "a server is extended once, before it starts");
}

pub(crate) fn all() -> &'static [Extension] {
    EXTENSIONS.get().map(Vec::as_slice).unwrap_or(&[])
}
