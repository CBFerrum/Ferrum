# Durable processes

A solution's processes run for hours or months: they wait for people, for events and for deadlines, and
do their work through business capabilities. This page is what a process may say, how a run behaves, and
how to run the Full Process profile. The compiler is [rules.md](rules.md); the requirements are FLC-PROC
and FLC-HUMAN in [traceability.md](traceability.md); the evidence is
[evidence/w3-processes.md](evidence/w3-processes.md).

| Where | What |
|---|---|
| `crates/logic/src/compile.rs`, `solution.rs` | The steps, and what the compiler refuses |
| `crates/logic/src/process.rs` | A run as a pure state machine: what happened in, what to do out |
| `crates/logic/src/calendar.rs` | Waits as elapsed time or business days, resolved to instants |
| `crates/kernel/src/process.rs`, migration `20261108000003_logic_process.sql` | Runs, tasks, decisions, waits, signals and calendars: Ferrum's records, and the activities a runtime calls |
| `crates/process` | The runtime on Temporal, and what starts and signals runs |
| `server/src/process.rs` | The server's switch: `TEMPORAL_ADDRESS`, with the `process` feature |

## Steps

A process is a graph of steps. A structured step holds blocks, each a graph of its own that ends at its own
`end` steps, so a join is never drawn by hand. Within a block the graph is acyclic; repetition is the repeat
step's. A step may read the output of a step that surely ran before it (`snapshot.output.total`), in its block
or the blocks around it, and nothing else.

