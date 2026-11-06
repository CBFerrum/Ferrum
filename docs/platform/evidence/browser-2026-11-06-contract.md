# A browser on a running server, 6 November 2026: a building contract, from estimate to retention

Construction (FERRUM-INDUSTRY-007) had run only inside its own suite, through the API. This walk
took a contract from its estimate to its retention through the pages, on the walk server: a debug
build of Ferrum PR 89 on one machine, with its own database (`ferrum_walk`), the shell from
Ferrum-Frontend a76295e and Construction 0.1.6 as pinned. The workspace's clock is UTC and the
browser's was India's. It ran from 19:59 to 20:15 UTC.

One person, the workspace's administrator, worked as the contracts manager and the site manager.

## What was walked

| Step | What it showed |
|---|---|
| Construction installed | From the App Manager's Store. Its home, the contracts page: "Contracts" and "New estimate", with nothing saying claims need the books named first |
| Setting up | Construction Settings (the company, a works item claims are invoiced as, 5% retention) and an estimate for Harbour Cafe, through the API from the signed-in page: EST-2026-00001, Kitchen extension, groundworks 8,000, 40 m² of brickwork at 120 and roofing 6,000, with 10% markup: 18,800 and 20,680 |
| The estimate | On its page: Mark It Sent, then Accept It. CON-2026-00001, worth 20,680, its lines the estimate's with the markup spread across them (8,800, 5,280 and 6,600), holding the settings' 5% |
| A variation | VAR-2026-00001, an extra roof window at 1,500, on its page: Submit It, then Approve It, decided that day by the administrator. It joined the contract as a line of its own, V-VAR-2026-00001, and the contract was worth 22,180 |
| A claim | On the Progress Claims page, opened with the contract and the day in its address, the claim filled itself with the contract's four lines. Groundworks 100% and brickwork 50%: PC-2026-00001, 11,440 to date, 572 retention held, 10,868 due. Certify It made a draft sales invoice in ERP to Harbour Cafe for 10,868, "Progress claim PC-2026-00001, work to 2026-11-06" |
| Refused | Through the API: a later claim putting groundworks back to 90% ("line 1 was certified at 100%: work certified is not taken back"), and completing the contract before it was certified in full ("certified 11440 of 22180: certify the whole contract before completing it") |
| The rest | A claim with every line at 100% was due 10,203, certified and invoiced. Completing the contract invoiced the retention held, 1,109, "Retention released, CON-2026-00001", and left none held |
| The contracts page | "Worth 22180 · certified 100% · held 0", Complete |

## What it found, and what changed

| Finding | Now |
|---|---|
| The contracts page wrote amounts as stored ("Worth 22180", "held 572") | Construction 0.1.7 (ferrum-app-construction PR 1): money, grouped and to the cent |
| An estimate sent and not yet answered was on no page but its list | The contracts page lists the estimates waiting for an answer |
| Nothing said that claims need the books named | Before Construction Settings exist, the page says so and links to them; with nothing yet, it says an accepted estimate becomes a contract |
| Mark It Sent, Accept It, Submit It, Approve It and Certify It said only "done" | Ferrum PR 89: an action that says nothing is described from its answer ("Now Certified; made a Sales Invoice") |

## What it did not cover

- Separate people as site manager and contracts manager: the walk ran as the administrator. The suite tests that a site manager may measure a claim but not certify it, nor approve a variation.
- A rejected variation, one that would leave the contract worth less than certified, and the site diary. The suite tests each.
