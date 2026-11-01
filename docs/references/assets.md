# Assets: references

**Compared with:** Snipe-IT `e6dded3eac75` (`referance/assets/snipe-it`; reference paths below start there) against Ferrum Assets `a8b657d5c725` (`ferrum-app-assets`, working tree, clean; Ferrum paths start at the ERP folder), the records frame in `ferrum/web` `9ecf5bfc8404` and the platform `ferrum` `2a701db8901a` (both have uncommitted edits, `ListView.tsx` among them; only the files named were read). **Date:** 2026-11-01

## What Ferrum has

- A finance-grade fixed-asset register, 14 entities: `ferrum-app-assets/packages/assets/entities/assets.yaml` (location tree, finance-book label, categories with per-company accounts, assets numbered `AST-YYYY-NNNNN` with custodian, cost centre and a schedule table) and `operations.yaml` (movements Transfer/Issue/Receipt, value adjustments, repairs, maintenance plans with logs, disposals).
- Rust engine: exact depreciation, four methods, pro-rata start (`ferrum-app-assets/src/schedule.rs`); a per-minute upkeep that posts due depreciation as journal entries and opens due maintenance tasks (`src/periodic.rs`, `src/maintenance.rs`); capitalisation with CWIP from purchase invoices (`src/purchase.rs`); revaluation and capitalised repairs (`src/value.rs`); sale or scrap with gain or loss (`src/disposal.rs`); custody history (`src/movement.rs`); five reports (`src/reports.rs`).
- Three roles, row access by custodian, personal-data erasure and hand-over of assets when a custodian leaves: `ferrum-app-assets/packages/assets/package.yaml`.
- No `ui/` (`ferrum-app-assets/app.yaml` declares none), so people use the generic frame: list with filters, saved views, bulk bar and CSV export, board, calendar and timeline (`ferrum/web/src/app/ListView.tsx`, `Views.tsx`), and a form with tabs, activity, comments and attachments (`FormView.tsx`, `Collab.tsx`).
- Not there (grepped `src`, `packages`, `tests`): serial number, manufacturer or model, warranty, end-of-life date, verification or audit, barcode or QR, expected return date, photo. Deliberate limits are in `ferrum/docs/limits.md` (Assets).

## What the references do that Ferrum does not

