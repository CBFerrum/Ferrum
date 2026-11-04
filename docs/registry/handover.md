# Handover

Where the work stands, kept here rather than in anyone's memory (specification §19.5). Newest first.
What is true of each requirement is in the registry; this says what happened and what comes next.

## 2026-11-04, evening: sign-up and plans, an industrial solution through to commissioning, meetings' captions and recordings, a hotel, and a browser on a running server

**State:** each PR was green on its CI before it merged. No release was tagged.

| Where | What |
|---|---|
| Ferrum PR 53 | Workspace sign-up at `accounts.{base}/signup`, the operator's plans (`FERRUM_PLANS`, `set_plan`), the people quota held by a trigger, the assistant held to the plan's tokens, and closing one's own workspace (FERRUM-CLOUD-002, JOURNEY-012 in parts) |
| Ferrum PR 51 | An industrial solution in an app's bundle, imported into Operations on install as device profiles and acceptance procedures; commissioning waits for a passed run of each; the install plan lists the equipment (FERRUM-IND-018) |
| Comms PR 5 | Comms 0.9.0: captions each person gives their own voice, kept as the meeting's transcript; recordings through LiveKit Egress (FERRUM-COMMS-002) |
| Ferrum-Frontend PRs 13, 14 | The workspace's plan, usage and closing in Administration; the equipment in the install dialog |
| New: ferrum-app-hospitality | Hospitality 0.1.0: properties, room types and rooms, rate plans by season, stays, folios and the night audit (FERRUM-INDUSTRY-001, JOURNEY-010 in parts), adapted from ERPNext 12's Hotels module |
| New: ferrum-app-nonprofit | Nonprofit 0.1.0: memberships invoiced through ERP and counted once paid, donations posted to the books, funds, grants and volunteers (FERRUM-INDUSTRY-011), adapted from ERPNext 13's Non Profit module |
| New: ferrum-app-education | Education 0.1.0: admissions into students and enrollments, fees through ERP, a timetable without clashes, assessments and attendance (FERRUM-INDUSTRY-004), adapted from ERPNext 13's Education module |
| Ferrum PR 54 | Comms 0.9.0, Hospitality, Nonprofit and Education compiled in; the operations guide's recordings; entity files may start with a comment |

**Evidence:**
- Mutants were run and all caught: 11 for sign-up and plans, 8 for acceptance (one added after the browser run), 9 for Comms by hand, 10 for Hospitality by hand, 9 for Nonprofit and 11 for Education by hand. Hospitality's one survivor was a check nothing could reach; it was removed and the guard that matters was tested instead.
- Kernel, Operations, Comms and Hospitality tests ran on this machine. The server was checked with clippy with and without Operations.
- **A browser on a running server, for the first time** (`docs/platform/evidence/browser-2026-11-04.md`): from sign-up to closing, and an industrial solution from upload to commissioning. It found a real bug that no test had: an installation that brings Operations imported with the request's stale schema. PR 51 fixes it, with a test and a mutant.

**Not done:**
- Real LiveKit Egress, real speech recognition, or a real call.
- Equipment, and a channel manager for the hotel.
- Personal-data erasure of meeting transcripts.
- The other industry packs, the ecosystem apps, store monetization, the control plane, the fleet and the partner console. The registry lists them.

**Lessons:**
- A browser run finds what scenario tests miss. This one found a schema that went stale within one request, a panel that did not refresh, and a word in the singular.
- An entity file that began with a comment was read as one entity (fixed in PR 54).
- sqlx 0.9 refuses SQL built at runtime unless it is marked `AssertSqlSafe`.
- A binary in `target/debug` is a hard link to `deps/`: delete both to free the space. Old test binaries in the shared target held 1.6 GB.

## 2026-11-04, later: help and assistants in the shell, payments for subscriptions, deposits and courses, and Learn's SCORM, xAPI and LTI

**State**: on `main` of each repository; each PR was green on its CI before it merged. No release was tagged.

