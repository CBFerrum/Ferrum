# Governance and Risk as a pack (10 November 2026)

Can an industry app that is Rust code today be delivered as configuration alone? The app measured is
`ferrum-app-grc` 0.1.14 ("Governance and Risk": seven entities and a settings record, three roles, five
document hooks, six actions on two entities, a daily upkeep, about 500 lines of Rust, one bespoke page). It
was rebuilt with the calls an administrator makes, on Ferrum main at 660efe5, with nothing else: one
customization batch (8 custom entities, 15 checks), 11 role rules for 3 roles, 4 scheduled automations,
one solution of 3 rules (reviewed by a second administrator), and one Studio space. No Rust, SQL or plugin
is in the pack.

- The pack: [packs/grc/grc.pack.json](../../../packs/grc/grc.pack.json) (`ferrum.pack/1`, 23
  customizations, 3 roles, 4 automations, 1 Studio item, 1 solution, no records; requires Studio).
- The builder and the restated scenarios: [crates/studio/tests/grc_pack_it.rs](../../../crates/studio/tests/grc_pack_it.rs).
  It builds the pack, fails unless the export is byte for byte the committed file, installs the committed
  file in a fresh workspace, releases and activates its solution there (a second administrator), and walks
  every step of the app's `tests/grc_it.rs` as people holding the pack's roles: 20 steps marked same, 10
  different, 3 cannot (each "cannot" asserts what happens today).
- Not done: no browser walk of the pack's pages (the space and the generic record pages were checked
  through the kernel only); nothing ran on a cluster; the harness ages records by SQL where the app's test
  moved its upkeep's clock (computed fields and automation conditions read the real day).

