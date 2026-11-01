# Requirements registry

The state of Ferrum against its completion specification, kept as data next to the code. The
capability matrix ([capabilities.md](../capabilities.md)) and the backlog ([backlog.md](backlog.md))
are generated from it. Conversation and memory are not the project's state; this is.

```bash
python3 scripts/registry.py check      # CI runs this
python3 scripts/registry.py generate   # after changing the registry
python3 scripts/registry.py export     # every entry in full, as JSON
```

## An entry

Entries are written compactly, one area to a file, with `defaults` at the top. `export` gives them
in full, with these fields:

| Field | Written as | Meaning |
|---|---|---|
| `id` | `FERRUM-AREA-001` | Permanent. |
| `title`, `component`, `spec` | | What it is, and the specification's section. |
| `status` | see below | |
| `basis` | `tested`, `read`, `probe` | How far the status was established. |
| `workstream` | 1 to 12 | The specification's dependency order. |
| `repositories`, `dependencies` | `deps` | Where it lives, and what must be true first. |
| `implementation.files` | `files` | Paths; `ferrum-app-pos:src/x.rs` for another repository. |
| `implementation.commits` | | Commits that built it. |
| `acceptance.*` | | Criteria, per the specification's seven kinds. |
| `evidence.test_runs` | `tests` | What proves it. |
| `evidence.release_digest`, `evidence.reviewer` | | The qualified image, and the person who reviewed it. |
| `external_dependencies` | `external` | Entries of [blockers.yaml](blockers.yaml). |
| `known_limitations` | `limits` | What it does not do. |

## Status

| Status | Meaning |
|---|---|
| `verified_complete` | Passes its acceptance tests against a released digest, reviewed by a person. |
| `implemented_unqualified` | Code exists; sufficient acceptance evidence does not. |
| `partial` | Some required behaviour exists; identifiable parts are missing. |
| `documented_only` | Described, not established as working. |
| `missing` | Not found in the repositories. |
| `blocked_external` | Waits on credentials, access, certification, hardware or validation. |
| `not_applicable` | An approved boundary rules it out for a profile, with the boundary stated. |

`scripts/registry.py` refuses `verified_complete` without test runs, a `sha256:` digest and a
reviewer. No entry has that yet, because no release has been qualified against this registry.

## How far to trust an entry

`basis` matters more than `status`:

- **tested**: tests ran during the audit, and for this work's invariants `scripts/mutants.py` shows
  they fail when the invariant is broken.
- **read**: someone read the code. It says what the code does, not that it works.
- **probe**: a keyword search over the source. A `missing` on this basis means nothing was found, which
  is a lead to confirm when the work starts, not proof of absence. An `implemented_unqualified` on
  this basis means code and tests exist; nobody has run the specification's acceptance scenario.

The revisions read are in [release-manifest.yaml](release-manifest.yaml). Keep a checkout fresh before
probing: the audit found every sibling checkout behind its remote, which made finished work look
absent.

## The other working documents

| Document | Where |
|---|---|
| Architecture map | [plan/architecture.md](../plan/architecture.md), [plan/apps-ui.md](../plan/apps-ui.md) |
| Domain dictionary | [domain-dictionary.md](domain-dictionary.md) |
| Threat model | [threat-model.md](threat-model.md), [security.md](../security.md) |
| Capability registry | This folder; [capabilities.md](../capabilities.md) |
| API and event catalog | [api.md](../api.md), [events.md](../events.md), [plugins.md](../plugins.md) |
| Data-governance catalog | [data-governance.md](data-governance.md) |
| Release manifest | [release-manifest.yaml](release-manifest.yaml) |
| Operations handbook | [operations.md](../operations.md) |
| External-blocker register | [blockers.yaml](blockers.yaml) |
| Handover, session by session | [handover.md](handover.md) |
| Accepted advisories | [advisories.yaml](advisories.yaml) |

Release notes and a completion report are not generated yet; the registry holds what they need.
