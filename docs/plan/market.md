# Market: Beating SAP and Everyone Else

Research date 2026-10-28. Evidence tags: *(vendor)* = the company's own claim, *(advisor)* = a consultant/reseller claim,
*(thin)* = a single or weak source. The ERP applications market was ~$146B in 2025 (+11%), and the top 10 vendors hold
only 31.9% of it ([ARTW](https://www.appsruntheworld.com/top-10-erp-software-vendors/)). India's ERP market is estimated
at $1.3–1.9B for 2025, depending on definition ([IMARC](https://www.imarcgroup.com/india-enterprise-resource-planning-market),
[Ken](https://www.kenresearch.com/industry-reports/india-enterprise-resource-planning-software-market.md)).

## 1. Competitor matrix

| Vendor | Segment | Architecture / extensibility | Pricing | Strength | Weakness |
|---|---|---|---|---|---|
| **SAP S/4HANA** (RISE private / GROW public) | Enterprise; GROW for mid-market | HANA-only; Universal Journal (ACDOCA) ([SAP](https://learning.sap.com/courses/outlining-cost-management-and-profitability-analysis/discussing-the-concept-of-the-universal-journal_f62cf151-5562-44f5-8ff6-df16fb232e94)); mandatory Material Ledger ([SAP Community](https://community.sap.com/t5/enterprise-resource-planning-blog-posts-by-members/why-is-material-ledger-mandatory-in-s-4-hana/ba-p/13379982)); Fiori Elements UI generated from CDS annotations + RAP ([SAP](https://learning.sap.com/learning-journeys/getting-started-with-creating-an-sap-fiori-elements-app-based-on-an-odata-v4-rap-service)); key-user / ABAP Cloud / BTP extensions graded "clean core" levels A–D since Aug 2025 ([SAP](https://news.sap.com/2025/08/extend-sap-s4hana-cloud-right-way-clean-clear/)) | Quote-only, FUE-based; Joule billed in AI Units ([SAP](https://learning.sap.com/courses/introducing-joule/understanding-the-commercial-model)) | Cloud ERP Suite €5.92B in Q3 2026, +29% ([SEC 6-K](https://www.sec.gov/Archives/edgar/data/0001000184/000110465926118617/tm2628113d1_ex99-1.htm)) | Horváth survey (200 firms): **8%** of migrations on time, **>60%** over budget, 65% severe quality defects ([CIO](https://www.cio.com/article/3851772/sap-users-struggle-with-s4-hana-migration.html)); DSAG (Oct 2026) attacks opaque licensing ([CIO](https://www.cio.com/article/4232191/sap-customers-seek-renewed-control-over-their-erp-fate.html)); $172M Zimmer Biomet v. Deloitte S/4 lawsuit ([Loeb](https://www.loeb.com/en/newsevents/news/2025/09/loeb-represents-zimmer-biomet-in-172-million-lawsuit-against-deloitte)) |
| **Oracle NetSuite** | SMB/mid; 42k+ customers ([SiliconANGLE](https://siliconangle.com/?p=714541)) | Multi-tenant SaaS; SuiteScript with governance caps (e.g. 1,000 units per user-event script) ([Oracle](https://docs.oracle.com/en/cloud/saas/netsuite/ns-online-help/section_N3351480.html)); SuiteFlow; custom records; MCP-based AI connector ([Crowe](https://www.crowe.com/insights/netsuite-blog/suiteworld-2025-expecting-ai-finding-so-much-more)) | ~$75k–250k/yr for 25–100 users ([Vendr](https://www.vendr.com/buyer-guides/netsuite)) | ~$1.1B/quarter, +14% ([Nasdaq](https://www.nasdaq.com/articles/oracles-netsuite-division-focus-will-it-boost-revenue-growth)) | Renewal price hikes ([Oracle Community](https://community.oracle.com/netsuite/english/discussion/4217048/new-renewal-prices)) |
| **Oracle Fusion ERP** | Enterprise, finance-led | Single SaaS suite; AI Agent Studio included ([Oracle](https://www.oracle.com/de/news/announcement/oracle-expands-ai-agent-studio-for-fusion-applications-with-agentic-applications-builder-2026-03-24/)) | Per user | Gartner MQ leader ([CX Today](https://www.cxtoday.com/customer-analytics-intelligence/gartner-magic-quadrant-for-cloud-erp-for-product-centric-enterprises-2025-the-rundown/)) | AI consumption billing *(advisor, thin)* |
| **MS Dynamics 365 F&SCM** | Upper-mid/enterprise | Extensions only (X++ Chain of Command); overlayering not supported ([Microsoft](https://learn.microsoft.com/en-us/dynamics365/fin-ops-core/dev-itpro/extensibility/extensibility-home-page)) | $210/user/mo; Premium $300 ([ERP Research](https://www.erpresearch.com/en-us/dynamics-365-finance-operations-price-list)) | Gartner leader | Copilot metered per credit ([Stoneridge](https://stoneridgesoftware.com/?p=287702)) |
| **MS Business Central** | SMB/lower-mid; 50k+ customers ([yzhums](https://yzhums.com/63206/)) | AL event-subscriber extensions, no base edits ([Microsoft](https://learn.microsoft.com/en-us/dynamics365/business-central/dev-itpro/developer/devenv-extensibility-overview)); MCP server ([Microsoft](https://learn.microsoft.com/en-us/dynamics365/release-plan/2026wave1/smb/dynamics365-business-central/create-agentic-experiences-enhanced-mcp-server)) | $80 / $110 per user/mo ([Schneider](https://www.schneider.im/microsoft-dynamics-365-business-central-price-storage-increases/)) | M365 adjacency, AppSource | Price increases *(thin)* |
| **Odoo 20** (Sep 2026) | SMB/mid; 28M users, ~80% free ([Forbes BE](https://www.forbes.be/fr/odoo-valorise-10-milliards-deuros-cap-sur-le-milliard-de-chiffre-daffaires/)) | Python ORM on PostgreSQL, OWL frontend ([Odoo](https://www.odoo.com/documentation/19.0/developer/reference/frontend/owl_components.html)); Studio only on the Custom plan ([Odoo](https://www.odoo.com/pricing)); built-in MCP server ([Odoo](https://www.odoo.com/documentation/master/applications/productivity/ai/mcp_server.html)); CE LGPLv3, EE proprietary | ~$31–47/user/mo *(partner)* | ~€620M revenue 2025 | Only the last 3 majors supported ([Odoo](https://odoo.com/documentation/18.0/administration/supported_versions.html)); slow on large tables *(partner)*; €36M loss in 2025 |
| **Workday Financials** | Enterprise services | In-memory object model; Extend low-code ([Workday](https://www.workday.com/en-gb/products/platform-product-extensions/app-development.html)) | Subscription + Flex Credits | One model for transactions and reporting | No manufacturing depth *(thin)* |
| **Infor CloudSuite** | Industry mid/large mfg & distribution | Multi-tenant AWS; Mongoose tooling, no DB access ([Godlan](https://godlan.com/resource/infor-cloudsuite-industrial-personalizations-and-extensibility-options-in-the-cloud)) | Quote | Industry depth | Rigid in multi-tenant |
| **Epicor Kinetic** | Mid-market mfg/distribution | Application Studio + BPM ([Epicor](https://www.epicor.com/en/products/enterprise-resource-planning-erp/kinetic/global-business-management/ptw-application-studio/)) | Quote | Manufacturing | *(thin)* |
| **IFS Cloud** | Asset-heavy, field service, A&D | 2 releases/yr + monthly updates ([IFS](https://www.ifs.com/en/assets/cloud/what-new-in-ifs-cloud-25r2)) | Quote | EAM/FSM depth | *(thin)* |
| **Acumatica** | SMB/mid; 10k+ customers | Low-code + REST ([ERP Research](https://www.erpresearch.com/pricing/acumatica)) | Resource-based, unlimited users | Pricing model | Now Vista-owned (~$2B) ([Bloomberg Law](https://news.bloomberglaw.com/ip-law/buyout-firm-vista-to-acquire-eqts-acumatica-in-2-billion-deal)) |
| **India: Tally / Zoho / Busy / Marg** | MSME | Tally desktop (~27 lakh customers, ~₹852 cr FY26) ([Outlook Business](https://www.outlookbusiness.com/news/tally-solutions-expects-to-cross-1000-cr-revenue-in-fy27-with-25-30-pc-growth)); Zoho Books SaaS with own GSP for e-invoicing ([Zoho](https://www.zoho.com/in/books/e-invoicing)) | Tally ₹22.5k–67.5k one-time ([Tally](https://tallysolutions.com/business-guides/tallyprime-total-cost-of-ownership-india/)); Zoho ₹749–7,999/org/mo | Price, accountant familiarity | Accounting-first; weak manufacturing/inventory depth |
| **AI-native entrants** (Rillet, Campfire, DualEntry, Light, Everest, Doss) | VC-backed mid-market finance | AI-first close and revenue recognition ([ERP Research](https://erpresearch.com/en-us/ai-native-erp)); Rillet $100M at a $1B valuation ([TNW](https://thenextweb.com/news/rillet-100m-series-c-1bn-accounting-ai)) | Rillet ~$20–35k/yr | Speed of close | Finance-only; **"none offers manufacturing depth"** |
| **ERPNext / Frappe** | SMB/mid, India/MEA ([Frappe](https://docs.frappe.io/partner-guide/about-frappe)) | Python + MariaDB, metadata DocTypes; **PostgreSQL not supported for ERPNext** ([forum](https://discuss.frappe.io/t/is-erpnext-officially-supported-with-postgresql/163329)) | GPL-3.0; Frappe Cloud from $5/site/mo ([Frappe](https://frappe.io/cloud/pricing)) | 40.7k GitHub stars, India Compliance app | See §3 |
| **Other OSS** | — | Dolibarr (PHP), iDempiere (Java), OFBiz (bug-fix only), Tryton (Python); Twenty CRM (TS + Postgres, 60k★), Medusa (MIT, 37k★) | Free | Reusable CRM/commerce pieces | **No mature Rust ERP exists** (e.g. [rerp](https://github.com/microscaler/rerp) is early) |

## 2. Gaps worth attacking

1. **SAP ECC mid-size holdouts.** ~61% of ~35k ECC customers hadn't licensed S/4 by end-2024 ([Register](https://www.theregister.com/2025/03/20/sap_sees_little_progress_in)). Mainstream support ends 2027, extended to 2030. The 2031–33 bridge needs a RISE contract ([ASUG](https://www.asug.com/insights/sap-introduces-new-erp-transition-option-to-support-rise-with-sap-transformations)).
2. **Two-tier ERP**: subsidiaries of SAP groups, where SAP's licensing makes two-tier awkward ([Register](https://www.theregister.com/2025/09/16/sap_dsag_licensing_transparency/)).
3. **Transparent pricing**: NetSuite renewals, SAP licence opacity, metered AI credits.
4. **Fast transactions *and* fast reports in one product**: Frappe had to bolt on a non-real-time DuckDB copy because a 700M-row GL trial balance never finished ([Frappe blog](https://frappe.io/blog/engineering/half-a-billion-ledger-entries)).
5. **Incremental, correct stock valuation**: ERPNext's reposting jobs are slow and get stuck ([docs](https://docs.frappe.io/erpnext/stock-value-comparison), [forum](https://discuss.frappe.io/t/repost-item-valuation-got-stuck-failed/146487)). SAP made Material Ledger mandatory for the same reason.
6. **Fully open, Postgres-native suite**: ERPNext doesn't run on Postgres, and Odoo's Enterprise edition is proprietary.
7. **Electronics / EV-hardware makers**: BOM-revision and ECO handoff to contract manufacturers. **EU battery passport mandatory from 18 Feb 2027** ([Automotive IQ](https://www.automotive-iq.com/electrics-electronics/articles/eu-battery-passport-explained-requirements-timeline-and-compliance-steps-to-2027)). India's draft "Battery Pack Aadhaar" ([pv magazine](https://www.pv-magazine-india.com/2026/01/05/india-introduces-battery-aadhaar-system-to-track-ev-batteries-across-lifecycle)). ₹69,548 cr of approved electronics-component projects ([Angel One](https://www.angelone.in/news/economy/government-approves-31-electronics-component-projects-worth-7-877-crore-across-10-states)).
8. **AI-native ERP for companies that make things**: the funded AI-native entrants are all finance-only.
9. **India compliance in the core**: e-invoice above ₹5 cr turnover; 30-day IRP reporting window above ₹10 cr ([KPMG](https://kpmg.com/us/en/taxnewsflash/news/2024/12/tnf-india-new-group-of-taxpayers-required-to-report-invoices-within-30-days.html)); e-way-bill rule changes ([Taxscan](https://www.taxscan.in/gstn-issues-important-advisory-on-updates-to-e-way-bill-and-e-invoice-systems/468484)); GST 2.0 slabs from 22 Sep 2025 ([Thomson Reuters](https://www.thomsonreuters.com/en-us/help/onesource-global-trade/regulatory-insights/2025/october-14th/resetting-the-tax-landscape-inside-india-gst-2-0)).
10. **SEA e-invoicing wave**: Malaysia, the Philippines, Singapore InvoiceNow ([Thomson Reuters](https://insight.thomsonreuters.com/sea/business/posts/sea-e-invoicing-touchless-tax-compliance)).
11. **Tally upgrade path**: 7.3 crore registered MSMEs ([Statesman](https://www.thestatesman.com/business/msme-sector-achieves-key-milestones-in-2025-udyam-registrations-cross-7-3-crore-msme-ministry-1503532923.html)).
12. **Agent-safe, permissioned APIs**: Odoo, Business Central and NetSuite ship MCP. Released ERPNext relies on community
    servers ([forum](https://discuss.frappe.io/t/mcp-server-for-erpnext-120-tools-to-connect-ai-agents-to-your-erp/161198)),
    but Frappe's v17-dev code already has a built-in MCP endpoint, on by default (`frappe/mcp`). MCP itself is table stakes.
    The gap is *consistent* permissions: Frappe's list and document permission paths diverge, and 427 call sites bypass
    permissions ([analysis](analysis-erpnext.md#framework-deficiencies-frappe)).
13. **Upgrade-safe extensions + LTS**: SAP's clean-core levels and Microsoft's no-overlayering exist for this. Frappe users ask for an LTS ([forum](https://discuss.frappe.io/t/frappeverse-2026-has-left-me-confused/164673)).
14. **Multi-entity / intercompany**: manual in ERPNext ([forum](https://discuss.frappe.io/t/consolidation-of-two-companies-in-erpnext-but-ignore-inter-company-transactions/32874)).
15. **Implementation risk**: S/4 failures are driven by integration, process definition and interfaces ([CIO](https://www.cio.com/article/3851772/sap-users-struggle-with-s4-hana-migration.html)). Templates, migration and test tooling are the product.

## 3. What ERPNext users complain about

| # | Complaint | Source |
|---|---|---|
| 1 | Reports take ~5 minutes after <1 year at ~1,000 invoices/day, on a 48 GB server | [forum](https://discuss.frappe.io/t/most-report-in-erpnext-takes-about-5-to-minutes-to-run-when-database-in-1-year-old/162379) |
| 2 | MariaDB at 700M GL rows: trial balance never finishes | [Frappe blog](https://frappe.io/blog/engineering/half-a-billion-ledger-entries) |
| 3 | Repost Item Valuation slow / stuck / failed | [docs](https://docs.frappe.io/erpnext/stock-value-comparison), [forum](https://discuss.frappe.io/t/repost-item-valuation-got-stuck-failed/146487) |
| 4 | Stock ledger rebuild promised for v17, no date | [forum](https://discuss.frappe.io/t/frappeverse-2026-has-left-me-confused/164673) |
| 5 | No PostgreSQL support for ERPNext (dev branch only, isolation unfinished) | [forum](https://discuss.frappe.io/t/is-erpnext-officially-supported-with-postgresql/163329), [forum](https://discuss.frappe.io/t/postgres-support/162440) |
| 6 | MariaDB "Row size too large" caps fields per DocType | [docs](https://docs.frappe.io/erpnext/maximum-number-of-fields-in-a-form) |
| 7 | Major upgrades remove public APIs and break custom apps | [forum](https://discuss.frappe.io/t/concerns-questions-about-the-future-direction-of-erpnext-and-the-frappe-ecosystem/148057) |
| 8 | Minor updates break sites | [forum](https://discuss.frappe.io/t/help-needed-erpnext-15-66-0-x-frappe-15-72-3-broke-my-local-setup-on-bench-update/149009) |
| 9 | No LTS release, while implementations take 8–12 months | [forum](https://discuss.frappe.io/t/frappeverse-2026-has-left-me-confused/164673) |
| 10 | Dated UI (acknowledged by Frappe staff) | [forum](https://discuss.frappe.io/t/concerns-questions-about-the-future-direction-of-erpnext-and-the-frappe-ecosystem/148057) |
| 11 | Product fragmented into separate apps (CRM, Helpdesk) with duplication | same thread |
| 12 | Stale issues (~1.2k open), weak docs | same thread |
| 13 | Hard to self-host (MariaDB + Redis + Node), risky upgrades | [Capterra](https://capterra.com/p/164441/ERPNext/reviews/) |
| 14 | Support/partner quality (G2 support score 6.5) | [G2](https://www.g2.com/compare/erpnext-vs-syspro-vs-dolibarr-erp-crm), [forum](https://discuss.frappe.io/t/service-provider-s-unethical-practices/116015) |
| 15 | Multi-company gaps (company-restricted items/customers reached dev only in Jul 2026) | [Frappe](https://frappe.io/blog/product-updates/product-updates-for-july-2026) |

Frappe's own scale: 16k+ hosted sites. It claims "$5M MRR", but its per-region table adds up to ~$0.44M/month, so ~$5M
**ARR** is likelier *(vendor)* ([Frappe](https://docs.frappe.io/partner-guide/about-frappe)). v16 shipped Jan 2026 claiming
~2× speed. v17 (UI redesign + stock-ledger rebuild) has no date.

## 4. How Ferrum wins

You don't beat SAP by being a smaller SAP. Win where its architecture and business model make it slow and expensive,
start in a segment it serves badly, and make switching cheap.

### 4.1 Differentiation pillars (each is a measurable claim)

| Pillar | Claim | How it's proven |
|---|---|---|
| **Customization without code, safely** | Fields, rules, forms, workflows, approval matrices, reports, print formats and integration mappings, done by an analyst. Promoted sandbox → change-set → prod, and they survive upgrades | Upgrade preview + compatibility analyzer runs on every tenant before every release; LTS releases |
| **Plugin-first** | Core modules are packages on the public extension API; packages install per tenant at runtime | Marketplace CI tests every package against current and next kernel |
| **Fast transactions *and* reports** | Target ≥5× throughput per CPU vs Python ERPs; trial balance on 1B lines in seconds (partition pruning + period snapshots, columnar path later) | Published, reproducible benchmarks vs ERPNext/Odoo |
| **Correct ledgers** | Exact decimals, append-only hash-chained journal, DB-enforced integrity, no reposting | Property tests + differential tests vs ERPNext; auditor-readable invariants |
| **Open + Postgres-native** | Open-source core, plain Postgres, REST/GraphQL/MCP, self-host or cloud | `pg_dump` is a complete export |
| **AI that drafts, code that posts** | Agents work through typed, permission-checked APIs; AI drafts documents and change-sets, deterministic engines post | Every AI action is an audited user action |
| **Time-to-value** | Industry + localization packs, ERPNext/Tally importers, AI-assisted setup: weeks, not 12–24 months | Go-live time tracked per customer |

### 4.2 Wedge, in order

1. **ERPNext users who've hit the wall** (reports, reposting, upgrades, no Postgres). They need a mechanical migration with a
   parity report and a compatible API. They already buy open-source, metadata-driven ERP.
2. **Mid-market manufacturers/distributors in India → SEA → GCC** outgrowing Tally/Zoho/Busy and priced out of SAP/Oracle.
   GST, e-invoice and e-way bill are table stakes.
3. **Electronics / EV hardware** as the depth vertical: approved-manufacturer lists, ECO/ECN, serial genealogy and
   **battery passport** data (EU Feb 2027; India's Battery Pack Aadhaar), test-rig data capture, CAD BOM import.
4. Then up-market: SAP ECC holdouts and SAP subsidiaries (two-tier) once multi-GAAP, consolidation and APS exist (phase 4).

### 4.3 Business model

* Open-source core (licence decision in [PLAN.md](../PLAN.md#decisions-needed-from-you)).
* Managed cloud, priced per company + usage, **not per named user**. Against SAP/NetSuite/Dynamics per-user pricing, that
  difference is the headline.
* Enterprise packages (consolidation, APS, SoD, advanced WMS, HA tooling), marketplace revenue share, certified partners,
  LTS support contracts.

## 5. Hard truths

1. **SAP customers delay; they rarely leave.** 54% of DSAG members still run ECC, but most plan S/4 investment
   ([DSAG](https://dsag.de/presse/dsag-investment-report-2026-companies-are-investing-more-selectively-ai-is-becoming-established-cloud-computing-is-being-put-to-the-test/)).
   Start mid-market, subsidiaries and India, not Fortune 500.
2. **Distribution beats technology.** Odoo has 28M users and still lost €36M in 2025. Frappe is ~$5M ARR after a decade.
   Partners, localization and reference customers win deals. Rust alone won't.
3. **Ledger + valuation correctness at scale *is* the product.** That's why SAP built ACDOCA and a mandatory Material
   Ledger. Build those first, UI polish second.
4. **Upgrade safety must be designed in from v1**: versioned extension points + LTS releases, or the platform repeats
   ERPNext's breakage.
5. **AI won't rescue implementations.** Gartner expects >40% of agentic-AI projects to be scrapped by 2027
   ([Outlook Business](https://www.outlookbusiness.com/artificial-intelligence/over-40-of-agentic-ai-projects-will-be-scrapped-by-2027-says-gartner)),
   and incumbents give agents away. Win with AI that drafts while deterministic code posts, plus migration tooling and
   certified partners.
