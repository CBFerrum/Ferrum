# Where each profile stands

Gates are the specification's. A gate is shown as met only when everything in it has been run. Software evidence alone meets none of
G4, G5, G7 or G8, because each names something real.

| Gate | State |
|---|---|
| G0 Baseline | Met in software. Image digests and tenants could not be read from here |
| G1 Trust foundation | Tests pass. The topology has not been deployed to a cluster |
| G2 Resource lifecycle | Met for PostgreSQL. Objects were not run against S3 |
| G3 Secure source and build | Met on Docker, which is not the isolation production needs |
| G4 Static and preview | Not met: no real domain or TLS |
| G5 SSR and corporate access | Not met: no framework, no real access provider |
| G6 Distributed operations | Not met: the parts pass and have not been run together |
| G7 Operations and business service | Not met: no drill, no audit of the console, no incident exercise |
| G8 Production qualification | Not met |

| Profile | What works in software | What stands between it and a customer |
|---|---|---|
| RP-01 Commerce storefront | Commit to preview to approved production release on a serving node, with a real build and real containers | Real domain and TLS, a payment provider, a storefront against Commerce, a qualified runtime |
| RP-02 Corporate application | Access decisions, offboarding across the forge, members-only sites | A real IdP, MDM and access provider. Nothing here moves packets |
| RP-03 Specialized resources | Apps declare resources, PostgreSQL and objects provisioned, migrated, copied, restored, retained | S3, time-series or log analytics, scaling, cost |
| RP-04 Distributed charging | Connection ownership, durable billing frames and transaction numbers, evidence-based sessions | Run together on two gateways, chargers, meters, OCPP conformance |
| RP-05 Developer platform | Gitea repositories, protected branches, builds, previews, approvals, console | A registry, starters, an SDK, a qualified sandbox |
| RP-06 Customer network | Serving nodes and gateways that need nothing else to keep working | Everything else: no customer-network deployment has been attempted |
