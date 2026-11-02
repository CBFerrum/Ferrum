# Traceability

Generated from [docs/registry/fcp.yaml](../registry/fcp.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Not started | 89 |
| Partial | 83 |
| Implemented, unverified | 14 |
| Verified in software | 7 |
| Blocked externally | 4 |
| **All** | **197** |

## W00: Baseline and regression reconciliation

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-BASE-001 | Frozen inventory | Partial | AT-BASE-001 | 0 | Image digests need a token with read:packages, and no production tenant is reachable from here |
| FCP-BASE-002 | Existing capability reuse | Partial | AT-BASE-002 | 0 | Mapped by reading and probing; each entry is confirmed when its workstream starts |
| FCP-BASE-003 | Prior invariant regressions | Verified in software | AT-BASE-003 | 6 | Simulators only: no physical equipment |
| FCP-BASE-004 | Release pins | Verified in software | AT-BASE-004 | 1 | Generated and checked in CI; a signed evidence bundle is FCP-QUAL-008 |
| FCP-BASE-005 | Worktree safety | Verified in software | AT-BASE-005 | 1 | Reports dirty files and unused patches; it does not stop anyone committing them |
| FCP-BASE-006 | Frozen and forward CI | Implemented, unverified | AT-BASE-006 | 0 | Both lanes exist in Ferrum and Edge; Ferrum's forward lane has not run yet |
| FCP-BASE-007 | Evidence classification | Partial | AT-BASE-007 | 0 | This registry records evidence states; the older registry's `basis` is coarser |
| FCP-BASE-008 | Feature inventory | Partial | AT-BASE-008 | 0 | Lists every repository this token can see; images and production are recorded as unread, not absent |

## W01: Identity, policy and control plane

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-POL-001 | Tenant context | Partial | AT-POL-001 | 2 | Holds for today's APIs; project and environment scope does not exist |
| FCP-POL-002 | Scoped delegation | Partial | AT-POL-002 | 0 | Apps and support narrow a user's rights; no workload grant intersects them |
| FCP-POL-003 | Separation of duties | Partial | AT-POL-003 | 1 | Commands and revisions have it; capability changes do not |
| FCP-POL-004 | Immutable approval | Not started | AT-POL-004 | 0 | No approval bound to a canonical digest of artifact, configuration and plan |
| FCP-POL-005 | Subject lifecycle | Partial | AT-POL-005 | 1 | Subjects have stable ids; workload, partner and build subjects are not modelled |
| FCP-POL-006 | Federation security | Partial | AT-POL-006 | 1 | Tested against a stand-in provider; no real IdP tenant |
| FCP-POL-007 | Downstream offboarding | Partial | AT-POL-007 | 1 | Sessions, API keys and OAuth grants end at once; nothing tracks Git, peers or privileged credentials |
| FCP-POL-008 | Policy freshness | Not started | AT-POL-008 | 0 | Nothing records which policy revision an enforcement point runs |
| FCP-POL-009 | Emergency access | Partial | AT-POL-009 | 1 | Support's incident level is not break-glass: it depends on the same sign-in |
| FCP-POL-010 | Negative authorization matrix | Not started | AT-POL-010 | 0 | Roles are tested per app; there is no matrix over every route |
| FCP-SECRET-001 | Secret indirection | Partial | AT-SECRET-001 | 2 | Sealed values and brokered credentials; no leases or application secret references |
| FCP-SECRET-002 | Workload attestation | Not started | AT-SECRET-002 | 0 | Nothing exists |
| FCP-SECRET-003 | Root rotation | Partial | AT-SECRET-003 | 1 | The sealing key rotates; no signing or trust-root rotation |
| FCP-SECRET-004 | Key recovery | Partial | AT-SECRET-004 | 0 | Documented, not exercised |
| FCP-SECRET-005 | Credential expiry | Partial | AT-SECRET-005 | 0 | Gateways check expiry; no general policy |
| FCP-SECRET-006 | Environment secret scope | Not started | AT-SECRET-006 | 0 | Nothing exists |
| FCP-SECRET-007 | Compromise response | Not started | AT-SECRET-007 | 0 | Nothing exists |
| FCP-SECRET-008 | Operator visibility honesty | Partial | AT-SECRET-008 | 0 | Stated for support; not for every hosted profile |

## W02: Resource management

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-RES-001 | Logical resource binding | Not started | AT-RES-001 | 0 | Nothing exists |
| FCP-RES-002 | Idempotent provisioning | Not started | AT-RES-002 | 0 | Nothing exists |
| FCP-RES-003 | Required resource readiness | Not started | AT-RES-003 | 0 | Nothing exists |
| FCP-RES-004 | Tenant database isolation | Partial | AT-RES-004 | 1 | Row-level security protects platform tables; app-private resources do not exist |
| FCP-RES-005 | Scoped credentials | Not started | AT-RES-005 | 0 | Nothing exists |
| FCP-RES-006 | Resource migration | Not started | AT-RES-006 | 0 | Nothing exists |
| FCP-RES-007 | Provider capability mismatch | Not started | AT-RES-007 | 0 | Nothing exists |
| FCP-RES-008 | Data authority classification | Not started | AT-RES-008 | 0 | Nothing exists |
| FCP-RES-009 | Object consistency | Partial | AT-RES-009 | 0 | Uploads reconcile; no general object resource class |
| FCP-RES-010 | Backup semantics | Partial | AT-RES-010 | 0 | CNPG backups for the platform database; no resource-level consistency points |
| FCP-RES-011 | Customer resource detachment | Not started | AT-RES-011 | 0 | Nothing exists |
| FCP-RES-012 | Destructive deletion controls | Not started | AT-RES-012 | 0 | Nothing exists |
| FCP-RES-013 | Query console safety | Not started | AT-RES-013 | 0 | Nothing exists |
| FCP-RES-014 | Licensing gate | Not started | AT-RES-014 | 0 | Nothing exists |
| FCP-RES-015 | Resource portability | Not started | AT-RES-015 | 0 | Nothing exists |
| FCP-EXT-001 | Execution trust classes | Implemented, unverified | AT-EXT-001 | 2 | No isolated service extension class |
| FCP-EXT-002 | Manifest versioning | Implemented, unverified | AT-EXT-002 | 1 | Unknown fields are refused; resource fields are not declared yet |
| FCP-EXT-003 | Capability consent | Partial | AT-EXT-003 | 1 | Consent digest for permissions; no resources, network or cost |
| FCP-EXT-004 | Host resource scoping | Partial | AT-EXT-004 | 1 | Host derives tenant scope; no resource handles |
| FCP-EXT-005 | UI isolation | Implemented, unverified | AT-EXT-005 | 0 | Sandboxed frames and narrowed tokens |
| FCP-EXT-006 | Dependency safety | Partial | AT-EXT-006 | 0 | Dependencies shown in the plan; no privilege roll-up |
| FCP-EXT-007 | Lifecycle cleanup | Partial | AT-EXT-007 | 1 | Uninstall retains data; no resource bindings to revoke |
| FCP-EXT-008 | Certification scope | Partial | AT-EXT-008 | 0 | Signatures exist; no certification labels tied to a profile |
| FCP-EXT-009 | Deprecation migration | Not started | AT-EXT-009 | 0 | Nothing exists |
| FCP-EXT-010 | Core invariants | Partial | AT-EXT-010 | 0 | Automations run through permissions; not tested as an adversary |

## W03: Code

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-CODE-001 | Repository lifecycle | Not started | AT-CODE-001 | 0 | Nothing exists |
| FCP-CODE-002 | Ref consistency | Not started | AT-CODE-002 | 0 | Nothing exists |
| FCP-CODE-003 | Git storage authority | Not started | AT-CODE-003 | 0 | Nothing exists |
| FCP-CODE-004 | Backup completeness | Not started | AT-CODE-004 | 0 | Nothing exists |
| FCP-CODE-005 | Scoped Git credentials | Not started | AT-CODE-005 | 0 | Nothing exists |
| FCP-CODE-006 | Server hook restriction | Not started | AT-CODE-006 | 0 | Nothing exists |
| FCP-CODE-007 | Import and mirror egress | Not started | AT-CODE-007 | 0 | Nothing exists |
| FCP-CODE-008 | Git resource limits | Not started | AT-CODE-008 | 0 | Nothing exists |
| FCP-CODE-009 | PR approval integrity | Not started | AT-CODE-009 | 0 | Nothing exists |
| FCP-CODE-010 | LFS access | Not started | AT-CODE-010 | 0 | Nothing exists |
| FCP-CODE-011 | Forge outage isolation | Not started | AT-CODE-011 | 0 | Nothing exists |
| FCP-CODE-012 | Repository portability | Not started | AT-CODE-012 | 0 | Nothing exists |

## W04: Build and artifact supply chain

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-BUILD-001 | Webhook authenticity | Not started | AT-BUILD-001 | 0 | Nothing exists |
| FCP-BUILD-002 | Exact commit checkout | Not started | AT-BUILD-002 | 0 | Nothing exists |
| FCP-BUILD-003 | Builder sandbox | Not started | AT-BUILD-003 | 0 | Nothing exists |
| FCP-BUILD-004 | Production isolation | Not started | AT-BUILD-004 | 0 | Nothing exists |
| FCP-BUILD-005 | Secret exposure controls | Not started | AT-BUILD-005 | 0 | Nothing exists |
| FCP-BUILD-006 | Cache trust | Not started | AT-BUILD-006 | 0 | Nothing exists |
| FCP-BUILD-007 | Dependency pinning | Not started | AT-BUILD-007 | 0 | Nothing exists |
| FCP-BUILD-008 | Artifact finalization | Not started | AT-BUILD-008 | 0 | Nothing exists |
| FCP-BUILD-009 | Signing authority | Not started | AT-BUILD-009 | 0 | Nothing exists |
| FCP-BUILD-010 | Provenance privacy | Not started | AT-BUILD-010 | 0 | Nothing exists |
| FCP-BUILD-011 | Build cancellation | Not started | AT-BUILD-011 | 0 | Nothing exists |
| FCP-BUILD-012 | Tenant fairness | Not started | AT-BUILD-012 | 0 | Nothing exists |
| FCP-BUILD-013 | Log safety | Not started | AT-BUILD-013 | 0 | Nothing exists |
| FCP-BUILD-014 | Customer runner trust | Not started | AT-BUILD-014 | 0 | Nothing exists |

## W05: Deploy, static delivery and domains

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-DEPLOY-001 | Environment separation | Not started | AT-DEPLOY-001 | 0 | Nothing exists |
| FCP-DEPLOY-002 | Release immutability | Not started | AT-DEPLOY-002 | 0 | Nothing exists |
| FCP-DEPLOY-003 | Promotion serialization | Not started | AT-DEPLOY-003 | 0 | Nothing exists |
| FCP-DEPLOY-004 | Readiness gating | Not started | AT-DEPLOY-004 | 0 | Nothing exists |
| FCP-DEPLOY-005 | Migration compatibility | Not started | AT-DEPLOY-005 | 0 | Nothing exists |
| FCP-DEPLOY-006 | Preview access | Not started | AT-DEPLOY-006 | 0 | Nothing exists |
| FCP-DEPLOY-007 | Preview cleanup | Not started | AT-DEPLOY-007 | 0 | Nothing exists |
| FCP-DEPLOY-008 | Graceful rollout | Not started | AT-DEPLOY-008 | 0 | Nothing exists |
| FCP-DEPLOY-009 | Rollback correctness | Not started | AT-DEPLOY-009 | 0 | Nothing exists |
| FCP-DEPLOY-010 | Durable background work | Not started | AT-DEPLOY-010 | 0 | Nothing exists |
| FCP-DEPLOY-011 | Scheduler semantics | Not started | AT-DEPLOY-011 | 0 | Nothing exists |
| FCP-DEPLOY-012 | Capability reconsent | Not started | AT-DEPLOY-012 | 0 | Nothing exists |
| FCP-DEPLOY-013 | Serving independence | Not started | AT-DEPLOY-013 | 0 | Nothing exists |

## W06: SSR, private apps and workers

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-WEB-001 | Public/private cache separation | Not started | AT-WEB-001 | 0 | Nothing exists |
| FCP-WEB-002 | Framework conformance | Not started | AT-WEB-002 | 0 | Nothing exists |
| FCP-WEB-003 | Deployment skew | Not started | AT-WEB-003 | 0 | Nothing exists |
| FCP-WEB-004 | Browser origin isolation | Partial | AT-WEB-004 | 0 | Isolated apps have their own origins; customer sites do not exist |
| FCP-WEB-005 | Domain ownership | Partial | AT-WEB-005 | 1 | One HTTP token per host; no TXT proof, generations, wildcard rules or tombstones |
| FCP-WEB-006 | Origin bypass | Partial | AT-WEB-006 | 0 | Unknown hosts are refused for the shell; no customer-site ingress exists |
| FCP-WEB-007 | Certificate lifecycle | Partial | AT-WEB-007 | 0 | `/_tls/ask` gates on-demand certificates; no lifecycle or rate-limit handling |
| FCP-WEB-008 | Image and fetch SSRF | Partial | AT-WEB-008 | 0 | Plugins' fetch and gateways check destinations; no customer runtime exists |
| FCP-WEB-009 | Request resource budgets | Partial | AT-WEB-009 | 0 | Bodies are limited per route; no customer runtime exists |
| FCP-WEB-010 | Upload finalization | Partial | AT-WEB-010 | 1 | Uploads scan and finalize; not offered to customer applications |
| FCP-WEB-011 | Storefront authority | Partial | AT-WEB-011 | 0 | Commerce recalculates server-side; no storefront profile has been qualified against it |
| FCP-WEB-012 | Headless content lifecycle | Not started | AT-WEB-012 | 0 | Nothing exists |
| FCP-AI-001 | Agent capability boundary | Partial | AT-AI-001 | 1 | Typed tools under the caller's rights; no capability boundary for development tools |
| FCP-AI-002 | Model/provider policy | Partial | AT-AI-002 | 1 | Egress policy per workspace; no model catalog |
| FCP-AI-003 | Approval integrity | Partial | AT-AI-003 | 1 | Agents' commands wait for a person unless an envelope covers them |
| FCP-AI-004 | Development sandbox | Not started | AT-AI-004 | 0 | Nothing exists |
| FCP-AI-005 | Budget and cancellation | Partial | AT-AI-005 | 0 | Budgets per workspace |
| FCP-AI-006 | Evidence and abstention | Not started | AT-AI-006 | 0 | Nothing exists |
| FCP-AI-007 | Data boundary disclosure | Partial | AT-AI-007 | 0 | The egress policy is shown to administrators; nothing for local clients |
| FCP-AI-008 | Operational autonomy | Implemented, unverified | AT-AI-008 | 1 | No safety function depends on Ferrum; not an AI evaluation |

## W07: Access, network and endpoints

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-ACCESS-001 | Reachability versus rights | Not started | AT-ACCESS-001 | 0 | Nothing exists |
| FCP-ACCESS-002 | Provider administration isolation | Not started | AT-ACCESS-002 | 0 | Nothing exists |
| FCP-ACCESS-003 | Posture trust | Not started | AT-ACCESS-003 | 0 | Nothing exists |
| FCP-ACCESS-004 | Private connectors | Not started | AT-ACCESS-004 | 0 | Nothing exists |
| FCP-ACCESS-005 | Overlapping addresses | Not started | AT-ACCESS-005 | 0 | Nothing exists |
| FCP-ACCESS-006 | Revocation coverage | Not started | AT-ACCESS-006 | 0 | Nothing exists |
| FCP-ACCESS-007 | Privileged session scope | Not started | AT-ACCESS-007 | 0 | Nothing exists |
| FCP-ACCESS-008 | Header spoofing | Partial | AT-ACCESS-008 | 0 | Trusted-proxy handling for client addresses; no identity assertions |
| FCP-ACCESS-009 | Access outage policy | Not started | AT-ACCESS-009 | 0 | Nothing exists |
| FCP-ACCESS-010 | OT access boundary | Not started | AT-ACCESS-010 | 0 | Nothing exists |

## W08: Distributed execution

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-MSG-001 | Transactional outbox | Implemented, unverified | AT-MSG-001 | 1 | Acceptance case not run as written |
| FCP-MSG-002 | Consumer idempotency | Partial | AT-MSG-002 | 1 | Per consumer, by convention; no shared inbox |
| FCP-MSG-003 | External ambiguity | Partial | AT-MSG-003 | 1 | The ledger has OutcomeUnknown; other external calls do not |
| FCP-MSG-004 | Lease fencing | Implemented, unverified | AT-MSG-004 | 2 | Acceptance case not run as written |
| FCP-MSG-005 | Notification recovery | Implemented, unverified | AT-MSG-005 | 1 | Writers do not NOTIFY; dispatchers poll |
| FCP-MSG-006 | Cursor correctness | Not started | AT-MSG-006 | 0 | Nothing exists |
| FCP-MSG-007 | Snapshot subscription | Not started | AT-MSG-007 | 0 | Nothing exists |
| FCP-MSG-008 | Contract evolution | Partial | AT-MSG-008 | 0 | Contract schema versions on commands only |
| FCP-MSG-009 | Regional authority | Not started | AT-MSG-009 | 0 | Nothing exists |
| FCP-MSG-010 | Service delegation | Partial | AT-MSG-010 | 0 | Users are delegated through context; no workload identity |
| FCP-OPS-001 | Any-replica intake | Implemented, unverified | AT-OPS-001 | 1 | Commands are durable and dedupe; no charger start intent above them |
| FCP-OPS-002 | Exact connection owner | Partial | AT-OPS-002 | 0 | One gateway owns a station's socket in process; no ownership record or generation across gateways |
| FCP-OPS-003 | Reconnect generation | Partial | AT-OPS-003 | 0 | A reconnect replaces the local socket; no generation survives a move between gateways |
| FCP-OPS-004 | Physical intent deduplication | Implemented, unverified | AT-OPS-004 | 1 | Dedupe key and one live command; canonical digest mismatch not checked |
| FCP-OPS-005 | Final deadline gate | Verified in software | AT-OPS-005 | 1 | Simulators only |
| FCP-OPS-006 | Clock correctness | Verified in software | AT-OPS-006 | 2 | Simulators only |
| FCP-OPS-007 | Recovery target binding | Verified in software | AT-OPS-007 | 1 | Simulators only |
| FCP-OPS-008 | Inbound durability | Partial | AT-OPS-008 | 0 | Replies follow an in-memory hand-over, not a durable write |
| FCP-OPS-009 | Protocol versus business outcome | Partial | AT-OPS-009 | 1 | Commands end Completed on the charger's reply; no session domain |
| FCP-OPS-010 | Durable correlation | Not started | AT-OPS-010 | 0 | Transaction ids are in process memory |
| FCP-OPS-011 | Safe unknown outcome | Verified in software | AT-OPS-011 | 2 | Simulators only |
| FCP-OPS-012 | Meter evidence | Partial | AT-OPS-012 | 0 | Readings keep source time and quality; no billing-grade meter evidence |
| FCP-OPS-013 | Critical quota behavior | Partial | AT-OPS-013 | 0 | Quotas hold readings and leave commands alone; not tested for stop and report paths |
| FCP-OPS-014 | Client replay | Partial | AT-OPS-014 | 0 | Status can be read back; no replay cursor for clients |

## W09: Corporate operations

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-SCALE-001 | Single scaler owner | Partial | AT-SCALE-001 | 0 | One HPA for the API; no rule for anything else |
| FCP-SCALE-002 | Connection budgeting | Partial | AT-SCALE-002 | 0 | The API budget is documented; plugins, workers and migrations are not accounted for |
| FCP-SCALE-003 | Stateful scaling | Partial | AT-SCALE-003 | 0 | CNPG replicas only |
| FCP-SCALE-004 | Budget enforcement | Partial | AT-SCALE-004 | 1 | Storage quota enforced; no budget or spend cap |
| FCP-SCALE-005 | Critical reserve | Partial | AT-SCALE-005 | 1 | Gateways shed telemetry first; the platform has no reserve |
| FCP-SCALE-006 | Gateway drain | Not started | AT-SCALE-006 | 0 | Nothing exists |
| FCP-SCALE-007 | Noisy neighbor | Partial | AT-SCALE-007 | 2 | Events and jobs are fair per workspace; no quota on shared resources |
| FCP-SCALE-008 | Capacity evidence | Partial | AT-SCALE-008 | 0 | Edge rates measured once on one machine; nothing for the platform |
| FCP-UI-001 | Consistent scope UI | Not started | AT-UI-001 | 0 | Nothing exists |
| FCP-UI-002 | Asynchronous UX | Partial | AT-UI-002 | 0 | Jobs survive a refresh; no operation record for platform work |
| FCP-UI-003 | Accessible administration | Partial | AT-UI-003 | 0 | Automated checks per app; no manual WCAG review |
| FCP-UI-004 | Localization correctness | Partial | AT-UI-004 | 0 | Locale formatting exists; this work's strings are new |
| FCP-UI-005 | Permission explanation | Partial | AT-UI-005 | 0 | Administrators can ask why; ordinary denials are short |
| FCP-UI-006 | Operator recovery UX | Partial | AT-UI-006 | 0 | Warehouse and Shop Floor show uncertainty; nothing for platform operations |
| FCP-UI-007 | Bundle provenance | Implemented, unverified | AT-UI-007 | 0 | `dist/.source` digests each app's sources; not checked at install |
| FCP-UI-008 | Audit visibility | Partial | AT-UI-008 | 0 | Audit and evidence exist; no platform approvals to show |
| FCP-OBS-001 | Telemetry correlation | Partial | AT-OBS-001 | 0 | Request ids and metrics; no traces |
| FCP-OBS-002 | Tenant telemetry isolation | Partial | AT-OBS-002 | 0 | Row-level security covers audit and job data; no customer log service |
| FCP-OBS-003 | Bounded cardinality | Partial | AT-OBS-003 | 0 | Metrics carry no tenant label |
| FCP-OBS-004 | Operational alert coverage | Partial | AT-OBS-004 | 0 | Doctor and metrics exist; no alert rules |
| FCP-OBS-005 | Metering idempotency | Partial | AT-OBS-005 | 0 | Daily usage counts; no event ledger or price plans |
| FCP-OBS-006 | Money precision | Partial | AT-OBS-006 | 0 | Exact decimals in the platform; no invoice lines |
| FCP-OBS-007 | Budget transparency | Not started | AT-OBS-007 | 0 | Nothing exists |
| FCP-OBS-008 | Incident process | Not started | AT-OBS-008 | 0 | Nothing exists |
| FCP-OBS-009 | Abuse controls | Not started | AT-OBS-009 | 0 | Nothing exists |
| FCP-CLOUD-001 | Tenant lifecycle | Partial | AT-CLOUD-001 | 1 | Tenants, account states and exports exist; no signup, plan or closure workflow |
| FCP-CLOUD-002 | Official partner scope | Blocked externally | AT-CLOUD-002 | 0 | Partner terms are a legal and commercial decision |
| FCP-CLOUD-003 | Support least privilege | Implemented, unverified | AT-CLOUD-003 | 1 | Named, expiring and audited; a partner identity is not modelled |
| FCP-CLOUD-004 | No standing partner admin | Partial | AT-CLOUD-004 | 1 | No standing partner access exists; nothing proves it stays so |
| FCP-CLOUD-005 | Support revocation | Implemented, unverified | AT-CLOUD-005 | 1 | Revoked at once; active session tracking is by API key |
| FCP-CLOUD-006 | Residency coverage | Not started | AT-CLOUD-006 | 0 | Nothing exists |
| FCP-CLOUD-007 | Account restriction safety | Implemented, unverified | AT-CLOUD-007 | 1 | Restricted keeps reading and export |
| FCP-CLOUD-008 | Customer controlled profile | Not started | AT-CLOUD-008 | 0 | Nothing exists |
| FCP-CLOUD-009 | Service ownership | Not started | AT-CLOUD-009 | 0 | Nothing exists |
| FCP-DR-001 | Complete backups | Partial | AT-DR-001 | 0 | Platform database backed up; no complete inventory |
| FCP-DR-002 | Restore isolation | Not started | AT-DR-002 | 0 | Nothing exists |
| FCP-DR-003 | Tenant-level recovery | Not started | AT-DR-003 | 0 | Nothing exists |
| FCP-DR-004 | Key recovery test | Partial | AT-DR-004 | 0 | Documented, not exercised |
| FCP-DR-005 | Failover fencing | Partial | AT-DR-005 | 1 | Gateways fence on restore and epoch; no regional failover |
| FCP-DR-006 | Object and Git integrity | Partial | AT-DR-006 | 0 | A database restore runs in CI; no Git or object checks |
| FCP-DR-007 | Capacity recovery | Partial | AT-DR-007 | 1 | Fair dispatch; no reconnect storm test |
| FCP-DR-008 | Update trust | Partial | AT-DR-008 | 0 | Signed gateway updates and app bundles; no rollback-protected metadata |
| FCP-DR-009 | Runbook exercise | Partial | AT-DR-009 | 0 | Runbooks written; drills not recorded |
| FCP-DR-010 | RPO RTO evidence | Not started | AT-DR-010 | 0 | Nothing exists |

## W10: Qualification and rollout

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-QUAL-001 | Requirement traceability | Partial | AT-QUAL-001 | 0 | This registry maps requirements to evidence; no supported profile yet |
| FCP-QUAL-002 | Security baseline | Partial | AT-QUAL-002 | 0 | No pinned ASVS or SSDF mapping |
| FCP-QUAL-003 | Independent review | Blocked externally | AT-QUAL-003 | 0 | Needs an independent assessor |
| FCP-QUAL-004 | Provider qualification | Blocked externally | AT-QUAL-004 | 0 | Needs real provider tenants |
| FCP-QUAL-005 | Hardware qualification | Blocked externally | AT-QUAL-005 | 0 | Needs equipment and a site |
| FCP-QUAL-006 | Golden journeys | Not started | AT-QUAL-006 | 0 | Nothing exists |
| FCP-QUAL-007 | Mutation and fault coverage | Partial | AT-QUAL-007 | 0 | Mutants for what they cover; none for new platform work yet |
| FCP-QUAL-008 | Release evidence integrity | Partial | AT-QUAL-008 | 0 | Generated, attested image; not signed per bundle |
| FCP-QUAL-009 | Supported limits | Partial | AT-QUAL-009 | 0 | Limits written for what exists |

