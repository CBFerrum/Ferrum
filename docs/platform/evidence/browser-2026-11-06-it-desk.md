# A browser on a running server, 6 November 2026: the IT desk's rules

IT Services (FERRUM-ECOSYSTEM-003) had its home page drawn in a browser on 4 November, with its
flows untried there. This walk took them on the walk server: a debug build of Ferrum PR 89 on one
machine, with its own database (`ferrum_walk`), the shell from Ferrum-Frontend b39fffb and IT
Services 0.2.3 as pinned. The workspace's clock is UTC and the browser's was India's. It ran from
20:15 to 20:35 UTC.

One person, the workspace's administrator, raised everything, so each rule about who decides was
met from the side of the one who asked. The records were made through the API from the signed-in
page, and the desk was read on its page.

## What was walked

| Step | What it showed |
|---|---|
| Configuration items | app-server-01, a server, and Payroll, an application running on it and depending on it. The server made to depend on Payroll was refused: "these dependencies come back round to this item". A connection back to Payroll was kept, as connections may loop |
| The catalog | A New laptop offering for Payroll, needing approval and fulfilled within 48 hours. REQ-2026-00001 asked for one: due 8 November, 20:21 UTC, 48 hours on. Fulfilling it before approval was refused ("this request waits for its approval"). Approving and rejecting it as the one who asked were refused ("a request is decided by someone other than who asked") |
| An incident | INC-2026-00002, Payroll will not open, high impact and medium urgency: P2, due in 8 hours |
| A change | A month-end freeze from 28 November to 2 December. A normal change without a way back was refused ("a normal or emergency change says how it is undone"), and one inside the freeze too ("falls in the freeze for Month-end close: only emergencies then"). CHG-2026-00001, patching the server on 10 November, named Payroll among what it touches. Submitted, it could not be approved by whoever raised it ("a change is decided by someone other than who raised it") |
| The desk | Open incidents: P1 Printer on floor 2 offline, Breached (from 4 November), and P2 Payroll will not open. Requests waiting: "REQ-2026-00001 · Submitted". Changes coming up: "CHG-2026-00001 · Patch app-server-01 · Submitted" |

## What it found, and what changed

| Finding | Now |
|---|---|
| A waiting request was listed as its number and status, without what was asked for, by whom or by when | IT Services 0.2.4 (ferrum-app-itsm PR, merged with its CI run on this machine): what it asks for, who asked and when it is due, the soonest due first |
| A change "coming up" did not say when | When it starts |

## What it did not cover

- A second person deciding a request or a change: the walk had one, the suite tests the rule both ways.
- Discovery through a Discovery Agent's key, items not seen for 7 days, and Assets through the connector. The suite tests each.
