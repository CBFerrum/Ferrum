# Government and Retail against the builder, on paper (10 November 2026)

Governance and Risk was rebuilt as a pack and measured on a running kernel
([findings](grc-as-a-pack-2026-11-10.md)). This is the cheaper measurement for the next two candidates
of the plan's proof step: each app's behaviours read from its source (`ferrum-app-government` 0.3.0,
`ferrum-app-retail`) and judged against the builder as [docs/api.md](../../api.md) describes it at
Ferrum `92c480b`. **Nothing was built or run.** Rows marked "inferred" rest on a reading of the
documents. Its purpose is the list at the end: what the builder needs next, by the behaviours it blocks.

Same: a person gets the same thing. Different: they would notice. Cannot: one platform capability is
missing, named in the last column.

## Government (38 behaviours): 16 same, 11 different, 11 cannot

Measured on a running kernel the next day, as a pack: 23 same, 13 different, 1 cannot, 1 not exercised
([government-as-a-pack-2026-11-11](government-as-a-pack-2026-11-11.md)).

| # | Behaviour | Verdict | Missing |
|---|---|---|---|
| G1 | Permit Type: fields, unique name, documents and stages as lines | same | |
| G2 | No two stages with the same name | same | |
| G3 | Application: fields, `APP-{YYYY}-{seq:5}`, read-only fields | same | |
| G4 | A submitted application keeps what was applied for | same | |
| G5 | The type must be active to submit | same | |
| G6 | Submit needs every document the type lists, each with a file | cannot | a rule over a linked record's lines |
| G7 | Submit sets status and date | same | |
| G8 | Fee invoiced in ERP from settings, with currency | cannot (money) | ERP's steps called from configuration |
| G9 | Customer found by email, or made from the applicant | cannot | a create-if-absent effect |
| G10 | Approval refused until the fee invoice is paid | different (inferred) | |
| G11 | Take It Up records the officer | same | |
| G12 | "Waiting for" is the first stage group not passed; approval refused until all are | cannot | linked lines, ordered and grouped |
| G13 | Each stage is passed by the role its row names | cannot | a role named by data |
| G14 | Not the same person as the stage before or beside | cannot | G12's structure |
| G15 | An inspection stage requires findings | cannot | a flag on a linked line |
| G16 | Passing appends a review row (stage, reviewer, day, note) | cannot | an effect that appends a line |
| G17 | Approve with note, conditions, who and when | same | |
| G18 | Reject needs a reason | same | |
| G19 | Appeal once, within the type's days | same (inferred) | |
| G20 | Appeal decided by someone not in the decision | different | |
| G21 | An overturned appeal approves, fee checked | different | |
| G22 | Issue creates the Permit; expiry from the type's months | different (inferred) | |
| G23 | Permit: fields, numbering, read-only | same | |
| G24 | Revoke with reason and day | same | |
| G25 | Daily expiry | same | |
| G26 | Renewal reminder once, N days before, N per type | different: the offset is written, not read from the type | |
| G27 | Renew makes a draft; refused outside the window or when one is live | different (inferred) | |
| G28 | Applicant and holder told as it moves, per their preference | different | |
| G29 | Case due date from the category's days; resolution required | same | |
| G30 | Daily overdue flag | same | |
| G31 | Three roles | same | |
| G32 | Erase and retain rules per status | cannot (inferred) | personal-data declarations in a pack |
| G33 | Public register without sign-in, publish flags per type, CSV | cannot | public pages bound to any entity |
| G34 | Portal "Your services": apply, upload, submit, appeal, renew | cannot | a screen of a pack's own |
| G35 | Tile, quick create, assistant | different | |
| G36 | Officers' queues page | different | |
| G37 | Arabic and Hindi words | different | |
| G38 | Uninstall | different | |

## Retail (40 behaviours): 17 same, 6 different, 17 cannot

Eight of the seventeen are postings in ERP: six of stock, two of money.

