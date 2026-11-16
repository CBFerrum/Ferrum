# Secret paths

What may name what opens secrets, outside the vault (crates/kernel/src/vault; docs/security.md, "The vault").
`scripts/secret_paths.py` keeps it true: CI fails on a use outside the vault that is not listed here, on more uses
in a file than listed, and on any app that names the vault's tables, opens it, or calls the people's and operators'
functions. `--write` rewrites these tables from the code, and refuses to add to them: they may only shrink. The kernel's
stores are in the vault now (docs/plan/stateless.md, D5); what is left of the first table keys or seals with the old key
for a while, or reads it to move what it sealed, and goes as each moves.

## The master key outside the vault

`FERRUM_SECRET_KEY` and what reads it, by file (an app's as `package:path`), not counting tests and comments, and why
(`WHY` in scripts/secret_paths.py).

| Where | Uses | Why |
|---|---|---|
| `crates/kernel/src/approvals.rs` | 1 | a refusal names it: with the vault closed it still seals, as before |
| `crates/kernel/src/connect.rs` | 1 | a refusal names it: with the vault closed it still seals, as before |
| `crates/kernel/src/connections/mod.rs` | 1 | a refusal names it: with the vault closed it still seals, as before |
| `crates/kernel/src/connections/oauth.rs` | 2 | a sign-in's state is made and checked with the key it is given (the vault's, or the old key while older builds may run) |
| `crates/kernel/src/kms.rs` | 14 | unwraps the old key from a key-management service, where an installation still configures it |
| `crates/kernel/src/lib.rs` | 4 | the settings that carry the old key to the kernel (adopted from them), and what apps that still read it are given |
| `crates/kernel/src/messages.rs` | 3 | opt-out tokens and erased opt-outs are made with the key they are given (the vault's, or the old key); a refusal names it |
| `crates/kernel/src/sandbox.rs` | 5 | a sandbox of another installation's workspace opens what that one's old keys sealed, and seals the previous build's columns |
| `crates/kernel/src/secret.rs` | 14 | the old key itself: opens the previous build's columns, seals them while it is configured, rotates them |
| `crates/kernel/src/sso.rs` | 3 | a refusal names it: with the vault closed it still seals, as before |
| `crates/kernel/src/twofactor.rs` | 1 | a refusal names it: with the vault closed it still seals, as before |
| `crates/kernel/src/webpush.rs` | 2 | what it says when the push key does not open names it |
| `crates/kernel/src/workspace_move.rs` | 9 | a move opens what the source's old keys sealed, and seals the previous build's columns here |
| `ferrum-app-commerce:gateways/stripe/src/lib.rs` | 1 | Stripe's own `secret_key` (the name of a plugin's credential), not the old key |
| `server/src/app.rs` | 15 | reads the old keys from the environment for the kernel to adopt and for the previous build's columns |
| `server/src/doctor.rs` | 8 | says whether the old key is set, and how |
| `server/src/state.rs` | 2 | the old keys' settings |
| `server/src/tasks.rs` | 7 | `kms_wrap` prints the old keys wrapped by the key-management service |

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

## Apps that read secret fields past the vault

`Kernel::secret`, deprecated in the release that moved the stores into the vault (docs/plan/stateless.md, D5): each
call moves, in its app's own repository, to the app's own handle (`Vault::field`); the function goes the release after.

| Where | Calls |
|---|---|
| (none) | |

## Apps that read the old key

`settings.secret_key` and `settings.retired_secret_keys`, read by an app: until each moves onto its own handle (derived
keys, sealed data, secrets of its own), the old key cannot be retired (`ferrum task legacy_key`).

| Where | Uses |
|---|---|
| (none) | |
