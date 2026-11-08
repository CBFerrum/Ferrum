# The Logic Platform program

Customers changing how their business runs inside Ferrum, without changing its code: rules on records,
decision tables, durable processes with people in them, event pipelines from equipment, and analysis
that can propose work but not do it. This is the record of that program as it stands, kept beside the
code. Its source is the package of 8 November 2026 (`Ferrum_Logic_Platform_v2_Claude_Package.zip`,
digest in [catalog.json](catalog.json)): a proposed specification, not software.

| Document | What it is |
|---|---|
| [catalog.json](catalog.json) | The package's 124 requirements, their 124 acceptance cases and the 42 earlier scenarios it kept, word for word. Every case starts `not_run` |
| [traceability.md](traceability.md) | Every requirement's state. Generated from [docs/registry/flc.yaml](../registry/flc.yaml) |
| [baseline.md](baseline.md), [baseline.json](baseline.json) | What exists: heads, pins, builds and installed versions, as separate facts. `scripts/baseline.py --out docs/logic` |
| [reuse-map.yaml](reuse-map.yaml) | Each area the package names, against the code that already does part of it, at a pinned commit |
| [decisions.md](decisions.md) | The decisions the rest stands on (L-01 to L-08), with what was rejected and why |
| [profiles.md](profiles.md) | What each way of running Ferrum supports, and what it refuses |
| [capabilities.md](capabilities.md) | Business capabilities: where they come from, what each promises, and the gate every use goes through |
| [rules.md](rules.md) | A solution's source, its compiler, releases, review and activation, how customer rules run in every save, and how chains of events stop |
| [processes.md](processes.md) | Durable processes: their steps, how a run starts, does its work, waits for people and events, and recovers, and how to run them on Temporal |
| [evidence/](evidence) | What was run, by whom, on what, and what it showed |

The design itself stays in the package. [../references/logic.md](../references/logic.md) says what
mature projects were read for it, and under which licence.

## States

The same states as the platform program: see [../platform/README.md](../platform/README.md#states).
`scripts/registry.py` refuses a state its evidence does not reach. A simulator proves a software
scenario, not equipment; a mocked provider proves Ferrum's side of a contract, not the provider; a model
stand-in proves the plumbing, not an answer's quality.

## Names

The package's acceptance cases are numbered `AT-BASE-001` and so on, and several of those numbers are
also cases of the platform program. Here a case is named by its requirement (`FLC-BASE-001`), which is
unique.
