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
  document API, lists and filters, reports and Studio, live updates, MCP and plugins.

## Controls

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
  `crates/domain/src/nic_keys`, verified against NIC's own signed sample).
* Outbound requests (webhooks, plugins, identity providers, the IRP): https, hosts resolved once and
  pinned, private and loopback addresses refused, no redirects, time and size limits. Plugins
  reach only the hosts they declare; identity providers on a private network need
  `FERRUM_SSO_ALLOW_PRIVATE`.
* Plugins run as WebAssembly without ambient access, with CPU fuel, a memory cap, a host-call
  budget and a nesting limit; a failing plugin fails closed.
* Request bodies, uploads, exports and list pages have size limits; database statements time out
  (15 s by default).

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
* Security keys (WebAuthn) and SAML are not supported.
* Workspace administrators author print templates (sandboxed as above), automations and roles:
  they are trusted with their workspace.
