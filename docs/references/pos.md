# Point of Sale: references

**Compared with:** yrestom/POS-Awesome at `a86fd291ab18` (`referance/pos/posawesome`, GPL-3.0) and Ferrum Point of Sale 0.2.3 at `699f13d0a8a4` (`ferrum-app-pos`, branch `privacy/declarations`, working tree clean). **Date:** 2026-11-01

Paths: POS Awesome paths start at `referance/pos/posawesome/posawesome/`; `pos/X.vue` means `public/js/posapp/components/pos/X.vue` and `posapp.py` means `posawesome/api/posapp.py`. Ferrum paths are inside `ferrum-app-pos` unless they start with `ferrum/` (the platform at `5ebc51a9960c`) or `erp-platform/` (the ERP app at `993017fc502a`, clean). `referance/_inventory/` holds no route list for these apps, so absences below come from directory listings and grep.

## What Ferrum has

- **Server rules (20 entities, `packages/pos/entities/pos.yaml`):** registers with cashier rights (discount cap, price change, allowed users, negative stock), sessions with an opening float, sales and returns against the original sale, split tenders with change and a write-off limit, cash in and out, closing with a count by denomination, loyalty tiers with a points ledger. Code in `src/invoice.rs`, `src/logic.rs`, `src/session.rs`, `src/closing.rs`, `src/loyalty.rs`; a whole day is exercised in `tests/pos_it.rs`.
- **Books:** stock leaves on each sale; closing consolidates into one invoice per customer, credit notes and one settlement that equals the register's totals (`src/closing.rs`, "Consolidation" in `packages/pos/README.md`). Numbers are gapless per register prefix (`PosInvoice` in `packages/pos/entities/pos.yaml`).
- **Reports:** X and Z summary, daily sales, payment reconciliation, top items, hourly, returns, and a one-query item lookup (`src/reports.rs`).
- **The till (`ui/src/Register.tsx`, 801 lines):** register picker, opening float, scan box with pack barcodes, name-and-code item tiles, a cart with plus, minus and quantity, customer picker, split-tender dialog with quick cash and change, receipt print on 80 mm, 58 mm or A4, an outbox for unsent sales. It sends item and quantity only (`ui/src/lib/pos.ts:59`).
- **Offline:** a service worker (`ui/public/sw.js`), a price book per register (`src/site.rs`, `ui/src/lib/offline.ts`), pricing with the server's own engine in WebAssembly, sales queued with an idempotency key and the cashier's name.
- **The gap is in the till, not the server:** returns, typed discounts, price changes, loyalty, serials, batches, cash in and out and the closing count cannot be entered at the till (grep of `ui/src` finds none; it only shows a discount that pricing rules gave, `ui/src/Register.tsx:577`). They open as generic records or through the API.

## What the references do that Ferrum does not

Most valuable first. Size: S is days, M one to two weeks, L several weeks or a change to a shared package.

