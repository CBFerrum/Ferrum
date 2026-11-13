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
* (Added 10 November 2026.) A workspace's own menu may be marked for the portal (`portal: true`). Its
  entries on the portal's home are filtered by the same test as the generic page (what the person's
  portal roles reach as theirs), shown on Studio's page, with no page or steps of their own
  (docs/builder/portal.md, Portal menus).

* (Added 10 November 2026, from the portal pages' scoping.) An outside person takes a workflow
  transition only when it names one of their roles; a transition naming no roles is for members who may
  change the record (write scope), never for one who may only read it.

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
* *2026-11-10, built (service targets).* A target is a workspace's customization of any entity
  (`add_target`: a calendar, `start_when`, `within`, `pause_when`, `met_when`, and two fields the kernel
  keeps: the due moment and a state word). Pausing is an expression on the record (a workflow state, a
  status), not a mark on states, so a target works on entities with no workflow. Each record keeps one row
  per target (`record_target`): the round number, when it started, resumed and paused, the open seconds
  worked, the seconds allowed, the due moment, and when it was met and breached; no list of intervals. A
  pause adds the open time since the last resume to what was worked; a resume adds what is left from now. A
  re-opened record starts the next round in the same row. The breach is `breached_at`, set by the minute
  loop with an update that only succeeds while it is empty, so one replica or pass marks it; the two kept
  fields are then written as a workflow transition writes them, in any document status and past the
  record's rules, with the record's `updated` event, so automations see the state word change (`old` in
  their conditions). What a
  save does to a round is one pure function (`ferrum_logic::target::step`).
* *10 Nov 2026:* hours of the day are built (`logic_calendar.hours`, `crates/logic/src/calendar.rs`):
  windows per weekday in the zone, and `add_working`, `working_between`, `next_open`, `is_open` with
  stopped intervals; waits take `{"working": "PT8H", "calendar": …}`; administrators keep calendars at
  `/api/admin/calendars`. Daylight saving: open time is real elapsed time, a window runs from the first
  instant its start shows to the last instant its end shows, and a skipped time is the first instant
  after the gap. Targets on states, pausing states and breach rows are not built.

* *12 Nov 2026, built (BD-19):* approvals on a transition, as above, with three departures. A decision is kept with
  its request and step, not the record's version: a policy names the fields whose change cancels the decisions so
  far (`reset_on`), so a typo fixed in a note does not send an order round again. Escalation and reminders are a
  step's own thresholds (working time on a calendar when it names one) that the minute loop acts on once each, not a
  scheduled automation on the request's age. A rejection may move the record (`on_reject`) besides ending the round.

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

Amended 10 November 2026, when a designer lost a workspace's later changes: a redefinition (`redefine_entity`)
replaces every definition change made before it, so a designer starts from the effective definition
(`GET /api/customize/definition/{Entity}`), never from the latest definition row, and the change list marks
each customization a later redefinition replaced (`superseded_by`), by the same rule the schema's build applies.
A pack that would leave the workspace's own changes replaced says so in its plan.

Amended 11 November 2026, when Governance and Risk's control tests needed several evidence rows each (row C4
of evidence/grc-as-a-pack-2026-11-10.md): **a custom line entity may hold lines of its own, two levels deep**
(a record, its lines, their lines). A line's lines are `custom_record` rows whose `parent_id` is their line's
row, so the self foreign key's cascade takes them with their line and their record; still one table field per
child entity, so the third level is refused where the schema is built and a package's child entity holds no
table. Load is one statement (each line's rows nested in it); a save writes each table across all its parents
in at most three statements (delete what went, read the JSON of the rows changing, write those), whatever the
number of rows, and leaves unchanged rows alone. A row stays under its line (one sent under another is
refused), `parent` in a line's line is its line, and a field error there carries `path` beside `row`. List
filters, portal and owner paths, personal-data and hand-over declarations and Studio stay one level deep.
**Rules out**: a third level; nested lines on a package's entity; moving a line's line by its id.

Amended 10 November 2026, for a checklist on any record, sign-offs on an invoice, measurements on a work order:
**a customization may add a table of lines to a package's entity**, one level deep. The field is `x_…`, `type:
table`, `of` a custom child entity that is the lines of no other table and holds no table of its own; a package's
line entity, a ledger and a custom entity (whose tables come with its definition) take none. The rows are
`custom_record` rows of that child entity with the package's record id in `parent_id`; which table a table's rows
are in follows the child (`ChildInfo.table`), so load, save, filters, scopes, erasure, merge links and the delete
guard read them as they read a custom entity's lines. The self foreign key on `parent_id` could not hold a parent
in a package's table, so it gave way to a statement trigger on `custom_record` doing what its cascade did
(migration `20261110000010_packaged_lines.sql`, nothing rewritten). **Deleting a package's record** deletes the
`custom_record` rows under its id, also while the customization is off. **Uninstalling the package** keeping its
data keeps them, as it keeps its own rows; deleting its data deletes exactly the `custom_record` rows under its
entities' records, never the table (which holds every custom record of the workspace), and leaves the package's
own child tables to their own deletes. Switching the customization off hides the lines and keeps them; a package
upgrade leaves them. Checked by `packaged_lines_it` (an orphan check after each step; mutant
`uninstall-deletes-custom-record-wholesale`). **Rules out**: a generated table per customization; a package's
child table holding rows of a custom entity; a second level on a package's entity.

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

Amended 10 November 2026, when exporting and installing were built (BF-PACK-001 in part, BF-PACK-004):

* What a pack is today: an **unsigned** `ferrum.pack/1` JSON document an administrator exports from one
  workspace (`GET /api/admin/pack`) and installs in another (docs/api.md, "Packs"). It holds the active
  customizations (custom entities, fields, rules, workflows, print templates), the roles the workspace
  made and the rules it added to packaged ones, its automations (schedules and mail templates with them),
  Studio's items, the running release of each solution as a solution pack, and the reference records the
  exporter names, matched by key; no publisher yet. Its digest is the platform's canonical digest without
  `exported_at`, so two exports of an unchanged workspace have one digest.
* Installing shows a plan (the install, rolled back) and takes the plan's digest; everything but the
  solutions is applied in one transaction through the admin API's own functions, and each solution arrives
  as a draft that a second administrator reviews and someone activates. Installing again changes nothing;
  nothing is removed or deleted.
* "Records other than seeds" reads as reference records the exporter names, from entities holding no
  people's data, email, phone or secret.
* Still to come, inside a signed bundle: the publisher's signature and the trusted keys, upgrades shown
  through the customization baselines, removal, approvals, public pages and words, and an app store of packs.

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

## BD-10. A rule's aggregates read as the workspace

Decided 10 November 2026. Qualifies BD-01.

A Logic rule's aggregates (docs/logic/rules.md) count and sum every matching record of the workspace,
whoever saves: the saver's row scopes, read rights and field groups do not narrow them, and tenant isolation
is unchanged. A check that depends on who triggers it does not hold: "no visit while charges are unpaid"
that passes for anyone who cannot see charges is no control. This is how an automation already runs, as the
administrator who saved it, not as whoever changed the record. A rule's lookups keep reading with the
saver's rights.

What follows: a `derive` rule can write a total of records its saver cannot open, so whoever reviews a
release reads its aggregates (listed in the release's review) as reads across the workspace.

**Rules out**: an aggregate scoped to the saver, and any other configured read that widens beyond the
workspace or reaches a record's fields, not only its counts and sums.

## BD-11. Configured actions

Decided 10 November 2026. Qualifies BD-01. Asked for by rebuilding Governance and Risk as a pack
([evidence](evidence/grc-as-a-pack-2026-11-10.md)).

A workspace may add an action to an entity by customization (`add_action`) when the action says what it does
in `effects`: automation actions (`set_fields` on its record, `update_linked`, `create_document`, `notify`,
`email`) run in order in the action's one transaction. Its runner is checked as any action's is (the
permission it names on the record or a role's rule naming it, the record's scope, its document status and
`condition`), then against its `roles` and `refuse_if`, whose refusal is *forbidden* with the administrator's
words. The effects then run **as the system, on the runner's behalf**: they may write the record's read-only
fields (a status moved only by buttons, which is the point), records they write say the runner changed them,
and the audit trail records who ran which action with what parameters.

Why this is not the escape hatch BD-01 rules out:

* **No code and no SQL.** An effect is one of five automation actions whose values are CEL expressions,
  compiled and checked against the entities when the customization is saved.
* **Every check applies.** Each write goes through the save pipeline every write uses: computed fields,
  rollups, checks, Logic rules, unique keys, workflow edit rules and the apps' own hooks and guards on the
  record written, and the usual events and automations after commit. A refusal undoes the whole action.
  Running as the system lifts the writer's row scopes and read-only flags, never a check.
* **Narrow reach.** An effect writes the workspace's own fields (a custom entity's, a packaged entity's
  `x_` fields: what a Logic derivation may fill) and the fields a person may type; never the platform's
  columns, a computed, secret or grouped field, or a package's read-only field. A packaged entity's action
  is named `x_…`. No effect runs another action, so an action cannot loop; events it causes follow the
  automations' chain rule.
* **Bounded.** At most 20 configured actions on an entity, 10 effects and 10 refusals on an action.

This is the trust automations already have (they run as the administrator who saved them): an
administrator decides once, in reviewed configuration, what a button does; the person pressing it is held
to who may press it.

**Rules out**: an effect that writes the platform's or a package's own fields, runs another action or a
script, or skips a check because it runs as the system.

## BD-12. A workspace's own menu

Decided 10 November 2026. Asked for by Governance and Risk as a pack (rows U3–U6 of the
[evidence](evidence/grc-as-a-pack-2026-11-10.md)).

A coded app gets a home tile, quick create, a menu and the assistant's notes from four parts of its
manifest. A business built by configuration gets all four from **one definition**, `define_module`: a
module named `x_…` (packages never take such names) with a label, an icon and a colour, its entities and
reports in order, the entities it offers to create, and the assistant's instructions and suggestions.

* **One definition, one place.** The properties only a workspace's menu has (`color`, `description`,
  `create`, `assistant`) are refused on a package's module: an app says these in its manifest, so each
  thing has one way to be said.
* **Shown to whoever reads an item.** No second list of roles to keep in step with the role rules: a
  member who may read one of the menu's entities gets its tile; a Create row needs the right to create.
* **It names what is there.** A menu naming an entity the workspace lacks is refused; one whose entity
  goes later (an app uninstalled) loses that item with a schema warning and keeps the rest.
* **It is records, not a screen.** `/data/{menu}/` is the records frame scoped to the menu, as a
  records-only app's is. A screen of its own, a search provider and live tile counts stay with apps.

## BD-13. Configuration calls other apps' steps through the capability gate

Decided 10 November 2026. Carries out BD-05's first half and qualifies BD-11. Asked for by the paper
measurement of Government and Retail ([evidence](evidence/government-retail-on-paper-2026-11-10.md)),
where one gap blocked nine behaviours: a fee invoiced in ERP, a delivery note posted on hand-over.

A configured action's effect or an automation's action may `invoke` a capability of the workspace's
catalog (docs/api.md, "Calling another app's steps"): an entity's record operations, its package's
listed actions, its workflow's transitions.

* **The gate, not a path beside it.** It runs `invoke_tx`: the owning app's operation through the path a
  person's request takes, every hook, rule, check and posting of that app, in the action's or automation's
  one transaction. A refusal anywhere refuses the whole press and keeps nothing.
* **As the system, on the runner's behalf** (BD-11): an administrator decided once, in reviewed
  configuration, which step a button takes; the person pressing it is held to who may press it.
* **Once.** A capability that changes something runs once per effect key, the receipt committed with the
  change: by default per action (or automation), record, record version and effect, or per a `key`
  expression for good. The same key with other input is refused, never adopted.
* **Named and checked.** The capability and its input names are checked against the catalog when saved;
  one that goes later refuses the press, saying so, and breaks nothing else.

**Rules out**: configuration writing an app's tables or ledger lines itself; an effect reaching a
configured action through the gate (no effect runs another one); a step that changes something without
an effect key; a new kind of permission for it.

## BD-14. Linked records in expressions, and effects on a record's own lines

Decided 10 November 2026. Qualifies BD-10 and BD-11. Asked for by the paper measurement of Government and
Retail ([evidence](evidence/government-retail-on-paper-2026-11-10.md)): G6, G12, G15, G16 and R23.

An expression may read the record a link field names as `links.<field>`, its fields and its lines (docs/api.md,
"Linked records"); a configured action or an automation may append a line to the record (`append_line`) or
change the one line an expression picks (`set_line`).

* **Read as the workspace.** As BD-10 reads aggregates, and for the same reason: "submit needs every
  document the permit type lists" must not pass for a clerk who cannot open permit types. Secret fields
  are left out. What follows: a condition, a refusal or an effect's value can depend on, and an effect can
  copy, values of a record the person could not open themselves. An administrator sets that up, in
  reviewed configuration, as with automations and configured actions; it is not a way for a person to read
  more.
* **One level.** `links.type.documents` works; a link of the linked record is an id. No chains, no
  `links` of lines.
* **Only where a transaction is at hand.** Entity rules, an action's condition and refusals, workflow
  guards, automations and service targets. Not `default_expr`, `computed`, `visible_if`, `required_if`,
  `read_only_if` or a section's condition: they are worked out without a transaction and in the browser,
  which has no linked records, and a field that changed with a record a person never saved would be a
  surprise. The approvals inbox, which has no transaction, takes such a guard to hold; taking the step
  decides.
* **Known before it runs.** The names come from the expression itself, checked when saved against the
  entity's link fields, at most five; `links` read any other way is refused. Each place reads each name it
  uses once, without a lock: a rule sees a linked record as it is when the save runs.
* **Lines through the ordinary save.** `append_line` and `set_line` build a patch naming every stored line
  by its id, so other lines keep their values; the record's computed fields, row rules, totals, checks and
  audit run, one version per effect. A submitted record refuses them. No other effect writes a table.

**Rules out**: chains of links, a linked record read with the saver's rights in these places, `links` in
expressions the browser evaluates, lines within lines, and a role named by data.

## BD-15. Published lists

Decided 10 November 2026. Asked for by the paper measurement of Government and Retail
([evidence](evidence/government-retail-on-paper-2026-11-10.md)): G33, the public register Government writes in
its own code today.

A workspace publishes some fields of some records of an entity to anyone, without signing in, by
configuration (`publish_list`, docs/api.md "Published lists"): a register of issued permits, a price list, the
classes on offer.

* **A read-only allow-list.** The list names the fields that are sent, and nothing else leaves: not the id,
  not the owner, not the timestamps; a link shows its record's title. The filter and the order pick and sort
  rows and are never sent. A submittable entity shows submitted records only.
* **Refused when saved, not hidden when served.** Email, phone, secret and attachment fields, permission-group
  fields, fields never exported and declared personal data cannot be named at all, so an administrator learns
  at once what will not be public. A later change takes away what may no longer be shown, with a warning, and
  never widens a list.
* **Read as the workspace.** As BD-10's aggregates: the list is the workspace's statement of what is public,
  so nobody's permissions decide the rows. That is why the field rules are strict.
* **One answer for everything not published.** An unknown workspace, an inactive one, an unknown list and one
  taken down all give the same 404.
* **Small.** One page of at most 1000 rows, JSON, CSV and a plain page with no script; no paging, no search
  a visitor types, no page builder.

**Rules out**: public writes (public forms are Forms', BD-03); publishing email addresses, phone numbers,
secrets, files, permission-group fields or people's declared personal data; content that depends on who is
looking (per-visitor content, a guest's own records: those are the portal's, BD-02); a list of lines or ledger
rows.

## BD-16. Expression functions are additions

Decided 12 November 2026. Asked for by the Government pack ([evidence](evidence/government-as-a-pack-2026-11-11.md)):
a permit's expiry in calendar months took 2,479 characters, "Check" and "check" could not be told apart, an
empty number could not be told from 0, and a refusal could not say what was missing.

* **Additions only.** Dates, text, `isEmpty` and `coalesce` are new names, and CEL's strings extension is
  switched on ([expressions](expressions.md)). Every expression that evaluated before gives the same value; a
  name CEL already gives a meaning keeps it.
* **Empty stays readable as 0 unless asked.** An empty number still reads as 0 and an empty tick as false
  everywhere, so `doc.qty > 0` on a new record and every shipped check keep their answers. Only `isEmpty(x)` and
  `coalesce(x, ...)` given a field directly see that it was left empty; a value that is only a path still
  copies an empty value as empty (docs/api.md, "Configured actions").
* **An empty date gives empty.** A date function given no date gives null rather than failing a rule or a save.
* **Messages from the record, as the reader sees it.** A rule's or refusal's message may hold `{{ doc.<field> }}`
  (MiniJinja, as print templates), rendered over the record as the person told may read it; a message with no
  placeholder is shown as written. A rule may name the field its refusal is shown on.

**Rules out**: changing what an empty value reads as in existing expressions; a message showing a field its
reader may not read; functions the browser evaluates (it leaves any call to the server).

## BD-17. A workspace's settings

Decided 12 November 2026. Asked for by the GRC and Government packs ([GRC](evidence/grc-as-a-pack-2026-11-10.md) R9,
[Government](evidence/government-as-a-pack-2026-11-11.md)): a threshold or a policy a pack's rules read had to be
written into each rule, so changing it meant changing configuration.

* **A settings entity of the workspace's own.** `define_entity` takes `"kind": "settings"`, as 23 apps declare
  theirs: one record for the workspace (a second is refused, `Claim Policy holds one record for the workspace`,
  kept by a key of no fields in `custom_key`, so two saves at once cannot both make one), edited on its form, with
  permissions, rules and history like any record. Not per company.
* **Read as `settings.<Entity>.<field>` everywhere a record expression is** (docs/builder/expressions.md,
  "Settings"), a workspace's settings and a package's alike, the field's default until the record is saved.
* **Read as the workspace** (as BD-10's aggregates and BD-14's links): a rule must hold for someone who may not
  open the settings. Secret fields are never read.
* **Known before it runs.** Only `settings.<Entity>.<field>`; checked when saved against the schema (the entity of
  the settings kind, the field there and not a secret), refused naming it. The names come from the expressions, so
  each place that evaluates (a save, an action's checks, one effect, an automation's run) reads what it names once,
  in its transaction, and nothing when it names nothing. Nothing is cached: a change is seen by the next
  evaluation, on every server.
* **Packs carry them** as they carry entities; a pack carries the record's values only when the exporter names
  the entity among its reference records, and installing never overwrites a record the workspace already has.

**Rules out**: settings per person or per company in expressions; a settings value read with the saver's rights;
a secret in an expression; Logic's typed expressions and aggregate filters reading `settings` (they keep solution
parameters); `settings` read whole or by a computed name.

## BD-18. Effects that branch, find and loop

Decided 12 November 2026. Qualifies BD-11 and BD-14. Asked for by the Government pack
([evidence](evidence/government-as-a-pack-2026-11-11.md)): Submit and Reject were each two buttons kept in step for
want of a condition on one effect (G-rows on branching), and a second ERP customer was made under an address it
already knew (G9) for want of find-or-create; checklists from a template were typed by hand.

* **A `when` on any effect or automation action.** Absent means always, as before; nothing that ran changes.
* **`find`, with `create` and `update`.** By the fields named, which must pick exactly one record: two matching is a
  refusal, never a guess, and an empty value picks nothing. Runs that may create the same record take a transaction
  advisory lock on (workspace, entity, values) first, so the second run finds what the first made; a unique key over
  the fields refuses a duplicate made any other way. Without one, a record made by hand at the same moment can still
  be a second, and the next `find` then refuses.
* **`for_each` over a record's lines or the records linking to it**, the item as `item`; at most 100 items
  (`MAX_ITEMS`, a refusal beyond, never a silent cut), 10 effects per item, one level (no loop in a loop), and the
  chain-depth guard unchanged (a loop's writes raise events as any effect's do). An `invoke` in a loop is keyed per
  item, so each runs once.
* **`copy_lines` from a template**: the lines of a record a link names or an earlier effect found, mapped by
  expressions, appended or replacing, in one save; at most 100 lines.
* **Same trust and pipeline as BD-11**: as the system on the runner's behalf in an action (as the saver in an
  automation), every write through the save pipeline, in the one transaction; any refusal undoes the run and says
  which effect (`find:`, `copy_lines:`, `for_each, item 2, effect 1:`). Stored as written, so packs, change lists and
  `GET /api/customize/definition/{Entity}` carry them unchanged.

**Rules out**: nested loops; loops over a query of arbitrary records (only lines and records linking here); a find
that picks the first of several; a silent cut at the bound; error paths and compensation (BL-42), approvals, and
outbound HTTP in effects.

## BD-19. Transitions that do things, and approvals on any transition

Decided 12 November 2026. Carries out BD-04's approvals and extends BD-11 to workflows. Asked for by the builder
gap report (BL-25, BL-26, BL-29, BL-30, BL-32, BL-33, BL-34): eleven apps decide approvals in Rust and thirteen
"someone other than" rules; a same-status transition ran no rules, no effects and no automations.

* **A transition may ask and do.** `params` are declared as a button's; the server refuses a required one missing or
  one not asked for. `effects` are a configured action's (BD-11, BD-13, BD-18), run after the move as the system on
  the taker's behalf in the transaction, checked when saved. A rule `on: transition` runs only when a transition is
  taken, on the record in its new state, reading `transition` (`{action, from, to, params}`); a state's `required`
  already says what entering it needs. A same-status move raises `transitioned`, which automations take, reading
  `transition` (`{action, from, to, by}`), under the chain-depth guard as any event.
* **An approval policy is part of the transition**, not a second engine: steps in order (a step may open with the
  next), each naming its approvers once (a role, a field, an expression over the record and `settings`), how many
  (N, all, any), a `when` (an amount matrix is steps with conditions), independence (never the requester unless
  allowed; not earlier deciders; not whoever took a named transition, read from the record's history), and its
  reminder and escalation thresholds. Taking the transition makes a request (one pending per record) holding the
  policy as it was; when the last step is satisfied the transition completes once, as the system on the requester's
  behalf, in that decision's transaction. The record is locked before its request on every path, so concurrent
  decisions queue.
* **Counting is the process approvals' own** (`ferrum_logic::approval::satisfied`): one person twice is one; a
  requester counts only where allowed. Stand-ins are `person_away`, as for process tasks.
* **Who may decide** is worked out when a step opens and again at the decision; a step that cannot find enough
  people is refused with what opened it, never left unanswerable in silence.

**Rules out**: deciding by e-mail reply or link; a chain of stand-ins; approvals that skip the transition's own
gates, rules or effects; an approver reading a record they may not read (see below).

**Not yet** (both built the same day, in the amendment below): an approver reading the record they decide; a policy on a packaged document's own submit and cancel.

*Amended 12 November 2026 (approvals, part two):*

* **An approver reads the one record awaiting them.** While an open step awaits their decision (theirs, or for whom
  they stand in) and they have not decided it, someone with no right to the entity reads that record, its lines, its
  history and its files' downloads, and nothing else of the entity: lists, searches, writes and every other action
  still refuse them. It ends when they decide, the step closes, or the request completes, is recalled or ends; a
  reset that opens the step again asks them again, so they read again. Their own field groups still hide what they
  hide, unless the policy's `show` lists the field (none by default); a hidden field stays hidden in the history's
  diffs even when shown. Portal paths (`RowScope::admits`) did not fit: approvers are on the request, not on a path
  from the record, so the request is the grant (`Kernel::read_record`), and `get` is unchanged for every other caller.
* **Approval before a document is submitted or cancelled** (`set_approval`, `on: submit | cancel`): the same policy,
  on a submittable entity no workflow submits, without replacing the app's lifecycle. Every submit or cancel but a
  workflow's goes through `submit_tx`/`cancel_tx` (the API, create-and-submit, MCP, apps' hooks, automations), and
  there it asks: whoever presses it must be able to do it now and, for a submit, the document must pass its rules and
  the app's validation; the document stays as it is with `_approval`. Nothing the app writes on submit runs then.
* **A completed submit runs as its requester, at that moment**, never as the system: their context is loaded when the
  policy is satisfied, their right to submit (cancel) checked again, and the app's `before_submit`, `on_submit` and
  checks run as theirs, so hooks see the person who pressed it and records it writes are theirs. Why: an approval
  agrees to what someone asked; it lends them no rights, and the system's would let a submit through that its
  requester could no longer make. Refused then (their right gone, an app's check), nothing of the submit stays (a
  savepoint) and the request ends `failed`, with `failure` saying why, the requester told; the decision is kept.
  Only the system asks with no requester; its request completes as the system.
* **While pending**: a `reset_on` change voids the decisions; `lock: true` refuses any other edit (fields the server
  works out are not counted); otherwise edits are allowed. `lock` applies to transitions' policies too.
* **The system** (an automation, another app's engine) is asked like anyone unless the policy says `system_bypass`;
  a bypassing submit ends a request pending on the document.

**Still not built**: approval on a document's delete or amend; `show` on lines' fields; a policy on a submit whose
entity has a workflow (it goes on the transition, and is refused there with that said).


## BD-20. Message templates and channels

Decided 12 November 2026. Asked for by the audit of every app against its segment's leaders
(`gaps-2026-11-12`: Bookings BK-31 to BK-36, Documents DC-24, DC-40, DC-48, POS PS-58, Builder BL-48): seven apps
send messages to people by e-mail only, their wording fixed in Rust in English, and nothing sends a text or chat-app
message. Built once in the platform (`crates/kernel/src/messages.rs`; the API in [api.md](../api.md#messages)).

* **Every message is a template.** A package declares a default for each message it sends (`messages:` in its YAML,
  key `{package}.{name}`); a workspace edits a copy, sees the default beside it, and resets to it. A release that
  changes a default never writes over an edit: the edit keeps the digest of the default it started from, and the
  listing says `default_changed` when they differ. A workspace may add templates of its own, under a key no package's
  name starts.
* **Bodies are the print formats' engine** (MiniJinja, escaped for an e-mail's HTML, fuel-limited), per channel
  (e-mail subject and body, text, chat-app provider template with ordered values or free text, in-app title and text)
  and per language; the recipient's language is used when known (a user's own), else the workspace's, else English.
  A save renders every body against a record with every field of the entity and refuses an unknown field, a value
  the template does not declare, or a body that fails.
* **What a body sees of the record: the record as the workspace holds it, minus its secret fields** (and its lines'),
  never the reader-masked view. A message goes to people outside the workspace: who caused it (an automation's
  runner, a guest on a booking page, a portal person) is not who reads it, so their field groups say nothing about
  what the recipient may see; the template's author, an administrator, decides that. Secret fields are never
  rendered, whatever a template asks: they are not in what it reads, and a template naming one is refused when saved.
* **One send function and one effect.** `messages::send` for apps, `{"type": "message"}` for buttons, transitions,
  approvals and automations. For each recipient the channels are tried in order and the first they can be reached on
  is used (each, with `all`). Every send is a row of `message_send`: queued, held, sent, delivered, failed, or skipped
  with why (no address, opted out, no channel configured, a limit). Nothing is sent, and nothing recorded, unless the
  caller's transaction commits.
* **E-mail goes as all mail goes** (the outbox and the server's mailer); `email`, `notify`, the automation mail action
  and approval notices work unchanged, and may name a template instead.
* **Text and chat-app messages go to a channel connection that is configuration.** No provider is named in the kernel:
  a connection describes the HTTP request (address, method, headers, a JSON body or a form, each a template), where the
  provider's id and error are in its answer, and which statuses mean "later"; its secrets are sealed with the server's
  key as connections' are. One job per send, retried while the provider says "later", failed at once when it says
  "no". A test channel delivers into the log only. Delivery reports and replies come back on an inbound route per
  connection, authenticated as its description says (an HMAC of the body, or a shared token), refused and counted
  otherwise.
* **Choice is the consent records there are.** Operational messages always go (a restriction of processing still
  stops e-mail, as before); promotional ones respect an opt-out per address or number and channel, kept in
  `contact_consent` with a channel column (what was recorded before is about e-mail), set by a signed link, a reply's
  stop word or staff. Quiet hours hold promotional text and chat-app messages until they end, in the recipient's zone
  when known; limits per workspace and recipient an hour stop a runaway automation.
* **Numbers in international form.** A number without `+` is completed with the workspace's calling code (its trunk
  prefix dropped when it sets one); with none set it is refused with that reason, never guessed.
* **One-time codes** for apps: kept as a salted digest, sent through a template, verified once, expiring in ten
  minutes, five tries, five sends a recipient an hour. The test channel's log masks the code.
* **Personal data**: a person's sends and codes are in their report and go with their erasure, by id in its journal
  so a restore erases them again. **An opt-out outlives its person's erasure, without its address**: the address
  becomes an HMAC-SHA256 under the server's key (`FERRUM_SECRET_KEY`, and those being rotated out) of the workspace,
  channel and normalised address, which a promotional send to that address on that channel still matches, while no
  table, log, report or listing holds or finds the address (a consent given goes; staff remove an erased opt-out only
  by its row). A restored backup brings the clear rows back, as it does every erased datum, and the platform's answer
  applies: the erasure's journal keeps the digests, so `ferrum task reapply_erasures` (or a workspace taken back,
  FEO-078) erases them again (without a configured key an erased opt-out keeps its address; retiring a key for good
  stops the digests made under it from matching).

**Rules out**: a provider named in code; a message rendered with the reader's rights; a send outside the caller's
transaction; marketing past an opt-out; a number's country guessed.

**Not yet**: several connections of one kind with routing between them; reading a provider's template catalogue; MMS
and attachments outside e-mail; a person choosing channels for a notification kind (BL-48 stays the notifications'
own); scheduled reminders (an automation on a schedule sends them today).

## BD-21. Documents from data

Decided 12 November 2026. Asked for by the audit of every app against its segment's leaders
(`gaps-2026-11-12`: HR HR-15, 48, 49, 54, 60, 61, 65, 81; ERP finance FN-12, FN-16, FN-35; Documents DC-33, DC-47,
DC-57; POS PS-10, PS-54, PS-60; ERP operations OP-20; Builder BL-56, BL-57): an administrator could not design what the
system prints or files, an entity had one print layout, and there was no amount in words, no barcode and no way to
write a text or data file from records. Built once in the platform (`crates/kernel/src/documents.rs`, the helpers in
`crates/kernel/src/docfmt.rs`; the API in [api.md](../api.md#documents)).

* **Every document is a template**, declared by a package (`documents:`, key `{package}.{name}`) or a workspace, edited,
  reset and told of a changed default as message templates are (BD-20). Several per entity; one is its default; `roles`
  say who may pick one. A workspace's default changes what its entity prints with; without one a print is the print
  format as it was, byte for byte.
* **Four outputs, each escaping for its own syntax**: a page (HTML, a PDF where the server has its PDF service), text,
  CSV and XML. A file states its encoding (UTF-8, ISO-8859-1, ASCII) and line ends; a character the encoding cannot hold
  is refused, never replaced.
* **What a document shows depends on who reads it.** A person printing gets the record as they may read it, the print
  path's rule. A document the system generates for someone outside (the `generate` effect, an app's `generate`) is
  rendered as the workspace holds the record, its secret fields left out: BD-20's rule for messages, for the same
  reason. Secret fields are never rendered: a template naming one is refused when saved.
* **Language rules are data behind one interface.** Number grouping and marks, dates and month names come from a table
  of languages (`docfmt::LOCALES`); a currency's decimals and symbol from its record, else ISO's decimals; an amount in
  words from one function per language (`docfmt::speaker`) and a table of unit names per language and currency. English
  (with `en-IN` counting lakhs and crores), Hindi and Arabic, the languages the interface ships, are there; another is
  a row and a function more. Nothing is fixed to one country.
* **A file of data is an export.** Downloading a text, CSV or XML document needs export permission and is on the
  record's audit trail with its digest; a page needs read permission, as a print does.
* **Generated documents are kept and handed on.** `generate` stores the file on the record in the caller's
  transaction, replaces the template's earlier one when asked (not one a queued mail still waits for), and gives the
  file and its SHA-256 digest to later effects: a `message` attaches it (a file of the same record only), an `invoke`
  hands it to the Sign app. Signing itself is the Sign app's.
* **Bounded**: a body is at most 256 KB and its run fuel-limited as print templates are; one call renders at most 200
  records, 100 copies each, 1000 in all.

**Rules out**: a document rendered with its causer's rights for someone outside; a secret on paper; a number, date or
amount in words written in one country's way for everyone; a file whose structure a record's value can break; a
headless browser on the API server.

* **A report as a document** (added 12 November 2026). A template may be about a saved report's rows (`query: {report,
  group_by?}`) instead of one record: one file for the whole result, or one per record a link column of the rows names
  (per customer, per employee), kept on that record and mailed to it by the `report_documents` effect. The kernel asks
  for the rows through one trait (`documents::QueryRows`) the analytics engine fills, so the kernel does not know Studio.
  Rows are read with the report path's rights for whoever runs it: a person asking gets the rows and records they may
  read; the effect (a configured action, an automation, a schedule) reads them as the workspace, as a generated
  document is rendered, its secret fields left out. Bounded: 10,000 rows and 100 files a run; a person asking for more
  is refused with the numbers, an effect's run goes on in its next job after the last record it did, never cut.
  Rows that link to no record are in no file.

**Not yet**: a statutory register built on a report document; a Word or Office file as the source (DC-33); a document generated as a Docs document;
GS1-128 (FNC1) and other symbologies; amounts in words beyond the three languages; a page's header and footer without
the PDF service; a label printer reached directly (the template is the printer's language; sending it is the device
bridge's).

## BD-22. Comments and files shared with outside people

Decided 12 November 2026. Qualifies BD-02, which kept a record's comments and files from portal people wholesale. Asked for
by six apps waiting on what outside people may see of a record: a customer following a ticket, a client their project.

* **A mark on each comment and file, off by default.** `record_comment.shared` and `file_object.shared`; nothing written
  before is shared. Staff choose (`add_comment_shared`, `PUT /api/comments/{id}/shared`, `PUT /api/files/{id}/shared`);
  what an outside person writes or uploads is shared by nature and cannot be unshared.
* **The portal role's rule says what its people see, per entity**: `shared: read` (the shared comments and files) or
  `shared: add` (and their own beside them), on a rule that reads the entity; none says nothing of it. Only on the
  records the person can read anyway: the rule widens nothing about which records.
* **Everything unmarked stays out of sight everywhere.** Listing filters on the mark in SQL; a download by an outside
  person needs the mark (or their own upload, or a field of the record as they read it naming the file, as before);
  history, tags, assignments, followers, search and exports stay the members'. An outside person never follows a record,
  and a mention in an unshared comment does not reach them, so no notice carries unshared words.
* **Told both ways.** Staff sharing a comment tells the portal people who may see it through the platform's template
  `core.comment_shared` (a workspace edits it); an outside person's comment tells the record's followers, its members.

Threats and their tests (`outside_shared_it`, two portal people of different customers and a member of staff):
enumeration (another customer's record and no record at all both answer *not found*; an outside person whose rule says
nothing is refused before the record is looked at, the same for every record); a file id guessed (an unshared file
answers as a file that is not there); leaks through notices (an unshared comment, even one naming them, tells them
nothing); writing where only reading was given.

**Rules out**: a comment visible to some outside people of a record and not others; outside people choosing what staff
see as shared; a shared mark on a file tied to no record.

**Changed**: before, an outside person could download any file tied to a record they read, knowing its id; now only one
shared with them, their own, or one a field of the record names. Attaching a file directly to a record needs, for an
outside person, write access as before or `shared: add`.

## BD-23. Signed action links

Decided 12 November 2026. Eight apps hash guest links of their own (Bookings' reschedule, Field Service's tracking, ...);
six more need one (accept a quotation, rate a ticket). One mechanism: "a person with this link may do this one thing to
this one record" (`crates/kernel/src/action_link.rs`, docs/api.md "Signed action links").

* **What a link names**: a record; one or more of the entity's actions, or `view` alone; the fields its page shows (an
  allow-list, never a secret, a table, a file or a field in a group); parameters the issuer fixed; an expiry (7 days by
  default, 90 at most); 1 to 100 uses; an optional one-time code sent to an address the issuer named (the platform's
  codes, BD-20); who it was issued to; the origins that may frame its page (BD-24).
* **Issued three ways**: the `link` effect (giving `{id, url, expires_at}`, which a later `message` sends, naming `url`
  among its `secret` values so it is kept nowhere), `Kernel::issue_link` for apps, and staff from a record
  (`POST /api/v1/{Entity}/{id}/links`). A person issues only actions they could run themselves; a configured action
  limited to roles cannot be linked (a guest holds none). Revoked by staff (`DELETE /api/links/{id}`).
* **Used as a guest bound to the link**, through the save pipeline: a context with no person, outside, its reads scoped
  to the one record, holding the named actions and nothing else; the action's own checks (condition, document status,
  `refuse_if`, the app's hooks) all apply. The visitor's parameters are the action's declared ones, typed; the issuer's
  fixed ones override theirs. The use is counted in the transaction that runs the action, after locking the link, so a
  single-use link runs once however many requests race; a refused action or a wrong code uses nothing. The record's
  history gets `link:<action>` with the link and its recipient, beside the action's own entry.
* **The page** is plain server-rendered HTML, no script, labelled and keyboard-operable forms, the workspace's name and
  colour (`brand_color`), never cached, no referrer sent; its wording comes from the templates `core.link_page`,
  `core.link_done`, `core.link_gone` and `core.link_code`, which a workspace edits.

Threats and their tests (`action_links_it`, configuration only on a custom entity): guessing (256-bit tokens, only the
SHA-256 kept, compared again in constant time after the indexed lookup); enumeration (unknown, malformed, expired,
revoked, used up, another workspace's address and an unknown workspace all answer the same `None` and the same page);
replay (a single-use link used twice, and two uses at once, run once); reaching another record or action (the guest's
scope and named actions; an unnamed action refused); overriding what the issuer fixed; a code skipped or wrong (refused,
nothing used); the token kept in a message log (sent as a secret value). Rate limits: every public request counts
against the public limiter (kept in the database, per address and workspace), the code's sends and tries are the
platform's codes' limits.

**Rules out**: a link reaching more than one record; a link running a workflow transition (not built: an action can be
configured to move the record instead); a comment added by link (not built); a token kept or shown again.

## BD-24. Frame headers per page

Decided 12 November 2026. Forms could not offer embedding: every public page said `frame-ancestors 'none'`.

* **Off by default, per page.** A published list (`frame_origins`), an action link (`frame_origins`) and an app's site
  page (its answer's `x-ferrum-frame-ancestors` header, which Forms sets per form) may name the origins that may frame
  them: `https://host[:port]` (or `http://localhost`), at most ten, no paths, no wildcards, checked when saved. Only that
  page's answer gets `Content-Security-Policy: frame-ancestors <origins>`; every other answer keeps `'none'` (and
  `X-Frame-Options: DENY`, which cannot name origins and is left out when some are named).

