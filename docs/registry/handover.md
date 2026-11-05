# Handover

Where the work stands, kept here rather than in anyone's memory (specification §19.5). Newest first.
What is true of each requirement is in the registry; this says what happened and what comes next.

## 2026-11-04: the control plane, social media, and paid apps in the store

**State:** this PR follows PR 56. It compiles the control plane and Social in, and gives the shell
the icons they name (Ferrum-Frontend PR 16, merged, does the same for the launcher). The store's paid
apps are on the store's main branch. No release was tagged.

| Where | What |
|---|---|
| New: ferrum-app-control | Control Plane 0.1.0, run in a workspace of its own. Cells report how they are; workspaces are placed by room and their health read with the customer's health-only support key; releases by digest, qualified by a second Release Manager, rolled out a wave at a time in maintenance windows, soaking and halting; incidents told in updates; a public status naming no customer; recovery requests decided by a second operator; partners' portfolios, setup templates, cases and statements (FERRUM-CLOUD-001, -004, FERRUM-PARTNER-001, FERRUM-IND-021, each partial) |
| New: ferrum-app-social | Social 0.1.0: brands' accounts each reached through a platform connection; posts that fit each network, approved by someone other than their author and again after a change, sent at their time one publication per account through that account's own connection by the platform's delivery; what posts did read back; the inbox routed by handle and answered through the right connection. 0.2.0 brings a connector for Mastodon (FERRUM-ECOSYSTEM-005, partial: not run against a real server) |
| ferrum-store | Paid apps: plans by verified publishers who say who supports the app, purchases with the buyer's country's tax, licences the store signs, renewals with a week of grace, refunds within 14 days, disputes decided by an administrator, payouts net of tax and a 15% commission (FERRUM-EXT-009, partial: no real payment provider) |
| This PR | Control and Social compiled in; the shell draws server, share-2 and plug; FRONTEND_REV at Ferrum-Frontend PR 17's merge; the Integration Hub's history, `GET /api/admin/connections/{id}/activity`, which Ferrum-Frontend PR 17 shows on each connection (FERRUM-ECOSYSTEM-004, partial) |
| This PR: licences | The App Manager installs or upgrades an app a store sells only with a live licence for the workspace: `PUT /api/admin/apps/{id}/licence` keeps one once the store's signature, the app and one of the workspace's own hosts check out; each installation asks the store again, whether the app comes from the store or from the cluster where another workspace installed it; a store that cannot say whether it sells the app refuses the installation rather than letting it through; every licence is read again with the revocation list (FERRUM-EXT-009) |
| ferrum-store 049459d | Stripe as the payment provider: a card saved on Stripe's page and told by its signed webhook, off-session charges keyed for Stripe's idempotency and retried with the key, a payment not taken at once cancelled, a refused renewal tried again with a new key, refunds (FERRUM-EXT-009; tested against a stand-in of Stripe's API) |
| ferrum-app-hr 0.6.0 | Income tax slabs take a rebate and cesses and surcharges; India's new regime for FY 2025-26, as the Income Tax Department's page prints it, is a fixture the engine is checked against (FERRUM-VERIFY-002, now partial) |
| The walk's fixes | Ferrum-Frontend PRs 18 to 20 (labels name their fields, linked titles show their titles, Connections refresh while work waits, "1 record", the native line in words); Control 0.1.1, Government 0.1.1, Property 0.1.1, Social 0.2.1 and 0.2.2, Contracts 0.1.1, Objectives 0.1.1 |
| Every app rebuilt | The 36 apps with interfaces rebuilt against Ferrum-Frontend 80ff179 as patch releases, since each bundles its own record pages; this PR's lock takes them. FRONTEND_REV at PR 20's merge (16b5c57) |

**Evidence:**
- Control's scenario test runs the control plane in one workspace and a customer's in another, reached over HTTP with the key its administrator granted (`grant_support`), answered as the server answers one, each request on the customer's log. A rollout goes through two windows, halts when a cell falls ill while soaking and when one misses its window, and another finishes.
- Mutants by hand, all caught after fixes: Control Plane 96 (the first run caught 94 of 97: lowercasing an e-mail the kernel already lowercases was removed, and two mutants that did not compile were rewritten), the store's paid apps 35 (three gaps the first run found are tested now), Social 40 (three gaps tested, two mutants rewritten).
- The Integration Hub's history is tested in `crates/kernel/tests/connect_it.rs`; it has no mutants of its own.
- A browser on a server built from this branch (evidence/browser-2026-11-04-apps.md): every new app's tile and home page, the Control Plane's cell form, a cell's report, the public status and a workspace's placement. It found Governance and Risk's and Property's home pages filtering with `neq`, which the platform does not take (both now use `!=`, and the lock here takes the fixes), and three home pages with nothing to say when empty (now they say so).
- The store's `paid_apps` test walks plans, purchases, licences, renewals, trials, a refused renewal through its grace, refunds, a dispute, and payouts, with a refund after a payout coming off the next.
- `server/tests/apps_api.rs` installs a sold app: refused without a licence; licences for another workspace, signed by a publisher's key instead of the store's, lapsed, or for another app refused; installed with its own; a lapse read on the next round; another workspace refused though the app is in the cluster; a store that cannot list its plans refusing the installation.
- A browser walk of the apps' flows and portal pages (evidence/browser-2026-11-04-flows.md): six people signed in through the pages, four of them portal people with e-mailed codes; the Control Plane's release, rollout (through its soak to done), incident and public status, the partner's and customer's pages, Social on a Mastodon stand-in, a citizen's permit, a risk acceptance, an objective's check-in, an IT incident, ERP's company and an invoice, Property installed with a lessee's Your home, a certified metric, a contract approved and Digital Assets. It found nine faults, fixed and walked again, among them a public reply to a Mastodon direct message and a lessee's link to a draft invoice.
- The store's `stripe_payments` test runs the Stripe provider against a stand-in, a lost answer included; 31 mutants, all caught. HR's tax change: 9 mutants, all caught.
- Social's Mastodon connector runs in `tests/mastodon_it.rs` against a stand-in speaking the parts of Mastodon's API it uses, over HTTP: a wrong token refused, a post sent once though its first answer was lost (the same `Idempotency-Key` twice), engagements read back without boosts, nothing sent again once out after the server forgot its key, a comment, a mention and a direct message routed, a reply answering the comment. 27 mutants of the connector, all caught.

**Not done:** Stripe and Mastodon against the real services (the operator's account and keys, BLOCK-PAYMENT-KEYS, and an account's token); connectors for the other networks; an accountant's review and payroll cases from someone who knows each jurisdiction (FERRUM-VERIFY-002); Stripe in the walk (it paid through the store's test provider); the store's CI, which GitHub does not start for the account's billing; and a screen reader.

