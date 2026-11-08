# Closing the registry's open rows (from 8 November 2026)

When W8 landed, 47 of the Logic Platform's 124 rows were not verified. Many still said "comes with W3" or "no run
history exists yet", written in W0 and W1 and not revisited when later gates built those things. Others were real
gaps against the agreed plan. Each row was read again against its requirement and its acceptance case
(`docs/logic/catalog.json`), and against the code. A row moves only on evidence: a test that fails when the
guarantee is removed, and a mutant per new invariant.

## Batch 1: identity, recovery, versions, profiles, the boundary and release gates

**Company-owned runs act as their solution's own account (FLC-CAP-005, FLC-SEC-001).**
- *Gap:* a run nobody's act started acted with the rights of the administrator who activated its release, which
  is the admin fallback the plan ruled out.
- *Fix:* each solution now has its own service account, `solution.{id}@services.ferrum.invalid`.
  - It is given, in the activation's own transaction, exactly the rights of the capabilities its release was
    reviewed with: `Entity:action` rights on its role, and the roles single-role transitions need.
  - An administrator's capability (mail) is allowed at each step by that review, never by making the account an
    administrator. The flag travels as a crate-private argument, so no caller can claim it.
  - It is turned off when the solution stops. A solution activated before this gets its account from its active
    release the first time a run needs it.
- *Evidence (process_it):*
  - A run started by the platform acts as the account, with the activating administrator offboarded first.
  - The account's role holds exactly `Order: write`, it is not an administrator, it is refused a capability its
    release does not use, and it goes off with the solution.
  - Mutants caught: `company-run-acts-as-its-activator`, `solution-account-holds-more`,
    `admin-capability-for-anyone`, and `effect-borrows-rights` (retargeted).

**Unknown outcomes are looked up by receipt (FLC-UI-008, FLC-CAP-007).**
- *Gap:* nothing produced an unknown outcome for the runtime's own capabilities, because receipts settle repeats.
  When a worker's answer is lost after the effect commits, the run stops for a person, and the console had no
  capability-specific way to settle it.