| # | Behaviour | Verdict | Missing |
|---|---|---|---|
| R1 | "Is a store" on Warehouse | same | |
| R2 | Collect order: fields and numbering | same | |
| R3 | Order submitted, collected once, not also shipped | same (inferred) | |
| R4 | Random six-digit pickup code | different | |
| R5 | Ready only if the store holds every line | cannot | a read of stock balances |
| R6 | Bring from another store: a transfer for the shortfall | cannot | create a record with derived lines |
| R7 | Hand over only against the code | same | |
| R8 | Hand over makes and submits a delivery note | cannot (stock) | ERP's steps called from configuration |
| R9 | Cancel buttons | same | |
| R10 | Store shipment: fields and numbering | same | |
| R11 | Store defaults to the one holding the most | cannot | a read of stock balances |
| R12 | Ship It posts a delivery note from the store | cannot (stock) | ERP's steps |
| R13 | Not shipped twice, closed, or with nothing left | different (inferred) | |
| R14 | Return: fields, reason, lines | same | |
| R15 | Return quantity at most sold less already credited, per item | cannot | sums per item across records |
| R16 | Invoice issued, not a credit note; store of the company | same (inferred) | |
| R17 | Take It Back credits the invoice | cannot (money) | ERP's steps |
| R18 | Goods return to stock at original cost | cannot (stock) | ERP's steps |
| R19 | A taken return is frozen | same | |
| R20 | Transfer: fields, lines, from is not to, one line per item | same | |
| R21 | Send: the store must hold it; stock moves to transit | cannot (stock) | ERP's steps |
| R22 | Arrival moves stock from transit to the store | cannot (stock) | ERP's steps |
| R23 | Part arrival per item, remaining quantity | cannot | an effect on a line chosen by a parameter |
| R24 | Write off the rest, saying why | cannot (stock) | ERP's steps |
| R25 | Reject and cancel need a reason | same | |
| R26 | A transfer is frozen after the request | same | |
| R27 | Franchisee agreement: dates and percentages | same | |
| R28 | Royalty, marketing and total from the rates | same | |
| R29 | Report inside the agreement, sales not negative | same | |
| R30 | No overlapping periods per franchisee | cannot | an aggregate filter on the record's own fields |
| R31 | An invoiced report is frozen | same | |
| R32 | Invoice It raises the ERP invoice, once | cannot (money) | ERP's steps |
| R33 | Portal "Your franchise" | cannot | a screen of a pack's own |
| R34 | Stock by store report | cannot (inferred) | a report over stock balances |
| R35 | Erasure declarations | cannot (inferred) | personal-data declarations in a pack |
| R36 | Two roles | same | |
| R37 | Tile and four quick creates | different | |
| R38 | Staff home page | different | |
| R39 | Words | different | |
| R40 | Uninstall | different | |

## What is missing, by the behaviours it blocks

| Capability | Government | Retail | Blocked | Did Governance and Risk need it |
|---|---|---|---|---|
| ERP's money and stock steps called from a button or an automation, once | G8 | R8, R12, R17, R18, R21, R22, R24, R32 | 9 | no |
| Expressions and aggregates over a linked record's lines, and aggregate filters on the record's own fields | G6, G12, G14, G15 | R15, R30 | 6 | partly |
| A portal or public page of a pack's own, bound to its entities | G33, G34 | R33 | 3 | yes |
| A read of stock balances (and a report on them) | | R5, R11, R34 | 3 | no |
| An effect on the record's own lines (append one; change the one a parameter names) | G16 | R23 | 2 | partly |
| Create a record with derived lines, or create one only if absent | G9 | R6 | 2 | no |
| Personal-data declarations in a pack | G32 | R35 | 2 | no |
| A role named by data | G13 | | 1 | no |

## What stays code whatever the builder gains

* Retail's return of goods through a return note against each original delivery note, newest first, at
  the original cost (`returns.rs`).
* Government's walk over stage groups (`permits.rs`): a small state machine over data, with the acting
  role read from a row.
* An upload of several megabytes through a public page, and the guard against spreadsheet injection in
  a public CSV (`site.rs`).

## Verdicts

* **Government**: its cases, permit types, permits, expiry, revocation and roles could be a pack today.
  The review pipeline (G12 to G16) is the product; it waits for linked lines in expressions, an effect
  that appends a line and a role named by data, and the fee for ERP's steps. Keep as code until then.
* **Retail**: keep as code. Half of what it cannot do is postings in ERP and most of the rest reads
  stock. Its franchise part (R27 to R33) could move once ERP's steps can be called and a pack has a
  portal page.

## What this changes in the order of work

The first two rows block fifteen behaviours across two apps and neither needs an engine: they are the
next two things built after service targets, ahead of checklists and scheduling.
