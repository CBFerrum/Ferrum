# A browser on a running server, 6 November 2026: a restaurant's evening

Restaurants (FERRUM-INDUSTRY-002) had run only inside Restaurant's suite, through the API. This walk
took an evening through the pages, on the walk server: a debug build of Ferrum PR 89 on one
machine, with its own database (`ferrum_walk`), the shell from Ferrum-Frontend b39fffb and Restaurant
0.1.7 as pinned. The workspace's clock is UTC and the browser's was India's. It ran from 09:25 to
09:50 UTC.

One person, the workspace's administrator, worked as the manager, a server and the kitchen.

## What was walked

| Step | What it showed |
|---|---|
| Restaurant installed | From the App Manager's Store: "Install Restaurant v0.1.7", built into the server, with the nine entities it adds. Its home showed three bare headings: "The floor", "The kitchen", "Takeaway and delivery (0)" |
| Setting up | Three dishes as ERP items, and a walk-in customer, made through the API from the signed-in page. On Restaurant's own pages: an outlet, Walk Trattoria (company, walk-in customer); a menu, Dinner, with Margherita at 11 for the kitchen, Lemonade at 4 for the bar and Tiramisu at 6.50 for pastry, each picked as an item and named by hand; the outlet's active menu; Table 1 (4 seats) |
| The floor | Both tables Free. Table 1 opened a new order with nothing filled in: the outlet and the table were picked again by hand |
| Table 1's order | Three guests, two Margheritas and two Lemonades, typed as dish names: ORD-2026-000001, 30.00, each line priced from the menu with its station |
| To the kitchen | "Send to the Kitchen: done". Two tickets, Kitchen and Bar, each "For" "Table Table 1"; the kitchen's page showed each with its dishes and its next step. The bar's went through Start it, Ready and Served, and the order's Lemonade line followed it to Served |
| A sent line | Changing the pizzas' quantity was refused on save: "Margherita went to the kitchen: void it and order again". The line was voided instead, and one Margherita and three Tiramisus ordered: 38.50, the voided line not charged. Sent again: new tickets for the kitchen and pastry |
| The kitchen after the void | Still "Preparing · 2 × Margherita" for the voided line, beside "Queued · 1 × Margherita": the kitchen would have made three pizzas |
| The bill | Bill It, confirmed: a draft sales invoice in ERP to Walk-in guests for 38.50, the lemonades, one Margherita and the tiramisus, without the voided pizzas. The order Billed, Table 1 free |
| A delivery | Opened with its type and outlet in the address (the kit fills a new record from it). Saved without an address: "a delivery needs an address and a phone". With them, ORD-2026-000002 for a "margherita" typed in lower case, priced at 11 and kept lower case. Out for Delivery went through while the dish had not gone to the kitchen; then Delivered, and billed |
| Reservations | Table 2 for Ana Costa, two people, 19:00 to 21:00 on the workspace's clock. Refused, each saying why: 20:00 to 22:00 ("is held for Ana Costa then"), a party of three ("the table seats 2"), and an end before the start ("is after it starts") |

## What it found, and what changed

Restaurant 0.2.0 (ferrum-app-restaurant PR 2) fixes each of these:

| Finding | Now |
|---|---|
| A line voided after it went to the kitchen stayed on its ticket, so the kitchen would still make it | It is struck off its ticket, shown crossed out as voided; a ticket left with nothing to make is Voided and leaves the kitchen's page |
| A line's state could be set by hand: back to New would send it again, Served would skip the kitchen | Only the kitchen moves a line on; a person voids it |
| A delivery went out before its food had gone to the kitchen | It goes out once each dish is ready or served |
| A free table opened an order with nothing filled in; nothing on the floor started a takeaway or a delivery; an empty restaurant said nothing | A free table opens a new order for that table and its outlet; New takeaway and New delivery, at the outlet when there is one; an empty floor says to add an outlet, then tables |
| Actions said only "done" | Each says what it did ("Sent to Bar, Grill and Kitchen, for Table 1", what it billed, a ticket's step) |
| Tickets read "Table Table 1"; an order was named by its type, "Dine In", wherever it was linked; a dish typed in lower case kept that case on the ticket and the bill | "Table 1"; an order is named by its number and found by its guest's name or phone; dishes are written as the menu writes them |
| On the order's page a sent line's dish and quantity looked editable, the server refusing the change on save, and a dine-in order showed the delivery fields | A sent line is read-only there; a table's fields show for dine-in, a delivery's for deliveries |
| "is after it starts", said of a reservation's end | "must be after it starts" |

## The fixes, checked on their build

On Ferrum PR 89 at c57ae78, with Restaurant 0.2.0, at 21:00 UTC: an order for Table 2 of
"lemonade" in lower case and a tiramisu was written "Lemonade" as the menu writes it, and sent with
"Sent to Bar and Pastry, for Table 2". The lemonades voided, the bar's ticket was Voided and left
the kitchen's page, the pastry's stayed "Table 2 · 1 × Tiramisu". Setting the tiramisu to Served by
hand was refused ("Tiramisu is sent: the kitchen moves it on, and Void takes it off"). Billing said
"Billed 6.5: a draft invoice in ERP; the table is free". The walk's own ticket from before the
fix, "Table Table 1", still shows its two voided pizzas: it was made before the fix.

## What it did not cover

- Separate people as server and kitchen: the walk ran as the administrator. The suite tests who may do what.
- Cancelling an order, and a takeaway. The suite tests both.
- A dish's name typed by hand on the menu after picking its item: the server's copying from a link would overwrite a name someone chose, so it stays typed.