| Capability | Where in the reference | Ferrum today | Worth building | Size |
|---|---|---|---|---|
| Returns at the till: find the sale, pick lines, refund | `pos/Returns.vue`; the Return button in `pos/Invoice.vue`; `posapp.py:1213` (`search_invoices_for_return`) | Server done and tested: returns against the original sale with refund rules per tender (`src/invoice.rs`, `src/logic.rs`, step 6 of `packages/pos/README.md`). The till has no return path (`ui/src/Register.tsx`) | must | M |
| Hold and resume a sale | `pos/Drafts.vue`; `posapp.py:866` (`get_draft_invoices`) | A held sale is a draft by design (step 5 of `packages/pos/README.md`); the till can neither park one nor list drafts | must | S |
| Line sheet: discount, price change, unit, serial, batch, note; discount on the whole sale | `pos/Invoice.vue` (expanded row, additional discount); the `posa_allow_user_to_edit_*` switches in `fixtures/custom_field.json` | The server enforces rights and caps (`src/invoice.rs:280-311`; profile `allow_discount_change`, `max_discount`, `allow_rate_change`); the till sends item and quantity only (`ui/src/lib/pos.ts:59`) | must | M |
| Pay dialog: redeem points with the balance shown, card or UPI reference, credit sale with due date | `pos/Payments.vue` | The server takes points, `reference_no`, a Credit tender and a write-off limit (`PosInvoicePayment`, `src/invoice.rs:157`); `PosInvoice` has no due-date field, and where a credit sale's due date comes from is not verified; the dialog has one amount per method (`ui/src/Register.tsx:699-761`) | must | M |
| Store credit: refund a return to the customer's credit and pay with it | `pos/Payments.vue` (use customer credit); `posapp.py:814` (`get_available_credit`); feature list in `referance/pos/posawesome/README.md` | None: "Gift cards / store credit are not included" (Limits in `packages/pos/README.md`); a return refunds only through the tenders the register allows (`src/logic.rs:97`) | should | M |
| Close the till in the till: expected against counted per method | `pos/ClosingDialog.vue` | "Close register" opens the generic closing form (`ui/src/Register.tsx:204-208`); the server has the count by denomination and the X and Z report (`PosClosingDenomination`, `src/reports.rs`) | should | S |
| Item grid with picture, price and stock, group chips, list or card, quantity before adding | `pos/ItemsSelector.vue` | Tiles show name and code (`ui/src/Register.tsx:515-528`); `Item` has no picture field (`ferrum/packages/products/entities/products.yaml`); `pos_item_lookup` returns price and stock (`src/reports.rs:429`) and the till does not call it | should | M |
| Offers by item, group, brand or whole sale; free item; thresholds | `pos/PosOffers.vue`; `posawesome/doctype/pos_offer/pos_offer.json` | `PricingRule`: percent or rate per item or group, priority, a CEL condition (`erp-platform/packages/selling/entities/selling.yaml:1`); no free item, no whole-sale offer | should | L |
| Customer quick create and edit with address | `pos/Customer.vue`, `pos/UpdateCustomer.vue`, `pos/NewAddress.vue` | `LinkInput` picks an existing customer (`ui/src/Register.tsx:535`); creating one at the till: not verified | should | S |
| Serial and batch search and pick | `pos/ItemsSelector.vue` (search by serial or batch); `pos/Invoice.vue` (batch with expiry and stock) | Fields and checks exist on sale lines (`src/invoice.rs:482`); no picker; batches belong to an optional stock add-on (`erp-platform/addons/stock-ops/packages/stock_ops/entities/picking.yaml`) | should | M |
| Keyboard shortcuts for pay, delete line, discount | `pos/Invoice.vue:2059-2085`; `pos/Payments.vue:966` | Enter in the scan box only | should | S |
| Coupons, gift cards, referral codes | `pos/PosCoupons.vue`; doctypes `pos_coupon` and `referral_code` under `posawesome/doctype/` | None | later | M |
| Weighed items by scale barcode | `pos/ItemsSelector.vue` (`posa_scale_barcode_start`) | Pack barcodes only (`ItemBarcode.qty` in `packages/pos/entities/pos.yaml`) | later | M |
| Payment request to a phone (M-Pesa); card terminals | `pos/Mpesa-Payments.vue`; `posawesome/api/m_pesa.py` | None; devices wait on FERRUM-POS-003 (`ferrum/docs/registry/blockers.yaml:13`) | later | L |
| Counter extras: sales order, delivery charges, sales person, pay old invoices, template items with variants | `pos/SalesOrders.vue`; `posawesome/doctype/delivery_charges`; `public/js/posapp/components/payments/Pay.vue`; `pos/Variants.vue` | None in POS (ERP has orders and payments; `Item` has no variants) | later | M |

## Screens to hand-craft

1. **Return desk.**
   - *Purpose and layout:* refund goods without leaving the till. A dialog over the till: a search (receipt number, customer, scan the receipt), the matching sales, then the chosen sale's lines with sold, already returned and a return stepper; beside them a refund panel listing the tenders the register allows.
   - *Interactions:* POS Awesome picks one invoice from a table and loads it as negative lines, with quantity and total checks in the browser (`pos/Returns.vue`, `pos/Invoice.vue:1615-1651`). Here the server's rules are shown before they refuse: what the customer still owes is cancelled first, cash and card only up to what was taken, points never as cash (`src/logic.rs:97`); tenders that cannot be used are greyed with the reason; a reason field; reprint of the return receipt.
   - *Data:* `PosInvoice` by number, customer and date, `original_line`, `allow_in_returns` on the profile and the `pos_returns` report exist. A return needs the server (the original sale is read), so offline the button says so.
