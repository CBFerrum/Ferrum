# Security rules

The rules an app keeps so that it does not become the weak point. A native app is trusted code compiled into the
server: inside one process nothing but the shape of the code and the checks keeps it apart from the platform and from
other apps ([docs/registry/threat-model.md](../registry/threat-model.md): "Native app to platform | None: native apps are
trusted code, reviewed and compiled in | A bad dependency in any app"). So each rule below says why, and how it is
checked, or that it is reviewed and nothing checks it. The platform's own model and controls are in
[docs/security.md](../security.md).

## 1. Fail closed, and say why

When something an app depends on cannot be decided (the vault, a setting, a store, a right, a zone), refuse, in plain
words, and log the cause. Never fall back to a weaker mode silently ([docs/plan/stateless.md](../plan/stateless.md),
rule 8).

- `Kernel::outward(tenant)` refuses for a sandbox *and* for a workspace it cannot find or cannot ask about
  (`crates/kernel/src/sandbox.rs:215`).
- A closed vault refuses every verb; the app says "… right now: its keys are unavailable", never uses a plain value instead
  ([08](08-secrets-and-the-vault.md)).
- An error deciding a zone refuses staff; an unknown package is hidden, never shown ([docs/security.md](../security.md),
  *Zones*).
- A connector or a plugin that fails, fails closed.

Checked: the platform's own paths by its suites and mutants; an app's by its own tests of the refusal. Otherwise reviewed.

## 2. Tenant isolation

Every workspace shares one database. Row-level security is forced on every tenant table, and the server's role cannot
bypass it (it refuses to start as a superuser or with `BYPASSRLS`).

- **Open the right transaction.** `k.db.tenant_tx(tenant)` sets the workspace for the transaction
  (`set_config('ferrum.tenant_id', …, true)`); a query in it sees that workspace's rows only. `k.db.system_tx()` and
  `system_tenant_tx` see across workspaces: the threat model names "code that opens one is trusted to filter by tenant"
  as the weakest point of tenant isolation. Use them only for work that spans workspaces (upkeep's list of workspaces),
  and filter by `tenant_id` in every statement you run there.
- **An app's own table** carries `tenant_id` and row-level security like the platform's ([04](04-data-and-records.md)).
- **A `Ctx`** carries the person, the workspace, the schema and the rights; use the caller's, never build a system
  context to do what the caller may not (`Ctx::system` is for the app's own background work).
- **A public request** (`/api/site/{tenant}/{package}/…`) has no user (`SiteRequest::user` is `None`): the app decides
  what an anonymous caller may do and acts with a system or service context itself ([docs/api.md](../api.md), *App
  endpoints*; `crates/kernel/src/hooks.rs:118`).

Checked: row-level security in the database; the platform's isolation suites. An app's use of `system_tx`: reviewed, not
checked.

## 3. Input at trust boundaries

Everything from a browser, a public page, a webhook, a provider, a plugin or a file is untrusted.

| Input | Rule | Where the platform helps |
|---|---|---|
| SQL | Bind every value; an identifier only from metadata, quoted | `crates/kernel/src/ddl.rs` `q`; statements built with `format!` must be wrapped in `sqlx::AssertSqlSafe`, which makes them visible to review |
| Request bodies | A size limit per route | `Site::max_body` (1 MiB default) and `max_body_for`; the server reads no further, at most 1 GiB |
| Rich text | Clean it on the server, whatever the browser sent | `ferrum_kernel::richtext::sanitize_html` (the same allow-list as the kit's editor), `html_to_text` for a mail's text part |
| Templates | Escape every value | Print formats and messages are MiniJinja with HTML auto-escaping and a fuel limit |
| Files | Never served inline as the type they claim when a browser would run it | `files::runs_in_browser`, `ACTIVE_TYPES`; the scanner ([09](09-files-and-storage.md)) |
| CSV going out | A cell a spreadsheet would run as a formula is made plain | The platform's CSV guard (published lists, exports) |
| Addresses a person types | Never a private or loopback address, no redirects | `Kernel::vetted_client` ([11](11-integrations.md)) |
| A webhook | Verify the signature in constant time; refuse replays | A connection's inbound address ([11](11-integrations.md)) |
| JSON from a provider | Parse into types; limit sizes; never trust its ids across workspaces | Reviewed |

## 4. Never say whether something exists

Answer *not found* both for what is not there and for what the caller may not see, so that an id, a name or an address
gives nothing away ([05](05-permissions-and-people.md) explains *not found* versus *refused*):

- A file the caller may not download is `NotFound("file")`, the same as a file that is not there
  (`Kernel::may_download`, `crates/kernel/src/files.rs`); a save naming a file the saver could not download is refused
  without saying whether it exists.
- A published list: "No such workspace, an inactive one, no such list and a list taken down give one identical 404"
  ([docs/security.md](../security.md), *Published lists*).
- A webhook that is not accepted: `401 this call was not accepted`, whatever the reason.
- A sign-in that fails: the same answer for a wrong password and an unknown address, in the same time.
- Another person's portal record: `404`, never `403`.

Checked: the platform's suites for its own routes. An app's own routes: its tests, otherwise reviewed.

## 5. Rate limits

The server already limits every client address (6000 a minute per pool by default) and every user (3000 a minute), and
public site routes (300 a minute per address) ([docs/security.md](../security.md), *Availability*; [docs/api.md](../api.md),
*App endpoints*). An app adds its own where an action costs something or can be abused (a booking, a sign-up, a message
sent, a code checked):

```rust
// crates/kernel/src/lib.rs:378. Keys name the app: bookings|{tenant}|{ip}
k.rate_limit(&format!("bookings|{tenant}|{}", req.client_ip), Duration::from_secs(3600), 20, "too many bookings from here; try again later").await?;
```

It counts across every replica in the database and answers `TooManyRequests` (`429`) with your message past the limit.
When the database cannot be reached it returns the database's error: it fails closed, where the server's own
limiters fall back to the replica's own count ([docs/security.md](../security.md), *Availability*; `Db::count_hit`,
`crates/kernel/src/db.rs:429`).

## 6. Cryptography only through the vault

An app makes, keeps and uses keys only through its vault handle: `sign`, `verify`, `derive`, `seal`, `open`, `carry`
([08](08-secrets-and-the-vault.md)). It adds no crate that keys, seals or signs (`aead`, `aes`, `aes-gcm`, `argon2`,
`bcrypt`, `chacha20poly1305`, `ed25519-dalek`, `hkdf`, `hmac`, `jsonwebtoken`, `openssl`, `p256`, `pbkdf2`, `ring`,
`rsa`, `scrypt`, … the `CRYPTO` set in `scripts/secret_paths.py`). Hash functions alone (`sha1`, `sha2`, `sha3`, `md-5`,
`blake3`) are allowed.

The crates apps used before the vault are listed in docs/registry/secret-paths.md, *Cryptography crates in apps*, each to
move onto the vault's verbs or to stay where a third party's scheme needs it (a government's RSA, a signature format).
The list may only shrink: a new crate fails the check.

Compare secrets in constant time (the vault's `verify` does; Metrics' engine compares its token so). Never compare a
token with `==` on strings.

Checked: `scripts/secret_paths.py`.

## 7. Dependencies and licences

**Prefer a mature project.** Before writing something, look for a mature open-source project to run, depend on or adapt
(the owner's standing rule): LiveKit for meetings, Stalwart for mail, Forgejo for Git, SeaweedFS for the bundled store,
DataFusion in Metrics' engine. `docs/references/` holds what was compared for each app and for object stores
(`docs/references/object-stores.md`). Running a program beside the server is often better than linking its code in: it
keeps its own process, and its updates are its own ([10](10-companions-and-sign-in.md)).

**Only licences Ferrum may ship.** Ferrum is AGPL-3.0-or-later. `scripts/licenses.py check` reads `cargo metadata` of the
platform's build with every pinned app and fails on any Rust dependency whose licence nobody accepted: permissive ones
(MIT, Apache-2.0, BSD, ISC, Zlib, Unicode, CC0, BSL-1.0, PostgreSQL, …), MPL-2.0 (file-level copyleft), and
AGPL-3.0-or-later or -only. A licence expression passes when one alternative is accepted; every part of a conjunction must
pass. A GPL-3.0 or LGPL-3.0 crate is *not* in that list, although `scripts/check_apps.py` accepts an app's own manifest
under them: keep both the app and its dependencies inside what `scripts/licenses.py` accepts. The images a deployment
names must be engines approved in docs/registry/engines.yaml, their licences accepted too.

**Known vulnerabilities.** `cargo audit` runs in `scripts/local_ci.sh audit`; an advisory the build ignores needs an entry
in docs/registry/advisories.yaml with its reason, applicability and a review date, which `scripts/registry.py check`
holds to. Pin versions the platform already builds where you can (Comms takes `zip`, `quick-xml` and `lettre` at the
platform's versions and features, so a server build compiles each once).

**No `unsafe`.** `scripts/secret_paths.py` fails an app that adds an `unsafe` block, fn, impl, trait or extern; the
only ones left are listed (IoT Edge's gateway, which talks to hardware).

## 8. Unsafe content

- **Descriptions.** The README an app's manifest names is shown in the store and the install dialog; it is rendered
  safely there, but write it as plain Markdown without HTML of your own ([22](22-readme-and-repository.md)).
- **HTML from people** is cleaned on the server (`richtext::sanitize_html`) before it is stored or mailed; never trust
  that the browser's editor cleaned it.
- **HTML from outside** (a mail's body) is shown in a sandboxed frame by the interface; Comms cleans incoming mail with
  `ammonia` before it reaches a page.
- **Print pages** run no script and load nothing but inline styles and images (their Content Security Policy).
- **An app's own interface** declares only the origins it needs in `ui.csp` (`connect`, `frame`, `form`, `img`), never
  a wildcard script source ([17](17-the-interface.md)).

## 9. Code from strangers runs in a sandbox

A native app is reviewed and compiled in; nothing else runs inside the server's process. Code nobody reviewed that far
runs elsewhere ([docs/plugins.md](../plugins.md), *Services on their own servers* and *Limits and guarantees*):

| Runtime | Where it runs | What it reaches |
|---|---|---|
| `native` | Compiled into the server | Everything; only in builds of Ferrum, never from a bundle |
| `wasm` | The plugin sandbox: WebAssembly with no ambient access, CPU fuel, a memory cap, 1 000 host calls and 1 MB of output per call, nesting stopped after 4 levels | What its `plugin.json` asks for, consented to at install; only its declared hosts |
| `meta` | Nowhere: metadata only | Its packages' records |
| `service` | The publisher's servers | As an account of its own, the records `access` names, over the API |

An app that runs customers' code (Cloud's builds and sites) runs it in programs of their own, never in the server's
process, and says so in its README. A workspace's own logic runs in the logic platform's limits ([06](06-events-jobs-and-logic.md)).

## 10. Logs and errors

- No secret, token, password or session in a log line, an error, an audit event, an event's payload, a webhook or an
  export. Request logs keep the path without the query string, since queries carry tokens and codes.
- An error a person sees says what to do next and never leaks an internal detail: put the detail in the log
  ([20](20-writing-for-people.md)). Comms and Metrics log the vault's reason at `debug` and show a sentence.
- `{:?}` on a type that holds a secret prints it unless the type hides it: the vault's types do (`New`, `Derived`,
  `oidc::Client`, `oidc::Tokens`, `companion::File`), yours must too.

Checked: reviewed, not checked.

## 11. What goes to review

Each of these is read by a person before a pin, and some fail a check until it is:

| Change | Why | Check |
|---|---|---|
| A direct write to an `e_*` table outside the document pipeline | It skips hooks, validation, permissions and history | `scripts/mutation_paths.py` fails until a person gives it a kind in docs/registry/mutation-paths.md |
| A new cryptography crate, or `unsafe` | Keys outside the vault; memory safety | `scripts/secret_paths.py` (baseline only shrinks) |
| A secret read past its owner's handle (the vault's tables, its ways in) | Ownership of secrets | `scripts/secret_paths.py` |
| A new licence | Distribution | `scripts/licenses.py` |
| A public route, a webhook, a new role or right, `allow_private: true`, `system_tx` in a request path, an HTTP client of the app's own, a statement in `AssertSqlSafe` | Each widens what an outsider or a person can reach | Reviewed, not checked |
| A new operator setting, a new secret, a new service or companion | What the operator must run and rotate | `scripts/check_apps.py`, `scripts/settings.py`; the rest reviewed |

The maintainers' list before pinning is [24](24-review-checklist.md).

## Reporting a vulnerability

Privately to the maintainers, never in a public issue ([docs/security.md](../security.md)). Every app repository has a
`SECURITY.md` saying how ([22](22-readme-and-repository.md)).

## Checked by

- `scripts/secret_paths.py` (cryptography crates, `unsafe`, the vault's tables, ways into the vault).
- `scripts/licenses.py check`, `cargo audit` and `scripts/registry.py check` (advisories), in `scripts/local_ci.sh audit`.
- `scripts/mutation_paths.py` (direct writes).
- Row-level security in the database; the platform's isolation and permission suites with their mutants
  (`scripts/mutants.py`).
- Everything marked "reviewed": a person, before the pin.

## See also

- [05 Permissions and people](05-permissions-and-people.md), [08 Secrets and the vault](08-secrets-and-the-vault.md),
  [11 Integrations](11-integrations.md), [12 Privacy and data duties](12-privacy-and-data-duties.md),
  [23 Dos and don'ts](23-dos-and-donts.md), [24 Review checklist](24-review-checklist.md)
- [docs/security.md](../security.md); [docs/registry/threat-model.md](../registry/threat-model.md);
  [docs/licenses.md](../licenses.md); [docs/plugins.md](../plugins.md)
