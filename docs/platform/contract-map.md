# Contract map

The handoff's names are proposals (its C0). This maps them onto what exists, so nothing is built twice.

| Handoff name | What exists | Decision |
|---|---|---|
| `/api/platform/v1/...` | `/api/app/{package}/...` for apps, `/api/v1/{Entity}` for records | Management routes are `/api/app/cloud/...`; project and environment records are entities |
| Error shape `{"error": {"code", ...}}` | `{"error": code, "message": ..., "fields": [...]}` from `server/src/error.rs` | Kept. `retryable` and `operation_id` are added only where an operation exists |
| `Idempotency-Key` | `dedupe_key` in `op_command`; `event_id` in Shop Floor | `cloud_operation` scoped by tenant, family and key, bound to a canonical digest |
| `If-Match` | `version` on documents (`server/src/controllers/docs.rs`) | Reused |
| `PlatformProject`, `Environment` | Nothing | New entities |
| `Subject`, membership | `app_user`, `identity.rs` | Reused. Workloads get identities of their own |
| `Operation`, `DeliveryAttempt` | `op_command` for physical work | New for software work; `op_command` stays the physical ledger |
| `Outbox/Inbox` | `crates/kernel/src/outbox.rs`, per-consumer dedupe | Reused |
| Jobs | `crates/kernel/src/jobs.rs` | Reused for what a request should not wait for |
| `DomainClaim` | `tenant_host`, `server/src/controllers/domains.rs` | The shell's own hosts stay there. Customer-site claims are new, and refuse what `tenant_host` holds |
| `Artifact`, objects | `server/src/objects.rs`, `crates/kernel/src/files.rs` | Reused as storage; the artifact record is new |
| `SecretRef` | `crates/kernel/src/secret.rs`, connector and plugin credentials | Reused for sealing; references with versions are new |
| `SourceProvider`, `ResourceProvider` | `Connector` in `crates/kernel/src/connect.rs` | New contracts: a connector synchronizes business data, a provider operates infrastructure. Credentials reuse the connection store |
| Support grants, partners | `crates/kernel/src/support.rs` | Extended, not replaced |
| `.fapp` resource fields | `crates/apps/src/manifest.rs` | Extended by a versioned section; unknown fields already fail |
| `ConnectionOwner`, `ProtocolCall` | `gateway/src/adapters/hub.rs`, `ocpp.rs`, `op_command` | Extended in Operations and Edge |
| `UsageEvent` | `usage_day` in `crates/kernel/src/account.rs` | The daily count stays as the operator's view; a ledger of events is new |
