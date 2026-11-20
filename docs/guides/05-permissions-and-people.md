# Permissions and people

Who may do what in an app is declared in its packages and enforced by the kernel on every path: the records API,
lists and filters, reports, live updates, MCP, plugins and the app's own calls into the kernel
([docs/security.md](../security.md), *Model*). An app's job is to declare roles that match the jobs people do, to
go through the kernel so that those rules apply, and, where its own code decides (an endpoint, a direct query), to
decide the same way the kernel would. This guide says how.

## Roles and rights

A role is declared in a package's `package.yaml` (`RoleDef` and `PermissionDef` in `crates/meta/src/types.rs`). GRC's
(`ferrum-app-grc/packages/grc/package.yaml`):

```yaml
roles:
  - name: Risk Manager
    description: Keeps the risk register, the controls and their tests, the policies and the findings.
    permissions:
      - {entity: Risk, read: true, write: true, create: true, export: true}
      - {entity: Finding, read: true, write: true, create: true, export: true}
  - name: Risk Approver
    description: Accepts, rejects or revokes risks taken on above the tolerance.
    permissions:
      - {entity: Risk, read: true}
      - {entity: RiskAcceptance, read: true, write: true}
  - name: Policy Reader
    description: Reads the published policies and acknowledges them.
    permissions:
      - {entity: Policy, read: true, where: {status: Published}}
      - {entity: PolicyAcknowledgement, read: true, if_owner: true}
```

A rule (`PermissionDef`):

| Key | Meaning |
|---|---|
| `entity` | an entity, or `*` (never in a third party's roles, nor a portal role's) |
| `read`, `write`, `create`, `delete`, `submit`, `cancel`, `export`, `import` | the rights it gives |
| `if_owner` | only records the person created, or that name them in an `owner_fields` field |
| `where` | only records whose fields hold these values: `{field: value}`, `{field: [values]}`, or a range, `{amount: {lte: 10000}}`, `{due: {from_today: 0, to_today: 7}}` |
| `via` | own records through a path to the person's email (`patient.email`, `student.guardians.email`): at most two links, then optionally a child table |
| `read_groups`, `write_groups` | field groups (`perm_group`) it reads or writes beyond the default group |
| `hide` | fields and lines this rule neither shows nor accepts |
| `actions` | the entity's actions its holders may run on the records it reaches, whatever each action's own `permission` asks |
| `shared` | for a portal role: `read` or `add`, what of the record's comments and files its holders see |

