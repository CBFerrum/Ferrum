//! The zone of a request (docs/operations.md, "Zones"; crates/kernel/src/zones.rs): the replica's
//! pin; else the zone of the request's host; else the first zone whose networks hold the caller's
//! address as [`crate::extract::client_ip`] derives it; else the default. Nothing else the client
//! sends decides it.

use crate::state::AppState;
use axum::http::HeaderMap;
use ferrum_kernel::zones::{Credential, Entry};
use std::sync::Arc;

/// How this request came in, for a credential of `credential`'s kind.
pub async fn entry(st: &AppState, headers: &HeaderMap, credential: Credential) -> Arc<Entry> {
    let address = crate::extract::client_ip(headers);
    let placed = match st.zones.settled() {
        Some(placed) => placed,
        None => {
            let host = match crate::shell::request_host(headers) {
                Some(h) => st.kernel.host_zone(h).await,
                None => Ok(None),
            };
            st.zones.place(host, &address)
        }
    };
    Arc::new(Entry { zones: st.zones.clone(), placed, address, credential })
}

/// Whether a sign-in for staff is refused in this request's zone (enforced, not reported): the
/// password and passkey steps then treat a staff account as one that does not exist, before its
/// secret is read.
pub async fn refuses_staff(st: &AppState, headers: &HeaderMap) -> bool {
    entry(st, headers, Credential::SignIn).await.refuses_staff()
}

/// A step of signing in for `user` that comes before any secret of theirs is checked (the second
/// step's code): refused, without counting, when they are staff and the zone refuses staff. A kind
/// that cannot be read counts as staff.
pub async fn refuses_sign_in(st: &AppState, headers: &HeaderMap, tenant: uuid::Uuid, user: uuid::Uuid) -> bool {
    let entry = entry(st, headers, Credential::SignIn).await;
    entry.refuses_staff() && staff(&st.kernel.user_kind(tenant, user).await)
}

/// A sign-in about to open a session or hand out a token for `user`: refused as a failed sign-in is
/// (`Err`) when they are staff and the zone admits no staff; admitted and counted in report mode.
/// The entry is the session's zone.
pub async fn sign_in(st: &AppState, headers: &HeaderMap, tenant: uuid::Uuid, user: uuid::Uuid) -> Result<Arc<Entry>, ()> {
    let entry = entry(st, headers, Credential::SignIn).await;
    if entry.admits_staff() || !staff(&st.kernel.user_kind(tenant, user).await) {
        return Ok(entry);
    }
    ferrum_kernel::zones::count_refusal(&entry, tenant, user);
    if entry.refuses_staff() { Err(()) } else { Ok(entry) }
}

/// The answer to a refused sign-in: the one a wrong password gets, which says nothing about the
/// account.
pub fn sign_in_failed() -> crate::error::ApiError {
    crate::error::ApiError(ferrum_kernel::Error::Unauthorized("invalid tenant, email or password".into()))
}

/// A kind that cannot be read counts as staff: refusing then fails closed.
fn staff(kind: &ferrum_kernel::Result<String>) -> bool {
    kind.as_ref().map_or(true, |k| ferrum_kernel::zones::is_staff(k))
}

/// `GET /_zone`, on any host and with no session: the zone this request is in, whether it admits
/// staff and administrators, and the caller's address as the server sees it (`ferrum task doctor`
/// checks a public address with it). No secrets; limited like the other public endpoints.
pub async fn answer(st: &AppState, headers: &HeaderMap) -> Result<serde_json::Value, crate::error::ApiError> {
    st.public_limiter.check(&format!("{}|zone", crate::extract::client_ip(headers))).await?;
    let entry = entry(st, headers, Credential::Cookie).await;
    let rule = entry.name().and_then(|n| st.zones.rule(n));
    Ok(serde_json::json!({
        "zone": entry.name(),
        "staff": if entry.admits_staff() { "all" } else { "none" },
        "admin": entry.admits_staff() && rule.is_none_or(|r| r.admin),
        "address": entry.address,
    }))
}

/// `ferrum_zone_refusals_total{zone,mode,credential}` for `/metrics`: staff credentials a zone did
/// not admit since this replica started (admitted all the same in report mode).
pub fn metrics_text() -> String {
    let mut out = String::from(
        "# HELP ferrum_zone_refusals_total Staff credentials refused where the zone admits no staff (in report mode: admitted, and counted).\n\
         # TYPE ferrum_zone_refusals_total counter\n",
    );
    for (zone, mode, credential, n) in ferrum_kernel::zones::refusals() {
        out.push_str(&format!("ferrum_zone_refusals_total{{zone=\"{zone}\",mode=\"{mode}\",credential=\"{credential}\"}} {n}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    /// Every request path builds its context through admission (`Kernel::request_ctx`, or
    /// `Kernel::reload` of a context that came in by one). Only work with no request behind it may
    /// build one "inside", with `load_ctx`: a new request path cannot skip its zone.
    #[test]
    fn request_paths_build_their_context_in_a_zone() {
        const INSIDE: &[&str] = &["tasks.rs"];
        let call = ["load_", "ctx("].concat();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root.clone()];
        let mut found = vec![];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                    if std::fs::read_to_string(&path).unwrap().contains(&call) && !INSIDE.contains(&rel.as_str()) {
                        found.push(rel);
                    }
                }
            }
        }
        assert!(found.is_empty(), "these build a request's context without its zone (use Kernel::request_ctx): {found:?}");
    }

    #[test]
    fn refusals_are_a_counter_by_zone_mode_and_credential() {
        let text = super::metrics_text();
        assert!(text.contains("# TYPE ferrum_zone_refusals_total counter"), "{text}");
    }
}
