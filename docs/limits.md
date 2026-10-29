# Known limits

What Ferrum and its apps do not do yet, or do in a simpler way than you might expect. Each item
is deliberate for now, not a bug; when one matters to you, it is the place to start contributing.

## Platform

- **Row-level rules**: a role's rule reaches all records, the user's own (`if_owner`, with
  `owner_fields`), and/or those whose fields hold given values (`where`), plus per-user scopes on
  link values. Conditions are equality tests on data, select, check, link, email and int fields;
  ranges, dates and expressions ("due in the next week") are not supported.
- **Linked-record titles** (`titles=true`) are shown even when the caller cannot open the linked
  record, as in ERPNext: a record you can read names what it links to. Only the target's title
  field (or number) is exposed, never its other fields.
- **Units of measure**: an item lists its other units with a factor to its stock unit, and order,
  delivery, receipt and invoice lines are entered in any of them (stock moves, valuation,
  reservations and planning count stock units). The factor always comes from the item, not typed
  per line; stock entries and BOMs work in stock units; material requests keep their own factor
  per line. A line delivered in another unit than its order line counts against it through stock
  units, rounded to six decimals.
- **PDFs** come from a separate renderer service (Gotenberg, shipped in both deployments); without
  it, printing is the browser's. Print formats use inline (data:) images, as the renderer fetches
  nothing.
- **Platform mail** (`Kernel::queue_mail`, `queue_mail_with` for stored files, and "Email" on any
  document) sends one HTML message to one address through the server's SMTP settings: no cc,
  threading or bounce handling (the Mail app has those). Attachments need an SMTP server (not
  the development stub), up to 10 files and 20 MB per message.
- **Large files**: browsers upload up to 2 GiB straight to S3-compatible storage; local-disk
  installs stay at the 25 MB request limit. Apps that read a file's bytes (`read_file`) still load
  it into memory, so their own size caps apply. Completing a direct upload reads the object once to
  record its SHA-256.
- **Stock revaluation** is a stock reconciliation counting the same quantity at the new rate. Like
  every stock posting (and landed costs), it is valued when it is posted: a backdated document
  never re-values the ones after it (there is no reposting).
- **Personal data** requests reach what packages declare (`personal_data`), matched by an email
  field: data known only by name or phone is not found; files attached to records, messages sent to
  the person and chat messages are not reached; submitted documents keep their copies of names. Backups taken
  before an erasure still hold the data until they expire.
- **Rate limits** (sign-ins, documents emailed, 300 requests a minute per address on public
  endpoints) are counted in the database, so they hold across replicas; apps use the same
  (`Kernel::rate_limit`). They are per address: Campaigns' tracking pixels and links opened
  through a mail provider's image proxy share that proxy's budget.

## Accounting and finance

- Payment deductions are entered in the payment currency; the exchange difference posts to the
  company's single exchange gain/loss account.
- Budgets have no exception-approver role; dunnings do not net customer credits; MRR uses current
  plans; cash-flow forecasts take subscription amounts before tax.

## Stock and manufacturing

- A template's later changes reach the variants that still have its previous value (a value set
  on the variant stays); pick-list deliveries ship whole bundles; landed costs are posted forward (no repost, the sold share goes to cost of goods sold).

## Assets

- Finance books are labels only; grouped assets are sold whole; capitalization comes from purchase
  invoices only; company currency only; repairs neither extend life nor consume parts; declining
  balance never switches to straight line.

## Studio (reports, dashboards, spaces)

- One child table per query; field paths go at most `table.link.field` deep; comparison periods
  and pivots cannot be combined. Calculated columns work on the result's columns (not on each
  record before grouping) and cannot be sorted on; subtotals go by the first grouping only.
- Running a Studio query through `/api/reports` needs read access to Report (the Studio roles);
  placing a widget on a dashboard or space shares it with that container's audience.

## Apps

- **POS register** (`/app/pos`): pricing needs the server, so a sale can be completed offline only
  if its total was shown before the connection dropped (it then waits on the device and is sent
  later); no customer display, cash drawer or card terminal integration; receipts print through
  the browser. Closing a session uses the POS Closing Entry form.
- **Chat and calendar** (the Mail app): the chat screen (`/app/chat`) covers channels, direct
  messages, threads, mentions, edits, search and files (up to 20 and 25 MB per message; images
  are previewed). The calendar screen
  (`/app/calendar`) shows a month or a week of hours (an agenda on phones) with RSVPs; events
  you organize move by dragging them with a mouse (a repeating event is changed in its form, for
  the whole series); free/busy lookups are API-only.
- **Bookings**: one resource per booking; no recurring
  bookings, deposits or external calendar sync; notices can be up to a minute late. Without the Mail
  app, guest emails carry an add-to-calendar link instead of an invitation.
- **Field Service**: SLAs run in calendar hours; travel times are straight-line estimates.
- **Forms**: views count bot fetches; deleting an anonymous response does not free its
  one-per-respondent slot; CSV export stops at 100,000 rows; up to 500 spam responses per form per
  day are kept for review.
- **Learn**: learners see draft course titles and summaries (not content); instructors read all
  learners unless their course is scoped; certificates are print templates; no paid courses or SCORM.
- **Drive**: public links outlive their creator's access; one copy handles up to 1,000 items or 1 GB.
- **Campaigns**: templated links are not tracked; imports are set-based (no per-contact audit or
  attribute-change journeys); anniversary journeys check the current day only; automatic clicks by
  security scanners are not detected.
- **Sign**: an electronic signature with an audit trail (not PAdES); typed signatures use standard
  fonts, so non-Latin names render poorly; emailed links are readable by Mail managers (use access
  codes); without the Mail app, the signed PDF is emailed only when the server's mailer can
  attach files.
