//! Zones (docs/operations.md, "Zones"; docs/plan/topology.md, D4): a named place a request arrives
//! in, with a rule for who is admitted there. Level 1: a zone whose rule is `staff: none` admits no
//! staff (accounts of kind `member` or `support`, administrators included) on any credential. Level
//! 2: a zone whose rule names apps (`staff: ["crm"]`) or keeps administrators out (`admin: false`)
//! admits staff with their rights narrowed there ([`Rule::limited`], applied in `Kernel::request_ctx`).
//!
//! The settings are read and checked once, at start ([`Zones::parse`]); a request's zone is decided
//! by [`Zones::settled`] and [`Zones::place`], in this order: the replica's pin; else the zone of the
//! request's host row; else the first zone whose networks hold the caller's address; else the
//! default; else none (everyone is admitted, as without zones). Nothing the client sends decides it
//! but its address, as the server's `client_ip` derives it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// Zones, level 1 (pull request 229): an older build admits staff everywhere.
pub const FLOOR: crate::window::Floor =
    crate::window::Floor { feature: "zones", since: "2026-11-13T23:48:57Z", what: "zones that admit no staff refuse them", temporary: false };

/// Zones, level 2 (pull request 233): an older build does not narrow staff to some apps or keep administrators out.
pub const FLOOR_LIMITED: crate::window::Floor = crate::window::Floor {
    feature: "zones_limited",
    since: "2026-11-14T05:59:42Z",
    what: "zones that limit staff to some apps or keep administrators out narrow them",
    temporary: false,
};

impl crate::Kernel {
    /// At start, with zone settings in place: older builds that would not enforce them may no longer start
    /// ([`crate::window::require`]). Every replica of every pool does it; it changes nothing after the first.
    pub async fn require_zone_floors(&self, zones: &Zones) -> crate::Result<()> {
        if zones.is_off() {
            return Ok(());
        }
        self.require_floor(&FLOOR, "").await?;
        if zones.rules.values().any(Rule::limited) {
            self.require_floor(&FLOOR_LIMITED, "").await?;
        }
        Ok(())
    }
}

/// `FERRUM_ZONE_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Refuse what the rule refuses.
    #[default]
    Enforce,
    /// Admit, but count and log what would have been refused.
    Report,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Enforce => "enforce",
            Mode::Report => "report",
        }
    }
}

/// A network a zone places callers in by their address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    /// `10.0.0.0/8`, `2001:db8::/32`, or one address. Bits set past the prefix are refused, so
    /// that what is written is what is meant.
    pub fn parse(s: &str) -> Option<Net> {
        let (a, p) = match s.trim().split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s.trim(), None),
        };
        let addr: IpAddr = a.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.len() <= 3 => p.parse::<u8>().ok()?,
            Some(_) => return None,
            None => max,
        };
        if prefix > max || masked(addr, prefix) != addr {
            return None;
        }
        Some(Net { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 caller reaching an IPv6 socket shows as ::ffff:a.b.c.d.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        ip.is_ipv4() == self.addr.is_ipv4() && masked(ip, self.prefix) == self.addr
    }

    fn overlaps(&self, other: &Net) -> bool {
        self.contains(other.addr) || other.contains(self.addr)
    }
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn masked(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix)) };
            IpAddr::V4((bits & mask).into())
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(prefix)) };
            IpAddr::V6((bits & mask).into())
        }
    }
}

/// A zone's rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Whether staff are admitted at all (`staff: none` is false).
    pub staff: bool,
    /// `staff: [app, …]`: the apps staff are limited to (level 2). `None`: every app.
    pub apps: Option<Vec<String>>,
    /// The packages of `apps`, from the apps' manifests ([`Zones::resolve`]). Until resolved: none,
    /// so a rule that names apps never shows more than it says.
    pub packages: BTreeSet<String>,
    pub admin: bool,
    /// With `apps`: whether the workspace's own entities (made in the builder, of no package) are seen.
    pub custom: bool,
    /// A key of scope `scim` is admitted in a zone that admits no staff.
    pub scim: bool,
    pub networks: Vec<Net>,
}

