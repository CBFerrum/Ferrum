# Handover

Where the work stands, kept here rather than in anyone's memory (specification §19.5). Newest first.
What is true of each requirement is in the registry; this says what happened and what comes next.

## 2026-11-07, evening: a workspace's own interface words; duplicates named; watermarks; key results graded; datasets that pivot

**State:** this PR follows PRs 94 and 95. Its lock takes every app at its main of the evening of 7
November, each app's interface built with Ferrum-Frontend d5d98f2 (PRs 47 and 48), which
FRONTEND_REV now names. With them: Digital Assets 0.8.0 and 0.8.1, Objectives 0.6.0 and 0.6.1,
Metrics 0.4.0 and 0.4.1, Government 0.2.7 and 0.2.8, and every other app a patch. No release was
tagged.

| Where | What |
|---|---|
| Journeys | Walked on this PR's build ([the walk](../platform/evidence/browser-2026-11-07-words-and-watermarks.md)): a workspace's own Arabic word for Save on a CRM record; a duplicate Permit Type named by its field; a PDF asset's renditions as pictures; a shared link's watermark; Letters and key results graded on closing; a dataset pivoting over named values, built in Studio's builder; CRM, Drive and Field Service's board right to left |
| Ferrum PR 94 | InterfaceWord: a workspace's own words for its interface in a language, which every page reads as it loads. A unique index's refusal now names its fields ("another Permit Type already has this Type Name"), and an exclusion's words survive a restart (FERRUM-UX-006) |
| Ferrum PR 95 | A Studio pivot may name its values: those are its columns, in that order, whatever the records hold. Build scripts read their crate's directory when they run, so a target shared between checkouts no longer stops on one since deleted (FERRUM-STUDIO-001, FERRUM-ECOSYSTEM-010) |
| Frontend PR 47 | Admin, Interface words; pictures in attachment fields; the account page in Arabic and Hindi; a duplicate told from a version clash (`isStale`), in the form, a record's peek and Studio's builders |
| Frontend PR 48 | The report builder names a pivot's values: a field's options, yes and no, or typed |
| Digital Assets 0.8.0 | A shared link's watermark drawn across the pictures it hands out (FERRUM-ECOSYSTEM-012) |
| Objectives 0.6.0 | Scores on 1 to 5 or as letters, and key results graded with their objective (FERRUM-ECOSYSTEM-011) |
| Metrics 0.4.0 | A dataset pivots over the values it names, and compares a period with the one before (FERRUM-ECOSYSTEM-010) |
| Government 0.2.7 | Pass This Stage's Stage comes empty (the other session's fix) |
| Every app | Rebuilt on d5d98f2, a patch each. Apps' own screens go by the line's start and end, except what they place by coordinates (boards, timelines, charts, canvases, handles). The conversion is `rtl_convert.py`, run before each build; 239 lines across 20 apps |
| How it was checked | Each PR above passed its CI on this machine before it merged, at the head that was checked: `scripts/app_ci.sh` for the apps; clippy and the kernel's and Studio's suites for Ferrum; typecheck, the tests, the build and the audit for the Frontend. Digital Assets 0.8.0 failed there once, on a unit test's threshold, and once for disk, before it passed |

