# yrs 0.28.0, with one fix (vendored)

This is yrs 0.28.0 from crates.io (MIT, LICENSE beside this file), unchanged but for one fix in
`src/block.rs` and an `allow` of warnings at the top of `src/lib.rs` (a path dependency is built
as a local crate, so upstream's warnings would fail Ferrum's `-D warnings`). The fix: text split inside a surrogate pair (an emoji) becomes U+FFFD on each side, as in
Yjs, so both halves keep their UTF-16 length. Without it, a Yjs client's edit inside an emoji
corrupts the server's copy of a document, and snapshots written from it cannot be read back
(Ferrum docs/plan/spike-c3-yjs.md, "Found", 2).

* Upstream: issue y-crdt/y-crdt#714, fix y-crdt/y-crdt#715 (open on 30 October 2026).
* The change: `docs/plan/spikes/yjs_server/yrs-surrogate-split.patch`.
* Ferrum uses it through `[patch.crates-io]` in the workspace's Cargo.toml.

When a yrs release includes the fix: delete this folder and the `[patch.crates-io]` entry, and move
Ferrum's `yrs` dependency to that release.
