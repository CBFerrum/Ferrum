# Known limits

What Ferrum and its apps do not do yet, or do in a simpler way than you might expect. Each item
is deliberate for now, not a bug; when one matters to you, it is the place to start contributing.

## Platform

- **Row-level rules** are owner-based (`if_owner`, with `owner_fields`) plus per-user scopes on
  link values (a user limited to some companies, territories...). A role cannot carry an arbitrary
  condition such as "published courses only"; apps enforce those in their hooks.
- **Linked-record titles** (`titles=true`) are shown even when the caller cannot open the linked
  record, as in ERPNext: a record you can read names what it links to. Only the target's title
  field (or number) is exposed, never its other fields.
- **Units of measure**: material request lines convert with a factor per line; there is no
  per-item conversion table that sales and purchase documents pick factors from.
- **PDFs** come from a separate renderer service (Gotenberg, shipped in both deployments); without
  it, printing is the browser's. Apps make PDFs with `Kernel::print_pdf` and store them as files;
  the Mail app can attach files to emails, the platform mail queue cannot. Print formats use inline
  (data:) images, as the renderer fetches nothing.
- **Platform mail** (`Kernel::queue_mail`, used when the Mail app is not installed) sends one HTML
  message to one address, without attachments, through the server's SMTP settings.
- **Large files**: browsers upload up to 2 GiB straight to S3-compatible storage; local-disk
  installs stay at the 25 MB request limit. Apps that read a file's bytes (`read_file`) still load
  it into memory, so their own size caps apply. Completing a direct upload reads the object once to
  record its SHA-256.
- **Stock revaluation** without moving quantity is not supported; landed costs are posted forward.
- **BOM costing**: apps cannot plug into "Update Cost"; Manufacturing Operations adds its own action.
- **Engine-maintained child tables**: apps that keep rows in another document's table (crew rows,
  allocations) write them with SQL rather than through a kernel API.
- **SDK**: `ferrum-app dev --watch` re-uploads under the same version; bump the version (or use a
  pre-release such as `0.1.0-dev.3`) when content changes, as the server refuses changed content
  under an installed version.
- **Rate limits** are per replica and per IP for public endpoints (300 per minute), including
  Campaigns' tracking pixels and links behind a mail provider's image proxy.

## Accounting and finance

- Payment deductions are entered in the payment currency; the exchange difference posts to the
  company's single exchange gain/loss account.
- Budgets have no exception-approver role; dunnings do not net customer credits; MRR uses current
  plans; cash-flow forecasts take subscription amounts before tax.

## Stock and manufacturing

- Variant fields are not copied again when the template changes; pick-list deliveries ship whole
  bundles; landed costs are posted forward (no repost, the sold share goes to cost of goods sold).
- A manufacture request line is ordered by its whole work-order quantity, even if the work order is
  stopped before finishing.

## Assets

- Finance books are labels only; grouped assets are sold whole; capitalization comes from purchase
  invoices only; company currency only; repairs neither extend life nor consume parts; declining
  balance never switches to straight line.

## Studio (reports, dashboards, spaces)

- One child table per query; field paths go at most `table.link.field` deep; no subtotals or
  calculated columns; comparison periods and pivots cannot be combined.
- Running a Studio query through `/api/reports` needs read access to Report (the Studio roles);
  placing a widget on a dashboard or space shares it with that container's audience.

## Apps

- **POS register** (`/app/pos`): pricing needs the server, so a sale can be completed offline only
  if its total was shown before the connection dropped (it then waits on the device and is sent
  later); no customer display, cash drawer or card terminal integration; receipts print through
  the browser. Closing a session uses the POS Closing Entry form.
- **Chat and calendar** (the Mail app): the chat screen (`/app/chat`) covers channels, direct
  messages, threads, mentions, edits and search, but not file attachments in messages. The
  calendar screen (`/app/calendar`) is a month grid and phone agenda with RSVPs; there is no week
  view or drag-to-reschedule, and free/busy lookups are API-only.
- **Bookings**: the write throttle is in memory per replica; one resource per booking; no recurring
  bookings, deposits or external calendar sync; notices can be up to a minute late. Without the Mail
  app, guest emails carry an add-to-calendar link instead of an invitation.
- **Field Service**: SLAs run in calendar hours; travel times are straight-line estimates.
- **Forms**: views count bot fetches; deleting an anonymous response does not free its
  one-per-respondent slot; CSV export stops at 100,000 rows; up to 500 spam responses per form per
  day are kept for review.
- **Learn**: learners see draft course titles and summaries (not content); instructors read all
  learners unless their course is scoped; certificates are print templates; no paid courses or SCORM.
- **Drive**: public links outlive their creator's access; one copy handles up to 1,000 items or 1 GB;
  bytes of deleted files stay in storage until Drive adopts the kernel's file removal API.
- **Campaigns**: templated links are not tracked; imports are set-based (no per-contact audit or
  attribute-change journeys); anniversary journeys check the current day only; automatic clicks by
  security scanners are not detected.
- **Sign**: an electronic signature with an audit trail (not PAdES); typed signatures use standard
  fonts, so non-Latin names render poorly; emailed links are readable by Mail managers (use access
  codes); emails need the Mail app, because the completion email carries the signed PDF.