| Where | What |
|---|---|
| Ferrum PR 49 | Help in the account menu, opening the help center Ferrum-Frontend PR 11 added at `/data/help` (FERRUM-UX-003). Per-app assistants (FERRUM-AI-006): an app's `shell.assistant` guidance joins the context on its pages, `/api/assistant/suggestions` lists its questions, and the panel shows the records an answer read. The connector catalog is `blocked_external` on providers' sandboxes (FERRUM-CONNECT-002) |
| Commerce PRs 1, 2 | Commerce 0.6.0: subscriptions sold in the store (FERRUM-COMMERCE-002). Commerce 0.7.0: part of an invoice asked for, and part of a payment refunded, for other apps |
| Bookings PR 1 | Bookings 0.3.0: Booking Deposits, a connector with Commerce (FERRUM-BOOKINGS-003) |
| ERP PR 5 | ERP 0.7.1: ERP's assistant guidance and suggested questions |
| Learn PRs 1, 2, 3 | Learn 0.3.0, Paid Courses (a connector with Commerce): bought, invoiced, enrolled on payment, refunded within 14 days. Learn 0.4.0: SCORM 1.2 and 2004 lessons through scorm-again 3.4.6 (MIT, vendored with its provenance), and xAPI lessons with an endpoint of their own. Learn 0.5.0: LTI 1.3 tools, launched by OIDC with an id_token signed with Learn's key, sending grades back by AGS (FERRUM-LMS-002) |
| Ferrum PR 50 | Pins ERP 0.7.1 and Learn 0.5.0, and lists Paid Courses where Learn and Commerce are built in |
| HR PR 1 | HR 0.5.1: a submitted separation makes the employee a leaver on its relieving date; that day HR's upkeep ends their sign-in through deprovisioning (FERRUM-JOURNEY-003, partial) |
| Ferrum-Frontend PR 12 | The interface's language: words through `t` (Hindi and Arabic for the kit's), direction, and numbers and dates in a chosen locale (FERRUM-UX-006, partial) |
| Comms PR 4 | Comms 0.8.1: who is about in Chat, from sessions active in the last ten minutes (part of FERRUM-COMMS-002) |
| Ferrum PR 51 | An industrial solution in an app's bundle: the schemes it needs, device profiles and acceptance procedures, checked (FERRUM-IND-018, partial) |
| Ferrum PR 52 | The bootstrap's language from the person or the workspace; HR 0.5.1; three journeys recorded in parts (order to return, partner support, AI-assisted operation); the soak recorded |

**Evidence**:
- Every new test's figures were worked out by hand first.
- Mutants, run by hand from the scratchpad and all caught:
  - 10 for Paid Courses, 8 for SCORM, 8 for xAPI, 8 for LTI;
  - one SCORM survivor, the progress guard, was hidden behind permissions; the test now also tries as the administrator.
- scorm-again's events, data shapes and resume were checked by running the vendored runtimes in Node with a simulated package.
- LTI's id_token was verified against the JWKS Learn publishes. The client assertion was signed with a real tool key, and a second key was refused.
- **The soak passed every check** (soak run 41719722922, `docs/platform/evidence/soak.md`): three hours on the image with PR 45's list order. 11 errors in 2.78 million requests of the heavy workspace, outside its outages, and none for the others. The pace held (p95 268 to 285 ms) while the database grew to 8.8 GB. J12 is passes-in-parts.

**Not done:**
- Nothing here has run in a browser on a running server: the server could not be built on this machine for lack of disk. Real SCORM, xAPI or LTI content has not been tried.
- SCORM and xAPI packages run on the workspace's origin, so Learn Settings must allow them.
- LTI has no Deep Linking and no Names and Role Provisioning.
- The registry lists the rest.

**Lessons:**
- Ferrum strips `Authorization` from requests to app endpoints. A tool's bearer token reaches Learn only because it is a Ferrum API key of the tool's own service user, which the server checks itself.
- Objects built in another `node:vm` realm fail scorm-again's `constructor === Object` checks. Build test inputs inside the context.
- xAPI says an LRS should not refuse a voiding statement whose target is missing or already voided. The first test asserted the opposite.

## 2026-11-04: single sign-on by SAML, keys from a KMS, workflows that undo, and a group's books

**State**: on `main` of each repository, every PR green on its CI before merging. No release was tagged.

| Where | What |
|---|---|
| Ferrum PR 44 | SAML 2.0 (FERRUM-IDENTITY-006) on samael 0.0.22 and libxmlsec1, behind the `saml` cargo feature (on in the image). Each workspace signs its requests with a sealed key of its own, answers become one-time codes, and encrypted assertions are refused. Keycloak 26.8.1 in CI and once in a real browser |
| Ferrum PR 45 | Lists read their default order from an index (DDL version 2 resyncs once); Compose's database gets 512 MB of shared memory. This was the slowdown and the `/dev/shm` errors in the three-hour soak |
| Ferrum PR 46 | `FERRUM_SECRET_KEY` may be OpenBao or Vault Transit ciphertext (`FERRUM_KMS_TRANSIT`), or come from `FERRUM_SECRET_KEY_COMMAND` (FERRUM-IDENTITY-019). OpenBao 2.7.1 in CI |
| Ferrum PR 47 | Sagas on durable jobs: steps saved as they finish, a stable idempotency key per step, compensation newest first, `undo_on_cancel` (FERRUM-PLATFORM-019) |
| Cloud PR 14 | Tests for the 26 serve mutants that had survived, and a weekly `generated-serve` job |
| ERP PR 3 | Finance books (FERRUM-ERP-004), ERP 0.6.0 and Finance 0.3.0 |
| ERP PR 4 | A group's books (FERRUM-ERP-005), ERP 0.7.0: consolidated trial balance, profit and loss and balance sheet, eliminations, what each pair of companies says they owe each other |
| Assets PR 2, 3 | Finance books keep their own values, repairs add life and use parts, grouped assets split (FERRUM-ASSETS-002), Assets 0.3.0 |

