# Hardening: what running it for real found

W10 said what had not been done. This is what happened when it was. Revisions: `ferrum-app-cloud` `w10-hardening`, Ferrum
`platform/w10-hardening`, `ferrum-app-edge` `edge/journey`.

The point of writing this down is the column on the right. Most of these were found by something other than a test
written to find them, and each would have reached production with every suite green.

## Faults found

| Fault | What it would have done | Found by |
|---|---|---|
| A node's freshness moved only with the generation, and nothing published on a quiet day | A members' site stopped an hour after the last release, with everything healthy | Reading the node while adding key rollover |
| A generation went back with the database | After any restore, nodes refused every new snapshot as stale | Asking what a rewind must not undo |
| A cursor from before a restore pointed past everything | A listener silently missed events until the numbers caught up | The same |
| `publish` read routes before taking its lock | A slow publish put back what a quick one had replaced | Reading it |
| `mark_moved` left a command alone if the gateway had not confirmed it | A gateway dying between sending and confirming, and **a charger started twice** | J07, on real gateways |
| A site's request lost its trace in a spawned task | Nothing an app's route wrote named a request: no outbox event and no operation | Reading the rows of a real run. Every test passed, because tests call the site directly |
| opendal's HTTP transport was only installed when the server's own files were on S3 | **Every read of a Cloud store on S3 failed** | The S3 test, first run against moto in CI |
| The Deployment set `replicas` under its autoscaler | Every apply during a peak cut the API to three replicas | Writing the manifest check |
| `CloudOperationsStuck` went by a count | A busy system looked like a stuck one, and paged someone | Writing the alert's test |
| Three hook mutants had not found their text since W02, and nothing checked | Nothing tested that a hook cannot call out | Adding `--check` to Ferrum's script |
| A stopped project could still write a release plan | Refused only when run | `deploy_it` |
| Two events on one project had the same revision | The second restriction failed | `control_it` |
| A copy refused for good stayed pending for ever | Nothing said it had failed | Reading the copy path |
| The console: a second `main` landmark on every page, faint text at 2.87:1, no heading on an environment's page | Worse for anyone using a screen reader or with low vision | axe against the running console |

## Mutants

At `449c173`, the full set: 172 of 179 caught. One survivor was a mutant that had stopped meaning what it said. Six were real gaps
in the tests, each closed here: a reader passing open work, a position passing open work, an unproved domain being routed, the
management plane being reachable, a leaver reaching a resource, and an unenrolled connector carrying traffic. The results for the
mutants added since are in `FCP-QUAL-007`.

## A real run

`ferrum start` and `ferrum task cloud_controller` as separate processes on a fresh database, through real HTTP. Residency refused
with its reason, a real database provisioned and kept through a closure, `cloud_restrict`, an appeal and `cloud_lift`, a provider
notice with the resources it affects, and every page of the console in a browser: sign-in with a second factor, the step-up form
appearing where a person was stopped with what they had written still there, approval by someone else, and the closure running.
axe finds nothing of Cloud's on any page audited, with the step-up form open as well. The one finding left is the shell's skip
link. No page scrolls sideways at 375 pixels. That is automated checking and one pass by a machine, not a review by anyone who
relies on assistive technology.

## J07

`gateway/tests/charger_it.rs`: two API servers on one database, real `ferrum-site` processes that are killed, and a charger written
for the test that speaks OCPP 1.6. It runs in CI. No charger made by anyone was involved, and on one machine two gateways take turns
with one port.

## Figures

An Apple M1 Pro with 8 cores and 16 GB, PostgreSQL 17 with default settings, on one laptop, with the load generator on the same
machine. They say what this did there, and nothing about a deployment.

A serving node, release build, a 16 KB static page, 30 seconds at each level:

| Clients | Requests/s | p50 ms | p95 ms | p99 ms | Max ms | Failed | Memory |
|---|---|---|---|---|---|---|---|
| 1 | 12,656 | 0.07 | 0.11 | 0.14 | 4.6 | 0 | 5 MB |
| 8 | 41,193 | 0.19 | 0.29 | 0.35 | 2.4 | 0 | 7 MB |
| 32 | 53,172 | 0.53 | 1.05 | 2.11 | 48 | 0 | 10 MB |
| 128 | 54,339 | 2.08 | 4.22 | 6.69 | 215 | 0 | 18 MB |
| 256 | 53,593 | 4.06 | 9.94 | 16.95 | 131 | 0 | 29 MB |

Back serving 105 ms after a restart, from its own disk.

Durable work, debug build, so a floor: 5,400 operations across eight workspaces, one with twenty times the backlog of the others.

| Workers | Admitted/s | Drained/s | Claim p95 ms | Claim p99 ms | Quiet workspaces done | All done |
|---|---|---|---|---|---|---|
| 1 | 5,491 | 1,058 | 9.2 | 9.9 | 1.8 s | 5.1 s |
| 4 | 5,455 | 2,775 | 12.6 | 13.5 | 0.7 s | 1.9 s |
| 16 | 5,712 | 4,346 | 34.7 | 44.0 | 0.5 s | 1.2 s |

The quiet workspaces finishing first is the point: a loud one does not make them wait behind its backlog.

Recovery, one database with 200,000 events and a 6.5 MB dump: `pg_dump` 0.57 s, `pg_restore` 1.75 s, a server starting on it 54 ms,
one workspace taken back 7.6 s. That is the mechanism on a small database. It is not a recovery objective: no zone, provider or
network failed, and a dump of a real database takes as long as it takes.

## Still open

- Nothing has met a real provider, identity provider, DNS, certificate, KMS, charger or meter.
- Sustained means minutes here. Nothing has run for days, so nothing is known about memory growth, or anything slower than that.
- Alerts fire under `promtool` and nothing routes them anywhere.
- A person has not followed a runbook, run a tabletop, or reviewed accessibility by hand.
- Nobody independent has reviewed any of this.
