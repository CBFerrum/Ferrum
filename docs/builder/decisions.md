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
  so a restore erases them again.

**Rules out**: a provider named in code; a message rendered with the reader's rights; a send outside the caller's
transaction; marketing past an opt-out; a number's country guessed.

**Not yet**: several connections of one kind with routing between them; reading a provider's template catalogue; MMS
and attachments outside e-mail; a person choosing channels for a notification kind (BL-48 stays the notifications'
own); scheduled reminders (an automation on a schedule sends them today).