**Evidence**: every figure in the new tests was worked out by hand first, and the working is written in
each test's comments. Mutants for each change: 11 for SAML, 2 for KMS, 6 for sagas, 2 for list order, 4 for finance books and 9 for the group's books
(`scripts/mutants.py`, all caught); 13 for Assets, run by hand (all caught after two tests were made
stronger). The three-hour soak is running again on the image with the list order fix (soak run 41719722922); J12 is recorded when it ends.

**Not done, and not code:** an identity provider in a customer's tenant (BLOCK-IDP-TENANT) and a key
service run for real (BLOCK-KMS) are a person's to arrange. Consolidation translates at one closing rate.
It has no ownership percentages, no minority interest, no consolidated cash flow, and does not mirror
intercompany invoices (in the registry). Real provider connectors (FERRUM-CONNECT-002) need accounts with
each provider to verify against: blocked outside.

**Lessons:**
- On macOS, brew's libxmlsec1 and the system libxml2 both got linked, and the test segfaulted. brew's
  libxslt has to come first on `PKG_CONFIG_PATH` (operations.md has the line).
- Keycloak refuses unsigned requests by default.
- Two packages that define the same entity stop a server holding both.
- An app's CI that resolves Ferrum's main unlocked breaks quietly when Ferrum's API moves, and it shows
  only on that app's next PR.
- A mutant that is equivalent for straight-line plans (re-planning a book nothing changed) survived
  until a manual plan was in the test.
- `gh pr edit --body-file` with an empty file empties the description: chain the steps with `&&`.
- The disk filled twice in a day. Superseded artifacts in the shared target, old cargo git checkouts and
  `incremental/` are safe to remove when nothing is building.

## 2026-11-02, evening: what holds when things repeat, arrive late or go wrong

**State**: the review's source findings are fixed, tested, walked on the real binaries as people who
are not administrators, and on `main` of each repository: Ferrum PR 20, 21 and 22, ERP PR 2, IoT Edge
PR 6, 7 and 8. Upgrades from the versions before them were walked too. No release was tagged: signing is
a person's. [Plan](../plan/industrial.md) has the table. This is software correctness only: **nothing has
run on physical equipment**.

| Where | What |
|---|---|
| Ferrum (Operations 0.2.0) | One live command per named thing, operations kept to their app, envelopes with limits, `release_evidence.py` |
| ERP (Warehouse and Shop Floor 0.2.0) | Idempotent release, load evidence, refusals that stay on record, reservations, a second gate for the machine, `line_it.rs` |
| IoT Edge 0.4.0 | Execution bindings, `clock.rs`, the gate before dispatch, recorded dependencies, redirects and the allow list, a frozen CI lane, `dispatch_it.rs` |

**Behaviour that changes for anyone using these:** confirming a task needs a scan of the load; a
machine's `machine.start_job` must be marked **Through Its App Only**; a run waits for its material
unless it says Preloaded; an `allow` entry means the host it names; a gateway upgrade wants nothing
running.

**Not done, and not code:** the review's P2 (one warehouse and one discrete manufacturing reference
deployment on real equipment, against exact artifacts) waits on BLOCK-INDUSTRIAL-HARDWARE and
-VENDORS. Operator trials with gloves and scanners, sustained multi-site load, and a penetration test
are not done. Its P4 (optimization, more protocols, autonomy) stays frozen until those are.

**Lessons:** mutants found two tests that claimed more than they proved, and one mutant pointed at the
wrong suite. A killed mutant run leaves its edit behind: `git status` before anything else. A local
Ferrum patch rewrites an app's `Cargo.lock`, and `git add -A` commits it. A patch is silently unused
when the lock pins another version of the crate: read `[[patch.unused]]`. Every suite was green when a
walk through the real thing found that recovering a mission could leave an order with nothing to pick
it and its wave saying Picked. Walking is not optional. A mutant whose text has moved guards nothing
and stops a full run: four had, and a full run on the final commits found a fifth test that passed for
the wrong reason. Only real CI found that a called workflow shared a concurrency group with its caller's
sibling, and that the frozen lane's frontend was older than the interface needed. Running a green suite
again is cheap: three tries found two flakes in tests written this week.