| Step | Meaning | Bounds |
|---|---|---|
| `query`, `action` | A capability through the gate; an action's effect key is the same for every attempt at one activation. An action that can end unknown says where that goes (`on_unknown`); `approval` names the person's task it relies on | Only capabilities the solution `requires` |
| `decision` | A unique or first decision table; one route per outcome | |
| `human_task` | A person of a role decides, never the requester; `approvals` different people approve, one rejection rejects; it may be handed on `delegation` times; `material` is what the decision is bound to; a typed form | 1–9 approvals, 0–3 hand-ons, a timeout of at most a year |
| `wait_event` | An event of the catalog about the record `correlation` names | A timeout |
| `parallel` | `all`: every branch settles, one refused refuses the step. `any`: the first to complete wins, the others are asked to stop | 2–16 branches |
| `for_each` | Its body once per item, each named by a stable `key` from its data, never its position | 1–1000 items, 1–20 at once |
| `repeat` | Its body again while it ends `again` | 1–100 rounds, then `on_exhausted` |
| `subprocess` | Another process of the release, with its trigger given field by field; it reads nothing of its caller's | No recursion, direct or not |
| `case` | Its owner's role directs listed tasks, each once its dependencies are done; it closes once the required ones are | 1–32 tasks; no dependency circles |
| `compensate` | Undoes the run's actions newest first, each through its app's compensating action (the capability's `compensate_with`) | Each compensation must be in `requires` |
| `delay` | Waits a duration or business days | At most a year, or 260 business days |
| `end` | `completed` or `refused`, with what it hands back; `again` only in a repeat's body | |
| `suspend` | Waits for an authorized recovery; `resume_to` says where recovery may resume, and only where everything that step reads has run | |

No route from a failure, a timeout, an unknown outcome, running out of rounds or a recovery leads to a
completed end: the compiler refuses it. A process holds at most 128 steps, its blocks' included. Its
`max_steps` caps the steps one run takes; when the compiler sees every bound spent could pass it, it says so,
and a run that reaches it stops and waits for a person.

## A run

**Starting.** A process starts on an event of the catalog. The event is accepted once per business key (an
expression over the trigger), on the release active at that moment, in one transaction with the start it
queues. Activating a new version changes which release later events start, never a run already going. The
runtime starts the run under a workflow id made from the workspace and the run, so a start asked again finds
the run; one that ended is never started again.

**Effects.** Each step's work goes through the capability gate under the effect key `{run}/{activation}`: the
same on every attempt, so a retried step finds its receipt, and other input under the key is refused. It runs
with the rights of whoever started the run as they are when it runs; a run no person started acts with the
rights of whoever activated its release, as they are then. Revoked rights refuse the effect; nothing stronger
is borrowed. An effect that relies on an approval is refused if the approved record's version has changed since,
or the record is gone.

**People.** A task is decided on the material the person was shown (its digest); a decision on anything
else is refused, as is one by the requester, by someone without the role (an administrator included), or by
anyone but the person it was handed to. Decisions never change. A task is handed on only to someone holding its role, never back along its
chain or to the requester. A task's material is readable only by someone who may decide it, or an
administrator. Approved form values are checked against the form.

**Waiting.** A run waits for an event about one record. An event that comes before the run waits for it is
kept for seven days; an event about another record, or from another workspace, advances nothing. A
deadline is a duration, or business days on a workspace calendar: weekends and holidays skipped, the same
local time of day; a local time skipped by a clock change moves to the first time after it, one that happens
twice takes the earlier. Each deadline records the calendar, its revision, the zone and the zone data it was
worked out with: a revised calendar moves deadlines worked out after it, never those before. When a deadline
passes, the database says whether it or a decision came first, and the run hears that one outcome.

**Structure.** A race's loser is asked to stop, and what it had in flight is kept: an effect that settles
after its branch lost, or after the run was cancelled, is recorded and handed to a person. Compensation runs
newest first; one that fails stays an open item, and the run waits for a person, who can resume it to try what
is still open. A cancelled run closes its tasks and waits, and finishes once the work it had in hand settles.

**Recovery.** A suspended step is recovered by an administrator with a reason: resumed where its source
allows, or ended refused. Nothing marks it a success. What a run hands to a person (a late effect, a failed
compensation) is acknowledged by an administrator with what was done about it.

## The runtime

Temporal keeps each run's schedule; Ferrum keeps every business record. One workflow type loads the run's
release through an activity, so its bytes are in the history, feeds the state machine what happened, and
carries out what it asks as activities through the kernel. Each effect's activity id is its node's activation,
so a changed interpreter fails on replay instead of running a different effect (the spike's rule, decisions
L-01). Decisions, events, recoveries and cancellations reach a run through the outbox and Temporal signals,
and are handled once. Each says all it needs (a case's close carries every finished task's result), so one
that overtakes another still counts; a delivery to a run that has ended is dropped. Activities are retried until they answer, so a database or dependency away for
an hour stalls a run but never ends it; an effect retried finds its receipt. A run continues as new after 500 events, or sooner when Temporal suggests it,
carrying its state, deadlines and undelivered signals.

Everything Temporal stores is sealed with `FERRUM_SECRET_KEY` before it leaves the server, and opened with
it or a key in `FERRUM_RETIRED_SECRET_KEYS`. After rotating the key, keep the old one retired until every run
that started before the rotation has finished and Temporal has removed its history.

### Running the Full Process profile

| Setting | Value |
|---|---|
| `TEMPORAL_ADDRESS` | Temporal's frontend, `http://temporal:7233`. Unset, processes are refused (`unsupported_profile`) |
| `TEMPORAL_NAMESPACE` | Default `ferrum`. Create it first: `temporal operator namespace create --namespace ferrum` |
| `TEMPORAL_TASK_QUEUE` | Default `ferrum-processes` |

The server must be built with the `process` feature; the release image is. Replicas that run background work
dispatch the outbox, which starts and signals runs, and work the runs. Temporal Server 1.32.2 is the
version qualified (the spike, on PostgreSQL 17; the conformance test, on its dev server). A compose file for
Temporal comes with W8.

## Not yet

* The Studio endpoints, the run console and the recovery screens: W4. Today these are kernel functions
  (`decide_logic_task`, `delegate_logic_task`, `case_task_done`, `close_case`, `recover_logic_task`,
  `acknowledge_logic_attention`, `cancel_logic_run`, `logic_runs`, `set_logic_calendar`).
* A live stream of a run's status (FLC-PROC-014): runs are read as they stand.
* Attachments as a task's evidence (FLC-HUMAN-006): forms take typed values only.
* Replaying open runs' histories against a new worker build before it takes over: `ferrum_process::replay`
  does one; the release step that runs it over every open run comes with W8.
* A worker paused past its activity's timeout and then resumed was not tried (FLC-PROC-012); effect keys
  make its repeat a receipt, and Temporal refuses a timed-out attempt's completion.
