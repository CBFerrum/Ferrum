# A browser on a running server, 6 November 2026: ten journeys through the pages

Until now these journeys ran through the API, in the kernel's and the apps' suites. This walk took
them through the interface, as the people in them would. The server was built in debug on one
machine from Ferrum as of PR 71 (offline leases), with the apps pinned in its lock, against its own database
(`ferrum_walk`, never `ferrum_dev`), as in [the walks of 4 November](browser-2026-11-04-flows.md):
`FERRUM_BASE_DOMAIN=localhost`, the development mailer (sign-in codes read from the outbox), and the
shell's interface built from Ferrum-Frontend main. The later walks (hire to retire, the hotel stay
again and marketing to consent) ran on builds of the branches that fixed what the earlier ones
found: Ferrum PR 78, then the branch platform/pins-stay.

Four people signed in through the pages: the workspace's administrator and Dana, a member, at the
accounts host with passwords; Lena, a lessee and ERP customer, on the portal with e-mailed codes; and
Ravi, a new employee, at the accounts host with a password and, the first time, an e-mailed code.
Three marketing contacts (Lena Park, Zoe Brandt and Meera Iyer) only opened the links of the e-mails
sent to them, in a tab with no session.
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
| Learning completion (FERRUM-JOURNEY-011) | A course | In Learn, "Kitchen hygiene basics" built in the course builder: a chapter, a reading lesson written in Markdown and saved as typed, and a quiz lesson ("Quiz is required" until it had one); two questions in the question bank and a quiz of both with a pass mark of 70% (New quiz lost the lesson's kind, below); published |
| | A learner | Dana, a member without Learn's roles, read "You do not have access to Learn"; given the Learner role, she enrolled from the catalog, read the lesson and marked it done (1 of 2) |
| | Failed, then passed | The quiz with one wrong answer: 50%, "Not passed this time", the right answers kept back until her last attempt; again, 100%: "You completed Kitchen hygiene basics", with notices of her enrolment and completion |
| | A certificate | None at first: the course's Certificate on Completion is off unless chosen. Ticked by the administrator, Learn's upkeep issued CERT-2026-000001 to the finished enrolment within the minute and told her; My learning showed it with View certificate and Copy link to check it |
| | Checked by anyone | Its public page said "This certificate is valid", to Dana Reviewer for Kitchen hygiene basics, issued 5 Nov 2026 by Walk, and opened the printed certificate; asked without a session, the check answered for its code and said "certificate not found" for a code one letter off |
| Hire to retire (FERRUM-JOURNEY-003) | An offer | A designation, Barista; job opening HR-OPN-2026-0001; applicant Ravi Kumar; offer HR-OFF-2026-00001 (joining 5 Nov, a cost to company of 360000) submitted and accepted |
| | An employee with a sign-in | Create Employee made HR-EMP-00001, stayed on the offer and could not name his sign-in (below); his user set on the employee's form; onboarding HR-ONB-2026-0001 Completed when its two activities were done |
| | His own page | Ravi signed in at the accounts host (his password, then the e-mailed code); My space read "Ravi Kumar · Barista" with check-in, requests and payslips; he claimed 450 for travel (HR-EXP-2026-00001), which the administrator approved |
| | An advance | HR-EAD-2026-00001, 1000, submitted; its page offered no Make Payment Entry (below), so it was paid through the API: JV-2026-00002 in ERP, the advance Paid |
| | Leaving | Separation HR-SEP-2026-0001, relieving 5 Nov, submitted: Ravi Left. Create Full and Final Statement opened HR-FNF-2026-0001, "made with the 2 amounts open": the claim's 450 to pay him, the advance's 1000 to recover; a gratuity of 2000 added by hand: 2450 to pay, 1000 to recover, 1450 net |
| | Settled in one entry | Make Payment Entry: a draft journal entry in ERP, Employee Payables Dr 450, Salaries Dr 2000, Employee Advances Cr 1000, Primary Bank Cr 1450; submitted as JV-2026-00003, the statement Paid, the claim Paid and the advance Returned |
| | The sign-in ended | HR's upkeep made Ravi's user inactive on his relieving date; signing in as him was refused |
| Hotel stay (FERRUM-JOURNEY-010) | A hotel | Harbour View Hotel with its room night item; Deluxe King at 120, rooms 101 and 102; Set the Business Date asked for a date with nowhere to give it (below) |
| | A stay | STAY-2026-00001, Arjun Mehta, 5 to 7 November in 101: 240, Reserved; checked in, folio FOL-2026-00001; a minibar charge of 15 and a deposit of 100 by card |
| | Night audits | 5 November closed: 2 rooms, 1 occupied (50%), an average rate of 120, revenue per room 60, a no-show (STAY-2026-00002), the night's room charge posted; 6 November likewise |
| | Check-out | Checked out: a draft sales invoice for 255 (the minibar, two nights at 120, each line dated), room 101 Dirty; the 100 deposit was on the folio only (below) |
| | Again, on Hospitality 0.2.0 | A second hotel's business date set in a dialog ("The business date is 2026-11-06"); STAY-2026-00003 checked in through a Room dialog ("Checked in to room 102; folio FOL-2026-00002 is open"); a deposit of 50 in cash; the night audit "The night of 2026-11-07 is closed: 1 of 2 rooms occupied, 1 room charge posted, 0 no-shows"; check-out "the folio is invoiced as a draft, 120.00; 50.00 paid at the desk waits in ERP as draft payments to apply to it" |
| | Paid in ERP | SINV-2026-00007 submitted; the deposit's draft payment (Arjun Mehta, 50, reference DEP-102), Get Outstanding Invoices allocating it, submitted as PAY-2026-00005: the invoice Partly Paid, 70 outstanding |
| | A cancellation | STAY-2026-00004, Meera Iyer: Cancel the Reservation asked for a reason, required; "Guest changed travel plans": Cancelled, "its nights are free again", the reason kept on the stay |
| Marketing to consent (FERRUM-JOURNEY-009) | Installed | Campaigns 0.3.1 could not be installed (below); 0.3.2 came with Mail, Chat and Calendar (the dialog named it "comms", below), whose pages were blank (below) |
| | Contacts with consent | Three people imported from pasted rows, the city kept as an attribute and how they agreed kept on each ("Signed up at the front desk, 6 Nov 2026"); a segment, Guests in Pune (city is Pune), counting 2 of the 3 as its rule was typed |
| | A campaign | "Winter offer, Pune", its subject greeting by first name; the review stopped it until Mail's Public URL was set (it named the page with no way to it, below) and warned of the missing postal address, set in Campaign Settings (a list of one, below); sent to 2 as "Lena, a winter night on us", with the postal address and an unsubscribe link at the bottom |
| | Opened, clicked, unsubscribed | Lena Park's open and click were counted; her unsubscribe link asked first (opening it changed nothing), then "lena@guests.test will not receive our marketing emails any more": Unsubscribed, with the link and campaign as the source and a keyed hash of her address, and on the suppression list |
| | Enforced | Setting her back to Subscribed on her page was refused: "a contact who is unsubscribed can only opt back in themselves, by confirming a subscription form"; a copy of the campaign counted "1 person will get this email" with her as Unsubscribed, and reached only Zoe Brandt |
| | Results | The campaign's Results tab said "No results yet", with both e-mails sent and one opened and clicked (below) |
| | Double opt-in | A subscription form, Hotel newsletter (the city allowed), posted from a page of the hotel's site: Meera Iyer was told "Almost done" and was Pending; her e-mailed link asked "Subscribe meera@guests.test to Hotel newsletter?", then she was Subscribed, by "double opt-in: Hotel newsletter" |
| | Her own opt-in | Lena Park signed up on the same form and stayed Unsubscribed until she confirmed her own e-mailed link; then Subscribed, her suppression lifted; the form counted 2 submissions, 2 confirmed |

## What it found, and what changed

| Found | Fixed in |
|---|---|
| Lena, a portal person, read every person's name and address from `GET /api/users` (other customers, a citizen, a partner, a supplier's contact), and Helpdesk offered portal people as a ticket's assignee | Ferrum PR 74: the directory lists members alone, and only members read it |
| Field Service's time box read the browser's zone while the job shows the workspace's: 02:38 typed showed as 21:08 | Field Service 0.3.1: the box reads the workspace's clock, as the page shows it |
| Other pages show times in the browser's zone (a job's Reported At, shown by the form, read 6 Nov 01:04 beside the timeline's Thu 5 Nov 19:34) | Not yet: one zone for every page is a rule across the apps (FERRUM-UX-006) |
| Time added by hand that ends in the future was accepted (finishing at 21:15 at 21:09) | Field Service 0.3.1: refused by the page and the server, five minutes' slack |
| With CRM and ERP installed but not their connector, a submitted quotation shows no way to an order, and nothing says why | CRM 0.6.2: its status says how it becomes an order, with ERP and the CRM and ERP connector |
| After signing in with a password at the accounts host, it asks "Continue to Walk" once more | This PR: signing in there is choosing the account |
| Field Service Settings, which has no name of its own, was headed by the end of its id | Ferrum-Frontend PR 34: settings take their entity's label |
| A material request's Create Purchase Order asked for no supplier, then wanted one; seven actions read parameters they did not declare, and a request for quotation's Create Supplier Quotation and a bank line's Create Payment Entry could never work from a page | ERP 0.14.2 (Procurement 0.1.3, Finance 0.3.2): the actions declare them |
| A purchase invoice made from a receipt holds "PENDING-<receipt>" as the supplier's number, and could be submitted with it | ERP 0.14.2: refused at submit |
| A timesheet row entered on its page billed 0.00: the preview's zeros for billing hours and rate went back as if chosen | Projects 0.6.4: zero takes the default |
| Opening an entity of an add-on that is not installed (a material request before Procurement) says "entity MaterialRequest not found" | Not yet |
| Sign shows a 776-byte PDF as "0.00 MB" | Sign 0.2.4: "776 bytes", or "48 KB" |
| In Learn's course builder, New quiz left for a blank quiz form before the lesson's kind could save (a quiz lesson saves only with its quiz): the lesson came back a reading lesson and the new quiz unpicked | Learn 0.6.1: New quiz and New assignment make one in the lesson, named after it, and pick it |
| Once, choosing another account at the accounts host ended in "Signing in did not finish": another open tab of the workspace, told of the sign-out, likely began a sign-in of its own over the first one's | Ferrum PR 78: each sign-in keeps its own cookie |
| Create Employee made the employee but stayed on the offer, and could not name their sign-in (it read a user it did not declare) | HR 0.8.1: it asks for the sign-in and opens the employee; the actions of claims, advances, attendance and leave declare what they read |
| An advance's Make Payment Entry did not show on its page: its paid amount was empty, which the server's condition reads as 0 and the page's did not | HR 0.8.1 stores 0; Ferrum-Frontend PR 36: a page reads empty numbers and checks as the server does |
| Nothing settled a leaver's open claims and unsettled advances together | HR 0.8.0: the full and final statement, settled by one journal entry |
| After "Use another account" at the accounts host and signing in, "Continue to Walk" asked again | Ferrum-Frontend PR 37: an account chosen there is chosen |
| A draft journal entry (numbered on submit) is headed by the end of its id | Not yet |
| Set the Business Date, Check In and Cancel the Reservation read a date, a room and a reason they did not declare, so their pages could not give them | Hospitality 0.2.0: they ask for them |
| A deposit taken at the desk stayed on the folio: ERP had no payment for it, so the invoice said the guest owed it all | Hospitality 0.2.0: at check-out the folio's payments become draft payment entries in ERP, to apply to the invoice |
| Check In, the night audit and the cancellation said only "done" | Hospitality 0.2.0: each says what it did |
| Campaigns 0.3.1 asked for Mail 0.7 beside the server's Mail 0.10, so it could not be installed | Campaigns 0.3.2; Ferrum (platform/pins-stay): a compiled app whose requirement a compiled app does not meet fails the build's tests |
| The install dialog listed what it also installs by id ("comms 0.10.0") | Ferrum (platform/pins-stay) and Ferrum-Frontend PR 39: by name |
| Mail's pages were blank: the server trusted its database's record of the interface, whose files were in the storage of a server that ran elsewhere | Ferrum (platform/pins-stay): a release's files are looked for, and stored again when missing |
| The review named the settings pages that fix it, with no way to them | Campaigns 0.3.3: Fix links to them |
| Campaign Settings and Communication Settings opened as lists of one record | Ferrum (platform/pins-stay): a settings link opens its record; Ferrum-Frontend PR 38: the list screen does too |
| A sent campaign's Results said "No results yet": CRM's report of the same name answered for it | Campaigns 0.3.3: Email Campaign Performance; Ferrum (platform/pins-stay): two compiled apps cannot give one report name |
| A subscription form gave only its endpoint's path, and no form to paste into a website | Campaigns 0.3.3: HTML for Your Website |
| CRM dated what it stamps by UTC while reports use the workspace's date: in the hours between the two midnights a deal won that day fell outside the day's report (seen in Campaigns' CI) | CRM 0.6.4: the workspace's date throughout |

## What it does not show

- Nobody independent prepared the figures or checked the outcome.
- The order had no tax and no discount past an approval limit; the kernel's lead-to-cash test has
  both. No parts left a van's stock; Field Service's suite has that. Procure to pay had no request
  for quotation and no bank statement matched (Finance was not installed); the kernel's
  procure-to-pay test has both.
- Learning completion had no cohort, live session, graded assignment, paid course or packaged lesson;
  Learn's suite has them.
- Hire to retire paid no salary and took no leave (their own scenarios, in HR's suite), and the
  gratuity was added by hand: nothing works it out. The advance was paid through the API.
- The hotel's invoice and payments are drafts that ERP's people submit.
- Marketing e-mails went through the development mailer: nothing reached a real inbox, and no
  bounce or complaint came back (the signed webhook is in Campaigns' suite). The hotel's signup page
  was written into a tab with the form's fields, as a site would paste them, since 0.3.2 gave no
  HTML to paste. No A/B test or journey was walked; Campaigns' suite has them.
- One server on one machine, a debug build, not the released image or a deployment.