## 2026-11-01, evening: integrations, the record frame and every app's interface

**State**: everything below is on `main` of its repository, except what is listed under "Not done".

### What landed

| Where | What |
|---|---|
| Ferrum PR 8, 9, 10, 11 | Plugin credentials the plugin cannot read, an OAuth client-credentials broker, per-event outbox leases, hook isolation; a connector contract with capabilities, discovery and health, connectors as plugins, a provider that says wait left alone; records that say what links to them, shared views, `/metrics` for connections, an evidence pack, journeys across apps, Services and Field Operations suites |
| Ferrum-Frontend | Filters by period, emptiness and number; a panel beside lists, boards and calendars; import; merging; related records; shared views; calendar rescheduling; a Connections console; pdf.js |
| All 16 apps | A hand-crafted interface for each. See [references/README.md](../references/README.md) for what each has, how far it was tried and what was left out |
| Commerce gateways | Refunds, a Payment Exception a person decides, signed webhooks, PayPal's token held by the host |
| ferrum-sdk | One SDK, usable as a git dependency, with `fetch_with`, `verify_hmac_sha256` and test helpers |

### Evidence

CI is green on main for every app and for Ferrum at the merge of each PR. `server/tests/journeys_it.rs`
runs lead to cash and procure to pay across the apps and back, on figures worked out by hand. Each
interface has its own tests, type checks and bundle budget. They were then tried by hand on the real
binary in one browser, recorded per app in the references page.

Exercising them found what their tests had not: the click that ends a drag starting a new booking,
Helpdesk and Field Service both defining `ServiceContract` so the server would not boot, a launcher
icon the shell could not draw, a draft asset shown as overdue, and a button that stayed grey with no
reason. Helpdesk's replies had never reached a customer by email at all.

Not run: any real payment, identity, AI, antivirus, mail delivery, media or connector provider;
`preflight.sh` on a clean host; ERP's whole workspace on this machine (CI ran it). No independent
security, legal or accessibility review has happened. Nothing is `verified_complete`.

### Not done

Listed in [backlog.md](backlog.md). The largest: a separate control plane, signup and closure,
billing, a partner console, SAML, a key-management service, cross-system sagas, translation, a help
centre, deferred revenue and accounting dimensions, parallel books and consolidation, and any
certified connector. Publishing the SDK waits on packaging `@ferrum/*` and the owner's npm account;
payment and tax providers wait on keys only the owner can obtain.

### Findings to carry

- Apps commit `ui/dist`, so a Frontend change reaches an app only when its `ui/dist` is rebuilt and
  committed. `npm run check` compares only the app's own sources.
- A workspace keeps an app's installed version: after a pin moves, `POST /api/admin/apps/{id}/upgrade`.
- Locally, `journeys_it` needs a fresh database, because dropping every app's tables runs out of locks.
- Several parallel builds in one `target` use about 4 GB an hour on this disk.

## 2026-11-01: evidence, registry and foundations

**Read at** the revisions in [release-manifest.yaml](release-manifest.yaml). **Nothing here is pushed.**

### Branches to push, in this order

Platform first: the apps' declarations use `retain`, which older platforms refuse.

1. `CBFerrum/Ferrum`, branch `platform/foundation`, on `origin/main` 2f1917d.
2. `privacy/declarations` in sign, lms, pos, hr (with a test fix), drive, projects, assets, commerce,
   field-service, comms.
3. `bookings/series` in ferrum-app-bookings.
4. Bump the pins in Ferrum (`cargo update -p ferrum-app-x`), which moves `not_searched` for those apps.

Pushing to main starts the new release pipeline for the first time. The workflows have only been
checked for shape, and `promote` needs `id-token` and attestations, so watch that first run. The
editors job now pushes `candidate-*` tags, which `promote` does not remove.

Local `main` in the main checkout still holds 8e6205c, the plugin-sealing commit that was never
pushed. It is `acc2063` on this branch, so once that merges, reset local `main` to origin.

### What this work built

