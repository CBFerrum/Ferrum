# Government as a pack (11 November 2026)

Can the second industry app be delivered as configuration alone? The app measured is `ferrum-app-government`
0.3.0 ("Government": permit types with their documents and review stages, applications reviewed stage by
stage and paid through ERP, permits issued, revoked, expired and renewed, appeals, cases with deadlines, a
public register and a citizens' portal; about 1,000 lines of Rust in `permits.rs`, `cases.rs` and `site.rs`,
two bespoke pages). It was measured on paper the day before ([government-retail-on-paper-2026-11-10](government-retail-on-paper-2026-11-10.md):
16 same, 11 different, 11 cannot of 38). Since then the builder gained `invoke` (BD-13), `links.<field>`
with `append_line` and `set_line` (BD-14), service targets (BD-04), a workspace's own menu marked for the
portal (BD-12, BD-02), published lists (BD-15) and lines within lines (BD-06). This rebuilds the app with the
calls an administrator makes, on Ferrum main at c5f5214 with ERP's real packages, and measures it again,
behaviour by behaviour, on a running kernel. No Rust, SQL or plugin is in the pack.

- The pack: [packs/government/government.pack.json](../../../packs/government/government.pack.json)
  (`ferrum.pack/1`, 224 KB: 42 customizations, 5 roles, 3 automations, 1 Studio space, 236 `InterfaceWord`
  reference records; requires core, parties, products, accounting, selling and Studio).
- The builder and the restated scenarios: [crates/studio/tests/government_pack_it.rs](../../../crates/studio/tests/government_pack_it.rs).
  It builds the pack in a workspace with ERP, fails unless the export is byte for byte the committed file,
  installs the committed file in a fresh workspace with ERP, and walks every step of the app's
  `tests/government_it.rs` as officers, a services manager, a case officer and portal people holding the pack's
  roles, plus six steps the app's test does not take (marked "beyond the app's test"). Its comments mark 39
  steps same, 18 different, 2 cannot and 1 not exercised.
