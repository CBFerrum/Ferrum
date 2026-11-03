# Earlier findings, against the assembled release

The handoff asks that earlier findings be checked against what is assembled, not taken from a report. The
assembly is Ferrum `d723eb9`, pinning ERP `d124642` and IoT Edge `45ea519`; Edge's `main` is `fbe1e99`, which
changes only tests. Everything here ran on 2 November 2026, on simulators. **Nothing ran on equipment.**

| Finding | Where it is fixed | What proves it | Result |
|---|---|---|---|
| Duplicate warehouse mission on repeated release | `crates/ops/src/commands.rs`, `addons/wms/src/tasks.rs` | `wms_it`, `ops_it`; on the real binary, a release said three times with the gateway dead | Passed; one mission |
| Recovery choosing a device by operation name | `gateway/src/journal.rs`, `runtime.rs` | `recovery_it`, `dispatch_it`; a gateway killed mid-mission | Passed |
| Backward clock extending leases | `gateway/src/clock.rs` | `dispatch_it`, `recovery_it`, mutants `clock-` 6 of 6 | Passed |
| Stale dispatch checks and synthetic expiry | `gateway/src/runtime.rs` | `dispatch_it`, mutants `gate-` 6 of 6, `expired-` 1 of 1 | Passed |
| Absent dependencies treated as done | `gateway/src/runtime.rs`, `journal.rs` | `dispatch_it`, `recovery_it`, mutants `dependency-` 3 of 3, `ending-`, `platform-never` | Passed |
| Completion verifying location, not item or unit | `addons/wms/src/tasks.rs` | `wms_it`; wrong item and lot refused on the real binary, by an operator | Passed |
| Machine start before material, no reservation | `addons/mes/src/runs.rs`, `addons/wms/src/waves.rs` | `line_it`, `mes_it`; on the real binary, the press stayed off until the scan | Passed |
| Wrong-scan audit rolled back | `addons/wms/src/site.rs` | `wms_it`; `wms.wrong_scan` read from `audit_event` after refusals | Passed |
| Firmware redirects escaping the allow list | `gateway/src/adapters/transport.rs`, `defined.rs`, `export.rs` | `adapters_it`, `links_it`, mutants `redirect-` 2 of 2, `allow-` 1 of 1 | Passed |

Suites on the final commits: warehouse mutants 41 of 41, shop floor 15 of 15, ledger 24 of 24, hook effects 3 of
3, sockets 4 of 4, gateway site runtime 11 of 11. Repeat runs: `dispatch_it` 12 of 12; `line_it`, `wms_it`,
`mes_it`, `ops_it` 6 of 6 each. The gateway suite: 256 passed, none failed. Upgrades from the versions before
this work were walked on real binaries, both for a workspace with work in flight and for a 0.3.1 gateway killed
mid-mission ([industrial.md](../plan/industrial.md)).

CI on `main` for the assembled commit: see `ci_runs` in [baseline.json](baseline.json).
