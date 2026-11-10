# A gym built by configuration, walked on a running server (10 November 2026)

The measure of the builder ([the plan](../../plan/builder-first.md), "As revised"): a business nobody
wrote an app for, built by an administrator with no code. The server was built from Ferrum's main at
f5a5fe6 (`--no-default-features --features studio,forms`) with the interfaces of Ferrum-Frontend's
`bf/authoring` (merged as its PR 67), on this machine, port 5195, its own database. Workspaces "Iron Gym
B" (core, ERP's packages) and "Iron Gym C" (the same and Studio). Nothing here ran on a cluster, with a
real mailer or with a second person.

## Through `/api/v1` and the customization API, as the administrator

1. **Entities in one batch.** `POST /api/customize/batch` with five `define_entity` operations (Plan,
   Member, Membership with a lines table of Membership charge, Visit). A dry run answered `{"ok": true}`
   and saved nothing; a batch whose fourth operation named a missing entity was refused with `index` 3
   and saved nothing; the batch then answered `{"applied": 5}`.
2. **Keys.** A second Plan of the same name: 409, naming the field. A second Membership of the same
   member and start date: 409.
3. **Lines and totals.** A Membership with lines of 0.10 and 0.20: total 0.30, count 2, and the 999 sent
   as its total ignored. Replacing the lines recomputed both. The list filtered on a line's field.
4. **A check.** `add_rule` "the end date is after the start date": a Membership that ends before it
   starts, 422 with the rule's message.
5. **A workflow.** `set_workflow` with an approval by a role: a new Membership starts Requested and
   Approve moves it to Approved.
6. **A rule that looks at other records** (workspace with Studio). A solution whose rule on Visit
   declares an aggregate (`exists` over Membership, through `member`, where status is Active): compiled,
   drafted, released; its author could not review it; a second administrator saw its aggregates and
   approved it; activated. A Visit was accepted while the member had an active Membership and refused
   ("This member has no active membership") once all had ended, and accepted again when one was active.
