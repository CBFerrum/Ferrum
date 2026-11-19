//! What the app shell needs from the kernel (docs/plan/apps-ui.md): browser sessions per device,
//! host names that lead to a workspace, the kinds of people, and interface releases.

use crate::auth::Session;
use crate::{Ctx, Error, Kernel, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a replica trusts its answer about a session or a host before asking the database
/// again. Revocation on the same replica is immediate; on others it takes at most this long.
const CACHE_TTL: Duration = Duration::from_secs(20);
/// `last_seen_at` is written at most this often per session…
const SEEN_EVERY: chrono::Duration = chrono::Duration::minutes(5);
/// …and this often where the workspace ends idle sessions (`idle_minutes`), so that the timeout holds to
/// within a minute and the cache's 20 seconds.
const SEEN_EVERY_IDLE: chrono::Duration = chrono::Duration::minutes(1);
/// The shortest idle timeout a workspace may set, well above that precision.
pub const IDLE_MIN_MINUTES: i64 = 5;
/// And the longest: a week.
pub const IDLE_MAX_MINUTES: i64 = 7 * 24 * 60;
/// Hosts whose domain stopped pointing at us are marked unverified after this many failed checks.
pub const HOST_MAX_FAILURES: i32 = 7;
/// A released host (renamed or deleted workspace) redirects to the workspace for this long…
pub const HOST_REDIRECT_DAYS: i64 = 90;
/// …and nobody may claim it for this long after its release.
pub const HOST_QUARANTINE_DAYS: i64 = 365;
/// How long one check waits for an answer about a domain's DNS record (twice at most).
const RECORD_TIMEOUT: Duration = Duration::from_secs(2);

/// How a domain of a workspace's own is proven before it opens anything (`FERRUM_DOMAIN_PROOF`, docs/operations.md, "Custom
/// domains"). Pointing here proves only that: the cluster itself serves the token it checks for. Its record proves that
/// the domain's owner gave it to this workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DomainProof {
    /// `/.well-known/ferrum-domain` there answers its token (as before the setting).
    #[default]
    Points,
    /// That, and its DNS holds `_ferrum.{host}` TXT `ferrum-domain={token}` when it first becomes verified.
    Record,
}

impl DomainProof {
    /// The setting as written: `points`, `record`, or empty for the default, `record` on a cluster of many workspaces
    /// (one with a base domain) and `points` otherwise.
    pub fn from_setting(raw: &str, base_domain: bool) -> std::result::Result<DomainProof, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" if base_domain => Ok(DomainProof::Record),
            "" | "points" => Ok(DomainProof::Points),
            "record" => Ok(DomainProof::Record),
            other => Err(format!("FERRUM_DOMAIN_PROOF ({other}) must be points or record")),
        }
    }
}

/// Proof by record (`FERRUM_DOMAIN_PROOF=record`): an older build verifies a domain that only points here. Dated right
/// after main's last commit without it (fbfb38be, 2026-11-15T00:50:24Z) and before the merge that brought it.
pub const PROOF_FLOOR: crate::window::Floor = crate::window::Floor {
    feature: "domain_proof",
    since: "2026-11-15T00:51:00Z",
    what: "domains of a workspace's own open only once their DNS record proves them",
    temporary: false,
};

/// `FERRUM_DNS_RESOLVERS`: addresses, comma-separated, each with a port or on 53 (`10.0.0.2`, `10.0.0.2:5353`,
/// `[2001:db8::2]:53`). Empty: the system's resolvers.
pub fn dns_resolvers(raw: &str) -> std::result::Result<Vec<std::net::SocketAddr>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| {
            a.parse::<std::net::SocketAddr>()
                .or_else(|_| a.parse::<std::net::IpAddr>().map(|ip| std::net::SocketAddr::new(ip, 53)))
                .map_err(|_| format!("FERRUM_DNS_RESOLVERS: {a} is not an address (such as 10.0.0.2 or 10.0.0.2:53)"))
        })
        .collect()
}
/// Which live host of a target becomes its main one when it has none (the first it gets, or the one left when the
/// main one is released): a subdomain first, then the oldest of any kind (custom domains and the operator's hosts
/// alike), then by name. SQL over the alias `c`.
const SUCCESSION: &str = "c.kind <> 'subdomain', c.created_at, c.host";
/// Interface files stay this long after no workspace uses their release.
pub const UI_RETENTION_DAYS: i64 = 30;
/// A sensitive action needs the person to have proved themselves again this recently.
pub const STEP_UP_SECS: i64 = 600;

/// A browser session as the people who own it, and administrators, see it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DeviceSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub actor: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub user_agent: String,
    pub ip: String,
    /// The zone it was opened in (crate::zones), if any.
    pub zone: Option<String>,
}

/// What a valid browser session adds to who the person is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    /// Who really acts, when someone acts as the session's person.
    pub actor: Option<Uuid>,
    pub step_up_at: Option<DateTime<Utc>>,
}

impl SessionState {
    /// Whether the person proved themselves again recently enough for a sensitive action.
    pub fn stepped_up(&self) -> bool {
        self.step_up_at.is_some_and(|t| Utc::now() - t < chrono::Duration::seconds(STEP_UP_SECS))
    }
}

/// A browser session whose person is confirming it is them (docs/operations.md, "Asking again before sensitive work"):
/// every proof is bound to the session and its person, and marks only that session ([`Kernel::stepped_up`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepUp {
    pub tenant: Uuid,
    pub user: Uuid,
    pub session: Uuid,
    /// The account whose password and passkeys count: `(tenant, user)` itself, or, for a workspace account joined to a
    /// Ferrum account on a host that hands sign-in to the accounts host, that account (the directory's).
    pub keys: (Uuid, Uuid),
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TenantHost {
    pub host: String,
    pub kind: String,
    pub target: String,
    pub is_primary: bool,
    pub verify_token: String,
    pub verified_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    pub failures: i32,
    pub released_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// A released host still serves the people signed in there until then ([`Kernel::release_host`]).
    pub leaving_until: Option<DateTime<Utc>>,
    /// The zone the operator put it in (crate::zones); only the operator sets it.
    pub zone: Option<String>,
    /// What it shows ([`HostShape`]): a `shell` address only.
    pub home: Option<String>,
    pub apps: Option<Vec<String>>,
    pub label: Option<String>,
    /// The name an administrator gave it under the base domain (`{base_name}--{slug}.{base}`, [`name_under_base`]).
    pub base_name: Option<String>,
    /// When its owner's DNS record proved a domain of its own ([`DomainProof::Record`]); none: never.
    pub proved_at: Option<DateTime<Utc>>,
    /// It forwards ([`HostShape::forward`]).
    pub forward: bool,
    /// The Brand its pages show ([`Kernel::set_host_brand`]); none: the workspace's default.
    pub brand: Option<Uuid>,
}

/// What one check of a domain of a workspace's own found ([`Kernel::check_host`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCheck {
    /// It works: verified now.
    pub works: bool,
    /// It points here: the token answered from this cluster.
    pub points: bool,
    /// Its record proved it; `None`: not asked (it does not point here, the cluster asks for no record, or it was proven).
    pub record: Option<bool>,
}

/// What a `shell` address shows (docs/operations.md, "Addresses of a workspace"), as its administrators set it. It chooses
/// where a person lands and what the bar lists there, never what the address admits: every app opens on every `shell`
/// address, by link or typed path, and only the operator's zone keeps anything out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct HostShape {
    /// The app `/` opens there (its launcher route), for whoever may open that app; Home for everyone else.
    pub home: Option<String>,
    /// The apps the launcher and the bar's search list there, with a way to the main address for the rest; `None`: all.
    pub apps: Option<Vec<String>>,
    /// Its name beside the workspace's in the bar ("Sales").
    pub label: Option<String>,
    /// It serves no workspace, only sends each page on to the main address, `/` to `home` there (docs/operations.md,
    /// "Addresses of a workspace"): a front door with no sign-in, storage or bar of its own.
    #[serde(default)]
    pub forward: bool,
}

/// A change to a [`HostShape`]: `None` leaves a part as it is, `Some(None)` clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostPatch {
    pub home: Option<Option<String>>,
    pub apps: Option<Option<Vec<String>>>,
    pub label: Option<Option<String>>,
    pub forward: Option<bool>,
}

/// What a request's host shows ([`Kernel::host_shown`]): its shape, and whether it is the workspace's main address.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostShown {
    pub shape: HostShape,
    pub primary: bool,
    /// The Brand its pages show (crate::brand); none: the workspace's default.
    pub brand: Option<Uuid>,
}

/// How many live addresses a workspace holds when its plan names no `addresses` quota (crate::plans): as before quotas.
pub const ADDRESSES_DEFAULT: i64 = 20;
/// At most this many apps on one address, and this many short paths in one workspace.
pub const SHAPE_APPS_MAX: usize = 50;
pub const PATHS_MAX: usize = 50;
/// Names an administrator may not give an address under the base domain: they read as the service's own, or (`xn`, as in
/// `xn--…`) as a name in another script. The server adds its own hosts' first labels.
pub const RESERVED_NAMES: &[&str] = &["accounts", "hooks", "www", "api", "admin", "login", "mail", "xn"];
/// What an operator's address answers when an administrator tries to change it.
pub const OPERATOR_OWNS: &str = "this address is the operator's: only the operator changes or removes it";

/// `{name}--{slug}.{base}`: a name a workspace gives itself under the cluster's base domain. One DNS label, so the base's
/// wildcard certificate covers it, and never another workspace's: it ends in this workspace's own slug, a slug is never
/// another workspace's (`tenant_slug_retired`), and no slug made since this form exists holds `--`.
pub fn name_under_base(name: &str, slug: &str, base: &str) -> Result<String> {
    let shaped = (1..=30).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if !shaped {
        return Err(Error::field("name", "use 1 to 30 lowercase letters, digits or single hyphens, not at either end"));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(Error::field("name", "is kept for the service's own addresses"));
    }
    let base = base.trim().trim_matches('.').to_ascii_lowercase();
    let host = format!("{name}--{slug}.{base}");
    match normalize_host(&host) {
        Some(h) if h == host => Ok(h),
        _ => Err(Error::field("name", format!("{name}--{slug} does not fit in one part of an address (63 characters at most)"))),
    }
}

/// A shape as an administrator sets it, tidied (`label` trimmed, `apps` without repeats), or refused: only a `shell`
/// address has one; `home` and each of `apps` an app's id, at most [`SHAPE_APPS_MAX`]; `label` 1 to 40 characters with no
/// control characters. Which apps are the workspace's the server checks.
pub fn check_shape(target: &str, shape: &HostShape) -> Result<HostShape> {
    if shape == &HostShape::default() {
        return Ok(HostShape::default());
    }
    if target != "shell" {
        return Err(Error::field("target", "home, apps, label and forward are for addresses that open the workspace (shell)"));
    }
    // A forwarding address shows no bar: a launcher's apps and a label would mean nothing there.
    if shape.forward && shape.apps.is_some() {
        return Err(Error::field("apps", "means nothing on an address that forwards: it shows no bar"));
    }
    if shape.forward && shape.label.as_deref().is_some_and(|l| !l.trim().is_empty()) {
        return Err(Error::field("label", "means nothing on an address that forwards: it shows no bar"));
    }
    let app_id = |s: &str| {
        (2..=40).contains(&s.len())
            && s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if shape.home.as_deref().is_some_and(|h| !app_id(h)) {
        return Err(Error::field("home", "is not an app's id"));
    }
    let mut apps: Option<Vec<String>> = None;
    if let Some(list) = &shape.apps {
        if list.len() > SHAPE_APPS_MAX {
            return Err(Error::field("apps", format!("at most {SHAPE_APPS_MAX} apps")));
        }
        let mut out: Vec<String> = Vec::with_capacity(list.len());
        for a in list {
            if !app_id(a) {
                return Err(Error::field("apps", format!("{a:?} is not an app's id")));
            }
            if !out.contains(a) {
                out.push(a.clone());
            }
        }
        apps = Some(out);
    }
    let label = match shape.label.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(l) if l.chars().count() > 40 || l.chars().any(char::is_control) => return Err(Error::field("label", "1 to 40 characters of plain text")),
        Some(l) => Some(l.to_string()),
    };
    Ok(HostShape { home: shape.home.clone(), apps, label, forward: shape.forward })
}