Convention: **same** means the same outcome and the same kind of refusal (the words may be the
platform's, said where they differ); **different** means the intent is met another way a user notices;
**cannot** means configuration cannot do it today.

## Behaviours

| # | Behaviour | In the app | In the pack | What is missing |
|---|---|---|---|---|
| R1 | Risk register: fields, choices, labels, list columns, `RSK-{YYYY}-{seq:4}` | entity YAML | **same**: `define_entity` with numbering | |
| R2 | Inherent, current and residual scores, likelihood × impact, read-only | hook `before_validate` | **same**: computed fields | |
| R3 | Level of the current score (≤4 Low, ≤9 Medium, ≤16 High, else Critical) | hook | **same**: a computed select | |
| R4 | Likelihood and impact from 1 to 5 | hook ("is from 1 to 5") | **same**: field bounds ("must be ≤ 5") | |
| R5 | Controls never make a risk worse; the target no worse than now | hook | **same**: two checks | |
| R6 | Mitigating names its controls | hook | **same**: a check over the lines | |
| R7 | Accepted above the tolerance only with an approved acceptance | hook, SQL | **same**: solution rule with an `exists` aggregate | |
| R8 | An approved acceptance past its day no longer holds a risk, even before upkeep | hook compares `expires_on >= current_date` | **cannot**: still held until the midnight automation marks it Expired | `today` (a relative date) in an aggregate's filter; filters take literal values only |
| R9 | The tolerance is a settings record a Risk Manager edits | `GrcSettings` | **different**: a solution parameter (6); changing it is a new release an administrator saves and a second one reviews | workspace settings by configuration that checks, computed fields and rules can read |
| C1 | Controls: fields, unique name, list | YAML | **same** | |
| C2 | How long a test is current, workspace-wide | `GrcSettings.test_every_days` | **different**: a field of each control (default 365) | as R9 |
| C3 | Control tests are records of their own: a list across controls, filtered by control, sorted by day | `ControlTest` | **different**: tests are lines of their control, recorded by saving the control; no list of all tests | the standing is computed from the control's own lines because nothing can take the latest of other records (aggregates count, sum, exist) or write to another record (C9 needs one of them) |
| C4 | Several evidence files per test | `ControlEvidence` lines of a test | **cannot**: one evidence title and one file per test | lines within lines (a child entity holds no table) |
| C5 | Tested only once in place | hook, SQL | **same**: a check on the control comparing its lines with `old` | |
| C6 | An effective result has its evidence (a file) | hook | **same**: a check on the line | |
| C7 | Tested today or before | hook | **same**: a check with `today` | |
| C8 | The tester is whoever records it, read-only | hook | **same**: read-only, `default_expr: user.email` | |
| C9 | Last tested on and last result: the latest test by day, the last entered that day | hook after save, SQL | **same**: computed fields over the lines (CEL `filter`/`all`) | |
| C10 | Effective while the latest test was effective and is still current | hook | **same**: computed with `timestamp`/`duration` and `today` | |
| C11 | Upkeep marks stale controls as no longer effective | `maintenance` | **same**: a daily automation (00:10) saves each effective control again | |
| A1 | Acceptances: fields, `ACC-{YYYY}-{seq:4}` | YAML | **same** | |
| A2 | Its last day is after today while drafted | hook | **same**: a check | |
| A3 | Only a Risk Approver (or an administrator) accepts, rejects or revokes | action, `Forbidden` | **different**: refused as invalid with the app's words | a refusal of kind forbidden from configuration (checks always answer invalid) |
| A4 | A risk's owner does not approve taking it on | action, `Forbidden` | **different**: as A3, the owner copied by `fetch_from` | as A3 |
| A5 | Decisions are buttons shown only when they apply (Accept It, Reject It, Revoke It) | three actions with conditions | **cannot**: deciding is choosing a status in a select and saving | configured actions on custom entities (an action needs a hook); a workflow gives buttons, but its state is not a field rules, lookups, aggregates or automations can read, and a transition within one document status runs no checks, derivations or automation event, so R7 could not count a workflow-held decision |
| A6 | The status is read-only, moved only by decisions and upkeep | read-only, set by actions | **different**: an editable select whose moves (Draft to Accepted or Rejected, Accepted to Revoked or Expired) and movers are held by four checks | as A5 |
| A7 | The approver and the day decided are recorded | action | **same**: computed fields that keep their first value | |
| A8 | Revoking records the day revoked | action | **different**: the day decided is kept (another computed field could record it; not built) | nothing |
| A9 | Approving puts the risk's treatment to Accept | action writes the risk | **cannot**: the manager sets Accept afterwards, which the approval now allows | an automation or rule effect that sets fields of a linked record |
| A10 | After its last day an accepted acceptance is Expired | upkeep | **same**: a daily automation at 00:00 with `doc.expires_on < today` | |
| A11 | Expiry opens the risk again unless another acceptance holds it | upkeep | **different**: a daily automation at 00:05 marks accepted risks and a derivation opens those no acceptance holds; only above the tolerance (the app opens any accepted risk) | as A9: the marker field and the derivation stand in for writing the risk |
| A12 | Revoking opens the risk again at once | action | **different**: at the next 00:05 recheck, up to a day later | as A9 |
| A13 | A rejected acceptance leaves the risk as it is | action | **same** | |
| P1 | Policies: fields, unique title | YAML | **same** | |
| P2 | Publishing: the next revision when the text changed, the text and day kept | action | **different**: tick "Publish It" and save; a derivation fills revision, text, day and status and unticks it | configured actions (A5) |
| P3 | Nothing new to publish is refused | action | **same**: the derivation sets a hidden marker a check refuses | |
| P4 | What was published stays until published again | action | **same** | |
| P5 | Retiring a published policy | action | **different**: tick "Retire It" and save | as P2 |
| P6 | A Policy Reader sees only published policies | role `where` | **same** | |
| P7 | "I Have Read It" records my acknowledgement of the current revision | action creating a record as the system | **different**: the reader creates the acknowledgement herself (a create right on her own); revision, person and moment are filled and read-only | as P2: an action that creates a record |
| P8 | Once per person and revision | action ("you acknowledged revision 1 already", invalid) | **different**: a unique key (409 "another Policy Acknowledgement already has this Policy and Revision and Person") | a key's own message |
| P9 | A retired policy is not acknowledged | action condition (not found) | **different**: the link refuses it, invalid ("cannot be used here") | as P2 |
| P10 | A reader sees her own acknowledgements, a manager all | role `if_owner` | **same** | |
| F1 | Findings: fields, `FND-{YYYY}-{seq:4}` | YAML | **same** | |
| F2 | Past its day, a finding is overdue | upkeep | **same**: computed, and a daily automation (00:15) saves open ones again; also on any save after the day | |
| F3 | Closed, it says how it was fixed | hook | **same**: a check | |
| F4 | Closed, it is not overdue | hook | **same**: computed | |
| X1 | Three roles with the app's rights | `package.yaml` | **same** (the reader also creates her own acknowledgements, P7) | |
| X2 | An approver creates no risk and records no test; a reader creates no policy | roles | **same** (a test is a change to the control, C3) | |
| U1 | Its own page at `/grc/`: risks worst first, controls to test, open findings, published policies | `ui/src/Home.tsx` | **different**: a Studio space shared to the three roles with the same four lists; risks by score rather than level then score, no coloured level badges; each person sees the lists they may read | a list block sorted by two fields and showing a select as a badge (small) |
| U2 | Every record's list and form | the records frame | **same**: generic record pages in both (the pack's were not opened in a browser) | |
| U3 | An app tile for the three roles, with icon and colour | `shell.app` | **different** (was cannot): the menu `x_grc`'s tile, same icon and colour, for whoever reads one of its entities (the three roles here, and administrators, as the app's); it opens the records pages at `/data/x_grc/`, not a page of its own | a screen of its own (U1) |
| U4 | Quick create of a risk and a finding | `shell.create` | **same** (was cannot): the menu's `create`, offered to those who may create them (a Risk Manager), opening a new record's form | |
| U5 | What the assistant knows of its records, and the questions it suggests | `shell.assistant` | **same** (was cannot): the menu's `assistant`, the app's text with the pack's entity names, on `/data/x_grc/` pages, where `/open` sends its records | |
| U6 | A menu module "Governance and Risk" grouping its records | `modules` in `package.yaml` | **same** (was cannot): `define_module`; the records frame shows it once the web pages filter on `scope.modules` (with one menu, as now, they show it already) | |
| U7 | Its names and words in Arabic and Hindi | `ui/src/locales` | **different** (was cannot): 188 `InterfaceWord` records, the app's own words for 94 of the pack's 106 texts in each language; 12 texts, descriptions, the home space's labels and everything the server writes stay in English | the app's words for those texts; words for descriptions and Studio labels |
| U8 | Reports and notifications | none | **same**: none | |
| U9 | Uninstall ("ask") and versioned upgrades | app manifest, migrations | **different**: no uninstall; an upgrade is a newer pack installed over it (redefinitions under their guards) | uninstalling a pack (BF-PACK-003) |