impl Rule {
    /// Staff are admitted but narrowed here: limited to some apps, or never administrators.
    pub fn limited(&self) -> bool {
        self.staff && (self.apps.is_some() || !self.admin)
    }
}

/// The zone settings, checked.
#[derive(Debug, Clone, Default)]
pub struct Zones {
    rules: BTreeMap<String, Rule>,
    pin: Option<String>,
    default: Option<String>,
    pub mode: Mode,
}

/// Where a request is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    /// No zone: everyone is admitted, as without zones.
    Nowhere,
    /// In this zone. A name the settings do not hold (a host row naming a zone taken out of them)
    /// admits no staff.
    In(String),
    /// It could not be decided (the host's row could not be read): no staff are admitted.
    Unknown,
}

/// What kind of credential a request carries (logs and the refusals metric).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// The browser's session cookie.
    Cookie,
    /// A session token as a bearer token.
    Token,
    /// An `fk_` API key (any scope but `scim` and `dav`).
    ApiKey,
    /// An API key of scope `scim`.
    Scim,
    /// An app password of calendar and contacts apps (scope `dav`).
    Dav,
    /// An app token of an app's screens or own page.
    AppToken,
    /// An OAuth access token (MCP and other API clients).
    OAuth,
    /// A socket's credential.
    Socket,
    /// A sign-in about to open a session or hand out a token.
    SignIn,
    /// Work a request queued (a job, an event), done later in the zone the request was in.
    Queued,
}

impl Credential {
    pub fn as_str(self) -> &'static str {
        match self {
            Credential::Cookie => "cookie",
            Credential::Token => "token",
            Credential::ApiKey => "api_key",
            Credential::Scim => "scim",
            Credential::Dav => "dav",
            Credential::AppToken => "app_token",
            Credential::OAuth => "oauth",
            Credential::Socket => "socket",
            Credential::SignIn => "sign_in",
            Credential::Queued => "queued",
        }
    }
}

/// How a request came in: its zone, the caller's address and its credential. A context built for
/// a request carries it (`Ctx::zone`).
#[derive(Debug, Clone)]
pub struct Entry {
    pub zones: Arc<Zones>,
    pub placed: Placed,
    pub address: String,
    pub credential: Credential,
}

impl Entry {
    /// The zone's name, if the request is in one.
    pub fn name(&self) -> Option<&str> {
        match &self.placed {
            Placed::In(n) => Some(n),
            _ => None,
        }
    }

    /// Whether the rule admits staff on this credential.
    pub fn admits_staff(&self) -> bool {
        match &self.placed {
            Placed::Nowhere => true,
            Placed::Unknown => false,
            Placed::In(n) => self.zones.rules.get(n).is_some_and(|r| r.staff || (self.credential == Credential::Scim && r.scim)),
        }
    }

    /// Staff are refused here now (the rule refuses them and the mode enforces).
    pub fn refuses_staff(&self) -> bool {
        !self.admits_staff() && self.zones.mode == Mode::Enforce
    }

    /// Whether this zone keeps administrators out (level 2, enforced): administration is refused here.
    pub fn keeps_admins_out(&self) -> bool {
        self.zones.mode == Mode::Enforce && self.limit().is_some_and(|(_, r)| !r.admin)
    }

    /// The zone's name, when it narrows staff (level 2): what work queued here is marked with.
    pub fn marked(&self) -> Option<String> {
        self.limit().map(|(n, _)| n.to_string())
    }

    /// The rule of the zone, when it narrows staff admitted here (level 2).
    pub fn limit(&self) -> Option<(&str, &Rule)> {
        let name = self.name()?;
        self.zones.rules.get(name).filter(|r| r.limited()).map(|r| (name, r))
    }

    /// The zone as the refusals metric and the log name it.
    pub fn label(&self) -> &str {
        match &self.placed {
            Placed::Nowhere => "",
            Placed::In(n) => n,
            Placed::Unknown => "unknown",
        }
    }
}