**Lessons:**
- Each app bundles its own record pages and the kit from `../../ferrum/web`, another session's checkout on an older branch: a Frontend fix reaches the apps only when they are built against a Frontend that has it (the walk built them in a scratch layout linking a checkout of main), and every app needs its rebuild.
- Read toasts at once: a refusal shown as a toast fades in nine seconds, and reading the page's main area misses it.
- An action's parameter named like a field of the record is filled with that field's value.
- A page's own queries are not run by an app's scenario test: walk each home page in a browser, or scan the interfaces for filter operators the platform does not take (it takes `=`, `!=`, `<`, `<=`, `>`, `>=`, `in`, `not in`, `like`, `between`, `is` and their word forms `eq`, `ne`, `lt`, `lte`, `gt`, `gte`).
- Rows written straight to the database in a test go through row-level security too: an update outside a tenant's transaction matches nothing and says nothing. Assert the rows affected.
- An app can send one record through one chosen connection by queueing the platform's own delivery (`jobs::enqueue` with `connect::DELIVER_KIND`) for a stream mapped without `push`: a pushed mapping sends every change of the entity through every connection that maps it.
- Two checks can make a third redundant: an incident's latest update must say its status, and updates stay as posted, so a change of status without a new update can only come from reordering them, which is the one case the third check is for.

## 2026-11-04: objectives, digital assets and governed metrics

**State:** this PR follows PR 55 and compiles three more apps in. No release was tagged.

