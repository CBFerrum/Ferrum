# The Logic reference profiles on one assembled build (8 November 2026)

The package's exit (MASTER_PLAN, "Reference profiles") asks for six profiles, LRP-01 to LRP-06, on one
assembled build with two workspaces and ordinary roles. They ran so, through three scripts that drive the
running server over HTTP as people signed in as themselves:
- [`scripts/lrp_walk.py`](../../../scripts/lrp_walk.py), the profiles;
- [`scripts/logic_load.py`](../../../scripts/logic_load.py), a measured workload and fairness;
- [`scripts/restore_drill.py`](../../../scripts/restore_drill.py), a recovery into a new, isolated environment.

Their output is kept beside this page: [lrp-walk.txt](lrp-walk.txt), [logic-load.txt](logic-load.txt) and
[restore-drill.txt](restore-drill.txt).

They first ran when W8 landed, at Ferrum `edbc786`. They ran again once the registry's open rows were closed
([closing-gaps.md](closing-gaps.md), batch 3), on the build below. This page and the three outputs are from that
second run.

## The build and what it was pinned to

- **Server:** `cargo build -p ferrum-server --features process`, which is every default app (the countries,
  the business apps, Operations, Edge, Metrics, Intelligence, Studio, …) with the process runtime, at Ferrum
  `40ff6d7`.
- **Pins:**
  - Ferrum-Frontend `48907d8`;
  - ferrum-app-edge `96b4eb4`;
  - ferrum-app-metrics `848c2a3` (0.6.0);
  - ferrum-app-intelligence `6ea3c51` (0.2.0);
  - ferrum-app-objectives `f8e0444` (0.7.0);
  - the rest as `docs/registry/release-manifest.yaml` pins them.

  `scripts/release_evidence.py qualified` compares this build with the one at hand
  ([qualification.yaml](qualification.yaml)), and fails once any of them moves.
- **Runtime:** Temporal Server 1.32.2 (CLI 1.10.0 dev server, SQLite, namespace `ferrum-flc`) and PostgreSQL 17,
  on an 8-core Apple silicon Mac (macOS 15.6).
- **Workspaces:** `acme` and `globex`. The second was made with `ferrum task create_tenant ... packages:core,procs`,
  the Requests package being the walk's own small app (`ERP/flc/walk/packages/procs`).

## What each profile ran (all 75 checks passed)

**LRP-01, record policy.**
- An author wrote a rule in Studio and checked it.
- The author kept it, could not approve it, and could not have it activated by a requester. Another
  administrator approved it, and an administrator activated it.
