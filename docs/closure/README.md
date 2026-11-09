# The closure review

What stands between Ferrum and supported, complete business operations, as a review of 9 November 2026
found it (`Ferrum_Only_Closure_Review_2026-11-09.zip`, digest in [catalog.json](catalog.json)), and what
has been done about each part since. The review read source and documents at `19fecde`; it ran one
extracted piece of Field Service and nothing else, so what it says is a dated baseline to check, not a
list of proven bugs. Its scope is Ferrum's apps, shared policy, customization, AI and analytics, office
and collaboration, extensions, industrial apps and running Ferrum yourself. It leaves out Dikidex, cloud
providers' control planes, Cozystack, managed hosting and cloud billing.

| Document | What it is |
|---|---|
| [catalog.json](catalog.json) | The review's 82 work packages and their acceptance cases, word for word. Every case starts `not_run` |
| [traceability.md](traceability.md) | Every package's state, levels and limits. Generated from [docs/registry/feo.yaml](../registry/feo.yaml) |

These are work packages, not a count of defects, and their number is not a readiness percentage.

## States

The platform program's states ([../platform/README.md](../platform/README.md#states)), said of the
package's required change: `not_started` means that change has not begun here, even where something it
builds on exists (the levels say what does). `verified_software` means the change is made and its
acceptance case, or the part of it software can run, ran in the named tests. A package waiting on a
provider, equipment or an expert names the blocker, and keeps whatever software it needs in its own state
rather than behind the blocker.

## What a package was found to be

Each package names one or more of these, after its code and tests were read (the brief's rule 4):

| Class | Meaning |
|---|---|
| `reproduced_defect` | A test fails with the behaviour the review saw, and passes with the change |
| `source_flaw` | The code reads as the review says; not yet reproduced by a test, so still a hypothesis |
| `documented_incomplete` | The repository's own registers say it is incomplete |
| `qualification_gap` | The software exists; what it has not had is the run, review or equipment that would qualify it |
| `product_boundary` | Deliberately not offered, or offered narrower; the change is to state and enforce the boundary |
| `stale_evidence` | A document says one thing and the code another |

## Four levels

A library implementing a mechanism is not an app adopting it, which is not a customer's workflow using it,
which is not a supported release proving it. Each package gives each level one of `none`, `partial`,
`built`, `verified` (tests named) and, for a release only, `proven` (a qualification, with its profile,
digest and independent reviewer); `not_applicable` where a level has no meaning. `acceptance` is
`ran_in_software` only when the named tests run the package's acceptance case as written, otherwise
`not_run`.

## Order

The brief's dependency order, used as workstreams: A, baseline and failure regressions; B, shared
authority, identity, consent and receipts; C, finance and high-use business cycles; D, authoring,
customization, AI and semantics; E, connectors, SDK, office, accessibility and offline experience; F,
installation, migration, upgrade, restore, capacity and support; G, industry operating cycles and their
qualification. It orders the work; it does not shrink it.