| Capability | Where in the reference | Ferrum today | Worth building (must/should/later/no) | Size (S/M/L) |
|---|---|---|---|---|
| Physical verification: record that an asset was seen, a next-audit date, a due list, alerts, bulk audit by scanning | `app/Http/Controllers/Assets/AssetsController.php` (quickScan, audit, dueForAudit), `resources/views/hardware/quickscan.blade.php`, `app/Console/Commands/SendUpcomingAuditReport.php` | None: no verification field or entity in `ferrum-app-assets/packages/assets/entities/*.yaml`; the platform audit trail records edits, not sightings | must | M |
| Identity data on the asset: serial, manufacturer, model number, supplier, order number, warranty months, end-of-life date, photo | `app/Models/Asset.php` (rules and fillable list), `AssetModel.php`, `Manufacturer.php`, `Supplier.php` | The Asset entity (`ferrum-app-assets/packages/assets/entities/assets.yaml`) has only `item` and `purchase_invoice` to say what it is | must | S |
| Labels and scan lookup: QR and 1-D codes for sheet and tape printers; open an asset by tag or serial | `app/Http/Controllers/LabelsController.php`, `app/Models/Labels/` (Avery, Hema, Brother, Dymo, Zebra), routes `bytag` and `byserial` in `routes/web/hardware.php` | The print engine has a `qr()` function (`ferrum/crates/kernel/src/print.rs`) but the package ships no print format and no 1-D code; the frame has no scan input or camera code (`ferrum/web/package.json`, `src`) | should | M |
| Check-out and check-in with an expected return date, a target (person, location or another asset), an overdue list, bulk | `app/Http/Controllers/Assets/AssetCheckoutController.php`, `AssetCheckinController.php`, `BulkAssetsController.php`, `app/Notifications/ExpectedCheckinNotification.php` | Issue and Receipt movements set and clear the custodian and keep history (`ferrum-app-assets/packages/assets/entities/operations.yaml`, `src/movement.rs`); no return date, overdue list or notice | should | M |
| Operational status that decides availability (ready, pending, broken, lost, stolen, archived) | `app/Models/Statuslabel.php`, `database/seeders/StatuslabelSeeder.php`, `database/factories/StatuslabelFactory.php`, `availableForCheckout` in `app/Models/Asset.php` | Status is derived from schedule and documents (`status` in `assets.yaml`); disposal is only Sell or Scrap (`operations.yaml`): a lost asset can be scrapped, but there is no lost or stolen state, reason or recovery | should | S |
| Needs-attention dashboard and alerts: audits, returns, maintenance overdue, warranty and end of life | `app/Livewire/NeedsAttention.php`, `resources/views/dashboard.blade.php`, `app/Console/Commands/SendExpectedCheckinAlerts.php`, `SendExpirationAlerts.php` | Maintenance-due notices (`ferrum-app-assets/src/maintenance.rs`), a notice when a posting fails (`src/periodic.rs`) and the Maintenance Due report (`src/reports.rs`) | should | S |
| CSV importer with column mapping and sample files | `app/Importer/AssetImporter.php`, `app/Livewire/Importer.php`, `sample_csvs/` | Roles carry `import` (`ferrum-app-assets/packages/assets/package.yaml`); the generic import of existing assets with opening depreciation is not verified | should | S |
| Acceptance of custody by the receiver (terms, signature, decline, reminders) | `app/Models/CheckoutAcceptance.php`, `app/Console/Commands/SendAcceptanceReminder.php` | None in `operations.yaml`; whether Ferrum Sign (`ferrum-app-sign`) could drive it is not verified | later | M |
| Staff requests for a requestable asset, with a waiting list | `app/Models/CheckoutRequest.php`, `app/Models/Traits/Requestable.php`, `resources/views/hardware/requested.blade.php` | None | later | M |
| Software licences with seats and expiry | `app/Models/License.php`, `LicenseSeat.php`, `app/Notifications/ExpiringLicenseNotification.php` | None: no licence entity in `ferrum-app-assets/packages/assets/entities/` | later | L |
| Accessories and components assigned to a person or an asset, with stock floors | `app/Models/Accessory.php`, `Component.php`, `Consumable.php` | Consumables are ERP stock items; nothing attaches spare parts to an asset (`ferrum/docs/limits.md`: repairs do not consume parts) | later | M |
| Calendar of asset dates (audit due, return due, end of life, warranty end) | `app/Models/CalendarEvent.php`, `app/Models/Traits/HasCalendarEvents.php` | The frame calendar draws one date field of one entity (`ferrum/web/src/app/Views.tsx`) | later | S |
| Kits: one action hands out a standard bundle | `app/Models/PredefinedKit.php`, `app/Http/Controllers/Kits/CheckoutKitController.php` | None | later | S |
| Saved custom report templates | `app/Models/ReportTemplate.php`, `ReportsController.php` (postCustom) | Studio's report builder covers it (`ferrum/web/src/app/studio/report/`) | no | S |

Size is a guess: S a few days, M a few weeks, L more. Custom fields per model (`app/Models/CustomFieldset.php`) are left out: Ferrum adds fields per entity in `ferrum/web/src/app/Customize.tsx`. Read in code: asset controllers, models, the quick-scan and checkout views, the dashboard widget; the rest by file names only (not verified). ERPNext's own `assets` module is outside this brief.

## Screens to hand-craft

1. **Scan station** (asset manager, at a desk or walking a room with a phone). Purpose: verify a room, hand out or take back many assets in one sitting.
   - Layout: mode switch (Verify, Hand out, Take back) and options on the left (location, person, next verification date, note); a running result list on the right with a counter such as "12 of 40 expected here"; on a phone the scan box sits above the keyboard and the list scrolls beneath.
   - Interactions: the box stays focused and Enter submits, so a hardware scanner needs no clicks; each scan answers at once with a green or red row carrying the server's reason (other company, already held, not in service); optional sound; undo the last scan; at the end, assets expected in the location but not scanned, and scanned ones that belong elsewhere, are listed with one-click move or flag.
   - Data: have `number` as the tag, `location` and `custodian` (`ferrum-app-assets/packages/assets/entities/assets.yaml`) and movements to hand out or take back (`operations.yaml`). Missing: a verification record with last and next dates (table row 1) and a serial number (row 2). Camera scanning in the browser is not verified.
