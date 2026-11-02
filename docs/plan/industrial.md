# Industrial operations

The living plan for Ferrum as the enterprise and operational coordination layer above industrial
equipment. It answers the strategy assessment of 1 November 2026 and keeps its decisions here. The
registry (`docs/registry/industrial.yaml`) tracks each requirement.

**The boundary, from the assessment and kept here:** Ferrum never replaces a robot controller, a
PLC, a safety system or a fleet manager. It owns business truth, operational authority and
explainable recovery. A safety function never depends on Ferrum, and nothing here is a functional
safety claim. Rust's memory safety is not timing determinism, and a fast web server is not a
controller.

## What the assessment found, against the code

| Finding | Status | Where |
|---|---|---|
| Socket delivery is not a durable command channel | Stays best effort, as it says. Physical work goes through the command ledger, which never touches a socket | `crates/ops/src/commands.rs`, `crates/plugins/src/sockets.rs` |
| Socket buffers are unbounded | Closed. Each socket has a bounded queue. A sender says what overflow means: fail (the slow socket is closed and the caller told) or drop (a snapshot the next one replaces) | `crates/plugins/src/sockets.rs` |
| Event receipts do not make effects exactly once | Closed as far as it can be. The attempt is recorded before the plugin runs, so a retry after a crash is told so, with an `operation_id` that is the same on every attempt, to give the provider | `crates/plugins/src/events.rs`, `docs/plugins.md` |
| The device gateway is only an example | Stays an example, and its documentation says so. Identity is `crates/ops/src/identity.rs`: a key per device, bound to its workspace, site and allowed commands at commissioning | |
| Connection replacement | Closed. `kv.delete` takes `if_value`, so an old socket's close cannot remove the new one's mapping. The example uses it | `crates/plugins/src/host.rs` |
| Telemetry stored as business records | Closed for anything on the site runtime: measurements keep source time, receipt time, boot, sequence and quality, in day partitions outside the document engine | `crates/ops/src/telemetry.rs` |
| Plugin schedules are not an industrial scheduler | Stays as it is. Site scheduling runs on the site | `gateway/` in ferrum-app-edge |
| Private networks unreachable from `http.fetch` | Preserved. OT is reached through the site runtime, never by weakening the plugin sandbox | |
| HTTP from transactional hooks, secrets in plugin configuration | Already closed (`integrations.md`). Commands are intents written in the business transaction, so a command exists only if the document commits | `crates/ops/src/commands.rs` |

## Decisions

