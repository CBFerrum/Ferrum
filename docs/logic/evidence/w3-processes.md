# W3: durable processes and human work (8 November 2026)

Software evidence only, run on the owner's development machine (macOS, PostgreSQL 17.9) from the worktree
`ERP/flc/ferrum`, branch `logic/w3-processes`, based on Ferrum `a3b054d`. Temporal Server 1.32.2, the
version the spike qualified on PostgreSQL, ran as the CLI's dev server (`temporal` 1.10.0, SQLite, namespace
`ferrum-flc`). Databases `ferrum_flc_kernel_test`, `ferrum_flc_process_test`, `ferrum_flc_studio_test`,
`ferrum_flc_plugins_test` and `ferrum_flc_mcp_test`, reset by each suite.

## What ran

```bash
export CARGO_TARGET_DIR=$HOME/Desktop/Personal/ERP/flc/target CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
temporal server start-dev --headless --port 7233 --namespace ferrum-flc --db-filename …/temporal.db
# each suite with a database of its own (FERRUM_*_TEST_DATABASE_URL → ferrum_flc_*_test)
cargo test -p ferrum-logic --lib                                   # 109 passed
cargo test -p ferrum-kernel --lib                                  # 70 passed
cargo test -p ferrum-kernel --test <each of 37>                    # all pass; saml_it and saml_keycloak_it run
                                                                   # nothing without their identity provider
cargo test -p ferrum-meta                                          # 20 passed
cargo test -p ferrum-plugins                                       # 28 passed
cargo test -p ferrum-mcp                                           # 1 passed
cargo test -p ferrum-studio                                        # 28 passed
cargo test -p ferrum-ops                                           # 5 passed
TEMPORAL_ADDRESS=http://127.0.0.1:7233 cargo test -p ferrum-process   # 2 passed: the codec, the conformance run
PROTOC=/usr/bin/false cargo check -p ferrum-process                # builds without protoc
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p ferrum-server --no-default-features --features process -- -D warnings
cargo fmt --all -- --check
python3 scripts/registry.py check
python3 scripts/mutants.py <each W3 mutant>                        # 30 of 30 caught
```

## What it showed

**The language** (`compile.rs`, `solution.rs`): parallel steps, for-each, repeat, subprocesses, cases,
compensation, delays and business-day waits compile, each block checked as an acyclic graph whose steps read
only what surely ran before them. The compiler refuses: a list that is not one, an item key that is not text
or a number, bounds out of range, a branch without a name, `again` outside a repeat, running out of rounds or a
refused branch routed to success, an unknown calendar, case tasks that wait for each other or name an unlisted
task, a compensation not asked for, a recovery that resumes where a step's inputs may not have run, a
subprocess that recurses, an effect relying on an approval that does not surely come first.

**The state machine** (`process.rs`): the purchase example runs from trigger to completion, the reviewer
bound to the digest of what they saw and the requester excluded; an unknown outcome waits for a person and
cannot be resumed into success; late and repeated events change nothing; the same events ask for the same
things, before and after the run is serialized. In the structured example: one activation per item, named by
its key with the key's separators escaped; the portal's answer wins the race, the mail is asked to stop, and
the mail settling afterwards is recorded and handed to a person; two rounds of rework, then a case that will not
close before its required task; three rejections run the compensation newest first, a refused one stays open,
and a recovery tries only what is still open; a cancelled run awaits its three holds, records the one that
happened, and finishes; two items with one key are refused; a run past its step budget waits for a person.

**Calendars** (`calendar.rs`): two business days from a Friday with Monday a holiday is Wednesday at the same
time; a revised calendar moves it to Thursday and says which revision did; 02:30 on 8 March in New York moves
to 03:00 (`skipped`), 01:30 on 1 November takes the earlier (`repeated`); a wait names a calendar the workspace
has.

**Ferrum's records** (`process_it`, a driver doing what the runtime does): an event delivered twice at once
starts one run; the requester holding the reviewer's role still cannot decide, another task's material is not
found for an outsider, and the task's deadline records its calendar; a decision on other material, or with a
field the form does not ask, is refused; the approved order is made once under `{run}/order`, asked again it is
the same order, other input under the key is refused, and an approval of a record since deleted holds nothing; the order's own event completes the run, one that came
before the wait is kept for it, another order's event advances nothing; the request cancelled after approval
makes the order refused and the run wait for a person, who can end it but not resume it, and the cancellation
starts an investigation whose case refuses an unlisted task, a task before its dependency, closing before the
required one, and closing by an administrator who is not its owner or by the requester; a decision and a deadline at once have one outcome, a deadline after a decision tells
the decision, and a deadline asked again says the same; a task handed on as often as its bound (two) allows, only to reviewers, never back or to the
requester, decided by the last, and never by an administrator without the role; a revised calendar moves only later deadlines; the requester's roles revoked while the run waited
refuse the order; the requester stops their run, its task closes; another workspace starts and sees nothing.