- Not done: no browser (the portal page, the records pages, the space and the public register's HTML page were
  checked through the kernel and Studio's site handler only); the public register was read with
  `Kernel::published_list` and its CSV, not over `/api/public/…`; no file was uploaded (the harness writes a
  stored file's address into a document row); nothing ran on a cluster. The harness ages records by SQL where
  the app's test moved its upkeep's clock, and runs the schedule scanner a day ahead, as grc_pack_it does.

Convention, as for Governance and Risk: **same** means the same outcome and the same kind of refusal, the words
the platform's or the administrator's where they say the same thing (said where they differ); **different**
means a person notices, including when a refusal's words lose what the app's said; **cannot** means one
capability is missing; **not exercised** means the test suite does not reach it, whatever the reading says.

## What the pack is

| Part | What it holds |
|---|---|
| Entities (9, one defined twice) | `XPermitType` with `XPermitDocument` and `XPermitStage` lines; `XPermitApplication` with `XApplicationDocument` and `XApplicationReview` lines; `XPermit`; `XCaseCategory`; `XCase`. `XPermit` is defined, then redefined once applications exist, because a link to an entity not yet defined is refused and the two name each other. |
| Checks (17) | stage names once; what was applied for kept after submission; type open and every document given on submission; a passed stage is one it waited for; an inspection says what it found; every stage passed and the fee paid before approval; a rejection, an appeal, a decision on appeal and a revocation each say why; appealable and in time; renewable and in its window; a resolved case says how. |
| Configured actions (13) | Submit It (twice: with and without a fee), Take It Up, Pass This Stage, Approve It, Reject It (twice: with and without a stage waiting), Appeal the Rejection, Decide the Appeal, Issue the Permit, Withdraw It; Revoke It and Renew It on permits. |
| ERP | Submit It with a fee calls `parties.customer.create` (keyed once per address) and `selling.sales_invoice.create` (keyed once per application, a draft) through the capability gate; the approval reads the invoice through `links.fee_invoice`. |
| Menu | `x_government` "Government", landmark, #4338ca, quick create of an application and a case, the app's assistant notes, `portal: true`. |
| Published list | `permits`: number, type, holder, issued, expires, status, conditions of permits on the register, CSV. |
| Roles | Permit Officer, Case Officer, Services Manager as the app's; **Applicant**, a portal role reaching applications by `applicant_email`, permits by `holder_email` and cases by `citizen_email`, reading active permit types and the case categories; plus a spurious "Accounts Manager" (below). |
| Automations (3) | 00:00 each permit's standing and what its type says (register flags, months kept, renewal days); 00:05 renewal reminders; 00:15 open cases saved again (overdue). |
| Studio | A space "Government" with the officers' four queues. |

## Behaviours

| # | Behaviour | Paper | Measured | What a person notices, or the one capability missing | Why it moved |
|---|---|---|---|---|---|
| G1 | Permit Type: fields, unique name, documents and stages as lines | same | **different** | The type's form has a Company field (G8), an "On the Register for Good" box (G33) and a computed Step column in its stages (G12). | The paper did not see the price of G8, G12 and G33 on this form. |
| G2 | No two stages with the same name | same | **different** | "Check" and "check" are both taken; the app folds case. Missing: lower-casing in expressions (CEL's string functions are not enabled). | Exercised: the app's own step uses two spellings. |
| G3 | Application: fields, `APP-{YYYY}-{seq:5}`, read-only fields | same | **same** | One field more, Appeal Until, which the app's portal worked out. | |
| G4 | A submitted application keeps what was applied for | same | **same** | A check comparing with `old`. | |
| G5 | The type must be active to submit | same | **same** | A check reading `links.permit_type.active`. | |
| G6 | Submit needs every document the type lists, each with a file | cannot | **different** | Refused, but "still needed: every document its permit type lists, each with its file" does not name them (the app: "still needed: Site plan, Proof of ownership"); names match exactly, not in any case. Missing: a refusal's message worked out from the record. | `links.<field>` reads the type's lines (BD-14). |
| G7 | Submit sets status and date | same | **same** | | |
| G8 | Fee invoiced in ERP from settings, with currency | cannot | **different** | The invoice is ERP's own, a draft, raised once through the gate. The company and the fee's item are each permit type's, not the workspace's settings (missing: settings by configuration); the currency is left to ERP's default (not exercised: the app passes the company's). | `invoke` (BD-13). |
| G9 | Customer found by email, or made from the applicant | cannot | **different** | Made once per address by the effect's key, so a citizen's second fee goes to the same customer. A customer ERP already had under that address is not found (a second is made), and another name under the same address refuses the submission with "the key government-customer:… was used for parties.customer.create with other input: use a new key for a new request". Missing: find-or-create (a read whose answer decides the next effect). | `invoke` with a key. |
| G10 | Approval refused until the fee invoice is paid | different (inferred) | **same** | A check reading the invoice's `docstatus` and `outstanding_amount` through `links.fee_invoice`, in the app's words. | Exercised with ERP: draft, submitted unpaid, paid. |
| G11 | Take It Up records the officer | same | **same** | | |
| G12 | "Waiting for" is the first stage group not passed; approval refused until all are | cannot | **different** | Groups, order and "Fire safety, Health check" are as the app's, the groups worked out from "Alongside The One Before" by a computed Step on each stage line (a line sees its place and its record's other lines). Different: Waiting For names three stages, then "and N more" (CEL cannot join a list), and approval is refused with "every review stage is passed first", not the stages' names. | `links` over the type's lines; `append_line`. |
| G13 | Each stage is passed by the role its row names | cannot | **different** | Refused as forbidden, as the app, with fixed words, "the stage is passed by someone holding the role its row names" (the app: "Fire safety: a Fire Officer passes it"). Missing: a message from an expression. | `refuse_if` reads `links` and `user.roles`. |
| G14 | Not the same person as the stage before or beside | cannot | **same** | Forbidden, in the app's two sentences. | Steps on the stage lines and `refuse_if` over the reviews. |
| G15 | An inspection stage requires findings | cannot | **same** | Invalid, "a site inspection says what it found". | A check over the reviews and the type's lines. |
| G16 | Passing appends a review row (stage, reviewer, day, note) | cannot | **same** | The findings too; the review lines are read-only to people. A rejection while a stage waits adds one Failed row naming the stages it waited for (the app adds one row each). | `append_line`. |
| G17 | Approve with note, conditions, who and when | same | **same** | | |
| G18 | Reject needs a reason | same | **same** | A check (the server does not hold an action's `required` parameters; the pack's check refused the empty reason). | |
| G19 | Appeal once, within the type's days | same (inferred) | **same** | Appeal Until is kept on the rejection; past it, "the days to appeal it have ended" (the app adds the day, which the record shows); 0 days: "may not be appealed"; twice: "'Appeal the Rejection' is not available". | Exercised, including the window. |
| G20 | Appeal decided by someone not in the decision | different | **same** | Forbidden, in the app's words. | `refuse_if` over the reviews and deciders. |
| G21 | An overturned appeal approves, fee checked | different | **same** | Approved with the appeal's officer and conditions; refused while the fee is unpaid. | One action whose effects depend on the outcome parameter; the fee check is a check on any move to Approved. |
| G22 | Issue creates the Permit; expiry from the type's months | different (inferred) | **same** | Calendar months, the day kept unless the month is shorter, worked out in CEL; a renewal runs on from the day after the old permit ends. The end-of-month case was not exercised. | `create_document` with `links`. |
| G23 | Permit: fields, numbering, read-only | same | **same** | Status is computed from its facts (revoked, past its day), not stored by upkeep. | |
| G24 | Revoke with reason and day | same | **same** | | |
| G25 | Daily expiry | same | **same** | The midnight run saves each permit again. | |
| G26 | Renewal reminder once, N days before, N per type | different | **same** | The offset is read from the type (`links` in an automation's condition); told once. | `links` in automation conditions. |
| G27 | Renew makes a draft; refused outside the window or when one is live | different (inferred) | **same** | From the portal by its holder only; before the window "it may be renewed from its type's days before it ends" (the permit shows Renew From); while one is live "'Renew It' is not available" (the permit shows its renewal); a type that does not renew refuses. | Exercised. |
| G28 | Applicant and holder told as it moves, per their preference | different | **different** | Told at each step in the app's words, in the workspace only: `notify` sends an in-app notification of the automations' kind, not by e-mail and not as a kind the person chooses how to receive; the issue's notice does not give the dates. Missing: a workspace's own notification kind with channels. | |
| G29 | Case due date from the category's days; resolution required | same | **same** | Computed from the day it opened; resolved, it keeps the day. | |
| G30 | Daily overdue flag | same | **same** | Computed, and the 00:15 run saves open cases again. | |
| G31 | Three roles | same | **same** | Plus the portal role, which the app's portal did without. | |
| G32 | Erase and retain rules per status | cannot (inferred) | **cannot** | Asked for a citizen's data, the platform finds her ERP customer only, none of her applications, permits or cases; nothing is erased or retained by status. Missing: personal-data declarations for a workspace's own entities, in a pack. | Exercised. |
| G33 | Public register without sign-in, publish flags per type, CSV | cannot | **different** | The published list `permits` answers without sign-in with the kiosk's permit, holder and conditions, and not the types off the register; a CSV. Different: a type's flags reach its permits at the midnight run, not at once (a published list and a computed field read the record's own fields, so the permit keeps copies); "kept for good" is a box of its own (an emptied number reads as 0, so empty months mean "off at once"); no register of one type (`?type=`); the CSV is headed by labels, the JSON's keys are the fields' names (`register_holder`). The HTTP route and the HTML page were not exercised. | `publish_list` (BD-15). |
| G34 | Portal "Your services": apply, upload, submit, appeal, renew | cannot | **different** | Studio's records page of the menu marked for the portal: her applications (with Create), permits and cases (with Create); she applies, submits, withdraws, appeals, renews and raises cases with the buttons that are hers, sees nothing of anyone else's, and an officer's button is forbidden to her. Different: generic list and form instead of "Your services"; she types her name (an expression knows the person's email and roles, not their name); a document is replaced by editing its row; no hourly limit on drafts and cases; the type's list of documents is not shown beside her rows. Not exercised: uploading a file, any page in a browser. | Portal menus (BD-12, BD-02). |
| G35 | Tile, quick create, assistant | different | **different** | As GRC's U3–U5: the menu's tile with the app's icon and colour, quick create of an application and a case, the assistant's notes; the tile opens the records pages. | |
| G36 | Officers' queues page | different | **different** | A Studio space with the four queues, each person seeing those they may read; at most 50 rows a list (the app: 500). | |
| G37 | Arabic and Hindi words | different | **different** | 118 of the pack's 132 texts have the app's word in each language; the 14 without are the pack's own (Step, Appeal Until, Renew From, Holder, On the Register for Good, Last Day, On Register and seven hidden copies of the type). Checks' and refusals' words, descriptions and the space's labels stay in English. | |
| G38 | Uninstall | different | **not exercised** | A pack has no uninstall (BF-PACK-003); the suite does not try. | |

## Counts

38 behaviours: **23 same, 13 different, 1 cannot, 1 not exercised** (paper: 16 same, 11 different, 11 cannot).

| Moved | Behaviours |
|---|---|
| cannot → same | G14, G15, G16 |
| cannot → different | G6, G8, G9, G12, G13, G33, G34 |
| different → same | G10, G20, G21, G22, G26, G27 |
| same → different | G1, G2 |
| different → not exercised | G38 |
| unchanged | same: G3, G4, G5, G7, G11, G17, G18, G19, G23, G24, G25, G29, G30, G31; different: G28, G35, G36, G37; cannot: G32 |

The paper was right that the review pipeline was the product, and wrong about how much of it the new builder
holds: everything it does to one application (order, groups, roles from data, the same person twice, findings,
review rows, appeals) is met; what is lost is mostly words (a refusal cannot say which document, stage or role)
and two spellings of a name.

## Missing platform capabilities, by the behaviours each blocks

| Rank | Capability | Blocks (cannot) | Forces (different) | Total |
|---|---|---|---|---|
| 1 | **Personal-data declarations for a workspace's own entities**, in a pack: which field matches a person, what is erased or retained by status, and why | G32 | | 1 |
| 2 | **A refusal's message from an expression** (a check's `message`, a `refuse_if`'s), so it can name what is missing | | G6, G13, (G12) | 3 |
| 3 | **String functions in expressions**: lower-casing and joining a list (CEL's strings extension) | | G2, G12, (G6: names in any case) | 3 |
| 4 | **Fields of a linked record where only the record's own are read**: computed fields and published lists reading `links`, so a permit need not copy its type nightly | | G33 | 1 |
| 5 | **Settings by configuration** (GRC's rank 4 again): one record that rules and effects read | | G8, (G1) | 2 |
| 6 | **Find-or-create through the gate**: an effect whose run depends on a read before it (or a condition on an effect) | | G9 | 1 |
| 7 | **A workspace's own notification kind** with channels a person chooses | | G28 | 1 |
| 8 | **A portal page of a pack's own**, and the person's name in expressions | | G34 | 1 |
| 9 | A screen of a pack's own; list blocks above 50 rows | | G35, G36 | 2 |
| 10 | Words for the pack's own texts, descriptions and messages | | G37 | 1 |
| 11 | An empty whole number that reads as empty in expressions | | G33, (G1) | 1 |
| 12 | Uninstalling a pack (BF-PACK-003) | | (G38) | |

## What a user would notice

- **A citizen** applies, submits, appeals, renews and raises cases on generic records pages under a
  "Government" entry of the portal, typing her own name. She is told in the workspace, not by e-mail. When a
  document is missing she is told something is, not which. If she applies under another spelling of her name
  with the same address, her submission is refused in words about keys and capabilities.
- **An officer** passes stages, approves, rejects, decides appeals and issues permits with the app's buttons
  and nearly all of its refusals; a stage passed by the wrong role says so without naming the role; Waiting For
  shortens past three stages side by side; the queues page is a space of 50 rows a list.
- **A services manager** sets the company and fee item on each permit type, ticks "On the Register for Good"
  instead of emptying the months, sees a Step column in the stages, and finds "Check" and "check" both
  accepted. A change to what the register shows reaches the register the next morning.
- **Whoever answers a citizen's data request** finds nothing of hers but her ERP customer.

## Also found while building

- **Size.** 42 customizations (10 entity definitions, 17 checks, 13 configured actions, a menu, a published
  list), 4 roles of its own, 3 automations, 1 space, 236 word records. 18 expressions are longer than 500
  characters and 7 longer than 1,000. The longest, a permit's expiry in calendar months, is 2,479 characters of
  the 4,096 an expression may hold: CEL has no date type, no "add months" and no formatting, so a date is
  taken apart with `getFullYear`, `getMonth` and `getDate`, the month's length worked out by hand, and put back
  together with zero padding. Waiting For is 1,977 characters, written twice (Take It Up, Pass This Stage); the
  stage a press passes is a 961-character expression repeated inside three `refuse_if` and one effect. No one
  would type these in a designer screen: the test generates them from a dozen small Rust functions
  (`months_from`, `waiting`, `chosen` …), and a screen would need the same: named, reusable sub-expressions.
- **Computed fields are worked out in the alphabetical order of their names** (`EffEntity.exprs.computed` is a
  `BTreeMap`), each seeing only those before it. The permit's fields were named so that `expires_on` comes
  before `last_day`, `on_register` and `status`. Nothing in docs/api.md says so.
- **Computed fields see no `old`** (`doc.rs` evaluates them with `old: None`), but they do see the stored value
  of the record's own fields, so a field that refers to itself keeps its first value: a case's Resolved On is
  `status is resolved ? (doc.resolved_on != null ? doc.resolved_on : today) : null`.
- **An empty whole number reads as 0 and an empty check as false** in every expression (`cel_view`), so
  `doc.n == null` is never true, and a null copied from a linked record by `set_fields` is stored as 0. The
  type's "empty months: kept for good" needed a box of its own.
- **An effect has no condition**: Submit It is two actions (with a fee: customer, invoice, status, notice;
  without one: status, notice) and Reject It is two (with a stage waiting it adds a Failed review), each pair
  shown one at a time by mutually exclusive conditions. The person sees one button; the administrator keeps
  two in step.
- **An action's `required` parameters are not held by the server**; each "says why" of a button is a check of its
  own (4 of the 17). Nine checks compare with `old` (seven of them "on moving from X to Y", `old.status == X &&
  doc.status == Y`), because a `refuse_if` refuses as forbidden and the app's refusals there are invalid.
- **Two records that link to each other are defined twice** (a link to an entity not yet defined is refused).
- **Automations write no read-only field** (they run as their administrator): the permit's status became
  computed, and the copies and Reminded On are hidden but writable fields. Designed around from GRC's reading,
  not exercised here.
- **The capability gate's refusal reaches a citizen in its own words** (G9): accurate for an administrator,
  meaningless to the person pressing Submit It.
- **What carried over without effort**: `links` over the type's lines, `append_line`, `refuse_if` over
  `user.roles`, `invoke` with keys, the portal role's `via`, the published list and the portal menu all worked
  as documented the first time they were tried. The whole scenario passed on the fourth run; of the three
  failures before it, two were the platform's (the space's 50-row cap, the empty number reading as 0) and one
  the harness's.

## Verdict

**Not yet: keep the app, and retire it once personal-data declarations reach a workspace's own entities.**
That is the one "cannot" left (G32), and a town hall cannot run without answering a citizen's data request
with what it keeps and why. Everything a permit, an application or a case does on its own, and everything the
review pipeline does across stages, people and ERP, is met: 23 behaviours the same, including the fee, the
appeal, renewal and the reminders, with the stage logic the paper thought would stay code. The 13 differences
are ones a customer could choose to accept, at a cost in words: refusals that do not name what is missing
(rank 2), names compared in one spelling and Waiting For cut at three (rank 3), a register a day behind its
types (rank 4), settings per type (rank 5), in-app notices only (rank 7) and generic portal pages (rank 8).
Ranks 2 and 3 are small and would turn G2, G6, G12 and G13 back into "same".

What stays code even so: nothing of the app's behaviour. What stays hard by configuration is the date
arithmetic and the stage logic as expressions of one to two and a half thousand characters, which only a
generator (or named sub-expressions in the builder) makes maintainable.

## Platform faults seen while building (not fixed)

- `crates/kernel/src/pack.rs:647` (`export_roles`): a role several packages seed (ERP's Accounts Manager, seeded
  by accounting, selling and buying) is compared with one package's manifest only, so the rules the others seed
  are exported as rules the workspace added. The Government pack carries an "Accounts Manager" role with
  selling's rules (sales invoices, orders, delivery notes) that no administrator gave it.
- `crates/meta/src/effective.rs:144`: empty numbers and checks read as 0 and false, so a null can never be told
  from 0 in a check or computed field, and a null copied through an effect is stored as 0 (above).
- `crates/meta/src/effective.rs:46` and `crates/kernel/src/doc.rs:1521`: computed fields evaluated in name
  order, undocumented (above).
- The `input_changed` refusal (`the key … was used for … with other input: use a new key for a new request`) is
  shown to whoever pressed the button, a portal person included.
