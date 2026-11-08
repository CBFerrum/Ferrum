# The Logic reference profiles: what ran, where, and what did not (8 November 2026)

The package's exit (MASTER_PLAN, "Reference profiles") asks for six profiles, LRP-01 to LRP-06, on one assembled
build with two workspaces and ordinary roles. This page says, profile by profile, which parts were exercised,
in which kind of evidence, and which were not. It claims nothing beyond that.

The kinds of evidence:
- **Test**: a suite against a real PostgreSQL (and, for processes, Temporal's dev server), run on the owner's
  development machine. Each workstream's page has the commands and the numbers.
- **Walk**: the real `ferrum` binary in a browser or over HTTP, signed in as ordinary roles of a workspace.
- **Simulator**: Edge's gateway with simulated devices and named crash points. It is labelled as such, and is
  not hardware.

None of it is a production deployment, a live provider, a model, real equipment, an independent security
review, or an accessibility audit with assistive-technology users. Those stay blocked (BLOCK-*).

| Profile | Exercised | Kind | Not exercised |
|---|---|---|---|
| LRP-01 Record policy | Authored in Studio without source, checked, kept, reviewed by another, activated (W4 journey 1). One invalid record refused alike through insert, update, import, bulk, capability, automation, job, a plugin's `data.insert` and MCP; derivations in order; refusal evidence kept apart (W2 `logic_rules_it`, `platform_it`, `mcp_it`). | Walk, test | Latency was not measured under load (FLC-QUAL-004). |
| LRP-02 Durable procurement | Approval bound to what was shown, requester excluded; a record changed after approval refused at the effect; effects once under their key; an unknown outcome waiting for a person; rework and compensation newest first, a failed undo left open; signals before waits; replay with the codec, and on a new build (`logic_replay`) (W3 `process_it`, `conformance_it`, logic unit tests; W8). Requests approved and ordered on Temporal (W4 walk). | Test, walk | No live supplier or provider sandbox. The stock hold keeps the package's example name, as Ferrum has none (W2). |
| LRP-03 Generic condition response | Typed ingest, quality classes, exact normalization, event-time windows with lateness and idle sources, episodes with hysteresis and reopen, critical flows committing state and facts together across crash points, one Logic run per episode (W5 Edge `critical_it`, `conditions_it`). | Simulator, test | Real equipment and power cuts (BLOCK-INDUSTRIAL-HARDWARE). Offline scope and owner incarnation are as they were before W5. |
| LRP-04 Partner solution pack | A partner authors until their role ends (W4, `solutions_it`). A release leaves as a pack and arrives elsewhere as a draft with what to map, refused if changed on the way; nothing of the core is forked. Moving runs by an approved mapping. An app taken out of a workspace keeps its data and its runs wait, then go on once it is back (W8 `solutions_it`, `process_it`). | Test | Logical rebinding is done by editing the draft; no mapping assistant exists. |
| LRP-05 Agentic BI | Certified metrics read per person with their own rights, composed metrics, series with the current period partial, impact (W6 `composed_it`). Questions to typed requests, plan before reading, evidence, ask-back and decline, injected actions noticed and never obeyed, proposals as drafts (W7 `evals_it`). | Test | No model was called (BLOCK-MODEL-KEY). Intelligence has no dashboards, subscriptions or exports of its own; cancelling a long question does not apply to the rules provider. |
| LRP-06 Operator recovery | A worker away while a person decides, then another takes over (W3 `conformance_it`). Effects held after a restore until released (W8 `process_it`). Rights revoked while waiting refuse the effect. A failed compensation stays visible. Recovery by an ordinary administrator in Studio, resuming only where the process allows (W4 journey 7 walk). | Test, walk | No restore drill of PostgreSQL and Temporal's histories from different points into an isolated environment. |

## On one assembled build

One build ran the walk: `cargo build -p ferrum-server --features process`, which is every default app (the
countries, the business apps, Edge, Metrics, Intelligence, Studio, …) with the process runtime. It served
Ferrum-Frontend `dd70c28` on port 5170, with the database `ferrum_flc_walk` and Temporal's dev server (namespace
`ferrum-flc`). People signed in through the login page as ordinary roles of the workspace `acme`: two
administrators, a Solution Author, a reviewer and a requester. What it showed:

- **LRP-02 and LRP-04.** The requester submitted REQ-W8-1, and a run of `workspace.approval` 0.1.0 started on
  Temporal and waited for its review.
- **Compare (LRP-04).** admin2 opened release 0.3.0 and compared it with what runs now: 5 cases, 4 differing.
  This walk found a fault: a run that had ended refused replayed as completed. The replay now gives each step
  the refusal, unknown outcome or recovery it met (commit `b1745ae`, a `process_it` assertion, mutant
  `shadow-replays-refusal-as-success`). On the rebuilt binary, comparing 0.1.0 with 0.3.0 (by then running),
  that run replayed as refused on both sides, as it had ended, and came out the same.
- **Activate (LRP-04).** admin2 approved 0.3.0 and ran it instead of 0.1.0. The page said: new runs start under
  0.3.0 (generation 2), it replaced 0.1.0, the one run still open under 0.1.0 goes on under it, and nothing a
  run did is undone.
- **Move (LRP-04, LRP-06).** On the waiting run's page admin2 mapped review to `order_it`, and was refused:
  "review waits as a task, and order_it is a step that does not wait". Mapping review to review, admin2
  planned the move, and the page said another administrator approves it. The administrator approved it with a
  reason. Within seconds the runtime moved the run onto 0.3.0, and the record read planned, approved, moved.
  The review task, the same one, stayed open.
- **Continue (LRP-02).** The reviewer approved the task, and the run went on under 0.3.0's steps: the order step
  ran.
- **Packs (LRP-04).** The Solution Author downloaded 0.3.0 as a pack: its keys exactly the documented set, its
  needs listed, its review evidence without names. Taken back in, it arrived as a draft with nothing to map
  here. The reviewer, who does not author, was refused (403).

Not on this build: the Edge gateway and its simulators (LRP-03 ran on simulators in Edge's own suites);
Intelligence's questions (LRP-05 ran in `evals_it`); a restore (LRP-06's hold ran in `process_it`); a second
workspace taking in the pack (that ran in `solutions_it`).
