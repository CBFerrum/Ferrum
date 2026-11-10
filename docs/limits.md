# Known limits

What Ferrum and its apps do not do yet, or do in a simpler way than you might expect. Each item
is deliberate for now, not a bug; when one matters to you, it is the place to start contributing.

## Platform

- **Row-level rules**: a role's rule reaches all records, the user's own (`if_owner`, with
  `owner_fields`), and/or those whose fields hold given values or fall in a range (`where`: `{lte:
  10000}` on a number, `{from_today: 0, to_today: 7}` on a date), plus per-user scopes on link
  values. Equality works on data, select, check, link, email and int fields; expressions across
  fields are not supported. On datetime fields, a range compares the UTC date.
- **Linked-record titles** (`titles=true`) are shown even when the caller cannot open the linked
  record, unless its entity says `reference: restricted`, or a workspace sets that: a record you can
  read names what it links to, and only the target's title field (or number) is exposed, never its
  other fields.
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
- **Large files**: files past the 25 MB request limit go up to 2 GiB in 8 MiB parts through the
  server, on any store, and an upload goes on from what has arrived after a dropped connection,
  or when the same file is chosen again for the same record after the page went (the device keeps
  which upload it was, under the person's offline name; without a lease, only while the page
  lives). An upload left waiting is swept a day after its last part. The shell's uploads that
  outlive the page (`shell().uploads`, Drive's) still send each file whole, to an app's own
  address and within its limit. S3 deployments also take direct uploads with a presigned URL, which
  do not resume. Downloads stream from storage, the platform's and an app's that answers with the
  stored file (`SiteResponse::file`), and answer one byte range when asked (`Range: bytes=…`, 206),
  so a video seeks and plays in every browser; several ranges get the whole file. An app that reads a file's bytes (`read_file`) loads them into
  memory, which is refused past 256 MB (the largest an app works on whole, Learn's SCORM packages).
  Completing an upload reads the object once to record its SHA-256.
- **Stock revaluation** is a stock reconciliation counting the same quantity at the new rate. Like
  every stock posting (and landed costs), it is valued when it is posted: a backdated document
  never re-values the ones after it (there is no reposting).
- **Personal data** requests reach what packages declare (`personal_data`), matched by an email field
  under every address the person has had, or by the digits of a phone field (records holding no
  address by that alone, or through a link to a record found so), with the files attached to those
  records. A name, or a number that differs only by a country code, only lists records as possibly
  theirs and erases nothing; comments they wrote keep their text under an anonymized name; and
  packages that declare nothing are not searched (each answer names them). Records a package
  keeps for a stated reason, and submitted documents' copies of names, stay. An erasure deletes the
  workspace's stored exports; backups taken before it still hold the data until they expire, and
  after a restore `ferrum task reapply_erasures` erases it again (one workspace taken back to a copy
  erases it again itself). A restriction of processing stops
  mail queued to any of the person's addresses; apps that deliver on their own ask
  `Kernel::may_contact`.
- **Rate limits** (sign-ins, documents emailed, 300 requests a minute per address on public
  endpoints) are counted in the database, so they hold across replicas; apps use the same
  (`Kernel::rate_limit`). They are per address: Campaigns' tracking pixels and links opened
  through a mail provider's image proxy share that proxy's budget. Each user's own request budget
  (`FERRUM_USER_REQUESTS_PER_MINUTE`) and each address's (`FERRUM_ADDRESS_REQUESTS_PER_MINUTE`)
  are counted per replica, not across the cluster. Addresses are what the proxies in front report
  (`FERRUM_PROXY_HOPS`); requests that came through no proxy are not counted per address.
- **GST e-invoices and e-way bills** (`india`): Sales Invoices to registered businesses, SEZs and
  buyers abroad, and their credit notes; debit notes, deemed exports, reverse charge and
  e-commerce operators' details are not built. The direct connection speaks the IRP's own API
  (NIC's, or a GSP's that passes it through unchanged); GSPs with APIs of their own need a plugin
  on the documented events. The IRP makes e-way bills only for e-invoices: others, and every
  e-way bill cancellation, go through the e-way bill portal and are recorded here. E-way bills
  are made from Sales Invoices (outward supplies), not from delivery notes or stock transfers. Tax rows are told apart by "CGST", "SGST"/"UTGST", "IGST" and "Cess" in their
  description or account name; other rows count as other charges.
- **Live pushes** (in-app notifications, chat messages) are NOTIFYs sent at commit, and PostgreSQL
  serializes the commits of notifying transactions: at very high message rates they queue. Record
  changes do not (they reach live screens through the outbox, within about 250 ms).
- **Sign-in**: people sign in with a password, a passkey (WebAuthn) or single sign-on. Two-factor
  sign-in uses authenticator apps (TOTP); there are no text-message codes. Single sign-on speaks
  OpenID Connect, not SAML. An account is linked to one identity at a
  time: the one at the provider it last signed in through. People and groups can be provisioned by
  SCIM ([identity-lifecycle.md](identity-lifecycle.md)); without it, someone removed at the provider
  keeps the session they have until it expires unless deactivated here, because Ferrum does not
  receive OpenID Connect's back-channel logout. Apps do not yet hand over what they keep when someone
  leaves: Drive shares and helpdesk queues stay with them.

- **Custom entities** (`define_entity`) keep every record and every line in one table, `custom_record`,
  with their fields in JSON: no index is made on a custom field, so filtering a large custom entity by a
  field reads all its records. Their unique keys live in `custom_key`, kept by saves; a key is a
  field's exact text (as typed, case and all), and engine-kept values (`set_values`) may not touch one.
  A child entity holds no unique keys, is the lines of one table field only, and holds no table of its
  own; customizations add no table field to a package's entity. A total over lines (`rollup`) is a sum
  or a count, with no condition; computed fields see the total of the previous save.
- **Scheduled automations** look for due moments once a minute: the smallest step is a minute, and a
  moment fires up to about a minute and a half late. A schedule on a custom entity's date (or on any
  field kept in JSON) reads every record's date out of JSON on each look that may find something, with no
  index; a repeating schedule on a datetime field looks at every record that has started on every look.
  A monthly schedule is not offered on days 29 to 31: use `last`. Moments more than seven days old when
  the scan reaches them (after the servers were down) are skipped, not fired. A moment fires only for the
  automation, the record and the time; it does not wait for a business calendar, and a schedule cannot
  follow a linked document's change (an invoice paid).
- **Configured actions** (an `add_action` with `effects`, docs/api.md): at most 20 on an entity, 10
  effects and 10 `refuse_if` on one. Effects run as the system in the action's transaction and write only
  the workspace's own fields and fields a person may type; each effect reads the record again, and
  `update_linked` reaches one link of the record (not a child row's, not two links away). The entity's
  description says whether the caller may press each action (`may_run`, from its permission or a rule
  naming it and its `roles`); `condition` and `refuse_if` are judged on the record by the server when it is
  pressed (a form may judge them first, to explain a button that is off). A workspace replaces or removes only the actions its customizations added,
  never a package's. A rule's aggregate filters on the day as `{"today": n}`, compared with date fields only, never
  with a datetime, and on the saved record's own field as `{"doc": "<field>"}`, one of the same type, as it
  stands before derivations.
- **Linked records in expressions** (`links.<field>`, BD-14): one level (a link of the linked record is an
  id), at most 5 in one expression, read as the workspace without a lock (a rule sees the linked record as it
  is when the save runs, and two saves at once can each pass a check the other would have failed). Not in
  defaults, computed fields, `visible_if`, `required_if`, `read_only_if` or a line's rules; the browser leaves
  conditions that use them to the server, and the approvals inbox lists a step whose guard uses them.
- **Effects on lines** (`append_line`, `set_line`): one line per effect, on the record's own tables (not a
  linked record's, not lines within lines); `set_line` refuses when no line or more than one matches; not on a
  submitted record; the usual cap on lines.
- **Another app's steps** (`invoke`, docs/api.md): a button or an automation calls what the capability
  catalog lists (record operations, an app's listed actions, workflow transitions), never a configured
  action. ERP's own money and stock actions carry no contracts yet, so what it reaches there is creating
  and submitting its documents (a sales invoice, a delivery note) with the lines given; amounts are the
  expressions' numbers, not typed money. An automation a pack installs has its `invoke` checked when it
  runs, not at install. The default key holds per record version: a press after any change to the record
  runs the step again, so a step meant once for good names its own `key`.
- **Service targets** (`add_target`, docs/api.md): at most 8 on an entity, not on lines or ledgers. A target
  added to an entity that has records starts on each record at its next save: nothing goes back over
  existing records. A round keeps the open time worked, not a list of pauses, so a calendar changed during a
  round counts the time before the change as it was counted then. Breaches are marked by the minute loop: up
  to about a minute and a half after the due moment, at most 200 rounds a pass and 50 of one workspace's
  (a full pass looks again at once). The breach is written into the record's two fields in any document
  status, past its rules, with an `updated` event; when that fails (a calendar gone under it, an expression
  that no longer evaluates) the breach stays recorded and the state field shows it from the record's next save. There is no separate breach event, no endpoint per
  record and no screen yet: the two fields, automations and reports are the way to see them.
- **Redefining a custom entity** (`redefine_entity`) deletes no data: values of a field that goes stay in
  the records, out of sight, until a field of the same name comes back. A field that comes back with a
  type other than the one its values were saved under is not checked against them. A batch of
  customizations holds at most 100 operations and is validated one operation at a time (a schema build
  each), so its order matters.
- **Packs** (`/api/admin/pack`) are unsigned JSON, exported and installed by administrators; nothing
  is uninstalled, and an install never deletes: a customization, rule, automation, Studio item or record
  the source dropped stays in the target. Automations, Studio's items and records are matched by name,
  label and key, so two of one name are refused at export. Studio's queries and dashboard filters that
  hold record ids, and links between records by id inside JSON fields, travel as they are and point at
  nothing in another workspace. Records of an entity are installed in the order `records` names them: a
  link to a later one is refused. A private Studio item arrives owned by the installer. A pack holds at
  most 5 MiB and 10,000 records of one entity. A solution arrives as a draft, and a pack's plan does the
  install and rolls it back, so a large pack is planned at the cost of installing it. A pack carries the
  workspace's own interface words as `InterfaceWord` records (key `[language, english]`); the web pages read
  them for the names of entities, fields, choices, buttons, workflow states and sections; messages of
  checks and refusals, mail text and descriptions stay as written.
- **A workspace's own menus** (`define_module`) are named `x_…` and hold at most 50 items. Each opens
  the records frame at `/data/{menu}/`, never a screen of its own; it gives the bar no search provider
  and its tile no counts. Its tile goes to whoever reads one of its entities, not to chosen roles. An
  entity that goes leaves the menu with a schema warning.
- **Merging duplicates** is for entities that opt in (customers and suppliers so far). It finds
  duplicates by equal fields, not by resemblance, and is not undone. Append-only history keeps the id
  it was posted under: a report on it must ask `ferrum_kernel::merge::merged_ids`.
- **Field masks** show part of a value to someone outside its field group. A masked field cannot be
  filtered or sorted on by them, and they cannot write it back. No shipped package declares one yet.
- **Restricting processing** stops queued mail, and what asks `Kernel::may_contact`. It does not stop
  people reading or editing the records.
- **Retention rules** are set per workspace and per entity; packages suggest none. Customizations made
  before they remembered what they replaced are not compared with upgrades.
- **Hook order** can be declared and is checked, but apps' hooks have not declared theirs yet: they run
  in registration order.
- **The assistant** speaks one provider's protocol, or a proxy's that speaks it, and has no regional
  routing. Budgets are in tokens. What may leave is decided by `internal` entities and permission groups:
  a package that marks neither sends everything a person can read.
- **Working time** (`Calendar::add_working`, `working_between`, `next_open`, a wait in open time) looks
  at most 1,830 days (five years) ahead of where it starts, and says so past that; `working_between` counts
  over at most that span. Hours of the day are whole minutes. Deleting a calendar is not refused while a
  release's waits name it.
- **Background jobs** have no calendar: nothing schedules one for later or repeats it. Apps' periodic
  work stays in their `maintenance`. A running job hears of a cancellation within 5 seconds, and work
  done in earlier transactions stays done.
- **Workspace exports** are for taking data away, and nothing here reads one back in: restoring a
  workspace is done from a database backup. Each attached file is held in memory while it is written
  (the largest attachment sets what a replica needs), and a part that fails to store costs the
  attempt, which starts over. A person's erasure reaches exports already made only when they are
  deleted, after 7 days. Tables nobody declared are named in the manifest, not carried.
- **Malware scanning** looks at a file once, when it is uploaded: files stored earlier, or past the
  scanner's size limit, stay `unscanned`, and nothing rescans them when signatures change.
- **Bulk jobs** reach at most 100,000 records, and list the first 1,000 failures.

## Operations and the site runtime

- **Nothing has run on hardware.** Every adapter was tried against stand-ins ([compatibility](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/compatibility.md)). Nothing here is a functional-safety claim or a vendor qualification.
- **Definitions reach what they describe, on paper.** OPC UA, Sparkplug B, MTConnect, VDA 5050 3.0.0, Open-RMF, openTCS and rosbridge are read by definitions tried against stand-ins and other people's libraries. Sparkplug is not a primary host, and OPC UA does not browse ([compatibility](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/compatibility.md)).
- **Flows** have up to 500 steps and 2,000 wires, and run only at gateways: a device posting directly to the platform has none. What they remember is written down every 15 seconds, so a crash can lose that much and repeat an effect from it.
- **A serving node's figures come from one laptop.** About 54,000 requests a second for a 16 KB page, at 2 ms p50 and 7 ms p99 with 128 clients, with the load generator on the same machine. `docs/platform/evidence/hardening.md` has the rest. Nothing here describes a deployment, and nothing has run for days.
- **What a gateway sustains is not measured.** Decoding alone did 369,000 JSON messages a second and flows 269,000 readings a second on an Apple M1 Pro. Disk and network come first, and on gateway hardware nobody has tried it.
- **A link** keeps track of 20,000 different values, and a listener 1,024 callers. Past that, what is new is dropped and counted.
- **Peers** are one hop, and signed but not encrypted. Two Ferrum platforms exchange data through exports and ingest: there is no federation.
- **Certificates of a plant's own authority** work for `https` and `mqtts`, not `wss`.
- **Failover** between gateways is a person's decision, by design. There is no standby that takes over by itself.
- **Device keys** do not rotate in place: quarantine and enroll a new one.
- **Signed site updates** check the signature, version, expiry and hash, and roll back a build that keeps dying. They are tested in pieces, and have not been run through systemd on a gateway. Ferrum publishes no signing keys or releases: a site makes its own.
- **Console PINs** are short by nature, and the gateway holds their salted hashes: someone with its disk could recover them. Keep its storage encrypted; a site's list follows the platform's at each exchange, and the lease bounds how stale it gets.
- **Site traffic** shares the 300 requests a minute per address and workspace of `/api/site`. At the default of an exchange every 5 seconds, that is about 25 gateways behind one address. Past it a site backs off and catches up.
- **Telemetry** lives in PostgreSQL. It has not been measured against the strategy's 20,000 samples a second, which would be 345 GB a day: aggregate at the edge first.
- **A floor report** names a run by its number and is applied when it arrives. One the run cannot take stays in `op_fact` and the run says so; one naming no run stays in `op_fact` alone.

## Warehouse and shop floor

- **Picking**: discrete and batch by wave. Zone, cluster and goods-to-person picking are not built, nor are substitutions, cartonization or carrier connectors. A shipment holds one tracking number at a time; earlier parcels are in its audit trail.
- **Manufacturing execution** is discrete. Batch and continuous processes, and energy per run, are partner work. Recipes are approved job references, never controller logic.
- **Leftover material at the line** stays in the production bin until someone moves it.
- **OEE** is per workstation and period, by a written definition. A line's figure is not the product of its machines'.

## Accounting and finance

- Payment deductions are entered in the payment currency; the exchange difference posts to the
  company's single exchange gain/loss account.
- Budgets have no exception-approver role; dunnings do not net customer credits; MRR uses current
  plans; cash-flow forecasts take subscription amounts before tax.

## Stock and manufacturing

- A template's later changes reach the variants that still have its previous value (a value set
  on the variant stays); pick-list deliveries ship whole bundles; landed costs are posted forward (no repost, the sold share goes to cost of goods sold).

## Assets

- Depreciation booked before a grouped asset is split is shared by the two assets, so neither can
  then be cancelled; an asset a repair or a value adjustment changed is not split. Capitalization
  comes from purchase invoices only; company currency only; declining balance never switches to
  straight line.

## Studio (reports, dashboards, spaces)

- One child table per query; field paths go at most `table.link.field` deep; comparison periods
  and pivots cannot be combined. Calculated columns work on the result's columns (not on each
  record before grouping) and cannot be sorted on; subtotals go by the first grouping only.
- Running a Studio query through `/api/reports` needs read access to Report (the Studio roles);
  placing a widget on a dashboard or space shares it with that container's audience.

## Apps

- **POS register** (`/pos/`): sells offline from the register's price book, priced on the device with
  the server's own engine (`@ferrum/calc`). Sales wait on the device and the server prices each again
  when it arrives; one whose payments no longer cover it waits for a person. Stock is not reserved
  for an offline till, so two tills can sell the last item. No customer display, cash drawer or card
  terminal integration; receipts print through the browser. Closing a session uses the POS Closing
  Entry form.
- **Chat and calendar** (the Mail app): the chat screen (`/app/chat`) covers channels, direct
  messages, threads, mentions, edits, search and files (up to 20 and 25 MB per message; images
  are previewed). The calendar screen
  (`/app/calendar`) shows a month or a week of hours (an agenda on phones) with RSVPs; events
  you organize move by dragging them with a mouse (a repeating event is changed in its form, for
  the whole series); free/busy lookups are API-only.
- **Bookings**: one resource per booking; bookings repeat weekly, every 2 or 4 weeks or monthly at the
  staff member's local time, each an ordinary booking, and a guest gets a confirmation for each;
  no deposits, and calendars go out as signed feeds with nothing read back; notices can be up to a
  minute late. Without the Mail app, guest emails carry an add-to-calendar link instead of an
  invitation.
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
