# Threat model

What is defended and where the lines are. [security.md](../security.md) describes the controls in
detail. This is a working document, written from reading the code, and has not been reviewed
independently (`BLOCK-SECURITY-REVIEW`).

## Trust boundaries

| Boundary | Enforced by | Weakest point |
|---|---|---|
| Tenant to tenant | Forced row-level security with `ferrum_tenant()`, a role that cannot bypass it | `system_tx`: code that opens one is trusted to filter by tenant |
| Person to record | One rule set for reads and lists (`RowScope`), checked to agree in `rules_it` | Linked titles of open entities show records the reader cannot open |
| Workspace to platform | Tenant administrators cannot reach other tenants | Cluster-level tasks run with full access |
| Native app to platform | None: native apps are trusted code, reviewed and compiled in | A bad dependency in any app |
| WASM plugin to platform | Fuel, memory, deadline, declared hosts and permissions, one import | Plugin output reaches apps as data |
| Isolated app interface to workspace | Opaque origin or `{app}--{workspace}` hosts, narrowed tokens | Uploads flagged as trusted |
| Public pages to server | Rate limits per address, signed links, no session | Budgets are per replica for some limits |
| Identity provider to workspace | A `scim` key opens SCIM only; groups grant only mapped roles | Whoever holds the key can create members |
| Assistant to workspace | Acts as the person; writes need approval | Approval is not bound to an exact proposal (`FERRUM-AI-001`) |
| Platform to gateway | Off until `managed: true`. The site limits what a definition may open, and what is on the machine wins | Whoever can publish a definition chooses what a managed gateway connects to, within `allow` |
| Gateway to gateway | Signed requests from listed keys, rising times, and what was asked kept in the journal | The link is not encrypted, and a replay within the clock skew after a restart reaches only what is kept |
| Flow to equipment | No code, no cycles, simulated and reviewed by someone else. Commands go through the ledger as automation | A reviewer who approves what they did not read. Effects outside the ledger: alarms, notifications, exports |
| Operator to customer data | None technically: operators can read the database | Stated plainly: there is no customer-held key |

## Surfaces and how they are held

| Surface | Threat | Mitigation | Evidence |
|---|---|---|---|
| Plugin hook | A rolled-back document already paid for | `http.fetch` and `sockets.*` refused in hooks | `after_commit_it` |
| Plugin call | A runaway guest holds a transaction | Deadlines on both sides; epoch interruption | `runtime::tests` |
| Plugin HTTP | SSRF | Declared hosts, private addresses refused, DNS pinned, no redirects | `host.rs`; not tested here |
| Webhook delivery | SSRF, slow endpoint | The same vetting; leased, no transaction across it | `events_it` |
| Device definition | Runs something, opens a port, reads a file on a gateway | Hosts by `allow`, ports by `listen`, drivers from `drivers_dir`, certificates from its own folder, serial under `/dev` | `transport::tests` |
| Flow | Loops, floods the ledger, moves equipment | Cycles refused, work per message bounded, requests rate limited, issued as automation | `flow::tests`, `edge_it` |
| Site update | An old or forged build | A key installed with the site, expiry, version, hash, rollback | `update::tests` |
| Webhook at a site | Anyone on the network posts readings | A bearer token, compared by hash, on a port the site named. Not encrypted | `protocols_it` |
| Sign-in | Credential stuffing | Argon2id, lockouts, shared rate limits, dummy hash for unknown users | `identity_it` |
| Sessions | Theft, stale access | `token_version` plus NOTIFY; rows per device; sockets recheck | `lifecycle_it` |
| SCIM | A filter that matches too much | Unknown filters are errors | `lifecycle_it` |
| Identity linking | Takeover through a reused email | Linked by subject, on proof | `identity_it`, `sso_it` |
| File uploads | Malware, decompression bombs | Size limits. *Gap:* no malware scanning (`FERRUM-OPS-007`) | |
| Stored secrets | Database or backup theft | Sealed with AES-256-GCM, bound to their place; rotation | `secret_it`, `platform_it` |
| Supply chain | A poisoned build | Pinned bases, attested promotion, advisory register | Not yet run in CI |
| Site link | A stolen or replayed request moves equipment | Ed25519 per device, public keys only on the server, a timestamp that must rise. Quarantine ends a key's epoch at once | `ops_it`, mutants `ops-replay-accepted`, `ops-signature-unchecked` |
| Old gateway | A replaced holder keeps commanding | An epoch per domain; its reports join the history and change nothing; it is fenced at its next exchange and stops at the end of its lease without one | `ops_it`, `recovery_it` |
| Lost gateway disk | Its private key and operators' PIN hashes | Quarantine the device. PINs are short, so the hashes only slow an attacker: the disk must be encrypted | `docs/limits.md`; not tested |
| Site console | Someone on the site's network | Off by default, binds to `127.0.0.1` unless told otherwise, PIN with lockout. No TLS of its own: it belongs on a segmented network. `/api/status` is unauthenticated and shows counts only | `recovery_it`; not reviewed |
| Unattended retries | A timeout sends a robot twice | Intent is durable before the call; what is in doubt is never resent | `recovery_it`, mutant `site-blind-retry` |
| Agents | Text steers an agent into moving something | Typed tools, read and propose only; nothing runs without a person | `ops_it`, mutant `ops-ai-queues-directly` |
| Modbus, plain MQTT | No authentication on the wire | Only what the site's configuration names can be written. Segment the network; use TLS at the broker | Compatibility; not tested on a real network |
| Prompt injection | Content steers the assistant | Writes need approval. *Gap:* no adversarial tests (`FERRUM-AI-005`) | |