## Counts

58 behaviours: **33 same, 16 different, 9 cannot**. Of the app's own test, 33 marked steps: 20 same, 10
different, 3 cannot (A9 twice, R8). Everything the app computes and refuses about one record at a time came
over; what fails is everything that acts across records, everything that is a button, and everything the
app contributes to the shell.

## Missing platform capabilities, by the behaviours each blocks

| Rank | Capability | Blocks (cannot) | Forces (different) | Total |
|---|---|---|---|---|
| 1 | **Configured actions on records**: a button with a role or permission, a condition, a refusal that can be forbidden, and effects (set its record's fields, create a record as the system), whose effects rules and automations see | A5 | A3, A4, A6, P2, P5, P7, P9 | 8 |
| 2 | **Shell contributions in a pack**: an app tile with roles, quick create, the assistant's instructions, a menu module | U3, U4, U5, U6 | | 4 |
| 3 | **Writing a linked record**: an automation action (or rule effect) that sets fields of the record a link names, through its checks | A9 | A11, A12, C3 (with rank 5) | 4 |
| 4 | **Workspace settings by configuration**: a single record that checks, computed fields and rules read | | R9, C2 | 2 |
| 5 | **Aggregates over time and order**: `today` and relative dates in filters; the latest record by a field | R8 | C3 | 2 |
| 6 | **Words in a pack** (labels and choices in other languages) | U7 | | 1 |
| 7 | **Lines within lines** (a child entity with a table) | C4 | | 1 |
| 8 | A unique key's own message | | P8 | 1 |
| 9 | Uninstalling a pack (BF-PACK-003) | | U9 | 1 |
| 10 | Studio list blocks: two sort fields, a select shown as a badge | | U1 | 1 |

Ranks 1 and 3 are where the app's Rust earns its keep: its six actions, and what they and the upkeep
write to other records.

## What a user would notice

- **Deciding an acceptance is editing a status, not pressing Accept.** The select offers every status; the
  wrong ones are refused on save with the app's words, as an invalid save rather than "not allowed".
- **Approving does not accept the risk.** The Risk Manager must set the risk's treatment to Accept after
  the approval. Revoking leaves the risk accepted until the next morning's recheck (00:05); an expired
  acceptance opens its risk five minutes after midnight rather than at the upkeep.
- **Publishing and retiring a policy are tick boxes saved with the form**, and "I have read it" is creating
  an acknowledgement; acknowledging twice says "another Policy Acknowledgement already has this Policy and
  Revision and Person".
- **A test is a row on the control's page**, with one evidence file; there is no list of all tests.
- **The tolerance is no longer the Risk Manager's**: it is in the solution, changed by two administrators.
  How long a test is current is set per control.
- **No Governance and Risk tile, no quick create, no assistant help, no Arabic or Hindi**; the home page is
  a Studio space (where the shell lists it was not checked), risks sorted by score.

## Also found while building

- **Whole numbers in checks and computed fields are unsigned** when they come from a record:
  `doc.test_every_days * 24` fails ("Unsupported binary operator 'mul': UInt(90), Int(24)"); it needs
  `int(doc.test_every_days) * 24`. Every administrator who multiplies a field by a literal meets this.
- **Computed fields are worked out at save only.** A value that depends on the day (effective, overdue,
  expired) is stale until the record is saved again, so the pack keeps three daily automations whose only
  job is to save records again (`set_fields` of `status` to itself) and one that sets a marker field for a
  derivation to act on. It works; it is a workaround an administrator would not find unaided.
- **An exported solution carries the moment its source was reviewed**, so a pack is not reproducible from
  its build. The reference pack sets that moment to the day it was made (the test does this, and says so).
- **Read in code, not exercised:** `set_fields` from an automation skips read-only fields (automations run
  as their administrator, not as the system, and `doc.rs` ignores read-only input from anyone but the
  system); `read_only_if` is compiled but never enforced by the server. The pack avoided both.

## Verdict

**No: the app cannot be retired in favour of the pack today.** One record at a time, the pack is the app:
33 of 58 behaviours are the same, including every score, bound, check, computed standing and the daily
upkeep. Across records and in front of people it is not: approval does not accept the risk, revocation
reopens it up to a day late, an expired acceptance holds its risk until the midnight run, decisions and
publishing are status edits and tick boxes, a test has one evidence file, and the app's tile, quick create,
assistant help and translations are gone.

The shortest platform work that would make it so:

1. **Configured actions on custom entities** (rank 1): a button with roles, a condition and a forbidden
   refusal, whose effects set its record's fields and create records as the system, run through the
   save pipeline so rules and automations see them. This turns A3–A6, P2, P5, P7 and P9 into "same".
2. **An effect that writes the record a link names** (rank 3), from an action and from an automation, with
   that record's checks: approval accepts the risk, revocation and expiry reopen it at once (A9, A11, A12).
3. **`today` in aggregate filters** (rank 5, small): an expired acceptance stops holding a risk at once
   (R8).
4. **A pack's shell section and its words** (ranks 2 and 6): tile, quick create, assistant, module, and
   translations (U3–U7).

With those four, the remaining differences (tolerance as a reviewed parameter, test interval per control,
tests as lines with one evidence file, a key's message, a space for the home page, no uninstall) are ones a
customer could choose to accept; lines within lines (rank 7) and settings by configuration (rank 4) are
needed only to remove them.

## Measured again after configured actions (10 November 2026, later)

The platform gained three of the capabilities above, on branch `bf/actions`: **configured actions**
(`add_action` with `roles`, `refuse_if` and `effects` run as the system through the save pipeline; decision
BD-11), **`update_linked`** (an effect, and an automation action, that writes the record a link names under
its own checks and rules), and **the day in aggregate filters** (`{"today": n}`). Two faults found while
building were fixed on the way: a record's whole numbers are signed in expressions (`doc.days * 24` works),
and `read_only_if` is enforced by the server (it was not: a test sending input to a field whose condition
held saw it written).

The pack was rebuilt to use them and the same scenarios restated
([grc_pack_it.rs](../../../crates/studio/tests/grc_pack_it.rs); the committed pack is regenerated from the
builder and still checked byte for byte). It now holds 23 customizations (8 entities, 9 checks, 6 configured
actions), 3 roles, 3 automations, 1 Studio item and 1 solution of 2 rules. What changed in it:

- **Acceptances**: Accept It, Reject It and Revoke It are buttons for a Risk Approver, refused as forbidden to
  anyone else and, for the first two, to the risk's owner. The status is computed from a hidden, read-only
  decision the buttons set (Expired the day after its last day), so no one types it; four checks that held
  the moves and movers went. Approving writes the risk's treatment to Accept; revoking asks the risk to look
  again, which opens it at once unless another acceptance holds it. The day revoked is recorded.
- **Expiry**: the midnight automation saves an expired acceptance again (Expired) and asks its risk to look
  again in the same run; the 00:05 recheck of every accepted risk went. The rule's `held` counts approved
  acceptances whose last day is today or later, so an expired one stops holding its risk the day it expires,
  before any automation runs. The derivation that reopens a risk no longer looks at the tolerance, as the
  app does not.
- **Policies**: Publish It, Retire It and I Have Read It are buttons. Publishing sets the next revision, the
  text and the day; with nothing new, the check refuses the whole action in the app's words. Acknowledging
  creates the acknowledgement as the system, so a Policy Reader no longer creates records herself, and a
  retired policy is not found by a reader, as in the app. The two tick boxes and the derivation that read
  them went.

### Counts

58 behaviours: **46 same, 6 different, 6 cannot** (were 33, 16, 9). Of the app's own test, 34 marked steps
(the read-only status is now a step of its own): 30 same, 4 different, 0 cannot (were 20, 10, 3 of 33).

| Moved | Behaviours |
|---|---|
| cannot → same | A5 (decisions are buttons), A9 (approving accepts the risk), R8 (an expired acceptance holds nothing before the upkeep) |
| different → same | A3, A4 (forbidden, in the platform's words with the administrator's message), A6 (status read-only), A8 (day revoked), A11 (expiry reopens at the upkeep, unless another acceptance holds it, whatever the tolerance), A12 (revoking reopens at once), P2, P5 (publish and retire are buttons), P7 (I Have Read It), P9 (a retired policy is not found) |

### Still different or impossible

- **Different (6)**: R9 and C2 (the tolerance is a reviewed solution parameter, the test interval a field of
  each control: settings by configuration, rank 4); C3 (tests are lines of their control, rank 5's "latest
  of other records" and lines within lines); P8 (a key's own message, rank 8: "another Policy Acknowledgement
  already has this Policy and Revision and Person"); U1 (a Studio space, rank 10); U9 (no uninstall).
- **Cannot (6)**: C4 (lines within lines, rank 7); U3–U6 (shell contributions in a pack, rank 2); U7 (words in
  a pack, rank 6).
- **Still a workaround**: computed fields are worked out at save only, so the pack keeps daily automations
  that save records again (acceptances, controls, findings). The `today` filter removed the need for one of
  them (the 00:05 recheck), not for the others.
- **Not checked**: the record form was not opened in a browser. Reading the frontend (`FormView.tsx`), it
  shows an entity's actions as buttons from metadata with their conditions, confirmations and parameter
  dialogs, filtered by the action's permission or a role's rule naming it: a configured action appears
  without a frontend change, but `roles` and `refuse_if` are judged only by the server, so a Risk Manager
  sees Accept It and is refused when pressing it.

### Verdict, again

One record at a time and across records, the pack is now the app in 46 of 58 behaviours, including every
decision, its refusals and what it does to the risk. What keeps the app from being retired is no longer in
its Rust: it is the shell (tile, quick create, assistant help, menu module), words in other languages, lines
within lines, and settings by configuration.

## Measured again after a workspace's own menu (10 November 2026, later still)

The platform gained `define_module` (BD-12): one customization giving a workspace a menu with a launcher
tile, quick create and the assistant's notes; packs carry it, and `InterfaceWord` records travel as
reference records. The pack now holds 24 customizations (the menu `x_grc` "Governance and Risk": its six
entities that are not lines, risks first, quick create of a risk and a finding, the app's assistant text
and its four suggested questions). [grc_pack_it.rs](../../../crates/studio/tests/grc_pack_it.rs) checks the
installed menu, that each of the three roles reads one of its entities (so gets the tile), and that only the
Risk Manager is offered the two Create entries.

### Counts

58 behaviours: **49 same, 7 different, 2 cannot** (were 46, 6, 6). The app's own test is unchanged (its
steps do not reach the shell).

| Moved | Behaviours |
|---|---|
| cannot → same | U4 (quick create), U5 (assistant notes and suggestions), U6 (a menu module) |
| cannot → different | U3 (a tile with the app's icon and colour, opening the records pages) |

### Still different or impossible

- **Different (7)**: R9, C2, C3, P8, U1, U9 as before; U3 (the tile opens records pages).
- **Cannot (2)**: C4 (lines within lines); U7 (words in other languages: the pack carries no
  `InterfaceWord` records, and those translate the interface's words, not the pack's entity labels and
  choices).
- **What a menu is not**: a screen of its own (the home page is still a Studio space, U1), a search
  provider in the bar (the app has none either), live counts on its tile.
- **Not checked**: no browser. `/api/shell`, `/data/x_grc/` and `/open` were checked by the server's unit
  tests, not on a running server with this pack; the web pages do not yet filter the records frame by
  `scope.modules` (with one menu it makes no difference).

## Measured again with the workspace's words (10 November 2026, last)

The web interface now reads the workspace's own word (its `InterfaceWord` records in the person's language)
for the name of an entity, field, choice, button, workflow state or form section before the apps' names and
its own catalog (Ferrum-Frontend PR 69). The pack now carries 188 `InterfaceWord` reference records, built
from [packs/grc/words.json](../../../packs/grc/words.json): the coded app's own Arabic and Hindi words
(`ui/src/locales/{ar,hi}.json` and `ui/src/locales/names/{ar,hi}.json`) for the texts the pack defines, and
for nothing else (the test refuses a word for a text the pack does not define). Its requirements now name
core as well, whose entity `InterfaceWord` is. Nothing stood in the way: `InterfaceWord` travels as reference
data as it is, and the pack is 125 KB of its 5 MiB.

**The pack's texts**: 106 (15 entity labels and plurals, 48 field labels, those without a label counted as
the label shown, 33 choices, 8 button labels and confirmations, the menu's label and description; it has no
workflow and no form sections). **Arabic: 94 have a word, 12 do not. Hindi: 94 have a word, 12 do not.** The
12, the same in both: Evidence File, Test Every (Days), Tests, Risk Control, Risk Controls, Recheck, Risk
Owner, Decision, Revoked On, Nothing New, and the confirmations of Revoke It and Retire It: texts the pack
names differently from the app (tests as lines, the test interval per control, hidden fields for decisions
and publishing), for which the app has no word.

[grc_pack_it.rs](../../../crates/studio/tests/grc_pack_it.rs) installs the committed pack and checks that the
workspace's words for `ar` and `hi` (what a page's bootstrap sends) are the 94 of each, among them the words
for Risk and Mitigate in Arabic and Risk Acceptance and Critical in Hindi, and that installing it again adds
none.

**What stays in English whatever the pack carries**: the messages of checks, refusals and configured actions'
refusals; mail and notification text from automations; anything the server writes (a computed field's text,
an audit line); entity and field descriptions (the app translated those as well); the home space's list
labels (U1), which are Studio's, not interface words; and the 12 texts above.

**U7: cannot → different.** A person reading Arabic or Hindi sees the app's words for nearly every name on
the records pages and the menu, but not for the 12 texts, the descriptions or the home space, as the app gave
them. Not checked in a browser.

### Counts

58 behaviours: **49 same, 8 different, 1 cannot** (were 49, 7, 2). The only "cannot" left is C4 (lines
within lines).