| Where | What |
|---|---|
| New: ferrum-app-objectives | Objectives 0.1.0: objectives aligned to their parents within their periods, key results measured by check-ins, progress rolled up the tree, and the projects and portfolios behind them from Projects (FERRUM-ECOSYSTEM-011, partial) |
| New: ferrum-app-dam | Digital Assets 0.1.1: assets kept as Drive files with their rights, approved by version, back to review on a new version, expiring, and used only within their rights (FERRUM-ECOSYSTEM-012, partial). 0.1.1 renamed its roles Media Contributor and Media User: fixed assets already had an Asset User |
| New: ferrum-app-metrics | Metrics 0.1.0: a metric as a Studio query of one measure, certified by a steward who does not own it, back in draft when its definition changes, read by everyone with their own rights, kept as snapshots, deprecated pointing at a replacement, and a glossary. Its interface is at `/kpis/`, since the core keeps `/metrics` (FERRUM-ECOSYSTEM-010, partial) |
| This PR | The three compiled in; `ferrum-studio` patched to the workspace's crate, since Metrics is the first app to use it from git |

**Evidence:**
- Each app's scenario test passed against Postgres. Metrics' reads one definition as three numbers for two sales reps and their director, for a count and an average.
- Mutants by hand, all caught after fixes: Objectives 20, Digital Assets 19, Metrics 29. Metrics' first run found that a pivoted definition and a decimal value were never tested.
- The server checks with the three apps; CI's `rust` job runs `compiled_apps_fit_together`, which disk here does not allow.

**Not done:** the apps in a browser; ECOSYSTEM-004, -005, EXT-009, CLOUD-001, -004, PARTNER-001, IND-021.

