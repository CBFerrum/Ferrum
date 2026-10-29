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

**Signing in**
* Passwords: Argon2id; at least 10 characters with three character classes; 5 failures lock the
  account for 15 minutes; sign-in attempts are rate limited per address and workspace across
  replicas; an unknown email costs the same time as a known one.
* Sessions: signed tokens (8 hours by default) carrying a token version; changing the password,
  "sign out everywhere", an administrator's two-factor reset and deactivation end every session.
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

**Input and output**
* SQL: values are always bound; identifiers come only from metadata and are quoted; filters,
  sorts and searches may use only fields the caller can read.
* Templates (prints, mail): MiniJinja with HTML auto-escaping and a fuel limit; print pages are
  served with a Content Security Policy that runs no script and loads nothing but inline styles
  and images, also when shown from the browser.
* Outbound requests (webhooks, plugins, identity providers): https, hosts resolved once and
  pinned, private and loopback addresses refused, no redirects, time and size limits. Plugins
  reach only the hosts they declare; identity providers on a private network need
  `FERRUM_SSO_ALLOW_PRIVATE`.
* Plugins run as WebAssembly without ambient access, with CPU fuel, a memory cap, a host-call
  budget and a nesting limit; a failing plugin fails closed.
* Request bodies, uploads, exports and list pages have size limits; database statements time out
  (15 s by default).

**Availability**
* Rate limits held in the database (sign-in, public endpoints, mail) work across replicas and
  fail open if the database is briefly unreachable; the ingress limits requests per address.
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

Known and accepted:
* There is no per-user request budget beyond the ingress's per-address limit and the statement
  timeout; a runaway integration with a valid key is stopped by revoking the key.
* An e-invoice's signed QR code is checked for this invoice, number and seller, not against the
  IRP's signing key; recording one needs submit permission and lands in the audit trail.
* Session tokens are readable by script on the app's origin; the Content Security Policy is what
  keeps foreign script out.
* Security keys (WebAuthn) and SAML are not supported.
* Workspace administrators author print templates (sandboxed as above), automations and roles:
  they are trusted with their workspace.
