# IoT Edge

Ferrum's way of reaching equipment and looking after it, under the thesis of
[the industrial plan](industrial.md): Ferrum coordinates above equipment, and never replaces its
controllers, fleet managers or safety systems. Nothing here has run on physical equipment. Status
lives in the registry (`docs/registry/industrial.yaml`, FERRUM-IND-022 onward).

## What it is

**Definitions** ([guide](../edge-definitions.md)) say a device as data, so that supporting one is a few
lines of YAML. **Gateways** run `ferrum-site` on location: it reads, decides, runs commands within its
lease and keeps everything in its journal through an outage. **Flows** ([guide](../edge-flows.md)) are
logic drawn on a canvas, simulated and reviewed, run at the gateway. **The app** (`crates/edge`) holds
all of that as records and tells each gateway what to be, over a second signed call, so that Operations
stays independent of it.

## Server to server

| Between | How |
|---|---|
| A gateway and the platform | Signed exchange. A gateway runs on what it last heard |
| A gateway and a box in a cabinet | `peers`: one more adapter over a signed link, with no cloud. A command is answered from what the box kept, so a retry cannot start work twice. One hop only |
| A closed network and the platform | `relay`: passes only what gateways use, unchanged, since the signature covers it |
| A gateway and a historian, a broker, someone's cloud | Exports, by HTTP with an HMAC or MQTT, held until delivered, within a budget |
| Another system and a gateway | `webhook://`, or a definition that polls it |
| A device and the platform with no gateway | `POST /api/site/{tenant}/edge/ingest`, with a token |

## What was decided, and why

- **Edge lives in the Ferrum repository**, in `crates/edge`, because it moves with Operations.
- **`managed` is off by default**, because it lets a platform administrator decide what a gateway
  connects to. What is on the machine wins over what the platform sends.
- **Rules and flows move nothing.** A flow can ask, as automation, and the ledger decides.
- **A flow is a list of known steps and not a script**, so that it can be reviewed and simulated.
- **The same code simulates and runs**, in `ferrum-ops-contract`, so that what a reviewer saw is what runs.
- **No broker in the binary.** rumqttd 0.20 was tried: it panics when a message with properties
  reaches a 3.1.1 subscriber, which takes its listener down. Mosquitto beside the runtime is sturdier,
  and a fault in one leaves the other standing (`deploy/site/mosquitto.conf`).
- **OPC UA in the process, through async-opcua**, because a definition should read it as it reads
  anything else. A vendor's stack belongs behind a process boundary, as an external driver.
- **Peers are one hop**, so that nobody can build a loop. **Full Ferrum-to-Ferrum federation** is not
  built: two platforms exchange data through exports and ingest, as any two systems would.
- **Updates are the gateway's own software only.** Robot, PLC and safety firmware stay with their makers.

## What is left

See the gap table in [references](../references/edge.md). In order of what would matter first: trying
definitions on real equipment and recording it in [compatibility](../compatibility.md), history charts
on a device, flows for devices with no gateway, and Sparkplug primary host duties.
