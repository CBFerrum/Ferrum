# Data and records

An app's data is records of entities its packages declare, and, where an engine needs them, tables of its own.
Records go through the document pipeline, which checks, versions, audits and raises events for every change; the
app's own tables are the app's to keep right. This guide covers both, and the review that keeps writes outside
the pipeline few and named.

The format is defined in `crates/meta/src/types.rs` (what package YAML holds) and read by
`crates/meta/src/catalog.rs`; the kernel turns it into tables in `crates/kernel/src/ddl.rs` and serves it through
`crates/kernel/src/doc.rs`. The API's view of the same is [docs/api.md](../api.md), *Metadata* and *Documents*.

## Packages

A package is a directory `packages/<name>/` with `package.yaml`, `entities/*.yaml` (one entity or a list per
file), and optionally `sql/*.sql`, `print/<Entity>.html`, `print/*.layout.json` and a `README.md` documenting its
endpoints. The app's manifest lists it under `packages`. Everything is embedded into the crate at build time
(GRC's `build.rs`) and loaded with `ferrum_meta::catalog::load_embedded`.

`package.yaml` (`PackageManifest`):

| Key | What it holds |
|---|---|
| `package`, `version`, `label`, `description` | its name (the directory's) and its own version |
| `depends` | packages whose entities it links to, overlays or seeds; nothing else may be reached |
| `field_prefix` | the prefix every field it adds to other packages' entities carries (`in_` for India) |
| `modules` | menus: `{name, label, icon, order, items: [Entity or report:<name>]}` |
| `roles` | `{name, description, permissions, portal}` ([05-permissions-and-people.md](05-permissions-and-people.md)) |
| `workflows` | states and transitions of an entity (below) |
| `overlays` | changes to other packages' entities (below) |
| `seed` | records every workspace starts with: `{entity, key: [fields], records}`, made once by `key` |
| `personal_data`, `handover` | where people's data is and whose work moves when they leave ([12-privacy-and-data-duties.md](12-privacy-and-data-duties.md)) |
| `export_tables`, `merge_links`, `backfills` | engine tables a workspace export carries, link columns moved when records merge, data a release moves in batches |
| `events` | business events it publishes besides its records' own ([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md)) |
| `messages`, `documents` | the messages it sends and the documents it renders, each a template a workspace may edit |
| `connections` | outside services it describes, never with a secret ([11-integrations.md](11-integrations.md)) |
| `approvals`, `automations` | default approval policies and automations on its own entities, which a workspace may switch off or edit |
| `uses` | operations of packages it depends on that its app calls with `Kernel::call_tx`: `{capability: {package}.{entity}.{operation}, version: <semver range>}`, approved with the app at install ([11-integrations.md](11-integrations.md), [docs/app-contract.md](../app-contract.md), *Using another app's operations*) |

**A package works with only its dependencies installed.** The catalog refuses one whose links, overlays,
workflows, seeds or personal-data rules reach an entity outside its dependencies (`… seeds X, which is not among its
dependencies' entities`, `crates/meta/src/catalog.rs`). What must reach further is a connector
([01-what-an-app-is.md](01-what-an-app-is.md)). `ferrum task deps_report` lists what ties packages together.

## Entities

An entity (`EntityDef`) is a kind of record. GRC's `Finding` (`ferrum-app-grc/packages/grc/entities/grc.yaml`):

```yaml
- name: Finding
  label: Finding
  label_plural: Findings
  kind: master
  module: grc
  title_field: title
  description: Something an audit or a test found wrong, owned and due. Closed, it says how it was fixed.
  fields:
    - {name: title, type: data, required: true, in_list: true}
    - {name: severity, type: select, options: [Low, Medium, High, Critical], default: Medium, in_list: true, in_filter: true}
    - {name: control, type: link, to: Control, in_list: true}
    - {name: owner, type: email, required: true, in_list: true, in_filter: true}
    - {name: due_on, type: date, required: true, in_list: true}
    - {name: status, type: select, options: [Open, Remediating, Closed], default: Open, in_list: true, in_filter: true}
    - {name: remediation, type: text}
    - {name: overdue, type: check, read_only: true, no_copy: true, in_list: true, in_filter: true}
  numbering: {template: "FND-{YYYY}-{seq:4}"}
  list: {columns: [number, title, severity, owner, due_on, status, overdue], sort: due_on asc}
```

| Key | Meaning |
|---|---|
| `name` | PascalCase, `[A-Z][A-Za-z0-9]{0,62}`; its table is `e_<snake_case>` (`Finding` → `e_finding`; `ferrum_meta::table_for`) |
| `kind` | `master` (reference data, no lifecycle), `document` (a transaction; `submittable: true` gives draft → submitted → cancelled), `child` (lines of a parent through a `table` field), `settings` (one row per workspace, and per company when company-scoped), `ledger` (an engine's append-only table, read-only through the API) |
| `title_field` | what names a record in links, lists and search |
| `reference` | `open` (default: whoever reads a record linking here sees this one's title) or `restricted` (only those who may read it) |
| `numbering` | below |
| `company_scoped` | needs a `company` link; records are limited to the person's companies |
| `owner_fields` | email fields (or `table.field`) that make a record a person's own, besides who created it |
| `internal` | not to leave for an AI provider unless a workspace says otherwise (pay, health, personnel files) |
| `record_reads` | `open` or `all`: its reads are recorded on the audit trail wherever the workspace's own `record_reads` does not name it (a patient's chart, a personnel file); [operations](../operations.md#recording-who-read-what) |
| `mergeable`, `duplicate_fields` | duplicates may be merged; which fields suggest two records are one |
| `unique`, `indexes`, `exclusion` | unique groups of fields, indexes, and PostgreSQL exclusion constraints on overlapping validity (`{fields, from, to, message, when_not}`) |
| `rules` | checks on save, submit or a transition (below) |
| `list`, `form`, `search_fields` | the list's columns and sort; form sections `{label, fields, columns, collapsible, tab, visible_if}`; fields search reads |
| `actions` | buttons the app's hooks implement (below) |
| `targets` | service targets: what must happen within so much working time ([docs/api.md](../api.md), *Service targets*) |
| `table` | the physical table when it is not `e_<name>` (a ledger entity points at an engine's table) |
| `module`, `icon`, `description`, `label`, `label_plural` | where it shows and what it is called; `description` is what the help center searches |

Every entity table has columns the platform keeps, which no field may use: `id`, `tenant_id`, `version`,
`docstatus`, `number`, `created_at`, `created_by`, `updated_at`, `updated_by`, `ext`, `parent_id`, `idx`,
`amended_from`, `workflow_state`, `entity` (`RESERVED_FIELDS` in `crates/meta/src/lib.rs`).

## Fields

A field (`FieldDef`) has a `name` (snake_case, `[a-z][a-z0-9_]{0,62}`) and a `type`. Each type's column
(`column_type` in `crates/kernel/src/ddl.rs`) and what it is for:

| Type | Column | For |
|---|---|---|
| `data` | text | one line |
| `text`, `markdown`, `code` | text | several lines; Markdown; code |
| `int` | bigint | whole numbers |
| `decimal`, `currency`, `percent` | numeric(28,9) | exact numbers; money (`currency` names the field holding the currency code, `parent.<field>` from a line); a percentage |
| `date`, `datetime` | date, timestamptz | a business date in the workspace's zone; a moment |
| `check` | boolean | yes or no |
| `select` | text | one of `options` |
| `link` | uuid | a record of `to`; `on_delete: restrict` (default), `set_null` or `cascade`; a link to its own entity makes a hierarchy, and a loop is refused |
| `table` | none | lines: rows of the child entity `of`, in a table `<parent table>__<field>` |
| `json` | jsonb | structured data |
| `email`, `phone`, `url`, `color` | text | contact details, an address, a colour |
| `attach` | text | a file, tied to the record ([09-files-and-storage.md](09-files-and-storage.md)) |
| `secret` | none | write-only; sealed outside the record; reads say `true` when set ([08-secrets-and-the-vault.md](08-secrets-and-the-vault.md)) |

Money is always a decimal kind, never `int` or a float, and travels as a string in the API (`"1234.5"`,
[docs/api.md](../api.md), *Conventions*).

Properties a field may carry:

| Property | Effect |
|---|---|
| `label`, `description` | its name on screens (default: the name humanised) and help |
| `required`, `unique`, `index` | a value must be there; no two records share it; an index |
| `read_only`, `hidden` | people's input is ignored; not shown |
| `in_list`, `in_filter` | a column in lists; a filter |
| `default`, `default_expr` | a value, or a CEL expression, for a new record |
| `computed` | a CEL value; the field becomes read-only. Not for money (CEL has no decimal) |
| `rollup` | `{table, op: sum|count, field?}`: an exact total over the record's own lines, computed at every save before validation |
| `options`, `to`, `of`, `precision`, `currency`, `min`, `max`, `max_length` | the type's own settings |
| `visible_if`, `required_if`, `read_only_if`, `read_only_message` | CEL conditions; the server is the authority. With `read_only_message`, a change while `read_only_if` holds is refused with that message instead of ignored |
| `fetch_from` | `link_field.target_field`: copied from the linked record when the link changes |
| `link_filters` | `{field: value}` limiting a link's choices, checked on write; `{from: <field>}` or `{from: parent.<field>}` to follow another field |
| `perm_group`, `mask` | a field group roles grant separately; what someone outside it sees (`last4`, `email`, `hidden`) |
| `no_copy`, `no_export`, `locked` | not copied on amend or duplicate; never in a list's export; overlays may tighten it but never loosen it |
| `widget` | `studio_query` on a `json` field holding a Studio query |
| `on_delete` | for a link: what happens when its target goes |

A sensitive field (a bank account, an identity number) goes in a field group, masked where a person needs to know
there is one, and `no_export` where it must not leave in bulk ([docs/app-contract.md](../app-contract.md),
*Reviewed*).

## Children and links

A line is a record of a `child` entity, reached only through its parent's `table` field (`of:`). GRC's controls on a
risk:

```yaml
- name: RiskControl
  label: Control
  kind: child
  fields:
    - {name: control, type: link, to: Control, required: true, in_list: true}

- name: Risk
  # …
  fields:
    - {name: controls, type: table, of: RiskControl}
```

A `PATCH` replaces the lines with those given: rows with a known `id` are patched, rows without one added, rows
missing removed ([docs/api.md](../api.md), *Documents*). Lines go two levels deep at most (a record, its lines, their
lines).

A link is a UUID. A record you can read shows the titles of what it links to (`titles=true`), even when you cannot
open the target, unless the target's entity says `reference: restricted`. In expressions, `links.<field>` reads the
linked record (one level, at most five different links in one expression, refused where there is no transaction:
[docs/api.md](../api.md), *Linked records*).

## Fields an app adds to another app's entity: overlays

A package changes another package's entity with an overlay (`OverlayOp` in `crates/meta/src/types.rs`). A package
may use:

| Op | What it does |
|---|---|
| `add_field` | adds a field; with `column: true`, a real column placed after `after` (how a package takes over a field that moves out of an entity) |
| `set_property` | changes a field's property; a `locked` field may be tightened, never loosened |
| `add_rule` | adds a rule |
| `add_options` | adds options to a select |
| `set_entity_property` | changes an entity's property |
| `add_action` | a document action on the other entity, implemented by the overlaying package's hooks (Projects' connector with ERP adds "Create Sales Invoice" to Project) |
| `add_section` | a form section for the fields it adds |

The rest (`define_entity`, `set_workflow`, `set_print_template`, `define_module`, `publish_list`, `set_approval`
and others) are a workspace's own, through `POST /api/customize` ([docs/api.md](../api.md), *Customization*).

A field an overlay adds without `column: true` is stored in the row's `ext` JSONB column, with no `ALTER TABLE`
(`Storage::Ext` in `crates/meta/src/effective.rs`). In a third party's app, fields added to other packages' entities
start with its `<snake prefix>_`; a package with a `field_prefix` holds its fields to it.

## Custom fields

A workspace adds its own fields with `{"op": "add_field", …}` through `POST /api/customize`; their names start with
`x_` (`custom field … must start with 'x_'`, `crates/meta/src/effective.rs`), they live in `ext`, and every list,
form, filter, export and API answer treats them as fields. A workspace defines whole entities too (`define_entity`,
names starting with `X`, stored in the generic `custom_record` table).

What this means for an app:

- **Never name a field `x_…` or an entity `X…`**: those namespaces are the workspace's and the plugins'.
- **Read and write records through the kernel** (`Kernel::get`, `insert`, `update`, `list`), which knows the
  workspace's fields. A query of `e_*` columns does not see what is in `ext`.
- **An upgrade never removes what a workspace added**: customizations are layered over the shipped metadata, and a
  release that would leave one writing a field that is gone flags it ([docs/api.md](../api.md), *Customization*).

## Numbering

`numbering: {template, gapless, on, date_field}` (`NumberingDef`) fills the record's `number`:

- tokens `{YYYY}`, `{YY}`, `{MM}`, `{DD}`, `{FY}` (the fiscal year's start year), `{field:<name>}`, `{seq:<width>}`;
- `on: insert` (default) or `on: submit`, inside the submit's transaction: what a legally gapless series needs;
- `gapless: true` keeps the series without holes; a workspace may change the template but a gapless series stays
  gapless;
- `date_field` gives the date the tokens use (default: today in the workspace's zone).

## Rules, expressions and state rules

**Rules** (`RuleDef`) are CEL checks a record must pass:

```yaml
rules:
  - name: due_after_found
    check: "doc.due_on >= doc.found_on"
    message: "{{ doc.title }} is due before it was found"
    on: save            # save (default) | submit | transition
    field: due_on       # where the refusal shows (default: the rule's name)
```

A rule `on: transition` runs only when a workflow transition is taken, on the record in its new state, and reads
`transition: {action, from, to, params}`. The message may show the record's values, as the person reading it may
read them.

**Expressions** are CEL over `doc` (and `parent` in a line), `user`, `today`, `now`, `time` (local to the
workspace), `settings` where loaded, and `links.<field>` where allowed. Whole numbers are signed integers; decimals
are floats for comparison, so a rule about money compares with care, and a computed amount is a hook's job. The
functions (`addMonths`, `formatDate`, `lower`, `coalesce`, …) are in
[docs/builder/expressions.md](../builder/expressions.md).

**State rules** belong to a workflow (`WorkflowDef`: `{name, entity, initial_state, states, transitions}`). A state
(`WorkflowState`) says its `docstatus`, the roles that may edit in it (`edit_roles`), the only fields a person may
change in it (`editable`) and the fields a record needs to enter and stay in it (`required`). A transition says who
may take it (`roles`), a guard (`condition`), who may not (`not_creator`, `not_creator_of`), what it asks
(`params`), what it does (`effects`) and who must agree first (`approval`); see
[05-permissions-and-people.md](05-permissions-and-people.md).

## Validation

What refuses a save, in the order a save runs them (`crates/kernel/src/doc.rs`; `DocHook` in
`crates/kernel/src/hooks.rs`):

1. Defaults and `fetch_from`; then each hook's `before_validate`, which may fill fields (and must say which in
   `fills`). Computed fields and rollups are worked out before validation.
2. The kernel's own validation (the private `validate` in `crates/kernel/src/doc.rs`): the metadata (required fields, types, options, lengths, ranges,
   `link_filters`, `required_if`, a workflow state's `required` and `editable`) and the entity's rules.
3. Each hook's `validate`, then `validate_change` (which compares with the stored record: "the time of a confirmed
   booking cannot change").
4. The write (a unique key another record holds is refused here, `409`), then `after_save` in the same transaction.

A hook refuses with an error, and the save rolls back:

| Error (`crates/kernel/src/error.rs`) | HTTP | Use it for |
|---|---|---|
| `Error::field(field, message)` | 422 `validation_failed`, the field named | a value that is wrong; shown on the field |
| `Error::Validation(vec![FieldError::new(…).in_row(table, i)])` | 422, with `row` | several fields, or a line's field |
| `Error::invalid(message)` | 422 `invalid` | a state that does not allow it ("the text is as published: there is nothing new to publish") |
| `Error::Conflict` | 409 | a version that moved, a key already used |
| `Error::Forbidden`, `Error::NotFound` | 403, 404 | see [05-permissions-and-people.md](05-permissions-and-people.md) |
| `Error::internal` | 500 | a bug; never for something a person did |

Write each message so the person knows what to do next, without blaming them or saying what they may not see
([20-writing-for-people.md](20-writing-for-people.md)). GRC's: `controls do not make a risk worse than it is
without them`; `you acknowledged revision 3 already`.

## Actions

An action is a button with a handler: declared on the entity, implemented by `DocHook::action` returning `Some`
(GRC's `src/policies.rs`):

```yaml
actions:
  - {name: publish, label: Publish It, permission: write, condition: "doc.status != 'Retired'"}
  - {name: acknowledge, label: I Have Read It, permission: read, condition: "doc.status == 'Published'"}
```

| Key | Meaning |
|---|---|
| `permission` | `read`, `write` (default), `create`, `submit` or `cancel`: the right the person needs on the record |
| `docstatus` | the document statuses it runs in |
| `condition`, `condition_message` | when it shows and runs; what a refusal says |
| `confirm` | the question asked first |
| `params` | what it asks for: `data`, `text`, `select`, `int`, `date`, `check`, `link`, `files`; a `secret` one is shown `[redacted]` wherever it is kept |
| `read_only` | it only reads: no lock, no audit entry, a read-only transaction |
| `hidden` | for code, never a button |
| `group` | buttons of one group show under one menu |
| `contract` | what it promises to processes and agents ([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md)) |
| `effects`, `roles`, `refuse_if` | a configured action (a workspace's, with no handler): what it does in order as the system (`set_fields`, `update_linked`, `create_document`, `notify`, `email`); the roles besides the permission; the reasons `{when, message}` that refuse it |

The kernel records `action:<name>` with its parameters on the audit trail before the handler runs, in the action's
transaction (`docs/registry/mutation-paths.md`, *What holds on every path*). A role's rule may name actions its
holders may run whatever the action's own permission asks (`actions:` on a `PermissionDef`).

## Transactions

Every request runs in one transaction; a hook runs inside it and must not commit, open another for the same work,
or call anything outside ([06-events-jobs-and-logic.md](06-events-jobs-and-logic.md)). The transactions the kernel
opens (`crates/kernel/src/db.rs`):

| Call | What it is | Use it for |
|---|---|---|
| `k.db.tenant_tx(tenant)` | one workspace's rows, by row-level security | the app's own work in a workspace: upkeep, an event sink, an endpoint |
| `k.db.read_tx(tenant, timeout_ms)` | read-only, on the read replica when it has caught up | reports and long reads |
| `k.db.side_tenant_tx(tenant)` | a short write on the side pool while another transaction is open | a record that must stand whether or not the request's transaction commits |
| `k.db.system_tenant_tx(tenant)` | the system's, pinned to one workspace | engine work that must see past a person's rights in one workspace |
| `k.db.system_tx()` | crosses workspaces | the installation's own work, done once (a table every workspace writes; see app-contract.md, *Upkeep*) |

The kernel's write calls come in two forms: `insert`, `update`, `submit`, `run_action`, `transition`, which open
their own transaction, and `insert_tx`, `update_tx`, `submit_tx`, `transition_tx`, which run in the caller's. Inside a
hook use the `_tx` forms with `hc.tx`. A context that skips permission checks for work the engine does on a
person's behalf, while `created_by` and the audit still name the person, is `ferrum_domain::util::elevated(ctx)`;
`Ctx::system(tenant, schema)` is the system itself.

Optimistic concurrency: records carry `version`; pass the expected version (`If-Match` in the API,
`expected_version` in the kernel) and a mismatch is `409`.

## Writes outside the pipeline, and their review

`Kernel::set_values_tx(ctx, tx, entity, id, values)` writes derived fields (status, progress, outstanding) with no
validation, hooks or audit of their own, as consequences of an audited operation in the same transaction
(`crates/kernel/src/doc.rs`). GRC's `publish` action uses it for `status`, `revision`, `published_text` and `published_on`.

An SQL `update`, `insert into` or `delete from` of an entity table (`e_*`) in an app's code skips the pipeline
altogether. Each one is listed by app, file, table and function in
[docs/registry/mutation-paths.md](../registry/mutation-paths.md), with its kind:

| Kind | What keeps it right |
|---|---|
| Engine work: send statuses, counts, queues, upkeep | values the app computes, no person's input |
| Consequence of an audited document operation | the operation that runs the hook is on the record |
| A person's action on a public link | the record carries its evidence |
| Records its own audit or activity entry | it is the record |

`scripts/mutation_paths.py` reads every compiled app's sources (not tests) and fails when the code has a statement
the review does not list, or the review lists one the code no longer has, until a person reads it and gives it a
kind (`--write` keeps the kinds given and writes `unclassified` for a new one, which still fails). So a new direct
write is a decision someone signs: prefer the kernel's calls, and when you write one, say in the pull request what
it is and why the pipeline is not the place.

What holds on every path, direct or not: row-level security keeps workspaces apart; a record under a legal hold is
not deleted by any statement (the `ferrum_held` guard); actions are audited before their handlers run.

## An app's own tables

An engine that needs tables the metadata cannot describe (a queue, presence, a journal, an index) keeps them in its
package's `sql/` files. Drive's `packages/drive/sql/001_drive.sql`:

```sql
create table drive_presence (
  tenant_id uuid not null references tenant(id) on delete cascade,
  file uuid not null,
  user_id uuid not null,
  name text not null check (length(name) <= 200),
  mode text not null check (mode in ('view', 'edit')),
  seen_at timestamptz not null,
  primary key (tenant_id, file, user_id)
);
create index drive_presence_seen on drive_presence (tenant_id, seen_at);
alter table drive_presence enable row level security;
alter table drive_presence force row level security;
create policy tenant_isolation on drive_presence
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
```

The rules:

- **Named for the app**: `<app>_<what>` (`drive_presence`, `crm_filed_mail`), never `e_…` (the entities') and never
  one of the platform's tables.
- **A `tenant_id` on every table holding a workspace's rows**, `references tenant(id) on delete cascade`, leading
  every primary key and index.
- **Row-level security enabled and forced, with the `tenant_isolation` policy above**, exactly as the kernel makes
  it for entity tables (`Ddl::rls` in `crates/kernel/src/ddl.rs`). Without `force`, the table's owner (the
  application's role) reads every workspace's rows. **Each partition too**: read by its own name, a partition is a
  table of its own that its parent's policy does not reach. Make every partition with the kernel's
  `ferrum_create_partition(parent, name, lower, upper)` (ERP's journal years, Operations' measurement days, the audit
  trail's months), which gives it its parent's policies, forced. A partition with none of its own reads as empty even
  to the system, and a dump (`deploy/dump.sh` reads each partition by its name) would hold none of its rows: a server
  refuses to start while one exists, naming it (`ferrum_partitions_without_policies()`), and `deploy/dump.sh` refuses
  to dump. Make the parent's policies before its first partitions. The doctor fails a table, a partition or a table in
  another schema than the vault's that does not force row-level security as a blocker:
  `isolation.row_security: N tables hold workspaces' rows without enforcing row-level security: …` (`ferrum task
  doctor`; `crates/kernel/src/doctor.rs`).
- **Files run once each, in file-name order, per cluster**, after entity tables exist, each in its own transaction,
  recorded with a checksum in `cluster_package_sql`. **Never edit a file once it has been applied**: the next start
  refuses (`package SQL drive/001_drive.sql changed after it was applied; add a new file instead of editing
  it`). Number them `001_…`, `002_…`.
- **Only add.** The schema sync never drops, retypes or renames a column; neither does a package file. A removal or
  a rename ships in two releases: the first stops using what is to go, the second removes it
  ([docs/operations.md](../operations.md), *Rolling back and the two-version window*), so the release before still
  runs on the database the new one left.
- **Nothing a pooler in transaction mode refuses, no superuser's statement**: an ordinary role runs these
  ([07-stateless-apps.md](07-stateless-apps.md)). No extension the database may not have; no `security definer`
  function that crosses workspaces.
- **Never the vault's tables** (`vault.*`): `scripts/secret_paths.py` reads packages' SQL too and fails one that names
  them.
- **Only a native app carries `sql/`.** The schema sync runs it raw, as the database's owner, for the whole
  installation, so a bundle of any other runtime (`wasm`, `meta`, `service`) that carries it is refused
  (`check_contents` in `crates/apps/src/bundle.rs`); their tables come from their entities. Private (unsigned) apps
  create no tables at all ([docs/plan/apps.md](../plan/apps.md) §5).

`scripts/window.py` checks the two-version window for the platform's own migrations and packages; it does not read
an app's repository (`--app` reads its package SQL after each package's first file, for what holds writes only), so an app's SQL is held to *only add* by review.

**Data a release moves** goes in `backfills` in `package.yaml` (`BackfillDef`: `{name, table or entity, needs, set,
batch}`): run per workspace in batches of its own transactions (1000 rows by default), resumed where they stopped,
with the rows still needing it counted in its report (`ferrum_kernel::backfill`). Never a long `update` in a migration
file. An upgrade that must reach records made before it (a new standard account, a field's default) does it this
way or says how ([docs/app-contract.md](../app-contract.md), *Reviewed*).

## Search and lists

- `search_fields` are what a list's search and the search bar read; `in_list` and `in_filter` make columns and
  filters; `list: {columns, sort}` is the default view.
- `GET /api/v1/{E}` takes `filters` (`[["field", "op", value]]`; ops `=`, `!=`, `>`, `>=`, `<`, `<=`, `in`,
  `not in`, `like`, `not like`, `between`, `is`; paths `child_table.field` and `link_field.target_field`), `fields`,
  `sort`, `limit` (at most 500), `offset`, `cursor` (for `created_at` and `updated_at` sorts), `search`, `titles`,
  `count`. Filters and sorts use only fields the caller can read. In code, `Kernel::list(ctx, entity, &ListQuery)`.
- An index for what is filtered or sorted often: `index: true` on a field, or `indexes: [[a, b]]` on the entity. An
  app's own table indexes on `tenant_id` first.
- A search provider of the app's own goes in `shell.search` ([03-the-manifest.md](03-the-manifest.md)).

## Limits

| What | Limit | Where |
|---|---|---|
| A list page | 500 records | [docs/api.md](../api.md), *Documents* |
| An import | 5000 records a call | `POST /api/v1/{E}/_import` |
| A bulk change | 500 records at once; 100,000 as a background job | `_bulk`, `_bulk/jobs` |
| An Excel export | 100,000 rows, refused past them | `_export?format=xlsx` |
| Row-level rules | equality and ranges per field; no expression across fields | [docs/limits.md](../limits.md), *Platform* |
| Lines | two levels | customization rules in [docs/api.md](../api.md) |
| An identifier | 63 bytes; longer table names keep a readable prefix and a hash | `fit_ident` in `crates/meta/src/lib.rs` |

The rest, and the platform's known limits, are in [docs/limits.md](../limits.md). Statement timeouts, batches and
indexes for performance are in [14-operations-for-apps.md](14-operations-for-apps.md).

## Checked by

- `crates/meta/src/catalog.rs` and `crates/meta/src/effective.rs`: the package format, dependencies, overlays,
  names, custom fields' `x_`.
- `crates/kernel/src/doc.rs`: validation, versions, permissions on every record operation.
- `crates/kernel/src/ddl.rs`: tables, columns, row-level security; package SQL applied once and never edited.
- `ferrum task doctor` (`crates/kernel/src/doctor.rs`, `isolation.row_security`): every table with a `tenant_id`,
  partitions included, in every schema but `vault`, forces row-level security; held to it by the mutant
  `doctor-misses-unforced-security` in `scripts/mutants.py`.
- `scripts/mutation_paths.py` and [docs/registry/mutation-paths.md](../registry/mutation-paths.md): direct writes to
  entity tables.
- `scripts/secret_paths.py`: no package SQL names the vault's tables.
- `server/src/packages.rs`, `compiled_apps_fit_together`: every compiled app's packages load into one catalog, and
  forms place no field nothing defines.
- Reviewed, not checked: an app's own SQL only adds; table names; money in decimal kinds; messages that say what to
  do next.

## See also

- [05-permissions-and-people.md](05-permissions-and-people.md): roles, scopes, field groups.
- [06-events-jobs-and-logic.md](06-events-jobs-and-logic.md): what a hook may and may not do.
- [12-privacy-and-data-duties.md](12-privacy-and-data-duties.md): `personal_data`, `handover`, erasure.
- [14-operations-for-apps.md](14-operations-for-apps.md): upgrades and performance.
- [docs/api.md](../api.md), [docs/builder/expressions.md](../builder/expressions.md),
  [docs/registry/mutation-paths.md](../registry/mutation-paths.md), [docs/limits.md](../limits.md).
