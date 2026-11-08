# Customer rules and solutions

A solution is a customer's own logic, written in the builder, by an agent or by hand: rules on records,
decision tables, processes and the tests that pin them down, with the capabilities it uses. This page is
about how a solution becomes something that runs, and how its rules run in every save of their records.
Processes run on the durable runtime (W3); until an installation has it, a solution with a process is refused
with `unsupported_profile` (decisions L-06).

Code: the source and its compiler in `crates/logic` (`solution.rs`, `compile.rs`, `decision.rs`, `expr.rs`,
`rules.rs`), pure so that the builder's simulator and the server run the same evaluator; releases, reviews,
activation and the rules' hook in `crates/kernel/src/logic.rs`.

## A solution's source

JSON, read strictly: a key twice in one object, an unknown key anywhere, a document over 1 MiB or nested past
64 levels is refused before anything else looks at it. Nothing in it refers to anything to fetch.

```json
{
  "schema_version": "ferrum.solution/1", "kind": "Solution",
  "id": "acme.orders", "version": "1.0.0", "label": "Order rules",
  "parameters": {"large": {"type": "decimal", "value": "1000"}},
  "rules": [
    {"id": "band", "entity": "Order", "phase": "derive", "when": "has(doc.total)",
     "writes": {"x_band": "doc.total > parameters.large ? \"large\" : \"small\""}},
    {"id": "positive", "entity": "Order", "phase": "validate", "check": "doc.total > dec(\"0\")",
     "message": "An order has a total above zero", "field": "total", "audit_refusals": true},
    {"id": "within_credit", "entity": "Order", "phase": "validate", "when": "has(doc.party)",
     "lookups": [{"name": "party", "link": "party", "fields": ["credit_limit"]}],
     "check": "doc.total <= lookups.party.credit_limit", "message": "This order is over the party's credit limit"}
  ]
}
```

Expressions are CEL's syntax with Ferrum's types (decisions L-02): exact decimals, money and quantities,
`unknown` kept apart from `false`, no clock, network or randomness, a step budget. Decision tables, processes,
human tasks and tests are described in `crates/logic/src/solution.rs`; `ui.layout` is where the builder draws
things and is the one part outside every digest.

## Compiling

`Kernel::compile_solution` checks a source against the workspace without keeping anything, and gives every
fault at once, each with a stable code and a JSON pointer (`/rules/2/check`, `/processes/0/nodes/4/input/id`):

* **names and versions**: ids, `major.minor.patch` versions, a label for every step;
* **capabilities**: each one used is asked for in `requires`, installed at exactly that version, and runs
  where it is used (a query step only reads, an action step changes something, a person's decision is a task);
* **types**: every expression and input mapping, money in one currency, quantities in one unit; parameters
  of the type they say;
* **graphs**: one entry, every route to a step that exists, every step reachable, no cycle, a step budget at
  least the graph's size, and a step reads only the outputs of steps that surely ran before it;
* **effects and recovery**: an effect's key is the same for every attempt at one activation; an effect that can
  end unknown says where that goes; neither a failure, an unknown outcome nor a lapsed task leads to success;
  the requester never decides their own request;
* **rules**: a derivation fills only the customer's own fields (`x_…` fields and custom entities), never the
  platform's (`company`, `owner`, `docstatus`, …) or an app's; a rule reads no restricted field (in a permission
  group, or a secret) of its record or of a linked one; one writer per field; derivations that read what
  another fills run after it, and two that wait for each other are refused;
* **decision tables**: unique, first or collect with a reducer, an explicit default, overlapping rows,
  unreachable rows and gaps found where ranges make it possible ("coverage unproven" where expressions do
  not); suggested tests at each boundary;
* **admission**: the solution's own tests are run, and must pass.

A release comes out: the source's digest (its meaning, without layout), an artifact digest over the compiler's
version, the profile, the checked definitions, the capabilities each at its digest, and the fields its rules
read or fill with their types (the lock).

## Releases, review and activation

| Step | Who | What is checked |
|---|---|---|
| `save_release` | an administrator | it compiles; a version is kept once and never changes (a database trigger refuses an update) |
| `review_release` | an administrator who is not its author | the digest they were shown is the release's; one decision each; a rejection says why |
| `activate_release` | an administrator | approved by someone other than its author, of exactly its digest, within 30 days, and rejected by nobody; compiled again against the workspace as it is now, it comes out with the same digest (a capability or field it uses has not changed); its rules run beside every other active release's (one writer per field, no circle, nothing an app's own hooks fill); the generation the caller saw is still the one that runs |
| `deactivate_solution` | an administrator | the generation the caller saw |

