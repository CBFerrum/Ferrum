# Traceability

Generated from [docs/registry/bf.yaml](../registry/bf.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Not started | 42 |
| Implemented, unverified | 1 |
| Verified in software | 3 |
| **All** | **46** |

## BF0: Decisions and inventory

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-BASE-001 | Decisions recorded | Implemented, unverified | AT-BF-BASE-001 | 0 | Nine decisions recorded before building, each with what it rules out and its gate; each is proven only when its gate lands and its rows here say so |
| BF-BASE-002 | Register checked | Verified in software | AT-BF-BASE-002 | 1 | Checked like the other programs (states, evidence, catalog agreement, limits); its self-test refuses a row verified without tests, partial citing nothing, in another gate, or silent on its limits. A person still decides what a test proves |
| BF-BASE-003 | Exact inventory | Verified in software | AT-BF-BASE-003 | 1 | A search of 40 public app repositories at the heads it lists: it finds where a feature is written, not whether it is right, and patterns can miss a feature written another way. The private Cloud repository is left out |
| BF-BASE-004 | Closure packages re-scoped | Verified in software | AT-BF-BASE-004 | 1 | Twenty open closure packages name the gate taking them over, whole or in part, with what it takes; the check refuses a gate that does not exist (self-test). The other 37 stay with their apps or the platform, unchanged |

## BF1: Portal access by configuration

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-PORTAL-001 | Portal roles | Not started | AT-BF-PORTAL-001 | 0 | Not started |
| BF-PORTAL-002 | Ownership through links | Not started | AT-BF-PORTAL-002 | 0 | Not started |
| BF-PORTAL-003 | Fields and actions under portal roles | Not started | AT-BF-PORTAL-003 | 0 | Not started |
| BF-PORTAL-004 | Generic portal pages | Not started | AT-BF-PORTAL-004 | 0 | Not started |
| BF-PORTAL-005 | Portal roles administered and previewed | Not started | AT-BF-PORTAL-005 | 0 | Not started |
| BF-PORTAL-006 | Proof: two portals as configuration | Not started | AT-BF-PORTAL-006 | 0 | Not started |

## BF2: Public pages

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-PUBLIC-001 | Public pages bound to an entity | Not started | AT-BF-PUBLIC-001 | 0 | Not started |
| BF-PUBLIC-002 | Availability | Not started | AT-BF-PUBLIC-002 | 0 | Not started |
| BF-PUBLIC-003 | A guest's own link | Not started | AT-BF-PUBLIC-003 | 0 | Not started |
| BF-PUBLIC-004 | Mail and limits | Not started | AT-BF-PUBLIC-004 | 0 | Not started |
| BF-PUBLIC-005 | Proof: booking pages as configuration | Not started | AT-BF-PUBLIC-005 | 0 | Not started |

## BF3: Approvals and independence

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-APPROVE-001 | Approvals on transitions | Not started | AT-BF-APPROVE-001 | 0 | Not started |
| BF-APPROVE-002 | Independence | Not started | AT-BF-APPROVE-002 | 0 | Not started |
| BF-APPROVE-003 | Delegation and escalation | Not started | AT-BF-APPROVE-003 | 0 | Not started |
| BF-APPROVE-004 | Targets in business time | Not started | AT-BF-APPROVE-004 | 0 | Not started |
| BF-APPROVE-005 | Proof: approvals as configuration | Not started | AT-BF-APPROVE-005 | 0 | Not started |

## BF4: Money and stock as capabilities

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-MONEY-001 | ERP money and stock steps as capabilities | Not started | AT-BF-MONEY-001 | 0 | Not started |
| BF-MONEY-002 | Driven by decision tables | Not started | AT-BF-MONEY-002 | 0 | Not started |
| BF-MONEY-003 | Proof: money policies as configuration | Not started | AT-BF-MONEY-003 | 0 | Not started |

## BF5: Rules without source

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-AUTHOR-001 | Rules without source | Not started | AT-BF-AUTHOR-001 | 0 | Not started |
| BF-AUTHOR-002 | Decision tables without source | Not started | AT-BF-AUTHOR-002 | 0 | Not started |
| BF-AUTHOR-003 | Structured steps without source | Not started | AT-BF-AUTHOR-003 | 0 | Not started |
| BF-AUTHOR-004 | Lossless round trip | Not started | AT-BF-AUTHOR-004 | 0 | Not started |

## BF6: Schedules on records

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-SCHED-001 | Relative to a record's date | Not started | AT-BF-SCHED-001 | 0 | Not started |
| BF-SCHED-002 | Repeating schedules | Not started | AT-BF-SCHED-002 | 0 | Not started |
| BF-SCHED-003 | When a linked document changes | Not started | AT-BF-SCHED-003 | 0 | Not started |
| BF-SCHED-004 | Mail templates | Not started | AT-BF-SCHED-004 | 0 | Not started |
| BF-SCHED-005 | Proof: schedules as configuration | Not started | AT-BF-SCHED-005 | 0 | Not started |

## BF7: Reconciliation in Studio

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-RECON-001 | Ledger source in Studio | Not started | AT-BF-RECON-001 | 0 | Not started |
| BF-RECON-002 | Records against the books | Not started | AT-BF-RECON-002 | 0 | Not started |
| BF-RECON-003 | Certified metrics on dashboards and objectives | Not started | AT-BF-RECON-003 | 0 | Not started |
| BF-RECON-004 | Proof: reconciliations as configuration | Not started | AT-BF-RECON-004 | 0 | Not started |

## BF8: Custom entities that carry an industry

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-ENTITY-001 | Child tables | Not started | AT-BF-ENTITY-001 | 0 | Not started |
| BF-ENTITY-002 | Unique keys | Not started | AT-BF-ENTITY-002 | 0 | Not started |
| BF-ENTITY-003 | Numbering and totals | Not started | AT-BF-ENTITY-003 | 0 | Not started |
| BF-ENTITY-004 | Links both ways | Not started | AT-BF-ENTITY-004 | 0 | Not started |

## BF9: Packs, and the proof

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-PACK-001 | Pack format | Not started | AT-BF-PACK-001 | 0 | Not started |
| BF-PACK-002 | Signed and installed | Not started | AT-BF-PACK-002 | 0 | Not started |
| BF-PACK-003 | Upgraded and removed | Not started | AT-BF-PACK-003 | 0 | Not started |
| BF-PACK-004 | Exported between workspaces | Not started | AT-BF-PACK-004 | 0 | Not started |
| BF-PACK-005 | Proof: a clinic as configuration | Not started | AT-BF-PACK-005 | 0 | Not started |
| BF-PACK-006 | A small hotel, and what still needs code | Not started | AT-BF-PACK-006 | 0 | Not started |

