# A browser on a running server, 6 November 2026: three journeys through the pages

Until now these journeys ran through the API, in the kernel's and the apps' suites. This walk took
them through the interface, as the people in them would. The server was built in debug on one
machine from Ferrum as of PR 71 (offline leases), with the apps pinned in its lock, against its own database
(`ferrum_walk`, never `ferrum_dev`), as in [the walks of 4 November](browser-2026-11-04-flows.md):
`FERRUM_BASE_DOMAIN=localhost`, the development mailer (sign-in codes read from the outbox), and the
shell's interface built from Ferrum-Frontend main.

Two people took part, each signed in through the pages: the workspace's administrator, at the
accounts host with a password, and Lena, a lessee and ERP customer, on the portal with e-mailed codes.
The workspace's time zone is UTC; the browser's was India's.

## What was walked

| Journey | Steps | What it showed |
|---|---|---|
| Support to resolution (FERRUM-JOURNEY-004) | A service level agreement | Made in its form: High 4 hours to answer and 16 to resolve, Medium 8 and 40, paused while on hold or waiting on the customer |
| | Lena raises a ticket | On Your tickets, "Kitchen tap leaking in Flat 2", High: TKT-2026-000002, "With the team" |
| | The agent takes it | In the inbox's Unassigned, High and "due in 4 h"; taken; first response due 06:28, resolution 18:28 |
| | An answer and a note | The reply e-mailed to her ("Reply sent. Emailed to lessee@walk.test."); first response "Fulfilled in 4 min"; resolution "Waiting for the customer", its clock stopped; an internal note added |
| | Lena reads and answers | "Waiting for your reply", the reply shown and the note not; her answer put it back "With the team"; resolution due 18:29, "Stopped for 1 min in all" |
| | Resolved and rated | Resolved with how, sent to her; she read "How it was resolved" and rated it 5 of 5; Customer Satisfaction by agent: one rated ticket, 5, 100% satisfied |
| Field work to invoice (FERRUM-JOURNEY-007) | A technician on site | FSWO-2026-00001 for Cyril, On the way from the walk of 5 November; "I've arrived" on My day; 0.12 h of time added by hand |
| | Signed and completed | What was done, Cyril's name and signature on the pad ("Signed by Cyril Citizen" on the timeline); completed after "The parts leave stock and the work order is locked" |
| | Invoiced | Create the invoice refused: "set the labour item in Field Service Settings to invoice labour". A Field labour item (a service, by the hour, 60) made in ERP and named in the settings; then "The invoice is made as a draft.", the job Invoiced, its line "Labour: Walk Admin, 0.12 h" at 60.00, 7.20 |
| | Issued and paid | In ERP the draft submitted as SINV-2026-00004, its payment recorded and submitted; the invoice Paid |
| Lead to cash (FERRUM-JOURNEY-001) | Stock to sell | A Coffee grinder item stocked in Stores, selling at 150; STE-2026-00001 opened 5 at 80 |
| | Lead, opportunity, quotation | LEAD-2026-00002, Maya Rao of Harbour Cafe; Create Opportunity gave OPP-2026-00001 (Prospecting, 10%); Create Quotation gave QTN-2026-00002 for the lead, 2 at 150, 300, submitted |
| | The order waits for a customer | The workspace had CRM and ERP but not their connector, so the quotation offered no order; "CRM and ERP" installed from the App Manager's store. Then Create Sales Order refused: "quotation QTN-2026-00002 is for lead Maya Rao; convert the lead to a customer before creating a sales order" |
| | Customer, order, delivery, invoice, payment | Convert to Customer gave CUST-00005; Create Sales Order gave SO-2026-00001, submitted ("To Deliver and Bill"); its delivery note DN-2026-00001 submitted ("To Bill"); its invoice SINV-2026-00005 submitted and its payment recorded |
| | Where it ended | Lead Converted, opportunity Won, quotation Ordered, the order Completed (delivered and billed in full), the invoice Paid; Stock Balance 3 grinders at 80 (240.00); Trial Balance "Balanced: Yes" with cost of goods sold 160.00 |

## What it found, and what changed

| Found | Fixed in |
|---|---|
| Lena, a portal person, read every person's name and address from `GET /api/users` (other customers, a citizen, a partner, a supplier's contact), and Helpdesk offered portal people as a ticket's assignee | Ferrum PR 74: the directory lists members alone, and only members read it |
| Times typed into a date-and-time box are read in the browser's zone while the pages show the workspace's: 02:38 typed showed as 21:08; a job's Reported At, shown by the form, read 6 Nov 01:04 beside the timeline's Thu 5 Nov 19:34 | Not yet (FERRUM-UX-006) |
| Time added by hand that ends in the future was accepted (finishing at 21:15 at 21:09) | Not yet |
| With CRM and ERP installed but not their connector, a submitted quotation shows no way to an order, and nothing says why | Not yet |
| After signing in with a password at the accounts host, it asks "Continue to Walk" once more | Not yet |
| A record with no name of its own (Field Service Settings, a draft stock entry) is headed by the end of its id | Not yet |

## What it does not show

- Nobody independent prepared the figures or checked the outcome.
- The order had no tax and no discount past an approval limit; the kernel's lead-to-cash test has
  both. No parts left a van's stock; Field Service's suite has that.
- One server on one machine, a debug build, not the released image or a deployment.
