# W8: migration, release and operations (8 November 2026)

Software evidence only, run on the owner's development machine (macOS, PostgreSQL 17.9) from the worktrees
`ERP/flc/ferrum` (branch `logic/w8-operations`, based on Ferrum `53ecdf7`) and `ERP/flc/web` (branch
`logic/w8-operations`, merged as CBFerrum/Ferrum-Frontend#57, `dd70c28`). Temporal Server 1.32.2 ran as the
CLI's dev server (`temporal` 1.10.0, SQLite, namespace `ferrum-flc`). Each suite had a database of its own
(`ferrum_flc_kernel_test`, `ferrum_flc_studio_test`, `ferrum_flc_process_test`, …), reset by the suite.

## What ran

```bash
export CARGO_TARGET_DIR=$HOME/Desktop/Personal/ERP/flc/target CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo test -p ferrum-logic --lib
cargo test -p ferrum-kernel --test process_it --test automation_mail_it --test recovery_it --test logic_rules_it \
  --test capability_it --test invariants_it --test events_it
cargo test -p ferrum-studio
TEMPORAL_ADDRESS=http://127.0.0.1:7233 TEMPORAL_NAMESPACE=ferrum-flc cargo test -p ferrum-process
cargo test -p ferrum-mcp --test mcp_it; cargo test -p ferrum-plugins --test platform_it
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ferrum-server --no-default-features --features studio,process -- -D warnings
cargo fmt --all -- --check
python3 scripts/mutants.py <each W8 mutant>     # all caught, below
python3 scripts/mutants.py --check; python3 scripts/registry.py check
# Frontend
npx tsc --noEmit; npx vitest run; npm run build:apps
```

The numbers each run gave are in the pull request.

## What it showed

**Operations (from the first W8 commit).** While the platform is held after a restore, or fenced to another
region, a step that would change something waits and is tried again; reads and people's tasks go on; once the
hold is released it runs under the same receipt (process_it; mutant `effect-ignores-restore-hold`). `/metrics`
carries runs by state, the oldest start's wait, open and overdue tasks and open recovery items; they were read
on the walk server. `deploy/temporal.yml` passed `docker compose config`. `ferrum task logic_replay` replayed a
walk run's real history on the build ("replays on this build"), and reported a missing workflow as missing.

**From automations (FLC-MIG-001).** Each automation becomes a draft Solution, once, with its event, its
condition as written, its actions as capability steps and where it came from. Its business key is `event.id`,
so it starts one run per event, as the automation ran; an automation with no condition now compiles (it used
to carry an unreachable end). An automation imported before is not imported again (mutant
`automation-imported-twice`). Business keys may read `event.id` and `event.topic` besides `trigger` and
`parameters`.

**One writer at the cutover (FLC-MIG-003).** process_it saves an automation that notifies a person of each
change to a request, imports it, reviews and activates its release, and delivers six changes:
- C-1, run by the automation before the activation, is not run again by the solution.
- C-2 was seen by the solution's subscriber before the activation (nothing listened yet) and by the
  automation's runner after it, which hands it over and starts the solution's run.
- C-6 was delivered to both subscribers while the activation committed, concurrently.
- C-3, C-4 and C-5 came after the activation, delivered in either order or concurrently.
- Then every one of them was delivered again.

The person was told exactly once about each change. The automation was off from the same step as the
activation, and turning it back on was refused while the solution runs. Each run records the generation it was
accepted under. Mutants `cutover-runs-both`, `cutover-leaves-automation-on` and `run-forgets-its-generation`
are caught.

**Going back (FLC-MIG-006).** Release 1.1.0 (its orders say "Placed under 1.1.0") was activated over 1.0.0, a
request was approved and ordered under it, then 1.0.0 was activated again. The answer named 1.1.0 as replaced,
one run still open elsewhere, and that nothing a run did is undone. The next request ran under 1.0.0 at the
new generation, and ordered as 1.0.0 orders. The 1.1.0 run stayed under 1.1.0, still running, with its order
and its one receipt.

**Shadow comparison (FLC-MIG-002).** A candidate 1.2.0 with a higher review threshold and a status on its
orders was compared with what runs now over the last 50 runs, each replayed with the answers, decisions and
events it met. Every run of a big request showed "decisions" (review became proceed), and every run that
ordered showed "effects" with the new status. Not an order, a receipt, a release or a run came of it, and a
reviewer without authoring rights was refused. Before the imported automation took over, its draft compared
with the automation event by event: C-1, which the automation acted on, the solution acts on too. A run whose
order was refused for its requester's rights replays as stopping for attention, as it did: a refusal replays
as a refusal, an unknown outcome as unknown, and a person's recovery as decided. The first version replayed
every recorded output as a success; the walk on the real binary found it, through a run that had ended
refused and replayed as completed. Mutants `shadow-forgets-what-happened` and
`shadow-replays-refusal-as-success` are caught.

**A capability out of service (FLC-OPS-007).** A request was approved, and the app was taken out of the
workspace before the order step ran. The step did nothing and the run stayed running, not failed. An attention
item, seen by administrators only, said why and that cancelling the run stops it instead. Trying again while
the app was out changed nothing. Once the app was back, the step ran once and the item closed as resolved.
Mutant `revoked-capability-fails-the-step` is caught.

**Packs (FLC-OPS-008).** solutions_it downloads a release as a pack holding exactly its source, digests,
needs (the "office" calendar, the submitted-request event, two capabilities) and review decisions without
names. Another workspace cannot read it, but can take it in: it arrived as a draft only, with the missing
calendar to map. A pack whose source was changed after export (one step budget) was refused. Mutant
`pack-taken-in-changed` is caught.

**Moving a waiting run (FLC-MIG-004).** Release 1.3.0 has no "review": its task is "approve".
- A move planned with no mapping was refused ("review has no counterpart in 1.3.0"), and so was one mapping
  review to the order step ("waits as a task, and order is a step that does not wait"). Someone not an
  administrator could not plan, and the planner could not approve their own plan.
- Approved by a second administrator, the run moved: its open task, the same row, now under "approve". Once
  decided, it ordered as 1.3.0 orders, and the run's page lists the move as planned, approved and moved.
- A move approved for another run whose task was decided before the runtime took the move up was refused,
  with "review was approved meanwhile" on the run's record; that run ordered as it was going to.

The same move ran on Temporal (conformance_it): the workflow handed the move to the kernel and continued as new
under 1.1.0, the task moved with its step, and the run completed with the new release's order. Mutants
`move-approved-by-its-planner` and `move-over-a-decision` are caught.

**Studio (Frontend #57).** The release page says what an activation replaced and keeps, compares with what
runs now (cases that differ first), and downloads a pack. The solutions page takes in a pack and lists what to
map. A run's page plans and approves a move and lists every move. These were drawn in the screen tests with
the accessibility check, and walked on the real binary as recorded in [lrp-profiles.md](lrp-profiles.md).

## The package's acceptance cases

| Case | Where it ran |
|---|---|
| AT-OPS-004: effects after a restore | `process_it`: hold, a changing step waits, a read goes on, release, once under its receipt |
| AT-OPS-007: revoke while runs sleep | `process_it`: app taken out of the workspace while a run waits; the step waits, an attention item for administrators, goes on once back |
| AT-OPS-008: a pack to a differently bound tenant | `solutions_it`: a draft only, the missing calendar to map, no secret; a changed pack refused |
| AT-MIG-001: legacy import | `solutions_it`, `process_it` |
| AT-MIG-002: replay with a candidate that changes decisions | `process_it`: differences per run, nothing run; the walk (lrp-profiles.md) |
| AT-MIG-003: activate during concurrent delivery | `process_it`: events before, during and after the activation, every order, delivered again; each told once |
| AT-MIG-004: migrate a waiting run to a release with removed steps | `process_it`, `conformance_it` on Temporal, the walk |
| AT-MIG-006: roll back after an effect completed | `process_it`: new runs under the earlier release, the later run's order and receipt stay |
| AT-QUAL-003: mutants | each W8 mutant named above, all caught; `run-started-twice` follows its moved text |

## Not done here

- **Shadow:** there is no permission-aware sandbox against providers' test systems. A step the candidate adds
  has no recorded answer and is reported as such.
- **Moves:** a run inside a loop, parallel branches or a subprocess, or with an effect in hand, does not move.
  A task keeps the deadline it had.
- **Cutover timing:** a replica learns of an activation from PostgreSQL's notification. An event it passes in
  the instant before, for a solution new to that event, is not started there; for a solution imported from an
  automation, the automation's runner starts it instead.
- **Restore drills:** no drill restored PostgreSQL and Temporal's histories from different points in time into
  a new, isolated environment. The hold and the replay are what was shown.
- **Scheduling and load:** no per-workspace budget of runs or worker time exists, and no load or soak test of
  logic workloads was run (FLC-OPS-002, -003, FLC-QUAL-004).
- **External blockers:** production Temporal operation, independent security review and an accessibility
  audit with assistive-technology users are outside software, and remain open.
