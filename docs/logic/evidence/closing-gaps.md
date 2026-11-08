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

## Still open after batch 1

- **Next Ferrum batch:**
  - task evidence attachments (FLC-HUMAN-006);
  - capability retirement with dependents (FLC-CAP-008);
  - erasure reaching run copies (FLC-OPS-006);
  - contraction under budgets (FLC-MIG-005);
  - a worker paused past its timeout (FLC-PROC-012);
  - typed references (FLC-COMP-002);
  - the older CEL's clock (FLC-COMP-003);
  - deny precedence, secret handles and diagnostic access over runs (FLC-SEC-002, -003, -007).
- **The apps' rows:**
  - Edge: FLC-EDGE-001, -008, -009, -011, -012;
  - Metrics: FLC-SEM-001, -004, -005, -006, -007, -008, -009, FLC-QUAL-005;
  - Intelligence: FLC-AI-005, -007, -008.
- **Then:** the six profiles, the workload and the drill run again on one assembled build, and the qualification
  stamped there.
- **External, recorded as blockers:**
  - a screen-reader audit with assistive-technology users (FLC-UI-004);
  - the DataFusion build, which needs more disk than this machine has (FLC-SEM-010);
  - named maintenance owners (FLC-QUAL-008).
