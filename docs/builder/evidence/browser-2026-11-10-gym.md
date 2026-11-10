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

## What still needs code or is not built

* **A whole solution does not travel.** The export carried the entities, the check and the workflow,
  but not the automations and not the solution with its rule (packs, BF9).
* **A public sign-up page** and **the member's portal** were not walked here (Forms was compiled in but
  not used; portal roles were walked on 10 November, [portal](browser-2026-11-10-portal.md)).
* **A monthly charge as an ERP invoice** was not walked: the daily automation created a custom record,
  not a Sales Invoice.
* **Reports** over the custom entities were not walked.

## Not verified

* The mail preview's sandboxed frame was blank in the tool's screenshots although the HTML placed in it
  was right.
* Keyboard-only use of the new screens.
* A money amount of 12.345 on a line of a custom entity was stored as typed.
