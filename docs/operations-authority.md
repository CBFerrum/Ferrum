# Operational authority

Who may command what, and how that is known. This is gate G0 of the assessment, and the rules the
ledger enforces. Code: `crates/ops/src/authority.rs`, `crates/ops/src/commands.rs`.

## Who owns what

| Layer | Owns | Never |
|---|---|---|
| Enterprise apps (ERP) | Business records: orders, stock on the books, money | Tells equipment anything |
| Operational apps (WMS, MES, Quality, maintenance) and the site runtime | Tasks, runs, holds, exceptions, the command ledger | Moves a robot itself, generates controller logic |
| Fleet managers, WCS, SCADA, controllers | Execution: routes, traffic, sequences | Decides business truth |
| Independent safety systems | Safety | Depends on Ferrum for anything |

## One writer

Each **domain** (by default a site's code) has one holder, a gateway, recorded in `op_authority` with
an **epoch**: a number that only rises. Only the holder is sent commands, and each command carries
the epoch it was issued under.

- A report from another epoch joins the history and changes nothing.
- There is no last write wins for stock, execution, money or safety configuration.
- There is no automatic failover. The assessment is explicit: failing over while authority is
  ambiguous is the unsafe case. A person moves authority.

## Moving authority

`POST /api/app/ops/authority/transfer` (Device Administrator), or **Move…** on a site in Operations.

1. The epoch rises, in one transaction with the new holder.
2. Everything that may have started under the old epoch becomes `OutcomeUnknown`.
3. The old holder is **fenced** at its next exchange. It stops dispatching and says so at its console.
4. A person looks at the equipment and says what happened to each command.

A fenced gateway that never hears, because it is cut off, stops at the end of its lease.

## The lease

Every exchange renews a lease, limited by the site's `max_offline_hours`. Past it a site starts
nothing new; running work is followed to its end and its evidence kept. This is the bound on how long
a revoked person stays on a console's list, and on how long a site can act on an old picture.

## What a command may claim

States say only what was established. Accepted is not completed. A stop requested is not stopped. A
timeout is not proof nothing happened. `OutcomeUnknown` leaves only by evidence: the equipment
answers, or a person records what they found, on the platform or at the console. Either way the other
side hears. A second, different ending does not overwrite the first: it stands, and the command is
flagged for a person. Nothing retries on a guess; doing it again is a new command, with a new
authorization.

## Who may do what

| Act | Role | Note |
|---|---|---|
| See it | Ops Viewer | |
| Issue, stop | Ops Dispatcher | A stop is a request unless it never left |
| Approve, refuse | Ops Manager | Anything marked as needing approval, and anything an agent proposed |
| Reconcile | Ops Recovery | Needs what was found, in words |
| Enroll, change lifecycle, move authority, set PINs | Device Administrator | |

Agents hold none of these. They read, and they propose; a proposal runs only when a person approves
it (`crates/ops/src/mcp.rs`).

## The chain from a business event to a machine

`WMS task` or `MES run` → `op_command` in the same transaction as the business change → site journal,
durable before the adapter is called → the adapter → what the equipment says, as reports → the
business record, through `ops.command.*` events. Each step is repeatable and none assumes the last
reached its end. Stock moves only on a scan or an explicit finding: a robot arriving moves nothing.
