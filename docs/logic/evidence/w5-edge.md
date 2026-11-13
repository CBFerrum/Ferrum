# W5: business-critical edge pipelines (8 November 2026)

Software evidence only, run on the owner's development machine (macOS, PostgreSQL 17.9) from the worktree
`ERP/flc/edge` (CBFerrum/ferrum-app-edge, branch `logic/w5-pipelines`, based on Edge `49cb686`, 0.5.9),
built against Ferrum's main (`af3136b`) from the local checkout, and merged as CBFerrum/ferrum-app-edge#14 (`1e8249e`). No equipment, no field protocol and no
real power cut were involved: the power cuts are process aborts at named crash points on the gateway's
real journal file, which is what a simulator can show and no more (BLOCK-INDUSTRIAL-HARDWARE).

## What ran

```bash
cargo test -p ferrum-edge-contract                            # 118 passed
cargo test -p ferrum-site --lib                               # 133 passed
cargo test -p ferrum-site --features failpoints --test critical_it   # 2 passed
cargo test -p ferrum-app-edge --test <each of 11>             # all pass, conditions_it among them
cargo test -p ferrum-app-edge --lib                           # 22 passed
cargo clippy --workspace --all-targets --features ferrum-site/failpoints -- -D warnings
cargo fmt --all -- --check
(cd ui && npm run typecheck && npm test && npm run build && npm run check)   # 45 passed; dist/ rebuilt
python3 scripts/registry.py check                             # in Ferrum
cargo clippy --locked -p ferrum-server --no-default-features --features edge -- -D warnings   # in Ferrum, Edge pinned at 1e8249e
```

## What it showed

**The language** (`contract/src/pipeline.rs`, `flow.rs`). *Bring to units* computes 2048 counts × 0.1 − 40
as exactly 164.8, 0.1 × 3 as 0.3, and quarantines "hot", null and a value outside what it can be. *By
quality* routes good, bad, and a reading seen two minutes before it arrived as stale. An event window
waits ten seconds for stragglers, counts the same reading once, gives a minute's average once the
watermark passes, corrects it on `revised` for a reading seen inside its lateness, names a later one
`late`, and closes a quiet device's window marked partial; wired so, a correction updates the evidence
and the command the first result asked for is not asked again. An episode opens only after its on-delay
(a dip under the line resets that), stays one episode while the value hovers in the deadband, closes only
after its off-delay, reopens as the same episode within its reopen time, takes a bad reading as evidence
and never as recovery, says silence once as stale, and names itself by flow, step, device and the moment
it began to hold. A step remembers 5,000 devices and says each one past that. The flows that ship
(cold rooms, energy meters) parse unchanged and keep hearing only trusted readings.

**A business-critical flow at a gateway** (`gateway/tests/critical_it.rs`). The test runs itself again as a
child that takes one step on the real SQLite journal and aborts at `before_flow_commit` or
`after_flow_commit` (feature `failpoints`). Aborted before: the journal holds neither the step's state nor
its facts, and the reading delivered again is decided afresh, once. Aborted after: it holds both, the same
reading changes nothing, and the next one is evidence for the same episode.

**On the platform** (`tests/conditions_it.rs`; a condition about a device of another site than the reporting gateway's is not taken, with a release of a process triggered by
`edge.condition_opened`, kept by one administrator and reviewed and activated by another). An opening fact
delivered twice makes one Edge Condition and one published opening, and the process has one run for the
episode. Four readings (one delivered twice, one seen before the newest) count four, and the late one does
not replace the newest value. A staleness notice leaves the condition open. A closing said twice closes
it once; a reopening opens the same condition again, publishes `edge.condition_reopened`, and still one
run. A second episode heard first by its closing is made, says it opened and closed, and gets its own run.
The condition's page lists its run. Another workspace sees no condition.

## Limits

- Field protocols that acknowledge a reading before the flow's step is written can lose that reading in
  between; polled devices are read again. No real power cut, equipment or disk-full test.
- The process runtime (Temporal) was not part of the platform test: it shows the start, not the task.
- Offline scope (FLC-EDGE-008) and owner incarnation (FLC-EDGE-009) are as they were before W5.
- Windows are tumbling; units are named, not converted between.
