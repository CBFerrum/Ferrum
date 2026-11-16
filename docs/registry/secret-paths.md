# Secret paths

What may name what opens secrets, outside the vault (crates/kernel/src/vault; docs/security.md, "The vault").
`scripts/secret_paths.py` keeps it true: CI fails on any code outside the vault's module that names the vault's tables,
on any app that opens the vault or calls the people's and operators' functions, and on an app's own cryptography or
unsafe code that is not listed here. `--write` rewrites these tables from the code, and refuses to add to them: they may
only shrink.

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
