# IoT Edge

Connects equipment to Ferrum, with the protocol as data.

- **Definitions**: a few lines of YAML say how to reach a device, how to read it and how to command
  it. TCP, UDP, serial, HTTP, WebSocket, MQTT, Modbus and OPC UA need no code, nor do Sparkplug B and
  MTConnect, and equipment can call in. Anything else runs as an external driver in any language. A
  definition is checked when saved, tried on a sample with no device attached, and reaches gateways
  only when published. Twenty templates start you off, for VDA 5050, Open-RMF, openTCS, ROS 2 and more.
- **Flows**: logic drawn on a canvas, run at the gateway with or without the link. A flow has no code
  and cannot loop, is simulated and reviewed by someone else before it runs, and the strongest thing
  it can do is ask for a command.
- **Gateways**: `ferrum-site` runs on location and keeps working with the link down. Its definitions,
  devices, rules and exports come from here, and it runs on what it last heard.
- **Alarms**: rules run at the edge, so they raise with the link down, and come back here to be
  acknowledged.
- **Reported state**: attributes and events from every device, what was asked of it and what it says.
- **Robots**: where each one is, on a plan of the site, and what it is doing.
- **Exports**: a site's data to a historian, another broker or someone else's cloud, kept until delivered.
- **Releases**: new runtimes, signed, rolled out a share of gateways at a time.

Ferrum coordinates equipment and does not control it. Rules raise alarms and do not move anything.
Tried on simulators and stand-ins only: see `docs/compatibility.md`.
