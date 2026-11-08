# W2: the compiler, customer rules and event chains (8 November 2026)

Software evidence only, run on the owner's development machine (macOS, PostgreSQL 17.9) from the worktree
`ERP/flc/ferrum`, branch `logic/w2-compiler`, based on Ferrum `fc2a953`. Databases `ferrum_flc_kernel_test`,
`ferrum_flc_studio_test`, `ferrum_flc_plugins_test` and `ferrum_flc_mcp_test`, reset by each suite.

## What ran

```bash
export CARGO_TARGET_DIR=$HOME/Desktop/Personal/ERP/flc/target CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
cargo test -p ferrum-logic --lib                                        # 87 passed
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --lib          # 70 passed
# every kernel integration test, one binary at a time (the disk holds one): 38 passed,
# logic_rules_it and event_chain_it among them; saml_it and saml_keycloak_it run nothing without their
# identity provider, as before
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test cargo test -p ferrum-kernel --test <each>
FERRUM_PLUGINS_TEST_DATABASE_URL=…/ferrum_flc_plugins_test cargo test -p ferrum-plugins --test platform_it
FERRUM_MCP_TEST_DATABASE_URL=…/ferrum_flc_mcp_test cargo test -p ferrum-mcp --test mcp_it
FERRUM_STUDIO_TEST_DATABASE_URL=…/ferrum_flc_studio_test cargo test -p ferrum-studio        # 26 + 1 + 1 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
python3 scripts/registry.py check
FERRUM_TEST_DATABASE_URL=…/ferrum_flc_kernel_test python3 scripts/mutants.py <rules-|release-|refusal-|chain-|replay-crosses-workspaces>
```

## What it showed

**The compiler** (`crates/logic/src/compile.rs`, `solution.rs`, `decision.rs`, `rules.rs`, `expr.rs`): the
package's two examples compile, named after Ferrum's capabilities (MaterialRequest's `read` and
`make_purchase_order`, `ferrum.mail`, MaintenanceRequest, ServiceRequest; the stock hold keeps the package's
example name, as Ferrum has none). Every fault comes back at once with a code and a pointer. Moving a box
changes no digest; rewording what a reviewer is told, or a parameter, changes the artifact digest. The
solution's own tests run at admission, a missing currency included ("unknown", not "proceed").

**Customer rules** (`logic_rules_it`), two workspaces; in the first an author, a reviewer, a clerk and a
stranger who may create orders but not read parties:

- A rule reading a field in a permission group is refused at its pointer; a rule naming a capability does not
  parse; anyone may check a source and only an administrator keeps one.
- A version is kept once, and the database refuses to change it. The author cannot approve it; nobody can
  approve a digest they were not shown; a rejection says why; an unapproved release does not activate; a
  second activation with a stale generation is refused.
- Derivations fill the customer's fields in the order their reads need (`x_band`, then `x_flag` from it), and
  the app's own guard sees the derived value.
- A total of zero is refused with the rule's message and its origin (`[acme.orders 1.0.0, rule positive]`),
  nothing is written, and the refusal's evidence (who, which rule, which record, false) is kept in its own
  transaction, with no copy of the order. A missing total is refused as "could not be checked".
- A credit limit is read from the linked party with the clerk's rights; the stranger, who may not read
  parties, is refused before the rule runs.
- One invalid order through insert, update, import, bulk, a capability's invocation, an automation and a job:
  each refused with the same message, and none written. (`platform_it`: a plugin's `data.insert` of a reading
  the customer's rule refuses; `mcp_it`: an agent's `create_record` and `invoke_capability` of a customer the
  rule refuses.)
- A second solution filling `x_band` cannot be activated beside the first.
- With `x_flag` taken out of the workspace, the approved next version is refused at activation
  (`stale_approval`), and the running one refuses its orders' saves until it is stopped; once stopped, an
  order with a zero total saves.
- The second workspace runs none of it and sees none of the first's releases.

**Event chains** (`event_chain_it`): two apps answering each other's changes ran exactly nine events (steps
0 to 8); the ninth was stopped before any handler, kept dead with `Alpha.created → Beta.created → …`;
replayed, it ran from step 0. One change answered threefold at each step was stopped at the budget (30 in
the test), no deeper than step 4. A job an event handler queued wrote its event one step down, not at 0. An
automation answering its own changes stopped at its own limit and recorded why.

**Mutants**: each of the ten new ones breaks one guarantee (rules skipped for system saves, refusal evidence
not kept, a release running unapproved, an approval of a digest not shown, activation beside a rival writer,
restricted fields readable, unknown passing a validation, derivations in written order, the chain's depth not
carried, its budget ignored): all ten caught, with the earlier `automation-mails-before-commit` and
`saml-refusal-keeps-the-sign-in`. The replay mutant (`replay-crosses-workspaces`) was rewritten for the new
replay query, and is caught. `scripts/mutants.py --check`: 246 mutants find their text.

```
caught   rules-skip-system-saves          caught   release-runs-unapproved
caught   rules-read-restricted            caught   release-approved-unseen
caught   rules-unknown-passes             caught   release-beside-a-rival
caught   rules-derive-in-written-order    caught   refusal-evidence-not-kept
caught   chain-depth-not-carried          caught   chain-budget-ignored
caught   replay-crosses-workspaces        caught   saml-refusal-keeps-the-sign-in
```

## The package's checker cases

| Case | Here |
|---|---|
| 01, 02 the examples | `t01_the_purchase_example_compiles`, `t02_the_condition_example_compiles` |
| 03 unknown root key | `t03_an_unknown_root_key_is_refused`, `unknown_keys_are_refused_everywhere` |
| 04-07 duplicate node, missing target, unreachable node, cycle | `t04_t05_t06_t07_graph_faults` |
| 08-09 unknown or undeclared capability | `t08_t09_capabilities_must_exist_and_be_asked_for` |
| 10 effect in a hook | `t10_a_rule_reaches_no_capability`, `logic_rules_it` |
| 11-12 protected derivation, validation that writes | `t10_t11_t12_rules_stay_inside_their_record` |
| 13 query that writes | `t13_a_query_cannot_write` |
| 14-16 attempt key, missing and successful unknown | `t14_t15_t16_effects_are_keyed_and_unknown_is_not_success` |
| 17 self-approval | `t17_nobody_approves_their_own_request` |
| 18-19 unknown decision, missing outcome route | `t18_t19_decisions_and_their_routes` |
| 20 a capability twice | `t20_a_capability_asked_for_twice`; the kernel's catalog lists each id once (`capability_it`) |
| 21 wrong runtime | `t21_a_process_needs_its_profile`, `unknown_keys_are_refused_everywhere` (`runtime`) |
| 22 step budget | `t22_the_step_budget_covers_the_graph` |
| 23-26 pipelines | W5, in Edge's contract |
| 27-28 analysis intents | W6 and W7 |
| 29-31 bindings | W3 and W4, with deployments |
| 32-33 duplicate key, non-finite number | `t32_t33_the_source_is_read_strictly`, `solution.rs` tests |
| 34 crosslinks | `scripts/registry.py check` (124 requirements, 124 cases) |

## Not run here

Nothing went through the HTTP server: the REST and `/api/v1` routes call the same kernel insert the tests
call. The Edge gateway's build of the evaluator, and a replay of one fixture through server and gateway, come
with W5. Nothing ran against a deployed image or for a customer.
