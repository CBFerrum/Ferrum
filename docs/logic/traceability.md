# Traceability

Generated from [docs/registry/flc.yaml](../registry/flc.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Not started | 24 |
| Partial | 91 |
| Implemented, unverified | 2 |
| Verified in software | 6 |
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
| FLC-COMP-001 | Strict parsing | Partial | AT-COMP-001 | 0 | Package YAML and Studio specs refuse unknown keys; duplicate JSON keys are not detected anywhere, and no solution source exists |
| FLC-COMP-002 | Business types | Partial | AT-COMP-002 | 0 | Money and tax go through ferrum-calc decimals; expressions have no typed money, quantity or reference |
| FLC-COMP-003 | Deterministic expressions | Partial | AT-COMP-003 | 0 | Edge flows evaluate purely in exact decimals; kernel CEL reads the wall clock when no time is supplied |
| FLC-COMP-004 | Canonical identity | Partial | AT-COMP-004 | 1 | Cloud's canonical JSON is now the platform's (crates/logic), and capabilities and invocations are digested with it; logic definitions do not exist yet to hash |
| FLC-COMP-005 | Dependency closure | Partial | AT-COMP-005 | 0 | Edge copies a reviewed subflow in at check time and stops a flow whose subflow changed; nothing else resolves dependencies |
| FLC-COMP-006 | Graph semantics | Partial | AT-COMP-006 | 0 | Edge flows are checked for cycles and missing steps; process graphs do not exist |
| FLC-COMP-007 | Bounded iteration | Partial | AT-COMP-007 | 0 | An Edge flow cannot loop and bounds one message's work; processes with bounded loops do not exist |
| FLC-COMP-008 | Effect typing | Partial | AT-COMP-008 | 1 | Plugins are refused external calls inside hooks at run time; no compiler checks effects, and automations send mail from their transaction |
| FLC-COMP-009 | Data-flow policy | Partial | AT-COMP-009 | 0 | Field masks and read permissions shape what a reader sees; messages, traces and destinations of tenant logic are not checked against them |
| FLC-COMP-010 | Source maps | Partial | AT-COMP-010 | 0 | Edge traces name the step a message reached; nothing maps logic runs to source revisions |
| FLC-HOOK-001 | Phase ordering | Partial | AT-HOOK-001 | 0 | Native hooks run derive, validate, change checks and writes in order; tenant derivations have no stage of their own |
| FLC-HOOK-002 | Pure hook boundary | Partial | AT-HOOK-002 | 1 | Enforced for plugins; an automation's e-mail action sends from inside its transaction |
| FLC-HOOK-003 | Competing writes | Partial | AT-HOOK-003 | 0 | Hooks filling the same field must be ordered or the server does not start; apps have not declared fills yet, and tenant rules do not exist |
| FLC-HOOK-004 | Bounded prefetch | Partial | AT-HOOK-004 | 0 | A plugin's reads in a hook go through its declared permissions and a cap on host calls; tenant rules with declared prefetch do not exist |
| FLC-HOOK-005 | Equivalent mutation paths | Partial | AT-HOOK-005 | 0 | The 96 direct writes were classified (FERRUM-PLATFORM-011); no single test sends one invalid record through every path |
| FLC-HOOK-006 | After-commit durability | Partial | AT-HOOK-006 | 1 | Events and queued mail commit with the change; an automation's e-mail does not |
| FLC-HOOK-007 | Rejection evidence | Not started | AT-HOOK-007 | 0 | A refused change is counted, not recorded as evidence |
| FLC-HOOK-008 | Event recursion | Partial | AT-HOOK-008 | 0 | Automations stop at depth 3 and events carry causation; there is no work budget per correlation across apps |

## W3: Durable processes and human work

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FLC-PROC-001 | Idempotent starts | Partial | AT-PROC-001 | 1 | A saga starts in the caller's transaction with a dedupe key; no process pins a release at acceptance |
| FLC-PROC-002 | Native action receipts | Verified in software | AT-PROC-002 | 1 | The receipt commits in the change's transaction: a repeat gets it, two racing get one change, a failure before the commit leaves neither (capability_it; mutants capability-no-receipt, capability-same-key-other-input). A database crash mid-commit is PostgreSQL's to keep atomic; receipts are not yet removed after their retention |
| FLC-PROC-003 | External effect identity | Partial | AT-PROC-003 | 1 | A saga step runs under the same key on every attempt; no input digest is bound to it |
| FLC-PROC-004 | Signal before wait | Not started | AT-PROC-004 | 0 | Nothing waits for a signal |
| FLC-PROC-005 | Signal spoofing | Not started | AT-PROC-005 | 0 | Nothing receives process signals |
| FLC-PROC-006 | Current authority | Partial | AT-PROC-006 | 1 | Jobs and automations recheck who they run as when they run; processes do not exist |
| FLC-PROC-007 | Replay determinism | Not started | AT-PROC-007 | 0 | No process runtime exists |
| FLC-PROC-008 | Parallel race | Not started | AT-PROC-008 | 0 | Sagas run one step at a time; nothing runs branches |
| FLC-PROC-009 | Business rework | Partial | AT-PROC-009 | 0 | A record's workflow can move back between states of one status, without a limit on how often |
| FLC-PROC-010 | Cancellation | Partial | AT-PROC-010 | 0 | Jobs and sagas separate a request to stop from stopping, and operational commands separate cancellation requested from cancelled; processes do not exist |
| FLC-PROC-011 | Compensation failure | Partial | AT-PROC-011 | 1 | A saga whose undo fails says what a person must still undo; processes do not exist |
| FLC-PROC-012 | Worker fencing | Partial | AT-PROC-012 | 1 | A job's lease token fences a worker that lost it; no process worker exists |
| FLC-PROC-013 | History bounds | Not started | AT-PROC-013 | 0 | No process history exists |
| FLC-PROC-014 | Snapshot plus subscription | Not started | AT-PROC-014 | 0 | Record pages update live; no run status stream exists, and the live updates were not reviewed against this case |
| FLC-HUMAN-001 | Independent approval | Partial | AT-HUMAN-001 | 1 | A workflow transition may refuse the record's creator whatever roles they hold; process tasks do not exist |
| FLC-HUMAN-002 | Input-bound approval | Partial | AT-HUMAN-002 | 1 | Approvals bound to a digest, by someone other than the requester, under a policy revision, are the platform's rules now (crates/logic/src/approval.rs); process tasks that use them come with W3, and a record's own approval is still bound to its version |
| FLC-HUMAN-003 | Timeout race | Not started | AT-HUMAN-003 | 0 | No task has a deadline |
| FLC-HUMAN-004 | Delegation | Not started | AT-HUMAN-004 | 0 | No task can be delegated |
| FLC-HUMAN-005 | Calendar semantics | Partial | AT-HUMAN-005 | 0 | Workspaces have a time zone; there is no versioned business calendar for deadlines |
| FLC-HUMAN-006 | Task evidence | Not started | AT-HUMAN-006 | 0 | No task exists to scope evidence to |
| FLC-HUMAN-007 | Case work | Not started | AT-HUMAN-007 | 0 | No discretionary case tasks exist |
| FLC-HUMAN-008 | Manual disposition | Partial | AT-HUMAN-008 | 0 | An operational command's unknown outcome is settled by evidence or a person; business effects have no disposition |

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

