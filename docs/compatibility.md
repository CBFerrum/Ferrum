# Compatibility

What the site runtime has been tried against, in the fields a qualification needs. **Nothing here has
run on physical equipment.** A row says what was exercised, on what, and what remains: support for a
protocol is not support for a device, and no entry is a functional-safety or vendor qualification.

Levels, in the assessment's words: *Known supported*, *Known unsupported*, *Conditionally supported*,
*Vendor-dependent*, *Customer validation required*, *Unresolved safety or compliance question*.
Certification labels (Community, Security-reviewed, Operationally verified, Vendor-qualified) are
earned per device model and firmware. None is held yet.

## Adapters

| | Simulator | Operator tasks | MQTT 5 | VDA 5050 | Modbus TCP |
|---|---|---|---|---|---|
| Level | Known supported, for rehearsal only | Known supported | Conditionally supported | Customer validation required | Customer validation required |
| Equipment | None. A fleet in a file | A person at the site console | Any device with MQTT 5 and a status topic | Vehicles and master controls speaking VDA 5050 | Controllers exposing registers |
| Protocol version | None | None | MQTT 5 through `rumqttc` | 2.0 and 2.1. Others are refused. 3.0.0 is not implemented | Modbus TCP through `tokio-modbus` |
| Reads | Battery, state | None | Topics named in the site's configuration | `state` topic | Registers named in the configuration |
| Writes | Missions | Tasks | Commands on the device's topic | Orders | Only registers named per operation |
| Authentication | None needed | PIN from the platform's list | Username and password; TLS by the broker's listener | As MQTT | None: Modbus has none. It belongs on a segmented network |
| Licences | Ferrum's | Ferrum's | MIT / Apache | As MQTT; the VDA specification is public | BSD-style |
| Maximum load | Not measured | Not measured | Not measured | Not measured | Not measured |
| Offline from the platform | Keeps running within the lease | Keeps running, at the console | Keeps running within the lease | Keeps running within the lease | Keeps running within the lease |
| Cancellation | Immediate | The person is told | A request on its topic, confirmed only by a status | `cancelOrder`, confirmed only by state | Refused. Stop it at the machine: a stop over Modbus would look like one without being one |
| Retry | Never automatic | Never automatic | Never automatic | Never automatic | Never automatic |
| After a crash | Asks the file, or with `cannot_find` it cannot ask | The task is still there | Cannot ask. The outcome is unknown until a status arrives | Cannot ask. Unknown until `state` names the order | Cannot ask. Unknown until the status register says |
| Tested by | `crates/site/tests/recovery_it.rs`, killing the real binary | The same | `crates/site/tests/adapters_it.rs`, real sockets against a stand-in broker | The same, against a stand-in broker | The same, against a stand-in controller |
| Not tested | | | Any real broker under load, TLS, reconnect storms | Any vehicle, any master control, any vendor simulator | Any controller, any gateway, word order on real devices |
| Excluded | Nothing physical happens | | Sparkplug B | Interaction with a vehicle's own safety; instant actions beyond cancel | Generating controller logic. Coils and registers not named |
| Maintainer | Ferrum | Ferrum | Ferrum | Ferrum | Ferrum |
| Support ends | With the release line | With the release line | With the release line | With the release line | With the release line |

## Known unsupported, for now

| What | Why, and what it would take |
|---|---|
| OPC UA | No adapter is built. The Rust clients add a large dependency and need testing against a certified server; a vendor's library across a process boundary is the likely route |
| ROS 2, Open-RMF | Integrate through a vendor-qualified adapter, as a process of its own. One traffic authority per domain stays the rule |
| Sparkplug B | Reading it needs its birth and death handling. MQTT's generic topics cover plain telemetry |
| Fieldbuses, MTConnect, EPCIS, AAS | Partner adapters, or import and export. MTConnect would be read-only |
| ThingsBoard, Siemens, Rockwell, AVEVA, Ignition, Dematic, Blue Yonder, Manhattan, MiR | Coexist and integrate per contract. There is no universal adapter and none is claimed |

## Unresolved safety and compliance questions

Ferrum sits above equipment, so these belong to the integrator and the equipment's maker:

- whether a given operation may be started remotely at all (IEC 62443, ISO 10218, ISO 3691-4, ISO 13849);
- what an automation envelope may allow at a given site;
- what a site must do when a mission's outcome is unknown and a person is near the equipment.

A safety function never depends on Ferrum.
