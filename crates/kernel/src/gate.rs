//! A workspace's own access controls (docs/operations.md, "Staff networks"; docs/security.md): the
//! networks its staff may work from. Checked where a request's context is built, after the zone
//! (crate::zones) and in addition to it, and before a staff sign-in reads any secret.
//!
//! The networks apply to people's credentials for staff accounts (browser sessions, session tokens,
//! OAuth tokens acting for a person, app passwords, app tokens, sockets, sign-in). Not to API keys
//! and SCIM keys (machines: a key has its own networks), nor to support access (granted explicitly,
//! and seen request by request).

use crate::Kernel;
use crate::error::{Error, Result};
use crate::zones::{Credential, Mode, Net};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Mutex;
use uuid::Uuid;

/// The most networks one list holds.
const MAX_NETWORKS: usize = 100;

/// The workspace setting `staff_networks`: `{"networks": ["10.0.0.0/8", "2001:db8::/32"], "mode":
/// "enforce" | "report"}`. No networks: off, as without it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaffNetworks {
    pub nets: Vec<Net>,
    pub mode: Mode,
}

impl StaffNetworks {
    /// Reads the setting as saved (`None` or `null`: off). Anything else that is not plainly right
    /// is an error, which the callers treat as refusing.
    pub fn parse(v: Option<&Value>) -> std::result::Result<StaffNetworks, String> {
        let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(StaffNetworks::default()) };
        const SHAPE: &str = "must be {\"networks\": [\"10.0.0.0/8\", …], \"mode\": \"enforce\" or \"report\"}";
        let o = v.as_object().ok_or(SHAPE)?;
        if o.keys().any(|k| k != "networks" && k != "mode") {
            return Err(SHAPE.into());
        }
        let mode = match o.get("mode") {
            None => Mode::Enforce,
            Some(Value::String(m)) if m == "enforce" => Mode::Enforce,
            Some(Value::String(m)) if m == "report" => Mode::Report,
            Some(_) => return Err(SHAPE.into()),
        };
        let list = match o.get("networks") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a.clone(),
            Some(_) => return Err(SHAPE.into()),
        };
        if list.len() > MAX_NETWORKS {
            return Err(format!("may hold at most {MAX_NETWORKS} networks"));
        }
        let mut nets = Vec::with_capacity(list.len());
        for n in &list {
            nets.push(n.as_str().and_then(Net::parse).ok_or_else(|| format!("{n} is not a network: write 10.0.0.0/8 or 2001:db8::/32"))?);
        }
        Ok(StaffNetworks { nets, mode })
    }

    /// Off: no networks.
    pub fn is_off(&self) -> bool {
        self.nets.is_empty()
    }

    /// Whether `address` (as `client_ip` derives it) may work here. One that is not an address (no
    /// proxy in front: `local`) is outside every list.
    pub fn admits(&self, address: &str) -> bool {
        self.is_off() || address.parse::<IpAddr>().is_ok_and(|ip| self.nets.iter().any(|n| n.contains(ip)))
    }

    /// As it is saved and shown.
    pub fn to_value(&self) -> Value {
        json!({"networks": self.nets.iter().map(|n| n.to_string()).collect::<Vec<_>>(), "mode": self.mode.as_str()})
    }
}

/// Whether the networks apply to a credential of this kind: people's, not keys of machines.
pub fn applies_to(credential: Credential) -> bool {
    !matches!(credential, Credential::ApiKey | Credential::Scim)
}

/// The one refusal of a credential from outside the networks (HTTP 403, code `network`). It says
/// nothing of which networks are allowed.
pub fn refusal() -> Error {
    Error::Refused { code: "network", status: 403, message: "this workspace cannot be used from this network".into(), pointer: None }
}

/// Refusals counted since start, by mode and credential (`ferrum_staff_network_refusals_total`).
static REFUSALS: Mutex<BTreeMap<(&'static str, &'static str), u64>> = Mutex::new(BTreeMap::new());

/// Counts and logs a credential from outside a workspace's networks (refused, or admitted in report
/// mode). The log names the person, workspace, address and kind of credential; never a secret.
pub fn count_refusal(mode: Mode, credential: Credential, tenant: uuid::Uuid, user: uuid::Uuid, address: &str) {
    *REFUSALS.lock().unwrap_or_else(|e| e.into_inner()).entry((mode.as_str(), credential.as_str())).or_default() += 1;
    tracing::warn!(person = %user, workspace = %tenant, mode = mode.as_str(), credential = credential.as_str(), address, "a staff credential from outside the workspace's networks");
}

/// The refusals counted so far: `(mode, credential, count)`.
pub fn refusals() -> Vec<(&'static str, &'static str, u64)> {
    REFUSALS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|((m, c), n)| (*m, *c, *n)).collect()
}

/// Where a sign-in comes from, for the steps before any secret is read: whether its zone refuses
/// staff (crate::zones), and the caller as the server sees it (crate::zones::Entry). A bare `bool`
/// is the zone's answer alone, from nowhere in particular (tools and tests).
#[derive(Debug, Clone, Default)]
pub struct Arrival {
    pub refuse_zone: bool,
    pub address: String,
    pub user_agent: String,
    pub zone: Option<String>,
}

