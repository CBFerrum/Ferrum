# Secret paths

What may name what opens secrets, outside the vault (crates/kernel/src/vault; docs/security.md, "The vault").
`scripts/secret_paths.py` keeps it true: CI fails on a use outside the vault that is not listed here, on more uses
in a file than listed, and on any app that names the vault's tables, opens it, or calls the people's and operators'
functions. `--write` rewrites these tables from the code, and refuses to add to them: they may only shrink. The slice
that moves the existing stores into the vault empties the first.

## The master key outside the vault

`FERRUM_SECRET_KEY` and what reads it, by file (an app's as `package:path`), not counting tests and comments.

| Where | Uses |
|---|---|
| `crates/kernel/src/approvals.rs` | 3 |
| `crates/kernel/src/connect.rs` | 4 |
| `crates/kernel/src/connections/mod.rs` | 8 |
| `crates/kernel/src/connections/oauth.rs` | 12 |
| `crates/kernel/src/connections/public.rs` | 3 |
| `crates/kernel/src/kms.rs` | 14 |
| `crates/kernel/src/lib.rs` | 4 |
| `crates/kernel/src/messages.rs` | 15 |
| `crates/kernel/src/privacy.rs` | 2 |
| `crates/kernel/src/receipt.rs` | 1 |
| `crates/kernel/src/sandbox.rs` | 6 |
| `crates/kernel/src/secret.rs` | 24 |
| `crates/kernel/src/service.rs` | 1 |
| `crates/kernel/src/sso.rs` | 9 |
| `crates/kernel/src/twofactor.rs` | 6 |
| `crates/kernel/src/webpush.rs` | 6 |
| `crates/kernel/src/workspace_move.rs` | 11 |
| `crates/plugins/src/lib.rs` | 9 |
| `crates/process/src/lib.rs` | 3 |
| `ferrum-app-commerce:gateways/stripe/src/lib.rs` | 1 |
| `ferrum-app-commerce:src/gift_card.rs` | 1 |
| `ferrum-app-commerce:src/shopper.rs` | 1 |
| `ferrum-app-pos:src/snapshot.rs` | 1 |
| `server/src/app.rs` | 18 |
| `server/src/controllers/admin.rs` | 1 |
| `server/src/doctor.rs` | 8 |
| `server/src/process.rs` | 3 |
| `server/src/state.rs` | 2 |
| `server/src/tasks.rs` | 7 |

## Cryptography crates in apps

An app's own key-holding cryptography, from before the vault: each to move onto the vault's verbs (derive, seal,
sign) or to stay where a third party's scheme needs it (a government's RSA, a signature format).

| App | Crate |
|---|---|
| `ferrum-app-bookings` | `hmac` |
| `ferrum-app-campaigns` | `hmac` |
| `ferrum-app-commerce` | `hmac` |
| `ferrum-app-comms` | `hmac` |
| `ferrum-app-drive` | `hmac` |
| `ferrum-app-drive` | `ring` |
| `ferrum-app-erp` | `hmac` |
| `ferrum-app-field-service` | `hmac` |
| `ferrum-app-forms` | `hmac` |
| `ferrum-app-live` | `hmac` |
| `ferrum-app-lms` | `jsonwebtoken` |
| `ferrum-app-lms` | `rsa` |
| `ferrum-app-sign` | `hmac` |
| `ferrum-eu-members` | `rsa` |
| `ferrum-india` | `aes` |
| `ferrum-india` | `jsonwebtoken` |
| `ferrum-india` | `rsa` |
| `ferrum-ops` | `ed25519-dalek` |
| `ferrum-procurement` | `hmac` |

## Unsafe code in apps

| Where | Blocks |
|---|---|
| `ferrum-app-edge:gateway/src/adapters/transport.rs` | 2 |
| `ferrum-app-edge:gateway/src/clock.rs` | 2 |
| `ferrum-app-edge:gateway/src/runtime.rs` | 2 |