**Lessons:**
- Studio's result columns are named by `name`, not `key`; reading the wrong one read every metric as 0, which only a test against the database showed.
- A route can be kept by the core (`/metrics` is Prometheus'): the manifest refuses it, so check the reserved routes before naming an interface.

## 2026-11-04, night: public services, retail, IT services, governance and risk, and contracts

**State:** Ferrum PR 54 merged (eight industry apps compiled in) and Ferrum-Frontend PR 15 (the launcher's icons). This PR compiles five more apps in and moves FRONTEND_REV to PR 15's merge. No release was tagged.

| Where | What |
|---|---|
| New: ferrum-app-government | Government 0.1.0: permits applied for on the portal with their documents uploaded, fees invoiced through ERP and paid before approval, rejections saying why, permits issued, expiring or revoked; cases with deadlines (FERRUM-INDUSTRY-010, partial: no legal review) |
| New: ferrum-app-retail | Retail 0.1.0: click and collect against a pickup code with the delivery note submitted at hand-over; franchise royalties invoiced through ERP. Replenishment is ERP Procurement's auto-reorder, not reimplemented (FERRUM-INDUSTRY-003, partial) |
| New: ferrum-app-itsm | IT Services 0.1.0: configuration items whose dependencies never loop, a catalog with approvals by someone else, incidents by the priority matrix with breaches, problems, and changes with freezes, conflicts and impact (FERRUM-ECOSYSTEM-002; -003 partial) |
| New: ferrum-app-grc | Governance and Risk 0.1.0: a risk register on a five-by-five matrix, acceptances by an approver who does not own the risk, controls tested with evidence, policies by revision, findings (FERRUM-ECOSYSTEM-014), adapted from CISO Assistant (AGPL-3.0) |
| New: ferrum-app-contracts | Contracts 0.1.0: drafts from approved clauses, approval above a threshold, signature through Sign, renewal with notice, obligations (FERRUM-ECOSYSTEM-008) |
| Ferrum PR 55 | The five compiled in; FRONTEND_REV at Ferrum-Frontend PR 15's merge |

**Evidence:**
- Each app's scenario test passed against Postgres with Ferrum 808cd94; Contracts' with Sign installed, sending through Sign's real action.
- Mutants by hand, all caught after fixes: Government 24, Retail 22, IT Services 36 (all on the first run), Governance and Risk 27, Contracts 29. The first runs found eight checks the scenarios never reached; each is tested now.

**Not done:** the apps in a browser; ECOSYSTEM-004, -005, -010, -011, -012, EXT-009, CLOUD-001, -004, PARTNER-001, IND-021.

**Lessons:**
- Look in ERP's add-ons before building: Procurement already replenishes stores, which Retail would have duplicated (and the name clashed with WMS').
- Before publishing an app, scan every app's entity and role names for clashes: the server's `compiled_apps_fit_together` test refuses a shared entity name, and a shared role name silently merges rights.

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
| New: ferrum-app-restaurant | Restaurant 0.1.0: orders priced from the menu, kitchen tickets by station, sent lines voided not edited, bills through ERP, takeaway, delivery and reservations (FERRUM-INDUSTRY-002), adapted from ERPNext 12's Restaurant module |
| New: ferrum-app-property | Property 0.1.0: units held by one lease's days at a time, rent and charges invoiced each period once through ERP, leases ended at their end or early, urgent requests holding a unit, inspections, and lessees' "Your home" on the portal (FERRUM-INDUSTRY-008, partial: no owners' portal) |
| New: ferrum-app-construction | Construction 0.1.0: estimates with a markup into contracts, variations decided by a Contracts Manager, progress claims recomputed when certified and invoiced through ERP less retention, retention released on completion, site diaries (FERRUM-INDUSTRY-007) |
| New: ferrum-app-logistics | Logistics 0.1.0: trips from delivery notes with stops ordered nearest first, dispatch checking vehicle, load, licence and that neither is out, proof of delivery by the trip's driver, and drivers' own stops (FERRUM-INDUSTRY-009, partial: road routing waits on a maps key) |
| New: ferrum-app-healthcare | Healthcare 0.1.0: appointments within hours and without clashes, consent by form version, visits completed by their practitioner and billed with follow-ups covered (FERRUM-INDUSTRY-005, partial: administration only, no legal review), adapted from ERPNext 12's Healthcare module |
| Ferrum-Frontend PR 15 | The launcher draws the new apps' icons (bed, hand and heart, hard hat, utensils, truck, stethoscope, cloud) |
| Ferrum PR 54 | Comms 0.9.0, Hospitality, Nonprofit, Education, Restaurant, Property, Construction, Logistics and Healthcare compiled in; the shell bar draws their icons; the operations guide's recordings; entity files may start with a comment |

**Evidence:**
- Mutants were run and all caught: 11 for sign-up and plans, 8 for acceptance (one added after the browser run), 9 for Comms by hand, 10 for Hospitality by hand, 9 for Nonprofit, 11 for Education and 11 for Restaurant by hand. Restaurant's two survivors were a test that never changed a price after ordering and a mutant undone by operator precedence; both are caught now. Logistics' 28 and Healthcare's 29 by hand: three survivors showed checks the scenarios never reached (a depot without its place, an inactive practitioner, a visit before hours) and are tested now. Construction's 34 by hand: one check the kernel already made was removed, and an uneven markup was added to the scenario. Property's 27 by hand: two survived at first (a lease starting the day after another ended early, and a period invoiced twice once its next date was wound back by hand); both are tested now. Hospitality's one survivor was a check nothing could reach; it was removed and the guard that matters was tested instead.
- Kernel, Operations, Comms and Hospitality tests ran on this machine. The server was checked with clippy with and without Operations.
- **A browser on a running server, for the first time** (`docs/platform/evidence/browser-2026-11-04.md`): from sign-up to closing, and an industrial solution from upload to commissioning. It found a real bug that no test had: an installation that brings Operations imported with the request's stale schema. PR 51 fixes it, with a test and a mutant.

**Not done:**
- Real LiveKit Egress, real speech recognition, or a real call.
- Equipment, and a channel manager for the hotel.
- Personal-data erasure of meeting transcripts.
- The other industry packs, the ecosystem apps, store monetization, the control plane, the fleet and the partner console. The registry lists them.

**Lessons:**
- Entity names are one namespace across every compiled app, and a role of the same name merges across packages: PR 54's second run found Property's MaintenanceRequest clashing with Assets'. Property's is RepairRequest now; Healthcare's Front Desk became Clinic Reception and Logistics' Dispatcher Fleet Dispatcher, so a hotel's desk holds no patients and field service no fleet.
- The server's test that every launcher icon can be drawn failed PR 54's first run: three new apps named icons the shell bar did not have. Check `server/shell/runtime.js`'s set when an app picks an icon.
- A list's rows (`find_tx`) come without their child rows: read a record whole with `get_tx` before reading its lines.
- A row built in `before_validate` needs its `id`: the kernel assigns ids to the rows it is given, before hooks run.
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