- *Fix:*
  - `look_up_logic_effects` (`POST /tasks/{id}/look-up`) reads each doubtful step's receipt under its own effect
    key, keeps what it found on the recovery item and audits it.
  - The console shows it (Ferrum-Frontend #59).
- *Evidence (process_it):* a step whose answer was lost after the order committed:
  - The run stops. The lookup finds the receipt, and another kernel reads the same evidence and the same run.
  - Resumed, the step repeats under its key: one order, and its outcome known.
  - Mutant caught: `look-up-misses-the-receipt`.

**A replaced capability version is waited out (FLC-CAP-004).** process_it replaces the `procs` package's version
while a run waits:
- The step neither adopts the new version nor fails, and the attention item names the version it is bound to.
- Back on its version, it is done once.
- Mutant caught: `replaced-version-taken-up`.

**Core refuses a durable process (FLC-BASE-005).** On an installation without the process runtime, logic_rules_it
keeps nothing and says the source needs the Full Process profile. Mutant caught: `core-runs-a-process`.

**The simulator decides as the runtime does (FLC-UI-007).** process_it runs requests at 1000 and 1000.01 for real,
then simulates each with what the real run read standing in:
- The decisions agree, proceed and review.
- The simulation labels the stand-in.
- The same check on the deployed build is part of the final walk.

**The boundary (FLC-SEC-004, FLC-SEC-005).**
- *Plugins:* an endless guest and a guest held in a slow host call, side by side, each stop at their own budget.
  The slow call is dropped and never completes, and a guest started meanwhile finishes without waiting.
- *Outbound calls,* at a real connection:
  - A host resolving to loopback is refused before anything is sent.
  - A permitted host's redirect to a private endpoint comes back unfollowed, the endpoint never reached.
  - Mutants caught: `egress-follows-redirects`, `egress-reaches-private-hosts`.

**Release gates (FLC-QUAL-001, FLC-BASE-006, FLC-QUAL-007).**
- `scripts/release_evidence.py qualified`:
  - Compares each qualification's recorded build with the build at hand: a digest of Ferrum's code, and the commit
    of the frontend and of every repository Cargo.lock builds.
  - When anything changed, it names it and fails until the scripts are run again and `--stamp` records the build.
  - The profile qualification is stamped at `0e8e898`, the build the six profiles ran on. It reads stale now, as it
    should, since this batch changed Ferrum's code; it is run again at the end of these batches.
- `scripts/licenses.py check` now also fails on any deploy image that is not an engine approved in
  `docs/registry/engines.yaml`. It found Moto in the kind cluster unrecorded; it is recorded as test-only.
  - It also accepts crates that name a licence file only while the file still reads as the licence it was read as.
    The Temporal SDK's crates had been refused since W3 for declaring none, and GitHub's CI runs this check; local
    CI did not until now.
- `scripts/registry.py` self-tests that a simulator-only claim, a reviewer not on record, a missing digest or a
  blocker each keep a row from being called qualified.

**Read again, already evidenced.** These rows' limits were stale; their evidence already existed or is cited now:
- FLC-CAP-001: catalog_it, plus the W4 walk.
- FLC-CAP-006: process_it now also asserts the run waits for the delivery after the order.
- FLC-SEC-006: no result cache across viewers.
- FLC-SEC-008: holds, waits, stale approvals and lost rights all stop work.
- FLC-BASE-003: the reuse map is confirmed area by area.
- FLC-OPS-006, replay after expiry: an old event delivered again after its receipt expired starts nothing.

## Checks for batch 1

All local, at the head that merged:
- Tests: ferrum-logic lib 118; ferrum-kernel `process_it`, `automation_mail_it`, `recovery_it`, `logic_rules_it`,
  `capability_it`, `invariants_it`, `events_it`; ferrum-studio 29; ferrum-process 3 (Temporal 1.32.2);
  `mcp_it`; plugins `platform_it` and the runtime tests (4). All passed.
- `cargo clippy` over the workspace with all targets, and the server with Studio and processes: clean. fmt: clean.
- Mutants: the nine above were each caught; `mutants.py --check` finds the text of all 311.
- Scripts: `registry.py check` passes. `licenses.py check`: 998 packages and 11 engines, none refused.
  `release_evidence.py qualified` reports the stale qualification as intended.
- Ferrum-Frontend #59: 504 tests, `tsc` and the app build pass.

## Batch 2: privacy, evidence, references, retirement, support, contraction, workers

**Erasure reaches what runs keep (FLC-OPS-006).** Erasing a person's data now also takes the erased fields out
of the copies runs keep of each erased record: the event that started a run, the run's own state, the material
shown and the signals waiting. The erasure is kept (`logic_erasure`, migration 20261108000007), and
`process_report` takes the fields out again when a runtime reports a run from a history taken before.
- *Evidence (process_it):* a request's contact address leaves its run's trigger, state and task material.
- The test runtime then reports its own pre-erasure copy, and the address stays gone.
- Mutants caught: `erasure-misses-run-copies`, `report-brings-erased-back`.

**Task evidence (FLC-HUMAN-006).** Files attach to an open task (`POST /tasks/{id}/evidence`), are scanned, and are
kept with that task alone. The task's own page lists them (Ferrum-Frontend #60).
- *Evidence (process_it):* another reviewer and an administrator read the file.
- It is not reached through another task, by the requester, or by the ordinary file route, and an outsider
  cannot attach.
- Mutants caught: `evidence-crosses-tasks`, `evidence-for-anyone`.

**Typed references (FLC-COMP-002).** Record ids and link fields are typed by the kind of record they name
(`x-ferrum-link` in capability schemas, `Ty::Ref` in the compiler).
- A release passing the id of the request that started it where an order's id is wanted is refused when kept,
  "a reference to a Request" (process_it).
- Every existing source still compiles.
- Mutant caught: `references-of-any-kind`.

**Retiring a capability (FLC-CAP-008).**
- `logic_dependents` (`GET /dependents?package=`) lists the active releases bound to an app's capabilities, with
  their open runs.
- Taking the app out records them in the audit trail.
- An action's contract may say it is `deprecated` and what replaces it; checking a release that uses it says so,
  and it still compiles.
- Mutants caught: `dependents-overlooked`, `removal-forgets-what-waits`, `deprecation-unsaid`.

**Deny precedence (FLC-SEC-002).** A compiler test now holds the rule that a customer's rule may not write a field
its app owns, nor one of the platform's. Mutant caught: `rule-writes-the-apps-field`.

**Secrets never reach runs (FLC-SEC-003).** A request with a secret starts a run. The secret, stored sealed apart,
appears in none of the run's trigger, state or material, nor in a pack.

**Partner diagnostics over runs (FLC-SEC-007).** The support report lists, from `diagnostics` up, the runs
waiting for a person and their open steps, without trigger, state, material or evidence. A support key opens no
run. Mutant caught: `support-report-shows-payloads`.

**The older CEL reads no clock (FLC-COMP-003).** Workflows' and automations' expressions take the workspace's time
from their caller, as the kernel always gave it, and have none otherwise. Mutant caught:
`older-cel-reads-the-clock`.

**A backfill under a running release (FLC-MIG-005).** backfill_it activates a release whose rule reads the old
field while the backfill is stopped half-way by a crash. It checks saves then and after the backfill resumed and
finished, the same activation throughout.

**A paused worker (FLC-PROC-012).** process_it holds one worker inside an order's effect, its receipt claimed,
while a second waits behind the claim.
- The first's database session is ended, as a database ends one left idle in a transaction.
- The second does the order, and the first, resumed, commits nothing.
- Asked again, the gate answers with the order that was made.

## Batch 3: the apps' rows

Edge, Intelligence and Metrics each merged their part, and Ferrum pins them: IoT Edge 96b4eb4 (#15), Intelligence
6ea3c51 (0.2.0, #3), Metrics 848c2a3 (0.6.0, #5).

**IoT Edge.**
- *A flow's lookups have an age (FLC-EDGE-008).* `enrich` takes `fresh`: a value seen longer ago, or at no known
  time, is left out. A site past its offline lease starts nothing new and says why (`recovery_it`, `dispatch_it`).
  Mutant caught: `enrich-ignores-age`.
- *Short of disk (FLC-EDGE-011).* `recovery_it` starves a site of disk. No plain sample reaches the platform while
  it is short, and given room again it takes up the command that waited. Mutant caught:
  `short-of-disk-keeps-samples`.
- *No step moves equipment (FLC-EDGE-012).* A write step, a set step naming a device and a math step writing to one
  are each refused when the flow is read. Mutant caught, run by hand against Edge 96b4eb4:
  `steps-take-unknown-fields`.
- *No formula reads a clock (FLC-COMP-003).* The gateway's formulas refuse `now()`, `rand()` and `today()`.
- *Authenticated observation (FLC-EDGE-001)* was already shown, and the row now says so.
  - `ops_it` stores nothing a gateway sends about another site's device, and refuses a request signed by another
    key, replayed or altered.
  - `conditions_it` keeps another site's gateway from reporting a condition about this site's device.
  - Mutants already in the list: `ops-other-site-measurements`, `ops-signature-unchecked`, `ops-replay-accepted`.
- *Distributed owner (FLC-EDGE-009).* `ops_it` overlaps an old and a new connection to one device. The old gateway's
  close ends nothing of the new connection, and its journal replayed late does not take it back. At the gateway,
  a fence or a new epoch during a batch stops what is left (`dispatch_it`). New mutants caught:
  `ops-old-close-ends-the-new`, `ops-late-open-takes-it-back`.

**Intelligence.**
- *Bounded loop (FLC-AI-005).* At most three tries to understand a question, each failure repaired, within 20
  seconds that cancel the work and end in a plain answer. Mutants caught: `tries-unbounded`, `no-time-budget`.
- *Memory governance (FLC-AI-007).* An example is used only once someone who did not propose it accepts it, only in
  its own workspace, and only for those who may read its metric. Mutants caught: `example-used-unaccepted`,
  `example-accepted-by-proposer`, `example-widens-reach`.
- *Model changes (FLC-AI-008).* A provider answering confidently but wrongly fails the fixed and adversarial sets,
  and the rules keep answering. Mutant caught: `failed-model-promoted`.
- *Stale answers (FLC-SEM-007, -009).* An answer is marked stale once its metric's certified definition changes.
  Mutant caught: `old-answer-never-stale`.

**Metrics.**
- *Currencies and fiscal time (FLC-SEM-004, FLC-QUAL-005).* A metric summing amounts in several currencies converts
  each row at the rate of its own day, keeps products exact and rounds each total half to even.
  - How it converts is certified with its definition.
  - Series go by fiscal quarters and years.
  - `currency_it`'s expected numbers are worked out by hand in its header: four currencies, a rate on its first
    day, an inverse rate, a refund dated back across a fiscal year, ties to the even digit, a sale with two
    matching lines counted once, and a missing rate.
  - Mutants caught: `rate-strictly-before`, `first-rate-ever`, `rounding-half-up`, `refund-sign-lost`,
    `inverse-not-inverted`, `missing-rate-read-as-zero`, `fiscal-start-ignored`, `fiscal-buckets-calendar`,
    `conversion-outside-fingerprint`, `currencies-mixed-in-formula`, `fan-out-unchecked`, `cut-short-summed`.
- *Kept values (FLC-SEM-008, -009).* A kept value keeps what the metric meant when it was read. It is its reader's
  alone: authors and stewards could read everyone's, an administrator's all-rows numbers among them, and no
  longer do. Mutants caught: `snapshot-forgets-meaning`, `steward-reads-others-values`,
  `author-reads-others-values`.
- *A Wren model reviewed, never a second authority (FLC-SEM-001, -010).* `POST /api/app/metrics/import/wren`
  sorts each measure into new, same, duplicate, conflict or unsupported.
  - A conflicting revenue stays exactly as certified; the import comes in only under another name, as a draft.
  - Anything other than one aggregate of one column is refused with its reason.
  - Mutants caught: `review-hides-conflict`, `duplicate-defined-twice`, `unknown-aggregate-approximated`,
    `calculated-column-taken`, `import-without-rights`.

**A metric read the same everywhere (Objectives 0.7.0, #8, f8e0444).** Objectives measured a key result by a
certified metric by running the metric's query itself. A converted metric would have measured it as a sum of the
amounts as they are, and a composed one could not measure it at all.
- It now asks Metrics for the value, with the key result's owner's rights, as Intelligence does.
- When Metrics has no value, nothing is measured that day, and Metrics' reason stays on the key result.
- Its `metrics_it` measures key results by a converted metric (8000 INR as 100 USD) and a composed one. Mutant
  caught: `metric-definition-run-here`.

The app mutants were run with a script that restored each file and then touched it. A restored file keeps its
copy's older time, so cargo would otherwise have taken the last mutated build for current.

## Checks for batch 3

All local, against Ferrum's main:
- IoT Edge (#15): contract 120, gateway lib 133, `critical_it`, `recovery_it` (three runs) and the app's tests all
  passed. Clippy with failpoints and fmt: clean.
- Intelligence (#3): through `scripts/app_ci.sh`, fmt, clippy, its unit test and `evals_it` passed. The interface step
  does not apply: it has no `ui/`.
- Metrics (#5): `scripts/app_ci.sh` passed: fmt, clippy, the unit tests (8), `composed_it`, `currency_it`,
  `datasets_it`, `metrics_it` and `wren_it`, and the interface's build check, typecheck and tests.
- Objectives (#8): `scripts/app_ci.sh` passed: fmt, clippy, the unit tests (4), `metrics_it`, `objectives_it`
  and the interface's check and tests.
- Ferrum:
  - `cargo metadata --locked` resolves the new pins, and the server builds with them and the process runtime;
  - `mutants.py --check` finds the text of all 324;
  - `registry.py check` and `licenses.py check` pass, the latter with 998 packages and 11 engines, none refused.
- The six profiles, the workload and the drill ran again on that build, at every pin above. All 75 walk checks
  passed, every workload bound held and the drill held (lrp-profiles.md). The qualification is stamped there.

## Batch 4: Metrics' engine for external sources, and what GitHub's checks had found

DataFusion had been recorded as blocked on this machine's disk. Built without debug information, Wren's engine
and DataFusion take 1.5 GB, so the plan's W6 item was built as it describes (ADR L-07): an isolated service.

**Metrics' engine (Metrics 0.7.0, #6, 624e1a5; FLC-SEM-005, -006, -007, -010).** `ferrum-metrics-engine`, in the
Metrics repository, plans a metric's query through its Wren semantic model with Wren's own engine
(`wren-semantic-core` 0.5.1, pinned) and runs it on DataFusion 55 over the operator's CSV and Parquet datasets.
- *Rights in the scan (FLC-SEM-005).* A Data Steward describes each dataset as an External Source: the rows each
  reader sees and the columns only some roles see. Metrics asks the engine for one reader at a time.
  - The engine reads each dataset through a table that filters its rows and leaves out those columns in its own
    scan. The dataset's files are deregistered once read, so no query can name them.
  - Tested through a relationship and through a calculated column over a hidden column.
  - Mutants caught: `rows-filter-skipped`, `hidden-columns-kept`, `no-values-every-row`, `every-row-for-everyone`,
    `hidden-for-no-one`.
  - *Found by walking the KPI page (Metrics 0.7.2, #8).* A value in a dataset that did not parse came back to the
    reader with its whole raw row, since the file is parsed before a reader's rules apply: another person's row,
    hidden columns included. A dataset that cannot be read is now refused as unavailable without the error's
    text, which goes to the engine's log. Neither the engine's address nor a dataset's paths reach a reader.
    Mutants `dataset-paths-told`, `unreadable-as-query-fault`, `parse-errors-told` and `engine-address-told`
    caught.
- *The sandbox (FLC-SEM-006).* One SELECT over the model's own models, with DDL, DML and statements disabled.
  - A dataset named directly, a file path, a URL, `SET`, `COPY` and several statements are refused.
  - Memory is bounded with nothing spilled: a million-group aggregation is refused at 1 MB. Time, rows and
    concurrency are bounded too, and the token is checked.
  - Mutants caught: `models-only-unchecked`, `statements-unchecked`, `memory-unbounded`, `time-unbounded`,
    `rows-uncapped`, `token-unchecked`.
  - *Found once the local CI ran the engine's tests as its workflow does (Metrics 0.7.1, #7).* A query of 3,329
    terms took over a minute to plan in a debug build, and the timeout fired 18 seconds late, since planning
    cannot be stopped midway. DataFusion's own recursion protection was off, so a deeper query could have
    overflowed a thread's stack and taken the process down. Now:
    - before anything is planned, a query's terms nest at most 100 deep and it reads at most 32 models, counting
      each time; a model's calculated columns and relationships are held to the same depth, and a semantic
      model to 32 models (it held 200), so the longest chain of relationships through one is planned at once;
    - each of the engine's threads has a 64 MiB stack, and DataFusion's recursion grows its stack as it needs;
    - a query's time is kept whether or not its work yields. GitHub's faster runner had answered a query a 1 ms
      limit should have stopped, since a timeout fires only when the work yields. Each query now runs as a
      task of its own, and the caller is refused once its wait and its time are up. The task keeps its turn
      until it has stopped, and one that panics is refused, not the engine lost.
    - The test runs on a thread with the engine's own stack: 2,000 values in one list and 90 terms are answered;
      101 terms, the deepest query of the largest size (9,985 terms), 33 models read and a deep calculated
      column are refused. A chain of 32 models, each read through the next's relationship, is answered
      exactly within 5 seconds in a debug build, and 33 are refused.
    - Wren's SQL for a long chain nests a subquery per relationship, so it may be parsed deeper (512) than the
      reader's own query (50).
    - Mutants caught: `query-depth-unbounded`, `query-reads-unbounded`, `model-expressions-unshaped`,
      `relationship-conditions-unshaped`, `models-unbounded`, `time-unbounded`, `planned-sql-held-to-readers-depth`;
      on the caller's bound, `caller-waits-for-the-work` and `work-not-stopped`, driven by work that never yields.
    - One survives: `caller-waits-on-unyielding-work`, which awaits the query in the caller. Every query the
      engine takes yields, so the time inside the task already stops it. The task bound guards against work
      that does not.
- *Freshness (FLC-SEM-007).* Every answer says when each dataset was last written. A value from data older than
  its source allows carries it under `stale`, and so does a composed metric over it and one of the records. An
  Intelligence answer says so in its `limits` (Intelligence 0.2.1, #4, a0056b3). With the engine gone, there is
  no value, said so. The KPI page says it too: it names the stale sources, and says when a metric has no value
  now, with the reason, instead of seeming to load (Metrics 0.7.1, #7; without repeating its name, 0.7.2).
  - Mutants caught: `written-at-unsaid`, `stale-never-said`, `stale-answer-unsaid`, `stale-limits-lost`.
- *Interoperability (FLC-SEM-010).* Wren's models answer exactly against hand-worked fixtures, calculated
  columns, CASE and relationships among them. An unknown function, `TOP` and a missing column fail explicitly; a
  correlated subquery is answered exactly or refused.
  - Mutants caught: `any-case-ignored`, `other-workspace-dataset`, `external-outside-fingerprint`,
    `external-unchecked-at-save`.
- *On the assembled build.* The engine binary ran as a process of its own beside the walk's server, which reached
  it at `FERRUM_METRICS_ENGINE_URL` with its token. LRP-05 now reads a metric over the lake as people signed in
  as themselves ([lrp-walk.txt](lrp-walk.txt)):
  - a steward described the lake's orders, and a reader could not;
  - a query that would drop a table was refused as it was saved;
  - each region's reader read their own region's orders alone, and a reader of no region read no rows;
  - with the lake's file two hours old, the value said it was stale, and so did an Intelligence answer's limits.
- *Deployment.* `deploy/metrics-engine.yml` adds the engine beside the stack with Compose. The new
  `deploy/k8s/metrics-engine` component runs two replicas on Kubernetes:
  - read-only, unprivileged, with a disruption budget;
  - only the API may call them, and they call nothing;
  - it points Ferrum at them and opens Ferrum's network policy to them.
  - Both images are approved in `docs/registry/engines.yaml`. `docs/operations.md` has a section for it; the
    Metrics repository has `docs/engine.md` and `engine/Dockerfile`.
- *The image.* `engine/Dockerfile` had never been built. Metrics' CI now builds it and runs it read-only and
  unprivileged, then:
  - asks it as Metrics does: the North's rows alone, nothing without the token, a change refused;
  - checks Docker's health check and a clean SIGTERM;
  - from main, publishes it as `ghcr.io/cbferrum/ferrum-metrics-engine` (Metrics #7).
- All 490 packages of the Metrics workspace pass `scripts/licenses.py`'s policy. DataFusion, Arrow, Parquet,
  sqlparser and Wren are Apache-2.0.

**What GitHub's checks had found.** Ferrum's own checks run here before each merge. Two of GitHub's jobs had
failed on main since W7 and W8, and the checks run here had covered neither.
- *audit:* `scripts/check_apps.py` wants each app's README to have a changelog row for its version. Edge (0.6.0),
  Intelligence and Metrics had none. They now do: Edge 0.6.1 (#16, 867bca9), Intelligence 0.2.1, Metrics 0.7.0.
  It now reads 53 apps, none short of the contract.
- *image:* the smoke test's backup failed with `tar: .: file changed as we read it`. The erasure journal is
  written by the outbox's worker after the erasure commits, and it was landing in the files while `tar` read
  them. The test now waits for the journal, which the backup must hold for `reapply_erasures` to work, and
  takes tar's status 1, a file changed while read, as a backup of a running server. `docs/operations.md` says
  the same to operators.
- From now on, `check_apps.py` runs here with the other audit scripts.

## Checks for batch 4

- Metrics (#6): the workflow's steps, run here one test binary at a time:
  - fmt;
  - clippy over the workspace;
  - Metrics' unit tests (10), `composed_it`, `currency_it`, `datasets_it`, `external_it`, `metrics_it`, `wren_it`;
  - the engine's unit test, `engine_it` (7) and `http_it`;
  - the interface: dist matches its sources, typecheck, tests.
- Metrics (#7): `scripts/app_ci.sh` passed, with the workflow's stack:
  - fmt;
  - clippy over the workspace;
  - Metrics' unit tests (10), `composed_it`, `currency_it`, `datasets_it`, `external_it`, `metrics_it`, `wren_it`;
  - the engine's unit tests (2), `engine_it` (8) and `http_it`;
  - the interface: dist matches its sources, typecheck, tests (3).
  - Nine new mutants were caught, and one survived, as above. All 490 packages pass the licence policy.
  - GitHub's run of the PR built, ran and asked the engine's image (`engine-image`), which this disk cannot.
- Metrics (#8): `scripts/app_ci.sh` passed (`engine_it` 9, interface tests 4); its four mutants were caught;
  `external_it` passed ten runs in a row and on a reused database. GitHub's run built, ran and asked the image.
- Intelligence (#4): fmt, clippy, its unit tests (2) and `evals_it`, against Metrics 0.7.0.
- Edge (#16): no code changed; its app library tests (22) and interface check passed.
- Ferrum:
  - the server's `journeys_it` (1) and `extensions_it` (1), which batch 3 could not fit on this disk;
  - the server built with every default app and the process runtime, on the pins above;
  - the profiles, the workload and the restore drill on that build ([lrp-profiles.md](lrp-profiles.md));
  - `cargo metadata --locked`, `registry.py check`, `check_apps.py`, `licenses.py check` (13 engines),
    `mutants.py --check`, the other audit scripts, and the k8s manifests rendered with the engine's component.

## After batch 4

- GitHub's checks on main passed for batch 4 (Ferrum #116, `0339fe0`): rust, saml, editors, audit, image (its
  smoke test with the attachments backup fixed) and promote. The cluster deploy to kind then passed, for the
  first time since W7.
- Metrics' engine image is public: CBFerrum's package policy now allows public packages, at the owner's
  decision, and `ghcr.io/cbferrum/ferrum-metrics-engine` pulls without credentials.
- **FLC-QUAL-008 is verified.** The repository owner, Chinmoy Bhuyan, took on all four profiles, their
  supported versions, incidents and retirements on 9 November 2026. [maintenance.md](../maintenance.md)
  records this, with retiring Temporal, Metrics' engine or a dataset, a model provider and an app release
  worked through as a tabletop. Support has one owner and no funded SLA (BLOCK-LTS-SUPPORT stays for that).

## Still open

123 of the 124 rows are verified. The last waits on people: a screen-reader audit with assistive-technology
users (FLC-UI-004, BLOCK-ACCESSIBILITY-AUDIT).

What software could add has been added (Frontend #61, `58d2786`, pinned here). A virtual screen reader
(Guidepup's) reads the guided editor, the task page and the run page from their accessibility trees, item by
item.
- It checks that every control has a name, that a task's subject is read before its decision buttons, that a
  run's refusal comes before what went well, and that the reason a button is off is read before the button.
- It found that a field the form requires was spoken without "required". The kit's Field hid its star from
  assistive technology and never told the control, nor tied its error or hint to it. Every field now tells
  its control (`aria-required`, `aria-invalid`, `aria-describedby`); without the fix, the test fails.
- Apps' own interfaces take the fix when they are next built against the kit.
