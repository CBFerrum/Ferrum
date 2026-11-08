# Traceability

Generated from [docs/registry/flc.yaml](../registry/flc.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Not started | 14 |
| Partial | 67 |
| Implemented, unverified | 2 |
| Verified in software | 40 |
| Blocked externally | 1 |
| **All** | **124** |

## W0: Establish reality and decision records

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-BASE-001 | Fresh assembled baseline | Verified in software | AT-BASE-001 | 1 | Heads, pins, builds and installed versions are read separately; installed versions come from the local development database only, so no production installation is known |
| FLC-BASE-002 | Safe worktree | Verified in software | AT-BASE-002 | 1 | Reports dirty files and unused patches; it does not stop anyone committing them |
| FLC-BASE-003 | Reuse map | Partial | AT-BASE-003 | 0 | Mapped by reading the code at the pins it names; each entry is confirmed when its workstream starts |
| FLC-BASE-004 | Durable backend ADR | Verified in software | AT-BASE-004 | 1 | One development-layout server without TLS, high availability or online backup; 200 workflows is not a capacity test; Temporal's documentation must list PostgreSQL 17 before production (its schema applied and every probe ran on 17.9) |
| FLC-BASE-005 | Supported profiles | Partial | AT-BASE-005 | 0 | Profiles are written down; no installation refuses a long-lived process yet, because none can be defined |
| FLC-BASE-006 | License inventory | Partial | AT-BASE-006 | 0 | Rust dependencies are inventoried by scripts/licenses.py; Temporal, Wren and DataFusion are reviewed by reading their licences, and nothing gates an unapproved engine in a deployment plan yet |

## W1: Shared types and business capabilities

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-CAP-001 | Typed capability registry | Implemented, unverified | AT-CAP-001 | 2 | Read from what a workspace installs, with effect, rights, input and output schemas, idempotency, completion and digest; the builder's endpoint, the agent's tools and the kernel agree digest for digest over ERP's packages (catalog_it). The builder's screens (W4) do not exist, so the case's UI is not yet part of it |
| FLC-CAP-002 | Native invariants | Verified in software | AT-CAP-002 | 2 | A ledger offers no capability, a read-only field is the app's to set, and actions run through their owning app (capability_it, mutant capability-ledger-offered). Record updates remain capabilities for the fields an app leaves writable: an app protects a balance by making it read-only or a ledger |
| FLC-CAP-003 | Trusted context | Verified in software | AT-CAP-003 | 2 | An input naming a workspace, a role or an administrator flag is refused unless the capability declares that field as data, and another workspace's records and receipts do not exist (capability_it, mutant capability-input-says-who-asks). Values resolved inside a process (references from a device message) come with W3 and W5 |
| FLC-CAP-004 | Version and digest lock | Partial | AT-CAP-004 | 1 | Each capability carries its package's version and a digest of what it promises; asked for another version, the gate refuses (mutant capability-version-adopted). Keeping a waiting process on its old binding needs processes (W3) |
| FLC-CAP-005 | Delegation modes | Partial | AT-CAP-005 | 0 | Jobs run with their creator's rights as they are now and service apps have accounts of their own; automations run on their saver's administrator rights, a third mode this program replaces |
| FLC-CAP-006 | Completion semantics | Partial | AT-CAP-006 | 0 | Each capability says what done means, and mail says it is queued, not delivered; a run waiting for the business outcome after an acceptance needs processes (W3) |
| FLC-CAP-007 | Unknown outcomes | Partial | AT-CAP-007 | 0 | Each capability declares its unknown-outcome behaviour; a native receipt lets the outcome of a transactional action be looked up. External effects that can end unknown, and their reconciliation, come with W3 |
| FLC-CAP-008 | Deprecation ownership | Not started | AT-CAP-008 | 0 | No capability revisions exist to deprecate |
| FLC-SEC-001 | Least privilege grants | Partial | AT-SEC-001 | 1 | Capabilities come from installed apps and each needs its own rights, so declaring one grants nothing (mutant capability-rights-ignored); mail is an administrator's. Solution packs, their reviewers and deployers come with W2 and W4; an automation still runs with its saver's administrator rights |
| FLC-SEC-002 | Deny precedence | Partial | AT-SEC-002 | 1 | A customization that makes a packaged required field optional is suspended; rules, workflows and tenant logic are not compared yet |
| FLC-SEC-003 | Secret handles | Partial | AT-SEC-003 | 1 | Connectors and plugins name secrets by reference; no solution source or run history exists yet to keep them out of |
| FLC-SEC-004 | WASM budgets | Partial | AT-SEC-004 | 1 | Plugins run under memory limits, epoch deadlines that count host calls, and a cap on host calls; no test here runs an endless guest beside a slow host call |
| FLC-SEC-005 | Egress and SSRF | Partial | AT-SEC-005 | 0 | Plugin fetches and webhooks refuse private addresses, pin the vetted address and follow no redirects; logic capabilities with destinations do not exist yet |
| FLC-SEC-006 | Cross-tenant caches | Partial | AT-SEC-006 | 0 | Studio computes every result for the viewer and keeps no shared result cache; logic plan and result caches do not exist yet |
| FLC-SEC-007 | Diagnostic access | Partial | AT-SEC-007 | 1 | Support grants are graded, expire and list every request for the customer; there are no run traces yet to restrict |
| FLC-SEC-008 | Fail-closed uncertainty | Partial | AT-SEC-008 | 0 | Delayed work whose requester lost their rights fails closed; logic has no permission or evidence state yet |

## W2: Compiler, pure rules and hook integration

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-COMP-001 | Strict parsing | Verified in software | AT-COMP-001 | 2 | A solution's source is JSON read strictly: a key twice, an unknown key anywhere, more than 1 MiB or 64 levels, a non-finite number or trailing text is refused before compiling (t03, t32, t33, unknown_keys_are_refused_everywhere). The format has no reference to fetch, so nothing is fetched |
| FLC-COMP-002 | Business types | Partial | AT-COMP-002 | 2 | Money in one currency, quantities in one unit, exact decimals, missing kept apart from unknown, and outputs typed from each capability's schema, all checked before publishing (types_follow_the_data, money_compares_exactly_at_the_boundary). Links are typed as text ids, so a reference to the wrong kind of record is not caught before running |
| FLC-COMP-003 | Deterministic expressions | Partial | AT-COMP-003 | 1 | The rules evaluator reads no clock, network or randomness: the kernel hands it today and now once per save, and functions are a fixed list (the_clock_is_a_fact_not_a_reading, an unknown function refused at compile). The Edge gateway's build of the evaluator and the replay across server and gateway come with W5; automations' and workflows' older CEL still reads the clock when no time is given |
| FLC-COMP-004 | Canonical identity | Verified in software | AT-COMP-004 | 2 | The source digest leaves out layout and keeps labels, forms and parameters; the artifact digest adds the compiler, the profile and the lock (layout_is_not_meaning_and_meaning_is). A review is of the digest shown and activation compares it, so a changed instruction needs a new approval (logic_rules_it) |
| FLC-COMP-005 | Dependency closure | Verified in software | AT-COMP-005 | 2 | A release locks each capability at its digest and each field its rules read or fill at its type; activation compiles it again and needs the same digest, and an active release whose fields changed refuses its entity's saves with stale_approval (logic_rules_it, a_release_approved_against_other_fields_does_not_run). Solutions have no subflows to resolve |
| FLC-COMP-006 | Graph semantics | Verified in software | AT-COMP-006 | 1 | One entry, routes to existing steps, every step reachable, no cycle, a step budget covering the graph, unique lower_snake ids, a label on every step, and a step reads only what surely ran before it (t04-t07, names_versions_and_bounds, a_step_reads_only_what_has_surely_run). Processes have no parallel branches, so no join can be malformed |
| FLC-COMP-007 | Bounded iteration | Verified in software | AT-COMP-007 | 2 | Cycles are refused and expressions have no comprehensions (t07); repetition is a for-each over at most 1000 items with stable keys from their data, at most 20 at once, or a repeat of at most 100 rounds, each with a route when it runs out, and a run's max_steps stops what the bounds together allow (compile and process tests; mutants rework-unbounded, item-keys-collide, step-budget-ignored) |
| FLC-COMP-008 | Effect typing | Verified in software | AT-COMP-008 | 2 | A rule cannot name a capability (t10, logic_rules_it), and at run time its evaluator has nothing to call; a query step cannot use a capability that changes anything, nor an action step a read (t13) |
| FLC-COMP-009 | Data-flow policy | Verified in software | AT-COMP-009 | 2 | A rule reads no restricted field of its record or of a linked one, at compile and at run time; its message is plain text; linked records are read with the saver's rights; refusal evidence keeps no copy of the record (logic_rules_it, rules_read_nothing_restricted_and_write_once). Process outputs, traces and destinations come with W3 |
| FLC-COMP-010 | Source maps | Verified in software | AT-COMP-010 | 2 | Every fault has a stable code and a JSON pointer; a refusal at run time names its solution, version and rule, and its evidence the release and rule (logic_rules_it). Process steps map to their nodes with W3; there are no subflows |
| FLC-HOOK-001 | Phase ordering | Verified in software | AT-HOOK-001 | 1 | Tenant derivations run after the app's own fills and before every validation, so the app's guards see the final record; a derivation writes only the customer's own fields, never company, owner or the app's (logic_rules_it, t10_t11_t12) |
| FLC-HOOK-002 | Pure hook boundary | Verified in software | AT-HOOK-002 | 3 | Tenant rules can call no function outside a fixed pure list (an http_get is refused at compile); an automation's e-mail is queued with its run (mutant automation-mails-before-commit); plugins are refused I/O inside hooks |
| FLC-HOOK-003 | Competing writes | Verified in software | AT-HOOK-003 | 2 | One writer per field within a solution (compile), across active releases and against apps' declared fills (activation), and an entity whose active rules conflict refuses its saves (logic_rules_it, cycles_and_competing_writers_are_found_when_prepared). No merge rule can be declared. Plugins' own hooks are not compared |
| FLC-HOOK-004 | Bounded prefetch | Verified in software | AT-HOOK-004 | 2 | A rule reads only the linked records it declares: four per rule, sixteen fields each, eight per save, none restricted; they are read with the saver's rights before any rule runs, and a saver who may not read one is refused (logic_rules_it) |
| FLC-HOOK-005 | Equivalent mutation paths | Verified in software | AT-HOOK-005 | 3 | One invalid order through insert, update, import, bulk, a capability's invocation, an automation and a job, and a customer through MCP's create_record and invoke_capability and a plugin's data.insert: all refused alike and nothing written. The REST and /api/v1 routes call the same kernel insert and are not sent through over HTTP here |
| FLC-HOOK-006 | After-commit durability | Verified in software | AT-HOOK-006 | 2 | Events and queued mail commit with their change: an automation run that rolls back queues nothing, and one delivered twice mails once (automation_mail_it, mutant automation-mails-before-commit); consumers dedupe by receipt (events_it) |
| FLC-HOOK-007 | Rejection evidence | Verified in software | AT-HOOK-007 | 1 | A rule with audit_refusals keeps when, which release and rule, which record, who and whether false or unknown, in a transaction of its own; the refused change is not written and no copy of it is kept (logic_rules_it). It takes a second database connection while the save's is held |
| FLC-HOOK-008 | Event recursion | Verified in software | AT-HOOK-008 | 1 | Work answering an event writes events one step further, whatever handler does it, jobs included; the dispatcher stops an event 8 steps from its chain's start, or past 1000 events from one change, before any handler runs, and keeps it dead with the chain it took; a replay starts afresh; automations stopped at their own limit say so (event_chain_it) |

## W3: Durable processes and human work

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-PROC-001 | Idempotent starts | Verified in software | AT-PROC-001 | 2 | An event is accepted once per process and business key, on the release active then, with its start queued in the same transaction; two acceptances racing make one run (process_it). The workflow id is made from the run, a repeated start finds it, and a run that ended is not started again (conformance_it; mutants run-started-twice, run-started-again). A reviewed catch-up over past events does not exist |
| FLC-PROC-002 | Native action receipts | Verified in software | AT-PROC-002 | 2 | The receipt commits in the change's transaction: a repeat gets it, two racing get one change, a failure before the commit leaves neither (capability_it; mutants capability-no-receipt, capability-same-key-other-input). A process's effects go through the same gate under {run}/{activation}: asked again it returns the same record (process_it). A database crash mid-commit is PostgreSQL's to keep atomic; receipts are not yet removed after their retention |
| FLC-PROC-003 | External effect identity | Verified in software | AT-PROC-003 | 2 | A step's effect key is the same on every attempt (the activity is retried under its activation) and the gate binds it to the input's digest: other input under it is refused (process_it). An outcome unknown goes where the step says, never to success; providers' own idempotency horizons are not recorded, and an unknown outcome is not queried back automatically |
| FLC-PROC-004 | Signal before wait | Verified in software | AT-PROC-004 | 1 | An event that comes before its wait is kept, by the record it is about, and handed over once when the run waits (process_it, held waits; mutant early-event-dropped). It is kept seven days: older ones no longer match but are not yet deleted |
| FLC-PROC-005 | Signal spoofing | Verified in software | AT-PROC-005 | 1 | A run hears only its workspace's committed events about the record its wait names; another record's event, or another workspace's, advances nothing (process_it; mutant event-crosses-records). Nothing outside Ferrum can signal a run |
| FLC-PROC-006 | Current authority | Verified in software | AT-PROC-006 | 1 | Each effect runs with the rights of whoever started the run as they are then, through the capability gate; rights revoked while it waited refuse it, and the run waits for a person (process_it; mutant effect-borrows-rights). A run no person started acts with whoever activated its release; there is no separate service grant per release |
| FLC-PROC-007 | Replay determinism | Verified in software | AT-PROC-007 | 2 | The state machine reads no clock, store or network: the same events ask for the same things, across a serialized continuation (process tests). The workflow's history replays with the same build and codec, and each effect's activity id is its activation, as the spike required (conformance_it). Replaying open runs before a new worker build takes over is a release step for W8 |
| FLC-PROC-008 | Parallel race | Verified in software | AT-PROC-008 | 1 | The first branch to complete wins; the others are asked to stop, and an effect one settles afterwards is recorded and handed to a person, never dropped (process tests; mutant late-effect-erased). Run in the state machine the runtime drives, not end to end on Temporal |
| FLC-PROC-009 | Business rework | Verified in software | AT-PROC-009 | 2 | A repeat runs its body again while it ends `again`, at most its rounds, then takes on_exhausted, never success (compile and process tests; mutant rework-unbounded) |
| FLC-PROC-010 | Cancellation | Verified in software | AT-PROC-010 | 2 | A cancelled run closes its tasks and waits, keeps awaiting the effects in hand, records one that completes anyway and hands it to a person, and finishes once they settle (process tests, process_it; mutant cancel-drops-work-in-hand). An effect in hand cannot be stopped midway: the gate runs it in one transaction |
| FLC-PROC-011 | Compensation failure | Verified in software | AT-PROC-011 | 2 | Compensation runs newest first through each app's compensating action; a refused or unknown one stays open, handed to a person, and the run waits for a recovery that tries only what is still open (process tests; mutant compensation-oldest-first). The compensating action is given the effect's record, not a mapping the solution declares |
| FLC-PROC-012 | Worker fencing | Partial | AT-PROC-012 | 1 | A stopped worker's runs are taken up by another (conformance_it); a repeated effect finds its receipt, and Temporal refuses the completion of an attempt that timed out. A worker paused past its timeout and then resumed was not tried |
| FLC-PROC-013 | History bounds | Verified in software | AT-PROC-013 | 1 | A run continues as new after 500 events, or when Temporal suggests it, carrying its state, deadlines and undelivered signals under the same workflow id and activations; the conformance test continues after every event, across a decision delivered while no worker ran |
| FLC-PROC-014 | Snapshot plus subscription | Not started | AT-PROC-014 | 0 | Runs are read as they stand (logic_runs, logic_run); there is no live stream of a run's status |
| FLC-HUMAN-001 | Independent approval | Verified in software | AT-HUMAN-001 | 2 | The requester cannot decide their own request whatever roles they hold, nor an administrator without the task's role, and a task approved by several needs that many different people; the compiler refuses a task that allows the requester (process_it; mutants requester-decides, administrator-decides) |
| FLC-HUMAN-002 | Input-bound approval | Verified in software | AT-HUMAN-002 | 1 | A decision is bound to the digest of what was shown; an effect relying on an approval carries it and is refused if the approved record's version changed since, or the record is gone (process_it; mutants decided-on-other-material, effect-on-stale-approval, approval-of-deleted-record). Any change of version counts as material; the release's digest is the policy it ran under |
| FLC-HUMAN-003 | Timeout race | Verified in software | AT-HUMAN-003 | 2 | A decision and a deadline are settled under the task's row lock: one of them, never both; a deadline after a decision tells the decision, and asked again after a lost answer it says the same (process_it; mutants deadline-overrides-decision, repeated-deadline-says-nothing). On Temporal a deadline is a timer, and the database decides what it means (conformance_it) |
| FLC-HUMAN-004 | Delegation | Verified in software | AT-HUMAN-004 | 1 | A task is handed on at most its bound of hand-ons (three at most), only to someone holding its role, never back along its chain or to the requester; who decided is recorded (process_it; mutants delegation-unbounded, delegation-counts-people, delegation-goes-back). Standing substitutes during leave do not exist |
| FLC-HUMAN-005 | Calendar semantics | Verified in software | AT-HUMAN-005 | 2 | Business days skip weekends and holidays at the same local time; a skipped local time moves past the gap, a repeated one takes the earlier; each deadline records the calendar, its revision, the zone and the zone data, and a revision moves only later deadlines (calendar tests, process_it). There are no schedules yet, so no misfire policy |
| FLC-HUMAN-006 | Task evidence | Partial | AT-HUMAN-006 | 1 | A task's material is readable only by someone who may decide it, or an administrator (process_it). Forms take typed values; attachments as evidence do not exist |
| FLC-HUMAN-007 | Case work | Verified in software | AT-HUMAN-007 | 2 | A case starts only its listed tasks, each once its dependencies are done, and is closed only by its owner's role, never by the requester, once the required ones are; other work needs a new reviewed definition; dependency circles are refused when compiled (process_it; mutants case-order-ignored, requester-closes-case). The close carries the finished tasks' results, so it counts even ahead of their own deliveries (mutant case-close-waits-for-deliveries) |
| FLC-HUMAN-008 | Manual disposition | Verified in software | AT-HUMAN-008 | 2 | A suspended step is recovered by an administrator with a reason, resumed only where its source allows or ended refused; nothing marks it a success; what a run hands to a person is acknowledged with a reason (process_it; mutant recovery-forces-success). The authority is an administrator's; no recovery role per solution |

## W4: Complete Studio product

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-UI-001 | In-app authoring | Partial | AT-UI-001 | 0 | Edge flows, fields and rules are authored in the product; there is no solution, decision or process builder |
| FLC-UI-002 | Single definition | Partial | AT-UI-002 | 0 | Studio's builder and JSON edit one query spec; nothing else has synchronized views |
| FLC-UI-003 | Typed inspector | Partial | AT-UI-003 | 0 | Edge's inspector shows a step's settings; no node shows permissions, effects, retries or completion |
| FLC-UI-004 | Accessible outline | Partial | AT-UI-004 | 0 | Edge's canvas is reached by Tab and moved by arrow keys, and the form designer moves fields by menu; no linear outline exists for processes |
| FLC-UI-005 | Semantic diff | Not started | AT-UI-005 | 0 | No release review exists |
| FLC-UI-006 | Conflict handling | Partial | AT-UI-006 | 0 | Records are saved against their version and a stale save is refused; drafts of logic do not exist |
| FLC-UI-007 | Simulator parity | Partial | AT-UI-007 | 0 | Edge simulates with the code a gateway runs; nothing simulates processes |
| FLC-UI-008 | Recovery console | Partial | AT-UI-008 | 0 | Operations shows unknown outcomes with a plain reason; nothing recovers business runs |
| FLC-UI-009 | Localization | Partial | AT-UI-009 | 0 | Interfaces carry Arabic and Hindi, right to left and workspace time zones; the builder does not exist |
| FLC-UI-010 | Preview isolation | Partial | AT-UI-010 | 0 | A draft or simulated Edge flow reaches nothing; process simulation does not exist |

## W5: Generic edge and event integration

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-EDGE-001 | Authenticated observation | Partial | AT-EDGE-001 | 1 | Devices and sites have identities held by the platform; pipeline v2 does not exist |
| FLC-EDGE-002 | Normalization provenance | Partial | AT-EDGE-002 | 0 | Definitions describe what devices report; no checked unit and scale normalization keeps raw provenance |
| FLC-EDGE-003 | Quality semantics | Partial | AT-EDGE-003 | 0 | Flows hear only readings within quality limits and can ignore stale inputs; there are no quality classes |
| FLC-EDGE-004 | State scope | Partial | AT-EDGE-004 | 0 | Each device is watched on its own; there is no limit on how many keys a flow's state may hold |
| FLC-EDGE-005 | Atomic critical state | Partial | AT-EDGE-005 | 0 | Platform flows apply a result with where they stand in one transaction; a gateway writes state every 15 seconds and may repeat an effect after a crash |
| FLC-EDGE-006 | Event-time lateness | Partial | AT-EDGE-006 | 0 | Platform flows hear closed minutes and say late gateway data is not heard; there are no watermarks |
| FLC-EDGE-007 | Episode identity | Partial | AT-EDGE-007 | 0 | Alarms are idempotent and notifications are rate-limited; there are no episodes |
| FLC-EDGE-008 | Offline scope | Partial | AT-EDGE-008 | 0 | A command asked for offline waits and expires; cached context and grant ages are not declared |
| FLC-EDGE-009 | Distributed owner | Partial | AT-EDGE-009 | 1 | Connections are fenced so an old one cannot revoke its replacement; pipeline owners do not exist |
| FLC-EDGE-010 | State migration | Partial | AT-EDGE-010 | 0 | A new revision starts with nothing remembered, said plainly; there is no drain or migration |
| FLC-EDGE-011 | Backpressure | Partial | AT-EDGE-011 | 0 | Traces are disposable and dropped first; there is no evidence reserve under disk pressure |
| FLC-EDGE-012 | Operational intent | Implemented, unverified | AT-EDGE-012 | 0 | A flow has no step that moves equipment, only one that asks the ledger; this acceptance case was not run as written |

## W6: Analytical semantics and engine reuse

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-SEM-001 | Single semantic owner | Partial | AT-SEM-001 | 0 | Metrics owns definitions, certification and lineage; no external model is imported yet |
| FLC-SEM-002 | Grain and joins | Partial | AT-SEM-002 | 0 | Studio joins along links and child tables; nothing refuses a measure that fans out |
| FLC-SEM-003 | Composed metrics | Not started | AT-SEM-003 | 0 | Metrics has no ratios or series |
| FLC-SEM-004 | Currency and time | Partial | AT-SEM-004 | 0 | Exchange rates exist and Studio has relative dates; metrics do not convert currencies or follow fiscal calendars |
| FLC-SEM-005 | Query authority | Partial | AT-SEM-005 | 1 | Native queries apply every joined entity's row scope for the viewer; external sources do not exist |
| FLC-SEM-006 | Query sandbox | Partial | AT-SEM-006 | 0 | Identifiers come from metadata, values are bound, aggregates are whitelisted, and queries run read-only under a timeout; no external engine exists |
| FLC-SEM-007 | Result freshness | Not started | AT-SEM-007 | 0 | Results carry no freshness, partial or approximate flags |
| FLC-SEM-008 | Sharing and schedules | Partial | AT-SEM-008 | 0 | Followed metrics are read with each follower's rights and shared dashboards compute for the viewer; exports and embeds are not reviewed against this case |
| FLC-SEM-009 | Semantic change control | Partial | AT-SEM-009 | 0 | A changed certified definition returns to draft and kept values carry its fingerprint; analyses do not exist to bind |
| FLC-SEM-010 | Engine interoperability | Not started | AT-SEM-010 | 0 | No Wren or DataFusion adapter exists |

## W7: Intelligence experience and governed proposals

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-AI-001 | Typed proposals | Partial | AT-AI-001 | 1 | The assistant acts through typed tools as the person, with writes approved in the conversation; no process or query proposals exist |
| FLC-AI-002 | Prompt injection | Partial | AT-AI-002 | 1 | Tools stay within the person's rights and egress policy; analysis over retrieved rows does not exist |
| FLC-AI-003 | Ambiguity | Not started | AT-AI-003 | 0 | No analysis conversation exists |
| FLC-AI-004 | Evidence-based answers | Not started | AT-AI-004 | 0 | No analysis answers exist |
| FLC-AI-005 | Bounded agent loop | Partial | AT-AI-005 | 0 | The assistant has budgets and egress limits; analysis loops do not exist |
| FLC-AI-006 | Separate action authority | Partial | AT-AI-006 | 0 | Writes need the person's approval in the conversation; nothing proposes a business process |
| FLC-AI-007 | Memory governance | Not started | AT-AI-007 | 0 | Nothing learns examples from feedback |
| FLC-AI-008 | Model changes | Partial | AT-AI-008 | 1 | The assistant has an evaluation harness; runs against a real model wait for a key (BLOCK-MODEL-KEY) |

## W8: Migration, release and operations

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-OPS-001 | Observability | Partial | AT-OPS-001 | 0 | Requests, jobs and events have logs, metrics and traces; runs and effects do not exist |
| FLC-OPS-002 | Fair scheduling | Partial | AT-OPS-002 | 2 | Events and jobs take turns per workspace; process workers do not exist |
| FLC-OPS-003 | Resource budget | Partial | AT-OPS-003 | 0 | Pools and each workspace's share of a replica are bounded; process workers and providers are not |
| FLC-OPS-004 | Restore fencing | Not started | AT-OPS-004 | 0 | No second store or effect ledger exists yet |
| FLC-OPS-005 | Backup completeness | Partial | AT-OPS-005 | 0 | The database is backed up and restored; artifacts, receipts and a process backend do not exist |
| FLC-OPS-006 | Retention | Partial | AT-OPS-006 | 1 | Retention and legal holds apply to records; process histories do not exist |
| FLC-OPS-007 | Revocation response | Partial | AT-OPS-007 | 0 | Apps can be turned off and keys revoked; capabilities cannot |
| FLC-OPS-008 | Portability | Partial | AT-OPS-008 | 0 | Workspaces export their records and Edge flows download as files; solution packs do not exist |
| FLC-MIG-001 | Legacy import | Not started | AT-MIG-001 | 0 | No importer exists |
| FLC-MIG-002 | Shadow comparison | Not started | AT-MIG-002 | 0 | No comparison exists |
| FLC-MIG-003 | One-writer cutover | Not started | AT-MIG-003 | 0 | No subscription generations exist |
| FLC-MIG-004 | In-flight migration | Not started | AT-MIG-004 | 0 | No running processes exist to migrate |
| FLC-MIG-005 | Schema lifecycle | Partial | AT-MIG-005 | 1 | Packages declare resumable backfills run in batches; contraction is by hand |
| FLC-MIG-006 | Rollback truth | Partial | AT-MIG-006 | 0 | Edge treats going back to an old flow as a change reviewed again; solutions do not exist |
| FLC-QUAL-001 | Exact artifacts | Partial | AT-QUAL-001 | 0 | The release manifest pins every component; no logic qualification exists to invalidate |
| FLC-QUAL-002 | Ordinary roles | Partial | AT-QUAL-002 | 0 | Journeys run as ordinary roles; the six reference profiles do not exist |
| FLC-QUAL-003 | Mutation coverage | Partial | AT-QUAL-003 | 0 | Mutants guard existing invariants; none exist yet for this program's |
| FLC-QUAL-004 | Performance evidence | Partial | AT-QUAL-004 | 0 | A soak measures the server; no logic workload is defined |
| FLC-QUAL-005 | Independent fixtures | Partial | AT-QUAL-005 | 0 | Financial journeys check hand-worked numbers; semantic and decision fixtures do not exist |
| FLC-QUAL-006 | Full journeys | Partial | AT-QUAL-006 | 0 | Lead to cash and procure to pay run across apps; the six reference profiles do not exist |
| FLC-QUAL-007 | External qualification | Partial | AT-QUAL-007 | 0 | The registry keeps simulators and mocks from being called qualified; this program's evidence is not yet recorded |
| FLC-QUAL-008 | Maintenance owner | Blocked externally | AT-QUAL-008 | 0 | Named people must take on supported profiles, versions and incidents |

