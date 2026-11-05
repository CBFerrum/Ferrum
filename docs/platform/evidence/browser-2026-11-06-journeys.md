# A browser on a running server, 6 November 2026: six journeys through the pages

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
| Procure to pay (FERRUM-JOURNEY-002) | A request | Grind Co as a supplier; material requests come with Procurement, installed from the App Manager (its dialog listed the 13 entities it adds); MAT-REQ-2026-00001 for 10 grinders into Stores by 15 November, submitted |
| | An order | Create Purchase Order refused: "no usable approved supplier for Coffee grinder: choose a supplier for them", with nowhere to choose one (below). Grind Co named the grinder's approved supplier; then PO-2026-00002, 10 at 70, 700.00, submitted ("To Receive and Bill") |
| | Received and billed | PR-2026-00001 submitted ("To Bill"); its purchase invoice's Supplier Invoice No came filled with "PENDING-PR-2026-00001", the typed number was added to it, and PINV-2026-00002 went in as "PENDING-PR-2026-00001GC-2026-501" (below). Cancelled ("reverses its postings"), amended with GC-2026-501 and submitted as PINV-2026-00003 |
| | Booked once, and paid | Another bill from Grind Co numbered GC-2026-501 refused: "this supplier invoice number was already entered". PAY-2026-00004 paid 700.00 (reference TRF-GC-501); the request Received (ordered in full), the order Completed (received and billed in full), the bill Paid |
| Project to profit (FERRUM-JOURNEY-005) | A project and its work | PROJ-0001 "Harbour Cafe fit-out" for Harbour Cafe, Time and Materials; an activity type, Fit-out work, billing 80 and costing 40 an hour; TASK-00001 "Install the counter" on its board |
| | Time | TS-2026-00001: three hours on the task, costing 120.00 but billing 0.00 (below); the row's billing hours and rate set in its fields, 240.00; submitted |
| | Invoiced | The project's Create Sales Invoice ("Invoice the project's unbilled time") refused: "set the service item of activity type Fit-out work to invoice its time"; with the Field labour item set, a draft of 3 hours at 80.00, 240.00, submitted |
| | The margin | Project Profitability: billed revenue 240.00, timesheet cost 120.00, margin 120.00 (50%), no time unbilled |
| Document to signature (FERRUM-JOURNEY-008) | A request | In Sign, a one-page service agreement for Harbour Cafe uploaded; signers one after another: Maya Rao (Customer), then the administrator (Walk Properties); her signature and the date signed placed under "Signed for Harbour Cafe:", theirs below; the review gave the document's fingerprint and said what kind of signature it is (an audit trail, not a qualified one); sent as SIGN-2026-00003 |
| | Signed in turn | Maya's page, from the link e-mailed to her, showed the document with its SHA-256 and the last day; signing without the consent refused ("Tick the box to agree to sign electronically"); with it, a drawn signature recorded. Only then did the administrator's link go out; a typed signature completed it ("Everyone has signed") |
| | Sealed and checked | The request Completed with the signed copy's fingerprint and seven steps (sent, opened, signed drawn and typed, sealed); Check integrity: the original and the signed copy unchanged, and each step tied to the one before |

## What it found, and what changed

| Found | Fixed in |
|---|---|
| Lena, a portal person, read every person's name and address from `GET /api/users` (other customers, a citizen, a partner, a supplier's contact), and Helpdesk offered portal people as a ticket's assignee | Ferrum PR 74: the directory lists members alone, and only members read it |
| Field Service's time box read the browser's zone while the job shows the workspace's: 02:38 typed showed as 21:08 | Field Service 0.3.1: the box reads the workspace's clock, as the page shows it |
| Other pages show times in the browser's zone (a job's Reported At, shown by the form, read 6 Nov 01:04 beside the timeline's Thu 5 Nov 19:34) | Not yet: one zone for every page is a rule across the apps (FERRUM-UX-006) |
| Time added by hand that ends in the future was accepted (finishing at 21:15 at 21:09) | Field Service 0.3.1: refused by the page and the server, five minutes' slack |
| With CRM and ERP installed but not their connector, a submitted quotation shows no way to an order, and nothing says why | Not yet |
| After signing in with a password at the accounts host, it asks "Continue to Walk" once more | This PR: signing in there is choosing the account |
| Field Service Settings, which has no name of its own, was headed by the end of its id | Ferrum-Frontend PR 34: settings take their entity's label |
| A material request's Create Purchase Order asked for no supplier, then wanted one; seven actions read parameters they did not declare, and a request for quotation's Create Supplier Quotation and a bank line's Create Payment Entry could never work from a page | ERP 0.14.2 (Procurement 0.1.3, Finance 0.3.2): the actions declare them |
| A purchase invoice made from a receipt holds "PENDING-<receipt>" as the supplier's number, and could be submitted with it | ERP 0.14.2: refused at submit |
| A timesheet row entered on its page billed 0.00: the preview's zeros for billing hours and rate went back as if chosen | Projects 0.6.4: zero takes the default |
| Opening an entity of an add-on that is not installed (a material request before Procurement) says "entity MaterialRequest not found" | Not yet |
| Sign shows a 776-byte PDF as "0.00 MB" | Not yet |

## What it does not show

- Nobody independent prepared the figures or checked the outcome.
- The order had no tax and no discount past an approval limit; the kernel's lead-to-cash test has
  both. No parts left a van's stock; Field Service's suite has that. Procure to pay had no request
  for quotation and no bank statement matched (Finance was not installed); the kernel's
  procure-to-pay test has both.
- One server on one machine, a debug build, not the released image or a deployment.
