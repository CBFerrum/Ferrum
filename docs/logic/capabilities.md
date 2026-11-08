# Business capabilities

A capability is something a business process, a simulation, an API client or an agent can ask Ferrum to do:
read an order, submit a request, run an app's action, notify a person, send an e-mail. Each one says what it
does to the world, what rights it needs, how a repeat is recognised and what "done" means. Customers' logic
names capabilities; it never declares them, so a solution cannot call a dangerous action harmless, and asking
for a capability grants nothing (`crates/kernel/src/capability.rs`, `crates/logic/src/capability.rs`).

## Where they come from

Nothing is typed in twice. A workspace's capabilities are read from what it has installed:

| From | Capabilities | Effect |
|---|---|---|
| Every entity | `read`, `list` | query |
| Every entity but a ledger | `create`, `update`, `delete`; `submit` and `cancel` when it is submittable | transactional action |
| Each action its package declares | `{action}` | query when the action is read-only, otherwise transactional action, unless its contract says another |
| Each transition of its workflow | `transition.{action}` | transactional action |
| The platform | `ferrum.notify` (in-app), `ferrum.mail` (queued with the outbox) | transactional action; external effect |

An id is `{package}.{entity}.{operation}`: `procurement.material_request.make_purchase_order`,
`selling.sales_order.submit`, `stockcap.hold.transition.approve`. Its version is its package's, and its digest
covers everything it promises. A ledger offers none: its engine posts it, and the API reads it through reports.
An action marked `hidden` (meant for code) is left out unless its app writes it a contract.

An app says more about an action with a contract:

```yaml
actions:
  - name: place
    label: Place
    permission: write
    contract:
      completion: The stock is held          # what "done" means
      compensate_with: release               # another action of the same entity that undoes it
      returns: {type: object, properties: {held: {type: integer}}}
      # deprecated: place_v2 replaces it from 1 March 2027   # on its way out, and what replaces it
      # effect, idempotency, retention_days and unknown_outcome are derived when left out
```

A contract that contradicts its action is refused when the package loads: a read-only action that changes
something, a writing one that calls itself pure, an undo that is not another action of the entity. The record
operations' names (`create`, `read`, `list`, `update`, `delete`, `submit`, `cancel`, `amend`) cannot be action
names.

A record's id, in what a capability takes and returns, names the kind of record it is (`"x-ferrum-link":
"Order"`), as link fields do: a process passing the id of one kind where another's is wanted is refused when it
is checked, while an id written as text is checked when it runs (FLC-COMP-002). An action marked `deprecated`
stays served to releases bound to it; checking a release that uses it says so, with what replaces it
(FLC-CAP-008). What runs on an app's capabilities is listed with `GET /api/app/studio/dependents?package=`
before the app is taken out, and recorded in the audit trail when it is: its steps then wait for it.

Events a process can start on or wait for are listed beside them: each entity's own (`created`, `updated`,
`deleted`, `submitted`, `cancelled`, `transitioned`) and the business events a package declares under `events:`.

## The gate

Every use goes through one gate, `Kernel::invoke`, which runs the owning app's operation along the path a
person's request takes, so every hook, rule, row scope, field permission and audit entry applies.

- **A change needs a key.** A capability that changes something takes a logical effect key: the same for every
  attempt at one effect, never an attempt's own. Its receipt commits in the transaction of the change.
- **A repeat is answered, not repeated.** The same key with the same input returns what was done
  (`repeated: true`), shown as the caller may see it now. The same key with other input is refused
  (`input_changed`). Two requests racing with one key make one change.
- **Who asks comes from their sign-in.** An input naming a workspace, a role or an administrator flag is
  refused, unless the capability's own input declares that field as data.
- **A version is a promise.** Asked for a version other than the installed one, the gate refuses rather than
  run something else (`capability_unavailable`).
- **A failure leaves nothing.** If the operation fails before the commit, neither the change nor the receipt
  is kept, and the same key can try again.

Receipts are kept per workspace and environment. Nothing removes them yet; how long each is kept is part of
its capability (`retention_days`).

## Refusals

| Code | When | HTTP |
|---|---|---|
| `schema_invalid` | The input is not the shape it should be, or a change came without a key | 422 |
| `forbidden` | The caller may not use it, or the input tried to say who is asking | 403 |
| `capability_unavailable` | Not installed here, not at that version, or a ledger | 409 |
| `input_changed` | The key was used with other input | 409 |
| `not_found`, `conflict`, `unavailable` | As every other request in Ferrum | |

The package's other codes (`stale_approval`, `outcome_unknown`, `recovery_required` and the rest) come with
durable processes; `crates/logic/src/refusal.rs` lists them all.

## Asking for them

| Who | How |
|---|---|
| The builder, an API client | `GET /api/app/studio/catalog?q=&entity=&effect=`, `GET /api/app/studio/catalog/{id}`, `POST /api/app/studio/catalog/{id}/invoke` with `{input, effect_key?, version?}` or an `Idempotency-Key` header |
| An agent | The MCP tools `list_capabilities` and `invoke_capability` |
| Ferrum's own code | `Kernel::catalog`, `Kernel::capability`, `Kernel::invoke`, `Kernel::invoke_tx`, `Kernel::events` |

All three describe a capability the same way, digest for digest (`crates/studio/tests/catalog_it.rs`).

## Not yet

- External effects stop at the outbox: `ferrum.mail` is queued once, and the mail server's delivery is not
  confirmed. A step whose answer was lost is looked up by its receipt (docs/logic/processes.md, Recovery).
