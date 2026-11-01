# Handover

Where the work stands, kept here rather than in anyone's memory (specification §19.5). Newest first.
What is true of each requirement is in the registry; this says what happened and what comes next.

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
