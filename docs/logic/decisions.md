# Decisions

Consequential choices for the Logic Platform program. The package names candidates; nothing here is
qualified until its entry in [the registry](../registry/flc.yaml) says so.

| # | Decision | Rejected, and why | Gate |
|---|---|---|---|
| L-01 | **Durable processes run on Temporal**, with one Ferrum workflow type that interprets a compiled, immutable release. The user chose this direction on 8 November 2026. It holds only if the qualification spike passes (below); Ferrum keeps every business record (tasks, decisions, effects, receipts, signals, deployments) in its own database, and Temporal keeps scheduling state | Ferrum's jobs and sagas as they are: no timers, waits, branches or people inside a workflow (FERRUM-PLATFORM-019). A new engine on PostgreSQL: one store and no new service, but Ferrum would own a durable engine, which the package allows only with funded ownership and a full conformance suite. Restate: its BSL needs a review per offering, and two engines are not built side by side | The spike's results, recorded here, before W3 starts. A failed probe or an operating cost out of proportion sends this back to the user with the evidence |
| L-02 | **Expressions are a typed CEL subset.** The existing `cel` crate parses; Ferrum checks types on its syntax tree, then evaluates with exact functions for money, quantities and decimals on `rust_decimal`. Units come from core `Uom`, zones from `chrono-tz`. Time, randomness and identifiers are facts handed in, never read from the machine. Unknown and error are values, not false | Plain CEL numbers: doubles, so money would round. FEEL in full: the package asks for a documented subset, not a claim of DMN conformance. A new language: one more thing to learn and secure | CEL conformance tests from cel-spec for the subset |
| L-03 | **One canonical digest.** Cloud's narrowed RFC 8785 (D-05: strings and integers only, null distinct from absent) moves into `crates/logic`; Cloud then uses the platform's copy. A release has a source digest and an artifact digest; layout is outside both, and labels, instructions and forms a person decides by are inside | Hashing request bodies: key order and whitespace are not meaning. A second canonicalizer: two digests for one intent | Shared test vectors with Cloud's existing ones |
| L-04 | **Names.** *Solution* is the unit a customer authors, reviews and deploys (the package's Solution Pack). *Process* is a durable solution component. *Rule* is a derive or validate step on a record. *Decision* is a decision table. *Pipeline* is an event component. Existing words keep their meaning: a record's *workflow* is its state machine, an *automation* is today's event-condition-action, a *flow* is an Edge flow, a *saga* is code-defined compensation. Edge's industrial solutions are templates an app ships; they become solutions when pipeline v2 lands, so one word means one thing | Renaming working APIs to match the package: the package itself forbids it. Reusing *workflow* for processes: it already names record approvals | Edge's solutions move under this word in W5, or this row changes |
| L-05 | **Where code lives.** `crates/logic` is pure (types, digests, capability contracts, parser, compiler, IR, evaluator, simulator core) so the Edge gateway can depend on it as it does on `ops-contract`. Database parts are kernel modules beside `jobs.rs` and `saga.rs`. The Temporal worker is `crates/process`, behind a server feature, so no app builds its dependencies. Solutions' endpoints extend Studio. Intelligence is an app in its own repository | Nine crates, one per noun in the package: its modules are logical boundaries. The worker in the kernel: every app would compile gRPC and protobuf | The first boundary that needs its own process splits it |
| L-06 | **Profiles** are Core, Full Process, Edge and Intelligence ([profiles.md](profiles.md)). A definition its installation cannot run is refused with `UNSUPPORTED_PROFILE`; nothing falls back to a weaker runtime that looks the same | Best effort on jobs when Temporal is absent: a deadline that a restart can lose is not a deadline | |
| L-07 | **Analytics.** Native questions stay on Studio's compiler, which already applies every joined entity's row scope for the viewer; Metrics stays the one authority for definitions. Wren's Rust core (Apache-2.0, `core/**`) is evaluated behind a provider trait for external sources only, with DataFusion's statements, files, network and memory locked down; in-process if its footprint allows, otherwise an isolated service | Rewriting Wren for language purity. Routing native queries through DataFusion: it would bypass row scopes Studio already enforces. Wren's other paths: not needed, and its trademark is not ours to use | Built on 8 November 2026 as the isolated service: `ferrum-metrics-engine`, in the Metrics repository (`wren-semantic-core` 0.5.1 on DataFusion 55, 1.5 GB to build without debug info). The server never links it; Metrics asks it over HTTP with each reader's rights, applied in each dataset's own scan ([closing-gaps.md](evidence/closing-gaps.md), batch 4) |
| L-08 | **Tracking and landing.** Requirements in [flc.yaml](../registry/flc.yaml) with the platform program's states; the package's text in [catalog.json](catalog.json). Each gate is a pull request, merged after the checks run on this machine, at the commit they ran on: the user's decision of 8 November 2026. That is not independent review, which stays [BLOCK-SECURITY-REVIEW](../registry/blockers.yaml) | Shipping the package's Python checker as the compiler: it is illustrative and says so. GitHub's CI as the merge gate: the user asked for checks run here | |

## L-01 qualification spike

What must hold before W3 commits to Temporal, each probe against pinned releases (Temporal server
1.32.2 with PostgreSQL persistence on PostgreSQL 17, `temporalio-sdk` 1.1.2, CLI 1.10.0 for local runs):

| Probe | Holds when |
|---|---|
| One logical effect, repeated activity attempts | The effect runs once under its key; a retry is the same request |
| Signal before wait | A signal sent before the workflow waits is consumed once, when it waits |
| Timer against revocation | A grant revoked while a timer sleeps stops the effect after it fires |
| Decision against timeout | A decision and a deadline arriving together end in one outcome |
| Payload codec | What Temporal stores is encrypted by Ferrum's codec; history holds references, not records |
| Replay after a worker change | A changed interpreter is caught by replaying recorded histories before it runs |
| Continue-as-new | Unconsumed signals and effect identities carry over, none twice |
| Restore out of step | Ferrum's database and Temporal's restored to different times: no effect is sent again before reconciliation |
| Footprint | Server memory and disk at rest and under a small load; compile time and size the SDK adds |

### Results, 8 November 2026: go

Every probe holds ([../plan/spike-l01-temporal.md](../plan/spike-l01-temporal.md), log in
[evidence/l01-spike-run.txt](evidence/l01-spike-run.txt)), against Temporal server 1.32.2 on PostgreSQL
17.9 with `temporalio-sdk` 1.1.2. W3 builds on Temporal under five rules the spike found: each activity's
id is its node activation's id (without it a reordered interpreter replays unnoticed); one codec on
every client, worker and replayer; signals addressed to the workflow, never a run; effects keyed and
receipted by Ferrum before anything is sent; a restore of Ferrum's database raises the recovery epoch,
and effects predating it wait for reconciliation. Not shown: high availability, TLS, online backup,
capacity, and Temporal's own statement of PostgreSQL 17 support.
