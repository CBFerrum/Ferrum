# A browser on a running server, 6–7 November 2026 (night): Property, Retail, Government, Logistics, Control Plane, IT Services and Healthcare

The walk server (port 5191) ran Ferrum PR 89's builds: first with Property 0.7.1, Retail 0.3.1 and
Government 0.2.1 pinned, then from 7 November with Logistics 0.3.0 as well (PR 89's head
66e3097). It was rebuilt at 02:34–02:38 IST on 7 November on Ferrum main 138554d (PR 89 and PR 90
merged, the shell on Ferrum-Frontend 922fcfb), after which the App Manager offered Property 0.7.2
and Retail 0.3.2. The walk used workspace walk-intranet in the desktop app's browser pane
(Chromium 152), signed in as its administrator, and on the portal as Olive Owner and Priya Vendor
with codes from the development mailer's outbox. ERP had no company there, so the walk made one
through ERP's own forms: Walk Estates (INR), its standard chart of accounts, and the items the apps
invoice with.

## Property 0.7.1, then 0.7.2

| What | Where | Seen |
|---|---|---|
| The update | Apps → Installed → Property → Update to 0.7.1 | Property moved from 0.6.0 to 0.7.1 in place. |
| Settings, then invoicing by upkeep | Property Settings: Walk Estates, items RENT, SVC, PAYOUT | Within a minute of saving, upkeep invoiced A1's lease for Oct and Nov 2026 (1,200 each) as draft ERP sales invoices, each linked from its Rent Invoice. |
| Service charges by floor area | Units A1 60 m², A2 40 m²; budgets for Quay Works: 2025 (Cleaning 3,000, Security 2,000) and 2026 (6,000, 4,000), each put in use | A new lease of A2 to Ivan Index from 1 Oct 2025 (1,000 a month, pays service charge), once activated, was invoiced by upkeep for its 14 periods to Nov 2026. Each line was 40% of its year's budget for the period's days: 169.86 for Oct 2025, 164.38 for Nov 2025, 339.73 for Jan 2026, 306.85 for Feb 2026. |
| Rent indexed | Rent index Walk CPI: 100 from Oct 2025, 104 from Oct 2026; the lease capped at 3% | From 1 Oct 2026 the rent was 1,030, not 1,040. The lease's rent reviews kept "2026-10-01, 1030, Indexed to Walk CPI: 104 over 100". |
| Reconciled | 2025 budget → Reconcile, 6,000 spent | Reconciled. Balance row: A2's lease, 92 days, 40% share, charged 504.11, due 604.93, difference 100.82, and an invoice to Ivan Index for it. |
| Owner's statement | Owner Statements → Quay Works, Oct 2026, expense "Gardening" 150 | After A1's October invoice was submitted (SINV-2026-00001) and paid (PAY-2026-00001): collected 1,200.00 (A2's draft invoice not counted), fee 10% = 120.00, expenses 150.00, payout 930.00. Bill The Payout made a draft purchase invoice from Olive Owner, numbered by the statement (OS-2026-00001), for 930.00 of Owner Payout; submitted, it is PINV-2026-00001. |
| 0.7.2 | Apps → Installed → Property → Update to 0.7.2 (after the rebuild) | Property moved to 0.7.2 in place. |
| The billed statement on the owners' portal | Olive Owner signed in with a code → Properties you own | Quay Works, 2 of 2 units let (A1 to Lena Lessee, A2 to Ivan Index, until 2027-09-30); Rent: October 2026 invoiced 1,200, still owed 0 (the drafts not shown); Statements: OS-2026-00001 · 2026-10-01 to 2026-10-31, collected 1,200, fee 120 (10%), expenses 150, her share 930, payout "930 to pay". |

## Retail 0.3.1, then 0.3.2

| What | Where | Seen |
|---|---|---|
| Install | Apps → Store → Retail | Retail 0.3.1 installed with its entities listed. |
| Stores and stock | Warehouses MG Road and Indiranagar, both Is A Store; STE-2026-00001 receipt of 5 kettles to MG Road and 2 to Indiranagar | |
| Ship from store | SO-2026-00002 for 2 kettles, then Ship From Store with no store named | The shipment chose MG Road, which holds the most of those holding all of it. Ship It (BlueDart, BD123456790IN) made and submitted the store's delivery note. SFS-2026-00002 is Shipped. |
| Returned to another store | SINV-2026-00002 for the order; Store Return at Indiranagar, 1 kettle, Changed Mind | Taken Back. A credit note was made through ERP, and the goods came back by return delivery note DN-2026-00002, against the note that sent them. |
| Stock by Store | Retail → Stock by Store | KETTLE 3 at Indiranagar (2 + 1 returned) and 3 at MG Road (5 − 2). |
| An order naming no warehouse, on 0.3.2 | Apps → Installed → Retail → Update to 0.3.2; SFS-2026-00001 (SO-2026-00001, its line naming no warehouse) → Ship It (Delhivery, DLV1107001) | 0.3.1 had refused it (below). Now Shipped: delivery note DN-2026-00005 submitted with its line and its default warehouse both MG Road, and the order's line delivered 2 of 2, the order To Bill. |
| Settings for transfers and franchises | Retail Settings: Goods in Transit (a warehouse made for it), items ROYALTY and MKTFEE | |
| A transfer beyond the giving store's stock | ST-2026-00001: MG Road to Indiranagar, 5 kettles → Send It; then Turn It Down ("MG Road has one kettle left; ask Indiranagar.") | Send It was refused: "the store does not hold it: Steel Kettle (1 of 5)". Turned down: Rejected, the reason added to its note. |
| A transfer part received, the rest lost | ST-2026-00002: Indiranagar to MG Road, 2 kettles → Send It → Some Of It Arrived (1) → The Rest Is Lost ("One fell off the van at a junction and was crushed.") | Sent by STE-2026-00002 into the goods in transit; Part Received by STE-2026-00003 (1 of 2); Closed Short with the other written off by STE-2026-00004 (Material Issue), what happened in its note. Stock by Store: MG Road 2, Indiranagar 1. |
| Click and collect, brought from another store | A sales order for 2 kettles naming Indiranagar (which held 1) → CC-2026-00001 → It Is Ready; Bring It From Another Store (MG Road); the transfer sent and arrived; It Is Ready; Hand It Over with a wrong code, then its own | It Is Ready was refused: "not in the store yet: Steel Kettle (1 of 2)". Bringing it made ST-2026-00003 (MG Road to Indiranagar, 1, "For click and collect CC-2026-00001"); once it arrived, Ready, its pickup code on the record. A wrong code was refused in the dialog ("the pickup code does not match"); the right one made it Collected, with delivery note DN-2026-00006 submitted for 2 from Indiranagar. An order naming no warehouse was refused for collection ("is not where the order ships from"): a collect order is for the store the order names. |
| A franchisee on the portal | Customer Fran Foods (fran@franchise.test), agreement Whitefield 2026–2027 at 5% royalty and 2% marketing; Fran a portal person, signed in with a code → Your franchise → Report your sales | The agreement showed with "Nothing reported yet". A January 2028 period was refused ("falls outside the franchise agreement"); October 2026 at 100,000 made ROY-2026-00001 (fees 7,000, Draft), and a report overlapping it was refused ("ROY-2026-00001 reports some of these days already"). |
| Invoiced through ERP | ROY-2026-00001 → Invoice It; the invoice issued | Invoiced: a sales invoice to Fran Foods for 7,000 (Franchise Royalty 5,000, Franchise Marketing Fee 2,000), issued as SINV-2026-00004. On Fran's portal the report read Invoiced, its fees opening the invoice on Your invoices: "INR 7,000.00 to pay", royalty and marketing fee for 2026-10-01 to 2026-10-31. |

## Government 0.2.1

| What | Where | Seen |
|---|---|---|
| A type with stages | Permit type Street Food Stall: 1 month, a Site inspection stage, renewable within 45 days, on the public register | |
| Through its stages | APP-2026-00001 for Priya Vendor: Submit It → Take It Up → Pass This Stage (note and findings) → Approve It (conditions) → Issue the Permit | PRM-2026-00001, valid from 7 Nov to 7 Dec 2026, keeping its conditions. |
| Renewed by the officer | PRM-2026-00001 → Renew It | APP-2026-00002, "Renewal of PRM-2026-00001". Taken through its stage and issued as PRM-2026-00002, running on from the day after the first ends: 8 Dec 2026 to 8 Jan 2027, its conditions carried over. The toasts read "Now Submitted" and "Now Issued; made PRM-2026-00002" (PR 89's action messages). |
| Holder told when renewal opens | PRM-2026-00003 for Ravi Juice, issued and left alone | Upkeep set its Reminded On to 7 Nov, once its renewal had opened. PRM-2026-00001, already renewed, was not reminded, as the rule says. |
| The public register | `/api/site/walk-intranet/government/register?format=json`, and `format=csv` fetched with no cookies | Both answered 200 without sign-in (the CSV as `text/csv` with `public, max-age=300`): number, type, holder, issued, expires, status and conditions for the three permits, and no addresses. |
| Renewed from the citizens' portal | Priya Vendor made a portal person (Administration → Users → New user, Kind: Portal person); a further permit issued to her by the officer (PRM-2026-00004, 7 Nov to 7 Dec); Priya signed in with a code → Your services | Her three applications showed with their stage, findings and conditions. Of her permits, PRM-2026-00002 said "It may be renewed from 2026-11-24.", PRM-2026-00001 (its renewal live) offered nothing, and PRM-2026-00004 offered Renew it: that made APP-2026-00005 in draft, and Submit sent it in (Submitted). Taken through its stage by the officer, it was issued as PRM-2026-00005, 8 Dec 2026 to 8 Jan 2027, which Priya then saw on the portal ("It may be renewed from 2026-11-24."). |
| The holder told | Notifications queued for Priya | "Your application APP-2026-00004 is submitted", "… is approved", "Your permit PRM-2026-00004 is issued, valid 2026-11-07 to 2026-12-07", "Your permit PRM-2026-00004 may be renewed: it ends on 2026-12-07" (upkeep, a minute after issue), then the same three for APP-2026-00005 and PRM-2026-00005, each queued to be mailed. |
| A case | Case category Street lighting, 7 days to resolve; Priya → Your cases → Raise a case | CASE-2026-00001 "Lamp out at the night market", Open, due 2026-11-14. The officer setting it Resolved with no resolution was refused ("a resolved case says how"); with one, it was Resolved On 2026-11-07, not overdue, and Priya saw "Resolved · The lamp was replaced on 7 November; …" on the portal. |

## Logistics 0.3.0

| What | Where | Seen |
|---|---|---|
| Install | Apps → Store → Logistics | 0.3.0 installed, listing FuelLog among its entities. |
| A day planned | Quay Depot; van KA-01-VAN-7 (500 kg, 12 a km); driver Walk Admin, the signed-in administrator; DN-2026-00003 for Ivan, submitted; a draft return note for Ivan's second kettle; TRP-2026-00001 for 7 Nov with three stops | Load 5 kg, 21.7 km in a straight line. Order the Stops went by window, then nearest: Bo Books (window from 00:00), the pickup (no window), then Ivan's delivery (from 08:00), 18.6 km. Dispatch It: "Now Dispatched". |
| The driver's page | `/logistics/drive` | TRP-2026-00001's stops in that order, with their windows, "Collect from …" for the pickup, and Delivered or Collected and Not delivered or Not collected for each. "Log fuel" above. The day's trips were kept on the device under the workspace's offline lease (1 trip, 7 Nov). |
| A proof recorded there | Stop 1 → Delivered | The dialog refused an empty proof ("Name who took the goods.") and one without a photo ("Take a photo of the goods or of the signature."). The pane has no file picker, so a generated PNG was put in the photo input from the console. Saved, the proof went through the phone's store and was sent: stop 1 read "Delivered Late", its window having closed at 01:00. |
| A collection | Stop 2 → Collected (handed over by Ivan, a photo) | Stop 2 Collected; its return note submitted as DN-2026-00004, 1 kettle back into MG Road. The proofs kept their photos (/files/…) and when they happened, about 80 ms before the server stored them. |
| The server gone | The walk server stopped at 02:34:39 for its rebuild, the driver's page left open | Within a minute the page read "No connection: your trips as of 02:34 GMT+5:30." and still listed the trip's three stops from the phone's copy. |
| A proof with no connection | Stop 3 → Delivered (received by Ivan Index, a photo, "Recorded with no connection") | Saved at 02:35:59: "1 proof is kept on this phone, to send.", the stop "Kept, to send". Send now, with no connection, kept it. |
| Sent when the server answered | The server back at 02:38:24 | The page sent it by itself: the proof stored at 02:38:26 (POST 201), with happened_at 02:35:59, and stop 3 Delivered and not late (its window 08:00–20:00). The waiting line went. |
| The trip completed | TRP-2026-00001 → Complete It | Completed, cost 223.20 (18.6 km at 12 a km). The home showed "3 of 3 stops done". |
| Fuel and the odometer | Log fuel: 20 L, 2,100, 12,000 km; then 25 L, 2,600, 11,900 km the same day | The first moved the van's reading to 12,000. The second was refused: "the odometer read 12000 km by that day already: it never goes back"; at 12,300 it was saved and the van read 12,300. |
| A service due by the kilometres | The van: service every 10,000 km, last at 2,000 | Service Due ticked on save (12,300 ≥ 12,000). It Was Serviced at 12,200 was refused ("the odometer reads 12300 km already"); at 12,350, next on 7 May 2027, the van read 12,350, last service 12,350, and was no longer due. |
| Fleet Costs | `/logistics/r/logistics_fleet_costs`, run with its defaults (this month) | KA-01-VAN-7: 1 completed trip, 18.6 km, 223.20, 45 L, 4,700.00, 300 km by the odometer, 15 L per 100 km. |

## Control Plane 0.4.1

| What | Where | Seen |
|---|---|---|
| Install | Apps → Store → Control Plane | 0.4.1 installed in walk-intranet, its entities listed. |
| Cells reporting | Cells in-west-1 (India) and eu-central-1 (Europe), capacity 50, each with its report token; `POST /api/site/walk-intranet/control/report` from the page with no cookies | in-west-1: Healthy. eu-central-1, its disk at 93%: Degraded, "disk at 93%". A wrong token: 403 "that is not this cell's token". |
| A public incident | Incidents → New: "Pages slow in Europe", Minor, Investigating, cell eu-central-1, an update | Saved Investigating with its started and posted times (the Public box: see below). |
| The status page | `/control/status/walk-intranet` | "Some systems are degraded"; the incident with its update; Europe: eu-central-1 Degraded; India: in-west-1 Operational; Coming maintenance: none planned; "Read … UTC; read again every minute". |
| Its JSON | `/api/site/walk-intranet/control/status` with no cookies | 200, `cache-control: public, max-age=30`: regions, cells and states, the incident and its updates, and no customer named. |
| Read again every minute | eu-central-1 reporting its disk at 61%; the incident given a Resolved update | The open page turned, without a reload, to "All systems operational", "No open incidents", both cells Operational, and the incident under "Resolved in the last three days" with both its updates, newest first. |

## IT Services 0.2.4

| What | Where | Seen |
|---|---|---|
| Install | Apps → Store → IT Services | 0.2.4 installed in walk-intranet. |
| Discovery | `POST /api/app/itsm/discovery` as the administrator: source scanner-dc1 with vm-host-2 and db-server-1 (runs on host-2), each with serial, location and details | Created 2, both Live, db-server-1 with a Runs On relation to vm-host-2. An item said to run on itself was made without that relation, refused as "an item does not relate to itself". |
| What people keep | db-server-1 renamed Orders DB and given an owner in its form; scanner-dc1 reporting it again under another name, in DC2 | Updated 1: still Orders DB with its owner, now in DC2, its last seen moved on. Another source (cloud-inventory) reporting the same serial was refused: "Orders DB is reported by scanner-dc1, another source". |
| Who reports | Mira Member (no discovery role) posting a report | 403: "discovery reports come from a Discovery Agent or a Configuration Manager". |
| A second person deciding | CHG-2026-00001 "Upgrade Orders DB to PostgreSQL 18", Normal, with its back-out plan, raised and submitted by the administrator; Mira given Change Manager (Administration → Users → Roles) and signed in | Approve It by the administrator who raised it: 403 "a change is decided by someone other than who raised it". Mira saw Approve It and Reject It on it; approved, it is Approved, decided by member@walk-intranet.test. |

## Healthcare 0.1.7

| What | Where | Seen |
|---|---|---|
| Install and setup | Apps → Store → Healthcare; settings (Walk Estates, item CONSULT); consent form Treatment consent (version 1); type Consultation (15 minutes, needing it); practitioners Dr Mira Member (Mira's sign-in address, 500) and Dr Raj Rao (600), both seeing patients all Saturday and Sunday; patients Asha Rao and Bina Das, both consenting | Five visits booked for 7 November: APT-2026-00001 to 00005. |
| A withdrawn consent | Bina's consent → Withdraw It; then her visit with Mira (APT-2026-00002) → Check In | Withdrawn, with when. Check-in refused: "the patient has not consented to Treatment consent, version 1". |
| Cancelling | APT-2026-00002 → Cancel It | Cancelled. Its 10:30 with Mira then took another booking (APT-2026-00007, Asha). |
| A visit missed | APT-2026-00003 (Asha with Raj at 11:00) → Did Not Come | No Show, nothing billed. It was accepted at 03:35, before the visit's time (see Found and fixed). |
| A practitioner's own view | Mira given Practitioner and signed in; reception had checked Asha in for 10:00 (Mira) and 12:00 (Raj) | Her menu: Home, Appointments, Patients, Patient Consents, Practitioners, Appointment Types. Her own visit: Complete the Visit made it Completed, its invoice drafted. Raj's visit: refused, "the visit's practitioner completes it" (the toast on the page, 403 by the API). Booking a visit herself: 403 "not permitted to create appointment". |

## Not walked, and why

- **A page opened offline.** This desktop browser pane cannot register service workers at all: Logistics' `/logistics/sw.js` and the platform's own `/sw.js` both fail with "an unknown error occurred when fetching the script". The server serves both correctly (200, `text/javascript`, `Service-Worker-Allowed: /logistics/`, with or without cookies). The page left open while the server was down was walked instead (above).
- **A case going overdue.** It needs its days to pass.
- **Control's releases, rollouts and partners**: not this walk.

## Found and fixed

| Found | Fix |
|---|---|
| Retail: Ship It refused an order naming no warehouse (422, `items[0].warehouse is required`), because ERP's "Create Delivery Note" copies the order's warehouse. The action dialog showed nothing. | Retail 0.3.2 (3e3c5c6) maps the store's delivery note itself, every line from the store; a collect order names the order's conflicts first. The dialog's silence is fixed in Ferrum-Frontend PR 44. Walked on 0.3.2 (above). |
| Property: Reconcile's "Charged" came from the budget's share worked out once (504.11), while the invoices bore 504.10. | Property 0.7.2 (803b5fa) reconciles against what the invoices bore. |
| Property: the index review said "104 over 100" without the 3% cap that set the figure. | Property 0.7.2: "held to its cap of 3%". |
| Property: Owner Payout Item showed under "Additional fields". | Property 0.7.2: its own heading, Owners. |
| Property: the owners' portal showed A2 at "1,000 monthly" after its rent was indexed to 1,030; "Your home" read the starting rent too. | Property 0.7.3 (ferrum-app-property PR 9): both portals show the rent in effect on the workspace's day, its reviews counted. |
| An action with a confirmation and parameters (Reconcile) asked twice. | Ferrum-Frontend PR 44: it asks once. |
| Logistics: a stop's Return Note offered every delivery note, not only draft returns; a proof sent at once still toasted "A proof kept on this phone was sent." | Logistics 0.3.1 (61d3573). |
| Logistics: the home never linked Fleet Costs (reachable only by its address) and showed "Cost 223.2". | Logistics 0.3.2 (ferrum-app-logistics PR 5): a Fleet costs link, and costs, days and readings formatted; with it, a signature drawn on the driver's page, kept offline with the proof. |
| Retail: Your franchise read "sales 100000 · fees 7000". | Retail 0.3.3 (ferrum-app-retail PR 4): sales and fees as money. |
| Government: "Apply for a permit" read "fee 0" for a type with none. | Government 0.2.3 (ferrum-app-government PR 3): its fee as money, or "no fee". |
| Healthcare: Did Not Come was accepted hours before the visit's time. | Healthcare's next patch (after the 7 November rebuild sweep): refused before the visit begins on the workspace's clock. |
| A new form showed a checkbox whose default is `1` unticked, yet saved it ticked: Control's new incident looked private and was saved public. | The Frontend's prefill ticks a box on `true`, 1 or "1" (the Frontend PR of 7 November); Control Plane 0.5.1 (ferrum-app-control PR 7) and Social's next rebuild default to `true`. |
