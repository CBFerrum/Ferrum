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
| **Cloud ignored a workspace restricted for billing** | A workspace that had stopped paying could go on building and provisioning | Asking what a restriction does in Cloud, for AT-CLOUD-007 |
| Support keys were checked by one extractor of several | A health-only key opened a live socket and reached browser-session routes, and no refused attempt was recorded | Reading every way a token becomes a caller, for AT-CLOUD-003 |
| A mutant meant for `mark_moved` changed `mark_in_doubt`, whose text it matched first | It survived, and showed nothing tested a command that was sent and never confirmed when authority moves | The mutant surviving |
| Two controllers announcing a provider notice at once | The one that lost the race could have told everyone a second time | The notice mutant surviving |
| Support grants had an API and no page | A customer could not see who had access without calling the API | Checking AT-UI-008 in a browser |
| A refusal said why and not what to do | Someone without the role had no way forward | A viewer trying it in a browser |
| On a phone, a plan was wider than the screen, and a history table could not be scrolled from the keyboard | Both pages unusable in part | axe and a keyboard walk at 375 pixels |
| **Retiring a resource dropped a database and roles by name** | Where the provisioner had the rights, it could destroy what someone else had made under that name | Closing a whole workspace, which retired a resource whose name had been taken |
| Settling an operation left its resource or repository where it was | A resource a person settled could never be retried, closed or deleted | The same, waiting for a resource nobody could vouch for |
| The shell added a second banner and skip link under the platform's bar, and its closed drawer and the bar's skip link sat off the left edge | A second banner for screen readers; right to left, a page ten thousand pixels wide | axe with the frame rebuilt, and right to left at 375 pixels |
| `delete_tenant` read the workspace through the pool | It found nothing behind row-level security, and stopped after every check had passed | Running the task against a throwaway server |
| A mutant for a replica being down matched the WebSocket tunnel's line first once it existed | It stopped testing requests | The mutant surviving |

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

## The second round

`scripts/support_check.py` against a real server: a health-only key reaches its report and nothing else, on every kind of route,
and every attempt is on the customer's list, marked refused or let in. In a browser against real processes: an operation
followed across a stopped controller, a provider refusing and coming back, a reload and a second session (AT-UI-002); a
viewer told why and what to do (AT-UI-005); a partner's grant reviewed and ended (AT-UI-008); the main pages worked by
keyboard alone. A billing restriction applied with the operator's task refused new work and left reading alone.

`tests/drill_it.rs` loses the database, the forge, the artifacts and their store at once and brings them back somewhere new
with the key from where an operator keeps it. The runbook said a forge restored elsewhere refuses pushes until its hooks are
rewritten; the drill pushed without that, so the runbook now calls it a precaution.

## The third round

WebSockets go through a serving node (`serve/tests/serving_it.rs`), with the pass, identity headers and a per-site limit as
for requests. A workspace can be closed, kept until a date and deleted by an operator: `crates/kernel/tests/closure_it.rs`
for the kernel, the end of `tests/resources_it.rs` for Cloud, and the tasks run against a throwaway server, where one was
broken. The shell's findings are fixed in Ferrum-Frontend#9 (not merged) and `server/shell/runtime.js`.

## The fourth round

Alerts reach an owner (Cloud's `deploy/k8s/alertmanager.yaml`): every critical one goes to on call as well, and a held restore or
a fenced region holds back the backlogs it causes. `scripts/check_alerts.py` fires each rule under promtool, routes each under
amtool, and sends a cause and its consequences through a real Alertmanager to receivers of its own. Checking the rules against
AT-OBS-004 found that nothing alerted on expiry: now a console, an old login, a repository credential or a preview still live ten
minutes past its time pages, and approved terms a month from their end and a certificate cert-manager has not renewed warn.
It also found every database-derived rule raised once per server replica, with the counts multiplied; they read the highest
value now, and a test with two replicas holds them to it.

Each workspace has a share of each replica (`FERRUM_TENANT_REQUESTS`, by default half the database connections): its requests
take turns among its own, a response says how long it waited, and one that waits five seconds is refused and counted.
`scripts/share_check.py` floods one workspace while another is timed (figures below, with the share off for comparison).

Edge's charger and MQTT ports let new connections in at a steady rate, so after an outage the ones already in are not held up
behind everyone's handshake, and a charger's stalled handshake is closed after ten seconds instead of holding its place.

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

A gateway killed with a hundred chargers on it, which all call the next one at once: every charger answered in 125 ms, all
known to the platform in 1.4 s, work asked for in the middle of it through in 2.3 s, and nothing sent twice.

The same hundred chargers let in at 25 a second: all back in 3.3 s, known to the platform in 4.4 s, waiting work through in
5.5 s, and a charger already in had its heartbeat answered in 0.2 ms while the rest came in.

One workspace flooding from 64 threads, another reading 100 contacts 500 times, 10 database connections, debug build:

| | The other alone, p50 / p95 | During the flood, p50 / p95 | The flood |
|---|---|---|---|
| Share on | 8.8 / 13.2 ms | 12.6 / 24.8 ms | 3,366 requests in 8.6 s, a median 128 ms waiting for a turn, none refused |
| Share off | 8.3 / 14.1 ms | 116 / 145 ms | 32,667 requests in 61.7 s |

The whole-profile drill: backed up in about half a second, back and checked in 4.5 s, with a tiny database and one repository.

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
- Alerts route to stand-in receivers. No real pager or chat has received one.
- A workspace's share is per replica, and the database's CPU is not divided between workspaces.
- A person has not followed a runbook or run a tabletop. Nobody has used the console with a screen reader.
- The shell's own findings are fixed in Ferrum-Frontend#9, which has not been merged.
- Nobody independent has reviewed any of this.
