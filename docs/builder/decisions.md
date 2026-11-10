# Builder-first decisions

Decided 10 November 2026, before any gate of [the plan](../plan/builder-first.md) was built
(BF-BASE-001). Each says what it rules out and which gate carries it out. A decision changes only by a
new entry here that says what it replaces.

## BD-01. What configuration holds, and never holds

**Holds**: entities, fields, child tables and keys of a workspace's own; workflows and approvals; roles,
portal roles included; public pages; rules, decision tables and processes (Logic solutions); schedules
and mail templates; Studio's reports, charts and dashboards; seeds and words; and packs of all of these.

**Never holds**: code, SQL, a credential (only a reference to one kept apart), anything that writes a
ledger or stock row directly, tax computation, or anything that runs outside the kernel's gates. What
configuration does goes through the same paths a person's request does: row scopes, field groups,
validation, the app's own hooks and guards (which run after the customer's rules), the capability gate
with its receipts, and the audit trail. A configured step that would need more rights than its
reviewed grant is refused, never widened.

**Rules out**: an "advanced" escape hatch with scripts; configuration writing tables; an app guard
skipped because a configuration says so.

## BD-02. Portal access (BF1)

* A role gets a **portal** mark. A portal role holds own-record rules, and read-only rules reaching
  every record of **reference data** (an entity no package declares as people's personal data and with
  no email or phone field: consent forms, appointment types), to pick from; never a rule for every
  entity (`*`), never delete, cancel, export or import, and no administrator rights. Field groups are
  reached only when a rule names them. (Amended 10 November 2026, from the BF1 walk: a patient giving a
  consent picks the form, which is nobody's own record.)
* **Portal people hold only portal roles**; a member role given to one counts for nothing, as all roles
  do today. **Guests hold none.** Members may hold portal roles (an employee who is also a customer).
* An own-record rule may name a **path** (`via`) from the record to a field holding an email: at most
  two links, then optionally a child table, then the field (`patient.email`,
  `student.guardians.email`). The record is the person's when the path ends at their email, compared
  as the kernel compares owner fields today (trimmed, case-insensitive). The path is checked against the
  schema when saved and again on each use: a path whose field is gone reaches nothing, never more.
* It is enforced where row scopes are: lists, single records, creates, changes, actions, exports,
  Studio's queries and search, through the web interface, `/api/v1`, MCP and processes alike. Lists use
  SQL subqueries joined on the workspace; a single record is checked in the same transaction, never
  from a cache, so a guardian removed from a student loses the student's records at once.
* The portal shows generic pages from the person's portal roles (BF-PORTAL-004), beside the pages apps
  contribute. Apps keep their own portal pages until their configured equivalent reaches parity.

**Rules out**: precomputed lists of a person's records (stale after a change); member roles for portal
people; a rule reaching records by anything but the person's own email.

## BD-03. Public pages (BF2)

* Public pages are **Forms'**: Forms already serves public forms with server-checked answers, limits
  and responses turned into records. A public page is a form bound to an entity, with an optional
  availability source, a guest link, mail templates and limits.
* The record is created through the kernel by the **page's own service account**, holding create on the
  bound entity and nothing else, so validation, hooks, numbering and audit apply.
* **Availability** is computed by the platform from configuration (a resource's weekly hours, a length,
  the records holding the resource, the business calendar's holidays, or a capacity per date), and
  re-checked in the transaction that books, under a lock on the resource, so the last time cannot be
  sold twice.
* **Guest links** are random 32-byte tokens kept only as SHA-256 hashes with an expiry; the page's
  window decides what the link may still do.
* Online payment stays with Commerce and its provider keys (BLOCK-PAYMENT-KEYS); a page records what
  is due.

**Rules out**: a second public-form engine in the platform; a public endpoint writing records outside
the kernel; tokens stored in plain.

## BD-04. Approvals and business time (BF3)

* Approvals extend **record workflows** (`WorkflowTransition`), not a parallel engine: a transition may
  need N approvals from a role or a person named in a field; each decision is a row keeping who, when,
  the record's version and the outcome. The transition happens when the Nth approval lands, in that
  approval's transaction. A rejection ends the round.
* **Independence** extends `not_creator` and `not_creator_of` with "not whoever made transition X",
  kept in the record's transition history.
* Delegation reuses the standing leave substitutes process tasks already have (FEO-034); escalation is
  a schedule (BF6) on the approval's age.
* **Business calendars gain hours of the day** (per weekday, in the calendar's zone). Targets on states
  are measured in business time; states marked as pausing stop the clock; a breach is a row written
  once (unique per record, target and round).

**Rules out**: approvals outside the record (an email reply as a decision); a breach computed only when
someone looks.

## BD-05. Money through capabilities (BF4)

* ERP's money and stock steps are **ERP's actions with contracts** in the capability catalog (effect,
  idempotency, completion, compensation), running ERP's own code. The builder calls them; it never
  posts a ledger line itself.
* Amounts, accounts and items come from **decision tables** typed in money; a process step binds them
  to a capability's input.

**Rules out**: a generic "post journal" capability open to every role; amounts computed in floating
point; a refund without its reversal.

## BD-06. Custom entities that carry an industry (BF8)

* Custom entities stay in `custom_record`. **Child rows are rows of `custom_record` too**, with
  `parent_id`, `parent_field` and `idx` columns (indexed), so Studio and rules read them like any
  entity and a parent and its rows are saved in one transaction.
* **Unique keys** are enforced through a side table, `custom_key (tenant_id, entity, key, value)`, its
  primary key the guard, written in the record's transaction: no DDL per workspace, and one workspace's
  keys never constrain another's. Adding a key over existing duplicates is refused, listing them.
* Numbering and totals use the platform's numbering series and exact-decimal rules (BF5).

**Rules out**: generated tables per workspace; uniqueness checked by a read before a write.

Amended 10 November 2026, when lines and keys were built (BF-ENTITY-001, BF-ENTITY-002):

* `custom_record` has no `parent_field` column. A custom child entity is the lines of one table field
  only (a second use is refused), so the child's name, already in `entity`, tells one table's rows from
  another's.
* `custom_key` carries `record_id`, the record holding the value, with a cascade from the record: a
  save can tell its own value from another record's, and deleting a record frees its keys.
* Totals over lines are a field property, `rollup` (a sum of a row field, or a count of rows), computed
  in exact decimals at every save, on any entity.

## BD-07. Packs (BF9)

* A pack is a **canonical JSON document** (the platform's canonical form and digest): metadata (name,
  version, publisher, requirements on installed apps), then customizations, custom entities, roles,
  workflows and approvals, public pages, Logic solution sources, schedules, templates, Studio items,
  seeds and words.
* It is **signed** with the publisher's Ed25519 key, the signing apps already use
  (`crates/apps/src/signing.rs`), verified against keys the workspace trusts; installing shows the plan
  and needs consent to its digest, as apps do.
* An upgrade shows its impact through the customization baselines (FEO-056) and keeps the customer's own
  changes; removal keeps records unless asked to delete them, and refuses while records are held.
* A workspace's configuration exports as an unsigned pack; signing is the publisher's act.

**Rules out**: packs carrying code, credentials, records other than seeds, or other workspaces' data.

## BD-08. How an app gives way to a pack

An app keeps running until its pack passes the app's acceptance scenarios restated against the pack.
Then a migration moves the app's records into the pack's entities (expand, backfill, verify, contract),
and the app is removed. Nothing is written by both at once. Until then both may be installed in
different workspaces, never for the same records.

## BD-09. Names

* **App**: code in its own repository. **Pack**: configuration, installable and versioned.
* **Customization**: a change to an entity (overlay operations). **Solution**: a Logic release (rules,
  tables, processes); a pack holds solutions.
* **Portal role**: a role marked for the portal. **Public page**: a Forms page bound to an entity.
* **Schedule**: a time-based trigger on records. **Approval**: a decision a transition waits for.
