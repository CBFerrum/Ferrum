# Security

How Ferrum keeps workspaces apart and data safe, what was reviewed, and what remains for an
operator to know. Report a vulnerability privately to the maintainers rather than in a public issue.

## Model

* **Workspaces (tenants)** share one PostgreSQL database. Every tenant row carries its
  `tenant_id`, and row-level security is forced on every tenant table: a transaction sees only the
  workspace it was opened for. Ferrum connects as an ordinary role and refuses to start as a
  superuser or a role with `BYPASSRLS`. Work that spans workspaces (sign-in, OAuth tokens,
  single sign-on in flight) runs in an explicit system context.
* **Who acts**: people (a password with optional two-factor sign-in, or single sign-on), API keys,
  OAuth clients (MCP assistants), and plugins, each through its own service user whose role is
  exactly its declared permissions. Every change lands in the audit trail with who made it.
* **What they may do**: roles grant read, write, create, delete, submit, cancel, export and import
  per entity, narrowed to the user's own records, to record scopes (companies) or to records whose
  fields hold given values, and per field group. The kernel enforces them on every path: the
  document API, lists and filters, reports and Studio, live updates, MCP and plugins. A role may be
  given until a moment, and pairs of roles an administrator declares are never held by one person
  (the database refuses the grant). A field in a group may show those outside it a mask (the last
  four characters) rather than nothing, and a field marked `no_export` is left out of every list
  export, whoever exports it. A workflow transition naming roles needs one of them and read access to
  the record; one naming none needs the right to change the record; portal people and guests take only
  transitions naming a role of theirs.

## Who can see what

What an operator can do depends on who runs it, and there is no profile in which whoever runs a process cannot read what
that process handles. Secrets are sealed with a key the deployment holds, so someone with only the database cannot read them.
A customer's own key means that data is out of reach of anyone without it, and not of the process using it. Support reaches a
workspace only by a grant from the customer, on the record: [support-access.md](support-access.md). In your own network that
operator is you ([platform/customer-controlled.md](platform/customer-controlled.md)).

[platform/asvs.md](platform/asvs.md) maps this to the controls it is held to, and says what is open.

## Controls