impl Zones {
    /// `FERRUM_ZONES` (JSON), `FERRUM_ZONE`, `FERRUM_ZONE_DEFAULT` and `FERRUM_ZONE_MODE`, checked
    /// strictly: anything that is not plainly right refuses to start, with a message saying what.
    pub fn parse(zones: &str, pin: &str, default: &str, mode: &str) -> Result<Zones, String> {
        let mode = match mode.trim() {
            "" | "enforce" => Mode::Enforce,
            "report" => Mode::Report,
            other => return Err(format!("FERRUM_ZONE_MODE is {other:?}: write enforce or report")),
        };
        let raw = zones.trim();
        let map: serde_json::Map<String, serde_json::Value> = if raw.is_empty() {
            serde_json::Map::new()
        } else {
            match serde_json::from_str(raw) {
                Ok(serde_json::Value::Object(m)) => m,
                _ => return Err("FERRUM_ZONES is not a JSON object of zone name to rule, like {\"public\": {\"staff\": \"none\"}}".into()),
            }
        };
        let mut rules = BTreeMap::new();
        for (name, rule) in map {
            let named = |m: &str| format!("FERRUM_ZONES: zone {name:?} {m}");
            if name.is_empty() || name.len() > 63 || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || name.starts_with('-') {
                return Err(named("is not a name: use lowercase letters, digits and dashes, up to 63"));
            }
            let serde_json::Value::Object(rule) = rule else { return Err(named("has a rule that is not an object")) };
            if let Some(k) = rule.keys().find(|k| !["staff", "admin", "custom", "scim", "networks"].contains(&k.as_str())) {
                return Err(named(&format!("has {k:?}, which a rule does not have (staff, admin, custom, scim, networks)")));
            }
            let (staff, apps) = match rule.get("staff") {
                None => (true, None),
                Some(serde_json::Value::String(s)) if s == "all" => (true, None),
                Some(serde_json::Value::String(s)) if s == "none" => (false, None),
                Some(serde_json::Value::Array(list)) => {
                    let mut apps = vec![];
                    for a in list {
                        let id = a.as_str().unwrap_or_default();
                        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                            return Err(named(&format!("has {a} in staff, which is not an app's id: write ids like \"crm\"")));
                        }
                        if !apps.contains(&id.to_string()) {
                            apps.push(id.to_string());
                        }
                    }
                    if apps.is_empty() {
                        return Err(named("has an empty list of apps in staff: write \"none\" to admit no staff"));
                    }
                    (true, Some(apps))
                }
                Some(_) => return Err(named("has staff that is neither \"all\", \"none\" nor a list of apps")),
            };
            let flag = |key: &str| match rule.get(key) {
                None => Ok(None),
                Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
                Some(_) => Err(named(&format!("has {key} that is not true or false"))),
            };
            let admin = flag("admin")?.unwrap_or(staff && apps.is_none());
            if !staff && admin {
                return Err(named("admits administrators but no staff, and administrators are staff"));
            }
            let custom = flag("custom")?;
            if custom.is_some() && apps.is_none() {
                return Err(named("has custom, which only a rule with a list of apps in staff has"));
            }
            let custom = custom.unwrap_or(false);
            let scim = flag("scim")?.unwrap_or(false);
            let networks = match rule.get("networks") {
                None => vec![],
                Some(serde_json::Value::Array(list)) => {
                    let mut nets = vec![];
                    for n in list {
                        let s = n.as_str().unwrap_or_default();
                        nets.push(
                            Net::parse(s).ok_or_else(|| named(&format!("has {n} in networks, which is not a network: write 10.0.0.0/8 or 2001:db8::/32")))?,
                        );
                    }
                    nets
                }
                Some(_) => return Err(named("has networks that are not a list")),
            };
            rules.insert(name, Rule { staff, apps, packages: BTreeSet::new(), admin, custom, scim, networks });
        }
        // Two zones holding one address would make the order of the JSON matter; it does not.
        for (a, ra) in &rules {
            for (b, rb) in rules.range::<String, _>((std::ops::Bound::Excluded(a.clone()), std::ops::Bound::Unbounded)) {
                if let Some((x, y)) = ra.networks.iter().flat_map(|x| rb.networks.iter().map(move |y| (x, y))).find(|(x, y)| x.overlaps(y)) {
                    return Err(format!(
                        "FERRUM_ZONES: zones {a:?} ({x}) and {b:?} ({y}) hold the same addresses: an address may place a caller in one zone only"
                    ));
                }
            }
        }
        let named = |setting: &str, v: &str| -> Result<Option<String>, String> {
            match v.trim() {
                "" => Ok(None),
                n if rules.contains_key(n) => Ok(Some(n.to_string())),
                n => Err(format!("{setting} is {n:?}, which FERRUM_ZONES does not name")),
            }
        };
        let pin = named("FERRUM_ZONE", pin)?;
        let default = named("FERRUM_ZONE_DEFAULT", default)?;
        Ok(Zones { rules, pin, default, mode })
    }

    /// Gives each rule that names apps their packages (`packages_of`: an app id → its manifest's
    /// `packages`, `None` for an app this server does not know). An unknown app, or one with no
    /// packages, refuses to start: a rule must never quietly mean less or more than it says.
    pub fn resolve(&mut self, packages_of: impl Fn(&str) -> Option<Vec<String>>) -> Result<(), String> {
        for (name, rule) in &mut self.rules {
            let Some(apps) = &rule.apps else { continue };
            let mut packages = BTreeSet::new();
            for app in apps {
                match packages_of(app) {
                    None => return Err(format!("FERRUM_ZONES: zone {name:?} names the app {app:?}, which this server does not know")),
                    Some(p) if p.is_empty() => {
                        return Err(format!("FERRUM_ZONES: zone {name:?} names the app {app:?}, which has no packages a zone can limit staff to"));
                    }
                    Some(p) => packages.extend(p),
                }
            }
            rule.packages = packages;
        }
        Ok(())
    }

    /// Of `names` (zones that queued work carries), those these settings do not define, each once.
    pub fn lacking(&self, names: &[String]) -> Vec<String> {
        let out: BTreeSet<&String> = names.iter().filter(|n| !self.rules.contains_key(*n)).collect();
        out.into_iter().cloned().collect()
    }

    /// The app ids the rules name.
    pub fn app_ids(&self) -> BTreeSet<String> {
        self.rules.values().filter_map(|r| r.apps.as_ref()).flatten().cloned().collect()
    }

    /// No zones: nothing behaves differently.
    pub fn is_off(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rule(&self, name: &str) -> Option<&Rule> {
        self.rules.get(name)
    }

    /// The zones that admit staff (where a refused person is sent): those that admit them wholly when
    /// there are any, else those that admit them limited.
    pub fn admitting_staff(&self) -> Vec<String> {
        let wholly: Vec<String> = self.rules.iter().filter(|(_, r)| r.staff && !r.limited()).map(|(n, _)| n.clone()).collect();
        if !wholly.is_empty() {
            return wholly;
        }
        self.rules.iter().filter(|(_, r)| r.staff).map(|(n, _)| n.clone()).collect()
    }

    /// Whether `name` (a host row's zone) is one of the settings' zones.
    pub fn has(&self, name: &str) -> bool {
        self.rules.contains_key(name)
    }

    /// The zone of every request, when it does not depend on the request: none without zones, the
    /// pin on a pinned replica. `None`: ask [`Zones::place`].
    pub fn settled(&self) -> Option<Placed> {
        if self.is_off() {
            return Some(Placed::Nowhere);
        }
        self.pin.clone().map(Placed::In)
    }

    /// The zone of a request on an unpinned replica: the host row's zone (`host`, as read), else
    /// the first zone whose networks hold `address`, else the default, else none. A host that
    /// could not be read places the request nowhere that admits staff.
    pub fn place(&self, host: Result<Option<String>, crate::Error>, address: &str) -> Placed {
        if let Some(p) = self.settled() {
            return p;
        }
        match host {
            Err(e) => {
                tracing::warn!("the zone of a request's host could not be read: {e}");
                return Placed::Unknown;
            }
            Ok(Some(zone)) => return Placed::In(zone),
            Ok(None) => {}
        }
        if let Ok(ip) = address.parse::<IpAddr>()
            && let Some((name, _)) = self.rules.iter().find(|(_, r)| r.networks.iter().any(|n| n.contains(ip)))
        {
            return Placed::In(name.clone());
        }
        self.default.clone().map_or(Placed::Nowhere, Placed::In)
    }

    /// A replica pinned to a zone that admits no staff cannot run background work: saved automations,
    /// schedules and jobs run as the people who made them, who are staff.
    pub fn check_background(&self, run_background: bool) -> Result<(), String> {
        match self.pin.as_deref() {
            Some(pin) if run_background && self.rules.get(pin).is_some_and(|r| !r.staff) => Err(format!(
                "FERRUM_ZONE is {pin}, a zone that admits no staff, and FERRUM_RUN_BACKGROUND is on. Background work (saved automations, schedules, jobs) runs as the staff who set it up, so it cannot run here: set FERRUM_RUN_BACKGROUND=false on this replica and run background work on one in a zone that admits staff"
            )),
            _ => Ok(()),
        }
    }
}