/// A short path of a workspace (docs/operations.md, "Addresses of a workspace", short paths): `/{alias}` and `/{alias}/…` on every `shell` address
/// redirect to `target`, a path of the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ShortPath {
    pub alias: String,
    pub target: String,
}

/// A short path's own form: 2 to 30 lowercase letters, digits or hyphens, starting with a letter (as an app's route).
pub fn alias_shaped(alias: &str) -> bool {
    let b = alias.as_bytes();
    (2..=30).contains(&b.len()) && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A target's own form: a path on the same host (`/`, not `//` or `/\`), without a fragment or control characters, at most
/// 2,000 characters. Which first segments it may have the server checks.
pub fn target_shaped(target: &str) -> bool {
    target.starts_with('/')
        && !target.starts_with("//")
        && !target.starts_with("/\\")
        && !target.contains(['\\', '#', ' '])
        && !target.chars().any(char::is_control)
        && target.len() <= 2000
}

/// Where a host name leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRoute {
    /// A host of this workspace, serving `target` (`shell`, `portal`, `site:{app}`).
    Serve { tenant: Uuid, target: String },
    /// A released host still redirecting: to the workspace's primary host for the same target
    /// (`None` when the workspace has none any more).
    Moved { tenant: Uuid, to: Option<String> },
    /// A released host in its leaving period: it still serves `target` to the people already signed in
    /// there, until `until`, and redirects everyone else to `to` (as [`HostRoute::Moved`]).
    Leaving { tenant: Uuid, target: String, to: Option<String>, until: DateTime<Utc> },
    /// A forwarding address ([`HostShape::forward`]): it serves nothing, and sends each page on to `to`, the workspace's
    /// main address (`None` when it has none), `/` to `home`'s first launcher route there (`None` in a zone, where it names
    /// only the main address).
    Forward { tenant: Uuid, to: Option<String>, home: Option<String> },
    /// Not one of ours, or quarantined.
    Unknown,
}

/// The kinds of people (`app_user.kind`).
pub const USER_KINDS: [&str; 4] = ["member", "guest", "portal", "service"];

/// An interface release: the files of an app's UI, named by the digest of their contents.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct UiRelease {
    pub app: String,
    pub digest: String,
    /// Path (under the release) → `{"size", "type"}`.
    pub files: Value,
    /// The entry page split around where the shell goes: `{"head", "body", "tail"}`.
    pub entry: Value,
    pub created_at: DateTime<Utc>,
}

/// A host name as typed or sent in a `Host` header → its canonical form (lowercase, no port, no
/// trailing dot), or `None` when it cannot be one of ours.
pub fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    // IPv6 literals and ports.
    let h = if h.starts_with('[') { return None } else { h.split(':').next().unwrap_or_default().to_string() };
    let label_ok =
        |l: &str| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    (h.len() <= 253 && h.contains('.') && h.split('.').all(label_ok)).then_some(h)
}

/// Whether the DNS of `host` holds the record that proves `token` (docs/operations.md, "Custom domains"): at
/// `_ferrum.{host}` itself a TXT record whose strings, joined, read `ferrum-domain={token}`, and no other `ferrum-domain=`
/// value beside it. Asked of `resolvers` (the system's when there are none) with a short timeout, by a resolver made for
/// this one question and dropped after it, so no answer is kept. A failure, no answer, two different values, or an alias
/// (CNAME) there, which would hand the proof to whoever holds the name it leads to (a wildcard alias to the cluster, say):
/// no.
pub async fn domain_record(resolvers: &[std::net::SocketAddr], host: &str, token: &str) -> bool {
    use hickory_resolver::TokioResolver;
    use hickory_resolver::config::{NameServerConfig, ResolverConfig};
    use hickory_resolver::proto::rr::{Name, RData, RecordType};
    let Some(name) = normalize_host(host).and_then(|h| Name::from_ascii(format!("_ferrum.{h}.")).ok()) else { return false };
    let mut builder = if resolvers.is_empty() {
        match TokioResolver::builder_tokio() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("the system's DNS resolvers cannot be read, so no domain is proven by its record: {e}");
                return false;
            }
        }
    } else {
        let servers = resolvers
            .iter()
            .map(|a| {
                let mut s = NameServerConfig::udp_and_tcp(a.ip());
                s.connections.iter_mut().for_each(|c| c.port = a.port());
                s
            })
            .collect();
        TokioResolver::builder_with_config(ResolverConfig::from_name_servers(servers), Default::default())
    };
    let opts = builder.options_mut();
    (opts.timeout, opts.attempts, opts.cache_size) = (RECORD_TIMEOUT, 2, 0);
    let Ok(resolver) = builder.build() else { return false };
    let answer = match resolver.lookup(name.clone(), RecordType::TXT).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(host, "no record proves the domain: {e}");
            return false;
        }
    };
    let mut values = std::collections::BTreeSet::new();
    for r in answer.answers() {
        match &r.data {
            RData::CNAME(_) => return false,
            RData::TXT(txt) if r.name == name => {
                let value: Vec<u8> = txt.txt_data.iter().flat_map(|s| s.iter().copied()).collect();
                if value.starts_with(b"ferrum-domain=") {
                    values.insert(value);
                }
            }
            _ => {}
        }
    }
    values.len() == 1 && values.contains(format!("ferrum-domain={token}").as_bytes())
}

/// A workspace's slugs when its rows leave this database for good (deleted; moved away and finished), in `tx` before they
/// go: retired, as the slug guard retires every slug given up (that trigger does not fire while these deletions turn
/// triggers off), beside those it gave up before. `tenant_slug_retired` is the installation's record, not the workspace's
/// rows: a deletion passes over it and a move leaves it behind, so no other workspace takes one of them here again (nor
/// forms the names and apps' origins they named), while the workspace itself, coming back with its id, does. The guard's
/// own lock comes first, before the workspace's row goes: one made with that slug at that moment waits, then finds it
/// retired.
pub(crate) async fn retire_slug(tx: &mut crate::db::Tx, tenant: Uuid) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum-slug:' || slug, 0)) from tenant where id = $1").bind(tenant).execute(&mut **tx).await?;
    sqlx::query("insert into tenant_slug_retired (slug, tenant_id) select slug, id from tenant where id = $1 on conflict (slug) do nothing")
        .bind(tenant)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A workspace's addresses when its rows leave this database for good (deleted; moved away and finished), in `tx` before
