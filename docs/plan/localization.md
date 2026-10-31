# ERP in every country

ERP should be ready for every country's taxes and rules: India, South East Asia, Europe, the United
States and the rest (the user, 31 October 2026). Stage 3 of [erp-repo.md](erp-repo.md) built the
foundation: ERP's engine holds no country's rules, a country is an add-on on one seam
(`ferrum_app_erp::localization`: taxes inside `pricing::calculate`, checks before submit, a new
company's accounts and templates), and India (`addons/india`, crate `ferrum-india`) is the first.

This plan covers what comes next: the shared parts every country after India needs (L0), then the
add-ons in an order, with what each must do and when it is done. L0 is built (31 October 2026, see
its section); the add-ons are built phase by phase on a go-ahead.

Dates are as reported on 31 October 2026 (sources at the end). Tax law moves often, and several
sources disagree, marked *(check)*. Each add-on confirms its dates against the authority's own
site (listed per country) when it is built.

## How a country add-on is built

Every add-on follows India's shape, in ERP's repository:

| Part | How | India today |
|---|---|---|
| Crate and app | `addons/<code>` (ISO 3166 code: `de`, `gb`, `us`, `sg`; `eu` for the rules EU members share), an app with `parent: erp`, `requires: {erp: …}` (EU members also `eu`) | `addons/india`, app `india` |
| Fields | its package's fields on ERP's and the shared entities, prefixed with its code | `in_gstin`, `in_place_of_supply` |
| Taxes | `Localization::determine`: which taxes apply (place of supply, category, reverse charge), or a tax service's answer | GST template by place of supply |
| Checks | `Localization::check` before submit; document hooks for the rest | HSN/SAC codes |
| Company set-up | `Localization::setup_company`: accounts, templates, numbering | GST accounts and templates |
| E-invoicing and reporting | a hook on submit and cancel queues the work in the outbox; an event sink talks to the authority or network with retries and an idempotency key; the answer (IDs, QR code, signature) lands in its fields; credentials are sealed secret fields | the IRP connector |
| Returns | `ReportSource` reports plus the files an authority takes (XML, CSV, JSON) | — |
| Prints | its package's print formats with the legal content | sales invoice |
| Tests | an end-to-end scenario with a fake authority, like India's fake IRP; the authority's sandbox where it has one | `tests/erp_it.rs` |

Each runs only where its package is installed and only for companies of its country, so one
workspace can keep companies in several countries.

Rules every add-on keeps:

* A document never waits on a network call, except where the law requires clearance before an
  invoice exists (Italy, Poland, Saudi Arabia, Mexico, Brazil). Then submitting refuses with a
  clear reason and a retry, and nothing is half-posted.
* An authority's reference (IRN, KSeF number, UUID) is stored once and never changes; cancellation
  follows the authority's own windows and rules.
* Credentials are per company, sealed, and never in logs. Calls go through the kernel's outbound
  rules (https, pinned hosts, no private addresses).
* Sandbox and production endpoints are a setting per company, defaulting to the sandbox.

## L0. The shared parts, before any second country

**Built (31 October 2026):** Ferrum 1b532f4 and b5b2d3f, Ferrum-Frontend 9ecf5bf, ERP 7a2a70c and
4aae57a (ERP 0.3.0, India 0.2.0), Commerce 6c74dbd, Point of Sale 0.2.3; pinned by the Ferrum commit
that marks this. What was built differs from the table in one place: a country stays the name
people type, and a read-only `country_code` (ISO 3166-1 alpha-2, `ferrum_domain::countries`, with the
other names people type) follows it on every save, so nothing that reads or writes `country` changes;
the upgrade step `country-codes` fills existing companies' and addresses' codes and logs names it
does not know. In short:

* `ferrum-calc`: charge type `on_item_quantity` (rate × the line's `tax_qty`, its stock quantity),
  `reverse_charge` rows (reported, never in a total), `tax_rounding` per document or per line; every
  input has a default, so callers name only what they set and older JSON keeps working.
* ERP: categories and exemption reasons on tax rows and items' tax rates, a stored tax breakdown on
  invoices (BG-23), reverse-charge postings (purchases: input and owed; sales: nothing), the company's
  Tax Rounding, the party's address and a ship-to on orders, deliveries and invoices
  (`localization::ship_to`).
* India: reads the code; reverse-charge purchases with RCM templates and accounts; e-invoices mark
  reverse charge and report a cess per unit.
* Point of Sale and the browser engine price offline as the server does, per-unit taxes included.

| Need | Change | Where |
|---|---|---|
| Countries as codes | a `Country` master (ISO 3166-1 alpha-2, seeded) that `country` on Company and Address links to, instead of free text. An upgrade step converts names it recognises ("India", "Bharat" → IN) and lists the rest for an administrator. India's company check reads the code. | core package (Ferrum), India's add-on |
| Place of supply from the ship-to | documents carry a shipping address whose country (and state, region or ZIP) the rules read; a billing-only document uses the billing address | ERP's selling and buying documents |
| Tax categories and exemption reasons | `category` (standard, reduced, zero, exempt, reverse charge, out of scope) and `exemption_reason` (EN 16931's VATEX codes) on tax rows and item tax rates; the calculation's breakdown per category and rate (EN 16931 BG-23) | ERP's accounting package, `ferrum-calc` |
| Per-quantity taxes | a charge type "per unit" (rate × stock quantity): excise, eco and packaging fees, some US local taxes | `ferrum-calc`, accounting package |
| Reverse charge | a tax that is posted to input and output tax but never charged to the party (the supplier's payable stays the net) | `ferrum-calc`, ERP's trade postings |
| Tax rounding | per document (each category's tax on the sum of its lines, EN 16931) or per line, a setting on the company or template | `ferrum-calc` |
| Withholding on payments | already: deducted tax rows and payment deductions; each country adds its certificates and returns | — |
| Gapless numbering per fiscal year | already: the kernel's numbering | — |

Done when: a test company outside India can be set up with country codes; an invoice with standard,
reduced and exempt lines shows the right breakdown per category; a reverse-charge purchase posts
input and output tax with nothing extra payable; a per-unit levy calculates; and ERP's, India's,
POS's, CRM's and Commerce's tests still pass.

## L1. The European Union (`eu`): the rules members share

**Built (31 October 2026):** ERP 34d813d (ERP 0.4.0, the European Union add-on 0.1.0, crate
`ferrum-eu` in `addons/eu`), compiled into Ferrum by the commit that marks this. What it does:

* Which VAT a sale or purchase of a company in a member state takes, line by line: at home; goods
  to businesses elsewhere in the EU as intra-Community supplies (Art. 138, category K) and services
  reverse-charged (Art. 196, AE); goods and digital services to consumers elsewhere at the
  destination's rates through the OSS when the company says so; exports (Art. 146) and services
  outside the EU (G); acquisitions and services from abroad reverse-charged by the company. Mixed
  OSS and home-taxed lines are refused (they belong on separate invoices).
* Invoices carry the legal mentions, the customer's VAT number as it was and VIES's consultation
  number; VAT numbers are checked for their country's form and in VIES (REST, through the
  outbox); a supply without VAT to a business waits for a VIES-confirmed number.
* Rates are data (`EuVatRate`): standard rates seeded from TEDB with the day each took effect,
  everything else fetched from TEDB's VAT Retrieval Service; a reduced-rate item carries its rate
  per country, suggested from its CN code, never guessed.
* EN 16931 UBL (Peppol BIS Billing 3.0 with both Peppol IDs), checked against the standard's
  rules before it is written; sending hands it to an access point connector through the outbox
  (`eu.peppol.send_requested`), which records delivery or failure.
* The EC sales list and the OSS return (in euro at the ECB rate of the period's end).

Not yet: the connector to a particular Peppol access point (it needs the user's provider and
account; any provider's connector answers the outbox topic), Intrastat, postponed import VAT, and
ViDA's later steps (2028, 2030), which come with their dates.

* VAT at standard, reduced and zero rates per member state (rates as data, dated, not code).
* Between businesses across borders: zero-rated with reverse charge on the buyer, the buyer's VAT
  number required and checked in VIES (the check and its consultation number kept on the
  document); goods to another member state on the EC sales list.
* To consumers in another member state: the destination's rate above the €10,000 EU-wide
  threshold, through OSS (or IOSS for low-value imports), with the OSS return.
* Exports zero-rated with proof of export; imports with postponed accounting where the member
  state allows it; Intrastat above each member state's thresholds.
* E-invoices to EN 16931 (UBL 2.1 and CII) and Peppol BIS Billing 3.0, sent and received through a
  certified Peppol access point (a provider at first, not one Ferrum runs).
* ViDA, adopted 11 March 2025: member states may require domestic e-invoicing without asking
  since April 2025; business invoices between member states become e-invoices with digital
  reporting from 1 July 2030; the single VAT registration and wider OSS from 1 July 2028; systems
  that existed before 2024 have until 2035 to align.

Done when: an EU company's domestic, intra-EU B2B, intra-EU B2C (OSS) and export sales take the
right taxes and texts; VIES answers are kept; the EC sales list and OSS return come out right; an
invoice validates against EN 16931's rules and goes to a Peppol test access point.

## L2. EU member states with mandates (one add-on each, on `eu`)

| Country | What it requires | Model | Authority |
|---|---|---|---|
| Germany | receiving e-invoices since 2025; issuing from January 2027 (turnover above €800,000) and January 2028 (everyone); XRechnung or ZUGFeRD; GoBD archiving | exchange, no clearance | BMF |
| France | receiving for every business and issuing for large and mid-sized ones since 1 September 2026; issuing for SMEs and micro-businesses from 1 September 2027; e-reporting of B2C and international sales; through an accredited platform (plateforme agréée) | platforms, the state directory | DGFiP |
| Italy | FatturaPA XML through SDI since 2019; preservation for ten years | clearance | Agenzia delle Entrate |
| Poland | KSeF: large taxpayers since 1 February 2026, the rest since 1 April 2026, the smallest from 1 January 2027; FA(3) XML; the KSeF number on payments between VAT payers from 2027 | clearance | Ministry of Finance |
| Belgium | Peppol between Belgian businesses since 1 January 2026; e-reporting from 2028 | Peppol | SPF Finances |
| Croatia | Fiscalization 2.0: B2B e-invoices since 1 January 2026, wider from 2027 | e-invoicing and e-reporting | Porezna uprava |
| Greece | myDATA reporting; B2B e-invoicing phased in 2026 (large businesses first, then everyone; dates differ across sources *(check)*) | reporting, certified providers | AADE |
| Spain | Verifactu (hash-chained, certified invoicing software): 1 January 2027 for companies, 1 July 2027 for others under RDL 15/2025; a further delay to 2028 is reported but not in the BOE *(check)*. B2B e-invoicing (Crea y Crece): October 2027 above €8 million turnover, October 2028 for others; the exact day differs across sources *(check)* | software control, then exchange | AEAT, BOE |
| Portugal | SAF-T (PT), ATCUD codes and QR codes on invoices, certified software | reporting, certified software | Autoridade Tributária |
| Romania | RO e-Factura between businesses since 2024; SAF-T (D406) | clearance | ANAF |
| Hungary | real-time invoice reporting (Online Számla) | reporting | NAV |

**Germany built (31 October 2026):** ERP 6f045dd (Germany 0.1.0, crate `ferrum-de` in `addons/de`, on
the EU's): XRechnung 3.0 checked against KoSIT's BR-DE rules (taken from its schematron), small
businesses (§ 19 UStG) and the domestic reverse charge (§ 13b UStG). The other member states follow.

**Belgium, Italy, Poland and Romania built (1 November 2026):** ERP 2db8112, crate `ferrum-eu-members`
(`addons/eu-members`, one app per member state on the EU's, sharing what member states add at home:
small businesses' exemptions and the domestic reverse charge with their invoice notes). Belgium:
Peppol on submit between Belgian businesses, enterprise numbers and 0208 IDs. Italy: FatturaPA
(FPR12, schema 1.2.3) with Natura codes, the virtual stamp duty and SDI's receipts, queued for the
company's SDI channel. Poland: FA(3) for KSeF with the rate codes, corrections citing KSeF numbers,
queued for the company's KSeF channel. Romania: CIUS-RO with ISO 3166-2:RO counties and Bucharest's
sectors, queued for ANAF. Not yet: the channels themselves (SDI, KSeF's API 2.0, ANAF's OAuth), Italy's
FPA12, split payment and withholding, and France, Spain, Portugal, Croatia, Greece, Hungary.

**Spain and France built (1 November 2026):** ERP 572204b and c062e2b, in `ferrum-eu-members`. Spain:
VERI*FACTU invoicing records chained by AEAT's SHA-256 fingerprint (its test vectors pass),
cancellation records, the verification QR code, records queued for the company's AEAT connection;
the recargo de equivalencia; the Canary Islands, Ceuta and Melilla outside the VAT area. France:
e-invoices to the company's plateforme agréée with both SIRENs and the PA's statuses, e-reporting of
the other sales, the reform's and the Code de commerce's mentions, SIREN, SIRET and VAT key checks.
Not yet: AEAT's and the PA's connections themselves, the SII, Facturae, the No-VERI*FACTU signed
records.

Order: by demand. Germany first (the largest market, formats only), then the clearance countries
(Italy, Poland), then France (platforms), Spain and Portugal. Done when, for each: its scenario runs
against a fake authority and the authority's sandbox, and its official validator accepts the files.

## L3. The United Kingdom (`gb`)

**Built (31 October 2026):** ERP 649d799 (the United Kingdom 0.1.0, crate `ferrum-gb` in `addons/gb`):
VAT at 20%, 5%, zero and exempt, the CIS reverse charge, exports and services abroad, imports of
services reverse-charged; VAT returns (nine boxes from the books) filed through HMRC's MTD API as
hmrc/vat-api defines it, with OAuth sign-in and refresh and the fraud prevention headers (the
browser's details when filed through `/api/app/gb/returns/{id}/file`; ERP's interface should file
through it). Not yet: postponed import VAT and the 2029 e-invoicing mandate (details due 2027-28).

* VAT at 20%, 5% and 0%, postponed import VAT, the domestic reverse charge for construction.
* Making Tax Digital for VAT: returns sent through HMRC's API from digital records.
* E-invoicing becomes mandatory for VAT invoices from 1 April 2029: exchange only, no clearance or
  real-time reporting; HMRC confirms the details between October 2027 and April 2028.

Done when: a UK company files a VAT return to HMRC's sandbox from its books, and its invoices
carry what the law asks.

## L4. The United States (`us`)

**Built (31 October 2026):** ERP 9e923ac (the United States 0.1.0, crate `ferrum-us` in `addons/us`):
sales tax by destination (origin within origin-based states) at the ZIP code's combined rate from the
states' rate tables (US Rate Table takes a state's table as published; without a ZIP code's rate,
submitting refuses where local taxes apply), only where the company is registered; exemption
certificates by state and date, marketplace facilitators, products' taxability by state; use tax
accrued as a reverse charge; sales recorded in AvaTax (createoradjust, committed, with the tax
charged as an override, AvaTax's own figure compared) and voided on cancel; the sales tax by
jurisdiction, economic nexus (the Sales Tax Institute's chart of 1 August 2026) and Form 1099
reports ($2,000 from 2026). Not yet: AvaTax as the source of rates on drafts (rates come from the
tables, so a document never waits on it), filing the states' returns, and the IRS's 1099 files.

* Sales and use tax by destination: state, county, city and district rates (thousands of
  jurisdictions), from the ship-to address (L0).
* Economic nexus per state, mostly $100,000 of sales a year; the 200-transaction test is going
  (about 16 states had dropped it by January 2026, Illinois from January 2026, Kentucky from
  August 2026) *(check per state)*. A report of sales per state against each threshold.
* Product taxability, exemption certificates per customer and state (resale, exempt
  organisations), marketplace facilitator rules, use tax on purchases.
* `determine` asks a tax service (Avalara AvaTax, Vertex, TaxJar or Stripe Tax, one behind an
  interface of the add-on's own), or reads rate tables for a single-state business; submitting a
  document commits it to the service and cancelling voids it, through the outbox with an
  idempotency key.
* 1099-NEC and 1099-MISC for suppliers, from their W-9 details (TIN).
* No national e-invoicing mandate.

Done when: a sale ships from one state to another and takes the destination's tax from a
service's sandbox (and from rate tables); exempt customers are not taxed; a cancelled invoice
voids at the service; the nexus report and 1099s come out right.

## L5. Canada (`ca`)

**Built (31 October 2026):** ERP a5283b6 (Canada 0.1.0, crate `ferrum-ca` in `addons/ca`): GST 5%,
HST by province (13% Ontario; 15% New Brunswick, Newfoundland and Labrador, Prince Edward Island;
14% Nova Scotia from 1 April 2025), PST in British Columbia (7%) and Saskatchewan (6%), RST in
Manitoba (7%) where the company is registered, QST 9.975% in Quebec; goods taxed where delivered,
services by the customer's address; zero-rated and exempt supplies, exports, input tax credits, and
the GST/HST (GST34 lines 101, 103, 106, 109) and QST returns. Not yet: NETFILE or GST/HST Internet
File transmission (the CRA's file formats need its certified-software process).

GST or HST by province, PST (British Columbia, Saskatchewan, Manitoba) and QST (Quebec), with the
place-of-supply rules, registration numbers on invoices and the returns. Done when: sales into each
province take the right taxes.

## L6. South East Asia (one add-on each)

**Built (31 October 2026):** ERP 713f2b5, crate `ferrum-countries` (`addons/countries`): one app per
country (`sg`, `my`, `id`, `th`, `ph`, `vn`) on a shared engine for VAT and GST that work the common
way (dated rates by item category on one or more tax rows, exports, recoverable or not, the reverse
charge on services from abroad, registration numbers checked, each country's return worked out
from the books). Singapore's GST F5, Malaysia's SST (sales and service tax on separate rows, not
recovered), Indonesia's PPN on 11/12, Thailand's 7% to 30 September 2027, the Philippines' 12%,
Vietnam's 10%, 5% and temporary 8%. Not yet: their e-invoicing (InvoiceNow, MyInvois, Coretax
e-Faktur, the EIS, Vietnam's coded e-invoices, e-Tax) and withholding taxes.

| Country | Taxes | E-invoicing and reporting | Authority |
|---|---|---|---|
| Singapore (`sg`) | GST 9%; GST F5 returns | InvoiceNow (Peppol, PINT-SG) with invoice data sent to IRAS: voluntary registrants since November 2025 and April 2026; new compulsory registrants and existing ones up to S$200,000 from 1 April 2028, then S$1 million (2029), S$4 million (2030) and above (1 April 2031) | IRAS, IMDA |
| Malaysia (`my`) | sales tax and service tax (SST) | MyInvois (validation by LHDN, with a UIN and QR code per invoice; consolidated invoices for consumers): above RM100 million since August 2024 down to RM1–5 million since 1 January 2026 (penalties from January 2027); businesses below RM1 million exempt. One source reports the exemption raised to RM3 million from 1 September 2026 *(check)* | LHDN (hasil.gov.my) |
| Indonesia (`id`) | PPN 12%, for non-luxury goods on a base of 11/12 (11% in effect); PPh withholding (articles 23, 4(2)); 16-digit NPWP | e-Faktur in Coretax since January 2025 (XML, transaction codes) | DJP |
| Thailand (`th`) | VAT 7%; withholding tax certificates | e-Tax Invoice and e-Receipt (voluntary, with tax incentives); e-Withholding | Revenue Department |
| Philippines (`ph`) | VAT 12%; expanded withholding (BIR 2307) | invoices transmitted to BIR's EIS for large taxpayers, e-commerce and computerised accounting systems by 31 December 2026 | BIR |
| Vietnam (`vn`) | VAT 10% (with temporary reductions) | mandatory e-invoices carrying the tax office's code, digitally signed; since June 2025 (Decree 70/2025) e-invoices from cash registers for more sellers, and adjustment or replacement instead of cancellation; a later Decree 254/2026 is reported *(check)* | General Department of Taxation |

Order: Singapore and Malaysia first (clear specifications, sandboxes), then Indonesia, Vietnam and
the Philippines, Thailand last (voluntary). Done when, for each: its scenario runs against a fake
authority and the authority's sandbox where there is one.

## L7. Elsewhere

**Built (31 October 2026):** on the same engine (ERP 713f2b5): Saudi Arabia (`sa`, ZATCA's return
boxes), the United Arab Emirates (`ae`, the VAT 201 by emirate), Australia (`au`, the BAS's GST
labels, ABN check), New Zealand (`nz`, the GST101A, IRD number check), Japan (`jp`, 10% and 8% on
rows of their own so tax is worked out once per rate per invoice, the T-number), Mexico (`mx`, IVA
on a cash basis from payments, the RFC) and China (`cn`, 13%, 9%, 6%, small-scale taxpayers, the
credit code's check). Peppol PINT e-invoices for Singapore, Australia, New Zealand, Japan and the
UAE (ERP bf03c35) and Malaysia's MyInvois connector (ERP 1484877: submission, validation,
cancellation within 72 hours) are built. Not yet: Fatoora phase 2, CFDI 4.0 with a PAC, fapiao,
Indonesia's Coretax, Vietnam's and the Philippines' e-invoices; Brazil. Japan's food rate cut from 1 April 2027 is enacted but its rate is not in
the table yet (items' own rates cover it).

| Country | Taxes | E-invoicing |
|---|---|---|
| Saudi Arabia (`sa`) | VAT 15% | Fatoora phase 2: invoices cleared (B2B) or reported (B2C) through ZATCA with a cryptographic stamp and QR code; the waves reached nearly every VAT payer by 30 June 2026 |
| United Arab Emirates (`ae`) | VAT 5% | Peppol (PINT AE) through accredited service providers: pilot since 1 July 2026; revenue of AED 50 million or more goes live 1 January 2027 (provider appointed by 30 October 2026 *(check)*), others 1 July 2027, government 1 October 2027 |
| Australia, New Zealand | GST 10%, 15%; BAS and GST returns | Peppol, voluntary between businesses |
| Japan (`jp`) | consumption tax 10% and 8% | the qualified invoice system since October 2023 (registration numbers on invoices); JP PINT over Peppol, voluntary |
| Mexico (`mx`) | IVA 16% | CFDI 4.0 stamped by an authorised provider (PAC), payment complements |
| Brazil (`br`) | ICMS, ISS, PIS/COFINS, and the reform: CBS and IBS at test rates in 2026 (0.9% and 0.1%), CBS replacing PIS/COFINS from 2027, ICMS and ISS phased out from 2029 to 2033 | NF-e, NFC-e and NFS-e authorised by the states and cities; IBS/CBS fields required on NF-e for the normal regime since August 2026 |
| China (`cn`) | VAT | fully digital fapiao |

These come by demand. Brazil and China likely through a partner's service rather than built in.

## What is not ERP's

The same pattern serves the rest:

* POS's fiscal devices and signatures are POS's add-ons: Germany's TSE (KassenSichV), France's
  NF525, Italy's RT, Austria's RKSV, Poland's fiscal printers.
* Payroll per country is HR's.
* Data protection and data residency are the platform's: regions, export, erasure under GDPR,
  India's DPDP Act and the PDPAs of Singapore and Thailand.

## Order

| Phase | What | Depends on |
|---|---|---|
| L0 | country codes, ship-to place of supply, tax categories, per-unit taxes, reverse charge, rounding | — |
| L1 | the EU's shared rules, EN 16931 and Peppol | L0 |
| L2 | Germany first, then the clearance countries, France, Spain and Portugal | L1 |
| L3 | the United Kingdom | L0 |
| L4 | the United States | L0 |
| L5 | Canada | L0 |
| L6 | Singapore and Malaysia, then Indonesia, Vietnam, the Philippines, Thailand | L0 |
| L7 | the Gulf, Australia and New Zealand, Japan, Mexico, Brazil, China, by demand | L0 |

L0 first: every later phase needs it, and it changes shared packages, so it goes alone. After it,
phases can run in parallel; each is an add-on of its own and touches nothing else.

## Sources (31 October 2026)

* EU ViDA: [Sovos, ViDA timeline](https://sovos.com/blog/vat/vida-timeline/), [Meijburg, ViDA formally adopted](https://www.meijburg.com/news/eu-proposal-vat-digital-age-package-formally-adopted), [KPMG, ViDA guide](https://kpmg.com/ch/en/insights/taxes/vat-digital-age.html), [vatcalc, ViDA](https://www.vatcalc.com/eu/eu-vat-in-the-digital-age-vida-adopted-by-ec/)
* France: [banqup, after 1 September 2026](https://www.banqup.com/resources/blog/france-s-e-invoicing-reform-where-businesses-stand-after-1-september-2026), [TJC Group, what changed in 2026](https://www.tjc-group.com/blogs/france-e-invoicing-reform-what-changed-in-2026/)
* Poland: [EY, KSeF timeline](https://www.ey.com/en_gl/technical/tax-alerts/poland-announces-new-timeline-for-mandatory-e-invoicing), [ecosio, Poland 2026](https://ecosio.com/en/blog/e-invoicing-in-poland-7-topics-you-need-to-know-in-2026/)
* Spain: [vatcalc, Verifactu delay](https://www.vatcalc.com/spain/spain-verifactu-delay-till-jan-2027-for-certified-e-invoicing/), [fiskaly, Spain 2026](https://www.fiskaly.com/blog/spain-fiscal-landscape-2026-verifactu-naticket-b2b-electronic-invoicing), [Tradeshift, Spain 2027](https://tradeshift.com/resources/compliance/spain-b2b-einvoicing-mandate-2027/)
* Germany and the UK: [ecosio, Germany](https://ecosio.com/en/blog/germany-e-invoicing-explained/), [KPMG, UK roadmap from April 2029](https://kpmg.com/us/en/taxnewsflash/news/2026/10/uk-b2b-e-invoicing-roadmap-april-2029.html), [Forbes, UK April 2029](https://www.forbes.com/sites/aleksandrabal/2025/11/29/uk-to-introduce-mandatory-e-invoicing-in-april-2029/)
* Belgium, Croatia, Greece: [Marosa, Europe overview](https://marosavat.com/resources/e-invoicing-in-europe-overview-and-dates), [Fintua, mandate watch list](https://fintua.com/blog/einvoicing-mandate-watch-list-2026/)
* Malaysia: [VATupdate, RM1 million threshold](https://www.vatupdate.com/2025/12/17/malaysia-raises-e-invoicing-exemption-threshold-to-rm1-million-cancels-final-implementation-phase/), [WS Group, MyInvois guide](https://wsco.asia/insights/e-invoice-myinvois-practical-guide/)
* Singapore: [IRAS, GST InvoiceNow requirement](https://www.iras.gov.sg/taxes/goods-services-tax-(gst)/gst-invoicenow-requirement), [Storecove, Singapore through 2031](https://www.storecove.com/blog/en/singapores-e-invoicing-mandate-how-the-peppol-network-is-shaping-gst-compliance-through-2031/)
* Philippines: [VATupdate, deadline 31 December 2026](https://www.vatupdate.com/2026/01/16/philippines-extends-e-invoicing-compliance-deadline-for-large-taxpayers-to-december-31-2026/)
* Vietnam: [KPMG, Decree 70/2025](https://assets.kpmg.com/content/dam/kpmgsites/vn/pdf/2025/04/decree70-amending-decree-123-en.pdf), [Edicom, Vietnam](https://edicomgroup.com/blog/vietnam-prepares-to-roll-out-e-invoicing-for-all)
* Indonesia: [DDTC, PPN 12% with DPP 11/12 in Coretax](https://news.ddtc.co.id/berita/nasional/1808011/ppn-12-dihitung-dengan-dpp-1112-faktur-pajaknya-sudah-via-coretax), [Ortax, DPP nilai lain in Coretax](https://ortax.org/membuat-faktur-pajak-dpp-nilai-lain-pada-aplikasi-coretax)
* Thailand and Japan: [vatcalc, Thailand e-Tax invoice](https://www.vatcalc.com/thailand/thailand-e-invoicing-e-tax-and-invoice-receipt-update/), [vatcalc, Japan JP PINT](https://www.vatcalc.com/japan/japan-jp-pint-tax-qualified-invoices-oct-2023/)
* The Gulf: [Avalara, UAE 2026](https://www.avalara.com/blog/en/europe/2026/03/uae-e-invoicing-mandate-2026-readiness-asp-pint-ae.html), [e-invoice.app, UAE provider deadline](https://www.e-invoice.app/blog/uae-einvoicing-asp-deadline-extended-october-2026)
* United States: [Avalara, states dropping the transaction threshold](https://www.avalara.com/us/en/blog/2025/06/states-eliminating-economic-nexus-transaction-thresholds.html), [Sales Tax Institute, economic nexus by state](https://www.salestaxinstitute.com/resources/economic-nexus)
* Brazil: [Fonoa, Brazil's 2026 reform](https://www.fonoa.com/resources/blog/brazil-tax-reform-e-invoicing-2026), [VATupdate, CBS and IBS deadlines](https://www.vatupdate.com/2026/09/09/brazils-2026-vat-transition-cbs-ibs-and-compliance-deadlines/)