**Zones** ([operations](operations.md#zones))
* Where a zone admits no staff, a staff credential of any kind is refused where the request's context is
  built, on every request, and the decision is never cached by person. The zone comes from the replica's
  pin, the host's row (set only by the operator), the caller's address as the configured proxies report it,
  or the default; nothing else the client sends. An error deciding it refuses staff.
* A staff sign-in there fails as a wrong password does, before the password is read, without counting
  toward lockout, and in the time an unknown address takes. Refusals of signed-in credentials say only
  that the credential works elsewhere, and where.
* Where a zone limits staff to some apps, their context is narrowed where it is built, on every request
  (never cached by person): entities of other packages are not reached by them nor by anything that acts for
  them (the system's contexts made from theirs, configured effects, `invoke`, automations on their changes,
  `links.` and rule totals), and answer as missing ones. An entity whose package is not known, an app id that
  cannot be resolved, a zone name a worker does not hold: hidden or refused, never shown. Jobs, events, process
  runs and approvals carry the zone of the request that made them, and are done in it. Roles only other apps
  ship are dropped from the context. Signed-in endpoints of other apps' packages, which run their own SQL, are
  refused; reports are kept to the packages of the app that gives them. Nobody is an administrator there
  unless the rule says so.
* Zones do not protect against a compromised replica: it reaches the whole database.

**Staff networks** ([operations](operations.md#staff-networks))
* A workspace may name the networks its staff work from. Staff on a people's credential (session, token,
  OAuth token, app password, app token, socket, sign-in) from elsewhere are refused where the request's
  context is built, after the zone, with `403` `network`, which names no network. API keys, SCIM keys and
  support access are not held to them; keys have their own.
* A staff sign-in from outside fails as a wrong password does, before any secret is read, without counting
  toward lockout and without using up a code. A setting that cannot be read refuses.
* Saving them in `enforce` needs the saver's own address inside; the operator can clear them.
* Sign-ins, second steps, failed and refused attempts of known accounts (capped at three an hour, then a
  summary), break-glass sign-ins and sessions' ends are audit events, with address, browser, zone and method.
  Attempts for unknown addresses are only counted. No secret is ever in them.
* With `new_device_notice`, staff are told once per new device (user agent without versions) that their
  account signed in from it.
* Under enforced single sign-on, only the administrators `break_glass` names (all of them when unset) keep a
  password; the list must name at least one active administrator, and each such sign-in is its own audit event.
* Audit events can go to the operator's sink (signed batches, in order, at least once, one sender) and to a
  workspace's own webhooks on `audit.*` topics; never to a `*` webhook or an app's. The sink's secret is never
  logged or sent.
* A workspace may end browser sessions idle longer than `idle_minutes` (to within about two minutes); the
  session is revoked, not only refused. A socket's own rechecks are not use.

**Sites and devices** ([threat model](registry/threat-model.md), [authority](operations-authority.md))
* Ferrum never replaces a safety system, and a safety function never depends on it.
* Each device signs its requests with its own Ed25519 key. The server keeps public keys only. Enrollment
  codes are one-time and short-lived. Signatures carry a rising timestamp, so a replay fails.
* One gateway holds a domain at a time, under an epoch that only rises. Whatever a replaced holder says
  joins the history and changes nothing. Nothing fails over by itself.
* Operators with a role cannot edit a warehouse document they only scan against, and separation of
  duties is enforced where it matters: whoever wrote a revision does not approve it, whoever placed a
  hold does not release it, repaired does not mean returned to service.
* A definition from the platform runs on a gateway, so the gateway limits it: hosts by `allow`, ports
  to listen on by `listen`, programs to `drivers_dir`, certificates to its own folder, and serial ports
  to `/dev`. Taking anything from the platform at all is off until `managed: true`.
* A flow holds no code and cannot loop, is simulated and approved by someone else before it runs, and
  can only ask the ledger for a command. Simulation runs the code a gateway runs, on the platform,
  with nothing reaching a device.
* A release is believed only if a key the gateway was installed with signed it, and an old one cannot
  be replayed. Peers and relays carry signed requests and hold no platform secrets. Webhooks take a
  bearer token, compared by hash. Peers and webhooks are not encrypted: keep them on a segmented
  network.

**The assistant** ([operations](operations.md#the-assistant))
* It acts only as the signed-in person, through the same permission-checked tools as MCP clients
  and the REST API; it is never an administrator for someone who is not. App screens cannot call
  it, and guests and portal people do not have it.
* Every tool that is not read-only waits for the person's decision in the conversation; refused
  or unanswered calls never run. Record contents, mail and documents the tools read can carry text
  written to mislead a model (prompt injection): its instructions treat them as data, and the
  approval step is the boundary that holds when they do not.
* Tool inputs that stream from the model are parsed strictly and checked against each tool's
  schema before anything runs; a reply that was cut off or declined runs none of its calls.
* What goes to the model's API is limited to what the person could read; the key stays on the
  server; workspaces can turn the assistant off.

**Signing in**
* Passwords: Argon2id; at least 10 characters with three character classes; 5 failures lock the
  account for 15 minutes; sign-in attempts are rate limited per address and workspace across
  replicas; an unknown email costs the same time as a known one.
* Sessions: signed tokens (8 hours by default) carrying a token version; changing the password,
  "sign out everywhere", an administrator's two-factor reset and deactivation end every session.
* Passkeys (WebAuthn): the relying party is the host signed in on; registration needs a session that
  signed in or stepped up in the last ten minutes and asks for no attestation; every ceremony's
  challenge is random, kept server-side for five minutes and taken once; the client data must name
  the ceremony, the challenge and this site's origin, outside any other site's frame; the
  authenticator must report the person present and verified; signatures are checked with `ring`
  (ES256, RS256 of 2048 bits or more, Ed25519), and a signature counter that does not move on is
  refused as a possible copy. A credential belongs to one account in the cluster. A verified passkey
  counts as both factors.
* Ferrum accounts (shapes B and C; one sign-in for many workspaces): a workspace's account joins a
  Ferrum account only on proof that it is the person's: its own password (and its authenticator code
  when it has one), a code sent to its address, its workspace's single sign-on, or confirming when the
  Ferrum account's address was proven by a mailed code and that account has no authenticator app;
  never on a matching address alone. Joining is recorded in the workspace's audit trail. A Ferrum
  account made from a workspace's password takes that account's password hash, sealed authenticator
  secret (sealed again for the new account) and unused recovery codes, after a code mailed to the
  address. An account whose address is unproven (a server without mail) is told nothing about
  which workspaces have its address: every workspace shows it the same step, and the proofs fail.
  Sign-in hands over to a workspace's host as a one-use code (32 random bytes, kept as a SHA-256)
  for one membership and one host, good for a minute, checked again when taken (still active, still
  joined); the host accepts it only with the `state` it sent, kept in its own `__Host-` cookie, so no
  one else's sign-in lands in a browser (login CSRF), and answers without a referrer. Ports are
  refused in the host names of these steps over https. The Ferrum account's sessions and tokens work
  only on the accounts host, and no workspace's work there; it gets no API keys. Once joined, the
  workspace's own password and passkeys open nothing while the cluster signs in through accounts,
  for pages and API clients alike; "sign out everywhere" ends every session of the account and of
  each workspace it joined, raising their token versions (OAuth grants and tokens end too). The
  accounts host's pages cannot be framed by other sites. Unknown hosts are served nothing; custom
  domains under the cluster's own domain are refused, and `accounts`, `hooks`, `www` and `api` are
  no workspace's names.
* Browsers and released addresses: on a host of one workspace's, the bar's runtime remembers that
  workspace in the browser; if the host ever answers for another (a released address's next owner, a
  year on), everything the browser kept for the origin is cleared (local and session storage,
  IndexedDB, caches, service workers) before the page goes on. Signing out sends
  `Clear-Site-Data: "cache"` (not storage: offline work waits to be sent).
* Ferrum accounts across regions: a region vouches for its own accounts only, to the regions it lists,
  at their fixed callback address: a one-use, one-minute code bound to the asking host's PKCE
  challenge (S256), so only the server that started the sign-in, holding the verifier, can take it,
  server to server over TLS from the address in its own configuration (no redirects followed). The
  visited region keeps the `state` and verifier in a `__Host-` cookie, holds no credentials for the
  visiting account (password, two-factor sign-in and passkeys are refused there), and never lets a
  visitor take the address of an account of its own.
* Two-factor sign-in: TOTP secrets sealed with the server key; each code accepted once; recovery
  codes stored as hashes; wrong codes count toward the lockout, and a correct password does not
  clear the count while the second step is pending; the ticket between the steps is no session;
  a workspace can require it, and then a session without it can only set it up.
* Single sign-on (OpenID Connect): authorization code flow with PKCE, a nonce, and a state bound
  to the browser that started it, used once within ten minutes; ID tokens checked against the
  provider's published keys (asymmetric algorithms only) for issuer, audience/authorized party,
  expiry and nonce; only email addresses the provider verified (or, for providers that do not say,
  within the domains the administrator listed); accounts linked to their identity at the provider,
  so the same address held later by someone else does not open them.
* Single sign-on (SAML 2.0, crates/saml): signatures checked by xmlsec1 (through samael) against the
  certificates in the provider's metadata, RSA or ECDSA with SHA-256 or better (no SHA-1), and only
  what a valid signature covers is read, so a signed element wrapped in forged ones proves nothing;
  metadata without a signing certificate is refused, as is metadata past its `validUntil`. An answer
  must be from the provider, in answer to a request of this workspace's still open (none unasked
  for), for this workspace's entity ID (an audience restriction is required) and its assertion
  consumer service, within its time window, with a bearer confirmation; it counts once, and a refused
  one uses the sign-in up. The provider posts to the server, which sends the browser on with a
  one-time code, only to an address of this server's own, and the browser that started the sign-in
  finishes it. SAML never says whether an address was checked, so a SAML provider needs its email
  domains listed. Each workspace signs its requests (RSA-SHA256) with a key of its own, made when
  first wanted and sealed by the server key, whose certificate its metadata publishes. A server
  built with SAML checks at `doctor` that xmlsec1 signs and verifies.
* API keys: 256 random bits, stored as SHA-256; OAuth access tokens last an hour, refresh tokens
  rotate.

**Data**
* Secrets (secret fields such as mail passwords, two-factor secrets, single sign-on client
  secrets) are sealed with AES-256-GCM under `FERRUM_SECRET_KEY`, bound to their record, and never
  appear in events, the audit log, exports or webhooks.
* The audit trail is a hash chain the database only appends to; personal-data erasure redacts
  values in it while keeping each event's place and hash.
* API responses carry `Cache-Control: no-store`, so browsers keep neither stale nor private copies.
* Request logs record the path without the query string: queries carry WOPI tokens, OAuth and
  sign-on codes and signed grants.
* Online editing (Drive over WOPI): the editor gets a token bound to the workspace, the file, the
  person and ten hours. When the editor publishes a proof key, each call must also be signed with
  it.

**Input and output**
* SQL: values are always bound; identifiers come only from metadata and are quoted; filters,
  sorts and searches may use only fields the caller can read.
* Templates (prints, mail): MiniJinja with HTML auto-escaping and a fuel limit; print pages are
  served with a Content Security Policy that runs no script and loads nothing but inline styles
  and images, also when shown from the browser.
* E-invoices: the direct IRP connection signs in with the credentials RSA-encrypted under the
  IRP's public key and exchanges payloads AES-encrypted under the session key the IRP returns
  (its client secret and password are sealed secret fields); a signed QR code is checked for the
  invoice, its number and the seller, and for its signature: under the key the company names, or
  else, for codes NIC issued, under NIC's published signing keys (shipped in
  India's add-on, `addons/india/src/nic_keys` in ferrum-app-erp, verified against NIC's own signed
  sample).
* Outbound requests (webhooks, plugins, identity providers, the IRP, push services): https, hosts
  resolved once and pinned, private and loopback addresses refused, no redirects, time and size
  limits. Plugins reach only the hosts they declare; identity providers on a private network need
  `FERRUM_SSO_ALLOW_PRIVATE`; pushes go only to the browsers' push services
  (`FERRUM_PUSH_SERVICES`), whatever endpoint a subscription names.
* Plugins run as WebAssembly without ambient access, with CPU fuel, a memory cap, a host-call
  budget and a nesting limit; a failing plugin fails closed.
* Files: attaching one to a record needs the right to change the record; one tied to no record is served
  only to its uploader and administrators until a record they save names it, and a save may not name a
  file its saver could not download. Portal people's loose uploads are limited to 20 an hour.
* Request bodies, uploads, exports and list pages have size limits; database statements time out
  (15 s by default).

**Published lists** ([BD-15](builder/decisions.md#bd-15-published-lists), [api](api.md))
* A workspace may show some fields of some records to anyone without signing in, read only, at
  `/api/public/{workspace}/lists/{name}` (JSON, and CSV when offered) and `/public/{workspace}/lists/{name}`.
  Only the fields the list names are ever sent: never a record's id, owner or timestamps, and a link as
  the linked record's title. Its filter and order may use other fields; their values are not sent.
* What cannot be published is refused when the list is saved: email, phone, secret and attachment
  fields, tables, fields in a permission group, fields never exported, people's personal data a package
  declares (a field, or a whole entity), and links whose titles are any of these or that name a restricted
  entity. A later change that makes a listed field unpublishable drops it from the list; a filter field that
  goes takes the list down rather than widen it. A submittable entity's list shows submitted records only.
* No such workspace, an inactive one, no such list and a list taken down give one identical 404, so the
  address does not say which lists exist (the answer's timing still differs a little between a workspace
  that does not exist and one that does; that a workspace exists is not secret elsewhere either).
* Each request counts against the public limiter (per address and workspace) before any lookup; the
  query is capped at the list's limit (1000 rows at most) and a 5-second statement timeout; answers are
  cacheable for a minute, so a list taken down may be seen for up to a minute where it was cached.
* The page runs no script, fetches nothing (`default-src 'none'`, its own style by hash), escapes every
  value, cannot be framed and asks not to be indexed. CSV cells that a spreadsheet would run as a formula
  are made plain by the one guard the platform's CSV uses.

**Availability**
* Rate limits held in the database (sign-in, public endpoints, mail) work across replicas and
  fail open if the database is briefly unreachable. Every client address has a request budget too
  (6000 a minute on each replica by default), taken from the entry `FERRUM_PROXY_HOPS` from the
  right of `X-Forwarded-For`: what a client writes there itself is never counted as its address.
* Every user has a request budget (3000 a minute on each replica by default, across all their
  sessions and API keys): a runaway script with a valid token is slowed down to `429`s instead of
  slowing everyone down.
* Replicas are stateless (PostgreSQL, S3), behind a pod disruption budget and an autoscaler;
  background loops retry with backoff.

**Browser**
* The web app's Content Security Policy allows scripts only from its own bundle; responses carry
  `X-Content-Type-Options: nosniff`, a referrer policy and same-origin framing. Session tokens live
  in session storage and travel in the `Authorization` header (no cookies, so no cross-site request
  forgery).
* Notifications on devices (Web Push, [operations](operations.md#notifications-on-devices-web-push)):
  each message is encrypted for the one device (RFC 8291), so the push service carries what it
  cannot read, and signed with the server's VAPID key, sealed with the secret key. A device's
  subscription belongs to its browser session and ends with it (signing out there or everywhere,
  the person made inactive); someone acting as another person cannot subscribe to their
  notifications; a subscription another person's browser session registered last is theirs. The
  core worker opens only addresses on its own host, and has no fetch handler.

## Review, 2026-10-30

Scope: passwords, two-factor sign-in, single sign-on, API keys and OAuth; tenancy and permissions;
outbound requests; templates and prints; plugins and the Commerce payment gateways (signatures,
replays, idempotency); uploads; dependency advisories (`cargo audit` in CI, `npm audit`: none).

Found and fixed:
* API responses had no `Cache-Control`. A browser reused its copy of a record after the engine
  changed it without a new version (a recorded IRN, an e-way bill), so the screen showed stale
  data, and per-user data could stay in a shared machine's cache. All `/api/` responses are now
  `no-store` unless a handler says otherwise.
* Two-factor sign-in was designed so that re-entering the password does not reset the count of
  wrong codes; without that, codes could be guessed indefinitely.
* Every writing transaction sent a NOTIFY (to wake the event dispatcher), and PostgreSQL
  serializes the commits of notifying transactions: under load, writes slowed each other down
  (an availability risk under bursts). Writers no longer notify; dispatchers poll. See the
  runbook's Capacity section for the measurements.
* The single sign-on start compared its return address with the request's host, which breaks
  behind proxies that rewrite it; the return address is safe without it (the provider sends the
  browser only to registered addresses, and only the browser holding the binding can finish).
* The Kubernetes manifests relied on ingress-nginx, retired in March 2026 (no more security fixes),
  for TLS, routing and the per-address request limit, and on CloudNativePG's built-in backups,
  removed in 1.31. They now use Gateway API routes, a per-address budget in the server itself, and
  the Barman Cloud plugin, and CI deploys them to a real cluster for every image.
* Deploying the manifests to a real cluster showed two availability faults. Losing the database
  primary held writes up for three minutes: CloudNativePG's smart shutdown waits for every client
  to disconnect, and the API's pooled connections never do. The primary now ends sessions after
  15 s and writes resume in about 25 s. And the API's spread across nodes counted a tainted node
  (one pods may not run on) as a node without a replica, so a rolling update could never place
  its first new pod; it now counts only usable nodes, per revision.
* The client's address for sign-in limits was always the last `X-Forwarded-For` entry: behind a
  CDN or an HTTP load balancer, every user shared the CDN's address and its sign-in budget.
  `FERRUM_PROXY_HOPS` says how many proxies to trust.

Known and accepted:
* NIC rotates its QR signing keys (the shipped ones run to 1 May 2027). A code under a NIC key a
  release does not know is refused when entered by hand, until NIC's new key is added to
  `nic_keys` or entered as the company's IRP QR key. From the direct connection, where the code
  comes straight from the IRP, it is recorded and the gap logged so invoicing does not stop. A
  private IRP's codes are checked only when the company enters that IRP's key.
* Session tokens are readable by script on the app's origin; the Content Security Policy is what
  keeps foreign script out.
* SAML: no single logout, and encrypted assertions are refused in so many words (TLS carries
  them; samael opens only some of XML Encryption's algorithms, in its own code rather than
  xmlsec1's); a provider whose policy needs either cannot be used. A workspace's key does not
  rotate by itself: it is good for ten years.
* Workspace administrators author print templates (sandboxed as above), automations and roles:
  they are trusted with their workspace.