A role with `visitor: true` is held by anyone on the customer door, an anonymous visitor included (A11, BD-88; never
also `portal`). Its rules name each entity (never `*`), read only with a `where` or on reference data holding no personal
data, and name only collection or read-only actions; never write, create, delete, submit, cancel, export, import,
`if_owner`, `via`, field groups or `shared`, and never an entity of a package with internal entities, a ledger or a
line. A rule naming a collection action (any role's) carries no `where`, `via`, `if_owner` or `shared`.

A role may hold several rules for one entity; a record qualifies through any of them. **Field groups follow the rule
that reaches the record**: a grouped field shows on a record, and may be written there, only when a rule reaching that
record grants its group (`read_groups`, `write_groups`, and the same role's group-only rows, which grant groups without
an action). An HR user who also holds self-service reads HR's groups on everyone and their own bank details on their
own record only. A role's group-only rows on an entity it reads nothing of grant nothing; a line's grouped fields show
by the groups the line entity's rows give the roles whose rules reach its record. A grouped field some record the
person reaches would hide is neither filtered nor sorted. The hourly upkeep tells administrators, once, of a role whose
groups now show less than before. A role's name may be shared by
several packages, each adding grants. Role names starting with `plugin:` are reserved for plugins' service users
(`crates/apps/src/lib.rs`, *Third-party namespaces*).

What a workspace's administrators add on top (roles given until a moment, pairs of roles nobody holds together, a
field group's mask, `no_export`) is theirs ([docs/security.md](../security.md), *Model*). Administrators may do
anything in their workspace (`explain_access` in `crates/kernel/src/access.rs` answers so).

**Design roles for the jobs people do**, each allowed what that job needs and no more; sensitive fields in a
field group, masked rather than hidden where a person needs to know there is one; a person's own records reachable
through `owner_fields` ([docs/app-contract.md](../app-contract.md), *Reviewed*). The catalog lists links a role may
fill but whose target it cannot read (`links_out_of_reach` in `crates/meta/src/catalog.rs`): a form field that
cannot search. Give the role read on the target, or leave the field to another role.

`Kernel::explain_access(ctx, user, entity, action, record, roles)` (administrators only; the route
`/admin/access/explain` in `server/src/controllers/admin.rs`) says whether a person may, and through which rules,
from the same code requests run. Use it in tests to say why.

## Not found, or refused

The kernel answers two different things two different ways (`row_scope` in `crates/kernel/src/ctx.rs`;
`Kernel::get_tx` in `crates/kernel/src/doc.rs`):

| The caller | The answer | Why |
|---|---|---|
| has no rule giving that right on the entity at all | `403 forbidden`, "not permitted to read risk" (`Ctx::require`) | it says nothing about any record: anyone with that role gets the same answer for every id |
| has the right, but this record is outside every rule they hold | `404 not_found`, as if there were no such record | saying "forbidden" would tell them the record exists |
| the record does not exist | `404 not_found` | |

So: **never tell someone whether a record they may not read exists.** Refuse with `403` only when the answer is the
same whatever record was asked for: the person lacks the right outright, the operation is not theirs (a portal
person asking for a record's comments, refused "the same for every record" before the record is looked at:
`crates/kernel/src/collab.rs`), or a step-up or zone stops them. Everything that depends on the record answers
`404`.

The platform holds to this everywhere a record or file could be probed: another person's ticket in Helpdesk's portal
is `404`, as are drafts, cancelled and other customers' invoices in ERP's ([docs/api.md](../api.md), *App
endpoints*); a signed link that does not work answers `404` with one page whatever the reason
(*Signed action links*); a file of a kind an app keeps for itself is refused "as a file that is not there" to
everyone, administrators included (`crates/kernel/src/files.rs`); a public sign-in code is answered the same, after
the same time, whether or not the address belongs to anyone ([docs/api.md](../api.md), *Auth*).

In an app's own code:

- Read through the kernel with the person's context (`Kernel::get`, `list`, `get_tx`): it already answers `404`
  for a record out of scope.
- Where the app finds records with its own SQL (a portal endpoint, a search), filter by what the person may reach
  *in the query*, and answer a miss and a refusal with the same `404` and the same message.
- `Error::NotFound(label)` for "not there or not yours"; `Error::Forbidden(message)` only for the cases above. The
  message never names the record ([20-writing-for-people.md](20-writing-for-people.md)).

## How an app checks

| Where the code runs | The context it has | How it checks |
|---|---|---|
| A document hook (`DocHook`) | `hc.ctx`: the person saving | The kernel checked the right to save before the hook ran. Calls the hook makes with `hc.ctx` are checked again; with `elevated(hc.ctx)` they are not, so do that only for values the engine owns (GRC sets `status` and `revision` that way) |
| A signed-in endpoint (`Site::handle`, `/api/app/{package}/…`) | `req.user: Some(Ctx)` | Call the kernel with that context; or `ctx.require(entity, Action::Read)?` before the app's own query, which also asks for a step-up where the workspace wants one |
| A public endpoint (`/api/site/{tenant}/{package}/…`) | `req.user: None` | The app decides what an anonymous caller may do, then acts as `Ctx::system` or a service context of its own; rate limit by `req.client_ip` (the server limits public routes to 300 a minute per network: an IPv4 /24, an IPv6 /64) |
| An event sink, a job, upkeep | none of a person's | `Ctx::system(tenant, schema)`; a job done for a person runs as that person, loaded when it runs (`Job::creator`), so someone who left gets nothing done |

`ferrum_domain::util::elevated(ctx)` keeps the person's name on what is written (`created_by`, the audit) and skips
permission checks, within the zone the person is limited to; `Ctx::system` is the system with no person. Never
elevate to do for a person what they may not do themselves: decide first, with their context, then elevate only
the engine's part.

**One read path for a caller's list.** Filters, sort and grouping a caller chose reveal the values of the fields they
read, so whoever asks (the list route, the `{entity}.list` capability, MCP, a plugin's `data.query`, an export) goes
through `Kernel::list_tx(ctx, tx, entity, q, action)`: a filter or sort on a field the caller cannot read is refused, a
filter through a link reaches only the linked records the caller reads, each row is masked. `Kernel::find_tx` is code's
own lookup: its filters and sort are the code's, never a caller's. An app report's caller-chosen grouping or filter
passes `ferrum_domain::reports::readable_choice`; a default the caller cannot read falls back through
`readable_default`; output columns go through `readable_columns`. What leaves the workspace (an export, a mailed
record, a document's data file) holds only records reached by rules that grant **export** (`list_tx(…, Action::Export)`,
`Kernel::get_for_export`), not every record the person reads. A form preview of a stored record reads only one the
caller reads; a save or preview fetches nothing from a `reference: restricted` record the caller does not read (the
link stands, its fields stay empty, a refusal names none of its values). A query one app asks another
(`Kernel::call_tx` of a capability that changes nothing) answers as the caller, never as the system.

**Merging, and links that decide reach.** A merge needs Delete on the duplicate and Write on the record that stays,
each through a rule that reaches that record: reading both is not enough. A merge that moves a link deciding who
reaches records is an administrator's: a field `ferrum_kernel::reach::fields` names (owner fields, fields a rule's
`where` reads, links a rule's `via` crosses, links to `Company` and to entities people are scoped by, fields approval
steps route by, and the links an entity declares in `reach_links`), and any link at all of a `parties` record (a
customer, a supplier). The moves are counted as they run, in the merge's transaction; for anyone else the merge is
taken back whole, and an administrator's says how many it moved (`reach_moved`). An app whose code reads records as the
system by a link to a mergeable entity (a portal's invoices by their customer) declares that link in the entity's
`reach_links`; `scripts/reach_links.py` finds one it did not declare (`docs/registry/reach-links.md` lists those that
decide no one's reach, with why).

**Actions any reader presses.** An action needs exactly its declared `permission` on the record, or a rule naming it.
`permission: read` on an action that may write (one not `read_only`) is allowed only when
`docs/registry/read-actions.md` says which it is: **A** the presser's own part (their enrolment, feed, acknowledgement
or vote, or a tally every reader adds to); **B** an outside person's answer, the handler refusing members
(`ctx.outside`); **C** its record stays as it is (it reads, or makes or changes other records under the presser's own
rights); **D** the handler checks a narrower right itself and changes its own record only where the presser's Write
scope reaches it. Anything else declares `write` or stricter. Each row names the test that presses the action as
someone it must refuse; `scripts/read_actions.py` fails on an action missing from the registry or a row naming a test
its app lacks.

Never a role named in code to decide what a person may do (`if ctx.has_role("Manager")`) where a rule or an action's
`roles` would say it: a workspace renames and splits roles. `ctx.has_role` is for a configured action's `roles` and
the like, which the kernel already checks (`action_gate` in `crates/kernel/src/ctx.rs`). Reviewed, not checked.

## Zones

A zone is where a request arrives, with a rule for who is admitted there: staff (all, none, or limited to some apps),
administrators, step-up ([docs/operations.md](../operations.md), *Zones*). Where a zone limits staff to some apps,
their context is narrowed where it is built: entities of other packages answer as missing, signed-in endpoints of
other apps' packages are refused, roles only other apps ship are dropped, and jobs, events, process runs and
approvals carry the zone of the request that made them ([docs/security.md](../security.md), *Zones*).

What an app owes:

- **Go through the kernel**, so the narrowing applies. An app's own SQL over another app's tables is exactly what a
  zone cannot narrow, which is one more reason never to read another app's tables
  ([11-integrations.md](11-integrations.md)).
- **Gate each report on an entity of its own packages**: `/api/reports/{name}` runs a report only for whoever may read
  the entity it names. `every_report_is_gated_on_its_own_apps_entity` in `server/src/packages.rs` fails a new report
  gated on another app's entity.
- **Keep the zone with the work**: start jobs and processes from the request's transaction with the kernel's calls,
  which record the zone. A task spawned on its own carries none.

Zones do not protect against a compromised replica.

## People outside the workspace

Members are staff (`member`, `support`), outside people are `portal` and `guest` accounts, machines are `service`
accounts ([docs/operations.md](../operations.md), *Zones*). `ctx.outside` is true for a portal person or a guest:
a record's comments, files, assignments, tags and history are never theirs.

**Portal roles** (`portal: true` on a role; [docs/builder/portal.md](../builder/portal.md)). Portal people
hold only such roles. A portal role's rule reaches its holder's own records (by `if_owner`, `owner_fields` or `via`)
to read, write, create and submit, and the actions it names; or it reads reference data whole (an entity with no
personal data and no email or phone field). Never delete, cancel, export or import; never `*`; never an
administrator. The server refuses anything else when the rule is saved (`PermissionDef::portal_problem`). Creating
or changing a record that would end outside the person's reach is refused. A transition is a portal person's to
take only when it names one of their portal roles.

**An app's portal pages** (`shell.portal` in the manifest; [docs/plugins.md](../plugins.md), *Portal pages*): drawn
by the app's own interface at `/portal/{id}/`, calling the app's own endpoints. Portal people hold no roles, so those
endpoints find what is theirs by `ctx.email` (proven by the code they signed in with), then read or change it as the
system after that check, rate limited. Helpdesk's "Your tickets" and ERP's "Your invoices" are the examples, and both
answer `404` for what is not the person's ([docs/api.md](../api.md), *App endpoints*). A portal person who signs up
themselves takes a portal role the workspace offers ([docs/api.md](../api.md), *Public sign-up*).

Outside people hold the app's pages to the same accessibility and words as members
([18-accessibility.md](18-accessibility.md), [20-writing-for-people.md](20-writing-for-people.md)).

## Sharing

| What | How | Where |
|---|---|---|
| A comment or a file shared with outside people | `shared: true` on it; a portal rule's `shared: read` or `add` says what its holders see | [docs/api.md](../api.md), *Collaboration* |
| One thing, one record, for whoever holds a link | a signed action link: `Kernel::issue_link(ctx, tx, entity, id, &LinkSpec { actions, show, params, expires_at, uses, recipient, .. })` in the app's transaction; `Kernel::revoke_action_links(tx, ctx, entity, record, Some(action) or None)` | [docs/api.md](../api.md), *Signed action links* |
| A page anyone with a link opens | `shell.public` below one of the app's routes; no bar, no person, no referrer, kept out of search engines | [docs/plugins.md](../plugins.md), *Public pages* |
| A published list | a workspace's `publish_list` customization | [docs/api.md](../api.md), *Customization* |

## Approvals and second-person rules

**The second person.** A workflow transition (`WorkflowTransition`) with `not_creator: true` may not be taken by
whoever created the record; `not_creator_of: [field or table.field]` by whoever raised the linked records ("whoever
raised an order does not approve paying it"). An approval policy never lets the requester approve unless it
says `allow_requester: true`; a step may say `not_earlier_deciders` and `not_who_took: [transitions]`.

**Approvals** (`ApprovalPolicy`; [docs/api.md](../api.md), *Approvals on a transition*): steps in order, each with
its `approvers` (a role, a field of the record naming people, or an expression), how many must agree (`need`: a
number, `all`, `any`, or an expression), `when`, reminders and escalation on a business calendar. Taking the
transition makes a request; the record stays where it is until the policy is met, then the transition completes
once, as the system on the requester's behalf, in the deciding transaction. `reset_on` fields void the decisions so
far when they change; `lock` stops other edits while it is pending.

An app ships default policies for its own documents in `package.yaml` (`approvals: [{entity, on: submit | cancel |
{action: name}, policy}]`) or in its workflow's transitions; a workspace switches them off or edits them, and a
release that changes one flags the workspace's edited copy. `DocHook::approval_decided` hears the outcome once, in the
deciding transaction; an error there is logged and audited, and the decision stands.

**When the rule is the app's own.** GRC accepts a risk above the tolerance only by a risk approver who does not own it
(`src/risks.rs`, tested in `tests/grc_it.rs`). A rule like that, which no metadata says, lives in a hook's `action` or
`validate` and in the scenario test, with its refusal's words. Prefer the metadata's (`not_creator`, an approval
policy) where it can say it: a workspace sees and edits those.

## Asking again: step-up

A workspace may name entities whose changes, deletions, exports or reads need the person to have proved who they
are in the last few minutes (`step_up`; [docs/operations.md](../operations.md), *Asking again before sensitive
work*). It is checked where every record operation asks for its rights, after them (`Ctx::require` calls
`check_step_up`). The answer is `403` with error `step_up` and `ways`; the bar's runtime asks the person and sends the
refused request again, for every page of the workspace's host.

What an app owes: go through `Ctx::require` or the kernel's record calls in endpoints that touch records, so the
rule applies; a live document's host says its entity (`DocHost::entity`) so each change is asked. An app's own
endpoint that reads or writes records with its own SQL and never asks `require` skips step-up. Its interface handles
the `step_up` refusal as any other ([17-the-interface.md](17-the-interface.md)): sending it again is safe, because the
refusal comes before anything is done.

## The workspace's access policy

Beside roles, a workspace sets controls an app's code must not get around (each in
[docs/operations.md](../operations.md) and [docs/security.md](../security.md)):

| Control | What the app owes |
|---|---|
| `record_reads`: reads of named entities recorded on the audit trail | read those entities through the kernel (`get`, `list`, reports, exports), or call `ferrum_kernel::controls::record_read(tx, ctx, entity, Read::One(id))` in the same transaction for a read of its own; a read the kernel does not see is not recorded. An entity holding sensitive records declares `record_reads: open` (or `all`) in its definition, so they are recorded where the workspace set nothing |
| Staff networks, zones, idle sessions | nothing beyond going through the kernel: they are decided where the request's context is built |
| `internal` entities and field groups toward AI providers (`Egress`) | mark pay, health and personnel entities `internal`; put sensitive fields in groups |
| An account restricted or closed (`ctx.read_only`) | nothing changes; reading and exporting still work. `Ctx::require` refuses changes; an app's own writes check `ctx.read_only` |

## Support access

A workspace's administrators give support access in layers (`health`, `diagnostics`, `assistance`, `incident`),
each an API key that runs as an account of its own with its own role, through every check anyone's request goes
through, never an administrator ([docs/support-access.md](../support-access.md)). An app gets nothing special to do
for it, as long as its endpoints check rights as above.

What an app may add: how its own part is doing, in the support report, with `HookRegistry::add_health(package, …)`
(`ferrum_kernel::support::Health`). It is asked only of workspaces that installed the package, in a transaction of
its own; only counts and problems in words pass (20 at most, each cut at 200 characters), so a health key reads no
record through it. IoT Edge reports how many gateways went silent.

## The audit trail

The kernel records every document operation with who did it, the version and the difference
(`crates/kernel/src/audit.rs`): creates, updates, submits, cancels, deletes, actions (before the handler runs, with
their parameters), transitions, approvals. Each record's trail is hash-chained (`GET /api/v1/{E}/{id}/audit` →
`{events, chain_valid}`), and each day is anchored. A secret field's value, a secret action parameter and a vault
value never reach it.

What an app records itself, in the same transaction as what it records:

| Call | Category | For |
|---|---|---|
| `audit::record(tx, ctx, entity, record_id, action, version, diff)` | `records` | an app's own operation on a record that the pipeline did not write (a direct write a person caused) |
| `audit::record_admin(…)` | `administration` | the kernel's own changes of people, roles and settings; not for apps' records |
| `audit::record_access(tx, ctx, action, details)` | `access` | a read of a marked entity, through `controls::record_read` rather than directly |

Drive keeps an activity log of its own for what people do with files ("Link Created", "Shared With Guest"), listed in
[docs/registry/mutation-paths.md](../registry/mutation-paths.md) as *records its own audit or activity entry*.

## Checked by

- `crates/kernel/src/ctx.rs` (`Ctx::require`, `row_scope`, `action_gate`) and `crates/kernel/src/doc.rs`: rights,
  scopes, `404` for records out of scope, on every record operation.
- `crates/meta/src/types.rs` (`PermissionDef::portal_problem`) and the catalog: portal roles' limits; a third party's
  roles never grant `*` (bundle check).
- `server/src/packages.rs`, `every_report_is_gated_on_its_own_apps_entity`: reports gated on their own app's entity.
- The app's scenario test: each role allowed what it should and refused what it should not, with the refusal's kind
  and words (the test kit's `refused(result, "forbidden" | "not found" | "invalid", says)`, [docs/testkit.md](../testkit.md)).
- `scripts/support_check.py` (against a running server): a health-only support key on every way in.
- Reviewed, not checked: an app's own endpoints filtering by reach and answering `404`; no role names in code;
  elevation only for the engine's part; reads of marked entities going through the kernel.

## See also

- [04-data-and-records.md](04-data-and-records.md): field groups, `owner_fields`, workflows.
- [06-events-jobs-and-logic.md](06-events-jobs-and-logic.md): who a job, an event and a process act as.
- [12-privacy-and-data-duties.md](12-privacy-and-data-duties.md), [13-security-rules.md](13-security-rules.md).
- [docs/security.md](../security.md), [docs/builder/portal.md](../builder/portal.md),
  [docs/support-access.md](../support-access.md), [docs/operations.md](../operations.md) (*Zones*, *Asking again
  before sensitive work*, *Recording who read what*).