Threat: clickjacking of a page that acts (an action link's Accept). Tested (`controllers::links` and `controllers::lists`
unit tests, the site page's in `controllers::shell`): an allowed page's headers and a disallowed one's.

## BD-25. Public pages for published records

Decided 12 November 2026. Extends BD-15 for a help centre, a job list, a course catalogue or a status page by
configuration (docs/api.md "Published lists").

* **A slug gives each published record an address.** `slug` names a text field of the entity (held to the list's field
  rules); with it, `/public/{workspace}/lists/{name}/{value}` is that record's page, showing the list's `fields` and the
  `detail` fields only it shows (an article's body), under the same rules. The page's title is its first field, its
  description `summary`, its canonical address the workspace's (`link_base`) or the host asked. Two records with one
  value: the first in the list's order is shown; give the field a unique key.
* **Search and pages over the published fields.** `?q=` matches the list's own text fields (any case, `%` and `_` as
  characters, 100 characters at most); `?page=` turns pages of the list's `limit` rows, 100 pages at most; past the last
  is the 404 of anything not published. Both through the same query as the list, so its filter and "submitted only"
  hold.
* **Offered to search engines only when it has a slug.** Then the list page and record pages carry `index, follow`, a
  canonical link and a description, and `/public/{workspace}/sitemap.xml` lists every published record's page (10,000 at
  most). A list without a slug keeps `noindex`, as before; a search or a page past the first is never indexed.
