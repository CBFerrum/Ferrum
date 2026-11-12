# Expressions

Record expressions are CEL ([docs/api.md](../api.md), "Entities") everywhere one is accepted: rules,
computed fields, `default_expr`, `visible_if`, `required_if`, `read_only_if`, an action's `condition` and
`refuse_if`, effect values, automation conditions, service targets and approval policies (a step's `when`, its
approvers' and escalation's `expr`). A transition's effects read what it was given as `params`; a rule `on:
transition` and an automation on `transitioned` read `transition` (BD-19). Besides CEL's own functions
(`size`, `contains`, `startsWith`, `endsWith`, `matches`, `int`, `string`, `timestamp`, `duration`...) they
have the ones below (decision [BD-16](decisions.md#bd-16-expression-functions-are-additions)). The browser
does not evaluate any function call: a form leaves a condition that uses one to the server.

## Dates

A date is a date field (`2026-11-12`), a date-time field (`2026-11-12T09:30:00Z`, or without a zone) or a
CEL timestamp. A function that gives a date back gives it in the form it was given; an empty date (null or
`''`), or an empty number of days, gives null rather than failing. Each can be called as a method too:
`doc.issued_on.addMonths(12)`.

| Function | What it gives | Example |
|---|---|---|
| `addDays(d, n)` | `n` days on (back when negative) | `addDays(doc.due, 30)` |
| `addMonths(d, n)` | `n` calendar months on, kept within the month: 31 Jan + 1 is 28 or 29 Feb | `addMonths(doc.issued_on, doc.valid_months)` |
| `addYears(d, n)` | `n` years on, 29 Feb kept within February | `addYears(doc.joined, 1)` |
| `startOfMonth(d)` | the first of its month, as a date | `startOfMonth(today)` |
| `endOfMonth(d)` | the last of its month, as a date | `doc.due <= endOfMonth(today)` |
| `weekday(d)` | 1 Monday ... 7 Sunday | `weekday(doc.visit) < 6` |
| `daysBetween(a, b)` | days from `a` to `b` (negative when `b` is earlier) | `daysBetween(doc.ordered, doc.delivered) > 14` |
| `monthsBetween(a, b)` | whole months from `a` to `b`, as `addMonths` counts them | `monthsBetween(doc.hired, today) >= 6` |
| `year(d)`, `month(d)`, `day(d)` | its parts, as numbers | `month(doc.posting_date) == 3` |
| `formatDate(d, pattern)` | the date as text, by a chrono pattern | `formatDate(doc.due, '%d %B %Y')` gives `12 November 2026` |

## Text

| Function | What it gives | Example |
|---|---|---|
| `lower(s)`, `upper(s)` | in lower or upper case, any language (null stays null) | `lower(doc.stage) == 'check'` |
| `s.lowerAscii()`, `s.upperAscii()` | the same for A to Z only (CEL's) | `doc.code.upperAscii()` |
| `s.trim()` | without spaces at either end | `doc.name.trim() != ''` |
| `size(s)` | its length in characters (CEL's) | `size(doc.pin) == 6` |
| `s.contains(t)`, `s.startsWith(t)`, `s.endsWith(t)` | whether it holds, starts or ends with `t` (CEL's) | `doc.email.endsWith('@gov.in')` |
| `s.replace(a, b)` | every `a` replaced by `b` | `doc.phone.replace(' ', '')` |
| `s.substring(from, to?)` | the characters from `from` up to `to` | `doc.number.substring(0, 3)` |
| `s.indexOf(t)` | where `t` first is, or -1 | `doc.title.indexOf(':') > 0` |
| `s.split(sep)` | a list of the parts | `doc.tags.split(',')` |
| `list.join(sep?)` | a list of texts as one | `doc.stages.map(s, s.stage).join(', ')` |
| `padStart(s, n, fill?)`, `padEnd(s, n, fill?)` | filled to `n` characters (with spaces unless `fill`) | `padStart(string(doc.seq), 5, '0')` |
| `concat(a, b, ...)` | text from parts: numbers written out, empty parts left out | `concat(doc.first, ' ', doc.last)` |
| `s.format(list)` | `%s`, `%d`... filled from the list (CEL's) | `'%s needs %d more'.format([doc.name, doc.short])` |

## Empty

An empty number reads as 0 and an empty tick as false in every expression, so `doc.qty > 0` works on a new
record; that does not change. To tell a field left empty from a 0 typed in, give the field directly to one of
these (`doc.qty`, `old.qty`, `links.plan.price`, `linked.price`; not a line of a list read inside `map` or
`exists`, whose empties read as 0):

| Function | What it gives | Example |
|---|---|---|
| `isEmpty(x)` | true for a field left empty, null, `''` or an empty list | `isEmpty(doc.valid_months) ? null : addMonths(doc.issued_on, doc.valid_months)` |
| `coalesce(x, y, ...)` | the first that is not empty, or null | `coalesce(doc.discount, links.customer.discount, 0)` |

## Settings

`settings.<Entity>.<field>` is a field of the workspace's settings: the one record of an entity of the `settings`
kind, the workspace's own (`define_entity` with `"kind": "settings"`) or a package's alike (Support Settings, CRM
Settings). Until that record is first saved, each field reads as its `default` (or empty). Decision
[BD-17](decisions.md#bd-17-a-workspaces-settings).

```
doc.amount <= settings.XClaimPolicy.limit
addDays(doc.received, settings.SupportSettings.auto_close_days)
```

* **Everywhere a record expression is**: rules (a line's too), computed fields, `default_expr`, `visible_if`,
  `required_if`, `read_only_if`, an action's `condition` and `refuse_if`, workflow guards, effect values,
  automation conditions and action values, service targets and approval policies. Logic solutions keep their own parameters and do
  not read `settings` (nor do aggregate filters).
* **Read as the workspace**: a rule over a setting holds for someone who may not open the settings. A secret
  field is never read.
* **Named, checked when saved**: only `settings.<Entity>.<field>` (not `settings` or `settings.<Entity>` whole, not
  `settings['X']`); the entity must be a settings entity with one record for the workspace (not one per company)
  and the field one of its fields and not a secret. Otherwise the customization, automation or action is refused
  naming it: `settings.XPolicy.colour: Claim Policy has no field colour`.
* **Fresh**: each place that evaluates (a save, an action's checks, each effect, an automation's run) reads the
  settings its expressions name once, in its transaction, and none when they name none; nothing is cached, so a
  change is seen by the next evaluation on every server.
* The browser does not read settings: a form leaves a condition that reads them to the server, as it does a
  function call.

## Messages

A rule's `message` and a `refuse_if` `message` may show values from the record with MiniJinja, as print
templates do: `"{{ doc.title }} is still waiting for {{ doc.waiting_for }}"`. They see `doc` as the person
told may read it: a field they may not read shows as nothing. A message without `{{` or `{%` is shown as
written, and so is one that cannot be rendered. A rule's `field` names the field its refusal is shown on
(by default, the rule's name).