## Privileged operations

These run without row-level limits or change who can do what, and each is audited: deprovisioning,
SCIM, erasure, replays, plugin installation, permission changes, secret reads, support sessions
(`user_session.actor`).

## Customer logic (the Logic Platform program)

What a customer writes is treated as program input that will run with real rights
([docs/logic](../logic/README.md)). Rows marked *planned* describe what the program builds; their
evidence column fills in as each lands, and nothing here is claimed before its test exists.

| Surface | Threat | Mitigation | Evidence |
|---|---|---|---|
| Solution source and imported packs | Duplicate keys that read differently in two parsers, remote schemas fetched at compile time, oversized nesting | Strict parser: duplicate keys, unknown keys, remote references and depth beyond the cap refused before anything resolves | *Planned (W2)* |
| A pack's requested capabilities | A pack grants itself an administrator's action | Declaring is asking: grants come from a reviewed deployment binding, by someone other than the author | *Planned (W1, W4)* |
| Capability inputs | A tenant id, role or another workspace's record passed as an argument | Tenant and rights come only from the caller's context; references resolve inside its scope; such fields are refused | *Planned (W1)* |
| Tenant rules in a record's transaction | Mail, HTTP, files, models or equipment reached before the record commits | The compiler refuses effects in rule phases, and rules run with no host to call | *Planned (W2)*; plugins already refused (`after_commit_it`) |
| Rule messages, traces, notifications | A rule reads a hidden field and says it in an error | Outputs carry the classification of what they read and are checked against the reader | *Planned (W2)* |
| Automations' e-mail | Sent from inside the transaction: a later failure retries and mails again | Queued in the outbox in the same transaction (defect found 8 November) | *Planned (W2)* |
| Signals into a waiting process | A well-formed signal for another workspace or record advances a process | The emitter is authenticated, the target resolved by business reference inside the workspace, conflicting duplicates flagged | *Planned (W3)* |
| Approvals | The requester approves their own request; inputs change after approval | The requester is excluded whatever roles they hold; a decision is bound to a digest of its material inputs and checked again before the effect | *Planned (W3)*; record workflows' `not_creator` exists |
| Effects and retries | A retry or a lost answer orders twice | One logical key per effect, not per attempt; a changed input under a used key is a conflict; an unknown outcome is reconciled, never resent blindly | *Planned (W1, W3)* |
| The process backend | Its administration reached by customers; histories holding records or secrets | Ferrum's API binds every workflow to a workspace; payloads are references through Ferrum's codec; the backend is not exposed | *Planned (W3)* |
| Restores | An effect sent again because a restored record says pending | Effects fenced by a recovery epoch until reconciled | *Planned (W8)* |
| Simulation | A test run sends real mail or moves stock | Effects captured by default; a sandbox run is separate and labelled | *Planned (W4)* |
| Analysis | A question answered from rows the asker may not read, or from a cache another person filled | Studio's row scopes for every joined entity; caches keyed by workspace, effective rights and policy revision | *Planned (W6)*; Studio's scopes exist |
| Intelligence | A retrieved record tells the agent to write or reveal | Proposals only, into an ordinary reviewed process; budgets on steps, tokens, queries and time | *Planned (W7)* |
