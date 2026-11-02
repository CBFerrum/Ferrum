# Handover

Where the work stands, kept here rather than in anyone's memory (specification §19.5). Newest first.
What is true of each requirement is in the registry; this says what happened and what comes next.

## 2026-11-02, evening: what holds when things repeat, arrive late or go wrong

**State**: the review's source findings are fixed, tested, walked on the real binaries as people who
are not administrators, and on `main` of each repository: Ferrum PR 20 and 21, ERP PR 2, IoT Edge PR 6
and 7, and this commit's pins. No release was tagged: signing is a person's. [Plan](../plan/industrial.md)
has the table. This is software correctness only: **nothing has run on physical equipment**.

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
sibling, and that the frozen lane's frontend was older than the interface needed.

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
