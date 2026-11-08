# W4: the Studio product (8 November 2026)

Software evidence only, run on the owner's development machine (macOS, PostgreSQL 17.9) from the worktrees
`ERP/flc/ferrum` (branch `logic/w4-studio`, based on Ferrum `0a87c1c`) and `ERP/flc/web` (branch
`logic/w4-studio`, based on Ferrum-Frontend `c59c9a2`). Temporal Server 1.32.2 ran as the CLI's dev server
(`temporal` 1.10.0, namespace `ferrum-flc`). The walks used the real `ferrum` binary built with
`--no-default-features --features studio,process`, serving the Frontend's built apps, on port 5170 with the
database `ferrum_flc_walk`, signed in through the login page as ordinary roles of the workspace `acme`.

## What ran

```bash
export CARGO_TARGET_DIR=$HOME/Desktop/Personal/ERP/flc/target CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
# Ferrum: each suite with a database of its own (FERRUM_*_TEST_DATABASE_URL → ferrum_flc_*_test)
cargo test -p ferrum-logic --lib                       # 117 passed
cargo test -p ferrum-kernel --lib, each --test         # 70 passed; all 37 test files pass (saml_it and
                                                       # saml_keycloak_it run nothing without their identity provider)
cargo test -p ferrum-studio                            # 29 passed, solutions_it among them
TEMPORAL_ADDRESS=http://127.0.0.1:7233 cargo test -p ferrum-process   # 2 passed
cargo test -p ferrum-meta, -p ferrum-plugins, -p ferrum-mcp, -p ferrum-ops   # 20, 28, 1 and 5 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ferrum-server --no-default-features --features process -- -D warnings
cargo clippy -p ferrum-server --no-default-features --features studio,process -- -D warnings
cargo fmt --all -- --check
python3 scripts/registry.py check
python3 scripts/mutants.py <each W4 mutant>            # 7 of 7 caught: draft-save-overwrites,
                                                       # approval-without-consent, cursor-passes-open-transaction,
                                                       # restored-cursor-not-resynced, author-reviews,
                                                       # related-runs-unchecked, diff-shows-old-as-new
# Ferrum-Frontend (its own node_modules, npm ci)
npx tsc --noEmit
npx vitest run                                         # 496 passed; render.test.tsx draws the solution, run and
                                                       # task screens and runs axe over them
npm run build:apps                                     # shell 124.7 KB, studio 121 KB of 200 KB entry JavaScript
```

## The walks

People in `acme`, each signed in through the login page: `author@acme.test` (Solution Author and Requester),
`requester@acme.test` (Requester and Reviewer), `reviewer@acme.test` (Reviewer), and two administrators.
The workspace calendar `office` is Asia/Kolkata with Saturday and Sunday off.

**Journey 1, build and test without source (FLC-UI-001, 007, 010).** The author started an approval from the
template, chose the trigger, the read and the order capabilities and the reviewing role in the guided view,
checked it (no faults), and simulated it: the reviewer's decision an hour later on the virtual clock, marked
stand-in, and the order marked not modelled until answered. With nobody deciding, the task timed out on
Tuesday 10 November, two business days on the office calendar from the Friday start, and the run stopped for
attention. Nothing was created. The author kept it as a release and could not approve it: the page said why.

**Review and activation (FLC-UI-005).** A second administrator opened the release, saw what it changes by
meaning and the one capability that changes records it would gain, consented to it by name, approved and
activated it. A later release showed each changed setting as it was and as it is: the review's label, its
new rejection route, the step budget from 100 to 120, and a deadline gone from two business days to one, which
the walk had not meant to change; that led to the fix of the business-days field below.

**Runs and tasks.** The requester submitted REQ-WALK-1: the run started on Temporal, the requester saw it on
Runs and on the request's Related tab ("Waiting on Reviewer: Approve the request"), had no task, and the
task's address answered "task not found" to them. The reviewer approved it; the order was made and the run
completed.

