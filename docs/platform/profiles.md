# Where each profile stands

Gates are the specification's. A gate is shown as met only when everything in it has been run. Software evidence alone meets none of
G4, G5, G7 or G8, because each names something real.

| Gate | State |
|---|---|
| G0 Baseline | Met in software. Image digests and tenants could not be read from here |
| G1 Trust foundation | Tests pass. The topology has not been deployed to a cluster |
| G2 Resource lifecycle | Met for PostgreSQL. Objects have run against an S3 interface, not against a provider |
| G3 Secure source and build | Met on Docker, which is not the isolation production needs |
| G4 Static and preview | Not met: no real domain or TLS |
| G5 SSR and corporate access | Not met: no framework, no real access provider |
| G6 Distributed operations | Not met: run together on real gateway processes with a charger written for the test, and no equipment |
| G7 Operations and business service | Not met: the console has had automated checks only, and there has been no drill or incident exercise |
| G8 Production qualification | Not met |

| Profile | What works in software | What stands between it and a customer |
|---|---|---|
| RP-01 Commerce storefront | Commit to preview to approved production release on a serving node, with a real build and real containers | Real domain and TLS, a payment provider, a storefront against Commerce, a qualified runtime |
| RP-02 Corporate application | Access decisions, offboarding across the forge, members-only sites | A real IdP, MDM and access provider. Nothing here moves packets |
| RP-03 Specialized resources | Apps declare resources, PostgreSQL and objects provisioned, migrated, copied, restored, retained, under a region policy and a budget | A real object store, time-series or log analytics, scaling on a provider, real cost |
| RP-04 Distributed charging | Connection ownership, durable billing frames and transaction numbers, evidence-based sessions, run together across a gateway dying | Chargers, meters, OCPP conformance, two gateways listening at once |
| RP-05 Developer platform | Gitea repositories, protected branches, builds, previews, approvals, console | A registry, starters, an SDK, a qualified sandbox |
| RP-06 Customer network | Serving nodes and gateways that need nothing else to keep working | Everything else: no customer-network deployment has been attempted |