/// they go: kept from every other workspace as released ones are ([`HOST_QUARANTINE_DAYS`] from now, or from the end of a
/// released one's leaving period), with no leaving or redirect period, since nothing is left to serve. All but a domain
/// of its own that never worked, which a removal drops too. `moved`: it moved away and may come back, so they are kept as
/// its own to take back (crate::workspace_move); a deleted workspace's are no one's. One kept already stays whose it was.
pub(crate) async fn quarantine_hosts(tx: &mut crate::db::Tx, tenant: Uuid, moved: bool) -> Result<()> {
    sqlx::query(
        "insert into host_quarantine (host, until, workspace) \
         select host, coalesce(greatest(released_at, leaving_until), now()) + make_interval(days => $2), case when $3 then tenant_id end \
         from tenant_host where tenant_id = $1 and (kind <> 'custom' or verified_at is not null or released_at is not null) \
         on conflict (host) do update set until = greatest(host_quarantine.until, excluded.until)",
    )
    .bind(tenant)
    .bind(HOST_QUARANTINE_DAYS as i32)
    .bind(moved)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Kernel {
    // ── sessions ──

    /// Records a new browser session for `s` (acted in by `actor` when someone else acts as the
    /// person) and returns its id (the cookie's token names it). Signing in proves the person, so
    /// a new session counts as stepped up.
    pub async fn open_session(&self, s: &Session, ttl: Duration, user_agent: &str, ip: &str, actor: Option<Uuid>) -> Result<Uuid> {
        self.open_session_in(s, ttl, user_agent, ip, actor, None).await
    }

    /// [`Kernel::open_session`] in a zone (crate::zones), recorded on the session.
    pub async fn open_session_in(&self, s: &Session, ttl: Duration, user_agent: &str, ip: &str, actor: Option<Uuid>, zone: Option<&str>) -> Result<Uuid> {
        self.open_session_with(s, ttl, user_agent, ip, actor, zone, None).await
    }

    /// [`Kernel::open_session_in`], keeping what a SAML sign-in's logout needs on the session
    /// ([`Kernel::sso_finish_full`]).
    #[allow(clippy::too_many_arguments)]
    pub async fn open_session_with(
        &self,
        s: &Session,
        ttl: Duration,
        user_agent: &str,
        ip: &str,
        actor: Option<Uuid>,
        zone: Option<&str>,
        saml: Option<&Value>,
    ) -> Result<Uuid> {
        self.open_session_proved(s, ttl, user_agent, ip, actor, zone, saml, &[]).await
    }

    /// [`Kernel::open_session_with`], keeping how its person proved who they are as they signed in (`proof`: the
    /// steps, `password` then `authenticator`, `passkey`, `sso`, `code`, `accounts`; an ID token's `amr`, crate::oidc).
    #[allow(clippy::too_many_arguments)]
    pub async fn open_session_proved(
        &self,
        s: &Session,
        ttl: Duration,
        user_agent: &str,
        ip: &str,
        actor: Option<Uuid>,
        zone: Option<&str>,
        saml: Option<&Value>,
        proof: &[&str],
    ) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let ua: String = user_agent.chars().filter(|c| !c.is_control()).take(300).collect();
        let ip: String = ip.chars().take(64).collect();
        let mut tx = self.db.tenant_tx(s.tenant).await?;
        sqlx::query(
            "insert into user_session (tenant_id, id, user_id, actor, step_up_at, expires_at, user_agent, ip, zone, saml, proof) \
             values ($1, $2, $3, $4, now(), now() + make_interval(secs => $5), $6, $7, $8, $9, $10)",
        )
        .bind(s.tenant)
        .bind(id)
        .bind(s.user)
        .bind(actor)
        .bind(ttl.as_secs_f64())
        .bind(ua)
        .bind(ip)
        .bind(zone)
        .bind(saml)
        .bind((!proof.is_empty()).then_some(proof))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Whether the browser session `sid` of `user` is still valid (not revoked, not expired, not idle
    /// longer than the workspace allows), and what it adds. Cached briefly; a request through it is
    /// activity, and refreshes `last_seen_at` now and then.
    pub async fn check_session(&self, tenant: Uuid, user: Uuid, sid: Uuid) -> Result<SessionState> {
        self.check_session_as(tenant, user, sid, true).await
    }

    /// [`Kernel::check_session`]; `active`: whether the use is the person's activity (a socket's own
    /// recheck is not). A session idle longer than the workspace's `idle_minutes` is ended here, so it
    /// cannot come back, and answered as one signed out.
    pub async fn check_session_as(&self, tenant: Uuid, user: Uuid, sid: Uuid, active: bool) -> Result<SessionState> {
        let expired = || Error::Unauthorized("this device was signed out; please sign in again".into());
        if let Some(e) = self.sessions.get(&(tenant, sid))
            && e.1.elapsed() < CACHE_TTL
        {
            return e.0.filter(|(owner, _)| *owner == user).map(|(_, state)| state).ok_or_else(expired);
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        type Row = (Uuid, DateTime<Utc>, Option<Uuid>, Option<DateTime<Utc>>, i32, bool);
        // A setting that cannot be read as a number fails the query, which refuses. A workspace that takes no changes
        // just now (moving, or the database fenced) is still read: its sessions are checked, not written.
        let row: Option<Row> = sqlx::query_as(
            "select user_id, last_seen_at, actor, step_up_at, \
             (select coalesce((settings->>'idle_minutes')::int, 0) from tenant where id = $1), current_setting('transaction_read_only') = 'on' from user_session \
             where tenant_id = $1 and id = $2 and revoked_at is null and expires_at > now()",
        )
        .bind(tenant)
        .bind(sid)
        .fetch_optional(&mut *tx)
        .await?;
        let idle = row.as_ref().map_or(0, |r| i64::from(r.4));
        let writable = row.as_ref().is_some_and(|r| !r.5);
        if let Some(r) = row.as_ref().filter(|r| writable && r.0 == user && idle > 0 && Utc::now() - r.1 > chrono::Duration::minutes(idle)) {
            self.end_idle(&mut tx, tenant, sid, r.1).await?;
            tx.commit().await?;
            self.sessions.insert((tenant, sid), (None, Instant::now()));
            return Err(expired());
        }
        let row_owner = row.as_ref().map(|r| r.0);
        let state = row.filter(|r| r.0 == user).map(|r| (r.1, SessionState { actor: r.2, step_up_at: r.3 }));
        let every = if idle > 0 { SEEN_EVERY_IDLE } else { SEEN_EVERY };
        if let Some((seen, _)) = state
            && active
            && writable
            && Utc::now() - seen > every
        {
            sqlx::query("update user_session set last_seen_at = now() where tenant_id = $1 and id = $2").bind(tenant).bind(sid).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let state = state.map(|s| s.1);
        if self.sessions.len() > 200_000 {
            self.sessions.retain(|_, v| v.1.elapsed() < CACHE_TTL);
        }
        // Cached with its owner: a session id presented with another person is no session.
        let owner = row_owner.filter(|_| state.is_some());
        self.sessions.insert((tenant, sid), (owner.zip(state), Instant::now()));
        state.ok_or_else(expired)
    }

    /// Ends a session that was idle too long (`seen`: when it was last used), and the device's pushes.
    async fn end_idle(&self, tx: &mut crate::db::Tx, tenant: Uuid, sid: Uuid, seen: DateTime<Utc>) -> Result<()> {
        sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and id = $2 and revoked_at is null")
            .bind(tenant)
            .bind(sid)
            .execute(&mut **tx)
            .await?;
        sqlx::query("delete from push_subscription where tenant_id = $1 and session_id = $2").bind(tenant).bind(sid).execute(&mut **tx).await?;
        crate::gate::session_ended(tx, tenant, sid, "idle").await?;
        tracing::info!(workspace = %tenant, session = %sid, last_seen = %seen, "a session idle too long was ended");
        Ok(())
    }

    /// Records that the person of session `sid` proved themselves again (password, passkey or a
    /// second factor) for a sensitive action.
    pub async fn step_up_session(&self, tenant: Uuid, sid: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let user: Option<Uuid> =
            // No way is said here: an ID token then says nothing of how (crate::oidc), rather than something untrue.
            sqlx::query_scalar("update user_session set step_up_at = now(), proof = null where tenant_id = $1 and id = $2 and revoked_at is null returning user_id")
                .bind(tenant)
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await?;
        // Every other replica drops what it holds of the person's sessions at once, so the interface's retry is let
        // through wherever it lands (crate::auth::USERS_CHANNEL).
        if let Some(user) = user {
            crate::auth::notify_users(&mut tx, tenant, Some(user)).await?;
        }
        tx.commit().await?;
        self.sessions.remove(&(tenant, sid));
        Ok(())
    }

    /// The ways the person of `who` can confirm it is them now, on `host`, in order of preference: `{"way": "passkey"}` (a
    /// passkey of theirs for a relying party the host may use), `{"way": "accounts_passkey"}` (failing that, a passkey of
    /// their Ferrum account kept for the accounts host `accounts`, which the dialog asks for there, in a small window:
    /// crate::sso, [`Kernel::accounts_step_up_begin`]), `{"way": "password", "code": bool}` (a password; `code`: the code
    /// from an authenticator app is asked too), `{"way": "provider", "name": …}` (an identity linked to them at an enabled
    /// single sign-on provider). Where the workspace requires single sign-on of them, neither a password nor a passkey
    /// proves anything: only the provider. Empty: no way at all.
    pub async fn step_up_ways(&self, who: &StepUp, host: &str, accounts: Option<&str>) -> Result<Vec<Value>> {
        // Only a workspace account joined to a Ferrum account, on a host that hands sign-in to the accounts host, has its
        // passkeys there (`keys`, set by whoever knows the hosts).
        let at_accounts = match accounts {
            Some(a) if who.keys != (who.tenant, who.user) => self.step_up_rps(a),
            _ => Vec::new(),
        };
        let mut tx = self.db.system_tx().await?;
        let row: Option<(bool, bool, bool, bool, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {}, exists (select 1 from passkey p where p.tenant_id = k.tenant_id and p.user_id = k.id and p.rp_id = any($5)), \
               exists (select 1 from passkey p where p.tenant_id = k.tenant_id and p.user_id = k.id and p.rp_id = any($6)), \
               coalesce(k.password_hash is not null, false), coalesce(k.totp_enabled_at is not null, false) \
             from app_user u join tenant t on t.id = u.tenant_id left join app_user k on k.tenant_id = $3 and k.id = $4 and k.is_active \
             where u.tenant_id = $1 and u.id = $2 and u.is_active and t.status = 'active'",
            crate::gate::SSO_ONLY
        )))
        .bind(who.tenant)
        .bind(who.user)
        .bind(who.keys.0)
        .bind(who.keys.1)
        .bind(self.step_up_rps(host))
        .bind(at_accounts)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((sso_only, passkey, accounts_passkey, password, code)) = row else { return Ok(Vec::new()) };
        let mut ways = Vec::new();
        if passkey && !sso_only {
            ways.push(serde_json::json!({"way": "passkey"}));
        } else if accounts_passkey && !sso_only {
            ways.push(serde_json::json!({"way": crate::sso::ACCOUNTS_PASSKEY}));
        }
        if password && !sso_only {
            ways.push(serde_json::json!({"way": "password", "code": code}));
        }
        if let Some((_, name)) = self.linked_provider(who.tenant, who.user).await? {
            ways.push(serde_json::json!({"way": "provider", "name": name}));
        }
        Ok(ways)
    }

    /// The person of `who` proved it is them (`way`: `password`, `passkey`, `accounts_passkey`, `provider`): their browser
    /// session counts as stepped up, and the audit trail says so (sign-in, `stepped_up`, with where it came from), in one
    /// transaction, so that neither is without the other. Only the session of that person, live; nothing else about it
    /// changes (whose it is, when it ends). Every other replica drops what it holds of the person's sessions at once.
    pub async fn stepped_up(&self, who: &StepUp, way: &str, from: &crate::gate::Arrival) -> Result<()> {
        self.stepped_up_by(who, &[way], from).await
    }

    /// [`Kernel::stepped_up`] by more than one proof given together (`password` then `authenticator` or
    /// `recovery_code`): every one is how the session's person last proved who they are (an ID token's `amr`,
    /// crate::oidc); the trail names the first as the way, as before.
    pub async fn stepped_up_by(&self, who: &StepUp, ways: &[&str], from: &crate::gate::Arrival) -> Result<()> {
        let way = ways.first().copied().ok_or_else(|| Error::invalid("a step-up names how the person proved it is them"))?;
        let mut tx = self.db.system_tenant_tx(who.tenant).await?;
        let marked = sqlx::query(
            "update user_session set step_up_at = now(), proof = $4 where tenant_id = $1 and id = $2 and user_id = $3 and revoked_at is null \
             and expires_at > now()",
        )
        .bind(who.tenant)
        .bind(who.session)
        .bind(who.user)
        .bind(ways)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if marked != 1 {
            return Err(Error::Unauthorized("this device was signed out; please sign in again".into()));
        }
        let detail = crate::gate::detail(from, way, serde_json::json!({"session": who.session}));
        crate::gate::event(&mut tx, who.tenant, who.user, "stepped_up", detail).await?;
        crate::auth::notify_users(&mut tx, who.tenant, Some(who.user)).await?;
        tx.commit().await?;
        self.sessions.remove(&(who.tenant, who.session));
        Ok(())
    }

    /// The signed-in devices of `user` (oneself, or anyone for an administrator).
    pub async fn list_sessions(&self, ctx: &Ctx, user: Uuid) -> Result<Vec<DeviceSession>> {
        if user != ctx.user && !ctx.is_admin {
            return Err(Error::Forbidden("only administrators see other people's devices".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_as(
            "select id, user_id, actor, created_at, last_seen_at, expires_at, user_agent, ip, zone from user_session \
             where tenant_id = $1 and user_id = $2 and revoked_at is null and expires_at > now() order by last_seen_at desc limit 200",
        )
        .bind(ctx.tenant)
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Signs one device out (one's own, or anyone's for an administrator); its pushes stop. Only a person at a screen
    /// (BD-85).
    pub async fn revoke_session(&self, ctx: &Ctx, sid: Uuid) -> Result<()> {
        ctx.changing_sign_in()?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and id = $2 and revoked_at is null and (user_id = $3 or $4)")
            .bind(ctx.tenant)
            .bind(sid)
            .bind(ctx.user)
            .bind(ctx.is_admin)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("no such session".into()));
        }
        sqlx::query("delete from push_subscription where tenant_id = $1 and session_id = $2").bind(ctx.tenant).bind(sid).execute(&mut *tx).await?;
        crate::audit::record_admin(&mut tx, ctx, "UserSession", sid, "revoke", None, serde_json::json!({})).await?;
        crate::gate::session_ended(&mut tx, ctx.tenant, sid, "revoked").await?;
        tx.commit().await?;
        self.sessions.insert((ctx.tenant, sid), (None, Instant::now()));
        Ok(())
    }

    /// Signing out of this device: revokes its session (no permission check: it is the caller's),
    /// and with it the device's pushes.
    pub async fn end_session(&self, tenant: Uuid, sid: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let ended = sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and id = $2 and revoked_at is null")
            .bind(tenant)
            .bind(sid)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if ended > 0 {
            crate::gate::session_ended(&mut tx, tenant, sid, "signed_out").await?;
        }
        sqlx::query("delete from push_subscription where tenant_id = $1 and session_id = $2").bind(tenant).bind(sid).execute(&mut *tx).await?;
        tx.commit().await?;
        self.sessions.insert((tenant, sid), (None, Instant::now()));
        Ok(())
    }

    /// Deletes sessions that ended more than a week ago (maintenance).
    pub async fn purge_sessions(&self) -> Result<u64> {
        let purged = self
            .db
            .purge(&[
                ("sso_logout_seen", "seen_at < now() - interval '1 day' and not ferrum_workspace_held(tenant_id)".into()),
                ("sso_sid", "created_at < now() - interval '90 days'".into()),
                ("user_session", "expires_at < now() - interval '7 days' or revoked_at < now() - interval '7 days'".into()),
            ])
            .await;
        // Failed sign-ins of hours that are over: their summaries.
        let mut tx = self.db.system_tx().await?;
        crate::gate::flush_failures(&mut tx, None).await?;
        tx.commit().await?;
        self.sessions.retain(|_, v| v.1.elapsed() < CACHE_TTL);
        purged
    }

    // ── hosts ──

    /// Where a host name leads: a verified custom domain or a subdomain of a workspace, a released
    /// host still redirecting, or nothing of ours (the caller decides about a default workspace).
    pub async fn route_host(&self, host: &str) -> Result<HostRoute> {
        Ok(self.host_row(host).await?.0)
    }

    /// The zone of a host that serves a workspace (`tenant_host.zone`, docs/operations.md, "Zones"),
    /// cached as [`Kernel::route_host`] is.
    pub async fn host_zone(&self, host: &str) -> Result<Option<String>> {
        Ok(match self.host_row(host).await? {
            (HostRoute::Serve { .. } | HostRoute::Leaving { .. }, zone, _) => zone,
            _ => None,
        })
    }

    /// What a host that serves a workspace's shell shows there ([`HostShape`]) and whether it is the main address, cached
    /// as [`Kernel::route_host`] is; `None` for any other host (a portal, a site, one moved or not ours).
    pub async fn host_shown(&self, host: &str) -> Result<Option<HostShown>> {
        Ok(match self.host_row(host).await? {
            (HostRoute::Serve { target, .. } | HostRoute::Leaving { target, .. }, _, shown) if target == "shell" => Some(shown),
            _ => None,
        })
    }

    /// The Brand a host that serves a workspace shows ([`Kernel::set_host_brand`]), cached as [`Kernel::route_host`] is.
    pub async fn host_brand(&self, host: &str) -> Result<Option<Uuid>> {
        Ok(match self.host_row(host).await? {
            (HostRoute::Serve { .. } | HostRoute::Leaving { .. }, _, shown) => shown.brand,
            _ => None,
        })
    }

    async fn host_row(&self, host: &str) -> Result<(HostRoute, Option<String>, HostShown)> {
        let Some(host) = normalize_host(host) else { return Ok((HostRoute::Unknown, None, HostShown::default())) };
        // Kept 20 s, and only while no workspace's settings changed meanwhile, here or on another replica (a workspace that
        // moved here or away, an address added elsewhere: crate::registry's notifications).
        let bumps = self.registry.bumps();
        if let Some(e) = self.hosts.get(&host)
            && e.3.elapsed() < CACHE_TTL
            && e.4 == bumps
        {
            return Ok((e.0.clone(), e.1.clone(), e.2.clone()));
        }
        let mut tx = self.db.system_tx().await?;
        #[derive(sqlx::FromRow)]
        struct Row {
            tenant_id: Uuid,
            kind: String,
            target: String,
            verified_at: Option<DateTime<Utc>>,
            released_at: Option<DateTime<Utc>>,
            zone: Option<String>,
            moved: bool,
            moved_to: Option<String>,
            leaving_until: Option<DateTime<Utc>>,
            home: Option<String>,
            apps: Option<Vec<String>>,
            label: Option<String>,
            is_primary: bool,
            forward: bool,
            brand: Option<Uuid>,
        }
        // A workspace that moved to another installation (workspace_move.rs) answers as moved on every host it had here. A
        // domain claimed for mail leads nowhere, claimed or released: it is no address.
        let row: Option<Row> = sqlx::query_as(
            "select h.tenant_id, h.kind, h.target, h.verified_at, h.released_at, h.zone, m.workspace is not null as moved, m.moved_to, h.leaving_until, \
             h.home, h.apps, h.label, h.is_primary, h.forward, h.brand from tenant_host h \
             join tenant t on t.id = h.tenant_id left join workspace_move m on m.workspace = h.tenant_id and m.state = 'moved' \
             where h.host = $1 and h.target <> 'mail'",
        )
        .bind(&host)
        .fetch_optional(&mut *tx)
        .await?;
        let zone = row.as_ref().and_then(|r| r.zone.clone());
        let shown = row
            .as_ref()
            .map(|r| HostShown {
                shape: HostShape { home: r.home.clone(), apps: r.apps.clone(), label: r.label.clone(), forward: r.forward },
                primary: r.is_primary,
                brand: r.brand,
            })
            .unwrap_or_default();
        let route = match row {
            None => HostRoute::Unknown,
            // To its address there when it has another one; otherwise the address still points here, and only says so.
            Some(r) if r.moved => HostRoute::Moved { tenant: r.tenant_id, to: r.moved_to.filter(|to| *to != host) },
            // Its leaving period first, then the redirect window from its end.
            Some(Row { tenant_id: tenant, target, released_at: Some(released), leaving_until: leaving, .. }) => {
                let now = Utc::now();
                if now - leaving.unwrap_or(released) < chrono::Duration::days(HOST_REDIRECT_DAYS) {
                    let to = main_host(&mut tx, tenant, &target).await?;
                    match leaving {
                        Some(until) if until > now => HostRoute::Leaving { tenant, target, to, until },
                        _ => HostRoute::Moved { tenant, to },
                    }
                } else {
                    HostRoute::Unknown
                }
            }
            // A forwarding address that served before still serves those signed in there for its leaving period. The main
            // address never forwards, even one an older build made main (it knows no `forward`): it would send people to
            // itself.
            Some(r) if r.forward && !r.is_primary && (r.kind == "subdomain" || r.verified_at.is_some()) => {
                let to = main_host(&mut tx, r.tenant_id, &r.target).await?;
                match r.leaving_until {
                    Some(until) if until > Utc::now() => HostRoute::Leaving { tenant: r.tenant_id, target: r.target, to, until },
                    _ => HostRoute::Forward { tenant: r.tenant_id, to, home: r.home.filter(|_| r.zone.is_none()) },
                }
            }
            Some(r) if r.kind == "subdomain" || r.verified_at.is_some() => HostRoute::Serve { tenant: r.tenant_id, target: r.target },
            Some(_) => HostRoute::Unknown,
        };
        tx.commit().await?;
        if self.hosts.len() > 100_000 {
            self.hosts.retain(|_, v| v.3.elapsed() < CACHE_TTL);
        }
        self.hosts.insert(host, (route.clone(), zone.clone(), shown.clone(), Instant::now(), bumps));
        Ok((route, zone, shown))
    }

    /// The workspace a host name serves the shell for (verified custom domain or subdomain).
    pub async fn tenant_for_host(&self, host: &str) -> Result<Option<Uuid>> {
        Ok(match self.route_host(host).await? {
            HostRoute::Serve { tenant, target } if target == "shell" => Some(tenant),
            _ => None,
        })
    }

    /// The pending, verified and released host names of the caller's workspace.
    pub async fn list_hosts(&self, ctx: &Ctx) -> Result<Vec<TenantHost>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_as(
            "select host, kind, target, is_primary, verify_token, verified_at, checked_at, failures, released_at, created_at, leaving_until, \
             zone, home, apps, label, base_name, proved_at, forward, brand from tenant_host where tenant_id = $1 order by created_at",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Adds a subdomain of the cluster's base domain for a workspace (operators and the cloud's
    /// sign-up; no proof needed: the cluster owns the base domain). The first host of a target is
    /// its primary.
    pub async fn add_subdomain(&self, tenant: Uuid, host: &str) -> Result<()> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a valid domain name"))?;
        let mut tx = self.db.system_tx().await?;
        self.claimable(&mut tx, &host).await?;
        sqlx::query(
            "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) \
             values ($1, $2, 'subdomain', 'shell', not exists (select 1 from tenant_host where tenant_id = $2 and target = 'shell' and is_primary and released_at is null), '', now())",
        )
        .bind(&host)
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(())
    }

    /// Gives every workspace without one its subdomain of the base domain, `{slug}.{base}` (shape B):
    /// its primary host when it has no other. Returns how many were added.
    pub async fn ensure_subdomains(&self, base: &str) -> Result<u64> {
        let base = base.trim().trim_matches('.').to_ascii_lowercase();
        if normalize_host(&format!("x.{base}")).is_none() {
            return Err(Error::invalid("the base domain is not a domain name"));
        }
        let mut tx = self.db.system_tx().await?;
        let added = sqlx::query(
            "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) \
             select t.slug || '.' || $1, t.id, 'subdomain', 'shell', \
               not exists (select 1 from tenant_host p where p.tenant_id = t.id and p.target = 'shell' and p.is_primary and p.released_at is null), '', now() \
             from tenant t where t.slug <> $2 and not exists (select 1 from tenant_host h where h.host = t.slug || '.' || $1) \
             on conflict (host) do nothing",
        )
        .bind(&base)
        .bind(crate::identity::DIRECTORY)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if added > 0 {
            self.hosts.clear();
        }
        Ok(added)
    }

    /// Refuses a host that is in use, or released and still leaving, redirecting or quarantined (counted from the
    /// end of its leaving period), or kept from everyone since its workspace left this database ([`quarantine_hosts`]). One
    /// statement reads both, so a deletion committing meanwhile is seen whole: its rows still there, or its quarantine.
    async fn claimable(&self, tx: &mut crate::db::Tx, host: &str) -> Result<()> {
        let recently = || Err(Error::Conflict("this domain belonged to another workspace recently and cannot be used yet".into()));
        let (live, released, kept): (Option<bool>, Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
            "select (select released_at is null from tenant_host where host = $1), \
             (select greatest(released_at, leaving_until) from tenant_host where host = $1 and released_at is not null), \
             (select until from host_quarantine where host = $1)",
        )
        .bind(host)
        .fetch_one(&mut **tx)
        .await?;
        match kept {
            Some(until) if until > Utc::now() => return recently(),
            Some(_) => {
                sqlx::query("delete from host_quarantine where host = $1").bind(host).execute(&mut **tx).await?;
            }
            None => {}
        }
        match (live, released) {
            (None, _) => Ok(()),
            (Some(false), Some(at)) if Utc::now() - at < chrono::Duration::days(HOST_QUARANTINE_DAYS) => recently(),
            (Some(false), Some(_)) => {
                sqlx::query("delete from tenant_host where host = $1").bind(host).execute(&mut **tx).await?;
                Ok(())
            }
            _ => Err(Error::Conflict("this domain is already in use".into())),
        }
    }

    /// `ferrum task add_host`: a host the operator vouches for (kind `operator`), for a name that only
    /// resolves inside a private network. It is served as verified at once and never checked. `zone`:
    /// the zone its requests are in (crate::zones), or none.
    pub async fn add_operator_host(&self, tenant_slug: &str, host: &str, target: &str, zone: Option<&str>) -> Result<()> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a host name"))?;
        check_target(target)?;
        let (tenant, _) = self.tenant_by_slug(tenant_slug).await?;
        let mut tx = self.db.system_tx().await?;
        self.claimable(&mut tx, &host).await?;
        sqlx::query("insert into tenant_host (host, tenant_id, kind, target, verify_token, verified_at, zone) values ($1, $2, 'operator', $3, '', now(), $4)")
            .bind(&host)
            .bind(tenant)
            .bind(target)
            .bind(zone)
            .execute(&mut *tx)
            .await
            .map_err(|e| match &e {
                sqlx::Error::Database(db) if db.code().as_deref() == Some("23514") => Error::field("host", "is not a host name"),
                _ => Error::from(e),
            })?;
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        crate::audit::record_admin(
            &mut tx,
            &system,
            "WorkspaceDomain",
            tenant,
            "add",
            None,
            serde_json::json!({"host": host, "target": target, "kind": "operator", "by": "operator"}),
        )
        .await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        self.primary_where_none(Some(tenant)).await?;
        Ok(())
    }

    /// `ferrum task set_primary_host`: makes any live host of a workspace (an operator's one too) the main one of its
    /// target, audited as the system's.
    pub async fn set_primary_host(&self, host: &str) -> Result<()> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a host name"))?;
        let mut tx = self.db.system_tx().await?;
        let tenant: Uuid = sqlx::query_scalar(
            "select tenant_id from tenant_host where host = $1 and released_at is null and (kind = 'subdomain' or verified_at is not null) and target <> 'mail'",
        )
        .bind(&host)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("host {host} (live and verified)")))?;
        not_forwarding(&mut tx, &host).await?;
        primary_in(&mut tx, tenant, &host).await?;
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        crate::audit::record_admin(&mut tx, &system, "WorkspaceDomain", tenant, "primary", None, serde_json::json!({"host": host, "by": "operator"})).await?;
        tx.commit().await?;
        self.hosts.clear();
        Ok(())
    }

    /// Gives each target of a workspace (`None`: of every workspace) that has live hosts but no main one its main host,
    /// by [`SUCCESSION`]: the first host a target gets, of any kind, becomes its main one, and an installation from
    /// before this rule (only operator hosts, none of them main) gets one at its next start or host change. Returns how
    /// many were made main. Two at once: the unique index `tenant_host_primary` lets one through; the other finds it
    /// done (or fails on the index, which is taken as done).
    pub async fn primary_where_none(&self, tenant: Option<Uuid>) -> Result<u64> {
        let q = format!(
            "update tenant_host set is_primary = true where host in ( \
               select distinct on (c.tenant_id, c.target) c.host from tenant_host c \
               where c.released_at is null and (c.kind = 'subdomain' or c.verified_at is not null) and not c.forward and c.target <> 'mail' \
                 and ($1::uuid is null or c.tenant_id = $1) \
                 and not exists (select 1 from tenant_host p where p.tenant_id = c.tenant_id and p.target = c.target and p.is_primary and p.released_at is null) \
               order by c.tenant_id, c.target, {SUCCESSION})"
        );
        let mut tx = self.db.system_tx().await?;
        let n = match sqlx::query(sqlx::AssertSqlSafe(q)).bind(tenant).execute(&mut *tx).await {
            Ok(r) => r.rows_affected(),
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        tx.commit().await?;
        if n > 0 {
            self.hosts.clear();
        }
        Ok(n)
    }

    /// [`Kernel::primary_where_none`] for every workspace: at each start.
    pub async fn ensure_primary_hosts(&self) -> Result<u64> {
        self.primary_where_none(None).await
    }

    /// Where a host's visitors would be sent once it is released: the main host of its target that would be left
    /// (another one already main, or the one that would take its place), or none. Not found: no such live host.
    pub async fn host_successor(&self, host: &str) -> Result<Option<String>> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a host name"))?;
        let mut tx = self.db.system_tx().await?;
        let q = format!(
            "select (select c.host from tenant_host c where c.tenant_id = h.tenant_id and c.target = h.target and c.host <> h.host and c.target <> 'mail' \
               and c.released_at is null and (c.kind = 'subdomain' or c.verified_at is not null) and not c.forward order by c.is_primary desc, {SUCCESSION} limit 1) \
             from tenant_host h where h.host = $1 and h.released_at is null"
        );
        let next: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(q)).bind(&host).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        next.ok_or_else(|| Error::NotFound(format!("host {host}")))
    }

    /// `ferrum task set_host_zone`: the zone a host's requests are in (crate::zones), or none. Only
    /// the operator sets it: a workspace's administrator has no way to.
    pub async fn set_host_zone(&self, host: &str, zone: Option<&str>) -> Result<()> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a host name"))?;
        let mut tx = self.db.system_tx().await?;
        let tenant: Option<Uuid> = sqlx::query_scalar("update tenant_host set zone = $2 where host = $1 and released_at is null returning tenant_id")
            .bind(&host)
            .bind(zone)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        // ponytail: this replica forgets it now; the others within the host cache's 20 seconds.
        self.hosts.remove(&host);
        let tenant = tenant.ok_or_else(|| Error::NotFound(format!("host {host}")))?;
        // A change of a host: a workspace from before the main-host rule gets its main one.
        self.primary_where_none(Some(tenant)).await?;
        Ok(())
    }

    /// Adds a custom domain to the caller's workspace, unverified until [`Kernel::host_checked`]
    /// finds it pointing here. `target`: `shell`, `portal` or `site:{app}`; or `mail`, a domain claimed for mail, which
    /// serves nothing and is proven by its DNS record alone ([`Kernel::domain_proven`]).
    pub async fn add_host(&self, ctx: &Ctx, host: &str, target: &str) -> Result<TenantHost> {
        self.add_host_shaped(ctx, host, target, &HostShape::default()).await
    }

    /// [`Kernel::add_host`], with what the address shows ([`HostShape`], a `shell` address only).
    pub async fn add_host_shaped(&self, ctx: &Ctx, host: &str, target: &str, shape: &HostShape) -> Result<TenantHost> {
        may_change_addresses(ctx)?;
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a valid domain name"))?;
        if target != "mail" {
            check_target(target)?;
        }
        let shape = check_shape(target, shape)?;
        let mut tx = self.db.system_tx().await?;
        room_for_address(&mut tx, ctx.tenant).await?;
        self.claimable(&mut tx, &host).await?;
        if shape.forward {
            main_to_forward_to(&mut tx, ctx.tenant).await?;
        }
        let token = hex::encode(rand::random::<[u8; 16]>());
        sqlx::query(
            "insert into tenant_host (host, tenant_id, kind, target, verify_token, home, apps, label, forward) values ($1, $2, 'custom', $3, $4, $5, $6, $7, $8)",
        )
        .bind(&host)
        .bind(ctx.tenant)
        .bind(target)
        .bind(&token)
        .bind(&shape.home)
        .bind(&shape.apps)
        .bind(&shape.label)
        .bind(shape.forward)
        .execute(&mut *tx)
        .await?;
        let mut said = serde_json::json!({"host": host, "target": target});
        said_shape(&mut said, &shape);
        crate::audit::record_admin(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "add", None, said).await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(self.list_hosts(ctx).await?.into_iter().find(|h| h.host == host).expect("just added"))
    }

    /// Gives the caller's workspace a name under the cluster's base domain, `{name}--{slug}.{base}` ([`name_under_base`]),
    /// for `target`, with what it shows: live at once, with nothing to prove (the cluster owns the base domain), a
    /// certificate on demand like the workspace's own subdomain, and sign-ins handed to it by the accounts host like any
    /// `shell` address. The first live address of a target becomes its main one. A name of its own that it let go of comes
    /// back to it at once, even while leaving: no other workspace can ever have it, so there is nobody to keep it from.
    pub async fn add_name(&self, ctx: &Ctx, base: &str, name: &str, target: &str, shape: &HostShape) -> Result<TenantHost> {
        may_change_addresses(ctx)?;
        check_target(target)?;
        let shape = check_shape(target, shape)?;
        let mut tx = self.db.system_tx().await?;
        room_for_address(&mut tx, ctx.tenant).await?;
        let slug: String = sqlx::query_scalar("select slug from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let host = name_under_base(name, &slug, base)?;
        let held: Option<(Uuid, bool)> =
            sqlx::query_as("select tenant_id, released_at is null from tenant_host where host = $1").bind(&host).fetch_optional(&mut *tx).await?;
        match held {
            Some((t, true)) if t == ctx.tenant => return Err(Error::Conflict(format!("{host} is already an address of this workspace"))),
            Some((t, false)) if t == ctx.tenant => {
                sqlx::query("delete from tenant_host where host = $1").bind(&host).execute(&mut *tx).await?;
            }
            // Only a workspace whose slug was made with `--` before this form existed can hold such a name.
            _ => self.claimable(&mut tx, &host).await?,
        }
        if shape.forward {
            main_to_forward_to(&mut tx, ctx.tenant).await?;
        }
        sqlx::query(
            "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at, base_name, home, apps, label, forward) \
             values ($1, $2, 'subdomain', $3, \
             not $8 and not exists (select 1 from tenant_host where tenant_id = $2 and target = $3 and is_primary and released_at is null), \
             '', now(), $4, $5, $6, $7, $8)",
        )
        .bind(&host)
        .bind(ctx.tenant)
        .bind(target)
        .bind(name)
        .bind(&shape.home)
        .bind(&shape.apps)
        .bind(&shape.label)
        .bind(shape.forward)
        .execute(&mut *tx)
        .await?;
        let mut said = serde_json::json!({"host": host, "target": target, "kind": "subdomain", "name": name});
        said_shape(&mut said, &shape);
        crate::audit::record_admin(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "add", None, said).await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(self.list_hosts(ctx).await?.into_iter().find(|h| h.host == host).expect("just added"))
    }

    /// Changes what one of the caller's workspace's live `shell` addresses shows. The row is locked while it changes, so
    /// two changes at once each build on the other: neither is lost. An operator's address is the operator's. One that
    /// served and comes to forward first has its leaving period (`FERRUM_HOST_LEAVING_DAYS`), as a removed one does, so
    /// that what browsers kept for it can be sent; the main address never forwards.
    pub async fn shape_host(&self, ctx: &Ctx, host: &str, patch: &HostPatch) -> Result<TenantHost> {
        may_change_addresses(ctx)?;
        let host = normalize_host(host).ok_or_else(|| Error::NotFound("no such domain".into()))?;
        let mut tx = self.db.system_tx().await?;
        type Row = (String, String, bool, Option<String>, Option<Vec<String>>, Option<String>, bool, bool);
        let row: Option<Row> = sqlx::query_as(
            "select kind, target, released_at is not null, home, apps, label, forward, is_primary from tenant_host where host = $1 and tenant_id = $2 for update",
        )
        .bind(&host)
        .bind(ctx.tenant)
        .fetch_optional(&mut *tx)
        .await?;
        let (kind, target, released, home, apps, label, forward, primary) = row.ok_or_else(|| Error::NotFound("no such domain".into()))?;
        if kind == "operator" {
            return Err(Error::Forbidden(OPERATOR_OWNS.into()));
        }
        if released {
            return Err(Error::invalid("this address is leaving the workspace: it no longer changes"));
        }
        let before = HostShape { home, apps, label, forward };
        let after = HostShape {
            home: patch.home.clone().unwrap_or_else(|| before.home.clone()),
            apps: patch.apps.clone().unwrap_or_else(|| before.apps.clone()),
            label: patch.label.clone().unwrap_or_else(|| before.label.clone()),
            forward: patch.forward.unwrap_or(before.forward),
        };
        let after = check_shape(&target, &after)?;
        if after.forward && primary {
            return Err(Error::field("forward", "the main address cannot forward: make another address the main one first"));
        }
        if after.forward && !before.forward {
            main_to_forward_to(&mut tx, ctx.tenant).await?;
        }
        sqlx::query(
            "update tenant_host set home = $2, apps = $3, label = $4, forward = $5, \
             leaving_until = case when $5 and not forward and $6 > 0 and (kind = 'subdomain' or verified_at is not null) \
               then now() + make_interval(days => $6::int) when $5 then leaving_until end where host = $1",
        )
        .bind(&host)
        .bind(&after.home)
        .bind(&after.apps)
        .bind(&after.label)
        .bind(after.forward)
        .bind(self.live().host_leaving_days)
        .execute(&mut *tx)
        .await?;
        let said = serde_json::json!({"host": host, "before": before, "after": after});
        crate::audit::record_admin(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "shape", None, said).await?;
        tx.commit().await?;
        // ponytail: this replica sees it now; the others within the host cache's 20 seconds.
        self.hosts.remove(&host);
        Ok(self.list_hosts(ctx).await?.into_iter().find(|h| h.host == host).expect("just changed"))
    }

    /// How many live addresses the workspace may hold (its plan's `addresses` quota, else [`ADDRESSES_DEFAULT`]) and holds.
    pub async fn address_room(&self, tenant: Uuid) -> Result<(i64, i64)> {
        let mut tx = self.db.system_tx().await?;
        let room = address_room(&mut tx, tenant).await?;
        tx.commit().await?;
        Ok(room)
    }

    /// The workspace's short paths (docs/operations.md, "Addresses of a workspace", short paths), in the order its administrators gave them.
    pub async fn short_paths(&self, tenant: Uuid) -> Result<Vec<ShortPath>> {
        let mut tx = self.db.system_tx().await?;
        let paths: Option<Value> =
            sqlx::query_scalar("select settings->'paths' from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?.flatten();
        tx.commit().await?;
        Ok(paths.and_then(|p| serde_json::from_value(p).ok()).unwrap_or_default())
    }

    /// Replaces the workspace's short paths with `paths`, checked together: each alias of its form ([`alias_shaped`]) and
    /// given once, each target a path of the same host ([`target_shaped`]), at most [`PATHS_MAX`]. The refusal names the
    /// alias at fault. Which segments are free and which targets lead into the workspace the server checks first. Kept in
    /// the workspace's settings, so they go wherever the workspace goes (a move, a sandbox). Two saves at once: the later
    /// one's list stands whole.
    pub async fn set_short_paths(&self, ctx: &Ctx, paths: &[ShortPath]) -> Result<Vec<ShortPath>> {
        may_change_addresses(ctx)?;
        if paths.len() > PATHS_MAX {
            return Err(Error::field("paths", format!("a workspace has at most {PATHS_MAX} short paths")));
        }
        for (i, p) in paths.iter().enumerate() {
            if !alias_shaped(&p.alias) {
                return Err(Error::field("paths", format!("/{}: use 2 to 30 lowercase letters, digits or hyphens, starting with a letter", p.alias)));
            }
            if paths[..i].iter().any(|q| q.alias == p.alias) {
                return Err(Error::field("paths", format!("/{} is given twice", p.alias)));
            }
            if !target_shaped(&p.target) {
                return Err(Error::field("paths", format!("/{} must lead to a path here, such as /crm/, never to another address", p.alias)));
            }
        }
        let mut tx = self.db.system_tx().await?;
        let before: Option<Value> =
            sqlx::query_scalar("select settings->'paths' from tenant where id = $1 for update").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let after = serde_json::to_value(paths).map_err(Error::internal)?;
        if paths.is_empty() {
            sqlx::query("update tenant set settings = settings - 'paths' where id = $1").bind(ctx.tenant).execute(&mut *tx).await?;
        } else {
            sqlx::query("update tenant set settings = settings || jsonb_build_object('paths', $2::jsonb) where id = $1")
                .bind(ctx.tenant)
                .bind(&after)
                .execute(&mut *tx)
                .await?;
        }
        let said = serde_json::json!({"before": before.unwrap_or_else(|| serde_json::json!([])), "after": after});
        crate::audit::record_admin(&mut tx, ctx, "WorkspacePaths", ctx.tenant, "set", None, said).await?;
        tx.commit().await?;
        Ok(paths.to_vec())
    }

    /// Makes a verified host the primary one of its workspace and target: links, emails and
    /// redirects use it, and the workspace's other hosts of that target redirect to it.
    pub async fn make_primary_host(&self, ctx: &Ctx, host: &str) -> Result<()> {
        may_change_addresses(ctx)?;
        let host = normalize_host(host).ok_or_else(|| Error::NotFound("no such domain".into()))?;
        let mut tx = self.db.system_tx().await?;
        let target: Option<String> = sqlx::query_scalar(
            "select target from tenant_host where host = $1 and tenant_id = $2 and released_at is null and (kind = 'subdomain' or verified_at is not null) \
             and target <> 'mail'",
        )
        .bind(&host)
        .bind(ctx.tenant)
        .fetch_optional(&mut *tx)
        .await?;
        target.ok_or_else(|| Error::invalid("only a verified domain of this workspace can be its primary one"))?;
        not_forwarding(&mut tx, &host).await?;
        primary_in(&mut tx, ctx.tenant, &host).await?;
        crate::audit::record_admin(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "primary", None, serde_json::json!({"host": host})).await?;
        tx.commit().await?;
        self.hosts.clear();
        Ok(())
    }

    /// Every host that serves a workspace for `target` now (a subdomain, or a verified domain, not
    /// released, nor forwarding), its main one first.
    pub async fn serving_hosts(&self, tenant: Uuid, target: &str) -> Result<Vec<String>> {
        let mut tx = self.db.system_tx().await?;
        let hosts = sqlx::query_scalar(
            "select host from tenant_host where tenant_id = $1 and target = $2 and released_at is null and (kind = 'subdomain' or verified_at is not null) \
             and not forward order by is_primary desc, host",
        )
        .bind(tenant)
        .bind(target)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(hosts)
    }

    /// Where a workspace's staff work, for a refusal to name: a host serving its shell whose zone is
    /// one of `zones` (those that admit staff), its primary one first. A host with no zone of its
    /// own is never named: which zone it is in depends on the replica and the caller.
    pub async fn staff_address(&self, tenant: Uuid, zones: &[String]) -> Result<Option<String>> {
        if zones.is_empty() {
            return Ok(None);
        }
        let mut tx = self.db.system_tx().await?;
        let h = sqlx::query_scalar(
            "select host from tenant_host where tenant_id = $1 and target = 'shell' and zone = any($2) and released_at is null \
             and (kind = 'subdomain' or verified_at is not null) and not forward order by is_primary desc, host limit 1",
        )
        .bind(tenant)
        .bind(zones)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(h)
    }

    /// The primary host of a workspace for `target`, if it has one.
    pub async fn primary_host(&self, tenant: Uuid, target: &str) -> Result<Option<String>> {
        let mut tx = self.db.system_tx().await?;
        let h = sqlx::query_scalar(
            "select host from tenant_host where tenant_id = $1 and target = $2 and is_primary and released_at is null and (kind = 'subdomain' or verified_at is not null)",
        )
        .bind(tenant)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(h)
    }

    /// Records the outcome of checking a custom domain (that it reaches this cluster and answers
    /// with its token), whatever proof the cluster asks for. Success verifies it; enough failures in a row unverify it.
    pub async fn host_checked(&self, host: &str, ok: bool) -> Result<()> {
        self.record_check(host, HostCheck { works: ok, points: ok, record: None }).await
    }

    /// Checks a domain of a workspace's own now ([`Kernel::check_hosts`], or its administrators): whether it points here,
    /// then what [`Kernel::host_reached`] makes of it. `None`: no such live domain.
    pub async fn check_host(&self, host: &str) -> Result<Option<HostCheck>> {
        let scheme = if self.settings.webhooks_allow_private { "http" } else { "https" };
        let points = self.host_answers(host, &format!("{scheme}://{host}/.well-known/ferrum-domain")).await;
        self.host_reached(host, points).await
    }

    /// Records a check of a domain of a workspace's own, once whether it points here is known (`points`). Where proof by
    /// record is asked ([`DomainProof::Record`]), a domain never proven opens nothing until its record is there as well
    /// ([`domain_record`], asked only when it points here); one proven once, or verified before, goes on by pointing here
    /// alone: it never left its workspace, so nobody can have taken it. A domain claimed for mail is proven by its record
    /// alone, whatever the cluster asks of addresses: nothing of it is served, so it has nowhere to point. `None`: no such
    /// live domain.
    pub async fn host_reached(&self, host: &str, points: bool) -> Result<Option<HostCheck>> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, bool, bool, bool)> = sqlx::query_as(
            "select verify_token, verified_at is not null, proved_at is not null, target = 'mail' from tenant_host \
             where host = $1 and kind = 'custom' and released_at is null",
        )
        .bind(host)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((token, verified, proved, mail)) = row else { return Ok(None) };
        let points = points || mail;
        let record = if points && !proved && (mail || self.settings.domain_proof == DomainProof::Record) {
            Some(domain_record(&self.live().dns_resolvers, host, &token).await)
        } else {
            None
        };
        let found = HostCheck { works: points && (verified || proved || record != Some(false)), points, record };
        self.record_check(host, found).await?;
        Ok(Some(found))
    }

    /// Writes what a check found: working, it is verified (and proven, when its record was); pointing here, it counts no
    /// failure (so a domain that points here and waits for its record is told from one that does not); otherwise
    /// [`HOST_MAX_FAILURES`] in a row unverify it.
    async fn record_check(&self, host: &str, found: HostCheck) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let tenant: Option<Uuid> = sqlx::query_scalar(
            "update tenant_host set checked_at = now(), failures = case when $2 then 0 else failures + 1 end, \
             verified_at = case when $3 then coalesce(verified_at, now()) when not $2 and failures + 1 >= $4 then null else verified_at end, \
             proved_at = case when $3 and $5 then coalesce(proved_at, now()) else proved_at end \
             where host = $1 and kind = 'custom' and released_at is null returning tenant_id",
        )
        .bind(host)
        .bind(found.points)
        .bind(found.works)
        .bind(HOST_MAX_FAILURES)
        .bind(found.record == Some(true))
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        self.hosts.remove(host);
        // A verified domain is a host like any other: the first of its target becomes the main one.
        if let Some(tenant) = tenant.filter(|_| found.works) {
            self.primary_where_none(Some(tenant)).await?;
        }
        Ok(())
    }

    /// The token a pending custom domain must answer with, by host (checked before any session). None for a domain
    /// claimed for mail: nothing answers for it, so it gets no certificate either ([`Kernel::host_may_have_certificate`]).
    pub async fn host_token(&self, host: &str) -> Result<Option<String>> {
        let Some(host) = normalize_host(host) else { return Ok(None) };
        let mut tx = self.db.system_tx().await?;
        let t = sqlx::query_scalar("select verify_token from tenant_host where host = $1 and kind = 'custom' and target <> 'mail' and released_at is null")
            .bind(host)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Whether `domain` is proven to be `tenant`'s for mail (docs/operations.md, "Domains for mail"): the workspace holds
    /// it, claimed for mail or as an address of its own, and its owner's DNS record proved it ([`domain_record`]). A
    /// domain is one workspace's whatever it is used for, so this is never true for two. One lookup by the domain: cheap
    /// enough for an app's settings hook and upkeep.
    pub async fn domain_proven(&self, tenant: Uuid, domain: &str) -> Result<bool> {
        let Some(host) = normalize_host(domain) else { return Ok(false) };
        let mut tx = self.db.system_tx().await?;
        let proven = sqlx::query_scalar(
            "select exists (select 1 from tenant_host where host = $1 and tenant_id = $2 and released_at is null and proved_at is not null)",
        )
        .bind(host)
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(proven)
    }

    /// Custom domains to check again (pending ones and verified ones not checked for a day).
    pub async fn hosts_to_check(&self, limit: i64) -> Result<Vec<String>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_scalar(
            "select host from tenant_host where kind = 'custom' and released_at is null and (checked_at is null or (verified_at is null and checked_at < now() - interval '5 minutes') \
             or checked_at < now() - interval '1 day') and not (verified_at is not null and host like '%.localhost') order by checked_at nulls first limit $1",
        )
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Whether `url` answers `host`'s token: the domain points at this cluster. Through the vetted
    /// client (no private addresses unless the server allows them, no redirects, ten seconds).
    pub async fn host_answers(&self, host: &str, url: &str) -> bool {
        let Ok(Some(token)) = self.host_token(host).await else { return false };
        let allow_private = self.settings.webhooks_allow_private;
        let Ok((client, url)) = self.vetted_client(url, allow_private, std::time::Duration::from_secs(10), "Ferrum domain check").await else { return false };
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => r.text().await.is_ok_and(|body| body.trim() == token),
            _ => false,
        }
    }

    /// Checks the custom domains that are due ([`Kernel::hosts_to_check`]): one is verified when
    /// `https://{host}/.well-known/ferrum-domain` answers its token (plain http when the server
    /// allows private addresses, in development) and, where proof by record is asked, its record is there
    /// ([`Kernel::host_reached`]); it loses that after [`HOST_MAX_FAILURES`] failed checks in a row. Returns how many
    /// were checked.
    // ponytail: a few per round, one after another; checks in parallel if a cluster carries
    // thousands of custom domains.
    pub async fn check_hosts(&self) -> Result<usize> {
        let hosts = self.hosts_to_check(6).await?;
        for host in &hosts {
            self.check_host(host).await?;
        }
        Ok(hosts.len())
    }

    /// Whether the edge may get a certificate for `host`: a verified custom domain, a subdomain,
    /// a pending custom domain (it must be served to be checked), or a released one still
    /// redirecting.
    pub async fn host_may_have_certificate(&self, host: &str) -> Result<bool> {
        Ok(!matches!(self.route_host(host).await?, HostRoute::Unknown) || self.host_token(host).await?.is_some())
    }

    /// Removes a custom domain from the caller's workspace. It is released, not forgotten: it keeps
    /// serving the people signed in there for its leaving period (`FERRUM_HOST_LEAVING_DAYS`), then
    /// redirects for [`HOST_REDIRECT_DAYS`] and stays quarantined for [`HOST_QUARANTINE_DAYS`], so
    /// work kept in browsers can be sent and old links and data never reach a new owner. Returns the
    /// end of the leaving period, if it has one (a domain never verified is simply dropped).
    pub async fn remove_host(&self, ctx: &Ctx, host: &str) -> Result<Option<DateTime<Utc>>> {
        may_change_addresses(ctx)?;
        let host = normalize_host(host).ok_or_else(|| Error::NotFound("no such domain".into()))?;
        let mut tx = self.db.system_tx().await?;
        let until = self.release_in(&mut tx, ctx.tenant, &host, true, self.live().host_leaving_days).await?;
        crate::audit::record_admin(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "remove", None, serde_json::json!({"host": host, "leaving_until": until}))
            .await?;
        tx.commit().await?;
        self.hosts.clear();
        Ok(until)
    }

    /// `ferrum task release_host`: releases any host of a workspace (a subdomain too), with a leaving period of
    /// `leaving_days` (the setting's when `None`), audited as the system's. Returns the workspace and the end of
    /// the leaving period, if it has one.
    pub async fn release_host(&self, host: &str, leaving_days: Option<i64>) -> Result<(Uuid, Option<DateTime<Utc>>)> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a host name"))?;
        let days = leaving_days.unwrap_or(self.live().host_leaving_days);
        if !(0..=365).contains(&days) {
            return Err(Error::field("leaving_days", "must be from 0 to 365"));
        }
        let mut tx = self.db.system_tx().await?;
        let tenant: Uuid = sqlx::query_scalar("select tenant_id from tenant_host where host = $1 and released_at is null")
            .bind(&host)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::NotFound(format!("host {host}")))?;
        let until = self.release_in(&mut tx, tenant, &host, false, days).await?;
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        crate::audit::record_admin(
            &mut tx,
            &system,
            "WorkspaceDomain",
            tenant,
            "remove",
            None,
            serde_json::json!({"host": host, "leaving_until": until, "by": "operator"}),
        )
        .await?;
        tx.commit().await?;
        self.hosts.clear();
        Ok((tenant, until))
    }

    /// Releases one host of `tenant` in `tx`: a custom domain never verified is dropped (nothing can depend on
    /// it); any other is released with a leaving period of `days` (none at 0). A primary host (or any, when its target has
    /// no primary) hands that place to another live host of the workspace for the same target first, by [`SUCCESSION`],
    /// so links and redirects keep leading somewhere; with none left, the target has no primary, as before. `custom_only`: an
    /// administrator's removal, which reaches only the workspace's custom domains and the names it gave itself under the base
    /// domain (an operator's address is the operator's; the workspace's own subdomain stays).
    async fn release_in(&self, tx: &mut crate::db::Tx, tenant: Uuid, host: &str, custom_only: bool, days: i64) -> Result<Option<DateTime<Utc>>> {
        // The workspace's live hosts are locked in one order, so two releases at once wait for each other
        // instead of each handing the primary to the other's host.
        let live: Vec<(String, String, String, bool, bool, DateTime<Utc>, bool, bool)> = sqlx::query_as(
            "select host, kind, target, is_primary, verified_at is not null, created_at, base_name is not null, forward from tenant_host \
             where tenant_id = $1 and released_at is null order by host for update",
        )
        .bind(tenant)
        .fetch_all(&mut **tx)
        .await?;
        let found = live.iter().find(|h| h.0 == host);
        if custom_only && found.is_some_and(|h| h.1 == "operator") {
            return Err(Error::Forbidden(OPERATOR_OWNS.into()));
        }
        let Some((_, kind, target, primary, verified, ..)) = found.filter(|h| !custom_only || h.1 == "custom" || h.6) else {
            return Err(Error::NotFound("no such domain".into()));
        };
        if kind == "custom" && !verified {
            sqlx::query("delete from tenant_host where host = $1").bind(host).execute(&mut **tx).await?;
            return Ok(None);
        }
        // The main place goes on by SUCCESSION (operator hosts included, a forwarding address never); also when the target
        // had no main host at all (an installation from before every target got one). A domain for mail has no main place
        // to hand on, and no leaving period: nobody is served there.
        let mail = target == "mail";
        let others: Vec<_> = live.iter().filter(|h| h.0 != host && h.2 == *target).collect();
        if !mail && (*primary || !others.iter().any(|h| h.3)) {
            let next = others
                .iter()
                .filter(|h| (h.1 == "subdomain" || h.4) && !h.7)
                .min_by(|a, b| (a.1 != "subdomain", a.5, &a.0).cmp(&(b.1 != "subdomain", b.5, &b.0)));
            sqlx::query("update tenant_host set is_primary = false where host = $1").bind(host).execute(&mut **tx).await?;
            if let Some(next) = next {
                sqlx::query("update tenant_host set is_primary = true where host = $1").bind(&next.0).execute(&mut **tx).await?;
            }
        }
        let until: Option<DateTime<Utc>> = sqlx::query_scalar(
            "update tenant_host set released_at = now(), is_primary = false, \
             leaving_until = case when $2 > 0 then now() + make_interval(days => $2::int) end where host = $1 returning leaving_until",
        )
        .bind(host)
        .bind(if mail { 0 } else { days })
        .fetch_one(&mut **tx)
        .await?;
        Ok(until)
    }

    /// Releases every host of a workspace (it was renamed away from them, or deleted).
    pub async fn release_hosts(&self, tenant: Uuid) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("update tenant_host set released_at = now(), is_primary = false where tenant_id = $1 and released_at is null")
            .bind(tenant)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        self.hosts.clear();
        Ok(n)
    }

    /// A workspace's slug, name and status.
    pub async fn workspace_brief(&self, tenant: Uuid) -> Result<(String, String, String)> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, String, String)> =
            sqlx::query_as("select slug, name, status from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("workspace".into()))
    }

    // ── people ──

    /// A person's token version (every session and token carries it; bumped to sign them out
    /// everywhere).
    pub async fn token_version(&self, tenant: Uuid, user: Uuid) -> Result<i32> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let v: Option<i32> =
            sqlx::query_scalar("select token_version from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(user).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        v.ok_or_else(|| Error::NotFound("no such user".into()))
    }

    /// A person's kind (`member`, `guest`, `portal`, `service`).
    pub async fn user_kind(&self, tenant: Uuid, user: Uuid) -> Result<String> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let k: Option<String> =
            sqlx::query_scalar("select kind from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(user).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        k.ok_or_else(|| Error::NotFound("no such user".into()))
    }

    /// Sets a person's kind (administrators). Changing it signs the person out everywhere.
    pub async fn set_user_kind(&self, ctx: &Ctx, user: Uuid, kind: &str) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if !USER_KINDS.contains(&kind) {
            return Err(Error::field("kind", "must be member, guest, portal or service"));
        }
        if user == ctx.user && kind != "member" {
            return Err(Error::invalid("you cannot change your own kind"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query(
            "update app_user set kind = $3, token_version = token_version + case when kind = $3 then 0 else 1 end where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(user)
        .bind(kind)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("no such user".into()));
        }
        crate::auth::notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        crate::audit::record_admin(&mut tx, ctx, "User", user, "kind", None, serde_json::json!({"kind": kind})).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    // ── interface releases ──

    /// Records an interface release whose files are already stored under `ui/{app}/{digest}/`,
    /// and that app `version` runs it. Idempotent: the digest names the contents, and a version
    /// that did not change the interface shares the release of the one before.
    pub async fn register_ui_release(&self, app: &str, digest: &str, version: &str, files: &Value, entry: &Value) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into ui_release (app, digest, files, entry) values ($1, $2, $3, $4) on conflict (app, digest) do update set last_used_at = now()")
            .bind(app)
            .bind(digest)
            .bind(files)
            .bind(entry)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into ui_release_version (app, version, digest) values ($1, $2, $3) on conflict do nothing")
            .bind(app)
            .bind(version)
            .bind(digest)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A release by app and digest.
    pub async fn ui_release(&self, app: &str, digest: &str) -> Result<Option<UiRelease>> {
        let mut tx = self.db.system_tx().await?;
        let r = sqlx::query_as("select app, digest, files, entry, created_at from ui_release where app = $1 and digest = $2")
            .bind(app)
            .bind(digest)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// The release an app version runs (the newest recorded for it).
    pub async fn ui_release_for(&self, app: &str, version: &str) -> Result<Option<UiRelease>> {
        let mut tx = self.db.system_tx().await?;
        let r = sqlx::query_as(
            "select r.app, r.digest, r.files, r.entry, r.created_at from ui_release_version v join ui_release r on r.app = v.app and r.digest = v.digest \
             where v.app = $1 and v.version = $2 order by v.created_at desc, v.digest limit 1",
        )
        .bind(app)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// Releases of an app with the versions that run them, newest first (the admin status
    /// page's rollback choice).
    pub async fn ui_releases(&self, app: &str, limit: i64) -> Result<Vec<(UiRelease, Vec<String>)>> {
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<UiRelease> =
            sqlx::query_as("select app, digest, files, entry, created_at from ui_release where app = $1 order by created_at desc limit $2")
                .bind(app)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await?;
        let versions: Vec<(String, String)> =
            sqlx::query_as("select digest, version from ui_release_version where app = $1 order by created_at").bind(app).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let v = versions.iter().filter(|(d, _)| *d == r.digest).map(|(_, v)| v.clone()).collect();
                (r, v)
            })
            .collect())
    }

    /// The release a workspace is pinned to for an app, if any.
    pub async fn ui_pin(&self, tenant: Uuid, app: &str) -> Result<Option<String>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let d =
            sqlx::query_scalar("select digest from tenant_ui_pin where tenant_id = $1 and app = $2").bind(tenant).bind(app).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(d)
    }

    /// Pins the caller's workspace to a release of an app's interface (a rollback while the
    /// installed one misbehaves, or a canary), or unpins it with `None`.
    pub async fn pin_ui(&self, ctx: &Ctx, app: &str, digest: Option<&str>) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if let Some(d) = digest
            && self.ui_release(app, d).await?.is_none()
        {
            return Err(Error::NotFound(format!("no interface release {d} of {app}")));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        match digest {
            Some(d) => {
                sqlx::query(
                    "insert into tenant_ui_pin (tenant_id, app, digest, pinned_by) values ($1, $2, $3, $4) \
                     on conflict (tenant_id, app) do update set digest = excluded.digest, pinned_at = now(), pinned_by = excluded.pinned_by",
                )
                .bind(ctx.tenant)
                .bind(app)
                .bind(d)
                .bind(ctx.user)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query("delete from tenant_ui_pin where tenant_id = $1 and app = $2").bind(ctx.tenant).bind(app).execute(&mut *tx).await?;
            }
        }
        crate::audit::record_admin(
            &mut tx,
            ctx,
            "UiRelease",
            ctx.tenant,
            if digest.is_some() { "pin" } else { "unpin" },
            None,
            serde_json::json!({"app": app, "digest": digest}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks releases as in use (every replica calls this for what it served lately, at most
    /// hourly), so they are not purged while tabs may still load their files.
    pub async fn touch_ui_releases(&self, used: &[(String, String)]) -> Result<()> {
        if used.is_empty() {
            return Ok(());
        }
        let (apps, digests): (Vec<&str>, Vec<&str>) = used.iter().map(|(a, d)| (a.as_str(), d.as_str())).unzip();
        let mut tx = self.db.system_tx().await?;
        sqlx::query(
            "update ui_release r set last_used_at = now() from unnest($1::text[], $2::text[]) u(app, digest) where r.app = u.app and r.digest = u.digest",
        )
        .bind(&apps)
        .bind(&digests)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Forgets releases nobody used for [`UI_RETENTION_DAYS`] and nobody pins, except the one an
    /// app version runs; returns them so their files can be deleted.
    pub async fn purge_ui_releases(&self) -> Result<Vec<(String, String)>> {
        let mut tx = self.db.system_tx().await?;
        let gone = sqlx::query_as(
            "delete from ui_release r where r.last_used_at < now() - make_interval(days => $1) \
             and not exists (select 1 from tenant_ui_pin p where p.app = r.app and p.digest = r.digest) \
             and not exists (select 1 from ui_release_version v where v.app = r.app and v.digest = r.digest \
               and v.created_at = (select max(w.created_at) from ui_release_version w where w.app = v.app and w.version = v.version)) \
             returning app, digest",
        )
        .bind(UI_RETENTION_DAYS as i32)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(gone)
    }
}

/// Administrators change a workspace's addresses and short paths; nobody does while the workspace is read-only (closed or
/// restricted): they stay as they are, so people can still read and export there. The operator's tasks still can.
pub(crate) fn may_change_addresses(ctx: &Ctx) -> Result<()> {
    if !ctx.is_admin {
        return Err(Error::Forbidden("administrator access required".into()));
    }
    if ctx.read_only {
        return Err(Error::Forbidden("this workspace is read-only now that its account is closed or restricted: its addresses stay as they are".into()));
    }
    Ok(())
}

/// `shell`, `portal` or `site:{app}`.
fn check_target(target: &str) -> Result<()> {
    let site = target.strip_prefix("site:").is_some_and(|a| {
        a.len() >= 2
            && a.len() <= 40
            && a.starts_with(|c: char| c.is_ascii_lowercase())
            && a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    if matches!(target, "shell" | "portal") || site { Ok(()) } else { Err(Error::field("target", "must be shell, portal or site:{app}")) }
}

/// (how many live addresses `tenant` may hold, how many it holds).
async fn address_room(tx: &mut crate::db::Tx, tenant: Uuid) -> Result<(i64, i64)> {
    Ok(sqlx::query_as(
        "select coalesce((select (quotas->>'addresses')::bigint from tenant where id = $1), $2), \
         (select count(*) from tenant_host where tenant_id = $1 and released_at is null)",
    )
    .bind(tenant)
    .bind(ADDRESSES_DEFAULT)
    .fetch_one(&mut **tx)
    .await?)
}

/// Refuses an address past the workspace's limit. One change of a workspace's addresses at a time (a lock held to the
/// end of `tx`), so two at once cannot both take its last place, on one replica or many.
async fn room_for_address(tx: &mut crate::db::Tx, tenant: Uuid) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum-hosts:' || $1::text, 0))").bind(tenant).execute(&mut **tx).await?;
    let (limit, used) = address_room(tx, tenant).await?;
    if used >= limit {
        return Err(Error::invalid(format!("a workspace has at most {limit} addresses")));
    }
    Ok(())
}

/// What an audit record says of a shape: only the parts set.
fn said_shape(said: &mut Value, shape: &HostShape) {
    for (k, v) in [("home", serde_json::json!(shape.home)), ("apps", serde_json::json!(shape.apps)), ("label", serde_json::json!(shape.label))] {
        if !v.is_null() {
            said[k] = v;
        }
    }
    if shape.forward {
        said["forward"] = serde_json::json!(true);
    }
}

/// A forwarding address needs a main address to send people to.
async fn main_to_forward_to(tx: &mut crate::db::Tx, tenant: Uuid) -> Result<()> {
    match main_host(tx, tenant, "shell").await? {
        Some(_) => Ok(()),
        None => Err(Error::field("forward", "this workspace has no main address to send people to")),
    }
}

/// The main address never forwards (docs/operations.md, "Addresses of a workspace").
async fn not_forwarding(tx: &mut crate::db::Tx, host: &str) -> Result<()> {
    let forward: bool = sqlx::query_scalar("select coalesce((select forward from tenant_host where host = $1), false)").bind(host).fetch_one(&mut **tx).await?;
    if forward { Err(Error::invalid("a forwarding address cannot be the main one: turn its forwarding off first")) } else { Ok(()) }
}

/// The main address of `tenant`'s `target`, live and working.
async fn main_host(tx: &mut crate::db::Tx, tenant: Uuid, target: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "select host from tenant_host where tenant_id = $1 and target = $2 and is_primary and released_at is null and (kind = 'subdomain' or verified_at is not null)",
    )
    .bind(tenant)
    .bind(target)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Makes `host` the main one of its target in `tenant`: the others first lose it (the unique index `tenant_host_primary`
/// allows one at a time).
async fn primary_in(tx: &mut crate::db::Tx, tenant: Uuid, host: &str) -> Result<()> {
    sqlx::query(
        "update tenant_host set is_primary = false where tenant_id = $1 and is_primary and host <> $2 \
         and target = (select target from tenant_host where host = $2 and tenant_id = $1)",
    )
    .bind(tenant)
    .bind(host)
    .execute(&mut **tx)
    .await?;
    sqlx::query("update tenant_host set is_primary = true where tenant_id = $1 and host = $2").bind(tenant).bind(host).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{HostShape, alias_shaped, check_shape, name_under_base, normalize_host, target_shaped};

    #[test]
    fn a_name_under_the_base_is_one_label_ending_in_its_own_slug() {
        assert_eq!(name_under_base("sales", "acme", ".Ferrum.Cloud.").unwrap(), "sales--acme.ferrum.cloud");
        assert_eq!(name_under_base("eu-2", "acme", "ferrum.cloud").unwrap(), "eu-2--acme.ferrum.cloud");
        for bad in ["", "Sales", "sa--les", "-sales", "sales-", "s.a", "s_a", &"a".repeat(31)] {
            assert!(name_under_base(bad, "acme", "ferrum.cloud").is_err(), "{bad:?}");
        }
        // Read as the service's own, or as a name in another script.
        for kept in ["accounts", "hooks", "www", "api", "admin", "login", "mail", "xn"] {
            assert!(name_under_base(kept, "acme", "ferrum.cloud").is_err(), "{kept}");
        }
        // One DNS label of at most 63 characters, or nothing.
        assert!(name_under_base(&"a".repeat(30), &"b".repeat(31), "ferrum.cloud").is_ok());
        assert!(name_under_base(&"a".repeat(30), &"b".repeat(32), "ferrum.cloud").is_err());
    }

    #[test]
    fn what_an_address_shows_is_for_shell_addresses_and_kept_tidy() {
        let shape = |home: Option<&str>, apps: Option<&[&str]>, label: Option<&str>| HostShape {
            home: home.map(str::to_string),
            apps: apps.map(|a| a.iter().map(|s| s.to_string()).collect()),
            label: label.map(str::to_string),
            forward: false,
        };
        assert_eq!(check_shape("portal", &HostShape::default()).unwrap(), HostShape::default(), "nothing set: any address");
        assert!(check_shape("portal", &shape(Some("crm"), None, None)).is_err());
        assert!(check_shape("site:commerce", &shape(None, None, Some("Shop"))).is_err());
        assert_eq!(
            check_shape("shell", &shape(Some("crm"), Some(&["crm", "helpdesk", "crm"]), Some("  Sales "))).unwrap(),
            shape(Some("crm"), Some(&["crm", "helpdesk"]), Some("Sales"))
        );
        assert_eq!(check_shape("shell", &shape(None, Some(&[]), Some(" "))).unwrap(), shape(None, Some(&[]), None), "an empty label is none");
        for bad in [shape(Some("CRM"), None, None), shape(None, Some(&["x"]), None), shape(None, None, Some(&"x".repeat(41))), shape(None, None, Some("a\nb"))]
        {
            assert!(check_shape("shell", &bad).is_err(), "{bad:?}");
        }
        let many: Vec<String> = (0..51).map(|i| format!("app{i}")).collect();
        assert!(check_shape("shell", &HostShape { apps: Some(many), ..Default::default() }).is_err());
        // A forwarding address: a `shell` one, with where `/` goes, and nothing a bar would show.
        let forward = |s: HostShape| HostShape { forward: true, ..s };
        assert_eq!(check_shape("shell", &forward(shape(Some("crm"), None, None))).unwrap(), forward(shape(Some("crm"), None, None)));
        assert!(check_shape("portal", &forward(HostShape::default())).is_err(), "never the portal");
        assert!(check_shape("site:commerce", &forward(HostShape::default())).is_err(), "nor a site");
        assert!(check_shape("shell", &forward(shape(None, Some(&["crm"]), None))).is_err(), "no launcher");
        assert!(check_shape("shell", &forward(shape(None, None, Some("Sales")))).is_err(), "no label");
        assert!(check_shape("shell", &forward(shape(None, None, Some(" ")))).is_ok(), "an empty label is none");
    }

    #[test]
    fn short_paths_and_their_targets_keep_to_this_host() {
        for ok in ["sales", "q4", "team-a", &"a".repeat(30)] {
            assert!(alias_shaped(ok), "{ok}");
        }
        for bad in ["s", "4q", "-a", "Sales", "a/b", "a.b", &"a".repeat(31)] {
            assert!(!alias_shaped(bad), "{bad}");
        }
        for ok in ["/", "/crm/", "/crm/e/Lead?view=mine", "/data/e/Customer"] {
            assert!(target_shaped(ok), "{ok}");
        }
        for bad in ["crm", "//evil.example/x", "/\\evil.example", "https://evil.example", "/crm#x", "/a b", "/a\nb", &format!("/{}", "a".repeat(2000))] {
            assert!(!target_shaped(bad), "{bad}");
        }
    }

    #[test]
    fn proof_and_resolvers_are_read_from_settings() {
        use super::{DomainProof, dns_resolvers};
        assert_eq!(DomainProof::from_setting("", true), Ok(DomainProof::Record), "a cluster of many workspaces");
        assert_eq!(DomainProof::from_setting(" ", false), Ok(DomainProof::Points), "one company's: as before");
        assert_eq!(DomainProof::from_setting("Points", true), Ok(DomainProof::Points));
        assert_eq!(DomainProof::from_setting("record", false), Ok(DomainProof::Record));
        assert!(DomainProof::from_setting("dns", true).is_err());
        assert!(dns_resolvers("").unwrap().is_empty(), "the system's");
        let read = dns_resolvers("10.0.0.2, 10.0.0.3:5353,[2001:db8::2]:53, 2001:db8::3").unwrap();
        let shown: Vec<String> = read.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["10.0.0.2:53", "10.0.0.3:5353", "[2001:db8::2]:53", "[2001:db8::3]:53"]);
        assert!(dns_resolvers("dns.example.com").is_err(), "addresses, not names");
    }

    /// A DNS server for one test: each question for a name `zone` holds is answered with its records; a name in
    /// `silent` gets no answer at all; any other name does not exist.
    async fn dns(zone: Vec<(&'static str, Vec<hickory_resolver::proto::rr::Record>)>, silent: &'static [&'static str]) -> std::net::SocketAddr {
        use hickory_resolver::proto::op::{Message, OpCode, ResponseCode};
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                let Ok(asked) = Message::from_vec(&buf[..n]) else { continue };
                let Some(q) = asked.queries.first().cloned() else { continue };
                let name = q.name().to_ascii().to_ascii_lowercase();
                if silent.contains(&name.as_str()) {
                    continue;
                }
                let mut answer = Message::response(asked.metadata.id, OpCode::Query);
                answer.metadata.recursion_desired = true;
                answer.metadata.recursion_available = true;
                answer.add_query(q);
                match zone.iter().find(|(n, _)| *n == name) {
                    Some((_, records)) => {
                        answer.add_answers(records.clone());
                    }
                    None => answer.metadata.response_code = ResponseCode::NXDomain,
                }
                let _ = socket.send_to(&answer.to_vec().unwrap(), from).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn a_domain_is_proven_by_its_own_record_alone() {
        use super::domain_record;
        use hickory_resolver::proto::rr::rdata::{CNAME, TXT};
        use hickory_resolver::proto::rr::{Name, RData, Record};
        const TOKEN: &str = "0123456789abcdef0123456789abcdef";
        let txt = |name: &str, strings: &[&str]| {
            Record::from_rdata(Name::from_ascii(name).unwrap(), 60, RData::TXT(TXT::new(strings.iter().map(|s| s.to_string()).collect())))
        };
        let ours = format!("ferrum-domain={TOKEN}");
        let zone = vec![
            ("_ferrum.right.test.", vec![txt("_ferrum.right.test.", &[&ours]), txt("_ferrum.right.test.", &["v=spf1 -all"])]),
            ("_ferrum.wrong.test.", vec![txt("_ferrum.wrong.test.", &["ferrum-domain=fedcba9876543210fedcba9876543210"])]),
            // Another workspace's token for the same domain beside this one's: two claims, so neither is proven.
            ("_ferrum.both.test.", vec![txt("_ferrum.both.test.", &[&ours]), txt("_ferrum.both.test.", &["ferrum-domain=another-workspaces-token"])]),
            ("_ferrum.empty.test.", vec![]),
            // An alias to a name that holds the right record, answered as a resolver answers it: the alias and the record.
            (
                "_ferrum.alias.test.",
                vec![
                    Record::from_rdata(
                        Name::from_ascii("_ferrum.alias.test.").unwrap(),
                        60,
                        RData::CNAME(CNAME(Name::from_ascii("_ferrum.right.test.").unwrap())),
                    ),
                    txt("_ferrum.right.test.", &[&ours]),
                ],
            ),
            ("_ferrum.split.test.", vec![txt("_ferrum.split.test.", &["ferrum-domain=", &TOKEN[..10], &TOKEN[10..]])]),
            ("_ferrum.upper.test.", vec![txt("_FERRUM.UPPER.TEST.", &[&ours])]),
        ];
        let at = [dns(zone, &["_ferrum.slow.test."]).await];
        let proven = async |host: &str| domain_record(&at, host, TOKEN).await;
        assert!(proven("right.test").await, "the right record, beside one of another kind");
        assert!(!proven("wrong.test").await, "a wrong token");
        assert!(!proven("both.test").await, "another workspace's token beside it");
        assert!(!proven("none.test").await, "no such name");
        assert!(!proven("empty.test").await, "no record");
        assert!(!proven("alias.test").await, "an alias there, though the name it leads to holds the right record");
        assert!(proven("split.test").await, "the record in several strings");
        assert!(proven("upper.test").await, "the answer's name in upper case");
        assert!(proven("Right.TEST").await, "the domain written in upper case");
        assert!(!proven("right.test.evil").await && !proven("").await, "another name, or none");
        let started = std::time::Instant::now();
        assert!(!proven("slow.test").await, "no answer in time");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(!domain_record(&["127.0.0.1:9".parse().unwrap()], "right.test", TOKEN).await, "no server there");
    }

    #[test]
    fn hosts_are_normalized_or_refused() {
        assert_eq!(normalize_host("ERP.Acme.com.").as_deref(), Some("erp.acme.com"));
        assert_eq!(normalize_host("acme.ferrum.cloud:443").as_deref(), Some("acme.ferrum.cloud"));
        for bad in ["localhost", "[::1]:5150", "-a.com", "a-.com", "a..com", "a_b.com", "", "é.com"] {
            assert_eq!(normalize_host(bad), None, "{bad}");
        }
        assert_eq!(normalize_host(&format!("{}.com", "a".repeat(64))), None);
    }
}