| Decision | Why |
|---|---|
| Three crates: `ferrum-ops-contract` (types and state machines, no I/O), `ferrum-ops` (the platform side, on PostgreSQL), `ferrum-site` (the site runtime, on SQLite, since 2 November 2026 in [ferrum-app-edge](https://github.com/CBFerrum/ferrum-app-edge)) | The site must run with no PostgreSQL and no kernel, and both sides must agree on one state machine |
| The site runtime is one Rust binary with an embedded journal | A small facility should not run a cluster. Separate services can be split out when availability, hardware access or ownership justifies it |
| The site journal is SQLite, WAL, `synchronous=FULL` | One writer, a single file to back up, survives power loss, nothing to operate |
| No message broker in the first scope | The journal plus one idempotent sync call covers store and forward. NATS JetStream stays the candidate when several consumers at a site need a stream. MQTT is an adapter, not the backbone |
| Devices sign requests with Ed25519, no shared secret | The server stores public keys only. The signed timestamp is strictly increasing per key, so a replay fails with no nonce table |
| PKI is not built | mTLS terminates at a proxy the customer runs. The key lifecycle here does not depend on one |
| Automatic failover between gateways is not built | The assessment: unsafe while command authority is ambiguous. A person transfers authority, the old holder is fenced first |
| WMS, MES and Quality are add-ons of ERP (`addons/wms`, `addons/mes`, `addons/quality`) | They extend its stock, manufacturing and inspection records. A second inventory or manufacturing app is what the assessment warns against |
| Physical positions are a subledger, not a second balance | Positions are written only with their ledger row. Crossing an ERP warehouse or changing ERP stock posts through ERP's documents, and a reconciliation reports any difference |
| Maintenance extends Assets, not a new app | Equipment, repairs and parts are already there |
| Scheduling starts with a deterministic policy | Explainable, testable and bounded. A solver is a later option behind the same inputs and only ever proposes |
| AI proposes typed plans. It never holds a credential for equipment | The ledger refuses anything an AI issued unless a person approved it or a bounded envelope covers it |
| Rust by default, not as a rule | A supported vendor library across a process boundary beats a rewrite |

## The command contract

Every command that could cause a physical effect carries the fields of `ferrum_ops_contract::Command`:
id, tenant, site, target, operation and schema version, issuer and approval, authority epoch,
expected target state, issue and expiry times, payload and its hash, workflow, step and correlation,
and configuration version.

```text
Proposed → Authorized → Queued → Accepted → Executing
   │           │           │        │          ├→ Completed
   └→ Refused  └→ Expired ─┘        │          ├→ Failed
                                    │          ├→ Aborted
                                    └→ Rejected└→ OutcomeUnknown → resolved by evidence
```

- **Accepted is not completed. Cancellation requested is not cancelled. A timeout is not proof that
  nothing happened. An acknowledged message is not a moved load.**
- Reports are idempotent by `report_id` and may arrive out of order. A late report never moves a
  command backwards; it stays in the history.
- `OutcomeUnknown` leaves only by evidence: a query of the equipment or a person's reconciliation.
  Nothing retries it automatically. A retry is a new command with a new authorization.
- Every report carries the authority epoch. One from an old holder is refused and recorded.

## The site runtime

`ferrum-site` runs next to the equipment and keeps working without the cloud, inside what was
authorized before the link went down.

- The intent to dispatch is durable before the adapter is called. After a crash a command caught
  between the two is `OutcomeUnknown`, never sent again.
- Telemetry, reports and facts wait in the journal and go up in one idempotent call. Telemetry has a
  byte budget and is shed oldest first by class; commands and execution evidence keep a reserve and
  are never shed. When even the reserve is threatened, it stops taking new work and says so.
- The lease carries a maximum offline age. Past it, no new work starts; running work is followed to
  its end and evidence is kept. The age is a setting, because this is a trade-off: a site offline
  cannot learn about a revoked person.
- A local console (no cloud needed) shows the queue and resolves unknown outcomes.
- After a restore, dispatch stays off until a person has reconciled the state against the equipment.

Adapters are small: the simulator, operator tasks, MQTT, VDA 5050 over MQTT, Modbus TCP. Each states
what it can prove, and none claims a certification. [Compatibility](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/compatibility.md) is the record of what was
tried, on what: today, simulators only.

## Gates

The assessment's gates, as they stand. Status per gate is in the registry.

| Gate | What it needs that code alone cannot give |
|---|---|
| G0 architecture authority | Done in `docs/operations-authority.md` |
| G1 secure connectivity | A customer's network and an independent review |
| G2 durable execution | Crash and replay tests here, then a soak on hardware |
| G3 offline site | A contracted duration under production-like load |
| G4 complete business flow | End-to-end scenarios here, then a site's real exceptions |
| G5 hardware interoperability | Specific models and firmware. **Nothing has been tried on hardware** |
| G6 recovery | Operator exercises with real people |
| G7 security and safety integration | A threat model here; hazard interfaces and independent review are specialists' |
| G8 operability | Runbooks here; support ownership and training are the owner's |
| G9 controlled autonomy | Measured against a baseline on real data |
| G10 release sustainability | A support policy the owner puts in force |

## What a live plant taught

On 2 November 2026 the whole chain was run on the real binary with a real site runtime: a warehouse, a
press and a simulated fleet, in a browser. Separate scenarios had all been green. It found:

- nothing brought material to a line, so a run in a managed warehouse could never post its output;
- a second hand-over sent everything verified so far again, and a shipment closed with a difference
  could be received twice;
- a real barcode never matched, and part of a name did;
- only an administrator could search an item: every test had signed in as one;
- a person's finding never reached the site, and a frozen platform looked connected for a minute;
- equipment behind a gateway could not be provisioned at all.

So: run the real thing before trusting a green suite, and not as an administrator.

## What a review of the connected system found

A source review on 2 November 2026, of the path from a business request to a device and back, found
that the foundations held and the seams between them did not. Each finding was confirmed in the code,
and more of the same kind turned up beside it. All are fixed, each with a test and a mutant:

| Finding | What it was | What holds now |
|---|---|---|
| A retried release made a second mission | Release took `Released` tasks and wrote another attempt | A release said twice is one mission, and the ledger's `op_command_one_live` index refuses a second live command for a task or run |
| Recovery chose a device by operation name | A restart could ask another device, whose "never heard of it" looked like leave to resend | What a command went out on is written with its intent, and everything after goes back to exactly that, or nowhere |
| Cancelling one vehicle's order stopped them all | `cancelOrder` went to every vehicle on the adapter | It goes to the vehicle that has the order, once that vehicle says so |
| A backward clock stretched leases | Time stayed at its high-water mark, and elapsed time stopped counting | Time is the platform's, carried by a clock nothing on the machine can set; a reboot is not believed |
| A clock that ran ahead locked out signing | Nothing could bring it back | The platform's time is believed, backwards too |
| Dispatch checks went stale in a batch | Planned once, sent later, and an expired command went with a second to live | Every command passes the gate the moment before it goes, and fencing is in the statement that writes the intent |
| An absent dependency counted as done | "Done" was what was not in the current lists | Only a recorded `Completed` satisfies one, and endings outlive their rows |
| A move verified the place and not the load | Any tote at the right bin finished the task | The load is scanned too, and a manager's word never reads as a scan |
| A reused scan could finish a task with nothing moved | A move that landed nowhere was ignored | It is refused |
| A machine could start before its material arrived | Release issued the start beside the supply request, on a quantity anyone could also count | Material is set aside on release, and a second gate starts the machine once it is at the station |
| Wrong-scan audit was rolled back | Written in the transaction that was then refused | A refusal commits what it recorded, then says no |
| Redirects could leave the allow list | The first host was checked, and `plant.example.com` matched `plant.example.com.elsewhere.org` | Every hop is checked, and an entry means the host it names |
| An envelope was a rate | Site, operation and a count, and it could stand in for a required second person | It says where, what, when and how much, covers agents only if told to, and never replaces a second person |
| Recovery forgot what a mission was for | Found stock ended the task, so an order could be left with nothing to pick it and a wave saying Picked. A run's material found at the line was not counted. The walk below found this, not the review | Found on the way, a pick or a line's supply goes on as a new task, and found where it was going it has arrived. What nobody found is on the wave's shortfall, and a shipment nothing reached closes |
| Receipts and issues could not be taken back | Cancelling one in a managed warehouse failed | A receipt goes back out by the inbound bin if it is still there, and an issue comes back in by it |

The same review left five checks on what was closed earlier. They were made on the same day:

| Asked | Found |
|---|---|
| Every external effect is refused in a hook | `http.fetch`, `sockets.send` and `sockets.close` are all that leave the system. Only the first was tested; each now has a test and a mutant of its own |
| Every integration that can command uses the ledger | Everything that reaches equipment issues through `commands::issue` or `issue_exclusive`: Edge's site, fleet, firmware, schedules and facts, Warehouse, Shop Floor and the agents' tools. On a gateway, a tag is written from one place, a command's dispatch. `plugins/examples/device-gateway` pushes over a socket and says it is an example; nothing installs it |
| Adapters use `operation_id` or reconcile | Stripe and PayPal give the provider a key that is stable per request, which holds across events as well as attempts. Razorpay has no such key for a refund, so it looks for its own note first |
| Connection management deletes conditionally | The example gateway is the only caller, and it passes `if_value`. The SDK offers both |
| Credentials: migration and leaks | Every plugin that holds one names it in its manifest, and the host keeps it out of `config`, sealed. No plugin reads a secret from `config`. A gateway's were not as safe: an HTTP failure quoted its URL, which can hold a `{secret.x}`, and the reason went to the platform. Fixed in Edge 0.4.0 |

Aggregate memory under many slow sockets stays unmeasured: each socket's queue is bounded, and their
sum has not been loaded.

The whole chain was then walked again on the real binaries, signed in as a supervisor, an operator and
a recovery person, in a browser and through the API: a run released and its material brought to the
line, wrong scans refused and kept in the audit chain, the press started only then, a pick released
three times with the gateway dead, gateways killed mid-mission and restarted with equipment that had
forgotten, a person finding the stock and settling the ledger, packing, a carrier's label and weight,
a delivery and a receipt taken back. The books reconciled at every step and no log held an error. It
found the recovery defect in the table above. A simulator stood in for the equipment.

Two things this does not change. Nothing has run on physical equipment, so a robot-commanding or
machine-starting pilot still waits on equipment, its recovery and its people (the blockers below), and no
claim of autonomy is made. And a gateway offline carries on with what it was given: it does not
originate new work, by design ([gateway guide](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/gateway.md)).

## Status

Kept in the registry (`docs/registry/industrial.yaml`), with what is and is not proven. The scale of
the assessment stands at 184 to 314 engineer-months, and most of what remains needs equipment and
people, not code: see the blockers.
