# W1: the capability catalog and its gate (8 November 2026)

Software evidence only, run on the owner's development machine (macOS, PostgreSQL 17.9) from the worktree
`ERP/flc/ferrum`, branch `logic/w1-capabilities`, based on Ferrum `71e438d`. Databases `ferrum_flc_kernel_test`
and `ferrum_flc_studio_test`, reset by each suite.

## What ran

```bash
export CARGO_TARGET_DIR=$HOME/Desktop/Personal/ERP/flc/target CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo test -p ferrum-logic                                             # 29 passed
cargo test -p ferrum-meta                                              # 18 + 1 passed
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --test capability_it   # passed 3 of 3 runs
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --lib                  # 69 passed
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --test invariants_it   # passed
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --test saga_it         # passed
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --test jobs_it         # passed
FERRUM_MCP_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-mcp                       # passed
FERRUM_STUDIO_TEST_DATABASE_URL=…/ferrum_flc_studio_test cargo test -p ferrum-studio                 # 26 + 1 passed
FERRUM_STUDIO_TEST_DATABASE_URL=…/ferrum_flc_studio_test cargo test -p ferrum-studio --test catalog_it
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test python3 scripts/mutants.py capability-         # 7 of 7 caught
cargo clippy -p ferrum-logic -p ferrum-meta -p ferrum-kernel -p ferrum-mcp -p ferrum-studio --all-targets -- -D warnings
cargo check -p ferrum-server --no-default-features
cargo fmt --all -- --check
python3 scripts/registry.py check
```

## What it showed

`capability_it`, two workspaces; in the first an administrator, a clerk, a viewer and an approver:

- The clerk's catalog has the record operations and actions their role allows and nothing else: no delete,
  no mail, no transition reserved for approvers, no `hidden` action without a contract, nothing from a ledger.
  A viewer's has `read`, `list` and the read-only action only.
- A change without a key is refused. One key and one input make one record however often it is asked for;
  the same key with other input is `input_changed`; two requests racing with one key make one change.
- The owning app's action runs once under its key; the hook's counter shows it.
- An action that writes and then fails leaves neither its change nor its receipt, and the same key then
  does it once.
- A field in a permission group the viewer cannot read stays hidden through `list` and `read`; the clerk's
  role reads it. A read-only field sent by the caller is not stored.
- `tenant_id` or `roles` in an input is refused as `forbidden`; so is the viewer creating anything.
- Asked for version 0.3.0 while 0.3.1 is installed, the gate refuses; asked for 0.3.1, it runs.
- The workflow's transition needs its role and still refuses the record's creator.
- `ferrum.notify` notifies once per key; `ferrum.mail` is refused to the clerk, and for the administrator is
  queued once in the outbox although asked twice.
- The second workspace uses the same key for its own record, cannot read the first's, and sees none of its
  receipts.

`catalog_it`, over ERP's selling and buying packages: 230 capabilities, worked out in 126 ms in a debug
build. The builder's endpoint returns exactly the kernel's descriptors, the agent's `list_capabilities`
returns the same descriptor for each id, digest included, and a customer created through the builder's
endpoint with an `Idempotency-Key` is returned, not created again, by the agent's `invoke_capability` under
that key.

The seven mutants each break one guarantee (the input digest check, the receipt, the rights check, the
ledger exclusion, masking in `list`, the version check, the trusted-context check); each made
`capability_it` fail.

## Not run here

The whole workspace's tests and the server with its default apps do not fit on this disk: the server was
checked without default features. Nothing ran against a deployed image or for a customer.