**The runtime** (`conformance_it`, on Temporal, every run continuing as new after each event): started by its
event, approved by a person, ordered under its effect key and completed by the order's event; started again, no
second run and nothing reported; every payload Temporal keeps carries the codec's encoding, and the history
replays with the codec but not without it; the worker stopped while a person decides, another takes over and
the run goes on; a review nobody decides times out on Temporal's timer, the task is `timed_out` and the run waits
for a person.

**Mutants**: thirty, each breaking one guarantee, all caught. State machine and compiler: `late-effect-erased`,
`compensation-oldest-first`, `rework-unbounded`, `item-keys-collide`, `step-budget-ignored`,
`cancel-drops-work-in-hand`, `case-close-waits-for-deliveries`, `subprocess-recurses`, `resume-reads-unrun`,
`approval-may-come-later`. Ferrum's records: `run-started-twice`, `requester-decides`, `administrator-decides`,
`decided-on-other-material`, `deadline-overrides-decision`, `repeated-deadline-says-nothing`,
`delegation-unbounded`, `delegation-counts-people`, `delegation-goes-back`, `effect-on-stale-approval`,
`approval-of-deleted-record`, `effect-borrows-rights`, `early-event-dropped`, `event-crosses-records`,
`case-order-ignored`, `requester-closes-case`, `recovery-forces-success`. The runtime: `history-not-sealed`,
`run-started-again`, `late-delivery-retried`. `scripts/mutants.py --check`: 276 mutants find their text.

**Found while re-reading, fixed before this record** (each now has a test and a mutant): a delegation bound
counted people instead of hand-ons; administrators could decide any task and close any case; a deadline asked
again after a lost answer told nothing (the run would have waited for ever); activities gave up after eight
attempts; transient errors while checking rights or an approval became refusals; an approval of a deleted
record held; a case's close could be overtaken by a task's delivery and ignored; a rotated key would have left
earlier runs unable to read their history.

## The package's acceptance cases

| Case | Here |
|---|---|
| AT-PROC-001 lost start, concurrent trigger | `process_it` (two acceptances at once); `conformance_it` (repeated start) |
| AT-PROC-002 crash at commit boundaries | W1's receipts (`capability_it`); `process_it` (the effect asked again) |
| AT-PROC-003 retry, then changed input | `process_it` (`input_changed`) |
| AT-PROC-004 event before the wait | `process_it` (held waits) |
| AT-PROC-005 another record's or workspace's signal | `process_it` |
| AT-PROC-006 grant revoked while asleep | `process_it` (roles revoked) |
| AT-PROC-007 replay with changed clock and state | the state machine's determinism test; `conformance_it` (replay with and without the codec) |
| AT-PROC-008 a branch commits after cancellation | `the_first_branch_wins_and_a_loser_that_settles_late_is_kept_for_a_person` |
| AT-PROC-009 rework twice, then past the maximum | `rework_runs_rounds…`, `running_out_of_rounds_compensates…` |
| AT-PROC-010 cancel while an effect commits | `a_cancelled_run_keeps_what_was_in_hand_and_finishes_when_it_settles` |
| AT-PROC-011 compensation fails | `running_out_of_rounds_compensates_newest_first_and_a_failed_undo_stays_open` |
| AT-PROC-012 paused worker resumes | Not run: a stopped worker replaced (`conformance_it`) |
| AT-PROC-013 history budget with pending signals | `conformance_it` (continues after every event) |
| AT-PROC-014 snapshot then subscription | Not built: no run status stream |
| AT-HUMAN-001 requester is also a reviewer | `process_it` |
| AT-HUMAN-002 changed after approval | `process_it` (R-4) |
| AT-HUMAN-003 decision at the deadline | `process_it` (race, and a deadline after a decision); `conformance_it` (timer) |
| AT-HUMAN-004 circular or excess delegation | `process_it` |
| AT-HUMAN-005 skipped and repeated hours, revised holidays | `calendar.rs` tests, `process_it` |
| AT-HUMAN-006 another task's evidence | `process_it` (material); attachments do not exist |
| AT-HUMAN-007 an unlisted privileged task | `process_it`; `a_case_offers_only_its_tasks_and_their_order` |
| AT-HUMAN-008 force-success on an unknown effect | `process_it` (no resume where none is allowed); `an_unknown_outcome_waits_for_a_person…` |

## Not run here

Nothing went through HTTP: W3 adds no endpoints (W4 adds the Studio endpoints and the run console). The
runtime ran on Temporal's dev server, not a PostgreSQL-backed cluster (the spike qualified that), and not in the
release image. No worker was paused past its activity's timeout. Nothing ran for a customer.
