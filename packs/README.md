# Reference packs

A reference pack is a whole solution built by configuration alone (custom entities, checks, roles,
automations, a solution's rules, Studio items) and kept here as the `ferrum.pack/1` document an
administrator would export and install ([docs/api.md](../docs/api.md), Packs). Each one rebuilds an app
that is Rust code today, to measure what configuration can and cannot express.

| Pack | Rebuilds | Test | Findings |
|---|---|---|---|
| [grc](grc/grc.pack.json) | `ferrum-app-grc` 0.1.14 | [grc_pack_it.rs](../crates/studio/tests/grc_pack_it.rs) | [grc-as-a-pack-2026-11-10](../docs/builder/evidence/grc-as-a-pack-2026-11-10.md) |
| [government](government/government.pack.json) | `ferrum-app-government` 0.3.0 | [government_pack_it.rs](../crates/studio/tests/government_pack_it.rs) | [government-as-a-pack-2026-11-11](../docs/builder/evidence/government-as-a-pack-2026-11-11.md) |

## How one is checked

Its test builds the pack in a fresh workspace with the calls an administrator makes, exports it, and fails
unless the result is exactly the committed file (pretty-printed, keys sorted, without `exported_at`, and
with the source's review moment of each solution set to the day the pack was made, since no rebuild
repeats it). It then installs the committed file in another workspace and restates the app's own
acceptance scenarios there, as people holding the pack's roles, each step marked same, different or
cannot.

After changing the builder, write the file again and commit it:

```
FERRUM_WRITE_PACKS=1 cargo test -p ferrum-studio --test grc_pack_it
FERRUM_WRITE_PACKS=1 cargo test -p ferrum-studio --test government_pack_it
```