2. **Line sheet and discounts.**
   - *Purpose and layout:* change one line without leaving the cart. Tapping a line opens a side sheet: quantity, unit, price when the register allows it, discount with its cap shown, serial entry, batch list with expiry and stock, a note. A sale-level discount sits under the total with the items' discounts summed (`pos/Invoice.vue`).
   - *Interactions:* the cap is shown before typing, not as a 422 after (`src/invoice.rs:280-311`); a manager lifts it with a PIN (our addition); shortcuts open the first line and focus the discount (`pos/Invoice.vue:2059-2085`).
   - *Data:* `PosInvoiceItem` carries `discount_percent`, `manual_rate`, `serial_no` and `batch_no`, and the profile holds the caps; the till drops them (`ui/src/lib/pos.ts:59`). Batch lists need the stock add-on (not verified on every install).
3. **Park, resume and recent sales.**
   - *Purpose and layout:* two buttons beside the cart. Hold sets the cart aside, returns focus to the scan box and shows a count; Held lists customer, time, lines and amount (`pos/Drafts.vue`); Recent lists today's sales for reprint or return.
   - *Interactions:* resuming asks before it replaces a cart that has lines; offline, held sales live on the device beside the outbox (`ui/src/lib/pos.ts`) because a draft needs the server; any receipt reprints with the existing builder (`receiptHtml`, `ui/src/lib/pos.ts`).
   - *Data:* draft `PosInvoice` per session and cashier exists; device-held sales are new.
4. **Pay dialog.**
   - *Purpose and layout:* the existing dialog (`ui/src/Register.tsx:699-761`) with the loyalty balance and "can redeem up to", store credit once it exists, a reference box on bank methods, a credit sale with due date, exact and quick-cash buttons and change in large type (`pos/Payments.vue`).
   - *Interactions:* Ctrl+X completes (`pos/Payments.vue:966`); the next method is pre-filled with what is still due; a shortfall inside the write-off limit says so; points and credit need a named customer, as the server already insists (`src/invoice.rs:157`).
   - *Data:* the points ledger and its `available` query exist (`src/loyalty.rs:79`) but the till has no way to read a balance (no endpoint found; not verified further); the rest exists.
5. **Close the till.**
   - *Purpose and layout:* the end of a shift on one page: per method opening, expected, counted and difference; a cash counter with plus and minus per note and coin that fills the counted cash; the shift's cash in and out; a Z report preview. POS Awesome's dialog only edits the counted amount (`pos/ClosingDialog.vue`).
   - *Interactions:* the cashier saves a draft count and a manager submits (step 9 of `packages/pos/README.md`); differences are coloured; Print Z.
   - *Data:* `PosClosingEntry`, `PosClosingDenomination`, `PosCashMovement` and `pos_sales_summary` (X and Z) all exist.

## What Ferrum already does better

- Offline selling: a service worker, a price book on the device, the server's own pricing engine in WebAssembly and an outbox that keeps who sold and re-prices on arrival (`ui/public/sw.js`, `ui/src/lib/offline.ts`, `ui/src/Register.tsx`). Grep finds no service worker, IndexedDB or offline queue in POS Awesome; it keeps the item list in localStorage (`pos/ItemsSelector.vue`).
- A sale is created and submitted with a client id and an idempotency key, so a retry never sells twice (`ui/src/Register.tsx:332,478`, step 5 of `packages/pos/README.md`); grep finds no idempotency handling in POS Awesome, which can submit in a background job (`posapp.py:797`).
- Cash control: an opening float per method, cash in and out documents, a count by denomination, X and Z reports (`src/session.rs`, `src/reports.rs`). POS Awesome has opening and closing shifts with expected against counted (`pos/ClosingDialog.vue`) and, by grep, none of the other three.
- Closing: one invoice per customer, credit notes per original invoice and one settlement that equals the register's totals, with cash differences booked and a mismatch refusing the closing (`src/closing.rs`); POS Awesome submits a full Sales Invoice for every sale (`posapp.py:555`).

## Licence notes

- POS Awesome is GPL-3.0 (`referance/pos/posawesome/license.txt`; `hooks.py` sets the app licence to GPLv3; `INDEX.md` agrees). Ferrum Point of Sale is AGPL-3.0-only (`app.yaml`). GPLv3 section 13 lets a GPL work be combined with an AGPL one, but `referance/README.md` says to copy nothing, so this page records behaviour and file paths only; take it as GPL-3.0-only until a licence review says otherwise.
- Its Vue and Vuetify code and `posawesome/translations/pt.csv` were read for behaviour and are not to be reused; this copy holds no screenshots (`referance/fetch.sh` strips media).