| Commit | Requirements |
|---|---|
| acc2063 | `FERRUM-IDENTITY-017` |
| 176d7c7 | `FERRUM-PLATFORM-017`, `FERRUM-EXT-002`, `FERRUM-EXT-003`, `FERRUM-EXT-004` |
| 0128cfc | `FERRUM-PLATFORM-012` to `015`, `FERRUM-PLATFORM-018` |
| 0f28652 | `FERRUM-RELEASE-001`, `002`, `006` |
| 4527f53 | `FERRUM-IDENTITY-002` to `004` |
| 924f3d7 | `FERRUM-VERIFY-001` |
| 83faa02 | Workstream 1: the registry and working documents |
| fc50add | `FERRUM-IDENTITY-010`, `011`, `013`, `017`, `018` |
| 20ef873 | `FERRUM-PRIVACY-002`, `004`, `006` |
| 08525bb | Rotation reaches containers; API reference |
| bookings/series 40fcc5d | `FERRUM-BOOKINGS-002` |

### Evidence

Ran here against PostgreSQL 17 on macOS, on this branch: the workspace's tests, `cargo fmt`, clippy with
warnings as errors, and the binary against a copy of the development database taken before these
migrations, driven over HTTP (42 checks). `scripts/mutants.py` breaks each new invariant and requires
its test to fail: 26 of 26 caught, with seven more for privacy.

Apps against this branch: the full suites of Bookings, POS, Commerce, Projects and HR pass, and the
catalog tests of Sign, Learn, Drive, Assets and Field Service accept their new declarations. Comms'
catalog test passes when built with the current `ferrum-apps`. Not run against this branch: ERP's own
suite, and the full suites of Sign, Learn, Drive, Docs, Assets, Field Service, Forms, Campaigns,
Helpdesk and CRM.

Not run at all: GitHub Actions, Docker or Kubernetes (Docker is not running here), any browser, a
real identity provider, or a real payment or tax provider.

### Findings to carry

- Every sibling checkout had been behind its remote, POS by 10 commits. Probing stale code reports
  finished work as missing: fetch first. Fast-forwarded, except `erp4/ferrum-app-erp`, which holds
  someone's uncommitted tax-profile work.
- `ferrum-app-*/.cargo/config.toml` builds against the sibling `ferrum`. Those files are git-ignored,
  and some lack `ferrum-calc`, which then fails as a type mismatch inside ferrum-app-erp. Pass
  `--config 'patch."https://github.com/CBFerrum/Ferrum".ferrum-calc.path="../ferrum/crates/calc"'`.
- `the_interface_is_a_release_the_platform_takes` fails in Comms with its locked `ferrum-apps`, which is
  older than its manifest. Its lock wants updating; nothing is wrong with the app.
- HR's `expense_claims_and_advances` began failing on 1 November 2026 because a claim took today's date.
  Fixed on `privacy/declarations` (47efc5e), which means HR's CI on main is red until that lands. Other
  tests may have the same trap: a fixed report window around a record dated today.
- 15 of the 17 apps Ferrum pins are behind their own main (release-manifest.yaml). Mostly READMEs, but
  check when bumping.
- The audit's own errors are worth remembering: a zsh loop that did not split paths reported everything
  as missing, and `&&` after `grep` committed past a failing clippy. Gate on the tool's exit code.

### Industrial operations (November 2026)

Ledger, site runtime, warehouse, shop floor, quality and maintenance exist and were walked end to end
on a real server with a simulated fleet ([plan](../plan/industrial.md), `industrial.yaml`). Nothing has
touched hardware and nothing is a safety claim. Mutants live in both repositories: run
`scripts/mutants.py` in `ferrum` and in `ferrum-app-erp` after changing an invariant. Test as a
person with a role, not as an administrator: `links_out_of_reach` exists because that was missed.

### Next, in dependency order

The generated [backlog](backlog.md) is the full list. The most valuable next steps:

1. Apps handle `ferrum.user.deprovisioned`: Drive shares, Helpdesk queues.
2. Mark sensitive entities `reference: restricted`: Employee, SalarySlip. The mechanism exists; no
   package uses it yet.
3. Bulk paths set `PRIORITY_BULK` (importer, `bulk.rs`), and Campaigns' sender gets the fairness the
   outbox has.
4. A shared job queue with cancellation and retry budgets (`FERRUM-PLATFORM-016`).
5. Native hooks audited for external calls beyond the IRP and VIES (`FERRUM-PLATFORM-017`).
6. A standing browser test suite and accessibility checks in CI (`FERRUM-UX-001`, `FERRUM-UX-005`).
7. Then the connector platform (`FERRUM-CONNECT-001`), which industry packs and most of workstream 8
   wait on, and the control plane (`FERRUM-CLOUD-001`), which workstream 9 waits on.

Most of the specification remains: see [capabilities.md](../capabilities.md). Nothing is
`verified_complete`, because no release has been qualified and no person has reviewed anything.
