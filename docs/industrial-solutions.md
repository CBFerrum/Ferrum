# Industrial solutions in an app

An app can carry an industrial solution (FERRUM-IND-018) in its bundle beside its packages. The
solution says three things:

- which connections the site runtime must open for it;
- the equipment it works with;
- how a site proves the solution before it goes live.

The format and its checks are `crates/apps/src/industrial.rs`. A bundle that breaks them is refused
at publishing and at install, like any other bundle.

```yaml
# app.yaml
industrial:
  connectors:
    - {scheme: modbus+tcp, version: ">=0.3"}     # a scheme the site runtime opens, as Edge's contract names it
  device_profiles: [industrial/profiles/meter.yaml]
  acceptance: [industrial/acceptance/sat.yaml]
```

**A device profile** is one equipment type: `id`, `vendor`, `model`, the `scheme` it speaks (which
`connectors` must name), and an optional `description`.
- Its `points` each have a `name` and the `address` the device holds it at, in the scheme's own
  notation (a register, a node id, an OBIS code). They also have a `type`: `bool`, `int16`,
  `uint16`, `int32`, `uint32`, `int64`, `float32`, `float64` or `string`. A point may give a `unit`
  and a `scale`, and its `access` is `read` or `read_write`.
- Its `commands` each have a `name`, and may write a `point`, which must then be `read_write`.

```yaml
id: line-meter
vendor: Acme
model: PM-200
scheme: modbus+tcp
points:
  - {name: voltage, address: "30001", type: float32, unit: V}
  - {name: relay, address: "00001", type: bool, access: read_write}
commands:
  - {name: trip, point: relay}
```

**An acceptance procedure** is what a site runs at commissioning: `id`, `title`, and ordered `steps`.
- `read` reads a profile's point and checks it against `expect`: `min`, `max` or `equals`.
- `command` sends one of the profile's commands, and may check a point afterwards.
- `manual` asks a person to check what its `instruction` says.

```yaml
id: meter-sat
title: Meter site acceptance
steps:
  - {id: voltage, kind: read, profile: line-meter, point: voltage, expect: {min: 207, max: 253}}
  - {id: trip, kind: command, profile: line-meter, command: trip, point: relay, expect: {equals: false}}
  - {id: seal, kind: manual, instruction: The meter's seal is intact}
```

A bundle is refused when:
- a named file is missing, or a file under `industrial/` is not named;
- a file does not parse, or has a field these do not know;
- a profile speaks a scheme `connectors` leaves out;
- a command writes a missing or read-only point;
- a step names a profile, point or command the bundle does not define;
- a read expects nothing, or a manual step says nothing;
- an id is repeated.

Which schemes a site can open is the site runtime's to say: this checks their names only.
`Bundle::industrial()` reads the solution for whoever imports it.

## In a workspace

Installing or upgrading the app, where the workspace has Operations, makes the solution the
workspace's (`crates/ops/src/acceptance.rs`):

- each device profile becomes a **Device Profile**, *From App* `{app}/{id}`. Its product and protocol
  come from the profile, and its read and write capabilities list the points and commands. It starts
  *tested on Simulator*. An upgrade updates what the app says and keeps what people recorded (how far
  it was tried, firmware, exclusions);
- each procedure becomes an **Acceptance Procedure** with its steps.

A device whose profile one of the procedures names is commissioned only after a passed **Acceptance
Run** of each. On the device's page in Operations, a Device Administrator starts a run and records each
step as it is done on site. The site runtime can record them through the same calls:

| Call | Body |
|---|---|
| `GET /api/app/ops/devices/{id}/acceptance` | The procedures for it, whether it passed each, its runs |
| `POST /api/app/ops/devices/{id}/acceptance` | `{procedure}`: a run, one result to fill in per step |
| `POST /api/app/ops/acceptance/{run}/steps/{step}` | `{value}` seen for a reading or a command's check; `{passed}` for a manual step; a `note` |

A reading, or a command's check, passes only when the value seen meets the expectation. A manual step
passes when the person says it holds. Each step is recorded once, by name and time, and audited. A
failed step ends the run, and a new run starts from the beginning.

The install dialog lists the equipment a solution describes and its procedures, as what the app
describes, not as what anyone tried with it.

**Not yet:**
- Edge's gateway does not run the steps itself. A person records them, or a site runtime through the
  calls above.
- A workspace that adds Operations after the app gets its profiles at the app's next upgrade.
- Which schemes a site can open is checked by name only.