/// Refusals counted since start, by zone, mode and credential (`ferrum_zone_refusals_total`).
static REFUSALS: Mutex<BTreeMap<(String, &'static str, &'static str), u64>> = Mutex::new(BTreeMap::new());

/// Counts and logs a staff credential the zone does not admit (refused, or admitted in report mode).
/// The log names the person, workspace, zone and kind of credential; never a secret.
pub fn count_refusal(entry: &Entry, tenant: uuid::Uuid, user: uuid::Uuid) {
    let mode = entry.zones.mode.as_str();
    *REFUSALS.lock().unwrap_or_else(|e| e.into_inner()).entry((entry.label().to_string(), mode, entry.credential.as_str())).or_default() += 1;
    tracing::warn!(
        person = %user, workspace = %tenant, zone = entry.label(), mode, credential = entry.credential.as_str(), address = %entry.address,
        "a staff credential in a zone that admits no staff"
    );
}

/// What level 2 kept from staff (`ferrum_zone_hidden_total`), by zone, mode and what: `admin` (an
/// administrator admitted as a member), `entity` (in report mode only, each check that would have
/// found an entity of an app the zone leaves out).
static HIDDEN: Mutex<BTreeMap<(String, &'static str, &'static str), u64>> = Mutex::new(BTreeMap::new());