* **Caching that follows publication**: `Cache-Control: public, max-age=60` as before, and an `ETag` of the content, so a
  record changed, unpublished or a list taken down is a new answer (or the 404) for whoever asks again; an unchanged
  page is a `304`.
* **Rate-limited** by the public limiter, kept in the database, per address and workspace, as every public read.

Threats and tests (`published_it`, `published_lists_it`, `controllers::lists`): an address naming a draft, a cancelled
or a filtered-out record, or a record of a list without a slug (all the one 404); a search reaching unpublished rows or
using `%` as a wildcard; markup in a value (escaped everywhere, the description included); the sitemap listing anything
unpublished.

**Rules out**: a page builder; content per visitor; a detail field the list's rules would refuse; Markdown rendered as
markup (a body is shown as its text).

## BD-26. An outside person's session on a site host (design only)

Decided 12 November 2026 as a design; **not built** in this slice, because doing it safely needs a session kind the
server does not have yet and a change to how every authenticated request is checked, which is wider than this slice.

**How hosts relate today.** A workspace's hosts (`tenant_host`) each serve one target: `shell` (the workspace),
`portal` (sign-in and the portal) or `site:{app}` (an app's public site at the root). The session cookie is
`__Host-ferrum` (host-only, `Path=/`, `HttpOnly`, `SameSite=Lax`, `Secure`), so a browser never sends one host's
session to another; and `ShellState::check_host` refuses any session on a `site:` host, the accounts host or the hooks
host. Changing requests are checked by `may_change` (`Sec-Fetch-Site: same-origin`, else `Origin` equal to the host).
So a site host today has no session at all, which is safe and is why apps' public sites cannot say "who is this".

**What would be safe**:

1. *A session of its own per site host.* Signing in on `shop.example` (the platform's e-mailed code, `POST
   /api/auth/code/sign-in` with the portal role the site offers, and public sign-up where the role allows it) issues a
   session row marked with the host (`session.host`), set as `__Host-ferrum-site` on that host only. `check_host`
   accepts on a `site:` host only a session whose `host` is that host, and on any other host refuses a session that has
   one: no session is shared with the staff workspace host, and a staff session never works on a site.
2. *Outside people only.* The session's context is loaded with `outside` forced on and only the portal roles the site
   offers (`tenant_host.portal_role`), so a member signing in on a shop is a customer there, nothing more.
3. *Forgery.* Every state-changing route on a site host needs `may_change` (same origin) plus, for forms, a per-session
   token in the form (`SameSite=Lax` alone lets a top-level cross-site POST through on older browsers).
4. *What apps see.* `SiteRequest.user` (today always `None` on a site) carries the context, so an app's public page asks
   "who is this" and shows records through the kernel's portal scopes, never its own lookups.
5. *Sign-out* clears the cookie and deletes the session row; the e-mailed-code limits (per address and per network
   address, kept in the database) apply as on the portal.

Threats it must be tested against when built: a staff session replayed on a site host and the reverse; a session of
one site used on another site of the same workspace; a cross-site form post; a member gaining member rights on a site;
enumeration by the code sign-in (it already answers alike).

## BD-27. Schedules by absence

Decided 12 November 2026. "Remind whoever logged no timesheet this week", "employees with no contract on file": a
schedule fires for what is missing (docs/api.md, Automations).

* **An `absent` clause on an `every` schedule**: records of the automation's entity passing its filter for which no
  record of another entity, linking to them by a named link field and passing its own filter, has a date in a period
  relative to the run: `week`, `month`, or the last `P{n}D` (1 to 366 days). Checked when saved: the entity, that the
  link field links to this entity, a date or datetime field, at most 8 conditions.
* **Once a period for each record**: the occurrence (`automation_occurrence`, whose primary key makes a moment fire
  once across replicas) is keyed by the period's start, so a daily schedule fires on the first day a record is missing
  something and not again that period. Occurrences of absence schedules are kept up to 367 days (others 30, as before).
* **Bounded like other schedules**: one SQL statement a page of 500 records, a `not exists` on the other entity in the
  same statement; the run judges the record again (it may have what it missed by then).

Tests (`absence_it`): people with no timesheet this week (an inactive one, and one who logged, left out); a daily run
across a week fires once; the next week again; one who logs between the scan and the run is not told; employees with
no contract in the last 365 days (a passport, or a contract of two years ago, does not count).

**Rules out**: absence across more than one link; a period not tied to the run (a fiscal quarter); counting (fewer than
N records) rather than absence.

## BD-28. Print formats

Decided 12 November 2026. Asked for by the owner ("print formats like ERPNext, so that people can design and customise
their prints, like invoices"); designed in `tools/PRINT-FORMATS-SPEC.md` (decisions D1-D17 there). Built in the
platform (`crates/meta/src/layout.rs`, `crates/kernel/src/layout.rs`, `crates/kernel/src/documents.rs`; the API in
[api.md](../api.md#print-formats)) for a designer in the interface.

* **A print format is a BD-21 page template, extended**, not a second mechanism: paper and margins, a letter head,
  copy labels, `enabled`, `based_on`, and a second kind, the **layout** (sections, columns, blocks as JSON). Packages ship
  standard formats; a workspace duplicates a package's layout to change it, never edits it in place.
* **One trusted renderer for layouts**, in the platform: it runs no tenant template; every string is escaped by one
  function, the only inline styles are numbers and colours the check accepted, there is no HTML block. The designer's
  canvas is this renderer's own output (with marks), so what is designed is what prints.
* **The workspace picks each entity's default** (`document_default`), a choice apart from the templates: the
  workspace's choice, else a workspace template flagged default, else a package's, else the print format; each only
  when enabled and the reader may use it.
* **Letter heads and print settings are core records** (`LetterHead`, `PrintSettings`, `Company.letter_head`); a page
  wears the one asked for, the format's, its record's company's, or the workspace's.
* **Images are the workspace's own files, inlined** as `data:` URLs (the PDF service fetches nothing), capped; drafts
  and cancelled records carry a watermark and may be refused; the PDF service is told the paper and margins.

**What it adds to BD-21's rule.** BD-21 said who reads what; print formats keep it and say it for what is new:
* The **record and its links** are read as the person printing may read them, in a layout as in a template, through
  the same loader (`record_vars`): a field group they may not read and a linked record they may not open are not
  there. A document **generated for outside** goes through the same page renderer as the workspace holds the record,
  secrets left out.
* The **company, the letter head and print settings are read as the workspace**: they are the workspace's branding,
  printed for whoever may print the record, never their secret fields.
* **An image is a file of this workspace read as its reader may**: a record's attachment as the person printing, a
  format's or the company's as the workspace, a letter head's only from that letter head's own attachments. Nothing
  else is ever fetched or emitted.
* **A secret field is refused when a format names it** (a layout's path as a template's text), and is never read when
  one renders: a stored format is checked again as it renders.

**Rules out**: tenant HTML or CSS in a layout; a designer that draws pages its own way; a remote image, another
workspace's file or an oversized one on paper; a package release overwriting a workspace's customised format; making
a package's format the default by editing it.

**Not yet**: translations of a layout's own labels (`texts`, kept, not applied); a letter head designed on the canvas
(slice 3); bulk printing from a list; receipt paper; a font image with more scripts than the PDF service's.

## BD-29. One described call, for everyone

Decided 12 November 2026. Five apps waited on it: Social posts to two networks only, CRM and Bookings cannot reach a
person's mailbox and calendar, ERP wrote its own "described HTTP call" for carriers, and Campaigns, Commerce and the
service apps need to call out and be called back. Built once (`crates/kernel/src/connections`; the API in
[api.md](../api.md#connections)).

* **A connection is configuration**: an address (`base`), how a call proves who makes it (none, a header holding a
  secret, basic, bearer, a request signed with HMAC-SHA256 over `<timestamp>.<body>` or the body, or tokens a sign-in
  gave, BD-32), default headers, the hosts besides the base's it may reach, a time limit (15 s, at most 60 s) and an
  answer size limit (1 MB, at most 10 MB), and who may use it: administrators and configuration always, people holding
  one of its `roles`, apps it lists in `apps`. Its secrets are sealed with the server's key, bound to the connection,
  never read back (the listing gives their names), moved by `reseal_secrets` with the rest. No provider is named in the
  kernel.
* **Operations are named calls**: method, a path (whose printed values are percent-encoded: an input cannot add a
  segment, a query or a host), query, headers, a JSON body template (`| json`) or a form, all templates over `inputs`,
  `secret` and `key`; `result` maps names to JSON pointers in the answer, `error_at` where the refusal is said,
  `temporary` the statuses meaning "later" (408, 425, 429, 500, 502, 503, 504 unless said), `idempotency_header` where
  the key goes, `inputs` the inputs it takes. A test connection answers each operation from its `fixture` and calls
  nobody.
* **Three ways in, one path**: `connections::call(k, ctx, app, &Call)` for apps (the mapped result; `Unavailable` for
  "later", `Invalid` for "no"); the `call` effect in buttons, transitions, approvals and automations, inline (its result
  is `results[i]` for later effects; a refusal refuses the run) or `queue: true` (a job after the run commits, retried
  only while the service says "later", eight attempts); the platform capability `ferrum.call` through the gate (an
  administrator's capability; its effect key is the call's key), so a pack's step names an operation.
* **Once per key**: an operation that changes something (every method but GET unless `changes` says) needs a key; the
  effect's is the run's own, as `invoke`'s (BD-13). A key that went through keeps its mapped result: the same key again
  gets it and calls nobody, with other inputs it is refused (`Conflict`). The service gets the same key on every attempt
  in the idempotency header, which is what makes "once" hold when our record of it is lost with a rolled-back run.
* **The guard** (`Kernel::fetch`), for every call, message channels' and token addresses' included: `https` only;
  plain `http` and private addresses only for a host the operator names (`FERRUM_CONNECTION_HOSTS`) or in development
  (`webhooks_allow_private`); only the connection's hosts; every resolved address refused when loopback, private,
  link-local (cloud metadata), shared, multicast, reserved, benchmarking or a v6 form of one; the vetted addresses
  pinned; no user name in an address; redirects followed by hand, at most three, each hop checked again in full
  (scheme, host, every address), a 303 or a POST's 301/302 made a GET without its body; the answer read up to its limit
  and refused past it.
* **Secrets stay put**: never in a template's output that is logged, an error (each is masked with every secret and
  token, and a client error never carries its URL), the audit trail (`secrets: true`), the call log, a job, an event or
  a receipt. The call log keeps when, which operation, the outcome (`ok`, `failed`, `temporary`, `refused`), the status,
  the duration and the record it was about, for 90 days; bodies only while an administrator has the debug capture on (at
  most an hour), every header that may prove who calls hidden, every secret masked, 4 KB of each body, dropped an hour
  after.
* **Message channels call through it**: a channel's described request goes through the same guard (same answers for
  what worked before; redirects within its own host are now followed and checked, an answer over 1 MB is refused).

Tests (`connections_it`): addresses that are not public (loopback by name and by address, the metadata address, a
private one, `::1`, a host the description lists that resolves to loopback) and other schemes refused; redirects to
another host and to a private one refused, one on its own host followed; a slow service is "later", a large answer
refused; header, basic, bearer and signed requests (the signature checked); a key calls once, another input under it is
refused; a path and a query cannot be steered; who may use it (a role, an app, neither); a 422 is final and says the
service's words, a 503 queued is retried and then done, a refused one is not retried; a secret echoed in an error is
masked; the capability; the log and the redacted capture; every table searched for every secret at the end.

**Rules out**: a provider named in code; a call to a private address without the operator naming its host; a retry
of an answer that is not stated as temporary; an operation that changes something without a key.

**What ERP's carriers would change**: each of `rate`, `label`, `track` and `cancel` becomes an operation of one
connection (`url` split into `base` and `path`, `rates_at`… into `result`, `error_at` and `temporary` as they are, the
`Idempotency-Key: {{ key }}` header into `idempotency_header`), the API Key and API Secret into the connection's
secrets (`secret.api_key` reads the same), `fixtures` into each operation's `fixture`, `inbound` into the connection's
inbound (BD-30) with a handler of ERP's. ERP's Rust keeps what is ERP's: making the shipment's values (`shipment`, `to`,
`from`, `rate`) into `inputs`, turning the mapped answer into rates, labels and tracking states, booking the cost. Not
done here: it is ERP's release.

## BD-30. Being called

Decided 12 November 2026. An inbound address per connection, `POST /api/connections/inbound/{id}`, no session.

* **Verified as the connection says**: `signature`, HMAC-SHA256 under one of its secrets of `<timestamp>.<raw body>`
  (the timestamp a header, within `window_secs`, 300 unless said, at most an hour), compared in constant time; or
  `token`, a secret both sides hold sent as it is in a header, compared in constant time. Mutual TLS is not offered.
* **Refused alike**: an unknown or turned-off address, an unsigned, wrongly signed or stale call, one past its size
  limit (256 KB, at most 1 MB) and a replay all get the same 401, `this call was not accepted`; each is counted (on the
  connection, `refused`; an unknown address in a process-wide counter).
* **Once**: a delivery is its caller's id (a header or a pointer into the payload), else the digest of what was signed.
  The row is written once (a second is a replay); a job processes it once, in one transaction with what it changes, and
  drops the payload. A payload that cannot be taken is marked failed with why and not tried again; a temporary fault is.
* **Onto records with no app code**: a mapping is "the one record of `entity` whose `by` fields hold these values,
  changed with `set`; none, and one is made with `by`, `set` and `create`" (unless `update_only`), each value an
  expression over `payload` and `item` (each element at `items_at`, at most 500). It is the `find` effect (BD-18): the
  record's own checks, rules and hooks apply, as the system. Checked when saved against the workspace's entities.
* **Or to an app**: `handler` names an app the connection lists in `apps`, which registered an `InboundHandler` with
  `HookRegistry::add_inbound_handler`; it runs in the same transaction.

Tests (`connections_it`, `connections_api_it`): alerts in a batch made, one changed and not made again; a replay, a
wrong signature, a stale one, an unsigned one, one too large and an unknown address answer alike and are counted; the
same body without a delivery id is a replay; a payload missing a required field fails and is not retried; a form with a
shared token goes to the app once; a handler the connection does not list is refused when saved; over HTTP the same.

**Rules out**: an inbound call processed while its caller waits; one processed twice; an unverified call.

## BD-31. Pulling

Decided 12 November 2026. `pulls` on a connection: `{name, operation, every_minutes (5 to a week), items_at, cursor:
token | time, next_at, cursor_input, max_pages (5, at most 50), map}`. The schedule scanner queues a pull when due (one
job at a time per pull); a run calls the operation (a reading one: one that changes something is refused when saved)
with the cursor as `cursor_input`, maps each item at `items_at` as BD-30 does (its own `map`, else the inbound one), and
keeps the cursor: the service's token at `next_at`, page by page, or the moment the last run that finished began. A
page's records and the cursor past it commit together. An administrator reads each pull's cursor, runs and last error,
and pulls now.

Tests (`connections_it`): two due at once and not queued twice; one page a run and the next run from the token kept;
the time cursor given as the next run's `since`; an item pulled again changed, not made twice.

## BD-32. A person authorises an account

Decided 12 November 2026. `docs/plan/integrations.md` named the broker unbuilt; Social, CRM and Bookings waited on it.

* **The provider is a description** in the connection's `auth`: `{type: oauth, owner: workspace | person, provider:
  {authorize_url, token_url, revoke_url?, scopes, client_id, client_secret? (a secret's name), client_auth: body |
  basic, params?, account_at?}}`. No provider is named in code.
* **The flow** is the authorisation code flow with PKCE (S256): `POST /api/connections/{id}/connect` from the browser,
  signed in, gives the address to send the browser to. Its `state` is 256 random bits, kept only as a digest, bound to
  the person and to their browser session, used once (a wrong try uses it up too), within ten minutes; the verifier is
  sealed. The redirect address is the platform's own and exact, `{PUBLIC_URL}/api/connections/callback`, never taken
  from the request; the callback answers a page and redirects nowhere. A provider's error is told in letters, digits and
  `_` only.
* **Whose account**: the workspace's, connected and disconnected by an administrator, or each person's own (their
  mailbox), connected by them where the connection lets them use it; every call by a person is made with their own.
* **Tokens are sealed** (bound to the connection and the person), never shown back; the listing gives the status and
  the account the provider names (`account_at`, `/id_token/<claim>` reading the id token's claims as they came from the
  token address). A job refreshes them ten minutes ahead; a call refreshes one expiring within the minute, and refreshes
  once and tries again when a service answers 401. Refreshes take a row lock, so two never race a rotating refresh
  token. A refresh the provider refuses marks the connection `needs_reconnecting` and tells its owner once (a person's
  own; the workspace's: who connected it). Disconnecting deletes the tokens here and revokes the refresh token at the
  provider when it gives a `revoke_url`.

Tests (`connections_it`, against a stand-in provider inside the test): the address the browser is sent to (the exact
redirect, S256, scopes); a tampered state, another browser session, a state used twice and another person refused; a
provider's error; the code exchanged with the verifier whose digest was the challenge and the client secret; a call with
the person's own token, none for another person; a refresh before expiry, after a 401, and by the job; a revoked
refresh token marks it and tells its owner once over two calls; reconnecting; disconnecting revokes; the workspace's
account refused to a person; a `javascript:` address refused; every table searched for the client secret, the tokens,
the states and the verifiers.

**Rules out**: an implicit or password flow; a redirect address from configuration or the request; a token shown back.

**Not yet**: a workspace on its own host signs in on the platform's public address (the session cookie must be the
one that address sees); dynamic client registration; signed JWT client authentication; device flow.

## BD-33. Connections in packages and packs, and their screens

Decided 12 November 2026. A package describes connections (`connections:` in `package.yaml`, `{name, label,
description}`); a workspace sees each offered, turned off, until an administrator saves it with its secrets (its
`source` then says the package). A pack carries the workspace's connections' descriptions, operations, mappings and
pulls, never a secret or a token, and installs each turned off under a name not taken (`source: pack:<name>`); one of
the name already there is left as it is. The admin routes (`/api/admin/connections…`) list, create, edit, delete, try an
operation deliberately, turn the capture on, read the call and inbound logs and the pulls; a person's
(`/api/connections…`) list theirs, connect and disconnect.

Tests (`connections_api_it`): a package's connection offered; a pack without the secret, installed off, again unchanged.

## BD-34. Approval before an action runs

Decided 14 November 2026. Apps with plain actions (a button's effects, an app's registered handler) could not ask for
approval before one ran; a change board waited on it. Extends BD-19.

* **The policy names the action**: `set_approval` and a package's `approvals:` take `on: {action: <name>}` beside
  `submit` and `cancel`, the same policy (steps, N of M, stand-ins, escalation, conditions, `lock`, `reset_on`,
  `system_bypass`, `message`). Refused when saved: an action the entity lacks, and a `read_only` one (nothing to agree
  to). A workflow does not stop it: it is not a transition.
* **Pressing it asks**: whoever presses must pass the action's own gates now (permission, roles, document status, its
  `when`, its refusals); the answer is `{_approval}` and nothing of the action runs. The request keeps the parameters
  as given; a secret one is kept as `[redacted]` and its value sealed apart with the server's key, bound to the request
  (`approval_request.sealed`), so neither the request's API nor the audit trail can read it, and `reseal_secrets` moves
  it with the rest. Without the server's key an action given a secret is refused rather than asked.
* **The last approval runs it once, as the requester at that moment** (BD-19's rule for submit): their context loaded,
  every gate checked again as theirs, the action given exactly what was asked (secrets unsealed), in the decision's
  transaction. Refused then (their right gone, the record no longer meeting the action's `when` or a refusal), nothing
  of it stays, the request ends `failed` with `failure` in the action's own words, secrets masked, and the requester is
  told. A rejection or a recall runs nothing.
* **Not tied to the record's state**: a transition or a submit does not end it (`approvals_moved` leaves it), and the
  record's workflow state is not compared when deciding; the action's own conditions decide at run time.

Tests (`action_approvals_it`): refused when saved; asked, the package's handler given exactly the parameters once as
the requester; the secret in no table, no request answer and no history; rejected and recalled run nothing; a record
closed meanwhile fails in the action's words and the requester is told; two approvers at the same moment run it once;
approval lends no rights; the key rotated while waiting and the action replayed under the new key alone.

**Rules out**: an action run as the system on an approval; a secret kept in the clear for an approver to see.

## BD-35. Connections in a person's data

Decided 14 November 2026. The call log, inbound deliveries, debug captures and sign-ins (BD-29 to BD-32) were outside a
person's data report and erasure.

* **What is theirs**: a call they made (`conn_call.user_id`, kept from now on), a call about a record of theirs, a call
  or a delivery whose kept text (error, mapped result, capture, payload) carries one of their addresses, written or
  percent-encoded, and their own sign-in. The report lists `{kind: call | inbound | sign_in, connection, …}` under app
  `connections`, never a token, a payload or a body.
* **Erasure**: a call's line stays (when, which operation, how it went) without who made it, its error, result, capture
  and input digest; a key's kept result goes with it, so a call again under that key answers nothing rather than their
  data. A delivery keeps its line without payload or error. Their sign-ins and sign-ins under way are deleted with their
  sealed tokens, and, once the erasure has committed, each is revoked at its provider where it gives a revoke address,
  best effort, recorded in the connection's history (`revoke_on_erasure`) and answered as `sign_ins_revoked`. Erasing
  again from the journal after a restore reaches the same rows (no revocation: it was done).

Tests (`connections_it`): reported (their sign-in, their call, the call about their record, the capture, the delivery);
erased, revoked at the stand-in provider and recorded; a restore's rows taken again by the journal.

## BD-36. A failed pull is tried again

Decided 14 November 2026. Extends BD-31.

* **"Later"** (a status the operation calls temporary, no answer): the run ends where its last page committed (the
  cursor never moves past a page not taken) and its job is tried again with the jobs' backoff (10 s, 20 s, 40 s), at most
  four attempts, each only while it would start before the pull's next scheduled run. The job's dedupe key keeps the
  schedule from queueing another meanwhile.
* **A failed run** (a refusal, a mapping that cannot be taken, or "later" past its attempts) is recorded in
  `last_error`, and the workspace's administrators are told once per streak of failed runs ("Pulling {pull} from
  {connection} failed", through the notification path); a run that works ends the streak.

Tests (`connections_it`): two "later" answers then done on the third attempt, the cursor unmoved meanwhile and nothing
queued over the retry; always "later": four attempts, told once; a 422: one attempt, told once over two runs; a run
that works, then a failure: told again.

## BD-37. Generated documents keep their type

Decided 14 November 2026. A generated HTML agreement was stored as untyped bytes.

* A generated file is stored with the type it was made as (HTML, PDF, text, CSV, XML with their charset).
* **Served** (`/files/{id}`, `/files/public/{id}`): its type (one stored without a type as `application/octet-stream`,
  as before), `X-Content-Type-Options: nosniff` and `Content-Disposition` with its name. A type a browser would run
  (HTML, XHTML, SVG, XML, script) is always an attachment and carries the sandbox the public file route already sends
  (`default-src 'none'; …; sandbox`), so it never runs on the application's own origin; uploads keep turning such types
  into bytes as before.

Tests (`documents_api_it`): each kind's type kept; HTML and XML an attachment under the sandbox, CSV an attachment, an
image inline where it was, a file without a type served as bytes.

## BD-38. Signed links: ended by an app, dated by the record, opened, with lines

Decided 14 November 2026. Extends BD-23; an app had to write `action_link` itself.

* **An app ends a record's links** in its own transaction: `Kernel::revoke_action_links(tx, ctx, entity, record,
  action)` (those naming `action`, or all), as staff revoking each would, in the record's history.
* **The `link` effect takes `until`**, an expression over the record giving a date (the link works through that day,
  UTC) or a moment; with `days` too, the sooner; never past the 90 days a link lives. One already past is refused, as a
  link with a past expiry is.
* **Opened** (`action_link.opened_at`): the first time a visitor acts on the page, its first POST-back (sending a code
  or an action, even one refused), recorded once with the record's `opened` event (`link: {id, recipient}`), which
  automations (`event: opened`) and apps' subscribers hear. Why the POST-back and not a beacon: the page runs no script,
  so a beacon would be an image, and mail scanners and link-preview services that render pages fetch images too; none of
  them submits a form. Reading the page alone is therefore not "opened".
* **A table field shown read-only**: `show` may list a table field when `columns: {field: [column]}` names 1 to 8 of
  its columns, each a field a public page may show (never a secret, a file, a table or one in a group); the page shows
  its rows (at most 200), every value escaped.
* **A link that does not work** answers the same page, status and headers whether its workspace exists or not: the
  wording is the platform's `core.link_gone` as shipped, never a workspace's edit, so a stranger learns nothing of which
  workspaces exist.

Tests (`action_links_it`, links' unit tests): ended by the app's call, the other kept, then all; an outside person ends
none; `until` from the record's valid-until, a past one refused, the rules of `until` (a unit test); the table shown by
its columns, five bad `columns` refused; a GET opens nothing, two POST-backs open it once, an automation on `opened`
hears it; the gone wording.

## BD-39. Faults a walk of a live server found

Decided 14 November 2026 (walk of 12 November, `RESULTS-APPS-3.md` F1 and F3 to F7).

* **A failed backfill runs again**: `POST /api/admin/backfills/{package}/{name}/rerun` queues one whose newest job
  failed at once, from where it got (a finished one still runs from the start at the next upkeep; one running is still
  refused). A backfill whose newest job failed is queued again by the upkeep once when its package's installed version
  differs from the one it was last queued under (`backfill_run.version`): the release that fixes it. Failing again under
  that version, it waits for an administrator. A job queued since a failure makes it no longer `failed`.
* **The published list's JSON** takes `?q=` and `?page=` as its page does and says `page` (always) and `q`; a page past
  the last is a `404`.
* **An outside person's upload** is shared by nature: stored so by every upload route, and listed so whatever its flag.
* **A link that does not work** answers alike whether or not its workspace exists (BD-38).
* **A letter head asked for by id** that is not there or not in use is refused (`422` on `letterhead`) instead of printing
  without it.
* **An expected `503`** is logged once, at WARN where it is answered; the request log's ERROR is for other failures.

Tests: `backfill_it`, `published_lists_it`, `outside_shared_it`, `print_it`, `request_log`'s unit test.

## BD-40. A rule that does not show some fields

Decided 13 November 2026. Field groups hide a field from every role not naming the group, so hiding one field from one
portal role meant re-granting every other role, and an app could not group a field of an entity it does not own
(Projects: a customer's portal person read a shared project's internal `notes` and its team).

* **`hide: [field, ...]` on a permission rule** (`PermissionDef.hide`): fields and child tables of the entity the rule
  neither shows nor accepts. Checked when saved: fields of the entity only; never the title field, a field of a unique
  key, a field of the rule's own `where` or the first step of its `via` (what names, keys or reaches the record); never
  on a rule for every entity.
* **Per record on what reads one record at a time**: a field is left out of a record read (`get`, a list's rows,
  `fields=`, record pages, prints and templates rendered as the person) when every rule of theirs reaching that record
  hides it; a rule reaching the same record that shows it brings it back (`ctx::record_hidden`).
* **Per entity on what reads a set**: a field some rule hides and no rule reading every record shows is unreadable for
  filters and sorting (refused, as a field group's is, so matching cannot leak it), search, reports, exports, history
  diffs and the field's description. Why not per record there: a filter or a sort over a set would tell the hidden
  values of the records reached only through the hiding rule. A rule reaching every record (staff) that shows the field
  keeps it everywhere.
* **Never written** by whoever holds a rule hiding it (unless a rule reaching every record shows it): a write naming it
  is refused, whatever its value (an "unchanged" pass would tell the value).
* **Packages declare it** in their roles. **A package's rule rows follow its releases**: a row its package wrote is
  marked `packaged` and rewritten by each install; an administrator's edit through the role editor makes the row theirs
  (`packaged = false`), never rewritten. Rows written before this release (whose they are is unknown) take only `hide`
  from the package, while theirs is empty, since nobody could have set it. Before, package rule rows were inserted
  `on conflict do nothing`: a corrected rule never reached an earlier workspace.
* **A packaged row follows its own package only** (corrected 13 November 2026). The row keeps the package that wrote
  it (`role_permission.package`): a release rewrites the row only when that package wrote it. Two packages declaring a
  rule for the same role and entity (several apps add rules to shared roles) leave the first row as it is, as before
  this decision: as first shipped, each overwrote the other's in install order, and CRM's `Lead.convert_to_customer`
  lost its grant on Address. A row written as packaged by the two releases before the correction carries no package:
  its origin is unknown, so no package claims it (its grants stay; its `hide` is taken only while empty, as a row from
  before this decision). Another package's row never takes this package's `hide`.

Tests (`packaged_rules_it`: two packages' rules for one role and entity in both install orders, the first kept and
the second changing nothing; the first package's corrected grant and `hide` reaching the workspace; a row of unknown
origin claimed by neither; an administrator's edit kept through both). (`rule_hide_it`): refused when saved (five ways); a portal person's three projects (hidden only through the
client's rule, shown through the sponsor's, shown on the one both reach) through read, list, `fields=`, filter, sort,
search, a print and writes; staff hiding it everywhere, history included, and staff also holding a showing rule; a
package's rule following a release, a row from before taking `hide`, an administrator's edit kept.

**Rules out**: a hidden field's value reaching anyone through a filter, a sort or a search; a package release
overwriting an administrator's rule. **Not yet**: hiding a column of a child table only; per-record answers for sets.

## BD-41. Service targets by record

Decided 13 November 2026. Extends BD-04. A target named one calendar and started at the save that started it: Helpdesk
dropped per-agreement hours and could not backdate an imported ticket; ITSM the same.

* **`calendar_field`**: where each record names its calendar, a data or select field of the record, or a link and such
  a field of the record it links to (`agreement.calendar`, read as the workspace, as `links` is). Empty there, the
  target's `calendar`; neither, plain time. A name the workspace has no calendar for refuses the save, as a missing
  calendar did.
* **`start_field`**: a date-time field the clock starts from, so the due moment is the open time after it; empty or
  still to come, the clock starts at the save, as before. A start already past its due moment is due at once and the
  sweep marks the breach once.
* **Worked out again once** (`ferrum_logic::target::reanchor`): each round keeps the calendar and start it was worked
  out from (`record_target.calendar`, `start_from`). When either changes on an open round, the open time already
  counted stays as the calendar it ran under counted it, what is left counts on the new calendar from now, and a start
  moved adds or takes away the open time between the two moments; pauses and a breach already recorded stay. Why not
  recount everything on the new calendar: a round keeps no list of its pauses, and an agreement changed mid-flight does
  not rewrite the hours already worked. Rounds from before this release record their anchors at their next save.

Tests (`targets_it` `by_record`, `target` unit tests): a start on a Friday in summer time due on the Monday after the
clocks went back; no agreement falls back to the target's calendar; a start still to come; a calendar swapped two hours
in, then a save that changes nothing; a start corrected; a start long past, breached once and still breached after a
swap.

## BD-42. Public pages count

Decided 13 November 2026. Extends BD-25. A help centre's article page could not count a view or a "was this helpful"
vote from a visitor who is not signed in.

* **Declared on the published list**: `count_views: <field>` and `votes: {up: <field>, down: <field>}`, whole-number
  fields of the record (not worked out, in no field group), on a list with a slug. The platform keeps them: what a
  person, an import or an API call sends for them is ignored, as for a total.
* **A view is the page's own POST-back**: the record page carries one script, allowed by its digest in the page's
  policy, posting `do=view` to the page's address; a fetcher or a scanner reading the page counts nothing. A vote is a
  form posting `do=up` or `do=down`. Only from the page's own origin (`Sec-Fetch-Site`, else `Origin`).
* **Once per visitor**: a visitor is a first-party cookie holding a random value (`ferrum_visitor`, set at their first
  post). Kept is only an HMAC under the workspace's own random key: with the day for a view (once a visitor a record a
  day; one day's digest cannot be matched to another's; kept two days), with the record for a vote (once a visitor a
  record; a different answer moves the vote).
* **An address's budget**: 60 counted posts an address an hour a workspace (`MAX_COUNTS_PER_HOUR`), the address kept as
  a keyed digest with its hour for two hours; past it, `429` and nothing counted; the public limiter as every public
  request.
* **No new version, no audit entry, no `updated` event**: the counter is moved by one statement, never below 0.

Tests (`published_lists_it` `counts`, `published` unit tests): the page's beacon and its policy; two GETs count nothing;
a visitor counted once, a second visitor counted, another site and an unknown value refused; votes once and changeable;
version and history unchanged; a counter sent by a person ignored; the next day counted again under an unlinkable
digest, no cookie value kept; a flood from one address counted to the budget; an unpublished record not counted.

## BD-43. Calls apps were missing

Decided 13 November 2026. Helpdesk wrote `record_tag`, `logic_calendar` and `file_object` itself.

* `Kernel::tag_tx(ctx, tx, entity, id, add, remove)`: as the tags route asks (a member who may change the entity and
  reads the record; the system), tags normalised the same way, at most 20, once each whatever their case.
* `Kernel::ensure_calendar_tx(ctx, tx, calendar)`: makes a working calendar of that name when the workspace has none,
  through the calendars route's checks; one there is left as it is (an administrator's edit stands). Administrators and
  the system.
* `Kernel::attach_files_tx(ctx, tx, ids, entity, record)`: the files going out with or kept on a record (a reply's
  attachments): each already on that record, or a loose upload of the caller's (the system's: any loose one), the loose
  ones attached; asked as attaching asks (whoever may change the record; an outside person whose rule lets them add,
  their uploads shared). A file not there, not theirs or another record's refuses the call, nothing attached.
  (`may_attach_tx` is the check in the caller's transaction.)
* The capability catalog lists `opened` among each entity's event kinds (BD-38).

Tests (`app_calls_it`, `capability_it`).

## BD-44. Secrets in every form, files and own words on a link's page

Decided 13 November 2026. Commerce 0.9.0 hit each.

* **A secret value is masked in every form it can be printed in**, where rendered text is stored or logged (the
  outbox's mail, a send's log, a channel's request log): as it is, HTML-escaped, percent-encoded, as JSON, and drawn by
  `qr` and `barcode` (their defaults) (`messages::encodings`), longest first. Before, only the plain form was masked: a
  token in a link's address or an attribute stayed readable. What is sealed is blanked once handed over, as before.
* **A link's action may take files**: a parameter `{"type": "files", "max_files": 1-10, "max_bytes": ≤ 10 MB,
  "accept": ["image/*", "application/pdf"]}` (one file of 10 MB without them). The page's form posts them as
  multipart (25 MB a post at most); each is held to its parameter, scanned and stored as an outside person's upload
  (shared), attached to the record in the use's transaction and given to the action as its ids; a refused action or a
  link not working discards them (rows and bytes). Files are never named by id by a visitor.
* **An action's own refusal words**: `condition_message` on an action, shown when its `condition` does not hold
  (to a link's visitor on the page, and to anyone through the API), rendered as a rule's message is (BD-16) over the
  record as the presser may read it.

Tests (`messages` unit test: a link carrying a secret token in an address, an attribute, text, JSON, a QR code and a
barcode; `action_links_it` `uploads`: none, too many, too large, the wrong type and a field that takes no files
refused with nothing stored; two attached and shared; after acceptance the action's own words and the upload gone).

**Not yet**: a barcode drawn with other options than its defaults is not among the masked forms (print a secret with
`barcode(v)` alone); a link-level refusal message (the action's serves every link to it).

## BD-45. Quorums from the record, absent parameters, targets on every write, rendering for another transport, and the App API 0.3.4

Decided 13 November 2026. ITSM 0.4.0 and CRM 0.13.0 hit each.

* **An approval step's `need` may be an expression** over `doc` and `settings` (`"need": "doc.quorum"`), compiled
  when saved, worked out when the request is made and kept on it (a later change of the record does not move it),
  clamped to at least one and to the approvers found when the step opens. A reset that voids the decisions works it
  out again. A number keeps refusing a step with fewer approvers than it asks; an expression asks for all there are.
* **An optional action parameter not given stays absent** in the action's effects and `refuse_if`: ask with
  `has(params.x)`, as packages already do (`when: "has(params.note)"`). Filling absent ones with null was tried and
  taken back the same day: it made `has(params.x)` true for every declared parameter, so an effect guarded by it
  would have cleared the field it sets. `coalesce(has(params.x) ? params.x : null, ...)` gives a default.
* **Service targets follow every engine write**: a save (create, update, submit, cancel, a transition, an action's
  effects, an import), the breach sweep, and now `set_values_tx`, `set_child_values_tx` and `set_child_rows_tx`, which
  work the record's targets out again and write their two fields as they write (no version, no event). Not covered:
  an app's own SQL on an entity's table, and the published counters (BD-42), which no target should watch.
* **Rendering for another transport**: `messages::render_for(k, tx, ctx, &Message, channel)` renders one recipient's
  message exactly as `send` would (template, language, record, settings, values) and sends and records nothing: its
  `subject` and `html`, its `text`, and for a promotional message the opt-out address and the `List-Unsubscribe` /
  `List-Unsubscribe-Post` values; refused (`403`, `opted out`) for an address that opted out, an erased person's
  digest included, or whose processing is restricted. `Kernel::opted_out(tx, tenant, channel, address)` asks the same.
* **The App API is 0.3.4** (`ferrum_apps::PLATFORM_VERSION`); an app using any of the following says
  `platform: ">=0.3.4, <0.4"`. 0.3.3 (print formats) carried the print fields; 0.3.4 carries: from PR 214,
  `Kernel::revoke_action_links`, the `link` effect's `until` and `columns`, the `opened` event and `on: {action}`
  approvals; from this slice, a rule's `hide`, a target's `calendar_field` and `start_field`, a list's `count_views`
  and `votes`, `tag_tx`, `ensure_calendar_tx`, `attach_files_tx`, `use_link_with` and `files` parameters,
  `condition_message`, an expression `need`, `render_for` and `opted_out`.

Tests: `approvals_it` (a quorum of two kept when the record changes to one, nine clamped to three, none to one, a
`need` that does not compile refused), `actions_it` `absent_params`, `targets_it` `engine_writes` (a paused ticket
resumed by `set_values_tx`), `messages_it` (an erased opt-out refuses the render on both channels; a promotional
render's link and headers; none on an operational one).

## BD-46. Answers beyond a JSON body

Decided 13 November 2026. Four apps moved onto connections (BD-29) and listed what a description could not say; Social
and CRM hit these.

* **A value read from an answer is a pick**: an operation's `result` value is a JSON pointer, as before, or an object
  (`{at}`, `{header: "Location"}`, `{link: "next"}` for the target of the `Link` header entry whose `rel` holds that
  word, made absolute against the answer's address, or `{expr}`), with `required: true` for one that must be there.
  A pointer whose key is not there reads as null; a required one that reads null fails the call (`the answer has no
  <name>`). An expression is the platform's (sums, text joins, `coalesce`) over `answer`, `headers` (lower-case names),
  `inputs` and `connection` (`{name, id}`); never `secret`.
* **In a mapping too** (inbound and pulls): a value is a pointer into the item (`/a/b`, null when not there), an
  expression as before (a string that is not a pointer, which still fails the item on a missing key, as CEL does),
  or `{at | expr, required}`; `connection` is bound. Why read the values first and hand the `find` effect only what
  they gave (`picked`): one evaluation, and `required` judged before anything is written.
* **Why the object is kept as text in `result`**: ERP builds `Operation` in code, so its fields keep their types; an
  object value is stored as `connections::SPEC` and its JSON (`connections::spec(&json!(…))` from Rust) and written
  back as the object. A pointer never starts with that character.
* **A pull pages by `next_at`**, now a pick: a pointer, a header, or `{link: "next"}`, whose address is asked as it is
  for the next page (the operation's headers and credentials, the guard checking its host like any address).
* **Retry-After**: a temporary answer's `Retry-After` (seconds or an HTTP date; at least 1 s, at most an hour) sets when
  a queued call's job and a pull's retry come back instead of the backoff, each still an attempt
  (`Error::RetryAfter`, a 503 like `Unavailable`). An app's own `connections::call` still sees `Unavailable`.

Tests (`connections_it` `answers`, unit tests): a post's id and address from headers, a sum and the connection's name
from expressions, a missing pointer null, a required one failing; an answer's expression cannot read a secret; a 429
with `Retry-After: 120` queued again ~120 s on; a feed paged by its `Link` header, and one whose `Link` names another
host refused by the guard; an item without its required key failing the payload, an optional one null; links and
`Retry-After` dates parsed and capped.

## BD-47. Bodies beyond JSON

Decided 13 November 2026. Social uploads media; ERP fetched carriers' labels from addresses in answers outside
connections.

* **A stored file sent**: a `form` value may be `{file, name?, content_type?, max_bytes?}` (templates giving the file's
  id and its name), which makes the form `multipart/form-data` (as does a `content-type: multipart/form-data` header);
  `body` may be the same object, the file then the whole body with its type. The caller must be able to read the file
  as downloading asks (a queued call reads as the system); each file at most its `max_bytes` (10 MB, at most 25 MB) and
  one request's files 25 MB in all; checked before anything is sent. Names in a part are the description's; a file
  name's quotes and line breaks are percent-encoded and a type's control characters dropped, so no part adds a header.
  Not streamed: the object store's interface reads a file whole (`FileStore::get`); the limits keep that small.
* **A file an answer gives**: a `result` value `{file: {from?, name?, max_bytes?}}`. Without `from` the answer itself is
  the file (the operation's answer limit is then the file's); with `from`, the address at that pointer is fetched
  through the guard, within the connection's own hosts, without its credentials (a signed address needs none; sending
  them to an address an answer chose is what this refuses). Stored as uploads are (scanned, counted against the
  workspace's storage), attached to the record the call names, the value its id. A test connection fetches nothing.

Tests (`connections_it` `files`, unit test): a multipart form with a caption and a photo, a photo as the whole body; a
file over the operation's limit and another person's upload refused with nothing sent; a label fetched from the
address in the answer, stored, attached to the parcel, no credentials sent; an address off the connection's hosts and
a file over its limit refused; an image answer stored; a part's name and type cannot break their lines.

## BD-48. Sign-in that is not the authorisation-code flow

Decided 13 November 2026. Social's networks with app-password sessions; the broker's proof-of-possession and
registration.

* **`auth: session`**: `{login, refresh?, token_at, refresh_at?, expires_at?, header?, prefix?}`. The `login`
  operation (templates over the connection's `secret`) gives a token at `token_at`; `refresh` (given
  `inputs.refresh_token`) renews it, else the login is made again. The expiry is seconds from now, a Unix time (past
  10^9) or a moment. The workspace's account (`conn_token`, nil user), sealed, refreshed a minute ahead by a call and
  ten minutes ahead by the job, once again after a 401, under a lock per connection so that two calls never both log
  in. A login or a refresh the service refuses marks it `needs_reconnecting` and tells the administrators once (who
  connected it, else every administrator): a wrong password is tried once, not on every call (an account lockout).
  Saving or merging its secrets, or `POST /api/admin/connections/{id}/session`, signs in afresh.
* **The login and refresh are never operations**: an app, a person, an effect, the gate, a pull and "try this" are
  refused them (their answer is a token), they are made without credentials of their own and never captured.
* **DPoP (RFC 9449)**: `provider.dpop: true` binds tokens to an ES256 key pair of the connection's own, made with
  `ring` (already the workspace's) and sealed among its secrets (`_dpop_key`, so `reseal_secrets` moves it): a proof on
  the token request and on every call (`Authorization: DPoP`, `ath` the token's digest), the nonce a server asks for
  (`use_dpop_nonce`, at the token address a 400, at a service a 401) kept per connection and server and the request
  made once more. Not done: `dpop_jkt` on the authorisation request, and pushed authorisation requests.
* **Dynamic client registration (RFC 7591)**: `provider.registration_url` (its metadata's `registration_endpoint`; the
  platform does not fetch discovery documents) with no `client_id`: the first sign-in registers the platform's client
  (the exact callback, `client_secret_basic`, the scopes, `dpop_bound_access_tokens`), once, under the connection's
  lock, and keeps it sealed (`_client_id`, `_client_secret`). Names starting with `_` are the platform's: never listed,
  set or dropped by an administrator.

Tests (`connections_it` `sessions`): signed in on first use, once; the login and refresh refused as operations, to
"try" and to a pull; refreshed by its operation when expiring and after a 401; a refused refresh telling the
administrators once over two calls without another login; reconnected; a wrong password tried once over two calls; a
provider that registers its client (once, with the exact callback) and binds tokens: the token request asked again
with its nonce, a call's proof under the same key with the token's digest and the service's nonce; the client secret,
tokens and password at rest nowhere.

## BD-49. Inbound without a timestamp

Decided 13 November 2026. ERP's carriers sign the raw body only, so their tracking stopped arriving.

* `verify: signature` without `timestamp_header` signs the raw body alone, and is saved only when the inbound names the
  caller's delivery id (`delivery_header` or `delivery_at`). The replay guard is the delivery id and the digest of the
  body, each taken once while the delivery is kept (90 days once processed): a header is not signed, so a captured body
  sent again under a new delivery id is a replay too. Refusals are the same 401 as every other.

Tests (`connections_it`, unit tests): refused when saved without a delivery id; a body sent again under a new id, an id
again with another body and a wrong signature refused alike and counted; a new delivery taken; a signature over a
timestamp not taken for one over the body.

## BD-50. Operating a connection

Decided 13 November 2026. ERP, Social and CRM.

* **Secrets merged**: `PATCH /api/admin/connections/{id}/secrets {set, remove}` sets or replaces the named secrets and
  removes others, keeping the rest; the audit trail names them (`secrets: {set, removed}`), never their values.
* **`meta`** on a description: an object (16 KB at most) kept and given back, never read by the platform.
* **A pull on a person's own connection** (`owner: person`) runs as each person connected: one row, cursor and failure
  streak each (`conn_pull.user_id`), made when they connect, run with their sign-in and their rights for the mapping
  (the `find` effect as them). Their account closed: their pulls go. Their sign-in needing reconnecting: the pull waits
  (the broker told them once), and connecting again runs it at once. A failed run tells them, not the administrators.
* **Fetched from outside**: a mapping's `outside: true` writes as a context whose `origin` is `outside`
  (`Ctx::origin`). The change's events carry `origin`; an automation's condition and effects read `origin` (empty for
  any other change). What apps must check: an automation or effect a package declares for new records that reaches
  people (invitations, notifications) adds `origin != 'outside'` to its condition; an app's hook reads `ctx.origin`;
  an app's event subscriber reads the event's `origin`. The platform cannot tell an outbound effect from another, so
  it does not skip any by itself.

Tests (`connections_it` `operating`): a secret added and removed by name, the rest kept, refused to a non-administrator,
audited without its value; `meta` kept, a text and an oversized one refused; nothing pulled before anyone connects; a
person's pull making a record as her, passed by an automation for new records that greets one made here; waiting while
her sign-in needs reconnecting with nothing more told, running again once she connects; gone when her account closes.

## BD-51. The connector hub's instances move, and the App API 0.3.5

Decided 13 November 2026.

* **Two controllers mounted `/api/admin/connections`**: the older connector hub (`connect.rs`, docs/connectors.md) and
  connections (BD-29). axum panics on a path and method mounted twice, so a server built from main did not start. The
  hub's instance routes move under `/api/admin/connector-instances` (`GET`/`POST`, `/{id}` `PUT`/`DELETE`, `/{id}/test`,
  `/{id}/schema`, `/{id}/sync`, `/{id}/reconcile`, `/{id}/drift`, `/{id}/activity`); `/api/admin/connectors`,
  `/api/admin/connection-drift/{id}/resolve` and `/api/connect/{id}/webhook` keep theirs. `routes_it` builds the whole
  application's router as the binary does and lists each controller's paths once.
* **The App API is 0.3.5**; an app using any of this says `platform: ">=0.3.5, <0.4"`. 0.3.5 carries: result and
  mapping picks (`header`, `link`, `expr`, `required`, null for a missing pointer, `connections::spec`), `next_at` as a
  pick, `Retry-After` and `Error::RetryAfter`; file parts, file bodies and file results; `auth: session`,
  `provider.dpop`, `provider.registration_url` and `connect_session`; `verify: signature` without a timestamp;
  `merge_connection_secrets`, `meta`, per-person pulls, a mapping's `outside` and `Ctx::origin`.

## BD-52. A link limited by another field of the record, and the App API 0.3.6

Decided 13 November 2026.

* **The fault**: a link's `link_filters` was a fixed equality map (`{is_group: false}`). Nothing let a link limit its
  choices by another field of the same record, so Commerce's Store Settings listed every company's warehouses and then
  refused the save, and each app wrote that refusal in Rust.
* **The shape**: a filter's value is a literal, or `{from: <field>}` (the linked record's field equals that field of the
  record being saved), or, in a line's field, `{from: parent.<field>}` (of the record or line it is in):
  `link_filters: {company: {from: company}}` on a document's `warehouse`, `{company: {from: parent.company}}` on a line's.
* **Checked when declared** (a package, a customization, a pack; `catalog::validate_link_from`): the named field
  exists on the record, or on every entity whose table holds the line; the filtered field exists on the linked entity;
  the two hold the same thing (links to the same entity, or the same plain type); `parent.` outside a line is refused.
* **Checked on write** wherever fixed filters are (`doc.rs` `check_links`): every insert, update and submit, so imports,
  effects, automations and a line's rows too, on every save, so changing the source field checks again the links
  filtered by it. An empty source field applies no filter (the field's own `required` decides). A mismatch is refused
  on the link field in plain words: "Warehouse Stores belongs to Company A; this record is for Company B" ("this line"
  in a row; "has <field> …" for a plain value). Engine writes (`set_values_tx`, `set_child_values_tx`) check no links,
  as before.
* **Pickers**: `/api/meta/{Entity}` carries the filter as declared. A picker resolves `from` against the form's values
  (the parent record's for a line) and sends an ordinary equality filter on the linked entity's field,
  `filters={"company": "<id>"}` (or `[["company","=","<id>"]]`) on `GET /api/v1/{Target}`; with the source empty, it sends
  none for that key.
* **The App API is 0.3.6**; an app using `from` says `platform: ">=0.3.6, <0.4"`. 0.3.6 carries `{from: …}` in
  `link_filters`.

## BD-53. An operation of steps

Decided 13 November 2026. Social needs uploads registered before their bytes, containers before publishing, a reply
that names the post it answers: two or three requests, a later one reading an earlier answer
(docs/references/social.md).

* **A step is another operation** of the same connection, called with inputs that are expressions over `inputs`,
  `steps` and `key`. Why not requests written inside the step: everything an operation already says (files, picks,
  `error_at`, `temporary`, `idempotency_header`) works for a step unchanged, and each step can be tried on its own.
  Why expressions and not templates: a step passes values (an object, a list, a number) to the next, which a template
  would turn to text. The expressions are minijinja's, the templates' own language, so the template functions (BD-56)
  work in both.
* **Kept per key**: each step is a call under `<key>:<step>` whose result is kept with the call log's entry, masked
  like every result, and the operation's own result under its key. A step that finished is never made again under
  that key. Why the call log and not a new store: it already keeps a keyed call for good (`conn_call_once`), and its
  90-day upkeep spares those.
* **A lost answer** (`conn_attempt.sending` still set): the request may have been delivered. Clear when the service
  answered 4xx or 503 (it did nothing) or the call was never sent. Otherwise the step is sent again only with an
  `idempotency_header` (the service takes the key once) or after its `confirm` read-back does not prove it; with
  neither, the call fails and says so. Why fail rather than guess: publishing twice is the fault Postiz needed twelve
  workflow versions to remove (`postiz.md` 3.6-3.7); a person can look, and a new key starts again.
* **One worker per key**: every keyed call that changes something takes `pg_try_advisory_xact_lock` on its key for
  its length; another worker is told `RetryAfter` 5 s, and then finds the stored answer. The lock holds one database
  connection while the call runs (ponytail: one connection per call in flight; a lease row if pools run short).
* **An address from an answer**: `url` in place of `path` (a template, not percent-encoded), checked by the guard
  against the base's host and `allowed_hosts` like a redirect; `credentials: false` for an address signed already
  (an S3 form refuses a second authentication); `form_fields` for a signed form's fields, in their order.
* Bounded: at most 12 steps; a step's operation has no steps of its own; pulls take an operation of one request.
* **Kept beside `Operation`, written on it**: ERP builds `connections::Operation` in code with every field named, so a
  new field would stop it compiling. The keys this part adds to an operation (`steps`, `url`, `success`,
  `credentials`, `form_fields`, `answer`, `lists`) are `connections::More`, kept in `Description::more` by operation
  name: a description reads and writes them on the operation like any other key (unknown keys still refused), and an
  app's `Operation { … }` is unchanged. (`Description` is built with `..Default::default()`, so its new fields are
  safe.)

Tests (`connections_steps_it` `steps`): register, put the bytes to the address the answer gave, post; the post
answered "later" once and resumed without the register or the upload again; the same key answered from the log, other
inputs refused; each step's answer kept, the token resting nowhere; a lost answer not sent again, proved by reading
back, sent again under its own key with an idempotency header; an address off the connection's hosts refused; two
workers on one key, one sending; wrong steps refused when saved.

## BD-54. A step that waits

Decided 13 November 2026. Containers, videos and uploads are processed before they can be published (Instagram,
Threads, TikTok, YouTube, Mastodon, LinkedIn): the publish waits for a status.

* `wait: {until, fail_if?, every_secs (5), within_secs (600), attempts (1000)}` on a step whose operation only reads.
  Each ask is logged; one the service answers "later" counts as not ready. `fail_if` stops the call with the service's
  words; `within_secs` is counted from the key's first attempt (its clock, BD-56), so a wait carried over many calls
  is still bounded; the asks are counted in the key's attempt.
* **Patience**: a call waits in place at most 20 s, then answers "later" after `every_secs`; for a queued call or an
  app's call that is `RetryAfter`, so the job comes back when the service should be asked again rather than holding a
  worker for minutes. Why not sleep inside the job: a worker held for a ten-minute transcode is a worker no other
  workspace gets.
* Once ready, the step's result is kept under its key like any step's, so the next call goes straight on.

Tests (`connections_steps_it` `waiting`): a container processed after three asks, published once; one that fails says
the service's status and publishes nothing; past its patience the call answers "later" after its own delay and the
next calls carry on from the wait without a second container; a wait bounded in time; a wait on an operation that
changes something refused.

## BD-55. Loops over items and chunks

Decided 13 November 2026. Several pictures, threads, communities; videos in chunks (YouTube's resumable 8 MB,
X's 1 MB APPEND, TikTok's chunks with the remainder in the last, LinkedIn's parts the service names).

* **`for_each`**: a step made for each item of a list, in order, each its own call under `<key>:<step>:<index>`; a
  retry goes on at the first item without a result. `previous` and `results` let each item answer the one before (a
  thread). `range: {file, offset, size}` sends that part of a stored file for each item (parts whose addresses and byte
  ranges the service gives). At most 100 items.
* **`chunks`**: a stored file sent `bytes` at a time; `chunk` gives the index, offsets, size, total, count and whether
  it is the last, so headers like `Content-Range` and commands like `segment_index` are templates. Each chunk is a call
  under `<key>:<step>@<offset>`, and the loop's place is kept, so a retry goes on at the first chunk not sent.
  `remainder: last` puts what is left over in the last chunk (TikTok's rule) instead of a short chunk of its own.
* **Streamed from storage**: `FileStore::get_range` reads one chunk's bytes; the server's store reads that range from
  disk or S3 (OpenDAL), so a 500 MB video is never held whole; a store that cannot read a range reads the whole and
  keeps the range (the default, tests). At most 25 MB a chunk; the file at most `FERRUM_CONNECTION_UPLOAD_MB` (25 MB
  unless the operator raises it, since a chunked file is never held whole); at most 10,000 chunks.
* **A lost chunk** is settled, in this order: by its operation's `idempotency_header`; by `offset_from`, a reading
  operation whose `offset` says how many bytes the service holds (a number, or `bytes=0-<last>` as a `Range` header
  says; no offset with a 2xx means the upload is complete and that answer is the result), and the loop goes on from
  there (less than was sent before is refused: the earlier chunks are not sent twice); by `restart_from`, which starts
  that upload again from the step it names (its first command), the steps from there under new keys (`<step>~<n>`, at
  most twice), so the old, broken upload is left and nothing published; otherwise the call stops and says so.
* **`success`**: statuses besides 2xx that mean it worked (308, "resume incomplete").

Tests (`connections_steps_it` `loops`): a thread of three, the second answered "later" once, resumed without the first
again, each answering the one before; a resumable upload whose second chunk's answer is lost, resumed from the
service's offset, each byte sent once and in order, read only as ranges; INIT/APPEND/FINALIZE whose second APPEND's
answer is lost, started again from INIT under new keys, finalised once; parts the service names, each its range,
their ETags collected for the finish; a file over the operator's cap; loops that cannot be refused when saved.

## BD-56. Template functions and the clock

Decided 13 November 2026. A record key made from the delivery's key (so a lost answer cannot post twice), rich-text
facets counted in bytes, lengths counted as a service counts them, a creation moment that a retry repeats.

* **The clock is the key's**: `now` and `today` are the moment of the key's first attempt (`conn_attempt.clock`), for
  an operation of steps and for a single keyed call alike, so a retry sends the same `createdAt` and the same record
  key. A call without a key (a read) has the moment it is made. Answers' and mappings' expressions bind `now` and
  `today` as the moment they run (BD-17 keeps the clock out of the expression language itself: it is a value given to
  it, as `user.now` is).
* **Filters, not a language of our own**: minijinja's filters, in the connections' environment only (print formats are
  unchanged): `sha256`, `hmac`, `base64`, `base64url`, `base32`, `urlencode`, `tid`, `length_in`, `entities`,
  `truncate`, `strftime`. `tid` is 53 bits of microseconds and 10 bits of a digest of the key in the sortable base32
  alphabet, 13 characters: the same key gives the same id, a later first attempt a larger one. `entities` finds links
  (`http(s)://…`, trailing punctuation left out), mentions (`@handle`, `@user@host`) and tags (`#word`, not a bare
  number), never inside a link, with offsets in bytes, characters, UTF-16 units or graphemes. A mention's account id
  (a service's own lookup) is a `for_each` step over the mentions, not a function.
* Not done: a weighted length (a service that counts some characters double and every link as 23): its weights are
  that service's rules, which a description cannot name yet; `length_in` and `entities` give the parts.

Tests (`connections_steps_it` `functions`, unit tests): a record's key, moment and day the same on a retry; a later
key sorting after; facets' byte offsets around an emoji with a tone; a grapheme count; a signature in hex that does not
send its key; truncate, split, join, default, urlencode, strftime; a pull's mapping reading `today`; RFC 4231's HMAC
vector; offsets in each unit.

## BD-57. Retry-After for every caller, and a description's limits

Decided 13 November 2026. Social's publish job saw only "later" and fell back on the jobs' backoff; Postiz caps each
network's concurrent jobs (`maxConcurrentJob`), and networks count requests per window.

* **`RetryAfter` reaches `connections::call`** (and the `call` effect inline, and the gate): a temporary answer with
  `Retry-After`, a wait past its patience (BD-54), a key another worker holds (BD-53) and a limit below are
  `Error::RetryAfter(why, seconds)`, a 503 like `Unavailable`; without a time it stays `Unavailable`. What apps must do:
  treat `RetryAfter` as `Unavailable` that says when (Social's job matched only `Unavailable` and `Wait`). This is why
  the App API moves (BD-62).
* **`limit: {per, every, concurrent}`** on a description. The window is counted in `conn_rate` (one row per
  connection and window, an atomic upsert), so every worker and replica shares it; a request past `per` is not sent and
  answers `RetryAfter` until the next window. `concurrent` places are advisory locks (`conn_turn:<connection>:<n>`)
  held for the request; one that finds none free waits up to 10 s, then answers `RetryAfter` 1 s. Every request
  counts: steps, chunks, waits' asks, pulls' pages, sign-in operations (those run before the request that needed them
  takes its place, so a session's login never waits on itself).
* Why per connection, not per account: a description is one service for one workspace's account, and a person's own
  connection is usually limited per application as well; a per-person limit is left until a description needs it.
* Upkeep drops windows older than a day, and attempts (`conn_attempt`) older than 90 days.

Tests (`connections_steps_it` `limits`): a 429 with `Retry-After: 7` reaching an app's call as seven seconds; two
requests a minute, the third not sent and told when; two workers' calls under `concurrent: 1` never overlapping, the
second waiting its turn; a limit without its window refused.

## BD-58. A mapping that writes as the system

Decided 13 November 2026. Social made `external_id` writable because a person's pull (BD-50) runs as that person and
read-only fields ignore her input.

* `as: system` on a mapping (inbound or a pull's): its writes run as the system (`is_system`, every permission), so
  read-only fields and fields she could not write are set; the person a pull runs for stays the records' maker
  (`created_by`), `origin: outside` still applies when the mapping says `outside`. Workspace pulls and inbound calls
  already ran as the system; this changes only a person's own pulls.
* **How it is enforced**: a mapping lives only in a connection's description, and a description is written only by
  `save_connection_def` and a pack's install, both administrators' (`admin_only`), or offered by a package and taken
  up by an administrator's save. No app call, effect, gate call or person's request carries a mapping or changes one;
  every save is audited. So `as: system` is what an administrator or a package declared, never what a caller asks.
* Why not a list of fields the mapping may set past read-only: the mapping already names every field it writes, and
  the person who could write it is the administrator; a second list would say the same thing twice.

Tests (`connections_steps_it` `as_system`): Ana's own pull making a post with the read-only id set, made as her; her
other pull without it leaving that field empty; her own write not setting it; her save of a description refused.

## BD-59. XML and form answers

Decided 13 November 2026. Social's feeds (autopost) read RSS and Atom; some sign-ins and older APIs answer forms.

* **`answer: xml`** (with `lists`) on an operation: the answer is read into JSON's shape and everything that reads an
  answer reads that (results, `next_at`, `items_at`, mappings). The shape: `{<root>: <element>}`; an element with
  neither attributes nor children is its text; otherwise `@name` for each attribute, each child by its name with its
  prefix as written (`dc:creator`, `media:content`; an element in a default namespace by its local name), and `#text`.
  A child that comes twice becomes a list; `lists` names those that are lists even once (a feed with one item), so a
  pointer does not change with the count. Why this shape rather than a feed model: one rule for RSS, Atom and plain
  XML, and nothing named for a format.
* **Safe by default**: `roxmltree` (already locked in the workspace) without document types, so no entity of the
  document's own is expanded and nothing outside is fetched; at most 64 levels deep; the size is the answer's, already
  bounded. Only an answer that worked must be XML; an error page is kept as it came, so `error_at` still sees its text.
* **`answer: form`**: `a=1&b=2` as an object of texts; `text` keeps the text as it is; `json`, or nothing, is the old
  rule (JSON, else its text). An existing description reads its answers as before.

Tests (`connections_steps_it` `xml_answers`, unit test): an RSS feed with `&amp;`, a CDATA title and a prefixed element,
and an Atom feed of one entry, pulled into posts; a prefixed element and an element's text beside its attribute read
by pointers; a form answer; a document type refused; an unknown kind refused; a document nested too deep refused.

## BD-60. A pull that asks for each item, and the signed-in account

Decided 13 November 2026. Reading back an account's own posts needs the account's id in the path (from its sign-in),
and each post's numbers are another request.

* **`each`** on a pull: a reading operation (one request, not a sign-in) asked once for each item of a page, its inputs
  expressions over `item`; its result is put in the item under `as`, so the mapping reads it. Bounded: `max` items a
  page (10, at most 50; a page with more fails the run before asking anything, so a description sets its page size to
  fit), and each ask is a request counted by the connection's `limit` (BD-57). A "later" from one carries the page
  over to the pull's retry, as a page's own "later" does (the page and its cursor are not committed).
* **Inputs from the account**: a pull's `inputs` are expressions over `account` (the account its sign-in named, the
  person's own on a person's pull), `auth` (what the sign-in kept, BD-61), `now` and `today`, evaluated once a run. An
  operation's templates read `account` and `auth` as well, loaded only when its words name them (one read of the
  sealed tokens). Why the account and not "the caller's record": a call already passes what a record holds as its
  inputs (an effect's expressions over `doc`); what a call could not reach was its own sign-in's facts.

Tests (`connections_steps_it` `pull_each`): an account's own posts read by its id from the sign-in, each post's likes
asked once and mapped; an operation's path from the account; a page over `max` failing the run without asking; `each`
on an operation that changes something refused.

## BD-61. Sign-in extras

Decided 13 November 2026. Found while describing networks for Social: Meta's long-lived tokens, Instagram's and
Threads' refresh by a GET with the token itself, VK's and TikTok's extra fields in the token answer, a Mastodon
instance's own client registration, tokens that never expire.

* **`keep`** (provider, or session): named fields of the token answer (an account's id, an instance's address) are
  sealed with the tokens as `kept`, and read as `auth.<name>` by templates and pull expressions (BD-60); a refresh that
  does not repeat them keeps the earlier ones. Not the tokens themselves; a kept field is shown where a template puts it
  (it is not a secret), unlike the tokens, which stay masked.
* **A token answer written as a form** (`access_token=…&expires_in=…`) is read as one, at the token address and in
  revocation.
* **`after`**: operations of the connection made, in order, after the code is exchanged, given the tokens and the
  client's id (the client secret is `secret.<name>` in their templates); each result's names replace the answer's. Why
  operations: the exchange is a request like any other (a GET with a query), and describing it keeps the platform
  free of any network's grant names.
* **`refresh`**: an operation that refreshes in place of the token address, given the same inputs; used even without
  a refresh token (the access token is its own). Its result is read as a token answer; a refusal marks the sign-in
  needing reconnecting, as a refused refresh does.
* **`registration`**: an operation that registers the platform's client at this server (Mastodon's `POST /api/v1/apps`
  takes `redirect_uris` and `scopes` as its own words, not RFC 7591's), given the exact callback, the scopes and the
  connection's name; its `client_id` and `client_secret` are sealed as RFC 7591's are (`_client_id`), once, under the
  connection's lock; the client then proves itself with basic authentication, as a registered one does. One connection
  is one server: a person's instance is a connection of its own (its base and addresses), registered on first use.
* **`never_expires`**: the answer's expiry is ignored; the token is never refreshed ahead (a 401 still asks once).
* All these operations sign in (`signs_in_with`): never called by an app, an effect, a pull or "try"; made without
  credentials of their own, never captured, the tokens they are given masked like secrets.
* The connection's secrets are now locked `for no key update` while they change, so a call logged meanwhile (whose row
  refers to the connection) is not held up; a registration made by an operation is logged like any call.

Tests (`connections_steps_it` `signing_in`): a form-encoded token answer, exchanged after the sign-in for a long-lived
token, the account's id kept and read in a path; refreshed by its GET without a refresh token, the id still kept;
the short and long tokens and the client secret at rest nowhere; the exchange refused as an operation; a client
registered at the server by its own request once over two sign-ins, with the exact callback and the scopes; the token
asked with the registered client; a token kept without an expiry.

## BD-62. Connections, part three, and the App API 0.3.7

Decided 13 November 2026.

* **The App API is 0.3.7**; an app using any of this says `platform: ">=0.3.7, <0.4"`. 0.3.7 carries: an operation's
  `steps` (with `when`, `confirm`, `wait`, `for_each`, `range`, `chunks`), `url`, `credentials`, `form_fields`,
  `success`, `answer`, `lists` (BD-53 to BD-55, BD-59); `connections::More` and `Description::more` (an app that builds
  descriptions in code sets them there); `FileStore::get_range` (a store an app supplies may read a range); the clock and
  the template functions (BD-56); `RetryAfter` from `connections::call` and a description's `limit` (BD-57); a mapping's
  `as: system` (BD-58); a pull's `inputs` and `each`, and `account`/`auth` in templates (BD-60); `provider.keep`,
  `after`, `refresh`, `registration`, `never_expires` and a session's `keep` (BD-61).
* **What an app must change**: only an app that matches `connections::call`'s errors: `RetryAfter` is "later" with a
  time (Social's publish job matched `Unavailable` and `Wait` only). An app that builds an `Operation`, an `Inbound` or
  a `Description` in code compiles unchanged: no field was added to the first two, and the third is built with
  `..Default::default()`. Every stored description, call and pull reads as before.
* Reference products read for this part, and which of their mechanisms each answers: docs/references/social.md.


## BD-63. A left-menu row for an app's own page, and the App API 0.3.8

Decided 13 November 2026.

* **The gap**: a package's module lists entities and `report:<name>` only, and the records frame drops anything else,
  so an app's own pages (Social's composer, calendar, accounts, media, inbox, numbers) had no row in the left menu;
  the only page rows (the till, Chat, Calendar, Meet, Documents) are written into the frame.
* **In the manifest, not the package**: a new shell point `shell.menu`, item `{id, module, path, label, icon?, roles?,
  after?}`. A page is the app's interface, which only the manifest knows (its routes, its `ui`); its `path` is checked
  as every other point's path is (it starts with one of the app's routes, plain segments only, so nothing outside the
  app), `roles` mean what they mean on `shell.app`, and a workspace's own menus (`define_module`) cannot have one,
  having no interface. `module` must be a module of the app's own packages (checked in the bundle; at serving, a row
  whose module is not there is left out). `icon` is a name (`square-pen`); one the menu does not draw falls back to a
  page's icon, as other rows fall back. `label` is the app's English, shown in the page's words as entity names are.
* **Served with the modules**: `GET /api/meta` gives each module `pages`, the rows this person sees (a member who may
  use the app and holds one of the row's roles; administrators all; portal people none). `after` names the module's
  item a row follows (an entity, `report:<name>` or another row's id); without one, or naming one not shown, rows come
  first in the order declared.
* **Drawn as other rows**: the frame's left menu and Home's module cards. Inside the app's own page under its own route
  a row is a client-side route (`/compose`, with the active state); anywhere else (another app's frame, the classic
  web app) a link that loads `/{path}`.
* **The App API is 0.3.8**; an app using `shell.menu` says `platform: ">=0.3.8, <0.4"`. 0.3.8 also carries BD-64 to
  BD-66.

## BD-64. A stored file's short-lived public address, for a service that pulls it

Decided 13 November 2026.

* **The gap**: some services take no bytes; a step sends `image_url`/`video_url` and the service fetches it (Instagram,
  Threads). The platform gave an app no address a service could reach.
* **What was there**: `/files/public/{id}` serves files marked public for good, which is not this; S3 presigned URLs
  exist for uploads only, and a local store has none. Reused: the streamed download with ranges (`Objects::stream`,
  `streamed_response`, `file_headers`), the server's key (`SecretKey::fingerprint`, HMAC-SHA256), `PUBLIC_URL` as the
  connection callback uses it. One route serves every store the same way.
* **The shape**: an operation's `public_files: {<name>: {file, minutes?, end_with_call?}}`; templates read
  `public_files.<name>`, a step's inputs too. A property of the operation, resolved before the request is built, rather
  than a template function: making an address writes a row and checks the caller may read the file, which a template
  (synchronous, run for every attempt) cannot do once.
* **The address**: `{PUBLIC_URL}/files/pull/{id}?s={HMAC(id)}`; the row (`conn_file_address`) names the file, the
  connection, operation, key and record of the call, the person, until when, and when it was ended. The signature is
  in the query, which the request log never writes; any change to the id or the signature misses. Default 60 minutes,
  at most 360. Not found alike for every refusal.
* **Ending**: at its time, or with `end_with_call` once the call has its answer (not while it waits for the service, which
  may still be fetching). A keyed call made again (a retry, a resumed wait) gets the address it made while it has ten
  minutes left, so the service is not given a second one. Rows go 90 days after they end.
* **A secret**: the masking every capture, error and result goes through hides `s=<64 hex>` (and `s%3D…`) wherever it
  appears; a capture of the request never shows it.
* **Without `PUBLIC_URL`** the call is refused: "a service cannot pull a file from localhost". Without the server's key
  there is no signature, so it is refused too.
* **The App API is 0.3.8** (BD-63). An app hands the file's id as an input and names the address in its description.

## BD-65. Credential-free reads of any public host

Decided 13 November 2026.

* **The gap**: a feed reader (Social's feeds) reads whatever site a person gives; `allowed_hosts` cannot list every site
  with a feed, so each new site waited for an administrator.
* **The flag**: a description's `any_host: true` lifts the host list and nothing else. What makes it safe is what it
  cannot carry: no auth, no secrets (given with it, kept from before, or merged later), no template naming `secret.`,
  `auth.` or `account.`, no inbound address, and only operations that read: GET (or steps of such), no body, form,
  form fields or public files. So a request it makes carries nothing of the workspace but its inputs.
* **The guard is unchanged**: https only (plain http only to a host the operator opened), every address resolved and
  refused when private, loopback, link-local or reserved, then pinned; each redirect checked the same way (a redirect to
  a private address is refused); answers capped by size and time as before.
* **Who**: saving any description is an administrator's (`save_connection_def`), and a package's connection is offered
  turned off for an administrator to turn on, so the flag is never set by anyone else.
* Part of the App API 0.3.8 (BD-63): an app's description may carry it; Social's `feeds` connection can drop its
  "add the host" step.

## BD-66. An app told when an approval is decided

Decided 13 November 2026.

* **The gap**: an app whose record went through an approval policy learned the outcome only by reading it back (Social's
  review: the post's state, polled).
* **A record hook**: `DocHook::approval_decided(hc, doc, &ApprovalDecided)`, beside `on_submit` and `after_action`,
  with a default that does nothing, so every app compiles unchanged. Registered per entity as the other hooks are, so an
  app hears of its own entities' approvals only (and a workspace's packages lacking the app's are left out as for every
  hook).
* **When**: once per request, at its outcome: `approved` (and done), `rejected`, or `failed` (approved, then refused when
  done, `failure` saying why). Not at a step that leaves the request pending. In the deciding transaction, after the
  outcome took effect, so the hook sees the record as the decision left it and its writes commit with the decision.
* **What it is given**: the record, the request's id and what was asked, the outcome, who asked, who decided (and for
  whom when standing in) and the decider's comment.
* **A failing hook**: each hook runs in a savepoint; an error rolls back that hook's writes, is logged (WARN) and audited
  on the record (`approval:hook_failed`, with the hook's name and the error), and the decision commits. A retryable
  error (serialization, deadlock) is passed on: `decide_approval` retries the whole decision, so a hook runs once for
  the decision that commits. A hook that must not miss an outcome reads the request again from its own job.
* Part of the App API 0.3.8 (BD-63).

## BD-67. Expressions that nest run on a stack of their own

Decided 13 November 2026.

* **The fault**: saving Social's Telegram description overflowed a 2 MiB thread (the size of a server's worker) in a
  debug build; its tests passed only on 16 MiB. The cause: its `publish` result is one CEL expression picking among a
  dozen answers by nested `?:`, and CEL's parser goes through a dozen grammar rules for each level, with large frames.
  Compiling it took between 2 and 4 MiB; evaluating it recurses as deep. Nothing else in the 33 network descriptions
  came near (templates, serde of nested steps and the rest of validation were measured on 2 MiB).
* **The fix, where every expression goes through**: `ferrum_meta::Expr` compiles, and runs, an expression with more
  than four levels of `?`, `(` or `[` on a stack of its own (`stacker::grow`, 16 MiB of address space, of which only
  the pages touched are used; `stacker` was already in the tree through `pest`). Every place an expression is checked
  or run (rules, conditions, a description's results and mappings, steps) is covered, on any thread. Flat expressions
  run where they are, as before. The limits (`MAX_NESTING` 32, `MAX_OPERATORS` 512) stay, and the deepest nesting
  they allow compiles on 2 MiB.
* Tests: `ferrum-meta` compiles and runs a dozen nested `?:` and the deepest allowed nesting on a 2 MiB thread (it aborts
  with a stack overflow without the fix); `ferrum-kernel`'s `descriptions_small_stack` loads, validates and works out
  the result of the Telegram description (a fixture) on 2 MiB.
