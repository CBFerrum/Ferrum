# W6: analytical semantics (8 November 2026)

Software evidence only, from the worktree `ERP/flc/metrics` (CBFerrum/ferrum-app-metrics, branch
`logic/w6-semantics`, based on Metrics `5ac511f`, 0.4.5), built against Ferrum's main (`a0c6aa5`) from the
local checkout and merged as CBFerrum/ferrum-app-metrics#4 (`c2b0b66`, Metrics 0.5.0).

## What ran

```bash
cargo test --lib                       # 4 passed
cargo test --test metrics_it           # passed
cargo test --test datasets_it          # passed
cargo test --test composed_it          # passed
cargo clippy --all-targets -- -D warnings; cargo fmt --all -- --check
# in Ferrum, with Metrics pinned at c2b0b66 and Edge at 17a7c41
cargo check --locked -p ferrum-server  # every default feature
python3 scripts/registry.py check
```

## What it showed

`composed_it`: *Average company terms* is `[Company terms] / [Company customers]`. A director who sees every
customer reads 60 ((30 + 60 + 90) / 3), a representative who made two reads 45, another who made one reads
90, and one who made none reads no value, with the reason, never zero. A metric that would be made of itself
by way of another is refused, naming the cycle (Loop A → Loop B → Loop A); one that is both a query and a
formula, or names a metric that is not there, is refused; a composed metric is certified only after its
parts. `impact` on *Company customers* lists the three metrics composed of it. A monthly series of the
average has three periods, the current one partial and 60, the two before without a value because no
customer existed then; a weekly series of the count gives the representative's 2. With a part deprecated,
the composed metric says the part is not certified rather than mean something else.

Studio's compiler already refused measuring a document's own fields at its children's grain (each invoice
once per item row) and two child tables at once; its unit tests are the evidence for FLC-SEM-002.

## Not done, and why

- **Wren and DataFusion (FLC-SEM-010).** The plan put an external engine behind a provider trait "disk
  permitting". The build machine has under three gigabytes free and DataFusion alone needs several to
  build, so no adapter was written. Native analysis stays on Studio's compiler, whose identifiers come from
  metadata and whose values are bound.
- **Currency by date and fiscal-period series (FLC-SEM-004).** Not built.
- The new fields' names (Composed Of, Series By) are not yet in Metrics' Arabic and Hindi name catalogs.