pub fn count_hidden(zone: &str, mode: &'static str, what: &'static str) {
    *HIDDEN.lock().unwrap_or_else(|e| e.into_inner()).entry((zone.to_string(), mode, what)).or_default() += 1;
}

/// What level 2 kept from staff so far: `(zone, mode, what, count)`.
pub fn hidden() -> Vec<(String, &'static str, &'static str, u64)> {
    HIDDEN.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|((z, m, w), n)| (z.clone(), *m, *w, *n)).collect()
}

/// The entities a staff member reaches in a zone whose rule names apps: those of the apps'
/// packages, of every package they depend on (shared ones such as contacts come in this way) and,
/// with `custom: true`, the workspace's own. A package the workspace has not installed has no
/// entities; an entity of a package outside these is left out, whoever it was made by.
pub fn entities_seen(rule: &Rule, schema: &ferrum_meta::Schema) -> BTreeSet<String> {
    let packages = packages_seen(rule, schema);
    schema.entities.iter().filter(|(_, e)| if e.custom { rule.custom } else { packages.contains(e.package.as_str()) }).map(|(n, _)| n.clone()).collect()
}

/// The rule's packages and every package they depend on, through the workspace's schema.
pub fn packages_seen<'a>(rule: &'a Rule, schema: &'a ferrum_meta::Schema) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&str> = rule.packages.iter().map(String::as_str).collect();
    while let Some(p) = todo.pop() {
        if seen.insert(p) {
            todo.extend(schema.package_depends.get(p).into_iter().flatten().map(String::as_str));
        }
    }
    seen
}

/// The refusals counted so far: `(zone, mode, credential, count)`.
pub fn refusals() -> Vec<(String, &'static str, &'static str, u64)> {
    REFUSALS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|((z, m, c), n)| (z.clone(), *m, *c, *n)).collect()
}

