# References

One page per product: what the open-source references do, what Ferrum has, what is missing, and which
screens are worth hand-crafting. Written on 2026-11-01 by reading the code of both sides, with every
claim pointing at a file. Nothing here was run, so "does" means "the code does".

| Page | Compared with |
|---|---|
| [edge.md](edge.md) | ThingsBoard, OpenRemote, EdgeX, Node-RED, Ignition, Open-RMF, VDA 5050 and others |
| [crm.md](crm.md) | Twenty, Frappe CRM |
| [projects.md](projects.md) | Huly, Plane |
| [helpdesk.md](helpdesk.md) | Frappe Helpdesk, Chatwoot |
| [hr.md](hr.md) | Frappe HR |
| [office-drive-docs.md](office-drive-docs.md) | Collabora Online, ONLYOFFICE, Univer, the Craft editors, Outline, Docmost, Frappe Drive, Nextcloud |
| [comms.md](comms.md) | Stalwart, LiveKit Meet, Zulip |
| [campaigns.md](campaigns.md) | listmonk, Mautic |
| [commerce.md](commerce.md) | Medusa |
| [pos.md](pos.md) | POS Awesome |
| [bookings.md](bookings.md) | Cal.com |
| [forms.md](forms.md) | Formbricks |
| [sign.md](sign.md) | Documenso, DocuSeal |
| [lms.md](lms.md) | Frappe LMS |
| [assets.md](assets.md) | Snipe-IT |
| [field-service.md](field-service.md) | OCA Field Service |
| [erp.md](erp.md) | ERPNext |
| [cloud.md](cloud.md) | Forgejo, Gitea, GitLab, Caddy, kamal-proxy, Pingora, Coolify, Railpack, BuildKit, Knative, Falco, k6, cargo-mutants: what Cloud built that they already do |

The copies themselves live outside this repository, in `../referance/` beside the app checkouts, with
how they were fetched, their commits and their licences. Ferrum's interfaces are written by hand:
these are read for what a product does and how it feels, never copied.

## Where each app stands

The pages above are the gap lists as they were read. These interfaces were then built by hand against
them, and this table says what was left out on purpose. "In a browser" says how far it was tried on a
real server; every one also has its own type checks, tests and bundle budget.

| App | Built | In a browser | Left out |
|---|---|---|---|
| CRM | Pipeline board, deal page, today, lost reasons | Board, deal, today | |
| Projects | Board, timeline, task pages, my work, planning, overview strip | Board and its order, timeline | One-key edits, drag to reassign, leave, lag, pinned tasks, cycles, intake |
| Helpdesk | One feed, reply and note composer, saved replies, files, inbox with keys, who has what; replies now reach the customer by email | Inbox, a reply emailed, first response met | Presence, merge, mentions, portal uploads, reading mail replies back, a public help centre |
| HR | Approvals, my space, team calendar, directory and org chart, careers, attendance requests, leave accrual | Four main screens, with no requests waiting | A step to send a claim for approval, onboarding tasks, CV upload, salary ranges |
| Bookings | Guest page, team schedule, today, booking page | Guest page on a phone, click to book, a refused move | Holding a time while typing, date overrides, hours editor, troubleshooter, hotel stays |
| Forms | Respondent page, builder, results | Template to published form to a response, on a phone | QR code, embed snippet, jump logic, partial responses |
| Sign | Signing page, sending, tracking, templates, real pages by pdf.js | Upload to sealed, on a phone | Multi-select, uploading a signature, preview as signer, a page for the signer's sealed copy |
| Learn | Player, quizzes, assignments, catalog, public pages, teaching, builder | Build, publish, enrol, player | Lessons that unlock in order, notes, ratings, per-lesson class counts |
| Assets | Overview, asset page, verify a room | Overview, a draft asset's page | Labels, bulk hand-out, camera scanning, reminders |
| Field Service | Dispatch board, my day, job page, work order page | Dispatch with the server's score | Offline queue, serial scanning, van stock, maps, leave, public tracking |
| Campaigns | Composer, review, results, audiences, import | List, new campaign | Social publishing, drag-and-drop blocks, journey canvas, contact profile |
| Drive | Folders of any size, sharing rules, moving, team folders, keys, search in the bar | List | Grid and thumbnails, folder zip, creating role shares, live updates for most people |
| Docs | Slash menu, mentions, contents, find and replace, history, Word and Markdown import, home | Slash menu, mention, saved and reloaded | Pictures in Word files, a folder tree |
| Comms | Chat notices, unread, members, search, mail conversations and drafts, domain health, RSVP, find a time, honest meeting state | Empty and unconfigured states only | Reactions, presence, scheduled send, filters, recording; no live mail or media server here |
| POS | Till, line editor, pay window, hold, returns, open and close, Z report | A cash sale with change | Store credit, whole-sale discount, manager PIN, coupons; no printer here |
| Commerce | Order desk, a money screen for refunds and exceptions, returns and refunds through the gateway, variants, tracking by link, a finished storefront | Desk, and the storefront on a phone as far as the cart | Refunds in another currency than the company's, variants made in bulk; checkout never met a real provider |
| ERP | Bank workbench, payment allocation, comparative statements, cash flow, figures that open their entries, a finance home, exchange revaluation; deferred revenue and expense with their report, accruals that reverse themselves, a supplier's orders on the portal | Finance home, profit and loss, the bank page with no account yet; a deferred invoice through its report, an accrual reversed, Your orders as a supplier | Dimensions beyond cost centre and project, statement rules, OFX and MT940 import |
