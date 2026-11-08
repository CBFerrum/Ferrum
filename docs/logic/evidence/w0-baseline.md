# W0: baseline, reuse map and decisions (8 November 2026)

Software evidence only. Run on the owner's development machine (macOS, PostgreSQL 17.9) from the
worktree `ERP/flc/ferrum`, branch `logic/w0-baseline`, based on Ferrum `a3236e6`.

## The package

`Ferrum_Logic_Platform_v2_Claude_Package.zip`: CRC checked and all 27 entries of its `SHA256SUMS` match.
Its three loose documents (`BUILDER_UX.md`, `CLAUDE_START_HERE.md`, `DATA_AND_API_CONTRACTS.md`) are the
same bytes as the copies inside it, and `Ferrum_Logic_Platform_v2_Complete_Plan.md` is every document
concatenated. Its research snapshot (Ferrum `6d65c1f`) is two commits behind the `main` read here.

## FLC-BASE-001 and FLC-BASE-002

```bash
FERRUM_SIBLINGS=$HOME/Desktop/Personal/ERP python3 scripts/baseline.py --out docs/logic
(cd scripts && python3 test_baseline.py)
```

`baseline.py` gained `--out` (so another program's baseline leaves `docs/platform`'s as it was) and
`FERRUM_SIBLINGS` (so a worktree outside the usual folder finds the other checkouts). It read 44
repositories, 39 pinned and 68 declared apps; many local checkouts are behind their default heads, and
the page says so rather than inferring one from another. `test_baseline.py` now also runs the
package's case for FLC-BASE-001: an app's head moved past the platform's pin is shown as behind, and
images and installations that could not be read are said to be unread, not absent. The existing case
(dirty checkout, unused patch, nothing staged or overwritten) is FLC-BASE-002's.

## The registry

```bash
python3 scripts/registry.py generate && python3 scripts/registry.py check
(cd scripts && python3 test_registry.py)
```

The 124 requirements are in `docs/registry/flc.yaml` with what the code already does for each, checked
by the same rules as the platform program's. Cross-repository citations were checked against each
repository's `origin/main` (Edge `b69698f`, Metrics `b708d7c`, Cloud `c64e0bc`, Frontend `155518c`),
because several local checkouts lag theirs and `registry.py` reads working trees.

## Found while reading, confirmed at `main`

1. An automation's e-mail goes straight to SMTP from inside its action transaction, before its run is
   recorded (`crates/kernel/src/automation.rs`, `execute`, `AutomationAction::Email`), although
   `Kernel::queue_mail` puts mail in the outbox in the caller's transaction. A later action failing
   rolls the run back and the next delivery mails again.
2. `automation::eval` compiles each CEL expression every time it evaluates one.
3. `Expr::eval` reads the wall clock when no `today` or `now` is supplied.
4. An automation runs with all of its saver's administrator rights.

All four are fixed or replaced in W2 and W8, each with a test that fails first.

## Not done here

Nothing was installed for a customer and nothing ran against production. Independent review of any
of this is `BLOCK-SECURITY-REVIEW`.
