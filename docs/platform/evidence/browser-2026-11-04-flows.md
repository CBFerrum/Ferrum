# A browser on a running server, 4 November 2026: the apps' flows and their portal pages

The second walk of the day, after [the apps' home pages](browser-2026-11-04-apps.md). The server was
built in debug from this PR's branch, with the apps pinned in its lock, against its own database
(`ferrum_walk`, never `ferrum_dev`): `FERRUM_BASE_DOMAIN=localhost`, a bootstrap token and secret key
made for the run, the development mailer (mail stays in the outbox, where the sign-in codes were read),
and `FERRUM_WEBHOOKS_ALLOW_PRIVATE`, so a connection could reach a stand-in on this machine. The shell's
interface was Ferrum-Frontend built on this machine: PR 18's branch, then main once PRs 18 to 20 had merged.

Six people took part, each signed in through the pages: the workspace's administrator and Dana, a
second member holding the approving roles (both with Ferrum accounts at `accounts.localhost`, Dana's
set up with the code from the outbox), and four portal people who signed in with e-mailed codes: Pat, a
partner's member; Cass, a customer's contact; Cyril, a citizen; and Lena, a lessee.

The Mastodon account was a stand-in (a short Python server speaking the parts of Mastodon's API the
connector uses), and eu-1's tooling was a loop posting its readings: neither is the real thing.

## What was walked

| App | Flow | What it showed |
|---|---|---|
| Control Plane | A release recorded, then qualified | Qualifying it as who recorded it refused ("a release is qualified by someone other than who recorded it"); Dana qualified it |
| | A partner, certified | Refused without a certification date; saved with one, its member's e-mail lowercased |
| | A cell's token replaced, the cell reporting | Down after 80 quiet minutes; healthy on a report with the new token; a wrong token refused |
| | A hosted workspace with no cell | Placed in eu-1 by itself |
| | An incident told in updates | Created only with its cell and a first update; a status change without an update saying it refused; the public status, signed out, showed both updates and no customer |
| | A rollout of the release | Started by Dana; the wave opened in eu-1's maintenance window and set its desired release (the cell reported "it runs 0.4.0, not the release it should"); upgraded, soaked an hour while healthy, and done at 09:03 UTC, the rollout with it |
| Partner portal (Pat) | Your customers | The portfolio with health, plan, renewal and support access, a note on the missing health key, a case raised (one open case after), onboarding refused with "no shared cell in eu has room" as eu-1 was full |
| Service portal (Cass) | Your service | Her workspace on eu-1 with its maintenance window, the incident and both its updates, and Pat's case with its due time by her Premium plan |
| Social | A Mastodon connection | Listed with what it can do; saved, then tested ("Working"); its history (reads and sends) shown |
| | A post to review, approved, out at its time | A link without `https://` refused; approving one's own post refused; Dana approved; at 08:10 UTC it went out as a status with its link, and the post became published with the network's id |
| | The inbox | A comment and a direct message read in, routed to the account; answered, and the answers reached the stand-in |
| | What posts did | Four engagements (three favourites, a boost) on the brand's line |
| Government | A permit type, then a citizen's application (Cyril) | Applied on Your services, the map uploaded, submitted; Dana took it up, approved it with a note and issued it; Cyril saw his permit valid until 4 December |
| Governance and Risk | A risk and its acceptance | Scored on the matrix (inherent 20, current 15, High); accepted by Dana, who does not own it |
| Objectives | An objective, a key result, a check-in | The check-in moved the key result to 50% and the objective with it |
| IT Services | An incident | High impact and urgency gave P1, due four hours after it opened |
| ERP | A company and its chart of accounts | 55 accounts; then the rent's invoice submitted |
| Property | Installed through the App Manager, then a lease | The plan listed what it adds; the lease activated and its first month invoiced |
| Lessee portal (Lena) | Your home | Her lease, the rent invoiced, a repair asked for (MR-2026-00001, open); the invoice opened on Your invoices once issued |
| Metrics | A metric of open IT incidents | Certifying it as its owner refused; Dana, a data steward, certified it; recorded, it read 1 with its lineage |
| Contracts | A contract to its signature | Submitted for approval above the threshold; Dana approved it; its signature is below |
| Government | A permit's fee, paid through ERP (Cyril, 4 November) | A Market Stall's application (fee 40) said its invoice was not issued yet; approving it before payment refused ("the fee is not paid yet"); in ERP its invoice SINV-2026-00002 issued and its payment PAY-2026-00001 recorded; approved and issued; Cyril saw PRM-2026-00002 valid until 4 February 2027 and opened the invoice from his application (EUR 40.00, Paid) |
| Control Plane | A partner's statement | With a 20% share, October's statement read 450 paid and 90 the partner's; November refused ("a statement is for a month that has ended") and October a second time refused; Pat read it under Statements |
| | A recovery request | A restore of northwind-retail to 06:00 with its reason; approving it as who asked refused ("a recovery is approved by someone other than who asked for it"); Dana approved it and marked it done; Cass read "Restore … Done" on Your service |
| Contracts and Sign | A contract to its signature (5 November) | Contract Settings named the signatory; a contract of 18,000 drafted with its PDF and submitted; approving it as its owner refused; Dana approved it and sent it; the counterparty signed on Sign's page from the link e-mailed to them (a typed signature, the consent ticked), then the signatory from theirs; the contract became active, signed on 5 November. The sealed PDF holds both signatures and a certificate of completion, whose hashes check: the original's SHA-256, and the signed part's (`head -c 1223 file.pdf`) |
| Store and App Manager | A paid app bought, installed, disputed and refunded (5 November) | A store on this machine (its own database, the test payment provider) sold the SDK's board example, published by a verified publisher and approved by a moderator in the store's pages, at EUR 9.00 a month. The workspace's administrator bought it for walk.localhost on the app's page (EUR 10.71 with Germany's 19%), read its licence on Your purchases and gave it in Ferrum's install dialog, which held Install until then and refused a malformed licence before sending it; Board installed. Cancelled at the end of the period; its charge moved back 20 days in the store's database to pass the refund window; a dispute opened, answered by the publisher in their console, refused by the store's administrator without a note (refused: "a dispute refused says why") and then refunded; the buyer read the refund and "Your dispute was refunded."; at Ferrum's next reading of the licences (06:23 UTC) Board's card under Installed said "Licence lapsed", the app still installed |
| ERP | A supplier's orders on the portal (Rina, Cleanline's contact) | Cleanline Services, with Rina as its contact, was sent PO-2026-00001 (four cleaning visits at 250, required by 30 November) and billed it as CL/2026/118. Rina signed in with an e-mailed code: Your orders listed the order with where it stands and its total and the bill with EUR 1,000.00 still owed; the order opened with its line and what was received of it, and printed as sandboxed HTML |
| ERP | Revenue over a service period, through the pages (5 November) | Walk Properties, made before ERP 0.9.0, sold Lena a year of common area cleaning: the line, priced 3,650.00 from the price list, was marked Deferred Revenue from 1 January to 31 December 2026. Submitting was refused until the company had a Deferred Revenue account (below); then SINV-2026-00003 went through, Recognise Deferred Revenue and Expense through 31 October recognised 3,040.00 (304 days at 10), and the Deferred Revenue and Expense report showed the line with 610.00 still deferred and 50.00 to recognise through 5 November |
| ERP | An accrual that reverses itself | October's electricity, 400 to Utilities against Accrued Expenses on 31 October with Reverse It: JV-2026-00001, submitted on 5 November, was reversed on 1 November at once, the general ledger showing both days |
| Bookings | A staff member's outside calendar | Bookings 0.4.0 installed from the App Manager; Wanda Walker's staff record took her outside calendar's address (a stand-in on this machine serving an Outlook-style calendar whose times name "India Standard Time"), kept as a secret; the upkeep read it within a minute and the record said when. The public booking page for Friday 6 November, in the guest's Asia/Calcutta, offered 09:00 and 11:00 to 16:00: the outside appointment from 10:00 to 11:00 took 09:30 to 10:30, and an event marked free at 14:00 took nothing. 13 November offered every half hour from 09:00 |
| Phone width and dark mode | The citizens' portal and the Control Plane's console at 375 px, dark | Both fit the width; the portal bar showed two of its five pages and not the one shown |
| Digital Assets | An image from Drive to its use | Uploaded to Drive, made an asset (its kind, type, size and digest read from the file), submitted and approved; used in advertising refused for its editorial-only licence ("an editorial-only asset is not used to sell"), used on the web recorded |

## What it found, and what changed

| Found | Fixed in |
|---|---|
| A record whose title field is a link showed the linked record's id as its heading or subtitle (a risk acceptance's risk, a rollout's release, a lease's unit) | Ferrum-Frontend PR 18 (`recordTitle` and the form's heading take `_titles`); the Control Plane's console asked for titles too |
| Every input on the Control Plane's portal pages had no accessible name: apps wrap inputs in the kit's `Field` without `htmlFor` | Ferrum-Frontend PR 18: `Field` names a single control itself |
| A Social account named its connection by an id that no page shows | ferrum-app-social 0.2.1: by its name, as the Connections page shows it |
| A message had no way to be answered from it | Social 0.2.1: Reply on the message; 0.2.2: its box no longer starts with the message's own text |
| Answering a direct message on Mastodon went out as a public status, which would publish a private conversation and, naming no one, not reach its author | Social 0.2.2: a reply is as open as what it answers and names its author |
| The partner page told a portal person who is no partner's member "Something went wrong… Try again" | ferrum-app-control 0.1.1: says the page is for a partner's members |
| "1 months" on the citizens' page | ferrum-app-government 0.1.1 |
| The lessee's rent linked to a draft invoice, which Your invoices does not show ("invoice not found") | ferrum-app-property 0.1.1: linked once issued; a draft says it is not issued yet |
| Send for Signature was offered for an approved contract without its PDF, then asked to attach it, which an approved contract refuses | ferrum-app-contracts 0.1.1: offered only with the PDF, which the field says to attach before submitting |
| A connection's tiles waited after "Sync now"; "1 records"; an install plan printed "Native: true" | Ferrum-Frontend PRs 19 and 20: the list looks again every 3 seconds while work waits; "1 record"; "Built into the server: it runs in the server itself, not in a sandbox" |
| An objective's owner was left empty and the save refused | ferrum-app-objectives 0.1.1: whoever records it, unless they name someone |
| A submitted application linked its draft fee invoice, which Your invoices does not open ("invoice not found") | ferrum-app-government 0.1.3: linked once issued; until then the application says the invoice is not issued yet |
| At phone width the portal bar showed two of its five pages, the one shown hidden, with nothing saying the rest could be scrolled to | This PR's `server/shell/portal.js`: on a phone the pages open below a button naming the one shown; Escape or a tap elsewhere closes them |
| "Support access ends not given" on the customer's page, "support access ends not given: ask them for a new health key" on the partner's | ferrum-app-control 0.1.3: "Support access is not given", or "has ended" |
| A second statement for a month refused as "Northwind Partners's statement for October 2026 has been issued" | Control 0.1.3: "Northwind Partners already has its statement for October 2026" |
| The signed contract linked Sign's sealed PDF, kept with Sign's audit: Dana, who approved the contract, was refused it ("not permitted to read signevent") | ferrum-app-contracts 0.1.3: the contract keeps a copy of its own |
| IoT Edge's CI failed twice on its rebuild with "deadlock detected" at boot: two tests of one binary reset the database at once | ferrum-app-edge 858f1ca: the tests of a binary take turns |
| A paid app could be bought, refunded and disputed only through the store's API, and its licence given only through Ferrum's | ferrum-store c252993: buying on the app's page, Your purchases, publishers' answers and administrators' decisions in the pages; this PR: the install plan lists the sold apps and their licences; Ferrum-Frontend PR 21: the install dialog asks for the licence, and an installed app whose licence lapsed says so |
| The licence's end read "2026-12-05T06:09:46Z" | Ferrum-Frontend PR 21: "5 Dec 2026" |
| A refunded purchase still read "Ends 2026-12-05" | ferrum-store f9655ac |
| Your orders was offered on the portal and drew a blank page: the workspace had ERP recorded at 0.7.1, so the shell served 0.7.1's interface while the server ran ERP 0.8.0's code and manifest, whose page that interface does not know | PR 58: a compiled-in app's interface is this build's, as its code and manifest are |
| An order billed in full before it was received read "To deliver" | ERP PR 8 (0.9.0): "Billed, to deliver" |
| A deferred invoice was refused for a company made before ERP 0.9.0: it had no Deferred Revenue account, and Create Standard Chart of Accounts, which the refusal points to, did nothing for a company that has accounts. The refusal named the default by its field key | ERP PR 10 (0.10.1): the action adds the standard accounts a company lacks and links its empty defaults (walked: Deferred Revenue, Prepaid Expenses, Accrued Income and Accrued Expenses added with their numbers, both defaults linked); PR 58: the refusal names the defaults in words |
| An accrual with Reverse It ticked on 5 November and then dated 31 October kept a reversal day of 6 November: the form's preview had worked out "the day after" from the first date. Exchange revaluations did the same | ERP PR 11 (0.10.2, Finance 0.3.1): the day after is taken when the entry is submitted |
| ERP's update in the App Manager (recorded 0.7.1, the build 0.10.1) listed all 34 of its entities under New entities, so the consent said nothing of what the update brings | PR 58: a compiled-in app's update lists only the entities the workspace does not have yet |
| Actions' own messages ("1 deferred line recognised through 2026-10-31") showed as "<action>: done" | Ferrum-Frontend PR 22: the record page shows the action's message; each app shows it once rebuilt against it |

After the fixes, the server was rebuilt with the new pins and the walk repeated each: the rollout headed
"0.5.0" and the console named it so; the partner page's eleven fields were named; Cyril read "1 month"
and, on the partner page, that it is for a partner's members; Lena's invoice opened (SINV-2026-00001,
EUR 950.00 due); a new direct message from Kim was answered with Reply, whose box was empty, and went out
as a direct status naming @kim; the approved contract without its PDF no longer offered signature.

With Government 0.1.3, Control 0.1.3, Contracts 0.1.3 and the bar, the server was rebuilt and each
walked again: the fee's application said its invoice was not issued yet, and linked it once issued; at
375 px the bar read "Walk · Your services ▾ · Sign out", the button listed all five pages with the one
shown marked and closed on Escape and on a tap elsewhere, and on a wide screen the pages stayed in the
bar; the statement refused a second time in the new words, and Pat's page read "support access is not
given: ask them for a new health key"; a second contract, CTR-2026-00003, went to its signature and
Dana opened its signed PDF (application/pdf, two pages: the signed page and the certificate).

On 5 November, with ERP 0.10.2, HR 0.7.1 and this PR's fixes, the server was rebuilt and the
accounting walked again: the App Manager's update of ERP (recorded 0.7.1) listed no new entities and
recorded 0.10.2; Walk Properties' Create Standard Chart of Accounts had added Deferred Revenue,
Prepaid Expenses, Accrued Income and Accrued Expenses with their numbers and linked both deferred
defaults; and Recognise Deferred Revenue and Expense, through yesterday, said "1 deferred line
recognised through 2026-11-04" (3,080.00 recognised in all, 308 days at 10) where it had said only
"done".

The onboarding refusal looked swallowed at first; it was a toast that had faded before the page was
read, and shows ("no shared cell in eu has room").

## What it does not show

- Each app bundles its own record pages, so the Frontend fixes reach an app when it is built again.
  All 36 apps with interfaces were rebuilt against Ferrum-Frontend 80ff179 as patch releases and this
  PR's lock takes them; after the rebuild, Governance and Risk's acceptance read "Single cell for the
  EU region" under its number.
- Stripe: the walk paid through the store's test provider. Stripe itself was not reached; the store's
  tests run it against a stand-in, the card's round trip from the pages included.
- The store's CI: GitHub would not start its jobs ("recent account payments have failed or your
  spending limit needs to be increased", a private repository); its 18 tests passed on this machine.
- A screen reader: accessible names were read from the browser's accessibility tree, not heard.
- The stand-ins: Mastodon and the cell's tooling were small programs on this machine, not the real ones.
