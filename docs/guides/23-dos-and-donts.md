# 23. Dos and don'ts

The whole handbook as one list, by area. Each line says what to do or not do, why, and what catches it: a script, a
test, the server refusing to start, or "reviewed" where only a person does. The guides explain each rule; the
contract (`docs/app-contract.md`) is the rule where they say the same.

"Refused" means the server or the build will not accept it. "Reviewed" means nothing checks it: a person must.

## The app and its repository

| Do / don't | Why | Caught by |
|---|---|---|
| Do put a new app in its own repository, `ferrum-app-<name>`, never in the platform's `crates/` | the platform stays an empty core; an app versions and releases on its own | reviewed |
| Do keep engines general and put a country's or a sector's rules in data or an add-on (`addons/<code>`, `parent:` the app) | one engine serves every country; ERP's engine holds no country (`docs/plan/localization.md`) | reviewed |
| Don't write `if country == "IN"` in an engine or a page | it makes the engine one country's | reviewed |
| Do look for a mature open-source project to run, depend on or adapt before building one, AGPL-compatible only | less code to own; licences Ferrum can ship | `scripts/licenses.py check` for what is compiled in; the choice is reviewed |
| Do license the app under AGPL-3.0 or a licence Ferrum ships | the platform ships it | `scripts/check_apps.py` |
| Do have `README.md`, `CHANGELOG.md`, `LICENSE`, `app.yaml`, `tests/*_it.rs`, and `ui/dist` built when there is an interface | the completeness contract | `scripts/check_apps.py` |
| Do add a `SECURITY.md` | people need a private way to report | reviewed (only Cloud and Code have one) |
| Don't add attribution lines (`Co-Authored-By`, "Generated with") to commits | the owner's rule | reviewed |
| Don't import another app's modules at run time; meet through the API, events, intents, slots, cards, or an operation it offers and your package declares in `uses:` (called with `Kernel::call_tx`) | apps upgrade and go missing independently (`docs/plan/apps-ui.md` §4 rule 1; `docs/app-contract.md`, *Using another app's operations*) | the catalog refuses a use of a package you do not depend on; the rest reviewed |
| Don't read or write another app's tables; read its records through the kernel, after `requires`-ing it | its tables are its own to change | `scripts/mutation_paths.py` for writes to `e_*` tables; reads reviewed |

## The manifest, versions and ranges

| Do / don't | Why | Caught by |
|---|---|---|
| Do fill `id`, `name`, `version`, `publisher`, `summary`, `categories`, `license`, `homepage`, `support`, `description` | the store and the install dialog show them | `scripts/check_apps.py` |
| Do write a `CHANGELOG.md` entry for every version in the manifest | people and the store read what changed | `scripts/check_apps.py` |
| Don't let a `requires` range stop below a sibling's new release | the platform cannot pin the two together | the server's `compiled_apps_fit_together` (`server/src/packages.rs`) |
| Do give a launcher entry an icon the bar draws | otherwise the launcher has nothing to show | `scripts/check_apps.py` against `server/shell/runtime.js` |
| Don't take a reserved route (`api assistant ws files mcp metrics oauth open data admin store account portal public accounts login logout s app home sw.js robots.txt favicon.ico manifest.webmanifest`, `_…`, `.…`) or another app's | the core and other apps own them | refused when a release is made available (`docs/plugins.md`) |
| Do raise `platform` to the version that has what you use (0.3.9 for operator settings) | an older platform would not read it | `scripts/check_apps.py` (operator settings) |

## Data, tables and migrations

| Do / don't | Why | Caught by |
|---|---|---|
| Do model records as entities of a package, so lists, forms, permissions, audit, events, import, export and the API come with them | one behaviour in every app (`docs/app-contract.md`, *Reviewed*) | reviewed |
| Do give every table of your own a `tenant_id`, row-level security enabled and forced, and the `tenant_isolation` policy (`tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())`) | a transaction sees one workspace only (`docs/security.md`, *Model*); Drive's `packages/drive/sql/001_drive.sql` is the pattern | the app's suite run as a non-superuser role (`scripts/app_ci.sh`); reviewed |
| Do prefix your own tables and indexes with the app's name (`drive_secret`, `drive_file_live_name`) | no clash with the platform or another app | reviewed |
| Don't write an entity table (`e_*`) with raw SQL outside the document pipeline unless the review lists it | it skips permissions, rules, audit and events | `scripts/mutation_paths.py` fails on a statement `docs/registry/mutation-paths.md` does not list |
| Do keep migrations additive (new tables and columns); make a destructive change an explicit step | an older build may still run against the database during an upgrade (`docs/plan/stateless.md`, rule 7) | reviewed; `scripts/window_rehearsal.sh` for the platform |
| Don't need a superuser, `BYPASSRLS`, `CREATE EXTENSION` or session state in a migration | the server runs as an ordinary role on any PostgreSQL | the server refuses to start as a superuser; `scripts/local_ci.sh managed` for the platform |
| Do keep money and decimals as strings and decimals in the database | no float rounding | reviewed |
| Do give a unique index a sentence with `set_constraint_message` | otherwise the person reads "duplicate value (`<index>`)" | reviewed |

## Permissions and people

| Do / don't | Why | Caught by |
|---|---|---|
| Do give roles for the jobs people do, each allowed what that job needs | least privilege (`docs/app-contract.md`, *Reviewed*) | reviewed |
| Do put sensitive fields in a field group, masked where a person must know there is one | bank details and identity numbers | reviewed |
| Do answer "not found" the same for "does not exist" and "you may not know it exists" | an address must not reveal what exists | reviewed; the app's tests |
| Don't trust the interface to hide what a person may not do | the API is the gate (`docs/plan/apps-ui.md` §4 rule 5) | reviewed |
| Do check every route's rights in the route | a route is reachable by anyone who knows it | the app's tests; reviewed |
| Do use the platform's portal and its people for outsiders | they see only what their address belongs to | reviewed |
| Do ask for step-up before sensitive work through the platform | the client and kit handle `403 step_up` | reviewed |

## Events, jobs and logic

| Do / don't | Why | Caught by |
|---|---|---|
| Do make every event handler idempotent | delivery is at least once (`docs/events.md`) | reviewed; the app's tests |
| Do compare `source_version` when order matters | retries and upgrades reorder events | reviewed |
| Don't call anything outside from a document hook | it holds the document's transaction (`docs/events.md`, *What may happen where*); plugins are refused `http.fetch` there | plugins: refused; native hooks: reviewed |
| Don't hold a transaction open across a provider or a person | it locks rows and outlives timeouts | reviewed |
| Do use a saga or a job for work that crosses systems, with an idempotency key per step | a step runs at least once | reviewed |
| Do run bulk work at bulk priority (`Trace::at_priority(PRIORITY_BULK)`) | a campaign must not stand in front of a password reset | reviewed |
| Do treat an unknown outcome as an answer (`outcome_unknown`), reconciled, never retried blindly | retrying it is how a thing gets done twice | reviewed |
| Do name job kinds `{app}.{what}` | one namespace for all apps (`HookRegistry::add_job_handler`) | reviewed |

## Stateless apps and settings

| Do / don't | Why | Caught by |
|---|---|---|
| Do start from the database address alone; read operator settings with `ferrum_kernel::env::var` | the table's value never reaches `std::env::var` | `scripts/check_apps.py` fails a declared variable read with `std::env::var`; `scripts/settings.py`; `scripts/one_input.sh` |
| Do declare every operator setting in `operator_settings` with key, env, type and doc | one registry, one owner per key | `scripts/check_apps.py`; the server refuses two owners |
| Don't put a secret in an operator setting | settings are not sealed | `scripts/check_apps.py` (no secret field); the registry refuses a variable it holds as a secret |
| Don't keep anything on local disk that you cannot rebuild | replicas come and go; two stores only (`docs/plan/stateless.md`, rule 2) | reviewed; `scripts/one_input.sh` |
| Don't rely on session state on a pooled connection (a `SET` outside a transaction, a session-level advisory lock, `LISTEN`); use transaction-local settings | a pooler in transaction mode is supported (`docs/plan/stateless.md`, rule 6) | `scripts/local_ci.sh managed` for the platform; reviewed for apps |
| Do assume several replicas at once: locks, leases, work done exactly once by the database | two replicas run every loop | reviewed |
| Do say in plain words why the app refuses to start | fail closed, say why (`docs/plan/stateless.md`, rule 8) | reviewed |

## Secrets and the vault

| Do / don't | Why | Caught by |
|---|---|---|
| Do take the vault handle inside your registration (`h.vault()`) and keep it | a handle asked for elsewhere is accepted by nothing and the server does not start | the server |
| Don't call the vault's ways that are not your handle, or name its tables | an owner opens only its own | `scripts/secret_paths.py` |
| Do read your secret fields with `Vault::field` | an app reads only the fields its own packages declare | the vault refuses another's |
| Don't add a cryptography crate of your own (one that keys, seals or signs) or `unsafe` code | cryptography only through the vault | `scripts/secret_paths.py` |
| Do bind a secret to the hosts it may go to and send it with `carry` | moving an address never moves the secret | the vault refuses other hosts |
| Do set a secret after the business transaction commits | a verb runs in its own transaction | reviewed |
| Don't put a secret in a log, an error, an event, a test's output, the environment or a setting | they are copied and kept | `scripts/settings.py` for the environment; the rest reviewed |

## Files and storage

| Do / don't | Why | Caught by |
|---|---|---|
| Do keep files in the platform's file store, tied to records, or as a file kind of your own (`h.file_kind("backup")`) | counted, scanned, moved and deleted as every file | reviewed |
| Don't keep a store, a bucket or keys of your own, or a path on disk | the platform owns storage and its keys | reviewed |
| Do answer with a stored file through `SiteResponse::file` | the server streams it; nothing large sits in memory | reviewed |
| Do give your customer a way to download what a file kind keeps | the workspace's export leaves file kinds out | reviewed |

## Companions and sign-in

| Do / don't | Why | Caught by |
|---|---|---|
| Do declare a program you run beside the server with `h.add_companion`, its files rendered from the database and the vault | one input; no secret in the environment | the server refuses two owners; `scripts/companions_rehearsal.sh` for the platform |
| Do sign people in to it through `h.openid_companion` | the workspace's people and rights | the server refuses a claim the platform sets |
| Do end the companion's session on `ferrum.companion.signed_out` | Ferrum stops answering, the companion's own session is yours to end | reviewed |

## Integrations

| Do / don't | Why | Caught by |
|---|---|---|
| Do call outside systems through connections and the kernel's outbound client: https, pinned hosts, no private addresses, no redirects, time and size limits | server-side request forgery and runaway calls (`docs/security.md`, *Input and output*) | refused by the outbound client; reviewed |
| Do check a webhook's signature with the vault and count it against a limit before any lookup | public routes are anyone's | reviewed |
| Do send mail and messages through the platform's messages and templates | language, opt-outs and the outbox | reviewed |
| Do ask `Kernel::may_contact` before an app delivers on its own | opt-outs hold for every app (`docs/limits.md`) | reviewed |
| Do `requires` an app whose records you read, with a range | the server checks compiled apps fit together | `compiled_apps_fit_together` |

## Privacy and data duties

| Do / don't | Why | Caught by |
|---|---|---|
| Do declare personal data (`personal_data` in a package) and tables of your own that hold it (`add_personal_held`) | erasure and export must reach it | reviewed |
| Do name in `export_tables` the engine tables a workspace export carries, and keep secrets out of them | other tables are listed as left out (`crates/meta/src/types.rs`) | reviewed |
| Do declare `handover` for what a leaver's successor takes over | deprovisioning hands it over | reviewed |
| Do mark fields `no_export` where they must never leave in a list export | the platform leaves them out for everyone | the platform |
| Don't send personal or secret values in events, webhooks or logs | they are copied and kept | reviewed |

## Security

| Do / don't | Why | Caught by |
|---|---|---|
| Do fail closed: when a check cannot be made, refuse | a check that errors open is no check | reviewed |
| Do bind every SQL value; take identifiers only from metadata, quoted | injection | reviewed |
| Do show rich text through `RichText` and save it through `sanitizeRichText`; the server cleans it too (`crates/kernel/src/richtext.rs`) | stored scripts | reviewed |
| Don't set HTML yourself (`dangerouslySetInnerHTML`) from data | stored scripts | reviewed |
| Do run strangers' code only in the plugin sandbox (WebAssembly with fuel and a memory cap) | no ambient access | the plugin host |
| Do keep dependencies few, maintained and under accepted licences | supply chain and licence | `scripts/licenses.py check`, `cargo audit` in `scripts/local_ci.sh audit` |
| Don't weaken an iframe sandbox or CSP for an editor, or put customer CSS over warnings or the bar | it would let content act as the workspace | reviewed |

## Operations

| Do / don't | Why | Caught by |
|---|---|---|
| Do keep statements short and indexed; batch large work | statements time out at 15 s by default | the database |
| Do add a health line for the support report (`add_health`) where your part can be unwell | operators see it | reviewed |
| Do say in the README and CHANGELOG what an operator must set, run or watch | they run it, not you | reviewed |
| Do keep an upgrade working for records made before it | upgrades keep what was made (`docs/app-contract.md`, *Reviewed*) | reviewed; the app's tests |

## Testing and CI

| Do / don't | Why | Caught by |
|---|---|---|
| Do test against a real PostgreSQL as a non-superuser role | superusers bypass row-level security | `scripts/app_ci.sh` |
| Do boot the app's suite from `ferrum-testkit` (`Kit::boot`, each app an `app!` entry) | it registers each app by its id with the packages it owns and opens the vault as the server does; otherwise the vault refuses or the test proves less | the kit |
| Do write the test that fails first, and keep a mutant for each invariant that matters | a test that stays green with the invariant gone proves nothing | `scripts/mutants.py` |
| Do run checks on your own machine (`scripts/app_ci.sh <checkout>`), as the workflow would | checks run on the developer's machine, not on GitHub (the owner's standing rule) | reviewed |
| Do use real software where it matters and say what was never run | a stand-in proves the stand-in | reviewed |

