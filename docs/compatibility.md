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
| Protocol version | None | None | MQTT 5 through `rumqttc` | 2.0 and 2.1. Others are refused. For 3.0.0 see the template under Definitions | Modbus TCP through `tokio-modbus` |
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
| Excluded | Nothing physical happens | | Sparkplug B, which a definition reads | Interaction with a vehicle's own safety; instant actions beyond cancel | Generating controller logic. Coils and registers not named |
| Maintainer | Ferrum | Ferrum | Ferrum | Ferrum | Ferrum |
| Support ends | With the release line | With the release line | With the release line | With the release line | With the release line |

## Definitions

A definition is an adapter written as data ([guide](edge-definitions.md)), so its promises are one
engine's, and what varies is the transport underneath. All of these are *Customer validation
required* per device model, and the templates in `crates/edge/templates` are starts, not support.

| Transport | Tried against | Not tried |
|---|---|---|
| TCP, UDP, serial, HTTP, WebSocket | Real sockets and a pseudo-terminal, with stand-ins (`crates/site/tests/adapters_it.rs`) | Real serial hardware, RS-485 timing, any vendor's device |
| MQTT 5 | rumqttd 0.20 and a stand-in. TLS is not exercised | Mosquitto, EMQX, HiveMQ, reconnect storms, MQTT 3.1.1 brokers |
| Modbus TCP and RTU | A stand-in controller, and a stand-in on a pseudo-terminal with two units | Any PLC, a real RS-485 bus, CRC errors on the line |
| OPC UA | async-opcua 0.19's own server: None, and Basic256Sha256 with SignAndEncrypt, users, and server trust by certificate (`crates/site/tests/protocols_it.rs`) | Any commercial server, a certified stack, certificates issued by a plant authority |
| Sparkplug B | Payloads from this repository's own encoder, checked against a hand-worked byte sequence | Any real edge node or host. It is not a primary host |
| MTConnect | A document shaped like an agent's | A real agent or machine tool |
| Listeners and webhooks | Real sockets | Load, and devices that reconnect abruptly |
| External drivers | A shell script | Any vendor's library |

| Template | Written from | State |
|---|---|---|
| VDA 5050 3.0.0 | The repository's JSON schemas, decoding a state shaped like them | No vehicle, no master control, no vendor simulator |
| Open-RMF | rmf-web's API server models | No running RMF |
| openTCS | Web API v8's OpenAPI document | No running kernel |
| rosbridge | The protocol's documentation, from memory of its operations | No running rosbridge. Check the action operations against your version |
| MiR | Public knowledge of its REST API | No robot. No copy of the vendor's documentation was read |
| Universal Robots dashboard | The driver's documentation of the dashboard client | No arm |

Measured on an Apple M1 Pro, in release, with `crates/ops-contract/tests/load.rs`: decoding alone,
369,000 JSON messages a second with three values each, 1,040,000 Modbus reads, 12,000 Sparkplug
messages of twenty metrics, and 269,000 readings a second through a five-step flow. That is what those
two steps cost. **What a gateway sustains is not measured**: it depends on its disk and network.

## Known unsupported, for now

| What | Why, and what it would take |
|---|---|
| Siemens S7, EtherNet/IP, ADS, BACnet, KNX, CANopen, PROFINET, other fieldbuses | No driver ships. An external driver wrapping a vendor's or PLC4X's stack is the likely route |
| OPC UA browsing, events and conditions, history | Not built. Node ids come from a person with a client |
| Sparkplug primary host | No STATE, no rebirth requests, no sequence checks. Reading works while nodes are alive and have sent a birth |
| ROS 2 by DDS directly | Through rosbridge. A vendor-qualified adapter as a process of its own is the rule |
| EPCIS, AAS | Import and export, not built |
| ThingsBoard, Siemens, Rockwell, AVEVA, Ignition, Dematic, Blue Yonder, Manhattan | Coexist and integrate per contract. There is no universal adapter and none is claimed |

## Unresolved safety and compliance questions

Ferrum sits above equipment, so these belong to the integrator and the equipment's maker:

- whether a given operation may be started remotely at all (IEC 62443, ISO 10218, ISO 3691-4, ISO 13849);
- what an automation envelope may allow at a given site;
- what a site must do when a mission's outcome is unknown and a person is near the equipment.

A safety function never depends on Ferrum.
