# Security baseline

Which controls this work is held to, where each is done, and what is open. This is a working map, not an
assessment: nobody independent has reviewed it (`BLOCK-SECURITY-REVIEW`), so nothing here is a claim of conformance
with anything. A row that says done says where to look, and means the code exists and a test covers it.

Pinned to [OWASP ASVS 4.0.3](https://github.com/OWASP/ASVS/tree/v4.0.3), level 2, and
[NIST SSDF 1.1](https://csrc.nist.gov/pubs/sp/800/218/final). A later version needs this reviewed again, not edited.

## ASVS 4.0.3

| Chapter | Where | Open |
|---|---|---|
| V1 Architecture | `docs/security.md`, `docs/registry/threat-model.md`, `docs/platform/decisions.md` | Nothing independent |
| V2 Authentication | Argon2id, lockout, TOTP, passkeys with device verification (`crates/kernel/src/auth.rs`, `passkey.rs`, tests `passkey_it`) | Breached-password checks. Hardware-bound requirements for administrators |
| V3 Sessions | Token versions, revocation, cookie and origin checks (`server/src/shell.rs`) | Idle limits are per deployment |
| V4 Access control | Row-level security on every workspace table, checked by `ferrum task doctor`. Deny by default in Cloud's policy (`contract/src/policy.rs`, tests `invariants_it`, `control_it`) | |
| V5 Validation and output | Typed parsing, parameterised SQL. A dynamic identifier comes from the catalog, never a request | No full review of every dynamic statement |
| V6 Cryptography | Sealed secrets, signatures with ed25519-dalek and ring, nothing of our own (`crates/kernel/src/secret.rs`) | Keys are held by the deployment, not a KMS (`BLOCK-KMS`) |
| V7 Errors and logging | Audit trail with a hash chain. Correlation ids. No secrets in the record (`crates/kernel/src/audit.rs`) | Log retention is the deployment's |
| V8 Data protection | Exports, erasure with a journal, holds, residency for Cloud (`crates/kernel/src/privacy.rs`) | Field-level classification is not complete |
| V9 Communication | TLS terminates at the gateway (`deploy/k8s/gateway.yaml`). Outbound reaches only public addresses where a customer names one | No certificate has been issued here. Peers and webhooks on a site are not encrypted, as `docs/security.md` says |
| V10 Malicious code | Signed app bundles, sandboxed plugins, provenance on builds (`crates/plugins`, `crates/apps/src/signing.rs`) | A container is not a boundary against a kernel exploit (`BLOCK-SANDBOX-QUALIFICATION`) |
| V11 Business logic | Plans bound to digests, approval by someone else, idempotent operations, unknown outcomes settled by a person | |
| V12 Files | Scanning through ClamAV, size limits, safe unpacking (`serve/src/sync.rs`) | No test of a real scanner in this work |
| V13 API | One route table, refused unknown fields, generated OpenAPI | Rate limits are per deployment |
| V14 Configuration | Pinned images and lockfiles, `cargo audit` and license checks in CI, restricted pods (`deploy/k8s`) | Images are attested, not signed per bundle |

## SSDF 1.1

| Practice | Where | Open |
|---|---|---|
| PO: prepare the organization | `docs/registry`, this file | Roles are not named (`docs/platform/services.md`) |
| PS: protect the software | Protected branches, reviews by pull request, D-18: this work does not merge itself | Requires signed commits nowhere |
| PW: produce well-secured software | Threat models, tests against real services, mutants, fault tests | No independent penetration test |
| RV: respond to vulnerabilities | `SECURITY.md`, `cargo audit` in CI with reviewed exceptions | No disclosure programme is running |

## What blocks a profile

A profile that names an unresolved critical control above does not ship: `docs/platform/stop-ship.md` says which.