## Releasing

| Do / don't | Why | Caught by |
|---|---|---|
| Do bump the version, write the CHANGELOG, build and commit `ui/dist` | the platform pins what you release | `scripts/check_apps.py`, `cli.mjs check` |
| Do widen a `requires` range when a sibling releases | otherwise the pair cannot be pinned | `compiled_apps_fit_together` |
| Do walk the flows people use on a running server before a release that changes them, and record what you found | the contract's review | reviewed |

## The interface

| Do / don't | Why | Caught by |
|---|---|---|
| Do use `@ferrum/kit` and `@ferrum/kit/primitives`; add a missing pattern to the kit | one look; a fix reaches every app | lint `forked-primitive` |
| Don't import `radix-ui`, `@radix-ui/*` or `class-variance-authority` in an app | forks drift | lint `forked-primitive` |
| Don't import Ferrum-Frontend's own modules (`@/…`) | apps use the packages only | the build fails |
| Don't write colour literals; use tokens | dark mode, contrast, brand accents | lint `raw-color` |
| Don't write `text-[Npx]` or text under 12px | type roles; readable text | lint `arbitrary-text-size`, `small-text` |
| Don't use `fallback={null}` | the page jumps and loses its place | lint `blank-fallback` |
| Don't block zoom | people need it | lint `zoom-lock` |
| Do start `styles.css` with the kit's `index.css` | the page ships unstyled otherwise | `cli.mjs check` |
| Do build and commit `ui/dist` with the sources | the crate embeds it | `cli.mjs check`, `scripts/check_apps.py` |
| Do keep the entry under 200 KB compressed and CSS under 50 KB; load pages when opened | first load on a phone | `cli.mjs check` |
| Do keep loading, empty, no matches, denied, offline, failed and stale apart | each needs different words and actions | reviewed |
| Don't show "saved", "sent" or "paid" before the server confirmed | a lie when the request fails | reviewed |
| Do take status tone from the stored id (`StatusBadge value=`) | labels are translated | reviewed |
| Do give every left-menu row an icon (entity `icon:`, module `icon:`, `shell.menu` `icon:`, or the kit's concept dictionary) | the owner's rule; a test in the kit holds the installed apps' entities | `navIconName.test.tsx` (Ferrum-Frontend) |
| Do read routes from the bootstrap (`routeBase`) | an operator may remap them | reviewed |
| Do call `shell().ready()` once drawn | the runtime reports an app that never does | the runtime |
| Do fit 375px and hold in dark mode | the contract | reviewed |

## Accessibility

| Do / don't | Why | Caught by |
|---|---|---|
| Do label every field (`Field`), name every icon button, keep visible words in the name | screen readers and voice control | reviewed |
| Do make every action reachable by keyboard, keep focus visible and unobscured, return focus after a dialog | keyboard users | the kit for its components; the rest reviewed |
| Do give every drag another way, and let single-key shortcuts be turned off | WCAG 2.5.7, 2.1.4 | reviewed |
| Don't use colour alone for a state | colour-blind and high-contrast users | reviewed |
| Do announce outcomes (`announce`, `SaveStatus`, `StatePanel`) | status messages (4.1.3) | reviewed |
| Don't claim "accessible" or "WCAG compliant": no assistive-technology audit has been done | it would be untrue | reviewed |

## Languages and formats

| Do / don't | Why | Caught by |
|---|---|---|
| Do send every word through `t("…")` with a double-quoted literal | the collector reads only that form | `cli.mjs check` (catalog entries must be words the sources ask for) |
| Don't call `t()` at a module's top level | it runs before the catalogs load | reviewed |
| Don't concatenate pieces of sentences, or build plurals in code | word order and plural forms differ | reviewed |
| Do pass numbers to `t()` already formatted (`formatDecimal`) | `t()` writes numbers with `String()` | reviewed |
| Do show moments with `formatMoment`, dates with `formatDate`; never `toLocaleString()` | one clock for every member | reviewed |
| Don't do arithmetic on money in the browser | totals come from the server or an engine in WebAssembly | reviewed |
| Do use logical CSS properties | right to left | reviewed |

## Words

| Do / don't | Why | Caught by |
|---|---|---|
| Do say what happened and what to do next, in plain words | people act on it | reviewed |
| Don't put a secret, SQL, a path, a stack trace or someone else's data in an error | leaks | reviewed; internal errors are replaced by the server |
| Do confirm what cannot be undone, and offer Undo for at least 8 seconds for what can | mistakes happen | reviewed |
| Do say in docs and READMEs what was run and what was not | readers decide on it | reviewed |

## APIs and docs

| Do / don't | Why | Caught by |
|---|---|---|
| Do use the records API and custom actions before writing a route | permissions, audit, events, idempotency built in | reviewed |
| Do answer with a stable `error` code; never make clients read the message | messages change and are for people | reviewed |
| Do page every list with a cursor and a cap | unbounded answers | reviewed |
| Do take an `Idempotency-Key` on creates and effects; refuse a key reused for another request | retries must be safe | reviewed |
| Do count your own limits with `Kernel::rate_limit` | holds across replicas | reviewed |
| Do only add within a version; deprecate with a stated removal version | callers keep working | reviewed |
| Do document routes in the repository's `docs/`, generated from the route table where you can | docs that drift are worse than none | Cloud's `scripts/api_docs.py --check`; otherwise reviewed |

## Checked by

The scripts named above, all in the platform's `scripts/` unless said: `check_apps.py`, `settings.py`,
`secret_paths.py`, `licenses.py`, `mutation_paths.py`, `mutants.py`, `app_ci.sh`, `local_ci.sh`, `one_input.sh`; the
server's own refusals at start; Ferrum-Frontend's `packages/build/cli.mjs check` and `uilint.mjs`, and its kit tests.
Every line marked "reviewed" has no check.

## See also

[README.md](README.md) (the rules at a glance) and [24-review-checklist.md](24-review-checklist.md); each area's guide:
[01](01-what-an-app-is.md), [03](03-the-manifest.md), [04](04-data-and-records.md),
[05](05-permissions-and-people.md), [06](06-events-jobs-and-logic.md), [07](07-stateless-apps.md),
[08](08-secrets-and-the-vault.md), [09](09-files-and-storage.md), [10](10-companions-and-sign-in.md),
[11](11-integrations.md), [12](12-privacy-and-data-duties.md), [13](13-security-rules.md),
[14](14-operations-for-apps.md), [15](15-testing-and-ci.md), [16](16-releasing.md), [17](17-the-interface.md),
[18](18-accessibility.md), [19](19-languages-and-formats.md), [20](20-writing-for-people.md),
[21](21-apis-and-docs.md), [22](22-readme-and-repository.md).