- One invalid request was refused with the rule's message through the records API, the older resource API,
  import (a requester may not import at all; an administrator's import was refused too), bulk and MCP.
- The other workspace was not bound by it.
- Saves under the rule took 4 ms at the median and 7 ms at p95, over 20.

**The simulator's parity (FLC-UI-007), run before LRP-02.**
- A policy release with a decision table (a total over 1000 is reviewed) ran two requests on the runtime: 1000
  proceeded, and 1000.01 was reviewed.
- On the same deployed build, the simulator decided each again from what that real run had read, standing in for
  the read. It decided the same both times, and labelled the stand-in.
- The policy was then stopped, so the procurement profile's runs were its own.

**LRP-02, durable procurement.**
- A submitted request started one run on Temporal.
- The requester and someone without the role could not decide. Two reviewers deciding at the same moment:
  one decision counted, and the other reviewer was told.
- One order was made under its effect key, and the order's own event completed the run.

**LRP-03, generic condition response.**
- The walk played a site's gateway. It enrolled with a key of its own (the code worked once) and signed what
  it reported.
- A report of a bearing running hot, sent seven minutes after it began, made one condition record carrying the
  time it began at the site, and one process run across apps.
- The same report sent again, as after a gateway restart, changed nothing. A report with a forged signature was
  refused.
- A reviewer took the case. When the condition cleared at the site, the case resolved and the record closed.
- This walk found a real race, now fixed (see W8's evidence): an event and the wait for it arriving at the same
  moment both missed each other.

**LRP-04, partner solution pack.**
- The author downloaded the procurement release as a pack: no secret, and no reviewer's name.
- In the other workspace, a partner whose Solution Author grant ends tomorrow took it in. It arrived as a draft
  showing the calendar to map. The workspace made its own calendar of that name, and the draft then checked.
- The partner kept it, and could neither approve nor activate it; that workspace's administrators did both.
- It ran there as that workspace's own run, which the first workspace cannot see.
- No core change was needed.
- Uninstall retention: Edge was taken out of the first workspace and its records went out of reach; brought
  back, every record was there again.

**LRP-05, agentic BI.**
- An analyst defined a metric, could not certify it, and a steward did.
- A question became a plan, an answer and its evidence, read with the asker's own rights. A reader who cannot
  read requests got no value from them.
- Words asking it to delete records and ignore its instructions were noticed and not acted on. A question about
  nothing readable was declined with what can be asked.
- A series marked the month not yet over as partial.
- A proposal became only a draft, for review, and a reader could not propose.

**LRP-06, operator recovery.**
- The requester's rights were withdrawn while their run waited. The order was refused, nothing was made with
  rights no longer held, and the run stopped for a person.
- A reviewer could not recover it. An administrator resumed it in Studio with a reason, and the order was made,
  once.
- A worker or backend crash ran in `logic_load.py`: the server was stopped with 15 runs decided and in hand, and
  every one ordered exactly once after it came back.
- A restore mismatch ran in `restore_drill.py`, below.
- A failed compensation is the state machine's and runs in its tests: a refused undo stays open for a person.

## The workload, on the same build (logic-load.txt)

| Scenario | Measured |
|---|---|
| Compile a source, simulate a run (HTTP round trip included) | 31 ms, 31 ms (median of 10) |
| Cold: first run after start | review open 589 ms after submitting; order 452 ms after the decision |
| Warm, ten in turn, each workspace | review open 557–601 ms median; decided to ordered 233–341 ms median, 442–451 ms p95 |
| 60 at once in one workspace beside 5 in the other | the flood 1048 ms median, 2091 ms p95; the other workspace 420 ms median, 427 ms p95 (its quiet p95 was 442 ms); 65 runs in 5.1 s; 8 steps told to wait their turn |
| Server stopped with 15 runs in hand | all ordered once, settled 17.5 s after the restart began |

The worker's limits on that server were 10 slots, 8 for effects and 2 per workspace: half of a 20-connection
pool.

## The restore drill, into a new environment (restore-drill.txt)

The drill:
1. Took PostgreSQL before a run ordered, and Temporal's store after it.
2. Stopped the original server and Temporal.
3. Restored both into a new database (`ferrum_flc_drill`, dumped and restored as the database's superuser, as
   `docs/operations.md` says), a new Temporal on port 7234 and a new server on 5171.

What happened on the new environment:
- It held. Both runs read back with the same keys.
- An order asked for while held waited.
- A person released the hold with `ferrum task release_restore`. The waiting order was then made, once.
- The run whose history was ahead of the database was named for a person, as an attention item "No receipt here
  for procs.order.create". Nothing was ordered again for it by itself.

## Real and stood in

**Real:**
- the server binary, PostgreSQL, Temporal, every HTTP call, the Studio screens, and the signing and enrollment
  protocol;
- each workspace's data, and each person's rights.

**Stood in, and labelled so:**
- the gateway, played by the walk with an Ed25519 key of its own (real equipment and power cuts are
  BLOCK-INDUSTRIAL-HARDWARE; Edge's gateway runs with simulators and crash points in its own suites);
- no model was called (BLOCK-MODEL-KEY);
- no provider sandbox was reached.

**Still outside software:** independent security review, an accessibility audit with assistive-technology
users, and production Temporal operation.