**Journey 7, recovery (FLC-UI-008).** REQ-WALK-2 was approved after its requester had cancelled it: the order
was refused (`stale_approval`) and the run waited for a person. The administrator read what happened (the
refusal first, then each step's outcome) on the run, opened the recovery item, was told the step had nowhere
to resume, recorded what was done and ended the run refused. After a refresh, and again after the server was
restarted, the run read Refused and the item showed who ended it, when and why.

**Journey 4, two editors (FLC-UI-006).** With the administrator holding version 3 of a draft open, the author
saved version 4. The administrator's save was refused with "saved by Ana Author at 8 Nov 2026 07:40 UTC"; "Keep
mine as a new draft" kept their text as a draft of its own, and the author's save stood. Again at version 5,
"Open the latest" asked before discarding, then showed version 5.

**Journey 2, one definition (FLC-UI-002).** In Source the administrator gave the review a business-day deadline
and added a parallel step with two branches; the guided view showed the parallel step with its branches
nested, renamed the review, and Source then still held the deadline, the parallel step and the action's
approval link. Setting `exclude_requester: false` in the source was refused by the check at that setting.
Saved and reloaded, all of it stood. A browser-kept copy was offered back after a server restart.

**Journey 6, keyboard and screen reader (FLC-UI-004).** By keyboard alone, from the skip link: the reviewer
reached the task list, opened a task, filled in the reason and approved; focus went into each new page and,
when the task closed, to the page that said so; another task was rejected the same way. The administrator,
by keyboard alone, added a step to a draft, checked it (each result read out through a live region), opened a
fault (focus moved to that step's settings), routed the first review's rejection to the new step and mended
the two faults that followed, then saved. The walk found, and these were fixed: fields whose labels did not
reach a screen reader; focus left behind after a route change, a decision, a check, a save or a closed notice;
a hint offered only on hover; Solutions offered to people it refuses. No screen reader was used.

**Localization (FLC-UI-009).** With Arabic chosen and Asia/Kolkata as their zone, the reviewer read a closed
task right to left, its deadline "10/11/2026 1:42 pm GMT+5:30" (08:12 UTC) and the decision at 1:43 pm; the
administrator's draft editor mirrored, outline on the right and inspector on the left, with nothing running
off the page. The task history said "rejected" with the status word, which in Arabic read as a description of
the person; it now has verbs of its own.

## What the tests show

`solutions_it` (Ferrum's Studio site, as people call it): calendars only for administrators; two editors on
one draft, the stale save naming who saved; a check that points at its fault and is filtered by the reader's
rights; a simulation that creates no run, task or record and queues no mail; a deadline on the workspace's
calendar; a review that lists the change and the new power and refuses approval without exact consent; the
run cursor under a slower transaction, and a restored cursor resynced; the processes about a record for
whoever may read it, and nothing of what their tasks are about; a task decided over HTTP, refused to its
requester and on other material, then kept with who decided it; a task handed on by email and its hand-on
kept with its reason; a partner author whose role ends; another workspace that sees none of it.

`render.test.tsx` (the Frontend): each screen drawn with data shaped like the server's, what reaches the page
(refusals first, who did what, why a button is off, what a capability promises), and axe over all of them,
which fails when the labels come loose (tried by removing them).

## Limits

- No diagram view: Guided and Source are the two views (FLC-UI-002).
- No screen reader session and no assistive-technology user (BLOCK-ACCESSIBILITY-AUDIT).
- The simulator and a deployed run were compared on one approval, not on boundary fixtures (FLC-UI-007).
- The recovery console settles an unknown outcome with a person's evidence, not a capability's lookup; one
  server was walked, not two replicas (FLC-UI-008).
- The workspace's own zone was UTC; Hindi was not walked (FLC-UI-009).
- Activation is per workspace: no test workspace or cohort first (W8). The run feed polls; nothing pushes.
