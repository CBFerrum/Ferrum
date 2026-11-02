# IoT Edge

Ferrum's way of reaching equipment and looking after it, under the thesis of
[the industrial plan](industrial.md): Ferrum coordinates above equipment, and never replaces its
controllers, fleet managers or safety systems. Nothing here has run on physical equipment. Status
lives in the registry (`docs/registry/industrial.yaml`, FERRUM-IND-022 onward).

## What it is

**Definitions** ([guide](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/definitions.md)) say a device as data, so that supporting one is a few
lines of YAML. **Gateways** run `ferrum-site` on location: it reads, decides, runs commands within its
lease and keeps everything in its journal through an outage. **Flows** ([guide](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/flows.md)) are
logic drawn on a canvas, simulated and reviewed, run at the gateway. **The app** ([ferrum-app-edge](https://github.com/CBFerrum/ferrum-app-edge)) holds
all of that as records and tells each gateway what to be, over a second signed call, so that Operations
stays independent of it.

## Server to server

| Between | How |
|---|---|
| A gateway and the platform | Signed exchange. A gateway runs on what it last heard |
| A gateway and a box in a cabinet | `peers`: one more adapter over a signed link, with no cloud. A command is answered from what the box kept, so a retry cannot start work twice. One hop only |
| A closed network and the platform | `relay`: passes only what gateways use, unchanged, since the signature covers it |
| A gateway and a historian, a broker, someone's cloud | [Exports](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/gateway.md#exports), held until delivered, within a budget: HTTP, MQTT, Sparkplug, NATS, Kafka, AMQP, InfluxDB, ClickHouse and the clouds |
| Another system and a gateway | `webhook://`, or a definition that polls it |
| A device and the platform with no gateway | `POST /api/site/{tenant}/edge/ingest`, with a token |

## What was decided, and why

- **Edge has a repository of its own**, [ferrum-app-edge](https://github.com/CBFerrum/ferrum-app-edge), like every other app
  (apps-ui.md §10.5): the app, the gateway (`ferrum-site`) and what they share (`ferrum-edge-contract`).
  Commands, identity, telemetry and the sync call stay in Operations, here, because ERP and Assets build
  on them too, and the gateway takes `ferrum-ops-contract` from this repository. It began here, in
  `crates/edge`, and moved on 2 November 2026 with its history.
- **`managed` is off by default**, because it lets a platform administrator decide what a gateway
  connects to. What is on the machine wins over what the platform sends.
- **Rules and flows move nothing.** A flow can ask, as automation, and the ledger decides.
- **A flow is a list of known steps and not a script**, so that it can be reviewed and simulated.
- **The same code simulates and runs**, in `ferrum-edge-contract`, so that what a reviewer saw is what runs.
- **No broker in the binary.** rumqttd 0.20 was tried: it panics when a message with properties
  reaches a 3.1.1 subscriber, which takes its listener down. Mosquitto beside the runtime is sturdier,
  and a fault in one leaves the other standing (`deploy/mosquitto.conf`).
- **OPC UA in the process, through async-opcua**, because a definition should read it as it reads
  anything else. A vendor's stack belongs behind a process boundary, as an external driver.
- **Peers are one hop**, so that nobody can build a loop. **Full Ferrum-to-Ferrum federation** is not
  built: two platforms exchange data through exports and ingest, as any two systems would.
- **A gateway's own updates are signed. Device firmware reaches sensors and meters only**, whose
  definition allows it, in rollouts through the ledger. Robot, PLC and safety firmware stay with their
  makers, whatever a definition says.
- **A definition cannot write by polling, or read a gateway's files.** Writing is a command, and a site
  names the one folder a definition may read.

## What is left

ferrum-app-edge's [parity record](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/parity.md)
says what each reference has and where this stands. In order of what would matter first: trying
definitions on real equipment and recording it in
[compatibility](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/compatibility.md), DTLS for
devices off a private network, DNP3 and DLMS, and an OPC UA server.
