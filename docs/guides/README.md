# The app developer's handbook

How to build an app for Ferrum, and the rules an app keeps. For anyone who writes one: a first-party app in a
`ferrum-app-<name>` repository, an add-on such as a country's rules, a plugin, an app with a server of its own; and
for the platform's maintainers who review an app before pinning it.

The normative texts stay where they are: [docs/app-contract.md](../app-contract.md) is what every app gives,
[docs/plan/apps.md](../plan/apps.md) the format and the runtimes, [docs/plugins.md](../plugins.md) the plugin
contract, extension points and interfaces, [docs/api.md](../api.md) the HTTP API, [docs/events.md](../events.md)
events, [docs/security.md](../security.md) and [docs/operations.md](../operations.md) security and operations. Where
a guide and one of those say the same, that document is the rule and the guide is how to follow it. Every guide names
the file, script or test it was read from; when a guide and the code differ, the code is right and the guide is a bug.

## How to read it

- **New to Ferrum**: 01, then 02 with a terminal open, then 03 to 06 as you need them.
- **Building a screen**: 17, 18 and 19, with the Frontend's `docs/design/` beside them.
- **Before asking for a pin**: 24, then the table below.
- **Looking up a word**: 25.

Each guide ends with *Checked by* (the scripts and tests that hold an app to it, or "reviewed, not checked" where
nothing does) and *See also*. Each says plainly what is not built, not checked, or never run.

## The guides

| Guide | What it covers |
|---|---|
| [01 What an app is](01-what-an-app-is.md) | the kinds (native, add-on, connector, suite, extension build, plugin, service), one repository per app, layout, naming, licence, what belongs in the platform, engines and content |
| [02 Your first app](02-your-first-app.md) | from an empty repository to a native app the platform builds, modelled on GRC: every file, every command, the checks, a development server, putting it in the platform |
| [03 The manifest](03-the-manifest.md) | `app.yaml` field by field, each with its refusal; versions, `platform` and `requires`, and how ranges go wrong |
| [04 Data and records](04-data-and-records.md) | packages, entities, fields, lines, links, overlays, custom fields, numbering, rules, validation, actions, transactions, direct writes and their review, an app's own tables, search, limits |
| [05 Permissions and people](05-permissions-and-people.md) | roles and rules, not found versus refused, how an app checks, zones, portals, sharing, approvals and the second person, step-up, the access policy, support access, the audit trail |
| [06 Events, jobs and logic](06-events-jobs-and-logic.md) | nothing leaves from a transaction, the outbox, event sinks and first-time checks, jobs, upkeep, sagas, unknown outcomes, idempotency, capabilities and contracts for processes, the builder, the assistant and MCP |
| [07 Stateless apps](07-stateless-apps.md) | the database address as the one input, operator settings against a workspace's settings, no local disk, any PostgreSQL, several replicas, readiness |
| [08 Secrets and the vault](08-secrets-and-the-vault.md) | an app's vault handle, scopes and verbs, managed secrets, secret fields, signing and sealing, carrying a secret to a host, rotation, what never to do |
| [09 Files and storage](09-files-and-storage.md) | the file seam, attach fields, an app's own kinds of file, scanning, quotas, the deletion lag, links |
| [10 Companions and sign-in](10-companions-and-sign-in.md) | programs an app runs beside the server, the files a render writes, signing people in to them through Ferrum |
| [11 Integrations](11-integrations.md) | calling other systems, webhooks in, mail and messages, connectors, other apps' data, countries and sectors as add-ons |
| [12 Privacy and data duties](12-privacy-and-data-duties.md) | erasure, export, retention, moves, sandboxes, deletion; personal data fields, opt-outs, regions |
| [13 Security rules](13-security-rules.md) | fail closed, tenant isolation, input at trust boundaries, rate limits, cryptography through the vault, dependencies and licences, unsafe content, what goes to review |
| [14 Operations for apps](14-operations-for-apps.md) | what an operator sees, limits and performance, upgrades and migrations, rolling restarts, observability, what to document |
| [15 Testing and CI](15-testing-and-ci.md) | an app's suite on a real PostgreSQL, tests that fail first, mutants, real software instead of stand-ins, `scripts/app_ci.sh`, what each check script checks |
| [16 Releasing](16-releasing.md) | versions and the CHANGELOG, ranges, how the platform pins an app, the store listing, paid apps, the release checklist |
| [17 The interface](17-the-interface.md) | each app's own interface, the build tool and the shell, the kit, tokens and patterns, states, phone widths, dark mode, live updates, offline, the UI lint |
| [18 Accessibility](18-accessibility.md) | WCAG 2.2 AA with the kit: keyboard, focus, names, forms, contrast, motion, zoom, tables, dialogs, live regions, right-to-left; how to test and record it |
| [19 Languages and formats](19-languages-and-formats.md) | every word through `t()`, plurals and variables, right-to-left, dates, times and zones, numbers, money, units, names and addresses |
| [20 Writing for people](20-writing-for-people.md) | the words on screens and in API answers, refusals, errors, empty states, confirmations, docs that say what ran |
| [21 APIs and docs](21-apis-and-docs.md) | an app's routes, errors with stable codes, paging, idempotency keys, rate limits, versioning, OpenAPI |
| [22 README and repository](22-readme-and-repository.md) | what every repository holds, the README and its sections, the toolkit, what a README must never claim |
| [23 Dos and don'ts](23-dos-and-donts.md) | the complete list by area, each with why and what catches it |
| [24 Review checklist](24-review-checklist.md) | what a developer walks before asking for a pin, and a maintainer before pinning |
| [25 Glossary](25-glossary.md) | every term of the platform a developer meets |

## The rules at a glance

Every rule an app keeps, the guide that explains it, and what enforces it. "Reviewed" means no program checks it:
a person does, at review. Scripts run on the developer's machine (`scripts/local_ci.sh` for the platform,
`scripts/app_ci.sh` for an app); GitHub's workflows are switched off.