One release per solution runs. Activating or stopping one bumps the workspace's metadata version, so every
replica reads its rules again. Each step is in the audit trail.

## How rules run

`TenantRules` is a document hook on every entity, registered by `Kernel::boot`, so a save through the API,
`/api/v1`, an agent's tools, an import, a bulk change, a capability's invocation, an automation, a job or a
plugin meets the same rules (`crates/kernel/tests/logic_rules_it.rs` sends one invalid order through each).

1. The app's own hooks fill what they fill.
2. Every active release's derivations of the entity run, in the order their reads need, each seeing what the
   ones before it wrote.
3. Validations run: the metadata's rules, the app's guards, then the active releases' validations, against
   the record as it would be stored (`old` is the stored version).

A rule reads its record's unrestricted fields, `user.id`, `user.email` and `user.roles`, `today` and `now`
(handed in by the kernel, read once per save), its release's parameters, and the linked records it declared:
at most four per rule and eight per save, only the fields named, read with the saver's rights before any rule
runs. A saver who may not read a linked entity is refused before the rule is evaluated.

A refused save writes nothing. A check that does not hold refuses with the rule's own message, which is plain
text (nothing from the record is put into it), followed by where it came from:

```
total: An order has a total above zero [acme.orders 1.0.0, rule positive]
```

A value or a check that cannot be worked out (`unknown`: a missing amount compared, money without a currency)
refuses too, saying it could not be checked: a rule that cannot run as it was approved does not run some other
way, and does not quietly not run. Guard optional fields with `has()`.

A rule that sets `audit_refusals` keeps evidence of each refusal in `logic_refusal`, written in a transaction
of its own, since the save's rolls back: when, which release and rule, which entity and record, who, and
whether the check was false or unknown. It keeps no copy of what was refused. `Kernel::logic_refusals` lists
them for administrators.

If what an active release was approved against changes (a field it reads or fills is removed or changes type,
or two active releases come to fill one field), its entity's saves are refused with `stale_approval` naming
the release, until a new version is compiled, reviewed and activated, or the solution is stopped.

## Chains of events

Work that answers an event (an app's event handler, an automation, a plugin, a job one of them queued) writes
events one step further down the chain than the event it answers, whatever wrote them (`outbox.depth`, carried
in every transaction from `db.rs`'s `Trace`). The dispatcher stops an event before any handler runs when it is
more than `chain_depth` steps (8) from the change that started its chain, or when that change has been followed
by more than `chain_events` events (1000), and keeps it as a dead event that says why and how the chain went:

```
stopped: this event is 8 steps from the change that started its chain (at most 8). Chain event-41:
Alpha.created → Beta.created → Alpha.created → … Something answers its own changes: correct the rule,
automation, plugin or app that does, then replay this event.
```

A request's or a job's own events are step 0, so an import of ten thousand rows is one step, not a loop.
Replaying a stopped event starts a chain of its own. Automations keep their own, shorter limit (three), and an
automation stopped by it records why.

## Limits

* Links are typed as text ids: a link to the wrong kind of record is not caught before running.
* A rule cannot loop, and solutions have no for-each yet; cycles are refused, so iteration is unbounded nowhere.
* Plugins' own hooks that fill fields are not compared with rules at activation; apps' hooks are.
* Writing the evidence of a refusal takes a second database connection while the save's is held.
* Processes, their runs, tasks and recovery come with W3; the builder's screens with W4.
