# A browser on a running server, 6 November 2026: an online order, from the storefront to its refund

Online order to return (FERRUM-JOURNEY-006) had run only inside Commerce's suite, through the API.
This walk took it through the pages, on the walk server: a debug build of Ferrum PR 85 (one clock,
the image walk's fixes) on one machine, with its own database (`ferrum_walk`), the shell from
Ferrum-Frontend 24c3951 and Commerce 0.7.7 as pinned. The workspace's clock is UTC; the browser's
was India's. From 07:32 to 07:50 UTC.

Two people: the workspace's administrator, as the shop's manager at the desk, and Mila Weber, a
shopper with no account, on the store's own host in a browser with no session there.

## What was walked

| Step | What it showed |
|---|---|
| Commerce installed | From the App Manager's Store: "Install Commerce v0.7.7", built into the server, with the 27 entities it adds; installed |
| The shop | An item (Walk mug, MUG-01) made on its page; its price (€12 on Standard Selling), a shipping service item, 20 mugs received into Finished Goods, a fixed €4.90 delivery and the store's settings (Walk Shop, guest checkout, Germany, payment by bank transfer with its instructions) made through the API from the signed-in page; the mug published on the store from its Website Item page |
| The store's host | A domain for "Store's public site" added in Administration, Domains; the walk server cannot be reached on port 80 for its check, so its row was marked verified in the database, and then named `walk-shop.localhost` (the browser pane runs no scripts on a nip.io name). The store's home, an item page with "In stock", the cart and checkout, served there with no session |
| Shopping | Two mugs added ("Added to your cart"); the cart: €24.00, delivery (Standard delivery, 3-5 days) €4.90, total €28.90; checkout with an e-mail, a name and an address in Berlin, billed to the same address; delivery and payment: the bank transfer's instructions; "Place order · EUR 28.90" |
| Waiting for money | The shopper's page: "Waiting for payment, Order SO-2026-00002", the steps, the instructions, the lines and a link only for her; at the desk, To pay: "Bank transfer: mark it paid when the money arrives" |
| Paid | Mark paid: "€28.90 received by Manual. The payment goes into the books"; "Payment recorded"; the order moved to To ship |
| Shipped | Ship: DHL, its parcel number and tracking link, telling the shopper and making the invoice at once; DN-2026-00002 and SINV-2026-00008 (paid); the shopper's page: "Shipped on 6 November", "DHL · JJD0001234567" and "Track your parcel" |
| Returned and refunded | "Send money back…": one mug coming back and the delivery charge, as damaged (which took "Back on the shelf" off), with a note: €16.90 through the bank transfer, which "has no gateway to send money: you hand it back yourself". REFUND-2026-00001 waited for a decision; "I refunded it", saying how and its reference: credit note SINV-2026-00009 for −€16.90, the order "Partly Refunded" and closed, every step in What happened; the shopper's page: "EUR 16.90 refunded" |

## What it found, and what changed

| Finding | Now |
|---|---|
| The shopper's page said "Delivered" the moment the parcel left: an order shipped and invoiced at once is Completed in ERP, and nothing tells the store a parcel arrived | Commerce 0.7.8: it reads "Shipped" |
| "Tell the shopper" told nobody: Commerce sent only events (`commerce.order_placed`, `_shipped`, `_refunded`) "for a mailer or webhook", and nothing in a workspace sends them; no mail to the shopper was queued for the order, its shipping or its refund | Commerce 0.7.8: the shopper is e-mailed through the server's mailer: the order with its link and how to pay, the shipping when the desk says to tell them, money sent back when the refund says so (`returns_and_refunds_end_to_end` asserts each, and the refund not to be told) |
| The store's own host could not be given on this machine: a name under the server's base is refused, and the check fetches port 80 | Ferrum PR 88: on a server whose base domain is `localhost`, a `*.localhost` name is accepted and works at once |
| Admin, Domains offered 26 apps' "public sites", some under another app's title ("PDF's public site" for Drive) | Ferrum PR 85 and Ferrum-Frontend PR 41: only an app whose site handler serves pages (`Site::serves_pages`) is offered or accepted; Website 0.1.3 and Commerce 0.7.8 say theirs do |

## The fixes, checked on their build

On the walk server built from Ferrum PR 88 (Commerce 0.7.8, Website 0.1.3, the shell at
Ferrum-Frontend 3d36821), from 08:16 to 08:21 UTC:

| Check | What it showed |
|---|---|
| Domains | "Serves" offered This workspace, The portal and "Commerce's public site", the only app here whose handler serves pages (it offered 26 before); `walk-shop2.localhost` added in the form for it read "Working" at once, with no row made by hand |
| A second order | On `walk-shop2.localhost`, one mug, a guest's checkout as Jonas Berg, SO-2026-00003. Queued and sent at once: "Your order SO-2026-00003 at Walk Shop", with the amount, the bank transfer's instructions and the order's link |
| Shipped | Marked paid, then shipped with DHL and a parcel number, telling the shopper and invoicing at once: "Your order SO-2026-00003 has shipped" ("DHL has it, parcel JJD0007654321", and the link); the shopper's page read "Shipped", not "Delivered" |
| Refunded | The mug sent back (changed their mind, back on the shelf) and refunded by hand: "EUR 12.00 refunded for order SO-2026-00003" |

Left: after a `*.localhost` domain is added, the form's message still says it "starts working once
checked", though it already works.

## What it does not show

- Paid and refunded by hand (bank transfer): no payment provider (BLOCK-PAYMENT-KEYS).
- The store's host stood in for DNS by a row made in the database, which Ferrum PR 88 makes
  unnecessary on a developer's machine.
- The shop's master data (price, stock, delivery, settings) was made through the API, not their
  pages.
