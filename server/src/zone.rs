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

/// Where a sign-in comes from (ferrum_kernel::gate::Arrival): whether the zone refuses staff, and the
/// caller's address, browser and zone. The kernel's sign-in steps check the workspace's staff
/// networks with it, once they know the workspace, before any secret is read.
pub async fn arrival(st: &AppState, headers: &HeaderMap) -> ferrum_kernel::gate::Arrival {
    let entry = entry(st, headers, Credential::SignIn).await;
    ferrum_kernel::gate::Arrival {
        refuse_zone: entry.refuses_staff(),
        address: entry.address.clone(),
        user_agent: headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default().chars().take(300).collect(),
        zone: entry.name().map(str::to_string),
    }
}

/// A step of signing in for `user` that comes before any secret of theirs is checked (the second
/// step's code): refused, without counting, when they are staff and the zone refuses staff. A kind
/// that cannot be read counts as staff.
pub async fn refuses_sign_in(st: &AppState, headers: &HeaderMap, tenant: uuid::Uuid, user: uuid::Uuid) -> bool {
    let entry = entry(st, headers, Credential::SignIn).await;
    staff(&st.kernel.user_kind(tenant, user).await) && (entry.refuses_staff() || st.kernel.network_refuses(tenant, user, &entry.address).await)
}

/// A sign-in about to open a session or hand out a token for `user`: refused as a failed sign-in is
/// (`Err`) when they are staff and the zone admits no staff, or the workspace's staff networks do not
/// hold the caller's address; admitted and counted in report mode. The entry is the session's zone.
pub async fn sign_in(st: &AppState, headers: &HeaderMap, tenant: uuid::Uuid, user: uuid::Uuid) -> Result<Arc<Entry>, ()> {
    let entry = entry(st, headers, Credential::SignIn).await;
    if !staff(&st.kernel.user_kind(tenant, user).await) {
        return Ok(entry);
    }
    if !entry.admits_staff() {
        ferrum_kernel::zones::count_refusal(&entry, tenant, user);
    }
    if entry.refuses_staff() || st.kernel.network_refuses(tenant, user, &entry.address).await {
        // On the audit trail, as a refusal (capped as failures are); the answer says nothing more.
        if let Err(e) = st.kernel.record_sign_in_failure(tenant, user, "refused", "", &arrival(st, headers).await).await {
            tracing::warn!("a refused sign-in could not be recorded: {e}");
        }
        return Err(());
    }
    Ok(entry)
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

/// `ferrum_staff_network_refusals_total{mode,credential}` for `/metrics`: staff credentials from
/// outside their workspace's networks since this replica started (admitted in report mode).
pub fn network_metrics_text() -> String {
    let mut out = String::from(
        "# HELP ferrum_staff_network_refusals_total Staff credentials from outside their workspace's networks (in report mode: admitted, and counted).\n\
         # TYPE ferrum_staff_network_refusals_total counter\n",
    );
    for (mode, credential, n) in ferrum_kernel::gate::refusals() {
        out.push_str(&format!("ferrum_staff_network_refusals_total{{mode=\"{mode}\",credential=\"{credential}\"}} {n}\n"));
    }
    out
}

/// `ferrum_sign_in_unknown_total` for `/metrics`: failed sign-ins for an address no account has (never
/// recorded by address).
pub fn unknown_sign_in_metrics_text() -> String {
    format!(
        "# HELP ferrum_sign_in_unknown_total Failed sign-ins for an address no account has.\n# TYPE ferrum_sign_in_unknown_total counter\nferrum_sign_in_unknown_total {}\n",
        ferrum_kernel::gate::unknown_failures()
    )
}

/// `ferrum_audit_sink_behind_seconds` for `/metrics`, when the operator's audit sink is set: how long the oldest
/// event not yet sent has waited (0: none).
pub async fn audit_sink_metrics_text(k: &ferrum_kernel::Kernel) -> String {
    match k.audit_sink_behind().await {
        Ok(Some(s)) => format!(
            "# HELP ferrum_audit_sink_behind_seconds How long the oldest audit event not yet sent to the audit sink has waited.\n\
             # TYPE ferrum_audit_sink_behind_seconds gauge\nferrum_audit_sink_behind_seconds {s}\n"
        ),
        Ok(None) => String::new(),
        Err(e) => {
            tracing::warn!("how far behind the audit sink is: {e}");
            String::new()
        }
    }
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
