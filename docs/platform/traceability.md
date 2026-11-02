# Traceability

Generated from [docs/registry/fcp.yaml](../registry/fcp.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Not started | 9 |
| Partial | 45 |
| Implemented, unverified | 10 |
| Verified in software | 128 |
| Blocked externally | 5 |
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
| FCP-POL-001 | Tenant context | Verified in software | AT-POL-001 | 2 | One gate over a table of routes, walked as non-members and as another workspace's administrator. SDK and MCP aliases of these routes do not exist yet, so they are not covered |
| FCP-POL-002 | Scoped delegation | Verified in software | AT-POL-002 | 2 | Workload grant, the caller's rights and the person delegating intersect in one decision. Only one workload route exists so far |
| FCP-POL-003 | Separation of duties | Verified in software | AT-POL-003 | 2 | Self-approval is refused, and production widening needs two people. Capability plans are the only plans so far; release plans arrive with W05 |
| FCP-POL-004 | Immutable approval | Verified in software | AT-POL-004 | 2 | Approval is bound to a digest and policy revision, and rechecked at execution. A digest vector was computed independently in Python; no other SDK language exists yet |
| FCP-POL-005 | Subject lifecycle | Verified in software | AT-POL-005 | 1 | Workforce, partner, workload and emergency subjects decide differently. Partner access through Ferrum accounts is not yet connected to Cloud projects |
| FCP-POL-006 | Federation security | Verified in software | AT-POL-006 | 1 | Wrong audience, stale nonce, reused code and the rest are refused against a stand-in provider; the redirect URI is the server's own, never the caller's. No real identity provider tenant has been exercised, and SAML is not offered |
| FCP-POL-007 | Downstream offboarding | Verified in software | AT-POL-007 | 3 | Tracks each downstream target, shows residual access and retries with nothing held open. The forge and network revokers are test doubles until W03 and W07 add real ones |
| FCP-POL-008 | Policy freshness | Verified in software | AT-POL-008 | 2 | Coverage shows which point runs which revision, and a point past its age refuses. No real enforcement point reports yet; W05 and W07 add them |
| FCP-POL-009 | Emergency access | Verified in software | AT-POL-009 | 2 | Codes work once, for an hour, tell the security administrators and reach project administration only. Offline storage of the codes is the customer's procedure, and no drill has been run with one |
| FCP-POL-010 | Negative authorization matrix | Verified in software | AT-POL-010 | 1 | Walks every entry of the route table. The table is the only way in today; generated SDK and MCP aliases must join it when they exist |
| FCP-SECRET-001 | Secret indirection | Verified in software | AT-SECRET-001 | 2 | Secrets are references sealed per environment, never returned. Leased delivery to a running workload arrives with W05 and W06 |
| FCP-SECRET-002 | Workload attestation | Verified in software | AT-SECRET-002 | 2 | Short-lived signed tokens whose claims come from server records, for one audience, class and job. Attestation of the workload itself depends on the runtime W04 and W05 qualify |
| FCP-SECRET-003 | Root rotation | Verified in software | AT-SECRET-003 | 2 | Signing keys rotate with overlap and enforcement points see the new bundle. The sealing key's rotation is the platform's own, and has no drill yet |
| FCP-SECRET-004 | Key recovery | Partial | AT-SECRET-004 | 0 | Documented, not exercised |
| FCP-SECRET-005 | Credential expiry | Partial | AT-SECRET-005 | 0 | Gateways check expiry; no general policy |
| FCP-SECRET-006 | Environment secret scope | Verified in software | AT-SECRET-006 | 2 | Preview cannot read production's secrets and a public-prefixed name cannot hold one. Delivery into builds is W04's |
| FCP-SECRET-007 | Compromise response | Verified in software | AT-SECRET-007 | 2 | A compromised key stops at once and its unexpired tokens are listed. Running workloads do not yet renew by themselves, so that recovery is unexercised |
| FCP-SECRET-008 | Operator visibility honesty | Partial | AT-SECRET-008 | 0 | Stated for support; not for every hosted profile |

## W02: Resource management

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-RES-001 | Logical resource binding | Verified in software | AT-RES-001 | 2 | A logical name resolves to a managed database or a customer's own, and the workload gets a role of its own. Tested with PostgreSQL only |
| FCP-RES-002 | Idempotent provisioning | Verified in software | AT-RES-002 | 2 | Lost answers, a dead worker and a blind provider, against real PostgreSQL. No billable provider has been exercised |
| FCP-RES-003 | Required resource readiness | Verified in software | AT-RES-003 | 2 | Ready means inspected. An app waits, off, for what it requires. Activation polls every 30 seconds |
| FCP-RES-004 | Tenant database isolation | Verified in software | AT-RES-004 | 2 | Isolation is a database and roles per resource. Shared-database row security is not offered for app resources |
| FCP-RES-005 | Scoped credentials | Verified in software | AT-RES-005 | 2 | Rotation overlaps, then ends the old login and its sessions. Customer-owned databases are not rotated: the credential is theirs |
| FCP-RES-006 | Resource migration | Verified in software | AT-RES-006 | 2 | Sessions really killed mid-backfill. Session advisory locks do not survive a connection pooler; the generation check covers that case but it was not run through one |
| FCP-RES-007 | Provider capability mismatch | Verified in software | AT-RES-007 | 2 | Classes without a provider are refused |
| FCP-RES-008 | Data authority classification | Verified in software | AT-RES-008 | 2 | Detects a resource or a table gone. Corrupted rows are not detected: that takes checksums and integrity queries an app has to declare |
| FCP-RES-009 | Object consistency | Verified in software | AT-RES-009 | 2 | Up to 16 MiB through the broker, with a store double that fails on request. Not yet run against S3 itself |
| FCP-RES-010 | Backup semantics | Verified in software | AT-RES-010 | 2 | Logical copies at a moment, with marks and digests. No point-in-time recovery for app resources, and copies are not encrypted apart from the store |
| FCP-RES-011 | Customer resource detachment | Verified in software | AT-RES-011 | 1 | PostgreSQL. Attaching a customer's bucket is written and has not run against S3 |
| FCP-RES-012 | Destructive deletion controls | Verified in software | AT-RES-012 | 2 | Dependents, holds, retention and ownership block it, and are read again at execution. Which retention rules apply to whom is a customer's decision |
| FCP-RES-013 | Query console safety | Verified in software | AT-RES-013 | 2 | Read-only, on app resources. Elevated writes are not offered at all |
| FCP-RES-014 | Licensing gate | Blocked externally | AT-RES-014 | 2 | The gate refuses to the letter and is verified. Offering Timescale waits for approved terms, and no Timescale provider exists |
| FCP-RES-015 | Resource portability | Verified in software | AT-RES-015 | 1 | PostgreSQL to a second PostgreSQL server and objects to a second store. Roles, grants and outside extensions do not travel, and the manifest says so |
| FCP-EXT-001 | Execution trust classes | Verified in software | AT-EXT-001 | 2 | A bundle saying native is refused however it is signed. Native code comes with the build. Isolated service extensions are not a class yet |
| FCP-EXT-002 | Manifest versioning | Verified in software | AT-EXT-002 | 1 | Unknown fields are refused and named, including new resource fields. No format version: a field this server does not know stops the install |
| FCP-EXT-003 | Capability consent | Verified in software | AT-EXT-003 | 2 | Permissions, resources and hosts are in the consent digest, so a change needs a new consent. Network cost is not estimated |
| FCP-EXT-004 | Host resource scoping | Verified in software | AT-EXT-004 | 2 | Workspace and app come from the host. An attempt to say more, and a foreign handle, are refused and audited. No precompiled wasm fixture calls these: the test drives the host API directly |
| FCP-EXT-005 | UI isolation | Verified in software | AT-EXT-005 | 2 | A port per screen, an allowlist of paths and methods, sandboxed frames. Tested in jsdom, not a browser. A forged cross-origin message has no port to arrive on |
| FCP-EXT-006 | Dependency safety | Verified in software | AT-EXT-006 | 1 | An upgrade that brings something in names it, its hosts and its permissions, and the old consent does not cover it |
| FCP-EXT-007 | Lifecycle cleanup | Verified in software | AT-EXT-007 | 2 | Stopping ends access at once and starting restores it. Removing detaches and keeps the data. Jobs of a stopped app are not covered here |
| FCP-EXT-008 | Certification scope | Verified in software | AT-EXT-008 | 1 | A label is of one digest, with a profile, evidence and a reviewer, and is shown in the plan. Nothing in the server grants one: no certification process exists |
| FCP-EXT-009 | Deprecation migration | Verified in software | AT-EXT-009 | 2 | End of support, replacement and the way to get data out are shown before anyone agrees. The export itself is the app's to provide |
| FCP-EXT-010 | Core invariants | Verified in software | AT-EXT-010 | 1 | An automation goes through hooks and the submitted-document rule like anything else, can only say the four things it can say, and takes an administrator to save. Studio scripts are not covered here |

## W03: Code

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-CODE-001 | Repository lifecycle | Verified in software | AT-CODE-001 | 2 | A crash after the forge made it binds what is there. Against real Gitea 28.2.0 |
| FCP-CODE-002 | Ref consistency | Verified in software | AT-CODE-002 | 1 | Conflicting pushes over HTTPS and SSH race on real Git. Protection is the forge's and holds on both |
| FCP-CODE-003 | Git storage authority | Verified in software | AT-CODE-003 | 1 | One writable instance by design (D-08), so there is no second writer to split with. A killed forge keeps what it acknowledged. No high availability |
| FCP-CODE-004 | Backup completeness | Verified in software | AT-CODE-004 | 1 | History, pull requests and protection come out and a second forge fscks clean. Not a scheduled backup of a whole forge yet: DR-006 covers that gap |
| FCP-CODE-005 | Scoped Git credentials | Verified in software | AT-CODE-005 | 2 | Per-repository tokens, collected once, ended by revocation and by a person leaving |
| FCP-CODE-006 | Server hook restriction | Verified in software | AT-CODE-006 | 1 | DISABLE_GIT_HOOKS is required configuration, and every management path was tried against it |
| FCP-CODE-007 | Import and mirror egress | Verified in software | AT-CODE-007 | 2 | Refused before fetching and again by the forge when fetching. Submodules are not fetched |
| FCP-CODE-008 | Git resource limits | Partial | AT-CODE-008 | 1 | An oversized push fails plainly. Concurrent-clone limits and per-tenant fairness at the forge are not exercised |
| FCP-CODE-009 | PR approval integrity | Verified in software | AT-CODE-009 | 1 | A force-push dismisses approvals and required checks are by name. Real Gitea |
| FCP-CODE-010 | LFS access | Verified in software | AT-CODE-010 | 1 | A known object id from another repository gets nothing. Expiry of a signed URL is Gitea's |
| FCP-CODE-011 | Forge outage isolation | Verified in software | AT-CODE-011 | 1 | What is live needs no forge. A build says why it waits, fails with that reason, and asking again afterwards builds once |
| FCP-CODE-012 | Repository portability | Verified in software | AT-CODE-012 | 1 | Moved to a second real Gitea, every branch to the commit, in plain Git. No other forge product has been tried |

## W04: Build and artifact supply chain

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-BUILD-001 | Webhook authenticity | Verified in software | AT-BUILD-001 | 2 | Signed, replay-safe, and bound to the repository it claims |
| FCP-BUILD-002 | Exact commit checkout | Verified in software | AT-BUILD-002 | 2 | Fetched by commit, checked after, and the moved branch does not appear |
| FCP-BUILD-003 | Builder sandbox | Verified in software | AT-BUILD-003 | 1 | A hostile build reached nothing, on Docker. A container is not a boundary against a kernel exploit: this proves the configuration, not the isolation a shared host needs |
| FCP-BUILD-004 | Production isolation | Verified in software | AT-BUILD-004 | 1 | No network unless a mirror network is named. The mirror path has not been exercised |
| FCP-BUILD-005 | Secret exposure controls | Verified in software | AT-BUILD-005 | 1 | No credential is in reach, so there is nothing to print. Redaction exists for test secrets only |
| FCP-BUILD-006 | Cache trust | Verified in software | AT-BUILD-006 | 2 | A cache per repository and level of trust, at /cache. It goes when it passes 1 GiB, and a second worker on another host starts empty |
| FCP-BUILD-007 | Dependency pinning | Verified in software | AT-BUILD-007 | 1 | Images pinned by digest and the lockfile recorded. Packages a build fetches are as pinned as its lockfile: nothing proxies them |
| FCP-BUILD-008 | Artifact finalization | Verified in software | AT-BUILD-008 | 3 | Measured at finalization and again at promotion, by a process that did not run the build |
| FCP-BUILD-009 | Signing authority | Verified in software | AT-BUILD-009 | 2 | What is signed is what the finalizer computed. An attempt at anything else is refused and recorded. Keys are in the database, not a KMS |
| FCP-BUILD-010 | Provenance privacy | Verified in software | AT-BUILD-010 | 2 | What leaves is a view with nothing of whose it is. Nothing is published to a transparency log |
| FCP-BUILD-011 | Build cancellation | Verified in software | AT-BUILD-011 | 2 | Cancelled, killed or lost, the container goes and the fetch token was already gone |
| FCP-BUILD-012 | Tenant fairness | Verified in software | AT-BUILD-012 | 2 | A cap per workspace. Measured with one flood and one small build, not under sustained load |
| FCP-BUILD-013 | Log safety | Verified in software | AT-BUILD-013 | 2 | Escapes are gone at rest, logs are bounded and scoped. Pages still have to treat them as text |
| FCP-BUILD-014 | Customer runner trust | Verified in software | AT-BUILD-014 | 2 | The finalizer sets assurance from what it knows. A runner's claim goes nowhere near production |

## W05: Deploy, static delivery and domains

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-DEPLOY-001 | Environment separation | Verified in software | AT-DEPLOY-001 | 2 | Capabilities are per environment, and a workload token reaches its own environment only |
| FCP-DEPLOY-002 | Release immutability | Verified in software | AT-DEPLOY-002 | 2 | By digest, and measured again at promotion. Images of what runs are pinned the same way |
| FCP-DEPLOY-003 | Promotion serialization | Verified in software | AT-DEPLOY-003 | 2 | One compare-and-swap on the route generation. The loser fails and has to be planned again |
| FCP-DEPLOY-004 | Readiness gating | Verified in software | AT-DEPLOY-004 | 2 | Every replica has to answer. One that does not leaves what was live, and is stopped |
| FCP-DEPLOY-005 | Migration compatibility | Verified in software | AT-DEPLOY-005 | 2 | Resumes from its checkpoint, and rollback is refused when old code would not work |
| FCP-DEPLOY-006 | Preview access | Verified in software | AT-DEPLOY-006 | 2 | Every name, the address and a forged header. TLS and anything in front of the node are not exercised |
| FCP-DEPLOY-007 | Preview cleanup | Verified in software | AT-DEPLOY-007 | 1 | Two steps that each find their own work, stopped between them, and only what the preview holds |
| FCP-DEPLOY-008 | Graceful rollout | Partial | AT-DEPLOY-008 | 1 | Requests in flight finish and replicas drain. WebSockets do not pass a node, so nothing to reconnect is exercised |
| FCP-DEPLOY-009 | Rollback correctness | Verified in software | AT-DEPLOY-009 | 2 | Refused across a contracting migration or an older schema. A rollback restarts what it ran |
| FCP-DEPLOY-010 | Durable background work | Verified in software | AT-DEPLOY-010 | 2 | Written down before the caller hears anything, delivered at least once with a stable id |
| FCP-DEPLOY-011 | Scheduler semantics | Verified in software | AT-DEPLOY-011 | 2 | DST, missed runs and overlap are decided from the database |
| FCP-DEPLOY-012 | Capability reconsent | Verified in software | AT-DEPLOY-012 | 2 | A release cannot ask beyond what its environment was given, and an approval does not outlive what it was of. Egress capabilities are not enforced by Docker |
| FCP-DEPLOY-013 | Serving independence | Verified in software | AT-DEPLOY-013 | 1 | As a separate process, with nothing else up. Trust files are rotated by hand |

## W06: SSR, private apps and workers

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-WEB-001 | Public/private cache separation | Verified in software | AT-WEB-001 | 1 | Only what a node adds. A CDN in front is not exercised |
| FCP-WEB-002 | Framework conformance | Partial | AT-WEB-002 | 2 | Files and a process on $PORT, on two replicas, streamed. The table in the docs says what is not supported. No framework has been qualified |
| FCP-WEB-003 | Deployment skew | Verified in software | AT-WEB-003 | 2 | Old assets stay for two releases. Mutations from old pages are the application's to version |
| FCP-WEB-004 | Browser origin isolation | Verified in software | AT-WEB-004 | 1 | One host per deployment and cookie domains stripped. A browser was not used: a service-worker scope is decided by origin |
| FCP-WEB-005 | Domain ownership | Verified in software | AT-WEB-005 | 3 | One spelling, unique across every workspace, wildcards overlap |
| FCP-WEB-006 | Origin bypass | Verified in software | AT-WEB-006 | 2 | Name, address, old preview and forged headers. Direct access to a replica depends on the network |
| FCP-WEB-007 | Certificate lifecycle | Partial | AT-WEB-007 | 1 | A name that stops proving itself stops being served and a released one starts again. No certificate has been issued or renewed |
| FCP-WEB-008 | Image and fetch SSRF | Verified in software | AT-WEB-008 | 3 | Checked when named, and again at every connection, IPv6 and mapped addresses included. No redirect is followed. No image optimizer exists to attack |
| FCP-WEB-009 | Request resource budgets | Verified in software | AT-WEB-009 | 1 | A limit per site on a node, and a body limit. Not measured under sustained load |
| FCP-WEB-010 | Upload finalization | Verified in software | AT-WEB-010 | 2 | An intent first, bytes under a temporary name, one commit to make them visible, and a sweep for what was left |
| FCP-WEB-011 | Storefront authority | Partial | AT-WEB-011 | 0 | Commerce recalculates server-side; no storefront profile has been qualified against it |
| FCP-WEB-012 | Headless content lifecycle | Partial | AT-WEB-012 | 1 | A release is bound to the route it was planned against, and a pass expires. There is no content model |
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
| FCP-ACCESS-001 | Reachability versus rights | Verified in software | AT-ACCESS-001 | 2 | The decision only. Nothing here moves packets |
| FCP-ACCESS-002 | Provider administration isolation | Verified in software | AT-ACCESS-002 | 2 | Ferrum's own side. A real provider's boundary is not exercised |
| FCP-ACCESS-003 | Posture trust | Verified in software | AT-ACCESS-003 | 2 | Stale and self-reported posture are treated as unknown. No MDM is connected |
| FCP-ACCESS-004 | Private connectors | Verified in software | AT-ACCESS-004 | 2 | A connector holds a token naming it and asks for its own routes. A real connector does not exist yet |
| FCP-ACCESS-005 | Overlapping addresses | Verified in software | AT-ACCESS-005 | 1 | Routes follow the connector, never the address |
| FCP-ACCESS-006 | Revocation coverage | Verified in software | AT-ACCESS-006 | 2 | Outstanding enforcement points are shown, and token life bounds what is left |
| FCP-ACCESS-007 | Privileged session scope | Verified in software | AT-ACCESS-007 | 1 | A grant names its target and verbs and ends. A PAM service is not connected |
| FCP-ACCESS-008 | Header spoofing | Verified in software | AT-ACCESS-008 | 1 | Stripped at the node and tested with a real upstream. Only a serving node is a data path here |
| FCP-ACCESS-009 | Access outage policy | Verified in software | AT-ACCESS-009 | 2 | Members-only stops after an hour alone, public sites go on, and emergency access is separate |
| FCP-ACCESS-010 | OT access boundary | Verified in software | AT-ACCESS-010 | 1 | OT destinations refuse anything but a site-specific path, with no hardware behind them |

## W08: Distributed execution

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-MSG-001 | Transactional outbox | Verified in software | AT-MSG-001 | 1 | Crashes on both sides of the commit and of publication, as written |
| FCP-MSG-002 | Consumer idempotency | Verified in software | AT-MSG-002 | 1 | `first_time` for consumers that adopt it. Existing consumers keep their own keys |
| FCP-MSG-003 | External ambiguity | Verified in software | AT-MSG-003 | 4 | Provider calls and physical commands. Mail and webhooks are at least once, not unknown |
| FCP-MSG-004 | Lease fencing | Verified in software | AT-MSG-004 | 3 | A worker that lost its lease writes nothing, on a real claim by a real second worker |
| FCP-MSG-005 | Notification recovery | Verified in software | AT-MSG-005 | 1 | Writers do not notify at all, so there is no message to lose |
| FCP-MSG-006 | Cursor correctness | Verified in software | AT-MSG-006 | 2 | A cursor never passes a transaction still open. Retention and resync are not implemented |
| FCP-MSG-007 | Snapshot subscription | Verified in software | AT-MSG-007 | 2 | A position read before the state, past nothing still open. The console polls and does not use it yet |
| FCP-MSG-008 | Contract evolution | Partial | AT-MSG-008 | 0 | A site on another contract is refused plainly. A command newer than a gateway understands is not refused at the gateway yet, and Cloud events carry no version |
| FCP-MSG-009 | Regional authority | Verified in software | AT-MSG-009 | 1 | A witness, two databases and a real cut connection, on one machine. Not across real regions, and promotion is the operator's |
| FCP-MSG-010 | Service delegation | Verified in software | AT-MSG-010 | 2 | Tokens name who they are for, and a forged one is refused and recorded |
| FCP-OPS-001 | Any-replica intake | Implemented, unverified | AT-OPS-001 | 1 | Commands are durable and dedupe; no charger start intent above them |
| FCP-OPS-002 | Exact connection owner | Verified in software | AT-OPS-002 | 1 | Only the owner is given work, and only the gateway given a command can report on it. Through the sync endpoint, no charger involved. Pool members do not yet get each other's device links |
| FCP-OPS-003 | Reconnect generation | Verified in software | AT-OPS-003 | 1 | A closing only ends its own connection, and a replayed journal cannot take a device back. Ordered by the gateway's clock |
| FCP-OPS-004 | Physical intent deduplication | Implemented, unverified | AT-OPS-004 | 1 | Dedupe key and one live command; canonical digest mismatch not checked |
| FCP-OPS-005 | Final deadline gate | Verified in software | AT-OPS-005 | 1 | Simulators only |
| FCP-OPS-006 | Clock correctness | Verified in software | AT-OPS-006 | 2 | Simulators only |
| FCP-OPS-007 | Recovery target binding | Verified in software | AT-OPS-007 | 1 | Simulators only |
| FCP-OPS-008 | Inbound durability | Verified in software | AT-OPS-008 | 1 | Kept with SQLite synchronous=full before the answer. Within that storage fault model, on one machine |
| FCP-OPS-009 | Protocol versus business outcome | Verified in software | AT-OPS-009 | 2 | Charging only once a register moved. Written against OCPP, not against a charger |
| FCP-OPS-010 | Durable correlation | Verified in software | AT-OPS-010 | 1 | From the journal, and the same start gets the same number. Numbers are per gateway: a moved session keeps its device, and a clash across gateways is possible and unlikely |
| FCP-OPS-011 | Safe unknown outcome | Verified in software | AT-OPS-011 | 2 | Simulators only |
| FCP-OPS-012 | Meter evidence | Verified in software | AT-OPS-012 | 2 | Reset, late, missing, unconverted and signed readings are flagged, and only a person settles those. Signatures are not verified. The policy is in code, not per tariff |
| FCP-OPS-013 | Critical quota behavior | Verified in software | AT-OPS-013 | 1 | Past the allowance readings wait, and a stop, a report and a fact go through in one exchange |
| FCP-OPS-014 | Client replay | Verified in software | AT-OPS-014 | 1 | The history from a position, from any replica, and a position it never reached is told to start again |

## W09: Corporate operations

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-SCALE-001 | Single scaler owner | Partial | AT-SCALE-001 | 0 | One HPA for the API; no rule for anything else |
| FCP-SCALE-002 | Connection budgeting | Partial | AT-SCALE-002 | 0 | The API budget is documented; plugins, workers and migrations are not accounted for |
| FCP-SCALE-003 | Stateful scaling | Partial | AT-SCALE-003 | 0 | CNPG replicas only |
| FCP-SCALE-004 | Budget enforcement | Partial | AT-SCALE-004 | 1 | Builds count and a hard budget stops new ones. Runtime hours and storage are not metered, and resources are not admitted against it |
| FCP-SCALE-005 | Critical reserve | Partial | AT-SCALE-005 | 1 | Gateways shed telemetry first; the platform has no reserve |
| FCP-SCALE-006 | Gateway drain | Partial | AT-SCALE-006 | 1 | A device leaving makes in-flight work unknown, without redispatch. No drain of many sockets has been run |
| FCP-SCALE-007 | Noisy neighbor | Partial | AT-SCALE-007 | 2 | A build cap per workspace, a request cap per site, fair events and jobs. Nothing bounds a tenant's share of a shared database |
| FCP-SCALE-008 | Capacity evidence | Partial | AT-SCALE-008 | 0 | Edge rates measured once on one machine; nothing for the platform |
| FCP-UI-001 | Consistent scope UI | Verified in software | AT-UI-001 | 2 | The digest on screen when the form opened goes with the decision, and the server refuses a stale one. No automated browser test |
| FCP-UI-002 | Asynchronous UX | Implemented, unverified | AT-UI-002 | 1 | An operation has an address and all of it comes from the record. Looked at in a browser once, against real processes |
| FCP-UI-003 | Accessible administration | Partial | AT-UI-003 | 0 | Automated checks per app; no manual WCAG review |
| FCP-UI-004 | Localization correctness | Partial | AT-UI-004 | 0 | Locale formatting exists; this work's strings are new |
| FCP-UI-005 | Permission explanation | Implemented, unverified | AT-UI-005 | 1 | A refusal says what it would take, and one list being refused leaves the page working |
| FCP-UI-006 | Operator recovery UX | Verified in software | AT-UI-006 | 1 | Unknown, failed, replaced and lost stay different words. Warehouse and Shop Floor have their own |
| FCP-UI-007 | Bundle provenance | Verified in software | AT-UI-007 | 1 | dist/.source digests the sources and CI checks it. Not checked at install |
| FCP-UI-008 | Audit visibility | Implemented, unverified | AT-UI-008 | 0 | Who, what and detail per project. Partner sessions are not modelled |
| FCP-OBS-001 | Telemetry correlation | Partial | AT-OBS-001 | 0 | Request ids and metrics; no traces |
| FCP-OBS-002 | Tenant telemetry isolation | Partial | AT-OBS-002 | 0 | Row-level security covers audit and job data; no customer log service |
| FCP-OBS-003 | Bounded cardinality | Verified in software | AT-OBS-003 | 1 | No tenant or site in a label |
| FCP-OBS-004 | Operational alert coverage | Partial | AT-OBS-004 | 0 | Rules with somewhere to go. None has fired, and nothing routes them |
| FCP-OBS-005 | Metering idempotency | Verified in software | AT-OBS-005 | 2 | Replays, late usage and corrections, explained line by line. Builds are the only meter recording itself |
| FCP-OBS-006 | Money precision | Verified in software | AT-OBS-006 | 2 | Exact numbers in the database, rounded a line at a time, half up, with currencies of no minor unit handled. Refunds are corrections. No invoice |
| FCP-OBS-007 | Budget transparency | Verified in software | AT-OBS-007 | 1 | A month in progress says it is an estimate, and late usage is counted on the statement. No page shows it yet |
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
| FCP-DR-001 | Complete backups | Partial | AT-DR-001 | 0 | An inventory and an order. Not run whole |
| FCP-DR-002 | Restore isolation | Verified in software | AT-DR-002 | 3 | A real pg_dump and pg_restore: what was waiting does not run, and a person releases the rest. Noticed by database identity and timeline |
| FCP-DR-003 | Tenant-level recovery | Not started | AT-DR-003 | 0 | Nothing exists |
| FCP-DR-004 | Key recovery test | Partial | AT-DR-004 | 0 | Documented, not exercised |
| FCP-DR-005 | Failover fencing | Verified in software | AT-DR-005 | 1 | It stops before its time at the witness runs out, and does not take the right back. Gateways have their own fencing. One machine |
| FCP-DR-006 | Object and Git integrity | Partial | AT-DR-006 | 0 | A database restore runs in CI; no Git or object checks |
| FCP-DR-007 | Capacity recovery | Partial | AT-DR-007 | 1 | Fair dispatch; no reconnect storm test |
| FCP-DR-008 | Update trust | Partial | AT-DR-008 | 0 | Signed gateway updates and app bundles; no rollback-protected metadata |
| FCP-DR-009 | Runbook exercise | Partial | AT-DR-009 | 0 | Written. Nobody who did not write it has followed it |
| FCP-DR-010 | RPO RTO evidence | Not started | AT-DR-010 | 0 | Nothing exists |

## W10: Qualification and rollout

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-QUAL-001 | Requirement traceability | Verified in software | AT-QUAL-001 | 1 | Evidence taken away, a missing file and an unknown blocker each fail the check |
| FCP-QUAL-002 | Security baseline | Partial | AT-QUAL-002 | 0 | No pinned ASVS or SSDF mapping |
| FCP-QUAL-003 | Independent review | Blocked externally | AT-QUAL-003 | 0 | Needs an independent assessor |
| FCP-QUAL-004 | Provider qualification | Blocked externally | AT-QUAL-004 | 0 | Needs real provider tenants |
| FCP-QUAL-005 | Hardware qualification | Blocked externally | AT-QUAL-005 | 0 | Needs equipment and a site |
| FCP-QUAL-006 | Golden journeys | Partial | AT-QUAL-006 | 0 | Ten of twelve have their steps exercised in parts, none run whole, two not run. The check keeps that visible |
| FCP-QUAL-007 | Mutation and fault coverage | Partial | AT-QUAL-007 | 0 | Mutants for what they cover; none for new platform work yet |
| FCP-QUAL-008 | Release evidence integrity | Partial | AT-QUAL-008 | 0 | Generated, attested image; not signed per bundle |
| FCP-QUAL-009 | Supported limits | Partial | AT-QUAL-009 | 2 | Upgrades get 501, oversized bodies 413, replicas past 16 are refused. Limits in the documentation are not measured |

