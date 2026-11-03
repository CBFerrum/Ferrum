# Traceability

Generated from [docs/registry/fcp.yaml](../registry/fcp.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Partial | 19 |
| Verified in software | 173 |
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
| FCP-BASE-006 | Frozen and forward CI | Verified in software | AT-BASE-006 | 2 | On 2026-11-03 Edge's main moved to 782c0b0 while Ferrum's frozen run on main built --locked against 45ea519; the forward lane followed Edge to 782c0b0, and checked and tested the server, on this fix's branch, after its first run on main stopped for want of a database. Edge's own two lanes have not been put through the same |
| FCP-BASE-007 | Evidence classification | Verified in software | AT-BASE-007 | 1 | A simulator or a mock cannot be labelled qualified while the real thing is missing, and the check fails if one tries. The older registry's `basis` is coarser than these states |
| FCP-BASE-008 | Feature inventory | Verified in software | AT-BASE-008 | 1 | A repository a lock file pins and nothing here can read is listed as unread, as are images and production, not as absent. Git dependencies only |

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
| FCP-SECRET-004 | Key recovery | Verified in software | AT-SECRET-004 | 1 | With the database gone, the key comes from where an operator keeps it: nothing to break in a circle, no key in the backup, and another key opens nothing. There is no KMS here |
| FCP-SECRET-005 | Credential expiry | Verified in software | AT-SECRET-005 | 4 | A gateway past its lease stops starting work, and a node that has heard nothing for an hour refuses members with public sites going on. There is no setting to carry on regardless. Not tried on equipment. A connector past its token is refused by the same check, and no connector process exists to leave offline |
| FCP-SECRET-006 | Environment secret scope | Verified in software | AT-SECRET-006 | 2 | Preview cannot read production's secrets and a public-prefixed name cannot hold one. Delivery into builds is W04's |
| FCP-SECRET-007 | Compromise response | Verified in software | AT-SECRET-007 | 2 | A compromised key stops at once and its unexpired tokens are listed. Running workloads do not yet renew by themselves, so that recovery is unexercised |
| FCP-SECRET-008 | Operator visibility honesty | Verified in software | AT-SECRET-008 | 1 | Says what an operator can reach in each profile, and the check fails a document claiming an operator cannot see. Nobody has reviewed it, and there is no customer-managed-key profile to review it against |

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
| FCP-CODE-008 | Git resource limits | Partial | AT-CODE-008 | 2 | An oversized push fails plainly, and so does a tree deeper than 2048, refused by the forge's own git as it arrives from a client that does not hold itself back. Clones and pushes take turns per workspace at the gate in front of the forge, which also bounds their processes and CPU, one each, and one kept waiting is refused with 429: six of one workspace at two at a time took three packs' time while another's took one. There is no storage quota per workspace, and the forge here has none of its own |
| FCP-CODE-009 | PR approval integrity | Verified in software | AT-CODE-009 | 1 | A force-push dismisses approvals and required checks are by name. Real Gitea |
| FCP-CODE-010 | LFS access | Verified in software | AT-CODE-010 | 1 | A known object id from another repository gets nothing. Expiry of a signed URL is Gitea's |
| FCP-CODE-011 | Forge outage isolation | Verified in software | AT-CODE-011 | 1 | What is live needs no forge. A build says why it waits, fails with that reason, and asking again afterwards builds once |
| FCP-CODE-012 | Repository portability | Verified in software | AT-CODE-012 | 1 | Moved to a second real Gitea, every branch to the commit, in plain Git. No other forge product has been tried |

## W04: Build and artifact supply chain

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-BUILD-001 | Webhook authenticity | Verified in software | AT-BUILD-001 | 2 | Signed, replay-safe, and bound to the repository it claims |
| FCP-BUILD-002 | Exact commit checkout | Verified in software | AT-BUILD-002 | 2 | Fetched by commit, checked after, and the moved branch does not appear |
| FCP-BUILD-003 | Builder sandbox | Verified in software | AT-BUILD-003 | 1 | A hostile build reached nothing, on Docker and under gVisor (CI's gvisor job, where it saw gVisor's kernel instead of the host's). That proves the configuration and that gVisor holds it; qualifying a runtime class for shared hosts needs a real cluster with egress enforced, and a review |
| FCP-BUILD-004 | Production isolation | Verified in software | AT-BUILD-004 | 1 | No network unless a mirror network is named, under Docker and under gVisor. The mirror path has not been exercised |
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
| FCP-DEPLOY-008 | Graceful rollout | Verified in software | AT-DEPLOY-008 | 2 | A node stopped mid-request finishes it, and an open WebSocket ends with the node and the client is told, so it reconnects to what serves next. A socket stays on its replica until either side closes it. Uploads go to object storage, not through a node. On one machine |
| FCP-DEPLOY-009 | Rollback correctness | Verified in software | AT-DEPLOY-009 | 2 | Refused across a contracting migration or an older schema. A rollback restarts what it ran |
| FCP-DEPLOY-010 | Durable background work | Verified in software | AT-DEPLOY-010 | 2 | Written down before the caller hears anything, delivered at least once with a stable id |
| FCP-DEPLOY-011 | Scheduler semantics | Verified in software | AT-DEPLOY-011 | 2 | DST, missed runs and overlap are decided from the database |
| FCP-DEPLOY-012 | Capability reconsent | Verified in software | AT-DEPLOY-012 | 2 | A release cannot ask beyond what its environment was given, and an approval does not outlive what it was of. Egress capabilities are not enforced by Docker |
| FCP-DEPLOY-013 | Serving independence | Verified in software | AT-DEPLOY-013 | 1 | As a separate process, with nothing else up. Trust files are rotated by hand |

## W06: SSR, private apps and workers

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-WEB-001 | Public/private cache separation | Verified in software | AT-WEB-001 | 1 | Only what a node adds. A CDN in front is not exercised |
| FCP-WEB-002 | Framework conformance | Partial | AT-WEB-002 | 2 | Node on two replicas, with an answer streamed through a node as it is made, WebSockets through a node, and files beside it. The table in the docs says what is not supported. No framework has been qualified |
| FCP-WEB-003 | Deployment skew | Verified in software | AT-WEB-003 | 2 | Old assets stay for two releases. Mutations from old pages are the application's to version |
| FCP-WEB-004 | Browser origin isolation | Verified in software | AT-WEB-004 | 1 | One host per deployment and cookie domains stripped. A browser was not used: a service-worker scope is decided by origin |
| FCP-WEB-005 | Domain ownership | Verified in software | AT-WEB-005 | 3 | One spelling, unique across every workspace, wildcards overlap |
| FCP-WEB-006 | Origin bypass | Verified in software | AT-WEB-006 | 2 | Name, address, old preview and forged headers. Direct access to a replica depends on the network |
| FCP-WEB-007 | Certificate lifecycle | Partial | AT-WEB-007 | 1 | A name that stops proving itself stops being served and a released one starts again. No certificate has been issued or renewed |
| FCP-WEB-008 | Image and fetch SSRF | Verified in software | AT-WEB-008 | 3 | Checked when named, and again at every connection, IPv6 and mapped addresses included. No redirect is followed. No image optimizer exists to attack |
| FCP-WEB-009 | Request resource budgets | Verified in software | AT-WEB-009 | 1 | A limit per site on a node, and a body limit. Not measured under sustained load |
| FCP-WEB-010 | Upload finalization | Verified in software | AT-WEB-010 | 2 | An intent first, bytes under a temporary name, one commit to make them visible, and a sweep for what was left |
| FCP-WEB-011 | Storefront authority | Verified in software | AT-WEB-011 | 1 | Prices sent from a browser are ignored, totals are refused, another customer's order is not there, and only the gateway's user can mark a payment, for the right amount and currency. No payment provider, and no deployed storefront driving it |
| FCP-WEB-012 | Headless content lifecycle | Verified in software | AT-WEB-012 | 2 | Content entries are drafted, reviewed and published by someone other than the author; a changed version, a draft written from a revision since replaced, or a new entry over a live one is refused; a site's key reads only what is published, with locale fallback; a preview key stops at its expiry. A release is bound to the route it was planned against, and a pass expires. Content is a title and text, with no media or schema per type, and a preview key reads every draft in the workspace, not one |
| FCP-AI-001 | Agent capability boundary | Verified in software | AT-AI-001 | 4 | A stand-in that does whatever a ticket tells it gets nothing its person could not do, changes wait for that person, and physical work can only be proposed. Nothing relies on a model's judgement, and no real model was tried. A client running elsewhere is bound by its token, not by this |
| FCP-AI-002 | Model/provider policy | Verified in software | AT-AI-002 | 1 | A model the operator has not allowed is not used, and what a workspace forbids does not leave it, whichever model is asked. A model has no region here: the operator chooses the address it is reached at |
| FCP-AI-003 | Approval integrity | Verified in software | AT-AI-003 | 3 | A change approved and then altered, or approved too long ago, does not run. A command has no way to change after approval, a plan's approval names the digest shown, and nobody approves their own, whatever roles they hold |
| FCP-AI-004 | Development sandbox | Verified in software | AT-AI-004 | 3 | A development token exists for previews and not production, reaches only that environment, and says so in its scope. The server starter takes only such a token on a workstation and refuses a production one pasted in. The starter is the client made here: other clients are their makers'. Cloud does not check that what a preview is bound to is synthetic, and a raw provider credential is outside what a token can stop |
| FCP-AI-005 | Budget and cancellation | Verified in software | AT-AI-005 | 1 | Past a workspace's allowance the provider is not called, a turn stops at 25 steps, a failing service is tried four times, and one turn runs per thread. Usage is shown in tokens, not money. Scripted models |
| FCP-AI-006 | Evidence and abstention | Verified in software | AT-AI-006 | 1 | For physical work: a proposal says when what it rests on was read, and old evidence or a silent site is refused as unknown. An invented id is not found anywhere. The ERP's generic tools carry no freshness |
| FCP-AI-007 | Data boundary disclosure | Verified in software | AT-AI-007 | 1 | The guide says a client you run sends what it reads to its model's provider, and the check fails a document that claims otherwise. Cloud cannot see or limit such a client |
| FCP-AI-008 | Operational autonomy | Verified in software | AT-AI-008 | 1 | An agent has three tools: two to look and one to propose an operation the device declares. Nothing reaches a PLC or a safety function, which stay with their makers. Not an evaluation of any model |

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
| FCP-MSG-008 | Contract evolution | Verified in software | AT-MSG-008 | 3 | An old build ignores a field it does not know and refuses a meaning it does not. A gateway has this once it is built against this contract |
| FCP-MSG-009 | Regional authority | Verified in software | AT-MSG-009 | 1 | A witness, two databases and a real cut connection, on one machine. Not across real regions, and promotion is the operator's |
| FCP-MSG-010 | Service delegation | Verified in software | AT-MSG-010 | 2 | Tokens name who they are for, and a forged one is refused and recorded |
| FCP-OPS-001 | Any-replica intake | Verified in software | AT-OPS-001 | 2 | Durable before it answers, and the same through any server, on real gateways. No charger made by anyone |
| FCP-OPS-002 | Exact connection owner | Verified in software | AT-OPS-002 | 1 | Only the owner is given work, and only the gateway given a command can report on it. Through the sync endpoint, no charger involved. Pool members do not yet get each other's device links |
| FCP-OPS-003 | Reconnect generation | Verified in software | AT-OPS-003 | 1 | A closing only ends its own connection, and a replayed journal cannot take a device back. Ordered by the gateway's clock |
| FCP-OPS-004 | Physical intent deduplication | Verified in software | AT-OPS-004 | 2 | The same key for different work is a conflict, also at the same moment, and a command sent to a gateway that dies is not sent again. No real charger |
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
| FCP-SCALE-001 | Single scaler owner | Verified in software | AT-SCALE-001 | 1 | One HPA, and CI refuses a second owner of the count. Not applied to a cluster |
| FCP-SCALE-002 | Connection budgeting | Verified in software | AT-SCALE-002 | 2 | Against real PostgreSQL: replicas at their pools' maximum use the role's whole budget, one more is told the database is busy (503), so is a request past its pool, and the reserve lets a migration in. CI checks the manifests add up. Not run on a cluster at its maximum |
| FCP-SCALE-003 | Stateful scaling | Partial | AT-SCALE-003 | 0 | CNPG replicas only |
| FCP-SCALE-004 | Budget enforcement | Verified in software | AT-SCALE-004 | 2 | Builds, running hours and held resources are counted, once an hour however many look, and a hard budget stops new ones with what is up left up. Storage is not metered by size |
| FCP-SCALE-005 | Critical reserve | Verified in software | AT-SCALE-005 | 2 | On a gateway: only plain samples count against the budget and go first, commands and evidence are outside it, and with almost no disk it takes no new work and says why. Not tried on a disk that really filled, and the platform's own database has no such reserve |
| FCP-SCALE-006 | Gateway drain | Verified in software | AT-SCALE-006 | 2 | A real gateway killed with a hundred chargers on it and work out on twenty: nothing in doubt is sent again, waiting work and new work get through. A kill, not a graceful scale-in, on one machine |
| FCP-SCALE-007 | Noisy neighbor | Verified in software | AT-SCALE-007 | 4 | Queries: a workspace's API requests take turns within half of each replica's database connections, say how long they waited, and are refused and counted past five seconds. Flooded from 64 threads on one laptop, another workspace's p95 went from 13 to 25 ms, and to 145 ms with the share off. Backlogs: one with twenty times the work does not make others wait. Builds: a cap per workspace; sites, a request cap each. Per replica, measured once on one machine, and the database's CPU is not divided |
| FCP-SCALE-008 | Capacity evidence | Partial | AT-SCALE-008 | 1 | Percentiles, backlog and fairness measured for minutes on one laptop, so they describe that and not a deployment. Edge rates were measured the same way |
| FCP-UI-001 | Consistent scope UI | Verified in software | AT-UI-001 | 2 | The digest on screen when the form opened goes with the decision, and the server refuses a stale one. No automated browser test |
| FCP-UI-002 | Asynchronous UX | Verified in software | AT-UI-002 | 1 | In a browser against real processes: a resource asked for with the controller stopped, its provider refusing, and then back. The page followed each attempt and the reason, a reload and a second session showed the same, someone with no standing got nothing, and the retry finished. Relative times do not tick between updates |
| FCP-UI-003 | Accessible administration | Partial | AT-UI-003 | 0 | axe finds nothing at desktop or phone width on Cloud's pages, inside the platform's bar, once the shell's own findings are fixed (Ferrum-Frontend#9, merged and in the image). It found real problems first. The main pages were worked by keyboard alone, with visible focus and refusals announced. No screen reader, and nobody who relies on assistive technology |
| FCP-UI-004 | Localization correctness | Partial | AT-UI-004 | 0 | Dates and amounts go through the browser's locale. Right to left, Cloud's pages, the frame's drawer and the bar's skip link hold up at phone width with Ferrum-Frontend#9, merged and in the image. Strings are English only |
| FCP-UI-005 | Permission explanation | Verified in software | AT-UI-005 | 2 | In a browser: a viewer pressing a button their role does not allow is told so beside it, and that a project administrator can change it, without being told who. A refusal is announced in a live region. Proof is offered where it would cure the refusal |
| FCP-UI-006 | Operator recovery UX | Verified in software | AT-UI-006 | 1 | Unknown, failed, replaced and lost stay different words. Warehouse and Shop Floor have their own |
| FCP-UI-007 | Bundle provenance | Verified in software | AT-UI-007 | 1 | dist/.source digests the sources and CI checks it. Not checked at install |
| FCP-UI-008 | Audit visibility | Verified in software | AT-UI-008 | 3 | Support grants in the console: who, what they reach, why, given and ending, how often used, each request as refused or let in, and a way to end them, tried in a browser. Deployment and policy changes are in each project's record |
| FCP-OBS-001 | Telemetry correlation | Verified in software | AT-OBS-001 | 1 | One id from a request through other processes, also confirmed on a real server. There is no tracing backend, and logs go where a deployment sends them |
| FCP-OBS-002 | Tenant telemetry isolation | Verified in software | AT-OBS-002 | 1 | Row-level security refuses another workspace's operations and events, however the question is put. There is no customer log service to refuse |
| FCP-OBS-003 | Bounded cardinality | Verified in software | AT-OBS-003 | 1 | No tenant or site in a label |
| FCP-OBS-004 | Operational alert coverage | Verified in software | AT-OBS-004 | 3 | Drift, backlogs, operations nobody can vouch for, backup age, access past its time, terms and certificates near their end: each fires on its condition under promtool and not otherwise, once however many replicas report it, routes to its owner and every critical one to on-call under amtool, and a real Alertmanager delivers a cause with its runbook and holds back what it explains. The receivers there are stand-ins: real paging needs the deployment's addresses and keys |
| FCP-OBS-005 | Metering idempotency | Verified in software | AT-OBS-005 | 2 | Replays, late usage and corrections, explained line by line. Builds are the only meter recording itself |
| FCP-OBS-006 | Money precision | Verified in software | AT-OBS-006 | 2 | Exact numbers in the database, rounded a line at a time, half up, with currencies of no minor unit handled. Refunds are corrections. No invoice |
| FCP-OBS-007 | Budget transparency | Verified in software | AT-OBS-007 | 1 | A month in progress says it is an estimate, and late usage is counted on the statement. No page shows it yet |
| FCP-OBS-008 | Incident process | Partial | AT-OBS-008 | 0 | Severity, roles, containment, communication and review are written. No drill has been run: that takes people |
| FCP-OBS-009 | Abuse controls | Partial | AT-OBS-009 | 2 | Stopping one project and nothing else, an appeal, and the record, also on a real server. Rules hold a project's new builds for a person, with evidence, when it builds past a budget or its services stay at their CPU limit as mining does, read from docker stats on a real container, and an alert says so; a hold a person lifted stays lifted. Phishing, spam and public hosting are found by people's reports and handled with the same stop, appeal and record. Egress is neither enforced nor watched: that needs the qualified runtime class, which is blocked (D-09) |
| FCP-CLOUD-001 | Tenant lifecycle | Verified in software | AT-CLOUD-001 | 3 | Provisioning a workspace is the operator's create_tenant; active, grace, restricted and closed are the operator's, each on record. Closing keeps everything and stops what runs; after its date Cloud destroys what it provably made, and the workspace is deleted only when nothing is held elsewhere, files first. Closing rounds and retirement run again start nothing twice, a resource waiting for a person waits for them, and a store failing part way leaves every row. Self-service signup and billing are not here |
| FCP-CLOUD-002 | Official partner scope | Blocked externally | AT-CLOUD-002 | 0 | Partner terms are a legal and commercial decision |
| FCP-CLOUD-003 | Support least privilege | Verified in software | AT-CLOUD-003 | 3 | A health-only key reaches its report and is refused on records, administration, reports, an app's routes, browser-session routes, two-factor setup and a live socket, each attempt on the customer's list as refused. Run against a real server, and by deploy/smoke.sh. Calendar and identity-provider keys are scoped apart |
| FCP-CLOUD-004 | No standing partner admin | Verified in software | AT-CLOUD-004 | 2 | A partner's account in a workspace of its own reaches nothing of a customer's, and a grant does not renew itself. Software only |
| FCP-CLOUD-005 | Support revocation | Verified in software | AT-CLOUD-005 | 2 | A key stops at once, one customer's revocation leaves another's grant alone, and every use stays listed by request. A diagnostic already running is not interrupted. Sessions are tracked by key |
| FCP-CLOUD-006 | Residency coverage | Verified in software | AT-CLOUD-006 | 2 | Resources, backups, exports and releases, by what a deployment says of itself: nothing can check that. An attached resource cannot say where it is, so one held to a region refuses them. Cloud calls no model. No real region |
| FCP-CLOUD-007 | Account restriction safety | Verified in software | AT-CLOUD-007 | 2 | A restricted account keeps its sites up and its work running, can read, export, restore, go back to an earlier release and close a project, and starts nothing new, by a person or a push. Also tried on a real server. An unpaid account is not ended or deleted by anything here |
| FCP-CLOUD-008 | Customer controlled profile | Partial | AT-CLOUD-008 | 1 | What it depends on, what stops when cut off, and what an operator can do. Each row has its evidence, and they have not been run together: no customer network has |
| FCP-CLOUD-009 | Service ownership | Partial | AT-CLOUD-009 | 1 | A provider going away tells each project what it has there and what moving it would take, once, and stops taking new work on the day. Names and support periods are the company's to fill in, and none are |
| FCP-DR-001 | Complete backups | Verified in software | AT-DR-001 | 2 | Database, repositories, artifacts and the key come back together somewhere new and are checked before anything is claimed, on one machine. Customer databases come back through restore plans, tested apart. No real region or provider |
| FCP-DR-002 | Restore isolation | Verified in software | AT-DR-002 | 3 | A real pg_dump and pg_restore: what was waiting does not run, and a person releases the rest. Noticed by database identity and timeline |
| FCP-DR-003 | Tenant-level recovery | Verified in software | AT-DR-003 | 2 | Through real dump and restore, on the kernel's schema and Cloud's. The task itself has not been run, and a full ERP schema has not been taken back: circular references were read from a real one. Files are not checked against the store, and a resource made since is left at its provider |
| FCP-DR-004 | Key recovery test | Partial | AT-DR-004 | 1 | Recovery without the primary database works through the operator's copy of the key, with no plaintext in the backup. There is no KMS path to remove, so that half of the scenario cannot be run here |
| FCP-DR-005 | Failover fencing | Verified in software | AT-DR-005 | 1 | It stops before its time at the witness runs out, and does not take the right back. Gateways have their own fencing. One machine |
| FCP-DR-006 | Object and Git integrity | Verified in software | AT-DR-006 | 2 | Real repositories with a damaged pack and a missing LFS object. It is run by hand after a restore, and has not been run on a restored Gitea |
| FCP-DR-007 | Capacity recovery | Partial | AT-DR-007 | 2 | Charger and MQTT ports let new connections in at a steady rate, 100 a second unless set. A hundred chargers at 25 a second were all back in 3.3 s on one machine, with waiting work through in 5.5 s, while one already connected was answered in 0.2 ms. There is no profile target to meet, so no recovery is claimed to be fast enough |
| FCP-DR-008 | Update trust | Verified in software | AT-DR-008 | 4 | A gateway refuses a release that is unsigned, expired, not newer or was rolled back here, and a bad file never replaces what runs. A node refuses a snapshot that is older or not signed by a key it holds. A bundle changed after signing is refused and an older revocation list is ignored. An app store's listing has no expiry of its own |
| FCP-DR-009 | Runbook exercise | Partial | AT-DR-009 | 0 | Written. Nobody who did not write it has followed it |
| FCP-DR-010 | RPO RTO evidence | Partial | AT-DR-010 | 1 | Dump, restore and start measured once, on one laptop and a small database. No zone, database or provider failed, so no objective is claimed |

## W10: Qualification and rollout

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| FCP-QUAL-001 | Requirement traceability | Verified in software | AT-QUAL-001 | 1 | Evidence taken away, a missing file and an unknown blocker each fail the check |
| FCP-QUAL-002 | Security baseline | Partial | AT-QUAL-002 | 0 | ASVS 4.0.3 and SSDF 1.1 pinned and mapped, with what is open. A map, not an assessment: nobody independent has looked |
| FCP-QUAL-003 | Independent review | Blocked externally | AT-QUAL-003 | 0 | Needs an independent assessor |
| FCP-QUAL-004 | Provider qualification | Blocked externally | AT-QUAL-004 | 0 | Needs real provider tenants |
| FCP-QUAL-005 | Hardware qualification | Blocked externally | AT-QUAL-005 | 0 | Needs equipment and a site |
| FCP-QUAL-006 | Golden journeys | Partial | AT-QUAL-006 | 0 | J07 runs whole on real gateway processes and J09 is exercised in parts; ten of twelve in parts, none whole against real providers or equipment, J12 not run. The check keeps that visible |
| FCP-QUAL-007 | Mutation and fault coverage | Verified in software | AT-QUAL-007 | 2 | Every survivor led to a stronger test: seven at the last full Cloud run, and three since, one of which had been changing the wrong function. Every mutant added in this work is caught. CI checks each still finds its text; the full sets run weekly and by hand, and were not rerun after the last changes |
| FCP-QUAL-008 | Release evidence integrity | Verified in software | AT-QUAL-008 | 3 | For a release: an artifact that is not what was built or signed is refused before it is promoted, and a node refuses bytes that are not what they are named. Ferrum's own image is attested, and that is checked at deployment only on a cluster |
| FCP-QUAL-009 | Supported limits | Verified in software | AT-QUAL-009 | 2 | An upgrade other than a WebSocket gets 501, an oversized body 413, more than 16 replicas is refused, a site's sockets past its limit 429, and a hard budget refuses what would pass it. The documented figures were measured once, on a laptop, and no envelope is qualified |

