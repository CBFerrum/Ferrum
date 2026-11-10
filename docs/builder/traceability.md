# Traceability

Generated from [docs/registry/bf.yaml](../registry/bf.yaml) by `scripts/registry.py generate`; edit the registry, not this.
Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.

| State | Requirements |
|---|---|
| Not started | 29 |
| Partial | 2 |
| Implemented, unverified | 1 |
| Verified in software | 14 |
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
| BF-PORTAL-001 | Portal roles | Verified in software | AT-BF-PORTAL-001 | 1 | A role marked for the portal holds own-record rules, and read-only rules on reference data (no package declares the entity personal and it keeps no email or phone), never every entity, delete, cancel, export or import; portal people hold portal roles only, guests none, neither an administrator (portal_it; mutants portal-role-holds-anything, portal-reads-personal-data-whole, portal-people-hold-member-roles, portal-roles-given-to-anyone). Whether an entity is reference data is read from declarations and field types: an undeclared entity keeping people's details in a text field counts as reference data |
| BF-PORTAL-002 | Ownership through links | Verified in software | AT-BF-PORTAL-002 | 2 | Paths of at most two links and a child table to an email or text field, compared trimmed in any case; lists by SQL subqueries, single records looked up in their own transaction, never cached: a guardian taken off a student loses the student's records at once; another workspace's same email reaches nothing (portal_it; mutants portal-path-not-looked-up, portal-path-is-whoever-made-it, portal-path-ignores-the-rules-condition, portal-path-matches-case). Paths go outwards only: a record that links to the person's from elsewhere is not reached. Not measured on soak data (no index is added for the email fields paths end at) |
| BF-PORTAL-003 | Fields and actions under portal roles | Verified in software | AT-BF-PORTAL-003 | 1 | Grouped fields stay hidden unless a rule names the group; a rule names the actions its holders run whatever the action's permission, under its condition and its app's checks; a record's comments, files, assignments, tags and history are refused to portal people and guests; unscoped reports refuse paths (portal_it; mutants named-action-runs-on-any-record, rule-names-an-action-that-is-not-there, portal-people-read-a-records-surroundings, portal-people-read-a-records-history). Global search was not exercised as a portal person; it goes through the same list scopes |
| BF-PORTAL-004 | Generic portal pages | Verified in software | AT-BF-PORTAL-004 | 2 | Studio 0.3.0 gives the portal Your records: the entities a person's portal roles reach as theirs, their lists and forms, the actions they may run, without a record's discussion or activity. Walked in a browser as a portal person: lists, a record, a consent created, an action run, someone else's visit not found. A workspace on Studio 0.2.0 gets the page once Studio is upgraded; not walked on a phone |
| BF-PORTAL-005 | Portal roles administered and previewed | Verified in software | AT-BF-PORTAL-005 | 2 | Administrators mark a role for the portal, give rules their path and actions, and give portal people portal roles only, in Administration (walked); the explanation names the rule's path (portal_it, mutant explanation-ignores-the-record). The explanation screen itself was not walked for a portal person |
| BF-PORTAL-006 | Proof: two portals as configuration | Verified in software | AT-BF-PORTAL-006 | 2 | Healthcare's Patient Portal and Education's Family roles, shipped as configuration, reach exactly the visits, enrollments, attendance, fees and published report cards the apps' own pages show, with cancelling held to the clinic's window and consents recorded as the pages record them (healthcare_it, education_it). The apps' own portal pages stay until their pages are retired for the generic ones (BD-08); invoices stay on ERP's page |

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
| BF-SCHED-001 | Relative to a record's date | Verified in software | AT-BF-SCHED-001 | 2 | An automation with a relative schedule fires at a date or datetime field plus a signed offset of days, hours and minutes, a date field's at a time of day in the workspace's zone, once per record and moment however many replicas look (two scans at once, one run); the run reads the record afresh, so a date moved fires at its new moment (again, if the old one fired), a date cleared or moved away since the scan does not fire, a deleted record is skipped, and the condition is judged then; nothing already past fires for a new automation; right instants in Kolkata and across New York's change to daylight saving (schedule_it; mutants scheduled-moment-not-rechecked, scheduled-ignores-the-zone, schedule-takes-any-field). The smallest step is a minute; a custom field's dates are read out of JSON with no index; moments more than seven days late are skipped; no business calendar; not walked in a browser (the Administration dialog does not offer schedules yet) |
| BF-SCHED-002 | Repeating schedules | Verified in software | AT-BF-SCHED-002 | 2 | Every day, week (a weekday) or month (a day 1-28 or the last) at a time in the workspace's zone, for each record a filter of at most eight conditions passes, or every day, week, month or year from a record's date until another; each occurrence fires once across replicas, and a scan stopped part way resumes from its cursor, a page at a time, none lost or doubled; after downtime the last seven days are caught up and older moments skipped (schedule_it; mutants scheduled-page-repeats-its-cursor, scheduled-catches-up-forever, scheduled-occurrences-never-pruned). Days 29-31 of a month are not offered (last is); a repeating schedule on a datetime field looks at every started record on each look; the scan runs once a minute, so a moment fires up to about a minute and a half late; not walked in a browser |
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
| BF-ENTITY-001 | Child tables | Verified in software | AT-BF-ENTITY-001 | 2 | A custom master or document holds tables of custom child entities, their rows in custom_record under the parent, each child entity the lines of one table field only. Two tables of one order never mix through loading, list filters, adding, changing, removing and reordering rows, deleting (rows cascade), the workspace's export, Studio's grouping, measures and filters, or a portal path; rows are validated one by one, rules read the parent, 5,000 rows at most (custom_lines_it in the kernel and Studio; mutants custom-lines-load-mixes-tables, custom-lines-filter-mixes-tables, custom-lines-portal-path-mixes-tables, custom-lines-studio-grain-mixes-tables, custom-lines-studio-filter-mixes-tables). No index on a row's fields, which live in JSON; a row rule reading the parent reads all of it per row, slow near 5,000 rows; a customization adds no table to a package's entity; not walked in a browser |
| BF-ENTITY-002 | Unique keys | Verified in software | AT-BF-ENTITY-002 | 1 | Unique fields and keys of several fields on a custom master or document are rows of custom_key, written in the record's transaction with its primary key as the guard: of two saves at once one wins and the other is refused naming the fields; a changed, deleted or cancelled record frees its values, empty values hold none, another workspace's are its own; a key defined over records sharing values is refused listing them, and one removed takes its rows (custom_lines_it; mutants custom-keys-not-kept, custom-keys-held-by-cancelled, custom-key-over-duplicates). Values compare as typed (case counts); a child entity holds no keys; engine-kept values (set_values) may not touch a key field |
| BF-ENTITY-003 | Numbering and totals | Partial | AT-BF-ENTITY-003 | 1 | Totals are done: a field's rollup sums a number field of its own rows, or counts them, in exact decimals at every save and in the draft preview, before validation, on any entity, ignoring what a client sends (custom_lines_it: 0.1 + 0.2 is 0.3, a 19-digit sum is exact; mutant rollup-not-computed). Missing: numbering series on a custom document were not exercised against AT-BF-ENTITY-003; a rollup has no condition and no other operation, and computed fields see the total of the save before |
| BF-ENTITY-004 | Links both ways | Not started | AT-BF-ENTITY-004 | 0 | Not started |

