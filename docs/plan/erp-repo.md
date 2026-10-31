# ERP in a repository of its own

ERP was the one app split across repositories: its manifest in the server (`server/apps/erp`),
its interface in Ferrum-Frontend (`apps/erp`), its add-ons as crates of this workspace
(`crates/finance`, `procurement`, `stock-ops`, `mfg-ops`), and its packages and logic in
`ferrum-domain` (accounting, inventory, buying, selling, manufacturing; India as its add-on).
Every other app keeps its manifest, packages, logic, interface, tests and version together in
its own repository (apps-ui.md §10.5). [CBFerrum/ferrum-app-erp](https://github.com/CBFerrum/ferrum-app-erp)
brings ERP to the same shape, in stages that each leave every repository's main working.

## Stages

| Stage | What moves | Done when |
|---|---|---|
| **1. The app and its interface** | ERP's and India's manifests; ERP's interface (`ui/`, built on `@ferrum/records/frame` like CRM's, `ui/dist` committed and embedded in the `ferrum-app-erp` crate); the server depends on the crate | `/erp/` and the portal's invoices are served from the crate's interface; Ferrum-Frontend has no `apps/erp` |
| **2. The add-ons** | Finance, Procurement, Stock Operations and Manufacturing Operations, as crates of the repository's workspace with their tests | the server takes them from the ERP repository; this workspace has no `crates/finance` … |
| **3. The packages and the engine** | ERP's five packages and its logic, into ERP's crate; India's package and logic, into an add-on of its own on a seam every country uses; apps that build on the engine depend on ERP's crate | `ferrum-domain` holds the shared core only; this repository has no ERP or India code or package; existing workspaces run on unchanged |

Stages 1 and 2 are built (31 October 2026). Stage 3 is designed below.

## Stage 3: the packages and the engine

### What the other apps call today

Every app uses the domain's small helpers (`util`: field access, the `sys` and `elevated`
contexts, money and rounding, a company's currency and precision, exchange rates) and its report
framework (`reports::ReportSource`, `add_source`, `run`, `read_tx`, `Params`, `col`, `period`,
`resolve_company`, `today`); their tests take the platform's `packages()`, `register` and
`maintenance`. Beyond those, from every repository's main on 31 October 2026:

| Repository | ERP's engine | Document helpers | Its tests need |
|---|---|---|---|
| Point of Sale | `ledger` (`post`, `reverse`, `reversal_point`, `allocate`, `unallocate`, `Entry`, `Line`), `stock` (`post_moves`, `reverse_moves`, `MoveIn`, `parse_serials`), `pricing` (`calculate`, `calc_input`, `Side`), `flow::sync_invoice_outstanding`, `india::GstTemplateHook` | — | ERP's packages and hooks |
| Commerce | `ledger` (`allocate`, `load_open_item`), `pricing::calculate`, `flow::sync_invoice_outstanding` | — | same |
| Assets | `ledger` (`post`, `reverse`) | `trade::create_for_user`, `flow::created` | same |
| HR (`hr_erp`, `payroll_erp`) | `ledger` (`post`, `reverse`) | — | same |
| CRM (`crm_erp`) | `pricing::calculate`, `india::GstTemplateHook` | `flow` (`map_document`, `MapSpec`, `add_progress_in`, `created`), `trade::create_for_user` | same |
| Field Service | `stock::parse_serials` | `trade::create_for_user`, `flow::created` | same |
| Bookings, Helpdesk (`helpdesk_erp`) | — | `trade::create_for_user`, `flow::created` | same |
| Projects (`projects_erp`) | — | `flow::created` | same |
| Comms, Forms | — | — | `selling` (customers, the Sales roles) |
| Docs, Drive, LMS, Sign, Campaigns | — | — | the shared packages only |
| ERP's add-ons | `ledger`, `stock`, `stock_entry`, `manufacturing`, `pricing`, `flow::sync_invoice_outstanding` | `trade::create_for_user`, `flow::created` | ERP's packages and hooks |
| Ferrum | `india::gstin_check_char` (the demo); the server registers everything | — | Studio's and MCP's tests run on ERP's records; kernel, collaboration and SSO tests install `selling` or `accounting` for customers and companies |
| ferrum-store | — | — | checks uploaded apps against `ferrum_domain::packages()`, ERP's included |

