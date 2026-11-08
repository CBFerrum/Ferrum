# Spike L-01: does Temporal hold what Ferrum's durable processes need?

8 November 2026. The qualification spike ADR L-01 asks for ([../logic/decisions.md](../logic/decisions.md)) before
Ferrum's processes are built on Temporal. The question: with Ferrum keeping every business record in its
own database and Temporal keeping only scheduling, do the cases the Logic Platform package worries
about hold on released software, run here?

What ran, all pinned:
* Temporal server 1.32.2 (`temporal_1.32.2_darwin_arm64.tar.gz`, checksum verified against the release's
  `checksums.txt`) with PostgreSQL persistence on the local PostgreSQL 17.9;
* the Temporal CLI 1.10.0 (same check) for health and namespaces;
* `spikes/temporal_logic`: a worker and probes in Rust on `temporalio-sdk` 1.1.2, with a PostgreSQL
  database standing in for Ferrum's records (effects with receipts, grants, tasks, the recovery epoch).

To repeat it, with the server's databases made by `temporal-sql-tool` (`create-database`, `setup-schema
-v 0.0`, `update-schema --schema-name postgresql/v12/temporal` and `.../visibility`) and the server started
from `config/development-postgres12.yaml` with those names:

    (cd spikes/temporal_logic && cargo build)
    SPIKE=.../temporal-logic-spike TEMPORAL_BIN=.../temporal START_SRV=<script starting the server> \
    TDB=<temporal db> TVDB=<visibility db> FERRUM_SPIKE_DB=postgres://.../<spike db> spikes/temporal_logic/run.sh

## Results: go, with five rules

Every probe holds on the fourth run ([../logic/evidence/l01-spike-run.txt](../logic/evidence/l01-spike-run.txt)). The
first three found three things the design must do, and one mistake of the probes' own.

| Probe | What ran | Result |
|---|---|---|
| One effect under repeated attempts | The provider does the work and its answer is lost; Temporal retries | The work happened once (`sent=1`); the retry got the receipt (`repeated=true`). The same key with other input: `CONFLICT` |
| Signal before wait | One reply sent three times while the process was still busy: twice with one request id, once with another | Consumed once. Temporal dropped the repeated request id; Ferrum's inbox rule dropped the business repeat |
| Timer against revocation | The grant revoked while the process slept two seconds | `PERMISSION_DENIED` when it woke; nothing sent |
| Decision against deadline | 20 tasks with a one-second deadline, each decided by the API's compare-and-set at a different moment around it | 12 decided, 8 expired; in all 20 the process followed the outcome the record holds |
| Payload codec | A record's text through a process with Ferrum's AES-256-GCM codec | The answer decodes; the history fetched without the codec does not contain it; no row of Temporal's `history_node` does |
| Replay after a change | A three-step interpreter run, its history replayed with the steps reordered | Caught (`TMPRL1100`) **only when each activity's id is its node's id**. Without that, a reordering replays unnoticed: Temporal compares activity types and ids, not inputs |
| Continue-as-new | 12 signals and a repeat into a process that continues as new every 3 | 12 consumed, 12 unique, no loss, no repeat |
| Worker crash | The worker killed (`kill -9`) after the work was done, before it answered | Temporal retried after the timeout; the receipt was returned; `sent=1` |
| Temporal restored behind Ferrum | Temporal's databases restored to before an effect Ferrum had recorded; the process asked again | The effect's key found its receipt: `repeated=true`, sent once. The next effect ran normally |
| Ferrum restored behind Temporal | Ferrum's database restored to before an effect's receipt; the restore runbook raises the recovery epoch | Temporal did not redo the finished effect, and the next one was refused with `RECOVERY_REQUIRED` and left waiting: the lost receipt is reconciled by a person, never re-sent |
| Footprint | 200 workflows at once | Server 145 MB resident at rest, 199 MB after; worker 33 to 35 MB; Temporal's databases 18 MB and 26 MB; all finished in 1.7 s |

Building it: the SDK and its dependencies compiled in 78 s on this machine and took 0.9 GB of target
(debug, no debug info); the spike's binary is 73 MB. `temporalio-protos` runs `protoc` at build time
(installed here), or its `vendored-protox` feature avoids that.

### What the design must do

1. **Every activity's id is its node activation's id.** Otherwise a changed interpreter replays
   unnoticed. Release pinning stays the main defence; this makes replay a real check of it.
2. **One codec everywhere.** The client that starts, the worker that runs and the tool that replays
   all carry Ferrum's codec; a history it sealed cannot be replayed without it, so replay tooling and
   key recovery are part of the deployment.
3. **Signals go to the workflow, never to a run.** A handle from starting a process is bound to its
   first run, which is finished once it continues as new.
4. **Effects are Ferrum's.** A logical key per node activation (never per attempt), an input digest,
   and the receipt checked before anything is sent. Temporal's retries then cannot duplicate work,
   and neither can either side's restore.
5. **A restore of Ferrum's database raises the recovery epoch.** Effects whose intent predates it
   wait for reconciliation. The spike also showed the fence is strict: intents left from a drill were
   refused in the next run until they carried the current epoch.

### What the spike did not show

One server process in its development layout, without TLS, high availability or an online backup
(the restores were `pg_dump` and `pg_restore` with the server stopped). PostgreSQL 17 took Temporal's
schema (1.19, visibility 1.14) and ran every probe, but Temporal's own documentation must list it
before production. Load was 200 small workflows, not a capacity test. Nothing ran on a customer's
data.