### What an app is

| Rule | Guide | Enforced by |
|---|---|---|
| A repository of its own, `ferrum-app-<name>`; never in the platform's `crates/` | [01](01-what-an-app-is.md) | `scripts/check_apps.py` checks only `ferrum-app-*` sources; an app inside the platform's workspace is reviewed (Operations and Studio are exceptions from before the rule) |
| A licence Ferrum may ship (AGPL-3.0 or compatible) | [01](01-what-an-app-is.md), [13](13-security-rules.md) | `scripts/check_apps.py` (`LICENSES`); its dependencies `scripts/licenses.py` |
| A manifest with id, name, version, publisher, summary, categories, licence, homepage, support | [03](03-the-manifest.md) | `AppManifest::parse` (`crates/apps/src/manifest.rs`); `scripts/check_apps.py` |
| No field the server does not know | [03](03-the-manifest.md) | `AppManifest::parse` (`deny_unknown_fields`) |
| An add-on names its `parent` in `requires` | [01](01-what-an-app-is.md), [03](03-the-manifest.md) | `AppManifest::validate` |
| `requires` ranges meet the releases compiled beside them | [03](03-the-manifest.md), [16](16-releasing.md) | `compiled_apps_fit_together` (`server/src/packages.rs`) |
| `platform` covers the app API it uses (0.3.9 for the vault and operator settings) | [03](03-the-manifest.md) | `scripts/check_apps.py` for operator settings; the App Manager (`blocked`) at install; otherwise reviewed |
| Engines general; a country's or a sector's rules are data or an add-on | [01](01-what-an-app-is.md), [11](11-integrations.md) | reviewed |
| Look for a mature open-source project to run, depend on or adapt first (AGPL-compatible only) | [01](01-what-an-app-is.md), [13](13-security-rules.md) | reviewed; `scripts/licenses.py` for what is compiled in |

### Data and records

