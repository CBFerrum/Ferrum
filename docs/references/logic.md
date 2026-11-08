# Logic Platform: references

**Compared with:** Temporal server v1.32.2 and CLI v1.10.0, Temporal's Rust SDK (`temporalio-sdk` 1.1.2,
repository `temporalio/sdk-core` at `38dfee4`), Temporal's Rust samples, WrenAI (`core/`), the CEL
specification, the DMN TCK's level-2 decision-table cases, Restate (licence only), xyflow (licence only)
and Apache DataFusion (licence only), read on 8 November 2026; Wren's `core/` again for Metrics' engine on 8 and
9 November. Copies are in `referance/logic/` (shallow,
source only; Temporal's server as its schema, docs and configuration only, for disk). Reference paths
below are in each project's own repository.

The package asks for mature execution and query engines to be reused, and for Ferrum to write only its
business layer: the catalog, the compiler, governance, the builder and operations. The owner's rule
says the same (`referance/README.md`).

## What each is taken for

| Project | What it already does | Licence | How Ferrum takes it |
|---|---|---|---|
| Temporal server | Durable timers, signals, retries, child workflows, continue-as-new, versioned workers, replay; persistence on PostgreSQL (schema `v12`, so PostgreSQL 12 and later) | MIT | Run as a component for the Full Process profile (`deploy/temporal.yml`), once the L-01 spike passes. Never exposed to end users: Ferrum's API binds workflows to workspaces |
| Temporal Rust SDK (`temporalio-sdk`, `-client`, `-workflow`, `-common`, `-macros`, `-protos`) | Workers on Tokio, deterministic workflows and activities, replay testing; `rust-version` 1.92 | MIT (`LICENSE.txt`) | Depend on it in `crates/process`, pinned exactly; nothing else in Ferrum builds it |
| Temporal Rust samples | A workflow that interprets a definition at run time | No licence declared | Read only: nothing copied |
| WrenAI `core/` (Wren Engine, Rust on DataFusion) | A semantic layer that plans queries over models | `core/**` Apache-2.0; `docs/**` CC BY 4.0; the names are Canner's trademarks | Metrics' engine depends on `wren-semantic-core` 0.5.1, pinned exactly, as a service of its own (L-07, built in batch 4); its name is not used for anything of Ferrum's |
| Apache DataFusion | An embeddable query engine whose security is left to the embedding application | Apache-2.0 | Only through Wren, in Metrics' engine (DataFusion 55), with statements, files, network, memory, spill, query shape and time locked down |
| CEL specification | The language's semantics and conformance tests | Apache-2.0 | Conformance cases for the subset Ferrum supports (L-02) |
| DMN TCK, level 2 | Decision tables: unique, first, priority, collect, any; ranges | Apache-2.0 (`LICENSE-ASL-2.0.txt`) | Test cases for Ferrum's hit policies; Ferrum claims a documented subset, not DMN conformance |
| xyflow (React Flow) | Node editors with keyboard support | MIT | Only if the Edge canvas cannot be reused for a need (W4); not taken today |
| Restate | Durable handlers and workflows | BSL 1.1 (GitHub reports `NOASSERTION`) | Named in L-01 as the alternative not taken; never copied |

## What Ferrum already has that these do not replace

Ferrum's outbox, jobs and sagas keep their work (events, exports, short compensations); the Operations
ledger stays the only path to equipment; Studio's compiler stays the native query path because it
already applies row scopes; Metrics stays the authority for what a metric means. See
[../logic/reuse-map.yaml](../logic/reuse-map.yaml).

## What the Wren and DataFusion references settled for Metrics' engine

Read in `referance/logic/wren/core/wren-core/core/src` and checked against the crates the engine builds
(closing-gaps.md, batch 4):

- **Rights go in the scan, not in a view.** Wren's planned SQL reads each model as
  `(SELECT __source.x AS x FROM <table> AS __source) AS <model>` (the snapshots in `mdl/mod.rs`), so a filter
  on a view qualified by the table's name stops matching. The engine reads each dataset through a
  `TableProvider` of its own that filters rows and drops hidden columns inside its scan.
- **Wren's own rules are taken out.** Wren carries its own row and column access control
  (`logical_plan/analyze/access_control`) and a `dataSource`. The rules are Ferrum's, so the engine
  removes `rowLevelAccessControls`, `columnLevelAccessControl` and `dataSource` from a model before it runs.
- **Expressions are checked as Wren parses them.** Wren parses a model's calculated columns and
  relationship conditions with sqlparser's generic dialect (`mdl/utils.rs`). The engine checks their depth
  the same way before planning.
- **Wren's SQL nests a subquery per model and relationship.** A long chain of relationships outgrows
  DataFusion's default parser depth (50), so the engine parses Wren's planned SQL deeper (512) than the
  reader's own query, and holds a semantic model to 32 models.
- **The entry point is `transform_sql_with_ctx`** (`mdl/mod.rs`): the engine plans through it with its
  own `SessionContext`, then runs the planned SQL with DDL, DML and statements disabled.
- **DataFusion leaves security to its host,** as its documentation says, and some of it is not on by
  default. Built with `default-features = false`, as Wren builds it, it has no `recursive_protection`, so
  the engine turns that on and gives its threads 64 MiB stacks. A timeout fires only when the work
  yields, so each query runs as a task of its own. Errors raised while reading a file can carry the raw
  row, read before any rule applies, so they are never sent to a reader.

## What is not fetched, and why

Wren's `core/` is here as source; the engine builds Wren and DataFusion from crates.io (1.5 GB without
debug information), not from this copy. Temporal's server source is not here beyond its schema and docs;
the server runs from its released binaries and images, which the spike pins by checksum.