### Where each piece goes

| Piece | Stage 3 |
|---|---|
| `util`; the report framework; `flow`'s document helpers (`add_progress`, `add_progress_in`, `MapSpec`, `map_document`, `shared_line_fields`, `created`, `set_status`) with `trade::create_for_user` moving into `flow`; `contacts` (contacts' and addresses' hooks) and the item-units hook (from `setup`, as `products`); packages `core`, `contacts`, `parties`, `products`, `project_records`; `ferrum-calc` | stay in Ferrum: the shared core |
| `accounting`, `ledger`, `setup` (company set-up, fiscal years), `pricing`, `trade`, `stock`, `stock_entry`, `reservation`, `manufacturing`, `portal` (the portal's invoices); the rest of `flow` (`refresh_order`, `refresh_billing`, `invoice_status`, `sync_invoice_outstanding`); the built-in reports (trial balance to purchase register, now a `ReportSource`); journal partitions (`maintenance`) | the `ferrum-app-erp` crate, under the same module names |
| packages `accounting`, `inventory`, `buying`, `selling`, `manufacturing` | ERP's `packages/`, byte for byte |
| `india` (GSTIN, place of supply, GST templates), `einvoice`, `irp`, `nic_keys/`; package `india`; India's `app.yaml` | `addons/india`, crate `ferrum-india`, an add-on like Finance |

`ferrum-domain` keeps its name and its three entry points, so apps that use only the shared core
change nothing: `packages()` gives the five shared packages, `register` their hooks, and
`maintenance` (nothing left to do) stays until no app's tests call it.

ERP's crate gets a native app's entry points (`packages()`, `register`, `reports()`,
`maintenance`) beside `MANIFEST` and `UI`. The server keeps calling it by name, before the apps
it compiles in, so ERP's hooks still run ahead of its add-ons' on the same documents. India
becomes a compiled app behind the feature `india` (on by default), first in the server's list,
which keeps its hooks where they ran.

### Every country as an add-on

ERP's engine knows no country. A country's tax and legal rules are an add-on, as India already
is for workspaces: its package adds the fields the country needs, prefixed with its code
(`in_gstin`, `in_place_of_supply`), and its crate plugs into the engine at one seam,
`ferrum_app_erp::localization`:

```rust
#[async_trait]
pub trait Localization: Send + Sync {
    /// The package that switches it on in a workspace (India: `india`).
    fn package(&self) -> &'static str;
    /// Before a document's prices and taxes are calculated: what is taxed, how (India: the GST
    /// template for the place of supply). Runs inside `pricing::calculate`, so ERP's documents,
    /// CRM's quotations, POS's sales and Commerce's carts all get it.
    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()>;
    /// Before a priced document is submitted (India: HSN/SAC codes on outward supplies).
    async fn check(&self, hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()>;
    /// After a new company's standard chart: the country's accounts and tax templates.
    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()>;
}
```

Each runs only where its package is installed, and only for companies of its country (India's
GST rules need a company with a GST state; its set-up skips a company whose country is another),
so one workspace can keep companies in several countries. CRM and POS lose their calls into
India and get its rules, or any other country's, through `pricing::calculate` and
`localization::check`. Commerce's carts then take the GST template for the buyer's place of
supply rather than always the store's: GST's rule, and part of the step-2 agreement on Commerce.

Everything else a country needs is a mechanism the platform already has:

| A country needs | Through | India today |
|---|---|---|
| tax IDs and party details (GSTIN, VAT number, EIN, NPWP, UEN) | its package's fields on Company, Customer, Supplier and Address; document hooks scoped to its package | `in_gstin`, its check character |
| which taxes apply: rates, place of supply, reverse charge, exemptions, or a tax service's answer | `determine` | GST template by place of supply |
| what a document needs before it is final | `check`, or document hooks | HSN/SAC codes |
| a company's accounts and tax templates | `setup_company` | GST accounts and templates |
| e-invoices and real-time reporting, by clearance or over Peppol | a hook on submit and cancel queues the work; an event sink (the outbox) talks to the tax network; the answer (IDs, QR code) lands in its fields; credentials are secrets | IRP e-invoices, e-way bills |
| returns and statutory reports (GST and VAT returns, EC sales lists, SAF-T, 1099s) | `ReportSource` | — |
| legal content on prints (tax IDs, QR codes, reverse-charge wording) | its package's print formats | sales invoice |
| withheld taxes (TDS, EWT, PPh 23, WHT) | deducted tax rows (`ferrum-calc`), payment deductions | TDS accounts |
| numbering and records (gapless series per fiscal year, audit trail) | the kernel's numbering (`SINV-{FY}-{seq:5}`, gapless on submit) and chained audit log | — |

What the regions ask for, and where it lands. Dates are as known in mid-2026; each country's own
plan checks them again, since they move often:

| Region | Taxes | E-invoicing and reporting | Add-on |
|---|---|---|---|
| India (built) | GST: CGST and SGST, or IGST, by place of supply; cess; TDS and TCS | IRP e-invoices above ₹5 crore turnover, e-way bills; GST returns still to build | `india` |
| European Union | VAT at standard, reduced and zero rates; reverse charge between businesses across borders; VAT numbers checked in VIES; OSS and IOSS for distance sales; EC sales lists; Intrastat | EN 16931 invoices (UBL or CII, mostly over Peppol), required between EU businesses with digital reporting from July 2030 (ViDA); national schemes first: Italy's SDI, Poland's KSeF, France's certified platforms, Germany's (receiving since 2025, sending from 2027 and 2028), Spain's Verifactu, Portugal's SAF-T and ATCUD, Romania's e-Factura, Hungary's RTIR, Greece's myDATA, Belgium's Peppol (2026) | `eu` for the shared VAT rules and EN 16931; one per country for its scheme |
| United Kingdom | VAT, postponed import VAT | Making Tax Digital returns over HMRC's API | `gb` |
| United States | sales and use tax by state, county, city and district; economic nexus per state; product taxability; exemption certificates; 1099 reporting | no national e-invoicing | `us`: `determine` asks a tax service (Avalara, Vertex, TaxJar, Stripe Tax) or reads rate tables |
| Canada | GST or HST; PST and QST by province | — | `ca` |
| South East Asia | Singapore GST; Malaysia SST; Indonesia PPN and PPh withholding; Thailand VAT and WHT; Philippines VAT and EWT; Vietnam VAT | Singapore's InvoiceNow (Peppol); Malaysia's MyInvois (validated by LHDN, phased in since 2024); Indonesia's Coretax e-Faktur (2025); Thailand's e-Tax Invoice; the Philippines' EIS; Vietnam's e-invoices carrying the tax office's code (since 2022) | one per country |
| Elsewhere | Gulf VAT; GST in Australia and New Zealand; Japan's consumption tax with qualified invoices; Mexico's IVA; Brazil's ICMS and ISS, and the CBS/IBS reform | Saudi Arabia's Fatoora (clearance), the UAE's e-invoicing, Peppol in Australia, New Zealand and Japan, Mexico's CFDI, Brazil's NF-e, China's fapiao | the same seams |

Before the first country after India, the shared parts need what no Indian document has needed:

* ISO 3166 country codes on Company and Address (`country` is free text today): rules across
  borders compare them. A change to the core packages, with a check.
* `ferrum-calc`: taxes per quantity (excise, eco and packaging fees), and reverse charge (posted to
  input and output tax, never charged to the party).
* tax categories and exemption reasons on lines (EN 16931's VAT breakdown), as fields of the
  country's package.

The same pattern serves what is not ERP's: POS's fiscal devices and signatures (Germany's TSE,
France's NF525, Italy's RT, Austria's RKSV) are POS's add-ons; payroll per country is HR's; data
protection (GDPR, India's DPDP Act, the PDPAs of Singapore and Thailand) and data residency are the
platform's (regions, export, erasure).

Stage 3 builds the seam and moves India onto it. Every other country is an add-on of its own,
planned and built when it is picked (a localization plan, `docs/plan/localization.md`).

### Depending on ERP without a cycle

Crates form a line; only repositories point both ways, as every app's already do:

```
ferrum-kernel, ferrum-meta, ferrum-calc                      Ferrum
        ↑
ferrum-domain: the shared core                               Ferrum
        ↑
ferrum-app-erp: ERP's engine, packages, manifest, interface  ERP's repository
        ↑                        ↑
ferrum-india, ferrum-finance …   POS, Commerce, Assets, Field Service, HR, CRM
        ↑                        ↑
ferrum-server                                                Ferrum
```

* No crate below ERP's depends on it: in Ferrum's workspace only the server does, and Studio and
  MCP for their tests.
* Every repository names ERP's crate the same way (`git = "https://github.com/CBFerrum/ferrum-app-erp",
  branch = "main"`), so a build holds one copy. ERP's add-ons name it by path inside their
  repository, which Cargo resolves to the same git source when the server takes them.
* Ferrum's `[patch]` of its own crates already makes ERP's crates build against this checkout, as
  for every app; `Cargo.lock` pins ERP like any app.
* ERP's end-to-end test runs India's rules too, so ERP's crate takes India as a dev-dependency (a
  cycle through dev-dependencies, which Cargo allows for tests). Apps that need ERP's records only
  in their tests (Bookings, Helpdesk, Projects, Comms, Forms) take ERP's crate as a dev-dependency.
* Every server compiles ERP and India in, so the store checks uploads against their packages as
  well as the shared ones.

### Existing workspaces

No upgrade step is needed; the one this stage once planned has nothing to do.

* A workspace records its apps in `tenant_app` (app id, version) and its packages in
  `installed_package` (name, version); roles carry their package's name, and the cluster records
  each package's SQL files by package and file name with a checksum (`cluster_package_sql`). Which
  app owns a package is read from the apps' manifests: ERP's has listed its five packages since
  0.1 (U3), India's lists `india`.
* Stage 3 changes none of these: app ids, package names and versions stay, and the packages'
  files move byte for byte. Nothing moves in the database, nothing asks for consent, and there is
  no migration.
* ERP stays 0.2.0 and India 0.1.0: what a workspace installs (manifest, interface, packages) is
  unchanged, and a new version would only ask every administrator to approve an update that
  changes nothing. The start-up recording of ERP (`ensure_native("erp", "accounting")`) stays.
* A guard: ERP's tests pin the checksums of the SQL files clusters have applied
  (`accounting/sql/001_ledger.sql`, `002_project_index.sql`, `inventory/sql/001_stock.sql`). The
  kernel refuses to start once an applied file changes, so an edit would stop every running server.
* A rehearsal before pushing: the development database's demo workspace, made before stage 3,
  starts on the new build with no package SQL applied and the same apps and packages, and its
  pages, reports and invoices work as before.

### Tests

| Test | Today | Stage 3 |
|---|---|---|
| the end-to-end scenario (opening stock, procure-to-pay, order-to-cash with GST and a fake IRP, payments, credit notes, manufacturing, MRP, period closing, concurrent stock issues) | `crates/domain/tests/domain_it.rs` | ERP's `tests/erp_it.rs` on its own database (`ferrum_erp_test`); from step 4 also a company outside India that gets none of India's rules |
| the moved modules' unit tests | `crates/domain/src` | ERP's crate (reports' catalog, company set-up, stock) and India's (GSTIN check characters, e-invoice JSON, IRP encryption) |
| the embedded packages form a valid catalog | the domain, all eleven | the domain: the five shared; ERP: its five with them, together and each alone; India: its package with ERP's; plus the SQL checksums above |
| `crates/meta/tests/packages.rs` (shipped packages; `SalesInvoice`, `Customer.in_gstin`) | meta | the shared packages; the ERP and India assertions join ERP's and India's tests |
| `kernel_it` (fiscal years for an exclusion constraint and a company in use), `collab_it`, `sso_it` | install `selling` or `accounting` | the shared packages: item prices' exclusion constraint, a project's company |
| Studio's and MCP's tests, the server's documents test | ERP's records from the platform | ERP's packages and hooks from ERP's crate |
| CI databases | Ferrum creates `ferrum_domain_test` | Ferrum drops it; ERP's CI creates `ferrum_erp_test` |

### Order of the commits

Before anything is pushed, every repository is built and tested locally with the others patched to
their new versions. Then, in this order:

| Step | Repository | Change | Its CI builds against | Why every main stays green |
|---|---|---|---|---|
| 0 | Ferrum | `create_for_user` moves into `flow`, still reachable as `trade::create_for_user` | its own lock | nothing else changes |
| 1 | ferrum-app-erp | the engine, ERP's packages and their tests move in; India becomes `addons/india`; the add-ons use ERP's modules. ERP's and India's entry points give nothing while the `ferrum-domain` they are built with still has ERP's packages (`handed_over()`), so a test that calls both never gets two copies of a package or a hook | Ferrum's main | Ferrum's server still pins the ERP commit before this one; until step 3, ERP's CI runs its engine tests through `ferrum-domain`'s copy |
| 2 | POS, Commerce, Assets, Field Service, HR, CRM (a dependency); Bookings, Helpdesk, Projects, Comms, Forms (a dev-dependency); ferrum-store | imports move to `ferrum_app_erp::…` (and `flow::create_for_user`); CRM and POS drop their India calls; tests call ERP's (and where needed India's) entry points beside the shared core's; the store adds ERP's and India's packages | Ferrum's main and ERP's main | step 1's entry points still give nothing, and each repository uses one copy of each module |
| 3 | Ferrum | `ferrum-domain` keeps the shared core and `packages/` the five shared packages; the server takes ERP's and India's packages, hooks, reports and upkeep from their crates; the test changes above; `Cargo.lock` pins step 1 and every step-2 app | its own lock (`--locked`) | the pinned commits already use the new entry points; from now on app and ERP CIs get ERP's packages from ERP's crate alone |
| 4 | ferrum-app-erp | `handed_over()` goes, so its CI tests its own engine, the company outside India included; a CHANGELOG entry | Ferrum's main, from step 3 | — |
| 5 | Ferrum | its next pin of ERP takes step 4 | — | — |

CRM, Helpdesk, HR, Projects and Commerce belong to another session's work: their step-2 changes
are agreed with it first. Ferrum-Frontend has nothing to change.

### Done when

* `ferrum-domain` and `packages/` hold the shared core only, and no ERP or India module or package
  is left in this repository.
* Every main stays green through the six steps (ERP's, the eleven apps', the store's and
  Ferrum's), and Ferrum's image job and kind cluster pass.
* The rehearsal above passes: an existing workspace runs on with nothing moved.
* An Indian company's invoice, CRM quotation and POS sale pick their GST templates through the
  seam; a company outside India in the same workspace gets none of India's rules.

## Order of a change that spans repositories

1. Ferrum-Frontend first when an interface needs a new export (stage 1: the records frame's
   routes and home).
2. The ERP repository, whose CI builds against Ferrum's main and Ferrum-Frontend's main.
3. Apps that build on ERP's crate, whose CI builds against Ferrum's main and ERP's main.
4. Ferrum, switching its dependency and removing its copies in the same commit.
5. Ferrum-Frontend's removal of what moved, then Ferrum's `FRONTEND_REV`.

An interface compiled into an app's crate wins over a copy of it in `settings.ui_dir`, so an image
built from an older `FRONTEND_REV` never serves the old ERP interface.