**Evidence:** the walk; Ferrum `crates/kernel/tests/collab_it.rs` (the words, a base and a
regional language, the duplicate's words, the tag rule) and `crates/studio/tests/studio_it.rs`
(named pivot values); `ferrum-app-dam:tests/dam_it.rs` (a watermarked share);
`ferrum-app-objectives:tests` (key results graded on close); `ferrum-app-metrics:tests/datasets_it.rs`
(a certified pivot dataset loaded as CSV).

**Next:** Ferrum-Frontend's fixes the walk found: an action's once-shown link, code or token shown in a dialog to copy; untranslated English isolated on right-to-left pages; the pivot values' checkbox names. Then apps' own words through the kit's t(), listed for each app on Interface words. And Government's case going overdue once the workspace's day has turned.

## 2026-11-07, night: pages right to left; the records pages in Arabic and Hindi; PDFs' first pages; stages side by side

**State:** this PR follows PR 92. Its lock takes every app at its main of the night of 7 November,
each app's interface built with Ferrum-Frontend 63280d3 (PR 46), which FRONTEND_REV now names:
Digital Assets 0.7.0, Logistics 0.3.4 and 0.3.5, Property 0.7.5 and 0.7.6, Government 0.2.5 and
0.2.6, and every other app a patch.
No release was tagged.

| Where | What |
|---|---|
| Journeys | The records pages walked right to left, in Arabic ([the walk](../platform/evidence/browser-2026-11-07-right-to-left.md)): a list, a record and the account page, before and after the fixes they found (FERRUM-UX-006) |
| Journeys | A PDF uploaded to Drive and made an asset, on this PR's build ([the walk](../platform/evidence/browser-2026-11-07-pdf-renditions.md)): its first page drawn as its thumbnail and preview, text in a standard font included (FERRUM-ECOSYSTEM-012) |
| Frontend PR 46 | Right to left, icons that point along the line point the other way; margins, paddings, borders, insets, alignment and corners go by the line's start and end; tab strips' and column resize handles' arrow keys step along it. The records pages' words (the list's count, views, toolbar and search, the form's tabs, actions and toasts, child tables, link fields, the account page, the query builder) are in Arabic and Hindi, and a test keeps every word the interface asks for in both catalogs |
| Digital Assets 0.7.0 | A PDF's first page drawn as its renditions, with hayro in pure Rust (FERRUM-ECOSYSTEM-012) |
| Logistics 0.3.4 | A stop's weight, when not typed, from its note's lines and each item's weight (FERRUM-INDUSTRY-009) |
| Property 0.7.5 | A period across two budget years bears each year's days at that year's rate (FERRUM-INDUSTRY-008) |
| Government 0.2.5 | Review stages side by side (FERRUM-INDUSTRY-010) |
| The Control Plane 0.5.2 | A release rolled out to the status page walked: qualified by a second person, rolled out in waves by a Fleet Operator, its coming maintenance shown in each cell's window, gone while halted and back when resumed (FERRUM-CLOUD-004) |
| Every app | Rebuilt on 63280d3, a patch each: its pages mirror right to left, and the records pages' words in Arabic and Hindi |
| How it was checked | Each app PR above passed its CI on this machine before it merged, at the head that was checked (`scripts/app_ci.sh`); Government PR 4 failed twice there first (a test that did not compile, then a role the test had not declared) and was fixed on its branch |

**Evidence:** the walks; Ferrum-Frontend `src/lib/i18n.test.ts` (both catalogs hold every word the
interface asks for; the arrow keys along the line); `ferrum-app-dam:src/renditions.rs` and
`tests/dam_it.rs` (a PDF's first page drawn, one with no pages saying so).

**Next:** the other session's walks, on this build, of Logistics 0.3.4's weights, Property 0.7.5's budget years and Government 0.2.5's stages side by side; Government's case going overdue once the workspace's day has turned; an attachment field showing the image it holds (an asset's renditions show as download links); apps' own screens right to left, one by one.

## 2026-11-07: Logistics offline with the server stopped; partners see apps' own health; definitions built, not typed; every app rebuilt

**State:** this PR follows PRs 89 to 91. Its lock takes every app at its main of 7 November, each
app's interface built with Ferrum-Frontend a64416a (PR 45), which FRONTEND_REV now names. With
them: Logistics 0.3.1 to 0.3.3, the Control Plane 0.5.0 to 0.5.2, IoT Edge 0.5.0 and 0.5.1,
Property 0.7.3 and 0.7.4, Retail 0.3.3 and 0.3.4, Government 0.2.3 and 0.2.4, Healthcare 0.1.9 and
0.1.10, Social 0.6.2, Metrics 0.3.2, and every other app a patch. No release was tagged.

| Where | What |
|---|---|
| Journeys | Definitions built with Studio's builder, on this PR's build ([the walk](../platform/evidence/browser-2026-11-07-definition-builder.md)): a dataset grouped across a link and a metric of one measure, a second measure refused; Studio's endpoints serve only a workspace that has Studio |
| Journeys | The other session's walks of 6 and 7 November ([the walk](../platform/evidence/browser-2026-11-07-fork.md)): Property's service charges, rent indexing and an owner's statement, billed and seen by its owner on the portal; Retail's ship from store (an order naming no warehouse among them), a return at another store, transfers between stores, click and collect brought from another store, and a franchisee reporting on the portal; Government's stages, renewals, public register, a renewal by its holder on the citizens' portal and a case raised there; Logistics' day, with the server stopped while the driver's page was open, and on this PR's build a signature kept on the phone and sent once the server answered; IT Services' discovery and a change approved by a second person; Healthcare's withdrawn consent, cancelled and missed visits and a practitioner signed in; the Control Plane's cells and public status page |
| Property 0.7.3 | The portals show a lease's rent in effect today, not its starting rent (from the 7 November walk) |
| Retail 0.3.3 | The franchise portal shows sales and fees as money (from the 7 November walk) |
| Government 0.2.3 | The portal shows a permit type's fee as money, or "no fee" (from the 7 November walk) |
| Healthcare 0.1.10 | A visit is marked missed only once it has begun: Did Not Come was accepted hours before (from the 7 November walk) |
| Logistics 0.3.1 | Return notes offered only where they belong; a proof sent at once says "Proof saved." (from the 7 November walk) |
| Logistics 0.3.2 | A signature drawn on the driver's page, kept offline with the proof; the home links Fleet Costs and shows costs formatted (from the 7 November walk) |
| Logistics 0.3.3 | Rebuilt with Ferrum-Frontend a64416a; the 0.3.2 signature pad walked offline on this PR's build: drawn with no photo, saved while the server was stopped, sent by itself once it answered |
| Ferrum PR 90, Control Plane 0.5.0, IoT Edge 0.5.0 | Installed apps' own health in the support report; IoT Edge's gateways counted there and in a customer's health for partners (FERRUM-IND-021) |
| Control Plane 0.5.1 | A new incident's Public box shows ticked, as it is saved (from the 7 November walk) |
| Control Plane 0.4.1 | Its public status page walked: cells reporting without sign-in, a public incident and its update, the page turning "All systems operational" by itself once resolved, the JSON cached publicly for 30 seconds and naming no customer (FERRUM-CLOUD-004) |
| Ferrum PR 91, Frontend PR 45, Metrics 0.3.2 | A field may say how forms edit it (`widget`). `studio_query` on a json field shows Studio's report builder in place, with the first rows the query gives beside it, or the JSON for those who prefer it, and in a workspace without Studio. Metrics' metric and dataset definitions are built so, not typed (FERRUM-ECOSYSTEM-010) |
| Frontend PR 45 | A new record's box whose default is 1 shows ticked, as the server saves it: `default: 1` showed unticked and saved ticked, so an incident meant to stay internal was published. Control's and Social's defaults are now `true` too |
| Every app | Rebuilt on a64416a, a patch each: PR 45's ticked default, and for those not yet on 922fcfb, PR 44's action dialog (every refusal said, one dialog for parameters and a confirmation) |
| How it was checked | Each app PR above passed its CI on this machine before it merged, at the head that was checked (`scripts/app_ci.sh`, `gh pr merge --match-head-commit`). Its interface check is the kit's own `check` against the sources' digest, as GitHub's ui job runs it: a rebuilt bundle's CSS differs between machines (Tailwind's classes), its JavaScript does not. It tests `--workspace` when the workflow does, finds the test databases a suite reads by its own default, and patches every platform crate. IoT Edge's suites ran one test binary at a time, the two failpoint suites with their feature, and `cargo audit` inside the repository. Its brokers (NATS, RabbitMQ, Kafka, InfluxDB, ClickHouse, S3) were not run here: those tests return early without their service. Cloud's interface was checked in the sweep; its Rust has not changed since its last passing GitHub run (4 November) |

**Evidence:** the walks; `crates/meta/src/catalog.rs` (a widget only on a json field);
`ferrum-app-edge:tests/edge_it.rs` (gateways heard from, quiet and with problems);
`ferrum-app-control:tests/control_it.rs`; `ferrum-app-healthcare:tests/healthcare_it.rs` (a visit
missed only once begun); Ferrum-Frontend `src/app/prefill.test.ts` and `src/app/queryinput.test.tsx`.

**Next:** the limits the registry still names that need no outside party: words translated past the kit's states and dialogs, pages checked one by one right to left, a page where a buyer confirms a charge their bank asks about, and renditions of a video's frame or a PDF's first page.

## 2026-11-06, evening: a clinic's day; records headed by a link; a person's own clock; every app rebuilt

**State:** this PR follows PR 88. Its lock takes every app at its main of 6 November's evening, each
app's interface built with Ferrum-Frontend a76295e (PRs 42 and 43), which FRONTEND_REV now names:
Healthcare 0.1.7, Government 0.2.1, Retail 0.3.1, Digital Assets 0.6.1, Property 0.7.1, Metrics 0.3.1,
Objectives 0.5.2, Commerce 0.7.9, the Control Plane 0.4.1, Restaurant 0.2.0, Logistics 0.3.0, Retail 0.3.2,
Property 0.7.2, and the rest a patch each. No release was tagged.

| Where | What |
|---|---|
| Journeys | A clinic's day walked through the pages ([the walk](../platform/evidence/browser-2026-11-06-clinic-day.md)): set up, a consultation booked, check-in refused until the patient consented to the form's current version, the visit completed into a draft invoice, hours and clashes refused, a follow-up covered by the paid visit with no second invoice (FERRUM-INDUSTRY-005) |
| This PR | A record headed by a link (29 entities, an appointment by its patient, a lease by its unit) is named and searched by the linked record's title, at most two steps on and only through open entities: in link titles, list search, global search and notifications. Lists and reads of such entities always carry `_titles` (FERRUM-IDENTITY-011) |
| This PR, Frontend PR 43 | A person may choose a time zone of their own on the Account page (the `timezone` preference, checked against the zone database): times show and are entered on it, while today, due dates and defaults stay on the workspace's day. The bootstrap and `/api/auth/me` give `display_timezone` (FERRUM-UX-006) |
| This PR | An Item Price is headed by its item's name; the store set up through its pages (evidence in the order walk) |
| Frontend PR 42 | A local domain added in Domains says it works at once |
| Healthcare 0.1.7 | What the walk found: the home page's day was UTC's; patients' names came from a list of at most 500; an empty clinic said nothing; a visit under way looked editable, the server refusing the change on save; new appointments now default to today |
| Every app | Rebuilt on a76295e: times on a person's own clock. Commerce, the Control Plane, Digital Assets, Property and Retail took today from UTC's calendar on some screens, now the workspace's day (`todayIso`); Logistics has the same, told to the session working on it |
| Retail 0.3.0 | Ship from store, returns to any store, Stock by Store and goods a collection lacks brought from another store; transfers taken in parts, what never arrived written off (FERRUM-INDUSTRY-003) |
| Property 0.7.0 | Owners' statements: rent collected less the management fee and the expenses charged, billed as the owner's share and seen on the portal (FERRUM-INDUSTRY-008) |
| Digital Assets 0.6.0 | The place and people a file shows and its licence's terms, read from XMP and IPTC; approval waits for releases when people are named (FERRUM-ECOSYSTEM-012) |
| Government 0.2.0 | Review stages with site inspections, conditions, renewals, notices to applicants and holders, a public register, and the workspace's day (FERRUM-INDUSTRY-010) |
| Metrics 0.3.0, Objectives 0.5.0 | As their registry entries say (FERRUM-ECOSYSTEM-010, 011) |
| Journeys | A restaurant's evening walked through the pages ([the walk](../platform/evidence/browser-2026-11-06-restaurant-evening.md)): an outlet, a menu and tables set up, a table's order through the kitchen, a sent line voided, the bill in ERP, a delivery, reservations (FERRUM-INDUSTRY-002) |
| Restaurant 0.2.0 | What the walk found: a voided dish is struck off its kitchen ticket, which the kitchen would otherwise still make; a line's state only the kitchen moves on; a delivery goes out once made; the floor starts an order for its table, a takeaway or a delivery; dishes named as the menu writes them; actions say what they did |
| This PR | An action whose answer has no `message` gets one from what it says, the record's new status and what it made ("Now Admitted; made STU-2026-00001"): the walks met "done" after most apps' actions |
| Logistics 0.2.0 | Collections with return notes, delivery windows and late stops, a vehicle out again the same day, fuel logs, services due, trip costs and Fleet Costs (FERRUM-INDUSTRY-009) |
| Logistics 0.3.0 | A driver's page that works offline: the day's trips kept under the lease, proofs kept with when they happened and sent when the connection is back (FERRUM-INDUSTRY-009) |
| Registry | Hospitality's entry names the hotel walk (FERRUM-INDUSTRY-001) |
| Journeys | A school's day, a nonprofit's year of giving and a building contract walked through the pages ([school](../platform/evidence/browser-2026-11-06-school-day.md), [giving](../platform/evidence/browser-2026-11-06-year-of-giving.md), [contract](../platform/evidence/browser-2026-11-06-contract.md)): every rule held, each refusal saying why (FERRUM-INDUSTRY-004, 011, 007) |
| Education 0.2.0 | Each class's roll call, saving a group's attendance at once; today's classes named and on the workspace's day; counts past 200; an empty office says where to start; a score's limit written plainly |
| Nonprofit 0.1.8 | The home's figures over every record: it asked for 1,000 rows and the server pages at 500; the workspace's year; members not yet paid; the books named first; records dated today |
| Point of Sale 0.4.4, Objectives 0.5.2 | The same cap: a loyalty balance at the till and the review's progress came from one page of 500 entries; both now read every page by cursor |
| Construction 0.1.7 | Amounts written as money; estimates waiting for an answer listed; the books named before the first claim |
| Retail 0.3.2, Property 0.7.2 | From the walks of the session that built them: ship from store for an order naming no warehouse (the delivery note made from the store itself); reconciled service charges from what the lease's invoices bore; index reviews saying what held the rent; the owner payout item under its own heading |
| IT Services 0.2.4 | The desk's rules walked ([the walk](../platform/evidence/browser-2026-11-06-it-desk.md), FERRUM-ECOSYSTEM-003): the desk names what a request asks for, who asked and when it is due, and when a change starts |
| This PR | The help center opens from a record's Help link: `/data/help/{entity}` was read as an app's records and sent back to `/data/` (`shell::DATA_PAGES`, FERRUM-UX-003) |
| How it was checked | From Point of Sale 0.4.4 on, each app's CI (fmt, clippy, its scenario tests on scratch databases, its interface's checks and build) ran on this machine before it merged, not on GitHub: `scripts/app_ci.sh`. On a disk short of room, `NO_DEBUG=1` builds the tests without debug info in a target of their own |

**Evidence:** the walks; `crates/kernel/tests/rules_it.rs` (a record headed by a link, through an open
and a restricted entity, and its search); `crates/kernel/tests/collab_it.rs` (the `timezone`
preference); `ferrum-app-healthcare:ui/src/day.test.ts`; `ferrum-app-restaurant:tests/restaurant_it.rs` (the
voided ticket, a state set by hand, the delivery held until made); `server/src/controllers/docs.rs` (an action described from its answer).

**Next:** the walk's fixes and a person's own clock checked on this PR's build in a browser.

## 2026-11-06, afternoon: an online order through the storefront; shoppers told; apps that serve sites

**State:** this PR follows PR 85. Its lock takes Commerce 0.7.8 and Website 0.1.3 (their handlers serve
pages), Digital Assets 0.5.0, Property 0.6.0, the Control Plane 0.4.0, Objectives 0.4.0 and Social
0.6.0. FRONTEND_REV takes Ferrum-Frontend PR 41 (3d36821). No release was tagged.

| Where | What |
|---|---|
| Journeys | Online order to return walked through the pages ([the walk](../platform/evidence/browser-2026-11-06-order-to-return.md)): a storefront on its own host, a guest's order by bank transfer, marked paid, shipped with tracking and invoiced, followed from the shopper's link, a mug returned as damaged and refunded with a credit note |
| Commerce 0.7.8 | The shopper is e-mailed through the server's mailer: the order, its shipping when the desk says so, money sent back; before, only events went out and nothing sent them. A shipped order reads Shipped, not Delivered. Its storefront says it serves pages |
| Website 0.1.3 | Its handler says it serves pages, so Domains offers its site |
| Frontend PR 41 | Domains lists the apps the server says serve a site (PR 85's `sites`), not every app |
| This PR | On a developer's machine (base domain `localhost`) a `*.localhost` name is a custom domain that works at once, so a site's own host can be walked locally (it needed a row made in the database) |
| Digital Assets 0.4.0, 0.5.0 | XMP and IPTC from any file; each asset seen whole (video, sound, PDF, picture) for whoever sees it; office documents' first pages as their renditions (FERRUM-ECOSYSTEM-012) |
| Property 0.3.0 to 0.6.0 | Owners on the portal; days given back when a lease ends sooner; rent indexed each year; service charge budgets reconciled once the year is over (FERRUM-INDUSTRY-008) |
| Control Plane 0.3.0, 0.4.0 | Hosting invoiced monthly through ERP by the control-erp connector, with people and storage beyond a workspace's terms charged at the month's peak (FERRUM-CLOUD-003) |
| Objectives 0.4.0 | A review page: each active objective, those at risk first, its key results' progress since a chosen day (FERRUM-ECOSYSTEM-011) |
| Social 0.6.0 | Best times to post, by weekday and hour on the workspace's clock (FERRUM-ECOSYSTEM-005) |

**Evidence:** the walk; `ferrum-app-commerce:tests/commerce_it.rs` (`returns_and_refunds_end_to_end`:
the shopper's mails for the order, its shipping and a refund, and none for a refund not to be told).

**Next:** the walk's fixes on a build in a browser; a person's own time zone.

## 2026-11-06, morning: the tenant's life on the image; one clock for every member's page

**State:** this PR follows PR 84. Its lock takes every app at its main of 6 November's morning, each
app's interface built with Ferrum-Frontend 24c3951 (PR 40), which FRONTEND_REV now names: the
Control Plane 0.3.1, Property 0.5.0, Objectives 0.3.1, Social 0.5.1, Digital Assets 0.4.0, Intranet
0.1.3, Website 0.1.2, the thirteen apps below and the rest a patch each. No release was tagged.

| Where | What |
|---|---|
| The image | The tenant's life walked through the pages on the released image (main at 5a6c4b7): sign-up with its e-mailed code, the free plan's people limit refusing a fourth, a larger plan, closing, read-only, deletion after the keep date ([the walk](../platform/evidence/image-2026-11-06-lifecycle.md)); FERRUM-JOURNEY-012 is implemented, waiting on a reviewer |
| This PR | What the walk found: a closed workspace names who closed it by their e-mail, not an id; its refusal says the account is closed or restricted, not "until it is settled"; an operator's task gives each field's reason, not only "validation failed". Admin → Domains offers a public site only for the apps that serve one, and a `site:{app}` domain is refused for any other (found walking Intranet and Website) |
| Frontend PR 40 | Every member's page shows the workspace's clock, naming its zone when the browser's is another, and date-and-time boxes read it; an app's pages take the clock from the page; `formatMoment` for screens with their own shapes. A draft numbered on submit is headed as a draft; an add-on's record says its app is not installed |
| Thirteen apps | Their own screens on the workspace's clock: Helpdesk 0.7.2 (with the conversation's days), CRM 0.6.6, Projects 0.6.7, Commerce 0.7.7, Bookings 0.4.6, Campaigns 0.3.5 (its schedule box too), Docs 0.7.2, Drive 0.14.2, Forms 0.2.5, HR 0.8.3, Point of Sale 0.4.2, Sign 0.2.7, IoT Edge 0.4.6 (a chart's axis on its whole hours). Public pages, outsiders' portals and Comms' calendar and chat keep the reader's own |
| Control Plane 0.3.0 | Hosted workspaces invoiced monthly through ERP by the connector control-erp (FERRUM-CLOUD-003); statements count what was invoiced |
| Property 0.3.0 to 0.5.0 | Owners' Properties you own on the portal; a lease ended early gives back what was invoiced past its last day as a draft ERP credit note; rent indexed each year within its cap and floor |
| Objectives 0.3.0 | A key result can name a certified metric (the connector objectives-metrics) |
| Social 0.5.0 | Engagement kept day by day, and the report Social Engagement by Day |
| Digital Assets 0.4.0 | XMP from any file and a JPEG's IPTC give an asset its title, caption, keywords, creator and copyright; keywords become its tags (FERRUM-ECOSYSTEM-012) |
| Intranet, Website | Walked in a browser on the walk server by the session that built them (FERRUM-ECOSYSTEM-001, 006); their fixes are Intranet 0.1.2 and 0.1.3 and Website 0.1.2 |

**Evidence:** the two walks; `server/src/tasks.rs` (`a_refusal_gives_each_fields_reason`), the
Frontend's `format.test.ts` and `client.test.ts`, the apps' suites (Helpdesk's days and IoT Edge's
ticks in a zone that is not the browser's), `server/src/packages.rs` (the two connectors fit).

**Next:** Website and Commerce say their handlers serve pages (`Site::serves_pages`, new here) in
their next releases; until those are pinned, Domains offers no app's public site. A person cannot
choose a time zone of their own. A site's own host is not walked: a domain
under the server's base is refused, so it needs a real name.

## 2026-11-06, night, later: the walks' fixes checked on their build; Commerce, Metrics and Social pinned

**State:** this PR follows PR 82. Its lock takes every app at its main of 6 November's night: Commerce
0.7.5, Metrics 0.2.0, Social 0.4.0, Digital Assets 0.3.0, and 38 apps' interfaces rebuilt with
Ferrum-Frontend bb60115 (ERP 0.14.3, Campaigns 0.3.4, HR 0.8.2 and the rest a patch each). No release
was tagged.

| Where | What |
|---|---|
| Evidence | On PR 82's build, a sent campaign's results, a signup form's HTML pasted into a page, settings opening as their record and an install dialog naming what it brings ([the walk](../platform/evidence/browser-2026-11-06-journeys.md), "The fixes, checked on their build") |
| Commerce 0.7.5 | A payment request's page asks Mark Paid for the amount and currency received, filled from the request: from that page it always failed |
| Metrics 0.2.0 | Certified metrics delivered to who follows them on the workspace's day (FERRUM-ECOSYSTEM-010) |
| Social 0.3.0, 0.4.0 | A Bluesky connector beside Mastodon's, and a picture with alt text on a post (FERRUM-ECOSYSTEM-005) |
| Digital Assets 0.3.0 | An approved asset shared outside the workspace by a link for one recipient (FERRUM-ECOSYSTEM-012) |
| Interfaces | Every app's interface rebuilt with Ferrum-Frontend bb60115, so record pages read empty numbers as the server does and settings open as their one record |

**Next:** the tenant's life on the image is still to walk.

## 2026-11-06, night: hire to retire, a hotel stay and marketing to consent walked; actions that pages could not run

**State:** this PR follows PRs 79 and 81. Its lock takes Campaigns 0.3.3, CRM 0.6.4, HR 0.8.1,
Hospitality 0.2.0, Projects 0.6.5, Sign 0.2.5, Bookings 0.4.4, Education 0.1.5, Field Service 0.4.1, Learn 0.6.2, Drive 0.14.0,
Objectives 0.2.1, Digital Assets 0.2.0, ITSM 0.2.0 (with its connector to Assets), Property 0.2.0,
Retail 0.2.0 and the Control Plane 0.2.0. FRONTEND_REV takes Frontend PRs 36 to 39 (bb60115). No release was
tagged.

| Where | What |
|---|---|
| Journeys | Hire to retire, the hotel stay again and marketing to consent walked through the pages ([the walk](../platform/evidence/browser-2026-11-06-journeys.md)); FERRUM-JOURNEY-003, 009 and 010 are implemented, waiting on an independent reviewer |
| This PR | An app's interface is stored again when its files are missing (the walk server's database recorded Mail's from a server that ran elsewhere, and its pages were blank); a settings link opens its one record; an install plan names the apps it brings; two compiled apps cannot give one report name, and an app whose requirement a compiled app does not meet fails the build's tests |
| HR 0.8.1, Hospitality 0.2.0, Bookings 0.4.4, Education 0.1.5, Field Service 0.4.1, Learn 0.6.2, CRM 0.6.3, Projects 0.6.5, Sign 0.2.5 | Actions declare the parameters they read, so record pages ask for them; those that need a time (Bookings' Reschedule, Field Service's Schedule) are left to the screens that offer one. Hospitality's desk payments become draft payment entries in ERP at check-out |
| Campaigns 0.3.2, 0.3.3 | It installs beside Mail 0.10 again; the review links to the settings that fix it; a signup form gives HTML to paste; its campaign report is Email Campaign Performance, apart from CRM's, whose report of the same name answered for it |
| CRM 0.6.4 | Dates are the workspace's, not UTC's: between the two midnights a deal won that day fell outside the day's report (Campaigns' CI failed so for its New York workspace) |
| Frontend PRs 36 to 39 | A page reads empty numbers as the server does; an account chosen at the accounts host is chosen; settings open as their one record; the install dialog names what it brings. The shell takes them with FRONTEND_REV; each app when its interface is next built |
| Control Plane 0.2.0 | A workspace is made on its cell from its setup, by a fleet operator or when a partner onboards a customer: FERRUM-PARTNER-001 and CLOUD-001 say so |
| Registry | OPS-006 (uploads in parts, Drive adopting them) and ECOSYSTEM-001 and 006 (Intranet, Website) are implemented; OPS-007, ECOSYSTEM-003, 011 and 012, INDUSTRY-003 and 008 say what their apps added (texts from the session that built them) |

**Evidence:** the walk; `server/tests/apps_api.rs` (a release's missing file stored again),
`server/src/packages.rs` (`compiled_apps_fit_together`: requirements and report names), the apps'
suites (CRM's now runs in a zone whose date is not UTC's).

**Next:** Commerce 0.7.5 declares what a payment request's page could not send, and the Frontend PR 39
image job was running: pin them when merged. Rebuild the
apps' interfaces with Ferrum-Frontend main, so their pages take PRs 36 to 39.
A draft journal entry is still headed by the end of its id; an add-on's entity opened before it is
installed says "not found"; pages show times in the browser's zone (FERRUM-UX-006). The tenant's life on
the image is still to walk.

## 2026-11-06, later: learning completion walked, a site without internet, a job finished offline, a leaver settled

**State:** this PR follows PRs 76 and 77. Its lock takes Learn 0.6.1, Field Service 0.4.0, the Control
Plane 0.1.7 and HR 0.8.0. No release was tagged.

| Where | What |
|---|---|
| Journeys | Learning completion walked through the pages ([the walk](../platform/evidence/browser-2026-11-06-journeys.md)): a course built, a member enrolled, the quiz failed once and passed, the certificate issued and checked on its public page; FERRUM-JOURNEY-011 is implemented, waiting on an independent reviewer |
| Learn 0.6.1 | A lesson's New quiz and New assignment make one in the lesson and pick it; they left for a blank form and the lesson lost its kind (found in the walk) |
| PR 76 | A site without internet takes a release in a bundle, loaded into one host or a site registry: FERRUM-OPS-003 is implemented |
| Field Service 0.4.0 | A technician finishes a job without a connection, the steps held and sent in order with their own times: FERRUM-FIELD-002 is implemented |
| Control Plane 0.1.7 | The public status has a page anyone opens; FERRUM-CLOUD-004 stays partial (signatures and provenance, certificate renewal, running restores) |
| HR 0.8.0 | A leaver's full and final statement: open claims and advances gathered, gratuity or a recovery added, one journal entry settling all of it; FERRUM-JOURNEY-003 waits only on its walk through the pages |
| This PR | Each sign-in at the accounts host keeps a cookie of its own, so tabs that set off together (every open tab, when someone signs out) no longer undo each other ("Signing in did not finish", seen in the walks) |

**Evidence:** the walk; `ferrum-app-lms:ui/src/screens.test.tsx` (New quiz, failing with the change
reverted), `ferrum-app-hr:tests/hr_it.rs` (`hire_to_retire`, the statement to its entry and back),
`deploy/offline/test_offline.py`, `ferrum-app-field-service:tests/field_service_it.rs` and its
`ui/src/held.test.ts`, `server/src/shell.rs` (`each_sign_in_keeps_its_own_cookie`).

**Next:** hire to retire through the pages, on a server with HR 0.8.0; marketing to consent, the stay,
and the tenant's life on the image. Pages show times in the browser's zone while Field Service shows the
workspace's (FERRUM-UX-006); CRM says nothing of the connector its orders need; an add-on's entity
opened before it is installed says "not found"; Sign shows a small file as "0.00 MB" until Sign 0.2.4
is pinned.

## 2026-11-06, morning: three more journeys through the pages, and what the walks found

**State:** this PR follows PR 75. Its lock takes ERP 0.14.2 (Procurement 0.1.3, Finance 0.3.2), Field
Service 0.3.1 and Projects 0.6.4, and FRONTEND_REV takes Frontend PR 34. No release was tagged.

| Where | What |
|---|---|
| Journeys | Procure to pay, project to profit and document to signature walked through the pages ([the walk](../platform/evidence/browser-2026-11-06-journeys.md)); FERRUM-JOURNEY-002, 005 and 008 are implemented, waiting on an independent reviewer |
| This PR | Signing in at the accounts host is choosing the account: the way back no longer asks "Continue" again |
| ERP 0.14.2 | Seven actions declare the parameters they read, so their record pages ask for them (a supplier quotation's supplier and a bank line's payment party could never be given before); a purchase invoice is not submitted with its draft's placeholder number |
| Field Service 0.3.1 | Time added by hand is read on the workspace's clock, as the job shows it, and none that ends in the future is taken |
| Projects 0.6.4 | Time entered on a timesheet's page bills at the activity's rate (the preview's zeros went back as choices) |
| Frontend PR 34 | Settings are headed by what they are, not by the end of an id |
| Registry | Six entries whose remaining gaps are outside hands are blocked on them: JOURNEY-013 (partner terms), JOURNEY-014 (a model), IND-020 and INDUSTRY-006 (safety review, hardware), EXT-007 and RELEASE-005 (the owner's support commitment) |

**Evidence:** the walk; `ferrum-app-erp:addons/procurement/tests/procurement_it.rs` and
`addons/finance/tests/finance_it.rs` (declared parameters, the payment party), `tests/erp_it.rs` (the
placeholder refused), `ferrum-app-field-service:tests/field_service_it.rs` (time to come refused),
`ferrum-app-projects:tests/projects_it.rs` (a row as the page sends it, failing with the fix reverted),
`server/src/controllers/accounts.rs` (signing in is choosing).

**Next:** pages show times in the browser's zone while Field Service shows the workspace's (one rule
for every page, FERRUM-UX-006); CRM says nothing of the connector its orders need; an add-on's entity
opened before it is installed says "not found". The other journeys through the pages: hire to retire,
marketing to consent, the stay, learning completion.

## 2026-11-06, early: three journeys through the pages, files streamed by four apps, axe in the Frontend's tests

**State:** this PR follows PR 74. Its lock takes Drive 0.13.0, Comms 0.10.0, Helpdesk 0.7.0 and Learn
0.6.0, and FRONTEND_REV takes Frontend PR 33. No release was tagged.

| Where | What |
|---|---|
| Journeys | Support to resolution, field work to invoice and lead to cash walked through the pages on a running server ([the walk](../platform/evidence/browser-2026-11-06-journeys.md)); FERRUM-JOURNEY-001, 004 and 007 are implemented, waiting only on an independent reviewer |
| Drive, Comms, Helpdesk, Learn | Downloads and attachments answer with the stored file, streamed by the server (PR 72); FERRUM-OPS-006 waits on resumable uploads |
| Frontend PR 33 | axe-core over every screen the render tests draw; FERRUM-UX-005 stays blocked on the manual audit |
| SDK PR 3 | Examples of a service app and of an industry pack in metadata; FERRUM-EXT-006 is blocked on publishing to npm (new BLOCK-NPM-PUBLISH) |
| Mutation paths | Versions and public links read one by one; FERRUM-PLATFORM-011 is blocked on the independent review |

**Evidence:** `docs/platform/evidence/browser-2026-11-06-journeys.md`; each app's suite for its streamed
files.

**Next:** the walk's other findings: date-and-time boxes read in the browser's zone while pages show
the workspace's, time by hand that ends in the future, no word that CRM's orders need the connector
with ERP, the accounts host asking twice, records headed by their id. The other journeys through the
pages: procure to pay, hire to retire, project to profit, document to signature, marketing to consent,
the stay, learning completion.

## 2026-11-06: the directory of people is the members'

**State:** found walking support to resolution in a browser: a portal person (a lessee, signed in with an
e-mailed code) read every person's name and address from `GET /api/users`, other customers, a citizen,
a partner and a supplier's contact among them, and Helpdesk offered portal people as a ticket's
assignee. No release was tagged.

| Where | What |
|---|---|
| This PR | The directory lists the workspace's active members alone (no portal people, guests, service or support accounts), and only a member reads it; anyone else is refused |

**Evidence:** `crates/kernel/tests/shell_it.rs`: a portal person is refused the directory, and the
administrator's directory holds the members, not the portal people; mutants `directory-read-by-anyone`
and `directory-lists-everyone`.

**Not done:** Drive's share box finds people by the directory, so it now calls a portal person's address
outside the workspace, which it is.

## 2026-11-05, night: addresses out of the history's payloads, and Edge's people

**State:** this PR follows PR 66. Its lock takes IoT Edge 0.4.4 and FRONTEND_REV takes Frontend PR 30. No release was tagged.

| Where | What |
|---|---|
| This PR: payloads | An erasure redacts the person's addresses wherever they sit in the workspace's audit payloads (apps that name who acted by address, Edge's and ERP's shop floor among them), each event keeping its hash and recording the redaction; replaying the journal after a restore does it again, from the addresses the restored account and records hold (FERRUM-PRIVACY-007) |
| This PR: rows | A record matched by its rows says which table and how many of its rows are the person's; the Personal data page says "their row of Recipients, removed" (Frontend PR 30) |
| IoT Edge 0.4.4 | Its customers' people and report recipients are found in the rows that hold them, and erasure removes those rows alone (FERRUM-PRIVACY-002) |

**Evidence:**
- `crates/kernel/tests/privacy_it.rs`: an app's event naming Pat by address (`by`) reads `[erased]` after her erasure, the rest of the payload kept; the same event as a backup had it reads `[erased]` again once the journal is replayed.
- `ferrum-app-edge:tests/portal_it.rs`: the boss's request finds their row of the weekly schedule alone; erasing it leaves the schedule going to ops.

**Not done:** free-text notes in payloads stay as written.

## 2026-11-05, evening: people in a child table's rows, and the direct writes reviewed

**State:** this PR follows PR 65. No release was tagged.

| Where | What |
|---|---|
| This PR: rows | A personal-data declaration may match people held in a child table's rows (`match: table.field`): a request copies those rows alone, erasure removes them (the table in `erase`) or blanks their fields (`table.field`) and leaves the record, its other rows and its files, and the journal names the rows by id so a restore loses them again (FERRUM-PRIVACY-002) |
| This PR: direct writes | The 96 statements in 13 apps that write entity tables outside the document pipeline, reviewed and listed by kind (`docs/registry/mutation-paths.md`): workspace isolation and legal holds hold on all of them in the database, actions are audited before their handlers write, and the sharing, approval and consent paths record what they do (FERRUM-PLATFORM-011) |

**Evidence:**
- `crates/kernel/tests/privacy_it.rs`: a host's visit lists Pat and Kim as companions; Pat's request copies her row alone (not the host's address); erasing her removes her row and leaves Kim's, the host's name and address; the journal holds the row's id and no address, and replayed after the row is put back, it removes it again. Declarations matching rows with a phone, erasing the record's own field, or naming a table the entity has not are refused.

**Not done:** Edge declares its customer people and report recipients once this is on main; the review's open points (public endpoints' validation, engine updates and versions) are listed in it.

## 2026-11-05, late: state field rules, backfills, a subscription's revenue over its days, and the people seven apps hold

**State:** this PR follows PR 64. Its lock takes ERP 0.14.1, the personal-data declarations of seven apps, and every app's interface rebuilt with Ferrum-Frontend 00dea07; FRONTEND_REV takes Frontend PRs 27 to 29. No release was tagged.

| Where | What |
|---|---|
| This PR: state field rules | A workflow state may name the only fields a person changes while a document is in it (`editable`) and the fields it needs (`required`): an edit of another field is refused with its reason, and a document is neither saved in the state nor moved into it without what it needs; the form shows the rest read-only and those fields required (FERRUM-PLATFORM-008) |
| This PR: backfills | Packages declare backfills (a table or entity, the rows that still need it, what it sets, the batch). The upkeep runs each per workspace as a job, a batch per transaction with the last row it reached, so a failure or restart carries on from there and visits no row twice; done, it counts the rows that still need it. `GET /api/admin/backfills`, `POST .../rerun`, and the Packages tab show and rerun them (FERRUM-PLATFORM-010) |
| This PR: numbers | A number that differs from the one asked for only by a country code lists its records as possibly the person's, beside namesakes, never erased for it (FERRUM-PRIVACY-002) |
| This PR: holds | The database refuses deleting a record under a legal hold by any path, an app's own SQL among them (Drive's permanent deletes, say): every entity table gets a guard, which a workspace's own deletion passes (FERRUM-PRIVACY-004) |
| This PR: exports | An export goes on without a file the storage no longer has, naming it in the manifest (`files_missing`) instead of failing; the server tells a missing object (not found) from storage being down. An erasure marks the exports it deleted, so none offers files that are gone |
| ERP 0.14.0 | Subscription invoice lines carry the days they bill, so a plan whose item defers its revenue bills (it could not) and is earned day by day; a refund's credit note gives back over the days it credits, recognised negative; a line takes its item's deferral when added with its period; MRR reads plans as they stood, with expansion, contraction and net revenue retention (FERRUM-ECOSYSTEM-009, FERRUM-ERP-006) |
| Healthcare 0.1.3, Government 0.1.5, Contracts 0.1.5, Control Plane 0.1.5, Logistics 0.1.3, Property 0.1.4, Restaurant 0.1.3 | Personal-data declarations: patients (and their appointments, consents and fee validities through them, kept as medical records), applicants, permit holders and citizens (drafts erased, the rest kept as public records), counterparties, hosted customers' contacts, drivers, repair reporters, and guests known by phone alone (FERRUM-PRIVACY-002) |
| Every app's interface | Rebuilt with Ferrum-Frontend 00dea07 (PR 28), a patch release each: Assets, Bookings, Campaigns, Cloud, Commerce, Comms, Construction, Contracts, Control Plane, CRM, DAM, Docs, Drive, Edge, Education, ERP 0.14.1, Field Service, Forms, Government, GRC, Healthcare, Helpdesk, Hospitality, HR, ITSM, LMS, Logistics, Metrics, Nonprofit, Objectives, POS, Projects, Property, Restaurant, Retail, Sign and Social: record forms show a workflow state's kept fields read-only and its needed ones required |
| Frontend PRs 27 to 29 | The erasure's confirmation and result name the stored exports it deletes; fields a workflow state keeps are read-only and what it needs is required; the backfills' report on the Packages tab; the workspace export made, followed and downloaded from the Workspace tab (it had no place in the interface), saying when it was deleted, expired or left files out |

**Evidence:**
- `crates/kernel/tests/collab_it.rs`: while a customer waits for approval only its address changes (its name is refused, saying it stays as it is while Pending); approving it without an address is refused, then done once it has one; an approved customer keeps its address. The refusals count for the metrics.
- `crates/kernel/tests/jobs_it.rs`: a file whose object was lost is named in the export's manifest and result and left out of its files, and the export finishes. `crates/kernel/tests/privacy_it.rs`: the export an erasure deleted says so.
- `crates/kernel/tests/backfill_it.rs`: 620 notes in batches of 250; the second batch fails the first time it reaches note 400, as a crash would; the job carries on from note 250 and every note is set once (`touched` is 1 for all); the report says done, 620 changed, none remaining; a note its set cannot change is counted after a rerun; the upkeep never reruns a finished one by itself; clerks are refused, and a rerun while running is a conflict. Bad declarations (a name in capitals, a table and an entity, an unknown entity, no rows, an empty batch, a name twice) are refused.
- `ferrum-app-erp:addons/finance/tests/finance_it.rs`: a year of support (3650) billed through a subscription from 1 April waits as deferred revenue; 910 is earned through 30 June; cancelled then, its credit note of 2740 gives back over the 274 unused days, so sales stay at 910 through 30 September and nothing waits. MRR from 1 February to 31 March: 410 at the start (Pro alone then), 62 of expansion, 372 churned, 100 at the end; revenue churn 90.73%, net revenue retention 24.39%. The whole ERP workspace's 49 test binaries pass.
- Each app's scenario: Healthcare finds Asha by address and number with her appointments, keeps them, erases the customer her bills went to; Government erases Ria's unsent draft and keeps what she put to the office; Restaurant finds Bo's delivery and booking by her number written two ways and blanks them.

- `crates/kernel/tests/privacy_it.rs`: a visitor under a hold is refused to a direct SQL delete as well as to the platform's.
- `crates/kernel/tests/privacy_it.rs`: asked with +1-555-0100, a visitor recorded as 555-0100 is listed as possibly Pat's beside the namesake, and is not erased with the request by number.

**Not done:** no package declares a backfill yet, and contracting a schema is by hand; no general archived state; plans are flat prices per interval (no usage-based or tiered plans); Edge's people sit in child rows a declaration cannot name; staff addresses on work records (owners, approvers, assignees) are not declared.

## 2026-11-05, night, later: traces, exports an erasure reaches, non-controlling interests, a group's cash flow, intercompany invoices, email clicks as touches

**State:** this PR follows PR 63. Its lock takes ERP 0.13.0, CRM 0.6.0 and Campaigns 0.3.0. No release was tagged.

| Where | What |
|---|---|
| This PR: traces | OpenTelemetry traces over OTLP/HTTP when the deployment names a collector (`OTEL_EXPORTER_OTLP_ENDPOINT`): each request is a span with its method and path, a caller's W3C `traceparent` its parent, flushed at shutdown; without an endpoint the log is Loco's as before (FERRUM-OPS-009) |
| This PR: erasure and exports | An erasure that reaches anything deletes the workspace's stored exports, copies from before, and an export it lands on while being made starts again, at no cost to its attempts; a declaration matches records that hold no address by their phone field (a restaurant's bookings), or through a link to a record another declaration finds (a patient's appointments), and the catalog refuses a declaration on a child table's rows, which a request could not read; records kept for a reason are no longer held to the thousand that guards erasure against a shared address, so years of an employee's attendance no longer refuse their request; the data-governance catalog lists every declaration at this lock (69, from 18 of the 36 apps compiled in) and what holds nothing to reach (FERRUM-PRIVACY-002) |
| ERP 0.12.0 | Non-controlling interests (IFRS 10): a company says how much of it its parent holds, and the consolidated balance sheet and profit and loss split equity and profit between the owners of the parent and the non-controlling interests, from each company's own net assets and profit. A group's cash flow (IAS 7): every flow at the rate of its day, the effect of exchange rates on cash, what was eliminated only in part a line of its own (FERRUM-ERP-005) |
| ERP 0.13.0 | A sale to one of the group's companies makes that company's purchase invoice: from the supplier that represents the seller, numbered and dated as the sale, at its net prices, once; the sale is not cancelled while it stands (FERRUM-ERP-005) |
| CRM 0.6.0, Campaigns 0.3.0 | With the Campaigns app's CRM link (`campaigns_crm`), an email campaign belongs to a CRM campaign, and a click on it from a won deal's lead's or contact's address, before the deal was won, is a touch in Campaign Attribution (FERRUM-CRM-002) |

**Evidence:**
- `server/src/telemetry.rs`: a request span reaches a stand-in collector as an OTLP/HTTP protobuf POST to `/v1/traces` carrying its name, path and service; a caller's `traceparent` is read. Beside a running server (ferrum_walk) a stand-in collector received the span of a request sent with a `traceparent`, in that trace.
- `crates/kernel/tests/privacy_it.rs`: an export made before Pat's erasure is deleted by it (`exports_deleted: 1`), its download refused and its object gone from storage; a table booked under Pat's number, holding no address, is erased by a request with that number written another way; a visit linking to Pat's visitor record is found and erased with it, journalled and erased again after a restore; 1002 receipts kept for accounting are exported and left, while 1001 visitors under one front-desk address are refused as no one person. `crates/kernel/tests/jobs_it.rs`: an erasure landing as an export's part is stored sends the export back to the queue with no attempt counted and nothing of it stored; it is then made. With the check taken out, the export finishes with what it read before the erasure, and the test fails.
- `ferrum-app-erp:addons/finance/tests/books_it.rs`: India holding 80% of Europe, non-controlling interests of 310 in equity and 270.20 of profit; Group Co holding 90% of India as well, 784 and 228.28; the group's cash flow 4,725 from operating, 275 the effect of exchange rates, from 0 to 5,000, reconciled.
- `ferrum-app-erp:addons/finance/tests/books_it.rs`: Group Co bills Group India for 250 units; India's purchase invoice is from the supplier that is Group Co, numbered and dated as the sale, at its net total; a second is refused; submitted, it keeps the sale from being cancelled; the books balance.
- `ferrum-app-campaigns:tests/campaigns_it.rs`: Chen clicked the spring email tied to CRM's Spring 2026; a deal won from a lead at that address is that campaign's under every model.

**Not done:** a purchase mirrored into the seller's books, and credit notes mirrored; goodwill (the investment is not set against the subsidiary's equity); database queries and jobs as spans of their own; personal-data declarations for the 18 apps that make none (Healthcare's patients, Government's citizens and Contracts' counterparties among them).

## 2026-11-05, night: the apps take up phone lookups, kept-back exports, attribution and product rules

**State:** this PR follows PR 62. Its lock takes eight apps at their follow-ups to PRs 60 and 61, CRM through 0.5.1. No release was tagged.

| Where | What |
|---|---|
| Bookings 0.4.2, Commerce 0.7.3, Field Service 0.2.3, Hospitality 0.1.3, Nonprofit 0.1.3, Education 0.1.3 | Their personal-data declarations name their phone fields (bookings' guests, carts, service requests and work orders, technicians, stays, members and volunteers) and their people's names (FERRUM-PRIVACY-002) |
| HR 0.7.3 | Employees and job applicants are found by phone and listed by name; employees' bank account numbers, IBANs, tax, national ID and passport numbers are left out of every list export (FERRUM-IDENTITY-012) |
| CRM 0.4.3, 0.5.0, 0.5.1 | Leads by phone and name; a Campaign Attribution report (linear, first or last touch over a won deal's lead, opportunity and submitted quotations); Product Rules (requires, excludes, so many per unit) that hold a quotation back until they are kept, and an action that adds what they need, priced like any line (FERRUM-CRM-002) |

**Evidence:**
- Each app's own scenario passed against the Ferrum of PRs 60 to 62 before it was pushed, and its CI passed on Ferrum main.
- `ferrum-app-crm:src/reports.rs`: linear, first and last touch over repeated touches, the parts adding up. `ferrum-app-crm:tests/crm_it.rs`: Expo's deal and a webinar's lead taken on by Expo give Expo 2150, 2000 and 2300 and the webinar 150, 300 and nothing; a sales user's report counts only his deals; a quotation of three widgets lists the installation it needs and the six kits short, is refused at submit, gets Mounting Kit × 6 and Installation × 1 added by the action, each priced, lists the self-fit guide that cannot go with installation, and goes out once kept.
- Walked on a server built with CRM 0.5.0 (ferrum_walk, CRM installed from Administration, Packages): a product rule "Rent comes with cleaning" made on its form; a quotation for Cyril Citizen with only Rent saved as a draft listing "Rent comes with cleaning (Rent needs Common area cleaning)", its submission refused with that reason at the lines, then with the cleaning line saved with nothing broken and submitted (QTN-2026-00001). The Campaign Attribution report's page took its parameters and ran (no won deals in that workspace).
- `scripts/check_apps.py` over this lock: all apps and add-ons pass.

**Not done:** product rules set no prices of their own and leave a choice between excluded items to a person; the Campaigns app's sends are not attribution touches; packages other than HR mark no field `no_export`.

## 2026-11-05, night: failed postings as metrics, and passkey names erased with their owner

**State:** this PR follows PR 61. No release was tagged.

| Where | What |
|---|---|
| This PR | `/metrics` counts the submits, cancels, actions and workflow steps that did not go through, per package, entity and operation (FERRUM-OPS-009): `refused` by their own rules (a posting with no account to post to, a stale version) or a `fault`, which is also logged with its cause. Who may not do something, or a record that is not there, is not the operation failing and is not counted. Workspaces name their own workflow steps, so label values are quoted as the exposition format says |
| This PR: role pairs | A grant already held is no new grant: an identity provider re-syncing someone who held both roles of a pair before it was declared is no longer refused (migration `20261105000004_role_conflicts_held.sql`); granting either afresh still is |
| This PR: history | The platform's audit payloads other than record diffs were read (FERRUM-PRIVACY-007): ids, flags, settings, hosts and reasons, except a passkey's name, written when its owner added it. Erasing a person now takes their passkeys' names out of that history, also of passkeys removed since, keeping each event's hash. A support grant keeps its grantee's name, an outside party's |

**Evidence:**
- `crates/kernel/tests/collab_it.rs`: an approval whose condition fails is counted once as refused for Customer's Approve; the creator's refused approval (who asked, not the step) is not counted, and nothing is a fault.
- `server/src/controllers/system.rs`: the metric is a counter, and a step name holding quotes and a line break stays inside its quotes.
- `crates/kernel/tests/rules_it.rs`: a role the stand-in already holds, synced again as SCIM writes it, is not refused although they hold both of a pair.
- `crates/kernel/tests/privacy_it.rs`: a passkey Pat added and has removed is named in its history; erasing Pat leaves its relying party and erases its name.

**Not done:** OpenTelemetry traces; counts per workspace (a label per workspace would grow without bound).

## 2026-11-05, night: a person found by phone number, or possibly by name

**State:** this PR follows PR 60; FRONTEND_REV is at Ferrum-Frontend PR 26's merge. No release was tagged.

| Where | What |
|---|---|
| Ferrum-Frontend PR 26 | The Personal data page asks for a phone number and a name; records sharing the name are listed apart, left out of the person's copy and never erased for it |
| This PR | A personal-data request may give a phone number and a name besides the email (FERRUM-PRIVACY-002). A declaration names its `phone` field, and the records whose number has the same digits, however either was written, are the person's like an email match: exported, and erased with them. A declaration naming its `name` field lists the records holding that name (case and spacing aside) apart, under `possibly`, for an administrator to judge; a name alone never erases anything. The platform's contacts, addresses, customers and suppliers declare their phone fields, and contacts and individual customers their names; a declaration's phone and name fields are checked when the catalog loads |

**Evidence:**
- `crates/kernel/tests/privacy_it.rs`: a walk-in recorded with only "+1 (555) 0100" is found when asked with "+1-555-0100", beside the three records the email reaches; a namesake "pat   LEE" with another address is listed as possibly theirs, the records already theirs not repeated; too few digits are refused; an erasure asked with the number reaches the walk-in and leaves the namesake; declarations naming a phone field that is not one, or a name field that does not exist, do not load.
- `crates/kernel/tests/kernel_it.rs` and `collab_it.rs` boot the platform's packages with their new declarations.
- Walked on this branch's server (ferrum_walk) with Ferrum-Frontend PR 26: asked for citizen@walk.test with "+91-9876543210" and "Cyril Citizen", the Personal data page listed the customer the email reaches, a walk-in customer recorded as "+91 98765 43210" with no address, and the login account, and listed a namesake with another address apart as possibly theirs.

**Not done:** apps' declarations name no phone or name field yet; a number asked for without the country code it was recorded with is not found.

## 2026-11-05, evening: roles one person may not hold together, and fields kept out of exports

**State:** this PR follows PR 59; FRONTEND_REV is at Ferrum-Frontend PR 25's merge. No release was tagged.

| Where | What |
|---|---|
| This PR | Segregation of duties (FERRUM-IDENTITY-014): an administrator declares pairs of roles one person may not hold together, with why (`/api/admin/role-conflicts`). A trigger on `user_role` refuses any grant that would join a pair, so the users API, SCIM groups and apps all meet it, and the refusal names the pair and the reason; one person's grants are checked one at a time. Whoever held both before the pair was declared keeps both, named beside the pair and in the evidence for a review. What an automation makes counts as its saver's, so an approval step that bars a document's creator bars them too |
| This PR: exports | A field marked `no_export` is left out of every list export (CSV, spreadsheet), whoever exports it and even when asked for by name (FERRUM-IDENTITY-012): read on the record, not taken away in bulk. No package marks one yet; HR's bank and identity numbers are the first candidates, once this is on main |
| Ferrum-Frontend PR 25 | The Roles tab lists the pairs with whoever holds both, declares them and allows a pair together again; a refused grant says why where it was made |

**Evidence:**
- `crates/kernel/tests/rules_it.rs`: a pair refused with itself, with a role that does not exist, without a reason and by a non-administrator; declared, kept in order, with the stand-in who held both named, also in the evidence pack; giving both again refused with the reason and changing nothing; refused for someone new; refused for a grant written straight into the table (the path SCIM and apps take); allowed together again, both given.
- `server/src/controllers/docs.rs`: with Customer's tax ID marked `no_export`, an export's columns leave it out, also when asked for by name.
- `crates/kernel/tests/collab_it.rs`: an automation saved by the administrator makes a customer; the administrator cannot approve it (its creator, through their automation), and a second person does.
- Walked on this branch's server (ferrum_walk): Social Author and Social Approver declared with a reason and Dana, who held both, named; saving her roles refused with that reason; without Social Approver accepted, and the pair held by no one; allowed together again. Dana's roles were put back afterwards.

**Not done:** rules over three roles at once, or across documents (whoever raised an order paying it); a pair that limits administrators; HR marking its bank and identity numbers `no_export` (it needs this on main first).

## 2026-11-05, later: records keys, roles given until a moment, automations that run as their saver, a group's books under IAS 21, the app contract, and every app rebuilt

**State:** this PR follows PR 58. Its lock takes ERP's main through PR 13 and the 35 apps it compiles in at their rebuild against Ferrum-Frontend PR 22 (patch releases on their main branches; Cloud, deployed apart, was rebuilt too), with the five apps the contract check found short at their fixes; FRONTEND_REV is at Ferrum-Frontend PR 24's merge. No release was tagged.

| Where | What |
|---|---|
| This PR | Records keys (FERRUM-IDENTITY-008): an API key whose scope is `records:Customer=read;SalesInvoice=read+create` reaches only `/api/v1/…` and `/api/meta/…`, with its owner's rights narrowed to the entities and actions it names by the same narrowing as app screens (`PermSet::limited_to`): never more than the person who made it, never an administrator's, no field group. An unknown entity or action is refused when the key is made; the database's check admits well-formed records scopes only. Everything else that takes a token (browser sessions, sockets, two-factor setup) refuses such a key |
| This PR: role ends | A role may be given until a moment (FERRUM-IDENTITY-009): access ends then, as the person's context leaves the role out and a cached context lapses at that moment rather than at its time to live. The hourly upkeep removes the ended grant, so automations' and Ops' notices, the site console's operators, the access evidence and apps' own lookups of who holds a role stop finding the person. An end already past, or on a role not given, is refused; a SCIM group giving a role held by hand until a moment makes it the group's. A department or a branch limits someone as a scope on that entity, which the rules scenario now shows on an entity other than Company |
| Ferrum-Frontend PR 24 | The roles dialog takes the day each role is held through (empty: for good), and the users list shows it beside the role. The Roles tab asks why someone may or may not do something (FERRUM-IDENTITY-013, an API until now): a person, an action, an entity and optionally one record, answered as the server enforces it, or as if they held other roles |
| This PR: automations | An automation runs as the administrator who saved it, with their rights as they are when it runs, not as the system (FERRUM-IDENTITY-015). Once its saver is no longer an active administrator it does not run: the run is recorded as failed, saying why, and is not tried again for that event; an administrator who saves it again becomes its saver. One saved by no one (an app's) runs as the system, as before |
| Ferrum-Frontend PR 23 | The API keys page takes an optional "Only these records" and shows what each key reaches; records keys no longer vanish from it (it listed only keys without a scope) |
| ferrum-app-erp PR 12 | ERP 0.11.0: a group's consolidated trial balance, profit and loss and balance sheet translate a company in another currency as IAS 21 says: assets and liabilities at the closing rate, income, expenses and equity at the rate of their own day where the books have one, else the closing rate; what that leaves is the foreign currency translation reserve, an equity line of its own, worked out from the business that stays in the group (FERRUM-ERP-005, still partial). Its manifest names its README, which its add-ons share |
| This PR: the app contract | docs/app-contract.md says what every app gives (FERRUM-UX-004); `scripts/check_apps.py`, in CI's audit job, checks what a program can see in every app the lock compiles in: manifest, description, history and the README's row for the version, a scenario test, a built interface, a launcher icon the shell draws. It found 16 short: ERP's manifest named no description (11 with its add-ons), Assets' and Edge's no homepage or support, and CRM's, Helpdesk's and Projects' READMEs had stopped adding changelog rows; each fixed on its main (ERP in PR 12, where four of its add-ons still named a README they did not have, added in ERP PR 13) |
| 36 apps | Rebuilt against Ferrum-Frontend PR 22's merge, so their record pages toast what an action said it did; each's own CI passed on main, except Cloud's, which GitHub did not start |

**Evidence:**
- `crates/kernel/tests/twofactor_it.rs`: records keys for an unknown entity, an unknown action or none refused; a key for Currency (read) and ExchangeRate (write and read) kept in order; narrowed, the administrator who made it reads currencies and writes exchange rates through it, writes no currency, reads no company, and is no administrator.
- `crates/kernel/src/auth.rs` checks how a records scope is read (repeats joined, malformed ones refused); `server/src/extract.rs` checks the paths such a key may call.
- `crates/kernel/tests/rules_it.rs`: a role given until 1.5 seconds on reads a claim, then, its context cached a moment before, reads nothing (it fails with the cache's 30 seconds alone); an end already past, an end on a role not given and a non-administrator are refused; the upkeep removes the ended grant and leaves one that has not ended. Scoped to one agent, a person lists and opens that agent's claims and those naming none, never another agent's.
- Walked on this branch's server (ferrum_walk): Dana's Metric Reader held through 30 November, saved, listed and stored as that day's 23:59:59 in the browser's zone; the Roles tab's explanation for Dana reading customers (not allowed, with her 16 roles), as if she held Sales User (allowed, a what-if) and for one customer (a rule reaches it); a changed question clears the answer.
- Ferrum-Frontend's `roleEnds.test.ts`: the days chosen are sent as each day's close for the roles kept, and a day left as it was keeps the role's own moment.
- `crates/kernel/tests/invariants_it.rs`: an automation saved by a second administrator writes their address into the record it acts on; deactivated, it does not run, and its run says an administrator saves it again; saved again by the first, it runs as them. What no automation may do (oversell, skip a posting, rewrite what was submitted) still holds.
- `addons/finance/tests/books_it.rs` (ERP): the group-books scenario under the euro rates its tenant holds (90, 92, 89, 100): Europe's July sale and August grant at May's 89, its cash at the closing 100, a reserve of 275 and the intercompany difference still 200; India's group with Europe's April purchase at 92 and a reserve of 199; with a rate of 90 from July, 1,800, 450 and a reserve of 250; the statements balance with it and the profit leaves it out.
- `scripts/check_apps.py` over this lock: 49 apps and add-ons, none short.
- Ferrum-Frontend's `recordsKeys.test.ts`: what is sent for what someone typed, and what a key reaches in words.

**Not done:** ownership percentages, minority interest and a consolidated cash flow (FERRUM-ERP-005); records keys for reports and apps' own endpoints; per-field rights for a key (it reads no field group); the private repositories' CI (ferrum-app-cloud's rebuild, as the store's), which GitHub does not start for the account's billing.

**Lessons:**
- Two cargo commands on one machine can deadlock on cargo's own locks (a `cargo update` fetching, a `cargo test` waiting on the package cache) and sit at no CPU; a fetch from GitHub can stall with its connection open. Run one cargo at a time, and give git a low-speed limit (`CARGO_NET_GIT_FETCH_WITH_CLI`, `GIT_HTTP_LOW_SPEED_LIMIT`).
- Builds in several worktrees on one target directory fill the disk quickly: prune superseded artifacts between them.

## 2026-11-05: a supplier's orders on the portal, deferred revenue and accruals, outside calendars, a payroll bank file, and the IRP's own checks

**State:** this PR follows PR 57. Its lock takes ERP's main through PR 11, Bookings' through PR 2 and HR's through PR 2; FRONTEND_REV is at Ferrum-Frontend PR 22's merge. No release was tagged.

| Where | What |
|---|---|
| ferrum-app-erp PR 6 | ERP India 0.2.2 checks every kind of e-invoice it builds against the IRP's value rules as its developer portal states them, with the portal's three tolerance examples as a fixture prepared apart from the code (FERRUM-VERIFY-002). It found supplies to another state or an SEZ unit sent with CGST and SGST, which the IRP refuses, and IGST within the state not marked; both fixed |
| ferrum-app-erp PR 7 | ERP 0.8.0: Your orders on the workspace's portal, for the people a supplier's address belongs to: the purchase orders sent to them with what was received and billed, each printed or as PDF, and the bills ERP holds from them with what is still owed. Procurement 0.1.2 adds the requests for quotation waiting for their answer, with the link they were e-mailed (FERRUM-ERP-011, FERRUM-ECOSYSTEM-007) |
| ferrum-app-erp PR 8 | ERP 0.9.0: an invoice line over a service period books to a deferred revenue or expense account and is recognised by the day into its own account, each month and through a day an accountant chooses, under the invoice, so that cancelling the invoice reverses what was recognised. Two runs at once (two replicas, or an accountant's and the monthly one) recognise each day once. An order billed in full before it was received reads "Billed, to deliver" on Your orders |
| ferrum-app-erp PR 9 | ERP 0.10.0: a journal entry marked Reverse It (an accrual or a prepayment) is taken out again on its reversal day, once, by the upkeep or as it is submitted when the day has come; the Deferred Revenue and Expense report shows each deferred line with what was recognised and what still waits; Accrued Income and Accrued Expenses in the standard chart (FERRUM-ERP-006) |
| ferrum-app-erp PRs 10 and 11 | ERP 0.10.1: Create Standard Chart of Accounts, on a company that has accounts, adds the standard accounts it lacks under their standard parents and links its empty defaults, so companies made before 0.9.0 can defer. ERP 0.10.2 and Finance 0.3.1: an empty reversal day is the day after the entry's, worked out when it is submitted rather than while it is a draft. Both found walking in a browser |
| ferrum-app-hr PR 2 | HR 0.7.0: a payroll run's Bank Payment File, ISO 20022 pain.001.001.09 with the salary category or a CSV, one payment to each employee paid by bank, from Finance's Bank Account or an account given; refused, naming them, while anyone paid by bank has no account, and to anyone who may not read bank details (FERRUM-HR-002, still partial) |
| Ferrum-Frontend PR 22 | A record page toasts what an action said it did rather than "<action>: done"; apps show it once rebuilt against it, as ERP is in its PR 11 |
| ferrum-app-bookings PR 2 | Bookings 0.4.0: a staff member's own calendar elsewhere, read every quarter of an hour by its iCalendar address (a secret), keeps guests from the times it says they are busy, with the service's buffers. Only the times are kept. Recurrence, exceptions and moved occurrences through the icalendar crate (rrule underneath); all-day and floating times in the staff member's zone; Outlook's Windows zone names mapped (FERRUM-BOOKINGS-004) |
| This PR | A compiled-in app's interface is this build's, as its code and manifest are, whatever version was recorded when it was installed. A missing default account is refused in words ("the company's Default Accounts") rather than by its field key, pointing at Create Standard Chart of Accounts. A compiled-in app's update plan lists only the entities the workspace does not have yet (ERP's listed all 34 of its own) |

**Evidence:**
- ERP's scenario test reads a supplier's orders, an order's print and its bills through the portal's API as the supplier's contact: a draft order is not shown, and a customer's contact finds none of them. Procurement's test lists a request for quotation for its supplier's people only, and no longer once it is answered.
- A browser walk (evidence/browser-2026-11-04-flows.md): Rina, Cleanline's contact, signed in with an e-mailed code and read her order, its print and her bill. It found Your orders drawing a blank page: the workspace had ERP recorded at 0.7.1, so the shell served that release's interface while the server ran 0.8.0. `server/tests/apps_api.rs` now records an older version for a compiled-in app and checks the shell is told this build's.
- ERP's scenario test defers a year of support sold for 12,000 and a year of insurance bought for 3,650, recognises them through 31 March and 30 June, runs two recognitions at once and finds the line moved once (without the invoice's lock it moved twice), finds nothing to move when run again, refuses a day that has not come and a stock item marked deferred, reads the report's rows and totals against the deferred accounts, and cancels both invoices to see the deferrals and their recognitions reversed; `deferred.rs` checks what each day earns, with what rounding left on the last day.
- The same test accrues March's electricity on 31 March and finds it reversed on 1 April as it is submitted, refuses a reversal day before the entry and a customer line, waits on one whose day is far off and sees two upkeep runs reverse it once, and cancels it afterwards with nothing more posted.
- A browser walk of deferred revenue, an accrual and an outside calendar (evidence/browser-2026-11-04-flows.md): an invoice deferred over 2026, recognised through 31 October and read on the Deferred Revenue and Expense report; JV-2026-00001 accrued on 31 October and reversed on 1 November; Bookings installed, a staff member's Outlook-style outside calendar read by the upkeep, and the guest's booking page leaving out its busy hour. It found four faults: the three the ERP and Frontend PRs above fix, and ERP's update in the App Manager listing all its entities as new, which this PR fixes (`server/tests/apps_api.rs` checks an update of a compiled-in app lists none).
- HR's payroll scenario makes the bank file for its September run: refused while two employees have no account and to an HR user, then two payments of 173,247.14 in all, as pain.001 and CSV; `bank_file.rs` checks the IFSC-routed and SEPA forms and who is paid by bank.
- Bookings' scenario test serves an outside calendar from a stand-in on this machine: Asha's dentist hour (and the massage's 15 minutes after it) and a conference day leave her slots while a free reminder does not; it is not read again inside 15 minutes; a 500 is recorded and the earlier busy times kept; the dentist cancelled and read again; the address removed and the conference gone. `outside.rs` checks series in a Windows zone with an exception and a moved occurrence, all-day and floating times in London's summer time, and free, cancelled and Outlook-free events.

**Not done:** the other apps rebuilt against Ferrum-Frontend PR 22, so their record pages show actions' messages (ERP's and the shell's do); HR's bank file walked in a browser (its scenario test makes it); the bank file checked against a bank's validator, and payroll's statutory returns; a supplier acknowledging an order or proposing dates on the portal; an existing booking flagged when an outside calendar later shows the staff member busy then; outside calendars against Google's, Microsoft's and Apple's own servers (BLOCK-CONNECTOR-SANDBOXES); an accountant's review of the deferral and accruals (FERRUM-VERIFY-002); and the other partial requirements, whose limits say what each lacks.

**Lessons:**
- An installation records the version it installed. For a compiled-in app that is only history: its code and manifest are the build's, and anything chosen by the recorded version, such as the interface the shell loads, falls behind at the next upgrade of the build.
- `fetch_from` on a child row overwrites what the user typed: a default taken from the item belongs in the pricing hook, which fills only what is empty.
- Work that runs on every replica (ERP's upkeep says it is safe to) must lock what it reads before it decides: two recognitions at once each read a line as not yet recognised. Locking the invoice and taking invoices in id order made the second find it done; the test runs two at once with `tokio::join!`.
- The registry's check looks for other repositories' files in sibling checkouts, which here are other sessions' older branches: run it with the worktrees of the branches being pinned (a wrapper that sets `registry.SIBLINGS`), or its failures are about those checkouts, not the registry.
- An outside calendar's address is a secret that opens someone's calendar: errors about it are written without it (the kernel's vetted client puts the URL in its messages).
- The record form previews a draft through its hooks as it is edited: a default a hook fills from another field (a reversal day from the posting date) sticks when that field changes. Work such defaults out when the document is submitted.
- A release that adds standard accounts must reach the companies made before it: Create Standard Chart of Accounts now completes a chart, and the refusal for a missing default points there.
- Clicking a checkbox's label, not setting it with a form fill, is what the page notices: a form fill left Reverse It ticked on screen and unset in the page's state.

## 2026-11-04: the control plane, social media, and paid apps in the store

**State:** this PR follows PR 56. It compiles the control plane and Social in, and gives the shell
the icons they name (Ferrum-Frontend PR 16, merged, does the same for the launcher). The store's paid
apps are on the store's main branch. No release was tagged.

| Where | What |
|---|---|
| New: ferrum-app-control | Control Plane 0.1.0, run in a workspace of its own. Cells report how they are; workspaces are placed by room and their health read with the customer's health-only support key; releases by digest, qualified by a second Release Manager, rolled out a wave at a time in maintenance windows, soaking and halting; incidents told in updates; a public status naming no customer; recovery requests decided by a second operator; partners' portfolios, setup templates, cases and statements (FERRUM-CLOUD-001, -004, FERRUM-PARTNER-001, FERRUM-IND-021, each partial) |
| New: ferrum-app-social | Social 0.1.0: brands' accounts each reached through a platform connection; posts that fit each network, approved by someone other than their author and again after a change, sent at their time one publication per account through that account's own connection by the platform's delivery; what posts did read back; the inbox routed by handle and answered through the right connection. 0.2.0 brings a connector for Mastodon (FERRUM-ECOSYSTEM-005, partial: not run against a real server) |
| ferrum-store | Paid apps: plans by verified publishers who say who supports the app, purchases with the buyer's country's tax, licences the store signs, renewals with a week of grace, refunds within 14 days, disputes decided by an administrator, payouts net of tax and a 15% commission (FERRUM-EXT-009, partial: no real payment provider) |
| This PR | Control and Social compiled in; the shell draws server, share-2 and plug; FRONTEND_REV at Ferrum-Frontend PR 17's merge; the Integration Hub's history, `GET /api/admin/connections/{id}/activity`, which Ferrum-Frontend PR 17 shows on each connection (FERRUM-ECOSYSTEM-004, partial) |
| This PR: licences | The App Manager installs or upgrades an app a store sells only with a live licence for the workspace: `PUT /api/admin/apps/{id}/licence` keeps one once the store's signature, the app and one of the workspace's own hosts check out; each installation asks the store again, whether the app comes from the store or from the cluster where another workspace installed it; a store that cannot say whether it sells the app refuses the installation rather than letting it through; every licence is read again with the revocation list (FERRUM-EXT-009) |
| ferrum-store 049459d | Stripe as the payment provider: a card saved on Stripe's page and told by its signed webhook, off-session charges keyed for Stripe's idempotency and retried with the key, a payment not taken at once cancelled, a refused renewal tried again with a new key, refunds (FERRUM-EXT-009; tested against a stand-in of Stripe's API) |
| ferrum-app-hr 0.6.0 | Income tax slabs take a rebate and cesses and surcharges; India's new regime for FY 2025-26, as the Income Tax Department's page prints it, is a fixture the engine is checked against (FERRUM-VERIFY-002, now partial) |
| The walk's fixes | Ferrum-Frontend PRs 18 to 20 (labels name their fields, linked titles show their titles, Connections refresh while work waits, "1 record", the native line in words); Control 0.1.1, Government 0.1.1, Property 0.1.1, Social 0.2.1 and 0.2.2, Contracts 0.1.1, Objectives 0.1.1 |
| Every app rebuilt | The 36 apps with interfaces rebuilt against Ferrum-Frontend 80ff179 as patch releases, since each bundles its own record pages; this PR's lock takes them. FRONTEND_REV at PR 20's merge (16b5c57) |

**Evidence:**
- Control's scenario test runs the control plane in one workspace and a customer's in another, reached over HTTP with the key its administrator granted (`grant_support`), answered as the server answers one, each request on the customer's log. A rollout goes through two windows, halts when a cell falls ill while soaking and when one misses its window, and another finishes.
- Mutants by hand, all caught after fixes: Control Plane 96 (the first run caught 94 of 97: lowercasing an e-mail the kernel already lowercases was removed, and two mutants that did not compile were rewritten), the store's paid apps 35 (three gaps the first run found are tested now), Social 40 (three gaps tested, two mutants rewritten).
- The Integration Hub's history is tested in `crates/kernel/tests/connect_it.rs`; it has no mutants of its own.
- A browser on a server built from this branch (evidence/browser-2026-11-04-apps.md): every new app's tile and home page, the Control Plane's cell form, a cell's report, the public status and a workspace's placement. It found Governance and Risk's and Property's home pages filtering with `neq`, which the platform does not take (both now use `!=`, and the lock here takes the fixes), and three home pages with nothing to say when empty (now they say so).
- The store's `paid_apps` test walks plans, purchases, licences, renewals, trials, a refused renewal through its grace, refunds, a dispute, and payouts, with a refund after a payout coming off the next.
- `server/tests/apps_api.rs` installs a sold app: refused without a licence; licences for another workspace, signed by a publisher's key instead of the store's, lapsed, or for another app refused; installed with its own; a lapse read on the next round; another workspace refused though the app is in the cluster; a store that cannot list its plans refusing the installation.
- A browser walk of the apps' flows and portal pages (evidence/browser-2026-11-04-flows.md): six people signed in through the pages, four of them portal people with e-mailed codes; the Control Plane's release, rollout (through its soak to done), incident and public status, the partner's and customer's pages, Social on a Mastodon stand-in, a citizen's permit, a risk acceptance, an objective's check-in, an IT incident, ERP's company and an invoice, Property installed with a lessee's Your home, a certified metric, a contract approved and Digital Assets. It found nine faults, fixed and walked again, among them a public reply to a Mastodon direct message and a lessee's link to a draft invoice.
- The store's `stripe_payments` test runs the Stripe provider against a stand-in, a lost answer included; 31 mutants, all caught. HR's tax change: 9 mutants, all caught.
- Social's Mastodon connector runs in `tests/mastodon_it.rs` against a stand-in speaking the parts of Mastodon's API it uses, over HTTP: a wrong token refused, a post sent once though its first answer was lost (the same `Idempotency-Key` twice), engagements read back without boosts, nothing sent again once out after the server forgot its key, a comment, a mention and a direct message routed, a reply answering the comment. 27 mutants of the connector, all caught.

**Not done:** Stripe and Mastodon against the real services (the operator's account and keys, BLOCK-PAYMENT-KEYS, and an account's token); connectors for the other networks; an accountant's review and payroll cases from someone who knows each jurisdiction (FERRUM-VERIFY-002); Stripe in the walk (it paid through the store's test provider); the store's CI, which GitHub does not start for the account's billing; and a screen reader.

**Lessons:**
- Each app bundles its own record pages and the kit from `../../ferrum/web`, another session's checkout on an older branch: a Frontend fix reaches the apps only when they are built against a Frontend that has it (the walk built them in a scratch layout linking a checkout of main), and every app needs its rebuild.
- Read toasts at once: a refusal shown as a toast fades in nine seconds, and reading the page's main area misses it.
- An action's parameter named like a field of the record is filled with that field's value.
- A page's own queries are not run by an app's scenario test: walk each home page in a browser, or scan the interfaces for filter operators the platform does not take (it takes `=`, `!=`, `<`, `<=`, `>`, `>=`, `in`, `not in`, `like`, `between`, `is` and their word forms `eq`, `ne`, `lt`, `lte`, `gt`, `gte`).
- Rows written straight to the database in a test go through row-level security too: an update outside a tenant's transaction matches nothing and says nothing. Assert the rows affected.
- An app can send one record through one chosen connection by queueing the platform's own delivery (`jobs::enqueue` with `connect::DELIVER_KIND`) for a stream mapped without `push`: a pushed mapping sends every change of the entity through every connection that maps it.
- Two checks can make a third redundant: an incident's latest update must say its status, and updates stay as posted, so a change of status without a new update can only come from reordering them, which is the one case the third check is for.

## 2026-11-04: objectives, digital assets and governed metrics

**State:** this PR follows PR 55 and compiles three more apps in. No release was tagged.

| Where | What |
|---|---|
| New: ferrum-app-objectives | Objectives 0.1.0: objectives aligned to their parents within their periods, key results measured by check-ins, progress rolled up the tree, and the projects and portfolios behind them from Projects (FERRUM-ECOSYSTEM-011, partial) |
| New: ferrum-app-dam | Digital Assets 0.1.1: assets kept as Drive files with their rights, approved by version, back to review on a new version, expiring, and used only within their rights (FERRUM-ECOSYSTEM-012, partial). 0.1.1 renamed its roles Media Contributor and Media User: fixed assets already had an Asset User |
| New: ferrum-app-metrics | Metrics 0.1.0: a metric as a Studio query of one measure, certified by a steward who does not own it, back in draft when its definition changes, read by everyone with their own rights, kept as snapshots, deprecated pointing at a replacement, and a glossary. Its interface is at `/kpis/`, since the core keeps `/metrics` (FERRUM-ECOSYSTEM-010, partial) |
| This PR | The three compiled in; `ferrum-studio` patched to the workspace's crate, since Metrics is the first app to use it from git |

**Evidence:**
- Each app's scenario test passed against Postgres. Metrics' reads one definition as three numbers for two sales reps and their director, for a count and an average.
- Mutants by hand, all caught after fixes: Objectives 20, Digital Assets 19, Metrics 29. Metrics' first run found that a pivoted definition and a decimal value were never tested.
- The server checks with the three apps; CI's `rust` job runs `compiled_apps_fit_together`, which disk here does not allow.

**Not done:** the apps in a browser; ECOSYSTEM-004, -005, EXT-009, CLOUD-001, -004, PARTNER-001, IND-021.

**Lessons:**
- Studio's result columns are named by `name`, not `key`; reading the wrong one read every metric as 0, which only a test against the database showed.
- A route can be kept by the core (`/metrics` is Prometheus'): the manifest refuses it, so check the reserved routes before naming an interface.

## 2026-11-04, night: public services, retail, IT services, governance and risk, and contracts

**State:** Ferrum PR 54 merged (eight industry apps compiled in) and Ferrum-Frontend PR 15 (the launcher's icons). This PR compiles five more apps in and moves FRONTEND_REV to PR 15's merge. No release was tagged.

| Where | What |
|---|---|
| New: ferrum-app-government | Government 0.1.0: permits applied for on the portal with their documents uploaded, fees invoiced through ERP and paid before approval, rejections saying why, permits issued, expiring or revoked; cases with deadlines (FERRUM-INDUSTRY-010, partial: no legal review) |
| New: ferrum-app-retail | Retail 0.1.0: click and collect against a pickup code with the delivery note submitted at hand-over; franchise royalties invoiced through ERP. Replenishment is ERP Procurement's auto-reorder, not reimplemented (FERRUM-INDUSTRY-003, partial) |
| New: ferrum-app-itsm | IT Services 0.1.0: configuration items whose dependencies never loop, a catalog with approvals by someone else, incidents by the priority matrix with breaches, problems, and changes with freezes, conflicts and impact (FERRUM-ECOSYSTEM-002; -003 partial) |
| New: ferrum-app-grc | Governance and Risk 0.1.0: a risk register on a five-by-five matrix, acceptances by an approver who does not own the risk, controls tested with evidence, policies by revision, findings (FERRUM-ECOSYSTEM-014), adapted from CISO Assistant (AGPL-3.0) |
| New: ferrum-app-contracts | Contracts 0.1.0: drafts from approved clauses, approval above a threshold, signature through Sign, renewal with notice, obligations (FERRUM-ECOSYSTEM-008) |
| Ferrum PR 55 | The five compiled in; FRONTEND_REV at Ferrum-Frontend PR 15's merge |

**Evidence:**
- Each app's scenario test passed against Postgres with Ferrum 808cd94; Contracts' with Sign installed, sending through Sign's real action.
- Mutants by hand, all caught after fixes: Government 24, Retail 22, IT Services 36 (all on the first run), Governance and Risk 27, Contracts 29. The first runs found eight checks the scenarios never reached; each is tested now.

**Not done:** the apps in a browser; ECOSYSTEM-004, -005, -010, -011, -012, EXT-009, CLOUD-001, -004, PARTNER-001, IND-021.

**Lessons:**
- Look in ERP's add-ons before building: Procurement already replenishes stores, which Retail would have duplicated (and the name clashed with WMS').
- Before publishing an app, scan every app's entity and role names for clashes: the server's `compiled_apps_fit_together` test refuses a shared entity name, and a shared role name silently merges rights.

## 2026-11-04, evening: sign-up and plans, an industrial solution through to commissioning, meetings' captions and recordings, a hotel, and a browser on a running server

**State:** each PR was green on its CI before it merged. No release was tagged.

| Where | What |
|---|---|
| Ferrum PR 53 | Workspace sign-up at `accounts.{base}/signup`, the operator's plans (`FERRUM_PLANS`, `set_plan`), the people quota held by a trigger, the assistant held to the plan's tokens, and closing one's own workspace (FERRUM-CLOUD-002, JOURNEY-012 in parts) |
| Ferrum PR 51 | An industrial solution in an app's bundle, imported into Operations on install as device profiles and acceptance procedures; commissioning waits for a passed run of each; the install plan lists the equipment (FERRUM-IND-018) |
| Comms PR 5 | Comms 0.9.0: captions each person gives their own voice, kept as the meeting's transcript; recordings through LiveKit Egress (FERRUM-COMMS-002) |
| Ferrum-Frontend PRs 13, 14 | The workspace's plan, usage and closing in Administration; the equipment in the install dialog |
| New: ferrum-app-hospitality | Hospitality 0.1.0: properties, room types and rooms, rate plans by season, stays, folios and the night audit (FERRUM-INDUSTRY-001, JOURNEY-010 in parts), adapted from ERPNext 12's Hotels module |
| New: ferrum-app-nonprofit | Nonprofit 0.1.0: memberships invoiced through ERP and counted once paid, donations posted to the books, funds, grants and volunteers (FERRUM-INDUSTRY-011), adapted from ERPNext 13's Non Profit module |
| New: ferrum-app-education | Education 0.1.0: admissions into students and enrollments, fees through ERP, a timetable without clashes, assessments and attendance (FERRUM-INDUSTRY-004), adapted from ERPNext 13's Education module |
| New: ferrum-app-restaurant | Restaurant 0.1.0: orders priced from the menu, kitchen tickets by station, sent lines voided not edited, bills through ERP, takeaway, delivery and reservations (FERRUM-INDUSTRY-002), adapted from ERPNext 12's Restaurant module |
| New: ferrum-app-property | Property 0.1.0: units held by one lease's days at a time, rent and charges invoiced each period once through ERP, leases ended at their end or early, urgent requests holding a unit, inspections, and lessees' "Your home" on the portal (FERRUM-INDUSTRY-008, partial: no owners' portal) |
| New: ferrum-app-construction | Construction 0.1.0: estimates with a markup into contracts, variations decided by a Contracts Manager, progress claims recomputed when certified and invoiced through ERP less retention, retention released on completion, site diaries (FERRUM-INDUSTRY-007) |
| New: ferrum-app-logistics | Logistics 0.1.0: trips from delivery notes with stops ordered nearest first, dispatch checking vehicle, load, licence and that neither is out, proof of delivery by the trip's driver, and drivers' own stops (FERRUM-INDUSTRY-009, partial: road routing waits on a maps key) |
| New: ferrum-app-healthcare | Healthcare 0.1.0: appointments within hours and without clashes, consent by form version, visits completed by their practitioner and billed with follow-ups covered (FERRUM-INDUSTRY-005, partial: administration only, no legal review), adapted from ERPNext 12's Healthcare module |
| Ferrum-Frontend PR 15 | The launcher draws the new apps' icons (bed, hand and heart, hard hat, utensils, truck, stethoscope, cloud) |
| Ferrum PR 54 | Comms 0.9.0, Hospitality, Nonprofit, Education, Restaurant, Property, Construction, Logistics and Healthcare compiled in; the shell bar draws their icons; the operations guide's recordings; entity files may start with a comment |

**Evidence:**
- Mutants were run and all caught: 11 for sign-up and plans, 8 for acceptance (one added after the browser run), 9 for Comms by hand, 10 for Hospitality by hand, 9 for Nonprofit, 11 for Education and 11 for Restaurant by hand. Restaurant's two survivors were a test that never changed a price after ordering and a mutant undone by operator precedence; both are caught now. Logistics' 28 and Healthcare's 29 by hand: three survivors showed checks the scenarios never reached (a depot without its place, an inactive practitioner, a visit before hours) and are tested now. Construction's 34 by hand: one check the kernel already made was removed, and an uneven markup was added to the scenario. Property's 27 by hand: two survived at first (a lease starting the day after another ended early, and a period invoiced twice once its next date was wound back by hand); both are tested now. Hospitality's one survivor was a check nothing could reach; it was removed and the guard that matters was tested instead.
- Kernel, Operations, Comms and Hospitality tests ran on this machine. The server was checked with clippy with and without Operations.
- **A browser on a running server, for the first time** (`docs/platform/evidence/browser-2026-11-04.md`): from sign-up to closing, and an industrial solution from upload to commissioning. It found a real bug that no test had: an installation that brings Operations imported with the request's stale schema. PR 51 fixes it, with a test and a mutant.

**Not done:**
- Real LiveKit Egress, real speech recognition, or a real call.
- Equipment, and a channel manager for the hotel.
- Personal-data erasure of meeting transcripts.
- The other industry packs, the ecosystem apps, store monetization, the control plane, the fleet and the partner console. The registry lists them.

**Lessons:**
- Entity names are one namespace across every compiled app, and a role of the same name merges across packages: PR 54's second run found Property's MaintenanceRequest clashing with Assets'. Property's is RepairRequest now; Healthcare's Front Desk became Clinic Reception and Logistics' Dispatcher Fleet Dispatcher, so a hotel's desk holds no patients and field service no fleet.
- The server's test that every launcher icon can be drawn failed PR 54's first run: three new apps named icons the shell bar did not have. Check `server/shell/runtime.js`'s set when an app picks an icon.
- A list's rows (`find_tx`) come without their child rows: read a record whole with `get_tx` before reading its lines.
- A row built in `before_validate` needs its `id`: the kernel assigns ids to the rows it is given, before hooks run.
- A browser run finds what scenario tests miss. This one found a schema that went stale within one request, a panel that did not refresh, and a word in the singular.
- An entity file that began with a comment was read as one entity (fixed in PR 54).
- sqlx 0.9 refuses SQL built at runtime unless it is marked `AssertSqlSafe`.
- A binary in `target/debug` is a hard link to `deps/`: delete both to free the space. Old test binaries in the shared target held 1.6 GB.

## 2026-11-04, later: help and assistants in the shell, payments for subscriptions, deposits and courses, and Learn's SCORM, xAPI and LTI

**State**: on `main` of each repository; each PR was green on its CI before it merged. No release was tagged.

| Where | What |
|---|---|
| Ferrum PR 49 | Help in the account menu, opening the help center Ferrum-Frontend PR 11 added at `/data/help` (FERRUM-UX-003). Per-app assistants (FERRUM-AI-006): an app's `shell.assistant` guidance joins the context on its pages, `/api/assistant/suggestions` lists its questions, and the panel shows the records an answer read. The connector catalog is `blocked_external` on providers' sandboxes (FERRUM-CONNECT-002) |
| Commerce PRs 1, 2 | Commerce 0.6.0: subscriptions sold in the store (FERRUM-COMMERCE-002). Commerce 0.7.0: part of an invoice asked for, and part of a payment refunded, for other apps |
| Bookings PR 1 | Bookings 0.3.0: Booking Deposits, a connector with Commerce (FERRUM-BOOKINGS-003) |
| ERP PR 5 | ERP 0.7.1: ERP's assistant guidance and suggested questions |
| Learn PRs 1, 2, 3 | Learn 0.3.0, Paid Courses (a connector with Commerce): bought, invoiced, enrolled on payment, refunded within 14 days. Learn 0.4.0: SCORM 1.2 and 2004 lessons through scorm-again 3.4.6 (MIT, vendored with its provenance), and xAPI lessons with an endpoint of their own. Learn 0.5.0: LTI 1.3 tools, launched by OIDC with an id_token signed with Learn's key, sending grades back by AGS (FERRUM-LMS-002) |
| Ferrum PR 50 | Pins ERP 0.7.1 and Learn 0.5.0, and lists Paid Courses where Learn and Commerce are built in |
| HR PR 1 | HR 0.5.1: a submitted separation makes the employee a leaver on its relieving date; that day HR's upkeep ends their sign-in through deprovisioning (FERRUM-JOURNEY-003, partial) |
| Ferrum-Frontend PR 12 | The interface's language: words through `t` (Hindi and Arabic for the kit's), direction, and numbers and dates in a chosen locale (FERRUM-UX-006, partial) |
| Comms PR 4 | Comms 0.8.1: who is about in Chat, from sessions active in the last ten minutes (part of FERRUM-COMMS-002) |
| Ferrum PR 51 | An industrial solution in an app's bundle: the schemes it needs, device profiles and acceptance procedures, checked (FERRUM-IND-018, partial) |
| Ferrum PR 52 | The bootstrap's language from the person or the workspace; HR 0.5.1; three journeys recorded in parts (order to return, partner support, AI-assisted operation); the soak recorded |

**Evidence**:
- Every new test's figures were worked out by hand first.
- Mutants, run by hand from the scratchpad and all caught:
  - 10 for Paid Courses, 8 for SCORM, 8 for xAPI, 8 for LTI;
  - one SCORM survivor, the progress guard, was hidden behind permissions; the test now also tries as the administrator.
- scorm-again's events, data shapes and resume were checked by running the vendored runtimes in Node with a simulated package.
- LTI's id_token was verified against the JWKS Learn publishes. The client assertion was signed with a real tool key, and a second key was refused.
- **The soak passed every check** (soak run 41719722922, `docs/platform/evidence/soak.md`): three hours on the image with PR 45's list order. 11 errors in 2.78 million requests of the heavy workspace, outside its outages, and none for the others. The pace held (p95 268 to 285 ms) while the database grew to 8.8 GB. J12 is passes-in-parts.

**Not done:**
- Nothing here has run in a browser on a running server: the server could not be built on this machine for lack of disk. Real SCORM, xAPI or LTI content has not been tried.
- SCORM and xAPI packages run on the workspace's origin, so Learn Settings must allow them.
- LTI has no Deep Linking and no Names and Role Provisioning.
- The registry lists the rest.

**Lessons:**
- Ferrum strips `Authorization` from requests to app endpoints. A tool's bearer token reaches Learn only because it is a Ferrum API key of the tool's own service user, which the server checks itself.
- Objects built in another `node:vm` realm fail scorm-again's `constructor === Object` checks. Build test inputs inside the context.
- xAPI says an LRS should not refuse a voiding statement whose target is missing or already voided. The first test asserted the opposite.

## 2026-11-04: single sign-on by SAML, keys from a KMS, workflows that undo, and a group's books

**State**: on `main` of each repository, every PR green on its CI before merging. No release was tagged.

| Where | What |
|---|---|
| Ferrum PR 44 | SAML 2.0 (FERRUM-IDENTITY-006) on samael 0.0.22 and libxmlsec1, behind the `saml` cargo feature (on in the image). Each workspace signs its requests with a sealed key of its own, answers become one-time codes, and encrypted assertions are refused. Keycloak 26.8.1 in CI and once in a real browser |
| Ferrum PR 45 | Lists read their default order from an index (DDL version 2 resyncs once); Compose's database gets 512 MB of shared memory. This was the slowdown and the `/dev/shm` errors in the three-hour soak |
| Ferrum PR 46 | `FERRUM_SECRET_KEY` may be OpenBao or Vault Transit ciphertext (`FERRUM_KMS_TRANSIT`), or come from `FERRUM_SECRET_KEY_COMMAND` (FERRUM-IDENTITY-019). OpenBao 2.7.1 in CI |
| Ferrum PR 47 | Sagas on durable jobs: steps saved as they finish, a stable idempotency key per step, compensation newest first, `undo_on_cancel` (FERRUM-PLATFORM-019) |
| Cloud PR 14 | Tests for the 26 serve mutants that had survived, and a weekly `generated-serve` job |
| ERP PR 3 | Finance books (FERRUM-ERP-004), ERP 0.6.0 and Finance 0.3.0 |
| ERP PR 4 | A group's books (FERRUM-ERP-005), ERP 0.7.0: consolidated trial balance, profit and loss and balance sheet, eliminations, what each pair of companies says they owe each other |
| Assets PR 2, 3 | Finance books keep their own values, repairs add life and use parts, grouped assets split (FERRUM-ASSETS-002), Assets 0.3.0 |

**Evidence**: every figure in the new tests was worked out by hand first, and the working is written in
each test's comments. Mutants for each change: 11 for SAML, 2 for KMS, 6 for sagas, 2 for list order, 4 for finance books and 9 for the group's books
(`scripts/mutants.py`, all caught); 13 for Assets, run by hand (all caught after two tests were made
stronger). The three-hour soak is running again on the image with the list order fix (soak run 41719722922); J12 is recorded when it ends.

**Not done, and not code:** an identity provider in a customer's tenant (BLOCK-IDP-TENANT) and a key
service run for real (BLOCK-KMS) are a person's to arrange. Consolidation translates at one closing rate.
It has no ownership percentages, no minority interest, no consolidated cash flow, and does not mirror
intercompany invoices (in the registry). Real provider connectors (FERRUM-CONNECT-002) need accounts with
each provider to verify against: blocked outside.

**Lessons:**
- On macOS, brew's libxmlsec1 and the system libxml2 both got linked, and the test segfaulted. brew's
  libxslt has to come first on `PKG_CONFIG_PATH` (operations.md has the line).
- Keycloak refuses unsigned requests by default.
- Two packages that define the same entity stop a server holding both.
- An app's CI that resolves Ferrum's main unlocked breaks quietly when Ferrum's API moves, and it shows
  only on that app's next PR.
- A mutant that is equivalent for straight-line plans (re-planning a book nothing changed) survived
  until a manual plan was in the test.
- `gh pr edit --body-file` with an empty file empties the description: chain the steps with `&&`.
- The disk filled twice in a day. Superseded artifacts in the shared target, old cargo git checkouts and
  `incremental/` are safe to remove when nothing is building.

## 2026-11-02, evening: what holds when things repeat, arrive late or go wrong

**State**: the review's source findings are fixed, tested, walked on the real binaries as people who
are not administrators, and on `main` of each repository: Ferrum PR 20, 21 and 22, ERP PR 2, IoT Edge
PR 6, 7 and 8. Upgrades from the versions before them were walked too. No release was tagged: signing is
a person's. [Plan](../plan/industrial.md) has the table. This is software correctness only: **nothing has
run on physical equipment**.

| Where | What |
|---|---|
| Ferrum (Operations 0.2.0) | One live command per named thing, operations kept to their app, envelopes with limits, `release_evidence.py` |
| ERP (Warehouse and Shop Floor 0.2.0) | Idempotent release, load evidence, refusals that stay on record, reservations, a second gate for the machine, `line_it.rs` |
| IoT Edge 0.4.0 | Execution bindings, `clock.rs`, the gate before dispatch, recorded dependencies, redirects and the allow list, a frozen CI lane, `dispatch_it.rs` |

**Behaviour that changes for anyone using these:** confirming a task needs a scan of the load; a
machine's `machine.start_job` must be marked **Through Its App Only**; a run waits for its material
unless it says Preloaded; an `allow` entry means the host it names; a gateway upgrade wants nothing
running.

**Not done, and not code:** the review's P2 (one warehouse and one discrete manufacturing reference
deployment on real equipment, against exact artifacts) waits on BLOCK-INDUSTRIAL-HARDWARE and
-VENDORS. Operator trials with gloves and scanners, sustained multi-site load, and a penetration test
are not done. Its P4 (optimization, more protocols, autonomy) stays frozen until those are.

**Lessons:** mutants found two tests that claimed more than they proved, and one mutant pointed at the
wrong suite. A killed mutant run leaves its edit behind: `git status` before anything else. A local
Ferrum patch rewrites an app's `Cargo.lock`, and `git add -A` commits it. A patch is silently unused
when the lock pins another version of the crate: read `[[patch.unused]]`. Every suite was green when a
walk through the real thing found that recovering a mission could leave an order with nothing to pick
it and its wave saying Picked. Walking is not optional. A mutant whose text has moved guards nothing
and stops a full run: four had, and a full run on the final commits found a fifth test that passed for
the wrong reason. Only real CI found that a called workflow shared a concurrency group with its caller's
sibling, and that the frozen lane's frontend was older than the interface needed. Running a green suite
again is cheap: three tries found two flakes in tests written this week.

## 2026-11-01, evening: integrations, the record frame and every app's interface

**State**: everything below is on `main` of its repository, except what is listed under "Not done".

### What landed

| Where | What |
|---|---|
| Ferrum PR 8, 9, 10, 11 | Plugin credentials the plugin cannot read, an OAuth client-credentials broker, per-event outbox leases, hook isolation; a connector contract with capabilities, discovery and health, connectors as plugins, a provider that says wait left alone; records that say what links to them, shared views, `/metrics` for connections, an evidence pack, journeys across apps, Services and Field Operations suites |
| Ferrum-Frontend | Filters by period, emptiness and number; a panel beside lists, boards and calendars; import; merging; related records; shared views; calendar rescheduling; a Connections console; pdf.js |
| All 16 apps | A hand-crafted interface for each. See [references/README.md](../references/README.md) for what each has, how far it was tried and what was left out |
| Commerce gateways | Refunds, a Payment Exception a person decides, signed webhooks, PayPal's token held by the host |
| ferrum-sdk | One SDK, usable as a git dependency, with `fetch_with`, `verify_hmac_sha256` and test helpers |

### Evidence

CI is green on main for every app and for Ferrum at the merge of each PR. `server/tests/journeys_it.rs`
runs lead to cash and procure to pay across the apps and back, on figures worked out by hand. Each
interface has its own tests, type checks and bundle budget. They were then tried by hand on the real
binary in one browser, recorded per app in the references page.

Exercising them found what their tests had not: the click that ends a drag starting a new booking,
Helpdesk and Field Service both defining `ServiceContract` so the server would not boot, a launcher
icon the shell could not draw, a draft asset shown as overdue, and a button that stayed grey with no
reason. Helpdesk's replies had never reached a customer by email at all.

Not run: any real payment, identity, AI, antivirus, mail delivery, media or connector provider;
`preflight.sh` on a clean host; ERP's whole workspace on this machine (CI ran it). No independent
security, legal or accessibility review has happened. Nothing is `verified_complete`.

### Not done

Listed in [backlog.md](backlog.md). The largest: a separate control plane, signup and closure,
billing, a partner console, SAML, a key-management service, cross-system sagas, translation, a help
centre, deferred revenue and accounting dimensions, parallel books and consolidation, and any
certified connector. Publishing the SDK waits on packaging `@ferrum/*` and the owner's npm account;
payment and tax providers wait on keys only the owner can obtain.

### Findings to carry

- Apps commit `ui/dist`, so a Frontend change reaches an app only when its `ui/dist` is rebuilt and
  committed. `npm run check` compares only the app's own sources.
- A workspace keeps an app's installed version: after a pin moves, `POST /api/admin/apps/{id}/upgrade`.
- Locally, `journeys_it` needs a fresh database, because dropping every app's tables runs out of locks.
- Several parallel builds in one `target` use about 4 GB an hour on this disk.

## 2026-11-01: evidence, registry and foundations

**Read at** the revisions in [release-manifest.yaml](release-manifest.yaml). **Nothing here is pushed.**

### Branches to push, in this order

Platform first: the apps' declarations use `retain`, which older platforms refuse.

1. `CBFerrum/Ferrum`, branch `platform/foundation`, on `origin/main` 2f1917d.
2. `privacy/declarations` in sign, lms, pos, hr (with a test fix), drive, projects, assets, commerce,
   field-service, comms.
3. `bookings/series` in ferrum-app-bookings.
4. Bump the pins in Ferrum (`cargo update -p ferrum-app-x`), which moves `not_searched` for those apps.

Pushing to main starts the new release pipeline for the first time. The workflows have only been
checked for shape, and `promote` needs `id-token` and attestations, so watch that first run. The
editors job now pushes `candidate-*` tags, which `promote` does not remove.

Local `main` in the main checkout still holds 8e6205c, the plugin-sealing commit that was never
pushed. It is `acc2063` on this branch, so once that merges, reset local `main` to origin.

### What this work built

| Commit | Requirements |
|---|---|
| acc2063 | `FERRUM-IDENTITY-017` |
| 176d7c7 | `FERRUM-PLATFORM-017`, `FERRUM-EXT-002`, `FERRUM-EXT-003`, `FERRUM-EXT-004` |
| 0128cfc | `FERRUM-PLATFORM-012` to `015`, `FERRUM-PLATFORM-018` |
| 0f28652 | `FERRUM-RELEASE-001`, `002`, `006` |
| 4527f53 | `FERRUM-IDENTITY-002` to `004` |
| 924f3d7 | `FERRUM-VERIFY-001` |
| 83faa02 | Workstream 1: the registry and working documents |
| fc50add | `FERRUM-IDENTITY-010`, `011`, `013`, `017`, `018` |
| 20ef873 | `FERRUM-PRIVACY-002`, `004`, `006` |
| 08525bb | Rotation reaches containers; API reference |
| bookings/series 40fcc5d | `FERRUM-BOOKINGS-002` |

### Evidence

Ran here against PostgreSQL 17 on macOS, on this branch: the workspace's tests, `cargo fmt`, clippy with
warnings as errors, and the binary against a copy of the development database taken before these
migrations, driven over HTTP (42 checks). `scripts/mutants.py` breaks each new invariant and requires
its test to fail: 26 of 26 caught, with seven more for privacy.

Apps against this branch: the full suites of Bookings, POS, Commerce, Projects and HR pass, and the
catalog tests of Sign, Learn, Drive, Assets and Field Service accept their new declarations. Comms'
catalog test passes when built with the current `ferrum-apps`. Not run against this branch: ERP's own
suite, and the full suites of Sign, Learn, Drive, Docs, Assets, Field Service, Forms, Campaigns,
Helpdesk and CRM.

Not run at all: GitHub Actions, Docker or Kubernetes (Docker is not running here), any browser, a
real identity provider, or a real payment or tax provider.

### Findings to carry

- Every sibling checkout had been behind its remote, POS by 10 commits. Probing stale code reports
  finished work as missing: fetch first. Fast-forwarded, except `erp4/ferrum-app-erp`, which holds
  someone's uncommitted tax-profile work.
- `ferrum-app-*/.cargo/config.toml` builds against the sibling `ferrum`. Those files are git-ignored,
  and some lack `ferrum-calc`, which then fails as a type mismatch inside ferrum-app-erp. Pass
  `--config 'patch."https://github.com/CBFerrum/Ferrum".ferrum-calc.path="../ferrum/crates/calc"'`.
- `the_interface_is_a_release_the_platform_takes` fails in Comms with its locked `ferrum-apps`, which is
  older than its manifest. Its lock wants updating; nothing is wrong with the app.
- HR's `expense_claims_and_advances` began failing on 1 November 2026 because a claim took today's date.
  Fixed on `privacy/declarations` (47efc5e), which means HR's CI on main is red until that lands. Other
  tests may have the same trap: a fixed report window around a record dated today.
- 15 of the 17 apps Ferrum pins are behind their own main (release-manifest.yaml). Mostly READMEs, but
  check when bumping.
- The audit's own errors are worth remembering: a zsh loop that did not split paths reported everything
  as missing, and `&&` after `grep` committed past a failing clippy. Gate on the tool's exit code.

### Industrial operations (November 2026)

Ledger, site runtime, warehouse, shop floor, quality and maintenance exist and were walked end to end
on a real server with a simulated fleet ([plan](../plan/industrial.md), `industrial.yaml`). Nothing has
touched hardware and nothing is a safety claim. Mutants live in both repositories: run
`scripts/mutants.py` in `ferrum` and in `ferrum-app-erp` after changing an invariant. Test as a
person with a role, not as an administrator: `links_out_of_reach` exists because that was missed.

### Next, in dependency order

The generated [backlog](backlog.md) is the full list. The most valuable next steps:

1. Apps handle `ferrum.user.deprovisioned`: Drive shares, Helpdesk queues.
2. Mark sensitive entities `reference: restricted`: Employee, SalarySlip. The mechanism exists; no
   package uses it yet.
3. Bulk paths set `PRIORITY_BULK` (importer, `bulk.rs`), and Campaigns' sender gets the fairness the
   outbox has.
4. A shared job queue with cancellation and retry budgets (`FERRUM-PLATFORM-016`).
5. Native hooks audited for external calls beyond the IRP and VIES (`FERRUM-PLATFORM-017`).
6. A standing browser test suite and accessibility checks in CI (`FERRUM-UX-001`, `FERRUM-UX-005`).
7. Then the connector platform (`FERRUM-CONNECT-001`), which industry packs and most of workstream 8
   wait on, and the control plane (`FERRUM-CLOUD-001`), which workstream 9 waits on.

Most of the specification remains: see [capabilities.md](../capabilities.md). Nothing is
`verified_complete`, because no release has been qualified and no person has reviewed anything.

## 5 November: services on their publisher's servers (FERRUM-EXT-001)

The fourth extension class: `runtime: service` with a `service: {url, events}` section. Installing
makes the app an account of its own (kind `service`, role `service:<app>` carrying exactly its
`access`), an API key for it, and a webhook (`webhook.app`) to `{url}/events`; the service is told
at `{url}/install` with the key and the webhook's secret, and refuses by not answering 2xx, which
undoes everything. Deliveries to an app's webhook leave the record's contents out (`payload -
'doc'`): the service reads the record with its key. Off stops the key and the deliveries, removal
revokes the key and posts a signed `{url}/uninstall`. Plugins' accounts are service accounts too
now: they had been made as members, taking a seat (the migration moves existing ones).

- Tested: `server/tests/service_apps_it.rs` against a stand-in service; three mutants.
- Frontend PR 31 labels service apps and shows an app's webhook as the app's.
- Not done: no store has reviewed a service; the store accepts the runtime once its Ferrum pin moves.

## 5 November: the assistant's prompt versions and grounded-answer cases (FERRUM-AI-005)

Each turn hashes what the model is told (instructions, tools, model) into a prompt version, keeps
its text in `assistant_prompt` (per workspace), and records the version with each answer
(`usage.prompt`); `GET /api/assistant/prompts/{version}` reads it back. `server/src/assistant_eval.rs`
grades cases (records, a question, what the answer says, must not say, and reads first);
`ferrum task assistant_eval` runs `server/assistant-evals/grounded.yaml` in a workspace set aside
for it. Tested with a scripted model (`server/tests/assistant_eval_it.rs`): a grounded answer
passes, a guess fails on all three counts. Never run against a real model: BLOCK-MODEL-KEY.

## 6 November: one offline policy, a technician's day offline, the Developer Hub, ERP's contract

- **Offline leases (FERRUM-UX-007).** Each page's bootstrap carries `offline` (allowed, until, hours,
  key, person) under the workspace's `offline` setting (Administration → Workspace → Working offline);
  `GET /api/shell/offline` renews it. `@ferrum/client`'s offline module (Ferrum-Frontend PR 32) names
  cached reads (`cacheName`, under the lease and the person's sign-in key) and work not sent yet
  (`pendingName`, the person's), and `sweep()` forgets cached reads the lease no longer covers.
  POS 0.4.0, Docs 0.7.0 and Field Service 0.3.0 follow it. docs/offline.md is the policy.
- **Field Service 0.3.0 (FERRUM-FIELD-002).** My day is kept under the lease and shown offline;
  setting off and arriving taken offline are sent in order when the connection is back. The day's
  route is outside `RequireAuth` (it asks the server) and its page is kept by the app's own worker.
  Finishing a job still needs the connection.
- **Developer Hub (FERRUM-ECOSYSTEM-013).** New repository `CBFerrum/ferrum-app-developers`, compiled
  in (feature `developers`): an overview, the API reference from `/api/openapi.json` with reads to
  try, the events each record gives, and the person's keys.
- **ERP's contract (FERRUM-PLATFORM-004).** `ferrum-app-erp:docs/contracts.md` and
  `tests/contract_it.rs` (ERP PR 17).
- Walked in the browser: the Working offline setting saves and the lease follows it (8 hours, off,
  back to 72). Field Service's day with the server stopped under the loaded page: the kept day
  shows ("No connection: your day as it was at 19:35"), Start travel is held as waiting, and once
  the server answers again it is sent (the job En Route, nothing waiting). Not walked: a page
  opening offline from an app's worker, since the walking browser refuses service workers (POS's
  existing one fails to register the same way), nor POS and Docs with the network cut.
## 6 November: large files streamed from an app's answer (FERRUM-OPS-006)

`SiteResponse::file(status, type, headers, file)` names a stored file; the server streams it from
storage as the body (`controllers/sites.rs::named_file`, the workspace's own file only) and drops the
naming header. `read_file` refuses files past `MAX_READ_WHOLE` (256 MB, Learn's SCORM ceiling).
Next: Drive's content route answers with the stored file, then its pin moves here.
