//! HTTP clients for calls to services a workspace configures (identity providers, the GST
//! portals): https only (plain http too when private addresses are allowed, for development),
//! the host resolved once, refused when it is a private or loopback address, and pinned to the
//! vetted addresses so a DNS change between check and connect cannot redirect the call; no
//! redirects; a time limit.

use crate::Kernel;
use crate::error::{Error, Result};
use std::net::SocketAddr;
use std::time::Duration;

impl Kernel {
    /// A client for `url` and the parsed URL, or why the address is refused.
    pub async fn vetted_client(&self, url: &str, allow_private: bool, timeout: Duration, agent: &str) -> Result<(reqwest::Client, reqwest::Url)> {
        let unreachable = |e: String| Error::Unavailable(format!("{url} cannot be reached: {e}"));
        let parsed = reqwest::Url::parse(url).map_err(|_| Error::invalid(format!("{url} is not a URL")))?;
        if parsed.scheme() != "https" && !(allow_private && parsed.scheme() == "http") {
            return Err(Error::invalid(format!("{url} must use https")));
        }
        let host = parsed.host_str().ok_or_else(|| Error::invalid(format!("{url} has no host")))?.to_string();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| unreachable(e.to_string()))?.collect();
        if addrs.is_empty() {
            return Err(unreachable(format!("{host} does not resolve")));
        }
        if !allow_private && addrs.iter().any(|a| crate::outbox::is_private(a.ip())) {
            return Err(Error::invalid(format!("{host} is a private address")));
        }
        let client = reqwest::Client::builder()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent(agent)
            .build()
            .map_err(|e| unreachable(e.to_string()))?;
        Ok((client, parsed))
    }
}
