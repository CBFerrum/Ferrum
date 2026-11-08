# Profiles

What each way of running Ferrum supports for customer logic, and what it refuses. A definition that
needs more than its installation offers is refused with `UNSUPPORTED_PROFILE` and a reason, never run
on something weaker that looks the same (FLC-BASE-005).

| Profile | What it adds | Runs on | It refuses |
|---|---|---|---|
| **Core** | Fields and forms, rules on records (derive and validate, inside the record's transaction), decision tables, record workflows with four-eyes approval, today's after-commit automations | The Ferrum server and PostgreSQL, nothing else | Any definition that waits: deadlines, waiting for a reply, a person's task inside a process, retries over hours. Those need Full Process |
| **Full Process** | Core, plus durable processes: tasks for people, deadlines on business calendars, waiting for events, retries, compensation, reconciliation of unknown outcomes, the run console | Core, plus a Temporal service and Ferrum's process worker (`deploy/temporal.yml`) | A process that would run disconnected at a site: no site-local process runtime is qualified |
| **Edge** | Event pipelines at a gateway: units, quality, windows, episodes, business events out through the journal; equipment only through the Operations ledger | A `ferrum-site` gateway | Business processes at the site, and anything that moves equipment except a command asked of the ledger |
| **Intelligence** | Questions about certified metrics, answered with the asker's rights and with evidence; proposals that start an ordinary reviewed process | Core, Metrics, Studio, and a model provider the workspace's egress policy allows | Writing to records, approving, deploying, or answering what the asker may not see |

## What none of them do

- No profile is a safety function. Equipment protections stay with the equipment.
- No profile runs arbitrary code, shell, SQL or native libraries as a step. Code that a capability needs
  is reviewed once, as an app or a WebAssembly function with budgets, and reused.
- Moving a definition from one profile to another is a new release, reviewed again.

## Topology and limits

Each profile's measured limits and supported versions are recorded here as its evidence is collected
(FLC-QUAL-004). None is recorded yet.
