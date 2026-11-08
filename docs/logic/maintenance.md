# Maintenance: owner, supported versions, incidents, retiring a runtime

FLC-QUAL-008 asks for supported profiles, versions, incident procedures and ownership to be assigned, and its
acceptance (AT-QUAL-008) for the retirement of a provider or runtime that customers use to be worked through:
its impact, its replacement or export, and an accountable support path, all written down. This page is that
assignment.

## Owner

**Chinmoy Bhuyan** (the repository owner; GitHub `hyndex`) owns all four profiles, Core, Full Process, Edge
and Intelligence ([profiles.md](profiles.md)). That covers their supported versions, incident response and
every retirement below. Named on 9 November 2026, by their own decision.

What this does not give:
- **A second owner.** Support rests on one person. Name a second owner before promising anyone a response time.
- **A funded support or LTS commitment** (BLOCK-LTS-SUPPORT). Nothing is labelled production-supported, and
  no SLA is offered.

## Supported versions

Only the newest is supported. Fixes land on `main` and reach the next promoted image; there is no long-term
branch.

| Profile | What runs | Supported | Pinned by |
|---|---|---|---|
| Core | Ferrum's server and PostgreSQL | `main` and its latest promoted image (`ghcr.io/cbferrum/ferrum`, pinned by digest); PostgreSQL 17, as CI runs | `docs/registry/release-manifest.yaml` |
| Full Process | Core, plus Temporal | Temporal Server 1.32.x (qualified by ADR L-01 and the reference profiles) and `temporalio-sdk` 1.1.2 | `deploy/temporal.yml`, `Cargo.lock` |
| Edge | Core, plus IoT Edge and its site gateway (`ferrum-site`) | IoT Edge's latest release and its gateway binaries | `Cargo.lock`, IoT Edge's releases |
| Intelligence | Core, plus Metrics and Intelligence, and Metrics' engine for external sources | Metrics 0.7.2 and Intelligence 0.2.1; the engine image `ghcr.io/cbferrum/ferrum-metrics-engine`, pinned by digest (Wren 0.5.1 on DataFusion 55) | `Cargo.lock`, `docs/registry/engines.yaml` |

A new version of any of these is taken through the reference profiles again: `scripts/release_evidence.py
qualified` fails until they have run on it ([lrp-profiles.md](evidence/lrp-profiles.md)).

## Incidents

- **Reporting.** Security problems are reported as `SECURITY.md` says, privately. Everything else goes to
  an issue on `CBFerrum/Ferrum`, or on the app's own repository.
- **Severity.**
  - 1: data lost, or one workspace's data or a hidden column reachable by someone not allowed it, or the
    platform down.
  - 2: a profile degraded, such as processes not starting or external metrics without values.
  - 3: everything else.
- **Response.** The owner takes each report, works it with the playbooks in `docs/operations.md`, and says on
  the issue what was done. The playbooks are "Incident playbooks", "Durable processes" (restore hold, recovery
  items, `ferrum task logic_replay`), "After restoring a backup" and "Metrics' engine for external sources".
- **Afterwards.** A severity 1 or 2 gets a written account in the repository: what happened, what was done,
  and what changes. The next release notes link to it.

## Retiring a provider or runtime

Worked through for each runtime customers use. This is a tabletop exercise, not a rehearsed retirement: each
step is a feature with its own evidence, named beside it.

### Temporal: leaving the Full Process profile

- **Impact.** Runs in progress need their runtime. Once `TEMPORAL_ADDRESS` is gone, a solution with a
  process no longer compiles: "it needs the Full Process profile, and this installation runs Core"
  (`unsupported_profile`; `crates/logic/src/compile.rs`, test `t21_a_process_needs_its_profile`).
- **Steps.**
  1. Deactivate each solution that has a process. New events then start no runs (Studio, or
     `POST /api/app/studio/solutions/{id}/deactivate`; the profiles walk deactivates solutions between
     profiles).
  2. Let open runs finish, or cancel each with a reason, which is audited (`crates/kernel/tests/process_it.rs`).
  3. Export each release as a pack to take it elsewhere (`GET /api/app/studio/solutions/releases/{id}/pack`; LRP-04 and
     `crates/studio/tests/solutions_it.rs`).
  4. Remove `TEMPORAL_ADDRESS` and Temporal's deployment. Temporal's histories are kept for the namespace's
     retention (30 days) for as long as its store is. Runs, tasks, decisions and attempts stay in PostgreSQL.
- **Replacement.** In-record approvals move to Core's record workflows, with four-eyes approval, and to rules.
  Long processes move to another installation that runs Full Process, through the packs.

### Metrics' engine, or one external dataset

- **Impact.** External metrics have no value and say so ("Metrics' engine cannot be reached"), and so do
  metrics composed of them (`ferrum-app-metrics: tests/external_it.rs`). Intelligence declines to answer from a
  metric without a value, and says why (FLC-AI-004; `ferrum-app-intelligence: tests/evals_it.rs`).
- **Steps.** Deprecate the external metrics, or move their data into Ferrum's records and define them as
  metrics of the records; a changed definition is certified again. Then take the dataset out of the engine's
  configuration. External Sources stay, as the record of what was read.
- **Export.** Metric definitions and kept values are records, and export as records do.

### A model provider for Intelligence

- **Impact.** Answers continue: the rules provider answers without a model, and a model is used only once it
  passes the fixed and adversarial evaluations (FLC-AI-008; `ferrum-app-intelligence: tests/evals_it.rs`).
- **Steps.** Turn the provider off in the assistant's settings. Examples and questions are records and stay.

### An app release

- **Impact and steps.** A store's revocation, or `ferrum task app_revoke`, disables the app in every workspace
  that runs it and keeps its data ("Incident playbooks" in `docs/operations.md`). A process step that needs
  one of its capabilities waits, with an attention item, and administrators may cancel the run (FLC-OPS-007;
  `crates/kernel/tests/process_it.rs`).

In every case the owner above is accountable, and an issue on the repository is how a customer reaches them.