## BF9: Packs, and the proof

| Id | Requirement | State | Case | Tests | What it does not yet do |
|---|---|---|---|---|---|
| BF-PACK-001 | Pack format | Partial | AT-BF-PACK-001 | 2 | Done: a ferrum.pack/1 JSON document with a name, a version and the packages it requires, holding custom entities, fields, rules, workflows, print templates, roles and their rules, automations with their schedules and mail templates, Studio items, Logic solutions and reference records (seeds), read strictly (unknown keys refused, 5 MiB at most), with the platform's canonical digest leaving out exported_at: two exports of one definition have one digest, a changed word changes it (pack.rs unit tests, pack_it; mutant packs-digest-not-checked). Missing: approvals, public pages and words (their gates are not built), a publisher, and signing (BF-PACK-002) |
| BF-PACK-002 | Signed and installed | Not started | AT-BF-PACK-002 | 0 | Not started |
| BF-PACK-003 | Upgraded and removed | Not started | AT-BF-PACK-003 | 0 | Not started |
| BF-PACK-004 | Exported between workspaces | Verified in software | AT-BF-PACK-004 | 2 | A gym built by configuration (an entity with lines, a key, a total, a check and a workflow; a role and a portal role with a path; an automation on each new record and a daily one with a mail template; a report, chart, card, dashboard and space; a solution whose rule counts; three reference rows) is exported, planned and installed in a fresh workspace, where each works as it did, the solution as a draft a second administrator reviews before it runs; installed again it changes nothing; a field, a condition and a row changed at the source follow, records kept; a wrong digest, an oversized or unknown-keyed pack, a non-administrator, a missing or older package and a refusal half way are each refused with nothing kept (pack_it, pack_api_it; mutants packs-digest-not-checked, packs-plan-kept, packs-automations-duplicated, packs-carry-people). Unsigned and administrator to administrator only; nothing is uninstalled or deleted; Studio queries and JSON fields holding record ids travel as they are; matched by name, label and key; not walked in a browser (no screen yet) |
| BF-PACK-005 | Proof: a clinic as configuration | Not started | AT-BF-PACK-005 | 0 | Not started |
| BF-PACK-006 | A small hotel, and what still needs code | Not started | AT-BF-PACK-006 | 0 | Not started |

