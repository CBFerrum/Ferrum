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
| Operator to customer data | None technically: operators can read the database | Stated plainly: there is no customer-held key |

## Surfaces and how they are held

| Surface | Threat | Mitigation | Evidence |
|---|---|---|---|
| Plugin hook | A rolled-back document already paid for | `http.fetch` and `sockets.*` refused in hooks | `after_commit_it` |
| Plugin call | A runaway guest holds a transaction | Deadlines on both sides; epoch interruption | `runtime::tests` |
| Plugin HTTP | SSRF | Declared hosts, private addresses refused, DNS pinned, no redirects | `host.rs`; not tested here |
| Webhook delivery | SSRF, slow endpoint | The same vetting; leased, no transaction across it | `events_it` |
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