/// The one refusal of a staff credential where the zone admits no staff (HTTP 403, code `zone`):
/// the credential is good, so it is not 401, which would send a client to sign in again here; it
/// is refused at this address and works at the staff address, which it names when the workspace
/// has one (as `staff_address` in the API's answer).
pub fn refusal(staff_address: Option<String>) -> crate::Error {
    let message = match &staff_address {
        Some(host) => format!("this part of the workspace is opened at its staff address, {host}, not here"),
        None => "this part of the workspace is opened at its staff address, not here".to_string(),
    };
    // ponytail: `pointer` carries the address (the server's error answer names it `staff_address`); a field of its
    // own if a second refusal needs one.
    crate::Error::Refused { code: "zone", status: 403, message, pointer: staff_address }
}

/// Staff: every kind but the outside people's (`portal`, `guest`) and machines' (`service`), so a
/// kind added later is staff until said otherwise.
pub fn is_staff(kind: &str) -> bool {
    !matches!(kind, "portal" | "guest" | "service")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zones(json: &str) -> Zones {
        Zones::parse(json, "", "", "").unwrap()
    }

    #[test]
    fn settings_are_read_strictly() {
        assert!(Zones::parse("", "", "", "").unwrap().is_off(), "nothing set: no zones");
        assert!(Zones::parse("{}", "", "", "").unwrap().is_off());
        let z = Zones::parse(r#"{"public": {"staff": "none", "scim": true}, "office": {"networks": ["10.0.0.0/8"]}}"#, "public", "office", "report").unwrap();
        assert_eq!(z.mode, Mode::Report);
        assert_eq!(
            z.rule("public"),
            Some(&Rule { staff: false, apps: None, packages: BTreeSet::new(), admin: false, custom: false, scim: true, networks: vec![] })
        );
        assert_eq!(z.rule("office").map(|r| (r.staff, r.admin, r.scim)), Some((true, true, false)), "staff all, admin follows it, scim off");
        let refused = |zones: &str, pin: &str, default: &str, mode: &str, says: &str| {
            let e = Zones::parse(zones, pin, default, mode).expect_err(zones);
            assert!(e.contains(says), "{zones} {pin} {default} {mode}: {e}");
        };
        refused("[1]", "", "", "", "JSON object");
        refused("{nope", "", "", "", "JSON object");
        refused(r#"{"p": {"staff": "none", "colour": "red"}}"#, "", "", "", "\"colour\"");
        refused(r#"{"P": {}}"#, "", "", "", "not a name");
        refused(r#"{"p": 3}"#, "", "", "", "not an object");
        refused(r#"{"p": {"staff": "some"}}"#, "", "", "", "neither");
        refused(r#"{"p": {"staff": []}}"#, "", "", "", "empty list");
        refused(r#"{"p": {"staff": ["CRM"]}}"#, "", "", "", "not an app's id");
        refused(r#"{"p": {"staff": [3]}}"#, "", "", "", "not an app's id");
        refused(r#"{"p": {"staff": "none", "custom": true}}"#, "", "", "", "only a rule with a list");
        refused(r#"{"p": {"staff": "all", "custom": false}}"#, "", "", "", "only a rule with a list");
        refused(r#"{"p": {"staff": ["crm"], "custom": "yes"}}"#, "", "", "", "true or false");
        refused(r#"{"p": {"staff": "none", "admin": true}}"#, "", "", "", "administrators are staff");
        refused(r#"{"p": {"scim": "yes"}}"#, "", "", "", "true or false");
        refused(r#"{"p": {"networks": ["10.0.0/8"]}}"#, "", "", "", "not a network");
        refused(r#"{"p": {"networks": ["10.0.0.1/8"]}}"#, "", "", "", "not a network");
        refused(r#"{"p": {"networks": ["10.0.0.0/33"]}}"#, "", "", "", "not a network");
        refused(r#"{"p": {"networks": "10.0.0.0/8"}}"#, "", "", "", "not a list");
        refused(r#"{"a": {"networks": ["10.0.0.0/8"]}, "b": {"networks": ["10.1.0.0/16"]}}"#, "", "", "", "the same addresses");
        refused(r#"{"p": {}}"#, "q", "", "", "FERRUM_ZONE is \"q\"");
        refused(r#"{"p": {}}"#, "", "q", "", "FERRUM_ZONE_DEFAULT is \"q\"");
        refused("", "p", "", "", "FERRUM_ZONE is \"p\"");
        refused(r#"{"p": {}}"#, "", "", "audit", "enforce or report");
    }

    #[test]
    fn level_2_rules_name_apps_and_resolve_to_their_packages() {
        let mut z = Zones::parse(
            r#"{"sales": {"staff": ["crm", "helpdesk", "crm"]}, "back": {"staff": "all", "admin": false}, "full": {}, "own": {"staff": ["crm"], "admin": true, "custom": true}}"#,
            "",
            "",
            "",
        )
        .unwrap();
        let sales = z.rule("sales").unwrap();
        assert_eq!(sales.apps, Some(vec!["crm".to_string(), "helpdesk".to_string()]), "each app once");
        assert!(!sales.admin && !sales.custom && sales.limited(), "a list: no administrators, no custom entities, unless said");
        assert!(z.rule("back").unwrap().limited() && z.rule("back").unwrap().apps.is_none(), "all, without administrators");
        assert!(!z.rule("full").unwrap().limited());
        let own = z.rule("own").unwrap();
        assert!(own.admin && own.custom && own.limited());
        assert!(sales.packages.is_empty(), "nothing until resolved: fail closed");
        assert_eq!(z.app_ids(), BTreeSet::from(["crm".to_string(), "helpdesk".to_string()]));
        let known = |app: &str| match app {
            "crm" => Some(vec!["crm".to_string()]),
            "helpdesk" => Some(vec!["helpdesk".to_string(), "helpdesk_extra".to_string()]),
            "empty" => Some(vec![]),
            _ => None,
        };
        z.resolve(known).unwrap();
        assert_eq!(z.rule("sales").unwrap().packages, BTreeSet::from(["crm", "helpdesk", "helpdesk_extra"].map(String::from)));
        let mut unknown = Zones::parse(r#"{"p": {"staff": ["nope"]}}"#, "", "", "").unwrap();
        assert!(unknown.resolve(known).unwrap_err().contains("does not know"));
        let mut empty = Zones::parse(r#"{"p": {"staff": ["empty"]}}"#, "", "", "").unwrap();
        assert!(empty.resolve(known).unwrap_err().contains("no packages"));
        let entry =
            |placed: &str| Entry { zones: Arc::new(z.clone()), placed: Placed::In(placed.into()), address: String::new(), credential: Credential::Cookie };
        assert!(entry("sales").admits_staff() && entry("sales").limit().is_some(), "admitted, and limited");
        assert!(entry("full").limit().is_none());
    }

    #[test]
    fn networks_hold_addresses_of_their_family() {
        let n = Net::parse("10.0.0.0/8").unwrap();
        assert!(n.contains("10.200.3.4".parse().unwrap()) && !n.contains("11.0.0.1".parse().unwrap()));
        assert!(n.contains("::ffff:10.1.2.3".parse().unwrap()), "an IPv4 caller on an IPv6 socket");
        let v6 = Net::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1::5".parse().unwrap()) && !v6.contains("2001:db9::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));
        assert!(Net::parse("192.0.2.7").unwrap().contains("192.0.2.7".parse().unwrap()), "one address");
        assert!(Net::parse("0.0.0.0/0").unwrap().contains("203.0.113.1".parse().unwrap()));
        assert!(Net::parse("::/0").unwrap().contains("2001:db8::1".parse().unwrap()));
        for bad in ["", "10.0.0.0/", "10.0.0.0/+8", "10.0.0.0/8/8", "local", "2001:db8::1/129"] {
            assert!(Net::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_request_is_placed_in_order() {
        let json = r#"{"public": {"staff": "none"}, "office": {"networks": ["10.0.0.0/8", "fd00::/8"]}}"#;
        let off = zones("");
        assert_eq!(off.place(Ok(Some("public".into())), "10.0.0.1"), Placed::Nowhere, "no zones: nothing is placed");
        let pinned = Zones::parse(json, "public", "", "").unwrap();
        assert_eq!(pinned.place(Ok(Some("office".into())), "10.0.0.1"), Placed::In("public".into()), "the pin wins over host and network");
        assert_eq!(pinned.place(Err(crate::Error::Db("down".into())), "10.0.0.1"), Placed::In("public".into()), "and needs no host");
        let z = Zones::parse(json, "", "public", "").unwrap();
        assert_eq!(z.settled(), None);
        assert_eq!(z.place(Ok(Some("office".into())), "203.0.113.9"), Placed::In("office".into()), "the host's zone");
        assert_eq!(z.place(Ok(None), "10.9.8.7"), Placed::In("office".into()), "then the network");
        assert_eq!(z.place(Ok(None), "fd12::1"), Placed::In("office".into()), "IPv6 too");
        assert_eq!(z.place(Ok(None), "203.0.113.9"), Placed::In("public".into()), "then the default");
        assert_eq!(z.place(Ok(None), "local"), Placed::In("public".into()), "no address: the default");
        assert_eq!(z.place(Err(crate::Error::Db("down".into())), "10.0.0.1"), Placed::Unknown, "a host that cannot be read places nowhere staff go");
        assert_eq!(zones(json).place(Ok(None), "203.0.113.9"), Placed::Nowhere, "no default: no zone");
    }

    #[test]
    fn who_a_zone_admits() {
        let z = Arc::new(Zones::parse(r#"{"public": {"staff": "none"}, "edge": {"staff": "none", "scim": true}, "office": {}}"#, "", "", "").unwrap());
        let at = |placed: Placed, credential: Credential| Entry { zones: z.clone(), placed, address: "203.0.113.9".into(), credential };
        assert!(at(Placed::Nowhere, Credential::Cookie).admits_staff());
        assert!(at(Placed::In("office".into()), Credential::Cookie).admits_staff());
        assert!(at(Placed::In("office".into()), Credential::Scim).admits_staff(), "a scim key where staff work");
        assert!(!at(Placed::In("public".into()), Credential::Cookie).admits_staff());
        assert!(!at(Placed::In("public".into()), Credential::Scim).admits_staff(), "scim: false");
        assert!(at(Placed::In("edge".into()), Credential::Scim).admits_staff(), "scim: true");
        assert!(!at(Placed::In("edge".into()), Credential::ApiKey).admits_staff(), "only the scim key");
        assert!(!at(Placed::In("gone".into()), Credential::Cookie).admits_staff(), "a zone the settings no longer name");
        assert!(!at(Placed::Unknown, Credential::Scim).admits_staff(), "undecided: fail closed");
        assert!(at(Placed::In("public".into()), Credential::Cookie).refuses_staff());
        let report = Arc::new(Zones::parse(r#"{"public": {"staff": "none"}}"#, "", "", "report").unwrap());
        assert!(
            !Entry { zones: report, placed: Placed::In("public".into()), address: String::new(), credential: Credential::Cookie }.refuses_staff(),
            "report admits"
        );
        assert_eq!(z.admitting_staff(), vec!["office".to_string()]);
        assert!(is_staff("member") && is_staff("support") && is_staff("something-new"));
        assert!(!is_staff("portal") && !is_staff("guest") && !is_staff("service"));
    }

    #[test]
    fn background_work_needs_staff() {
        let z = Zones::parse(r#"{"public": {"staff": "none"}, "office": {}}"#, "public", "", "").unwrap();
        let e = z.check_background(true).unwrap_err();
        assert!(e.contains("FERRUM_RUN_BACKGROUND=false") && e.contains("automations"), "{e}");
        assert!(z.check_background(false).is_ok());
        assert!(Zones::parse(r#"{"public": {"staff": "none"}, "office": {}}"#, "office", "", "").unwrap().check_background(true).is_ok());
        assert!(Zones::parse(r#"{"public": {"staff": "none"}}"#, "", "", "").unwrap().check_background(true).is_ok(), "unpinned");
    }
}