2. **Asset page** (everyone). Purpose: answer "what is it, where is it, what is it worth, what happened to it" on one page.
   - Layout: header with status, net book value, custodian, location and buttons for the actions the record allows (move, hand out, repair, revalue, dispose); photo and facts on the left; tabs for Depreciation (book value over time, booked rows solid, planned rows dotted, a failed posting in red with its reason), History (one timeline of movements, repairs, value changes, maintenance logs, verifications), Maintenance and Files.
   - Interactions: hover a point for the period's amount and its journal entry; click a timeline entry to open it; each maintenance task shows its next due date as a chip.
   - Data: have schedule rows with `booked`, `journal_entry` and `posting_error` (`assets.yaml`), the five document types and attachments (`ferrum/web/src/app/Collab.tsx`). Missing: one history query across the document types (not built), a photo field, verifications.
3. **Register overview** (finance and asset managers). Purpose: where the money is and what needs a person today.
   - Layout: net book value by category and by location, depreciation due by month, then a short needs-attention list: failed postings, returns overdue, verification due, maintenance overdue, fully depreciated assets still in service.
   - Interactions: every figure opens the matching filtered list; an as-of date selector; rows export.
   - Data: have the Fixed Asset Register and Maintenance Due reports (`ferrum-app-assets/src/reports.rs`) and `posting_error`. Missing: return and verification dates (table rows 1 and 4). Studio (`ferrum/crates/studio`) can draw the charts; whether an app can ship them as seed data is not verified.
4. **Label sheet** (from the list's bulk bar). Purpose: print tags for new or re-tagged assets.
   - Layout: a dialog with sheet or tape size, content (QR, number, name, location) and a live preview of the first page.
   - Interactions: select assets in the list, press Print labels; the last layout is remembered; printing goes through the browser.
   - Data: have the number and a QR function in the print engine (`ferrum/crates/kernel/src/print.rs`). Missing: sheet geometries; take them from the printer makers' data sheets.

## What Ferrum already does better

- Books: depreciation is exact to the currency's decimals, posted to the ledger as balanced journal entries, idempotent and resumable (`ferrum-app-assets/src/schedule.rs`, `src/periodic.rs`). Snipe-IT shows an estimate from purchase cost and months (`app/Models/Depreciable.php`) and has no ledger (a search of `app/` and `resources/views/` for journal or chart-of-accounts code found only a notes relation on the action log and a comment).
- Capitalisation from purchase invoices with CWIP, capitalised repairs, impairment and revaluation, and disposal with gain or loss through a sales invoice (`src/purchase.rs`, `src/value.rs`, `src/disposal.rs`) have no counterpart in Snipe-IT (a search of `app/`, `resources/views/` and `database/migrations/` for impairment, revaluation, capitalisation, CWIP and gain or loss found nothing relevant).
- Maintenance as plans: periodic tasks open logs and notify the assignee (`ferrum-app-assets/src/maintenance.rs`); Snipe-IT's maintenance is a one-off record with dates and cost (`app/Models/Maintenance.php`; no recurrence found).
- Custody is guarded: a movement cannot be undone once a later one moved the asset (`src/movement.rs`), and assets in someone's care are handed over when they leave (`packages/assets/package.yaml`).
- Figures are recomputed from the documents behind them, never accumulated (`src/lib.rs`), and every report applies the caller's row access (`src/reports.rs`).

## Licence notes

- Snipe-IT is AGPL-3.0 (`referance/assets/snipe-it/LICENSE`, `referance/INDEX.md`); Ferrum Assets is AGPL-3.0-only (`ferrum-app-assets/app.yaml`). Compatible, but `referance/README.md` forbids pasting from the folder and nothing was pasted: this page describes behaviour and names files.
- Label sheet and tape geometries (`app/Models/Labels/`) are code written for Avery, Hema, Brother, Dymo and Zebra products; derive sizes from the makers' data sheets, not from those classes.
- The folder's `AGENTS.md` and `CLAUDE.md` are written for Snipe-IT's contributors and were not opened. No trademark policy file was found in the folder (the project's website was not checked: not verified).
