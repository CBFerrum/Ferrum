# ERP in a repository of its own

ERP was the one app split across repositories: its manifest in the server (`server/apps/erp`),
its interface in Ferrum-Frontend (`apps/erp`), its add-ons as crates of this workspace
(`crates/finance`, `procurement`, `stock-ops`, `mfg-ops`), and its packages and logic in
`ferrum-domain` (accounting, inventory, buying, selling, manufacturing; India as its add-on).
Every other app keeps its manifest, packages, logic, interface, tests and version together in
its own repository (apps-ui.md §10.5). [CBFerrum/ferrum-app-erp](https://github.com/CBFerrum/ferrum-app-erp)
brings ERP to the same shape, in stages that each leave every repository's main working.

## What stays in the platform

`ferrum-domain` is also the business engine other apps build on: Point of Sale prices sales and
closes into the books with it, Commerce prices carts, HR's payroll, Assets, Bookings and Field
Service post through it, CRM's and Helpdesk's ERP connectors extend its documents. It stays
here, with the packages every app shares (`core`, `contacts`, `parties`, `products`,
`project_records`) and `ferrum-calc`, until stage 3 gives those apps a stable place to depend on.

## Stages

| Stage | What moves | Done when |
|---|---|---|
| **1. The app and its interface** | ERP's and India's manifests; ERP's interface (`ui/`, built on `@ferrum/records/frame` like CRM's, `ui/dist` committed and embedded in the `ferrum-app-erp` crate); the server depends on the crate | `/erp/` and the portal's invoices are served from the crate's interface; Ferrum-Frontend has no `apps/erp` |
| **2. The add-ons** | Finance, Procurement, Stock Operations and Manufacturing Operations, as crates of the repository's workspace with their tests | the server takes them from the ERP repository; this workspace has no `crates/finance` … |
| **3. The packages and the engine** | the five packages, India's, and the domain's ERP logic; apps that build on it depend on ERP's crate | `ferrum-domain` holds the shared core only; an upgrade step moves the packages' ownership in existing workspaces (the app id stays `erp`, so no data moves) |

Stages 1 and 2 are built (31 October 2026). Stage 3 needs its own pass: it re-points every app
that calls the engine (POS, Commerce, Assets, Bookings, Field Service, HR's and the connectors)
and moves the domain's ERP tests with the packages; its design is the next step here.

## Order of a change that spans repositories

1. Ferrum-Frontend first when an interface needs a new export (stage 1: the records frame's
   routes and home).
2. The ERP repository, whose CI builds against Ferrum's main and Ferrum-Frontend's main.
3. Ferrum, switching its dependency and removing its copies in the same commit.
4. Ferrum-Frontend's removal of what moved, then Ferrum's `FRONTEND_REV`.

An interface compiled into an app's crate wins over a copy of it in `settings.ui_dir`, so an image
built from an older `FRONTEND_REV` never serves the old ERP interface.
