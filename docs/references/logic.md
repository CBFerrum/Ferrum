# Logic Platform: references

**Compared with:** Temporal server v1.32.2 and CLI v1.10.0, Temporal's Rust SDK (`temporalio-sdk` 1.1.2,
repository `temporalio/sdk-core` at `38dfee4`), Temporal's Rust samples, WrenAI (`core/`), the CEL
specification, the DMN TCK's level-2 decision-table cases, Restate (licence only), xyflow (licence only)
and Apache DataFusion (licence only), read on 8 November 2026. Copies are in `referance/logic/` (shallow,
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
| WrenAI `core/` (Wren Engine, Rust on DataFusion) | A semantic layer that plans queries over models | `core/**` Apache-2.0; `docs/**` CC BY 4.0; the names are Canner's trademarks | Evaluated in W6 behind a provider trait for external sources (L-07); its name is not used for anything of Ferrum's |
| Apache DataFusion | An embeddable query engine whose security is left to the embedding application | Apache-2.0 | Only through Wren in W6, with statements, files, network, memory and spill locked down |
| CEL specification | The language's semantics and conformance tests | Apache-2.0 | Conformance cases for the subset Ferrum supports (L-02) |
| DMN TCK, level 2 | Decision tables: unique, first, priority, collect, any; ranges | Apache-2.0 (`LICENSE-ASL-2.0.txt`) | Test cases for Ferrum's hit policies; Ferrum claims a documented subset, not DMN conformance |
| xyflow (React Flow) | Node editors with keyboard support | MIT | Only if the Edge canvas cannot be reused for a need (W4); not taken today |
| Restate | Durable handlers and workflows | BSL 1.1 (GitHub reports `NOASSERTION`) | Named in L-01 as the alternative not taken; never copied |

## What Ferrum already has that these do not replace

Ferrum's outbox, jobs and sagas keep their work (events, exports, short compensations); the Operations
ledger stays the only path to equipment; Studio's compiler stays the native query path because it
already applies row scopes; Metrics stays the authority for what a metric means. See
[../logic/reuse-map.yaml](../logic/reuse-map.yaml).

## What is not fetched, and why

DataFusion and the rest of Wren are large to build and this disk has little room: they are read and
built in W6, when space is made. Temporal's server source is not here beyond its schema and docs; the
server runs from its released binaries and images, which the spike pins by checksum.