impl From<bool> for Arrival {
    fn from(refuse_zone: bool) -> Self {
        Arrival { refuse_zone, ..Default::default() }
    }
}

impl Kernel {
    /// Whether staff of workspace `tenant` are refused from `address` by its networks: enforced, not
    /// reported (a report is counted here). A setting that cannot be read refuses.
    pub async fn network_refuses(&self, tenant: Uuid, user: Uuid, address: &str) -> bool {
        let nets = match self.staff_networks(tenant).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(workspace = %tenant, "a workspace's staff networks could not be read: {e}");
                return true;
            }
        };
        if nets.admits(address) {
            return false;
        }
        count_refusal(nets.mode, Credential::SignIn, tenant, user, address);
        nets.mode == Mode::Enforce
    }

    /// Whether a sign-in step from `arrival` treats `user` (of `kind`) as unknown here: staff, where
    /// the zone or the workspace's networks refuse them.
    pub(crate) async fn refuses_here(&self, arrival: &Arrival, tenant: Uuid, user: Uuid, kind: &str) -> bool {
        crate::zones::is_staff(kind) && (arrival.refuse_zone || self.network_refuses(tenant, user, &arrival.address).await)
    }

    /// The workspace's `staff_networks`, as saved.
    pub async fn staff_networks(&self, tenant: Uuid) -> Result<StaffNetworks> {
        let mut tx = self.db.system_tx().await?;
        let v: Option<Value> =
            sqlx::query_scalar("select settings->'staff_networks' from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?.flatten();
        tx.commit().await?;
        StaffNetworks::parse(v.as_ref()).map_err(Error::Internal)
    }

    /// `ferrum task clear_staff_networks tenant:<slug> reason:<text>`: the operator takes a
    /// workspace's networks away, when its administrators shut themselves out. On the workspace's
    /// audit trail as the system's, with `reason`; every replica drops what it holds at once.
    pub async fn clear_staff_networks(&self, tenant_slug: &str, reason: &str) -> Result<()> {
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why, in up to 500 characters"));
        }
        let (tenant, _) = self.tenant_by_slug(tenant_slug).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        sqlx::query("update tenant set settings = settings - 'staff_networks' where id = $1").bind(tenant).execute(&mut *tx).await?;
        let system = crate::Ctx::system(tenant, self.registry.schema(tenant).await?);
        crate::audit::record(&mut tx, &system, "Tenant", tenant, "clear_staff_networks", None, json!({"reason": reason, "by": "operator"})).await?;
        crate::auth::notify_users(&mut tx, tenant, None).await?;
        crate::registry::Registry::bump_version(&mut tx, tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(tenant);
        self.invalidate_tenant_users(tenant);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_is_read_strictly() {
        assert!(StaffNetworks::parse(None).unwrap().is_off());
        assert!(StaffNetworks::parse(Some(&Value::Null)).unwrap().is_off());
        assert!(StaffNetworks::parse(Some(&json!({"networks": []}))).unwrap().is_off());
        let s = StaffNetworks::parse(Some(&json!({"networks": ["10.0.0.0/8", "2001:db8::/32"], "mode": "report"}))).unwrap();
        assert_eq!((s.nets.len(), s.mode), (2, Mode::Report));
        assert_eq!(s.to_value(), json!({"networks": ["10.0.0.0/8", "2001:db8::/32"], "mode": "report"}));
        assert_eq!(StaffNetworks::parse(Some(&json!({"networks": ["192.0.2.1"]}))).unwrap().mode, Mode::Enforce, "enforce unless said");
        for bad in [
            json!("10.0.0.0/8"),
            json!({"networks": "10.0.0.0/8"}),
            json!({"networks": ["10.0.0.1/8"]}),
            json!({"networks": [7]}),
            json!({"networks": [], "mode": "audit"}),
            json!({"networks": [], "colour": "red"}),
            json!({"networks": vec!["10.0.0.0/8"; 101]}),
        ] {
            assert!(StaffNetworks::parse(Some(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn addresses_inside_and_outside() {
        let s = StaffNetworks::parse(Some(&json!({"networks": ["10.0.0.0/8", "2001:db8::/32"]}))).unwrap();
        assert!(s.admits("10.1.2.3") && s.admits("2001:db8::7") && s.admits("::ffff:10.1.2.3"));
        assert!(!s.admits("203.0.113.9") && !s.admits("2001:db9::1"));
        assert!(!s.admits("local") && !s.admits(""), "no address is outside");
        assert!(StaffNetworks::default().admits("local"), "off admits everyone");
    }

    #[test]
    fn people_not_machines() {
        for c in [Credential::Cookie, Credential::Token, Credential::OAuth, Credential::Dav, Credential::AppToken, Credential::Socket, Credential::SignIn] {
            assert!(applies_to(c), "{c:?}");
        }
        assert!(!applies_to(Credential::ApiKey) && !applies_to(Credential::Scim));
        assert_eq!(refusal().status(), 403);
        assert!(!refusal().to_string().contains("10."), "the refusal names no network");
    }
}
