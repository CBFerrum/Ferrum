# IoT Edge against the reference products

What each product does that matters here, and where Ferrum stands. Read from the copies in
`referance/iot`, `referance/robotics`, `referance/protocols` and `referance/brokers`, for what they do
and not how: no code or text is copied. **Nothing in the Ferrum column has run on physical equipment.**
The full account, capability by capability, is in ferrum-app-edge's
[parity record](https://github.com/CBFerrum/ferrum-app-edge/blob/main/docs/parity.md), and this is the
short form of it.

Statuses: **Yes** is built and tested. **Partly** says what is missing. **By design** is left out on
purpose. **No** is a gap.

## Connecting to equipment

| Capability | Who has it | Ferrum |
|---|---|---|
| Describe a device as data | EdgeX device profiles, OpenRemote agent links, ThingsBoard connectors | Yes: definitions, tried on a sample, published to gateways |
| MQTT 5, TLS, client certificates | All | Yes. Tested against rumqttd and a stand-in |
| HTTP polling, WebSocket, raw TCP and UDP, serial | OpenRemote, EdgeX | Yes |
| Equipment calling in, TCP, UDP, webhooks | ThingsBoard transports, OpenRemote | Yes, on ports the site names |
| Modbus TCP and RTU, shared bus, word orders | Most | Yes. Tested on a stand-in and a pseudo-terminal |
| OPC UA client: watch, read, write, method, encryption, users | Ignition, Kura, PLC4X, Azure IoT Operations | Yes, against async-opcua's own server. **Not** browsing, events and conditions, or history |
| Sparkplug B | Ignition, EMQX, HiveMQ | Yes: primary host and edge node, over rumqttd. No other Sparkplug software has met them. **Not** datasets or templates |
| MTConnect | Ignition modules, Fanuc gateways | Yes, read-only: everything, then the sample stream, against a stand-in |
| Binary frames with checksums | OpenRemote filters, custom drivers elsewhere | Yes, by definition |
| Siemens S7, EtherNet/IP, ADS, BACnet, KNX, IEC 60870-5-104, M-Bus | PLC4X, Ignition, OpenRemote | Yes, built in, by name. All but KNX met someone else's stack. No controller |
| CANopen, Bluetooth | ThingsBoard Gateway, Kura | External drivers that ship with the app. **PROFINET, EtherCAT, DNP3 and DLMS: No** |
| CoAP, LwM2M, SNMP, OCPP | ThingsBoard, Hono | Yes. Met aiocoap, Leshan, PySNMP and the Python `ocpp` library. **No DTLS, no SNMP v3** |
| LoRaWAN, Zigbee, Sigfox | ThingsBoard, OpenRemote, Hono | Through ChirpStack or Zigbee2MQTT over MQTT, with templates. Direct radios: No |
| Any other protocol | Process drivers in EdgeX and Kura | Yes: JSON lines in any language, as a process of its own |
| Discovery of devices on a shared connection | EdgeX provision watchers | Partly: strangers are noticed and a person claims them. No network scanning, on purpose |

## Robots and fleets

| Capability | Who has it | Ferrum |
|---|---|---|
| VDA 5050 | Master controls | Yes: a built-in adapter for 2.0 and 2.1, and a definition for 3.0.0. Interaction with a vehicle's safety is not touched. `zoneSet`, `responses` and map distribution are not used |
| Open-RMF | rmf-web | Yes, as a client of its API server: tasks, states, fleets. One traffic authority per domain stays the rule. Interrupt, kill, rewind and skip are not offered |
| openTCS | openTCS kernel | Yes, by web API v8. **Not** its event stream: it polls |
| ROS 2 | rosbridge, zenoh-ros2dds, rclrs | Through rosbridge: topics, services, action goals. Direct DDS: No, and it belongs in a vendor-qualified adapter |
| Mobile robot vendors | MiR, others | MiR's REST API as a template. Others by definition |
| Robot arms | UR driver, MoveIt | Playing a program already on the robot, through its dashboard. Motion planning: By design, never |
| Traffic, path planning, collision avoidance | Nav2, RMF, fleet managers | By design, never: they own execution |
| Fleet on a map | RMF dashboard | Yes: where robots say they are, for people to see |

## Logic at the edge

| Capability | Who has it | Ferrum |
|---|---|---|
| Thresholds with hold and clear | ThingsBoard alarm rules, OpenRemote | Yes: rules, at the gateway, offline |
| Flow builder on a canvas | Node-RED, ThingsBoard rule chains, OpenRemote flows, StreamPipes | Yes: 44 kinds of step, watched live, at the gateway offline or on the platform |
| Simulation before it runs | ThingsBoard's test script, partly | Yes, by the code a gateway runs, required before review |
| Review by someone else, staged activation | Rarely anywhere | Yes |
| Scripts in a flow | Node-RED function, ThingsBoard TBEL | By design: a flow has no code, so it can be reviewed |
| Loops in a flow | Node-RED | By design: refused |
| Flows acting directly on equipment | Node-RED, ThingsBoard | By design: a flow asks the ledger |
| Rule engine on the platform, for devices with no gateway | ThingsBoard | Yes: flows on the platform, a closed minute at a time |
| Enriching from what is known in a flow | ThingsBoard | Yes |
| Machine learning at the edge | Greengrass, Azure, StreamPipes | A statistical step, and external drivers for a model. No ONNX runtime is built in |

## The gateway

| Capability | Who has it | Ferrum |
|---|---|---|
| Works offline, store and forward | ThingsBoard Edge, Greengrass, Azure IoT Operations (72 h), Kura | Yes, within a lease and a disk budget, tested by killing the real binary |
| Managed from the platform | All | Yes, opt in by `managed: true`, and local configuration wins |
| Signed updates, rollback on failure | Greengrass, Kura | Yes, for the runtime itself. Signature, expiry, version and hash are checked. **Not** tried through systemd on a real gateway |
| Device firmware updates | ThingsBoard OTA | Yes, for sensors and meters whose definition allows it, in rollouts through the ledger. Robot, PLC and safety firmware stays with their makers, by design |
| Reverse tunnels, remote shell | ThingsBoard, Greengrass | By design: no way in from the platform |
| Gateway to gateway | ThingsBoard cloud-edge, Kura | Yes: signed peers, one hop, with no cloud |
| Relay out of a closed network | Hono, Azure Layered Network | Yes |
| Exports, store until delivered | Kura wires, Greengrass stream manager | Yes: HTTP, MQTT, NATS, Kafka, RabbitMQ, InfluxDB and ClickHouse, real in CI. S3, SQS and SNS against moto. Pub/Sub and Azure by the documented shape of the calls |
| An embedded broker | Greengrass Moquette, Azure IoT Operations | By design: Mosquitto beside it. See [the plan](../plan/edge.md) |
| Clustering, automatic failover | EMQX, NATS | By design for authority: failover is a person's decision. A broker cluster is the site's own |
| Prometheus metrics | Most | Yes: `/metrics` on the console |

## Devices and the platform

| Capability | Who has it | Ferrum |
|---|---|---|
| Device identity and lifecycle | All | Yes: Operations' enrolled keys, claimed to retired |
| Tokens for devices with no gateway | ThingsBoard, Hono | Yes |
| Attributes, reported and desired | ThingsBoard shared attributes, Ditto, Azure device twin | Yes: desired goes through the ledger and stays out of step until it is true |
| Twin hierarchies, AAS, Thing Descriptions | Ditto, BaSyx | Partly: assets in a tree, and a twin, a Thing Description and an AAS for each device, read only |
| Alarms with acknowledge and clear | ThingsBoard, Ignition | Yes |
| Dashboards with widgets | ThingsBoard, Ignition Perspective, OpenRemote | Yes: a grid of widgets, variables, shared links, history charts |
| Multi-tenancy with row-level security | ThingsBoard PE, Magistrala | Yes: the kernel's |
| Time-series store at scale | ClickHouse-backed products, Timescale | Partly: PostgreSQL partitions and rollups, not measured against the strategy's 20,000 a second |

## Licences that shaped this

ThingsBoard (BUSL from 4.4) and EMQX (BSL from 5.9) are for integrating with, not building on.
OpenRemote is AGPL. Mosquitto (EPL and EDL) runs beside the runtime as a separate program. async-opcua
is MPL-2.0, which sits comfortably in AGPL software.