| Rule | Guide | Enforced by |
|---|---|---|
| A package reaches only its dependencies' entities | [04](04-data-and-records.md) | the catalog (`crates/meta/src/catalog.rs`) |
| No field named as a platform column; custom fields `x_`, custom entities `X` are the workspace's | [04](04-data-and-records.md) | `RESERVED_FIELDS`, `crates/meta/src/effective.rs` |
| Records change through the document pipeline; each direct write to `e_*` is listed and given a kind | [04](04-data-and-records.md) | `scripts/mutation_paths.py`, [docs/registry/mutation-paths.md](../registry/mutation-paths.md) |
| An app's own tables carry `tenant_id` and force row-level security | [04](04-data-and-records.md), [13](13-security-rules.md) | `ferrum task doctor` (`isolation.row_security`, a blocker) |
| Package SQL is never edited once applied, and only adds | [04](04-data-and-records.md), [14](14-operations-for-apps.md) | `crates/kernel/src/ddl.rs` refuses an edited file; *only adds* is reviewed for apps (`scripts/window.py` reads the platform's repository only) |
| A release's data change is a backfill in batches, not a long migration | [04](04-data-and-records.md), [14](14-operations-for-apps.md) | reviewed |
| Money in decimal kinds, never floats | [04](04-data-and-records.md), [19](19-languages-and-formats.md) | reviewed |

### Permissions and people

| Rule | Guide | Enforced by |
|---|---|---|
| Roles for the jobs people do; sensitive fields in field groups | [05](05-permissions-and-people.md) | reviewed ([docs/app-contract.md](../app-contract.md)) |
| Never say whether a record the caller may not read exists: `404`, not `403` | [05](05-permissions-and-people.md), [13](13-security-rules.md) | the kernel for its own calls (`row_scope`, `get_tx`); an app's own endpoints reviewed, and its scenario test |
| An app's endpoints check rights through the kernel or `Ctx::require` | [05](05-permissions-and-people.md) | reviewed; the scenario test |
| Reports gated on the app's own entities | [05](05-permissions-and-people.md) | `every_report_is_gated_on_its_own_apps_entity` (`server/src/packages.rs`) |
| Portal roles reach only their holders' own records | [05](05-permissions-and-people.md) | `PermissionDef::portal_problem`, when the rule is saved |
| A second person where the work needs one | [05](05-permissions-and-people.md) | the kernel (`not_creator`, approval policies); an app's own rule by its scenario test |

### Events, jobs and logic

| Rule | Guide | Enforced by |
|---|---|---|
| Nothing leaves from inside a transaction | [06](06-events-jobs-and-logic.md) | plugins: refused by the host; native hooks: reviewed |
| Every handler is safe to run twice | [06](06-events-jobs-and-logic.md) | reviewed; `first_time` is the tool; the scenario test |
| Work that outlives a request is a job; upkeep goes workspace by workspace | [06](06-events-jobs-and-logic.md) | reviewed |
| Order compared by `source_version`, never arrival | [06](06-events-jobs-and-logic.md) | reviewed |
| An action's contract says what it does to the world | [06](06-events-jobs-and-logic.md) | the package load refuses a contradicting contract; the rest reviewed |

### Stateless, secrets, files, companions

| Rule | Guide | Enforced by |
|---|---|---|
| A replica starts from the database address alone | [07](07-stateless-apps.md) | `scripts/one_input.sh`; `scripts/settings.py` |
| Operator settings read with `ferrum_kernel::env::var`, never `std::env::var`; one owner each | [07](07-stateless-apps.md), [03](03-the-manifest.md) | `scripts/check_apps.py`; `scripts/settings.py`; the server refuses two owners |
| Nothing on local disk; files in the platform's file store | [07](07-stateless-apps.md), [09](09-files-and-storage.md) | reviewed; `scripts/object_store_rehearsal.sh` for the platform |
| Any PostgreSQL: an ordinary role, a pooler in transaction mode | [07](07-stateless-apps.md) | `scripts/app_ci.sh` (non-superuser tests); `scripts/managed_pg.sh` for the platform |
| Secrets only in the vault, through the app's own handle; never in a setting, a log, an error, a test's output | [08](08-secrets-and-the-vault.md) | `scripts/secret_paths.py`; the manifest has no field for one; the server will not start on a handle taken outside the registration |
| No cryptography crate of the app's own, no `unsafe` | [08](08-secrets-and-the-vault.md), [13](13-security-rules.md) | `scripts/secret_paths.py` (with a baseline that may only shrink) |
| An app's own kinds of file declared once, reached only through its handle | [09](09-files-and-storage.md) | `HookRegistry::file_kind` (the server stops on a second declaration) |
| A companion's sign-in and service declared inside the app's registration, once | [10](10-companions-and-sign-in.md) | `HookRegistry::settle` stops the server; `scripts/oidc_rehearsal.sh`, `scripts/companions_rehearsal.sh` |

### Integrations, privacy, security, operations

| Rule | Guide | Enforced by |
|---|---|---|
| Outside calls through connections and vetted clients, to declared hosts, with timeouts | [11](11-integrations.md) | the kernel's outbound client and the plugin host; reviewed for native code |
| Another app's records through the kernel, never its tables | [11](11-integrations.md) | reviewed; zones refuse other apps' endpoints |
| Personal data declared (`personal_data`, `handover`) so erasure and export reach it | [12](12-privacy-and-data-duties.md) | the catalog checks what is declared; reviewed for what is not |
| Fail closed; input checked at every trust boundary; rate limits on public routes | [13](13-security-rules.md) | the kernel and server for their part; reviewed for the app's |
| No raw HTML from a description or a person's text | [13](13-security-rules.md), [22](22-readme-and-repository.md) | `crates/apps/src/description.rs`; `crates/kernel/src/richtext.rs` |
| Dependencies under accepted licences | [13](13-security-rules.md), [16](16-releasing.md) | `scripts/licenses.py` |
| What an operator needs is documented, and the doctor can see it | [14](14-operations-for-apps.md) | `ferrum task doctor`; reviewed |

### Tests, releases, interface, words

| Rule | Guide | Enforced by |
|---|---|---|
| A scenario test (`tests/*_it.rs`) against a real PostgreSQL, as a non-superuser | [15](15-testing-and-ci.md) | `scripts/check_apps.py`; `scripts/app_ci.sh` |
| Formatting, clippy with warnings as errors, the tests, the interface's checks pass on the developer's machine | [15](15-testing-and-ci.md) | `scripts/app_ci.sh` |
| Tests that fail when the invariant goes | [15](15-testing-and-ci.md) | `scripts/mutants.py` for the platform; reviewed for apps |
| What was never run is said, never claimed | [15](15-testing-and-ci.md), [20](20-writing-for-people.md), [22](22-readme-and-repository.md) | reviewed; `scripts/registry.py` for the platform's registry |
| A CHANGELOG entry for every version | [16](16-releasing.md) | `scripts/check_apps.py` |
| No attribution lines in commits | [16](16-releasing.md) | reviewed |
| `ui/dist` built from the sources and committed, with `.source` | [17](17-the-interface.md) | `cli.mjs check` (Frontend `packages/build`); `scripts/check_apps.py` |
| Size budgets: entry script 200 KB compressed, CSS 50 KB | [17](17-the-interface.md) | `cli.mjs check` |
| Kit tokens and type roles; no raw colours, no text under 12px, no zoom locks | [17](17-the-interface.md), [18](18-accessibility.md) | the UI lint in `cli.mjs check`, against `ui-lint-baseline.json` |
| An icon on every launcher entry and left-menu row, one the shell draws | [17](17-the-interface.md), [03](03-the-manifest.md) | `scripts/check_apps.py`, `compiled_apps_fit_together` (launcher); menu rows reviewed |
| Phone width, dark mode, labels on fields; a walk on a running server before a release that changes flows | [17](17-the-interface.md), [18](18-accessibility.md) | reviewed ([docs/app-contract.md](../app-contract.md)); for an uploaded bundle, the install dialog's automatic look (lang, title, viewport, `alt`) |
| WCAG 2.2 AA | [18](18-accessibility.md) | partly the lint; otherwise reviewed. No app has been audited with assistive technology |
| Every word through `t()`; catalogs hold only words asked for | [19](19-languages-and-formats.md) | `cli.mjs check` (catalogs); `t()` itself reviewed |
| Words that say what to do next, never blame or leak | [20](20-writing-for-people.md) | reviewed |
| Errors with stable codes, paging, idempotency keys on an app's API | [21](21-apis-and-docs.md) | reviewed |
| A README with the toolkit's sections that points at the CHANGELOG and claims nothing it did not run | [22](22-readme-and-repository.md) | `scripts/check_apps.py` (description, CHANGELOG pointer); the rest reviewed |

[23 Dos and don'ts](23-dos-and-donts.md) has every rule as a line with why; [24 Review
checklist](24-review-checklist.md) is the list walked before a pin.

## What this handbook does not cover

- The platform's own internals beyond what an app touches: [docs/plan/architecture.md](../plan/architecture.md).
- Running an installation: [docs/operations.md](../operations.md).
- The store's service and the `ferrum-app` command: the `ferrum-store` and `ferrum-sdk` repositories.