7. **Time.** An automation `{"relative": {"field": "end_date", "offset": "-P3D", "at": …}}` with a mail
   template: the mail was queued at its minute, once, with the record's number and date in the
   template's markup. An automation `{"every": {"period": "day", "at": …, "filter": [["status", "=",
   "Active"]]}}` with `create_document`: one Visit for each active Membership. A schedule on a field
   that is not a date: 422.
8. **Preview.** `POST /api/admin/automations/preview` rendered the template against a Membership; a
   broken template was a field error on `template`.
9. **Redefining.** `redefine_entity` adding a field: old records loaded with their lines and took a
   value in the new field. Changing a date field that holds values into a number was refused: "records
   hold values in it, and a date value does not read as int".
10. **Export and import.** `GET /api/customize/export`, then `POST /api/customize/import` in a second
    workspace: the Membership entity was there with its lines and its rule.

## In the desktop app's browser

11. **Customize** listed the workspace's own entities with List and Design.
12. **Entity designer**: opened Membership (made through the API), added a field "Locker number", saved;
    the field was on the record form. Created "Class" from nothing; created "Work order" with a lines
    table (Description, Amount as Money) and a total "Sum of Lines.Amount". A Work order then saved
    through the API with lines of 120.10 and 79.95 totalled 200.05.
13. **Record form**: the Membership's lines grid, its totals (read-only), the workflow's Approve button.
14. **Workflow designer**: added a state "Ended" and tried to save: "'Ended' can never be reached: no
    action leads to it from the starting state". Added an action End from Approved to Ended: saved.
15. **Automation editor** (Administration, Automations): opened the reminder; it read "3 days before End
    date at 07:31, in the workspace's time zone"; Preview fetched the mail's HTML.
16. **Check builder** (Customize): Status, is not, Ended: "Written as doc.status != 'Ended'"; saved. A
    Membership set to Ended through the API was then refused with the rule's message.
17. **Rules tab** (Studio, the solution's draft): the rule with its aggregate and the notes on whose
    rights each read uses; Check answered "no faults".
18. **375 px**: the entity designer, the workflow designer and Studio's solutions page did not scroll
    sideways.

## What the walk found, fixed before the interfaces merged

* The shell's own records routes did not mount the entity and workflow designers: on a workspace
  without an app's records frame they could not be opened.
* Totals could be typed into on the record form (the server ignored what was typed).
* A redefinition showed as raw JSON in the list of changes.
* After creating an entity the page stayed at `/customize/new`; the move to the entity's own address
  was at first caught by the unsaved-changes guard.
* A new lines table's first field had no name, so the designer refused to save a table nobody had
  touched.

## Walked later the same day

19. **Reports.** Studio's query over Membership grouped by status with a count and a sum of the total,
    and over its lines (sum of the charges' amounts by description): both answered.
20. **The member's portal.** A role reading Membership through `member.email` and Member through
    `email`, marked for the portal; the member as a portal person; signed in with the emailed code (read
    from the outbox). She listed only her own memberships, another member's was not found, her own
    opened with its lines, she could not change it, and an entity her role does not name was closed.
21. **A public sign-up page** (a workspace with Forms). A public form mapped to Member (two questions and
    a fixed value): opened without signing in at `/api/site/{workspace}/forms/f/join`; a visitor's
    answers made a Member record with the name as typed, the address lower-cased and the fixed value.
22. **An ERP invoice from an automation.** "When a Membership is created, create a Sales Invoice" with
    the customer, the date and one line written as an expression: a draft invoice was made with that
    customer and item. Its line's rate came out 0, as it did for the same invoice made through the API:
    ERP prices a line from its price lists.
23. **The whole setup as a pack** (server built from `bf/packs`). `GET /api/admin/pack?records=XPlan`
    from the gym's workspace: 13 customizations, 2 roles, 2 automations, 1 solution, the plans; no
    user's address or token in it; a second export the same but for its time. In a fresh workspace:
    the plan listed what it would create and saved nothing; a wrong digest was refused; the install
    brought the entities, the plans, the automations, the portal role and the solution as a draft. A
    member and a membership with lines were then made there: the key, the exact total, the workflow's
    first state and the check all held. Installing the same pack again changed nothing, and both
    installs were on record.
24. **A button set up by configuration** (server at PR 167). `add_action` "End it" on Membership with a
    condition, a confirmation and two effects (set the record's status and a note; write a date on the
    member the record links to). It appeared on the record with no change to the interface; pressing it
    asked first, then did both; it then disappeared, and a second press through the API was refused.
25. **Changing a button, and the definition a designer starts from** (server at PR 168, through the API,
    12 of 12 steps). `add_action` of the same name replaced the button where it stood and the list of
    changes marked the earlier rows; `remove_action` removed one and refused a missing one;
    `GET /api/customize/definition/XMembership` held the button, the rule and the lines' entity, and
    sent back unchanged as a redefinition it left the entity's description identical.
26. **The buttons screen** (Ferrum-Frontend PR 70). Customize, Membership, Buttons: opened the button
    made through the API with its condition and refusal read into the builder and its two steps as
    cards; changed its label, added a refusal, saved. On a record the refusal holds for, the button was
    shown unavailable with the refusal's message as its description. A new button with a question
    ("Note") and one step writing the answer to a field: saved, pressed on a record, the answer was in
    the field; removed from the screen. The new-button form at 375 px did not scroll sideways.
27. **The designer keeps other screens' changes.** With the button and a rule in place, a field was
    added in the entity designer and saved: the button, its refusals and the rule were still there, and
    the list marked the rows the redefinition now carries as replaced.
28. **Names in Hindi** (Ferrum-Frontend PR 69). Administration, Interface words, "This workspace's own
    names" listed the gym's labels, fields, choices and buttons; words were written for "Memberships"
    and "End it"; with the person's language set to Hindi the list's title and the button read in Hindi,
    and "Status", "Member" and "Approve" read as the interface's own Hindi words.
29. **A menu for the gym** (server at PR 171, through the API, 19 of 19 steps with step 31).
    `define_module` "Gym" with five record types, two of them offered in the + menu, and notes for the
    assistant: the metadata index listed it as the workspace's own, `GET /api/shell` had its tile and
    two Create rows, a menu naming a record type the workspace does not have was refused, and an
    operation that does not parse was refused in the batch's own shape with its index.
30. **Calendars with hours.** `PUT /api/admin/calendars/gym_floor` with two windows on Monday and a
    Saturday until midnight: saved and read back; a window ending before it starts was a field error on
    `hours`; hours on a weekend day were refused. In Administration, Calendars (Ferrum-Frontend PR 71)
    the calendar opened in the dialog, "Copy Monday to the other business days" and Save wrote six days
    of the same windows and kept the holiday.
31. **Governance and Risk installed as a pack** in a fresh workspace with Studio (without Studio the
    plan said so and the install was refused): its tile and its two Create rows were in the bar, 94
    words in each of Arabic and Hindi were the workspace's, and a risk saved through the API came back
    scored 20, 15 and 8 and levelled High by the pack's rules.
32. **Two menus side by side in the browser** (the same pack installed in the gym's workspace). Home
    showed four tiles: ERP, Studio, Governance and Risk, Gym, each menu with its icon and colour. The
    Gym tile opened `/data/x_gym/`: the frame named Gym, only Gym's group in the sidebar, its card on
    Home. The + menu's Member entry opened a new Member there. `/open/XRisk` went to
    `/data/x_grc/e/XRisk`, titled Risk Register, with only that menu's group.
33. **The pack's pages in Arabic.** With the person's language set to Arabic the Governance and Risk
    pages read right to left: the list's title, the sidebar's six record types, the frame's name and
    the search box's label in the pack's words.
34. **The menus screen.** Customize, Menus listed both menus; Gym was opened, given a sixth record type
    and saved, and the index had it. The new-menu form and the calendar dialog at 375 px did not scroll
    sideways.
35. **An ERP invoice from a button, once** (server at PR 173). A button "Invoice it" on Membership
    invoked `selling.sales_invoice.create` with a customer, a company, one line and `submit: true`, with
    a key, then wrote `results[0].number` on the membership. Pressing it raised and submitted one ERP
    sales invoice (250, Unpaid) with its number on the membership; pressing again went through and
    raised nothing new; a capability that is not in the catalog was refused when the button was saved.
36. **Service targets** (server at PR 174, through the API, 15 of 15). On a custom Request entity with
    a target whose allowance follows the priority: a High request started running with a due moment
    and ignored what was sent for the two kept fields; Waiting paused it and cleared the due moment;
    opening it resumed with a later one; a lower priority moved it out; closing met it; re-opening
    started a new round; a priority with no allowance left no target on the record. A request due in a
    minute was marked breached by the minute loop, an automation whose condition reads `old` set a note
    once, and closing it afterwards read `met_late`.
37. **The targets screen** (Ferrum-Frontend PR 72). Customize, Request, Targets listed the target; a
    second one ("Resolution", 2 days counted in the `gym_floor` calendar, met when Status is Closed)
    was made from the screen with both kept fields made for it, and the first target and the entity's
    other fields were as before. A request's form showed the due moment read-only and the state as a
    badge ("Met late").
38. **The step card.** The buttons screen opened "Invoice it" with its step, its inputs, the submit
    tick and the key; saving it unchanged closed with "Nothing changed". Both forms at 375 px did not
    scroll sideways.

## What these later walks found

* The entity designer started from the newest definition row, and a redefinition replaces every
  definition change before it: a check or a button added by another screen was dropped on the next
  save, and the list still showed it as active. Fixed in PR 168 and Ferrum-Frontend PR 70.
* The buttons screen sent the description's `may_run` back and was refused; the refusal reached the
  screen as "Please check your input." because an operation that does not parse is answered in plain
  text. Both are fixed (the second in PR 171).
* `may_run` alone would have shown a button on a record the person may not change (own records only).
* A choice or a record called `constructor` or `toString` was shown as nothing.
* The menus screen sent the index's `package` mark back and every save of an existing menu was
  refused (the batch's new refusal shape put the server's words on the form, which is how it was seen).
* About twenty sentences lower-case a name before asking for its word ("No {value} yet", "Search
  {labels}"), so the word was never found, for apps' names as for a workspace's: an empty list read
  "لا risk register بعد". Words are now also found by their lower-cased English.
* Left as they are: the frame's own badge shows a menu's initials, not its icon; the bar's name of a
  menu is not translated.
* ERP's invoice line takes its price in `price_list_rate`; a `rate` in the step's input is ignored
  without a word (it is a computed field), and the first invoice raised by the button was submitted at
  zero. Nothing warns whoever sets the button up. Not fixed.
* Saving a button, target or menu that was already as saved showed the server's refusal as an error;
  an optional condition asked for at least one row; "2 days" counted in a calendar is 48 open hours,
  which the form did not say. Fixed in Ferrum-Frontend PR 72.

## What still needs code or is not built

* **Record ids written inside an expression do not travel.** The invoice automation of step 22 names a
  company, a customer and an item by id; it was removed before the export of step 23.
* **The demo data task** fails on a build without the India add-on (`in_gstin`: unknown field).
* **Uninstalling a pack, signing one, and the app store** are not built; an install never deletes.
* **Approvals by several people, public pages bound to any entity without Forms, and money steps as
  contracted capabilities** keep their places in the plan.

## Not verified

* The mail preview's sandboxed frame was blank in the tool's screenshots although the HTML placed in it
  was right.
* Keyboard-only use of the new screens, and a screen reader on a button shown unavailable.
* A button as someone without its role: covered by the server's tests, not walked in the browser.
* A money amount of 12.345 on a line of a custom entity was stored as typed.
