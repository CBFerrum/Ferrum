# Studio: building, reviewing and running solutions

How people build customer solutions in Ferrum, review them, and work with what they start: Studio's
endpoints (`crates/studio/src/logic.rs`), the kernel functions behind them (`crates/kernel/src/authoring.rs`,
`logic.rs`, `process.rs`), and the pages (`Ferrum-Frontend: src/app/studio/solutions`, `src/app/studio/runs`).
The language is [rules.md](rules.md) and [processes.md](processes.md); the requirements are FLC-UI in
[traceability.md](traceability.md); the evidence is [evidence/w4-studio.md](evidence/w4-studio.md).

## Who does what

| Who | May |
|---|---|
| Anyone signed in | Check a source against the capabilities their own roles let them use; see their tasks and the runs they started; see the processes about a record they may read |
| **Solution Author** (Studio's role) | Everything above, and edit drafts, simulate, keep releases, see the calendars and every release's review page. Granted until a date, it ends then (BUILDER_UX journey 8) |
| Administrator | Everything above, and review (never their own release), activate and stop releases, set calendars, recover and acknowledge runs' items, see every run |

An author builds with the capabilities their own roles grant: the catalog never describes what its reader may
not use, so a partner who builds purchase processes holds a purchasing role too. A release runs with the
rights of whoever starts each run, not its author's (processes.md).

## Drafts

A draft is a solution's source being edited. It is saved against the version its editor read
(`If-Match: "v3"`); a save that comes after someone else's is refused with the version now there, the name of
who saved it and when (409), so two editors never overwrite each other (FLC-UI-006). The editor then offers to keep its
own text as a new draft, or to open the latest. Unsaved text is also kept in the browser and offered back
when the page opens on the same version; saving still checks the version. A draft never runs: keeping it as
a release compiles it and stores an immutable release beside it.

The editor shows one definition in two views. **Guided** lists a process's steps as an outline, in the order
a reader follows them from the entry (a structured step's blocks nested under it), and edits the selected
step's settings in an inspector typed by what the step is: a capability chosen from the catalog adds itself to
`requires` at its installed version, routes are chosen among the steps of the same block, waits are a
duration or business days on a calendar. It edits the parsed source in place, so a setting it does not know,
and a structured step's own settings, survive (they are edited in **Source**). A step other steps route to
cannot be removed; the inspector says which. The outline is plain lists and buttons: every operation works
from the keyboard and reads to a screen reader (FLC-UI-004). The source is still checked whatever the guided
view shows: a setting it does not offer, such as letting the requester decide their own request, is refused
by the compiler at that setting's pointer.

## Checks and simulations

**Check** compiles the source against the workspace and keeps nothing; each fault has a stable code and a
pointer, and choosing one selects its step. **Simulate** runs a process on a virtual clock in the same state
machine the runtime drives (`ferrum_logic::simulate`): each read and action answers as the scenario says, a
person's task is approved or rejected after a chosen time or runs to its deadline, an event comes or never
does. A capability the scenario does not answer is reported as *not modelled* and the step sees an unknown
outcome, never an invented success. Deadlines are worked out on the workspace's calendars as the runtime
works them out. A simulation creates no run, task, record or message (FLC-UI-007, FLC-UI-010).

## Reviews and releases

A release's page shows what it is (digests, profile, the capabilities it may use with their effects and what
undoes them), what runs now for its solution, and what changed between the two by meaning
(`ferrum_logic::diff`): parameters, rules, decision tables and each row, processes and each step, and
capabilities. Moving a box changes nothing there; a label a person reads does (FLC-UI-005). The changing
capabilities the release may use that what runs now cannot are listed for the reviewer to consent to one by
one: approving without naming exactly those is refused. Approval is bound to the digest shown; the author is
told why they cannot approve, and the server refuses it anyway. Activation names the generation seen
running; one changed meanwhile is refused. Its answer says which release it replaced, the automation it took
over from (turned off in the same step), how many runs stay on another release, and that nothing a run did is
undone; going back is activating the earlier release again (FLC-MIG-006, FLC-MIG-003).

Before activating, **Compare with what runs now** replays recent work against the release and against what
runs now (`POST /solutions/shadow`): the last runs of its solution with the answers, decisions and events they
met, or an automation's recent events against what it did. Cases that differ come first, each with what
differs: how it ends, what it decides, the steps it takes, what it changes. Nothing runs, changes or is kept
(FLC-MIG-002). A step the release adds has no answer on record; `assume` gives one, by step or capability,
used only where history has none, and each case lists the assumptions it used. **Download as a pack** gives the release to another workspace: its source, digests, needs and
review decisions without names, never a connection or a credential; there **Take in a pack** makes a draft of
it and lists what must be mapped (FLC-OPS-008).

## Runs and tasks

The runs page lists the runs a person may see and keeps them up to date by polling `GET /runs?cursor=`. The
cursor is a transaction boundary: every transaction below it had finished when the answer was made, so a
change committed later by a slower transaction is in a later answer, never skipped; a run may come twice and
the page keeps the latest. A cursor from another database (a restored one) gets `resync` and the runs as they
stand (FLC-PROC-014). A long-open transaction holds the boundary back: answers repeat until it ends.

A task's page shows what the person decides on, the deadline with the calendar and revision it was worked out
on, and the form; a decision carries the digest of what was shown, and a changed one is refused with a way to
look again. A decision may be handed on, by email, as many times as its process allows, to someone holding
the role who has not had it; never to the requester. A closed task keeps who did what on it, when and why. A
case lists only its tasks, each with what it waits for. Recovery and attention items are an
administrator's: a recovery records what was done and resumes where the process allows, or ends the run
refused; nothing marks it a success. A step waiting for a capability out of service has an attention item
saying so; it goes on by itself once the capability is back (FLC-OPS-007). On a run that waits under an older
release, an administrator can plan moving it onto the release that runs now, naming the step of that release
each waiting step becomes, and another administrator approves exactly that; the run's page keeps every move
planned, approved, made or refused (FLC-MIG-004). A run waiting inside a loop, branches, a round or a
subprocess moves too: a step that holds others keeps its id there, and a waiting step moves within its block. A record's Related tab lists the processes about it, with their state and
who they wait on, to whoever may read the record; not what those tasks are about.

## Not yet

- A diagram view: the outline is the linear equivalent the specification asks for first; a drawn graph of
  the same definition is not built.
- Guided editing of decision tables and rules: they are edited in the source; decision steps route on their
  outcomes in the guided view.
- Deployment to a test workspace or site before a production cohort, with per-site acceptance: activation is
  per workspace (W8 and Edge's W5).
- A live push channel for runs: the page polls the cursor every five seconds.
