# Releasing

How an app's release is numbered, described and checked, how the platform pins it into its build, how a store lists
and sells one, and what the maintainers look at before they pin. The design is [docs/plan/apps.md](../plan/apps.md);
the manifest's fields are [03](03-the-manifest.md); the checks are [15](15-testing-and-ci.md).

## How an app reaches an installation

| Runtime | How a release reaches an installation |
|---|---|
| `native` | Compiled into the platform's build. The platform's `server/Cargo.toml` names the app's repository as an optional dependency behind a feature (`comms = ["dep:ferrum-app-comms"]`, `ferrum-app-comms = { git = "https://github.com/CBFerrum/ferrum-app-comms", branch = "main", optional = true }`), and `Cargo.lock` holds the exact commit: that commit is *the pin*. Never from a store bundle: an upload of a native app is refused |
| `wasm`, `meta` | A signed bundle (`.fapp`) from a store the installation trusts (`apps.registries`, `apps.trusted_keys`), or an upload where the operator allows it ([docs/api.md](../api.md), *Apps*) |
| `service` | Installed from a store like a bundle; its code runs on the publisher's servers ([docs/plugins.md](../plugins.md)) |
| A private native app | A server of its own, built in the app's repository on `ferrum-server` with `ferrum_server::extensions::extend` and `ferrum_server::run`; the platform's CI never fetches it ([docs/plan/apps.md](../plan/apps.md), §11) |

Most of this guide is about native apps, which is what every app in CBFerrum's repositories is today.

## Versions

- **The app's version is `version` in `app.yaml`.** The crate's version in `Cargo.toml` is not the app's (Comms' crate
  is `0.7.0`, the app `0.25.1`): readers, the store, the CHANGELOG and every check read the manifest's.
- **Semantic versioning.** A new major version for an incompatible change to anything another app relies on: an
  extension point's schema, an entity or field others link to, an intent's payload ([docs/plugins.md](../plugins.md):
  "An incompatible change to a point is a new major version of its owner, so `requires` ranges keep owners and add-ons
  in step"). Before 1.0, the minor is the major: 0.22 to 0.23 may break, 0.23.0 to 0.23.1 may not.
- **`platform`** is the range of the platform's App API the app works with: `">=0.3.9, <0.4"` for an app that reads
  operator settings and uses the vault (0.3.9). `scripts/check_apps.py` fails an app with operator settings whose range
  starts below 0.3.9. An installation outside the range shows the app as blocked (`blocked: [{app: "platform", …}]`).
- **`requires`** names each app the app needs, with a range. The platform's build test `compiled_apps_fit_together`
  (`server/src/packages.rs:589`) fails when a compiled app's range does not include the version compiled beside it:
  "<app> requires <dep> <range>, compiled with <dep> <version>". So when you release a sibling's next minor, check
  every app whose range stops below it, and widen theirs in the same batch.

## The CHANGELOG

`CHANGELOG.md` at the repository's root has a heading for every version, the manifest's included:
`scripts/check_apps.py` looks for `^## .*\b<version>\b` and fails "CHANGELOG.md says nothing of <version>" otherwise;
the README must point at `CHANGELOG.md` or list the version in a table. An add-on's history is its
repository's.

Each entry says, for its readers:

- **What it needs**: the platform's commit or App API ("Needs the platform's companion calls (Ferrum `9a9bd8f0` or later;
  its app API still says 0.3.9)"), and other apps' versions.
- **Whether the database or the interface changes** ("No database or interface change"), and any backfill or step an
  operator sees.
- **What changed**, in plain words, by what a person or an operator notices, with the names they will meet (settings,
  services, secrets, tasks).
- **What was run, and what was not**: against which real programs and versions ([20](20-writing-for-people.md)).

From Comms 0.23.0:

```markdown
## 0.23.0

Needs the platform's companion calls (Ferrum `9a9bd8f0` or later; its app API still says 0.3.9). No database or
interface change.

**LiveKit and the mail server take their files from the vault as they start.** Comms declares two services …
```

## Licences and paid apps

- An app's licence is one Ferrum may ship: official apps are `AGPL-3.0-only`. `scripts/check_apps.py` accepts a
  manifest licence beginning `AGPL-3.0`, `GPL-3.0`, `LGPL-3.0`, `MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause` or
  `MPL-2.0`; `scripts/licenses.py check` then holds every Rust dependency of the build to the accepted list (no GPL or
  LGPL there: [13](13-security-rules.md), *Dependencies and licences*). The repository carries a `LICENSE` file.
- **A sold app** is one whose store answers `GET {store}/v1/apps/{id}/plans` with plans. It installs or upgrades in a
  workspace only with a live licence for that workspace: the administrator gives it (`PUT /api/admin/apps/{id}/licence`),
  the server keeps it once a trusted store signed it, for that app and one of the workspace's own hosts, and asks the
  store again at each install and upgrade (`403` otherwise, saying why); every licence is read again with the
  revocation list, and a lapse shows without removing anything (`server/src/appmgr.rs`, `set_licence`, `licensed`). This
  applies to apps from a store; a native app compiled into the build is not sold through it.

## The store listing

What a store and the install dialog show comes from the manifest and the repository:

| Shown | From |
|---|---|
| Name, summary, categories, publisher, homepage, where to ask for support | `app.yaml` (all required by `scripts/check_apps.py`) |
| The description | The README the manifest names (`description: README.md`): the install plan shows its opening prose as plain text, about 600 characters, without HTML, pictures, badges, headings, tables or code (`ferrum_apps::description::excerpt`). Put what the app is and does first ([22](22-readme-and-repository.md)) |
| The launcher's icon | `shell.app` contributions' `icon`, one the shell draws (`scripts/check_apps.py`) |
| What it asks for | The install plan's `permissions` and `increases`: records it reads and writes, hosts it calls, public endpoints, sockets, schedules, operations it uses from other apps. An upgrade that asks for more stops for a new consent |
| Whether it may lose data on removal | `uninstall: ask` (data deleted only when the administrator asks) or `keep` |

The store service's review, countersigning, storefront and revocation list are designed in
[docs/plan/apps.md](../plan/apps.md), §5 and §9; what an installation reads of a store is in [docs/api.md](../api.md),
*App stores*. What of the store service itself is built and running was not checked for this guide.

## How the platform pins an app

1. The app's `main` has the release: the version in `app.yaml`, the CHANGELOG entry, the README, `ui/dist` built with
   `.source`, its checks passed on the developer's machine (`scripts/app_ci.sh`).
2. In the platform's checkout: `cargo update -p ferrum-app-<name> --precise <commit>` for each app in the batch. Only the
   apps' own crates move; check that nothing else in `Cargo.lock` did.
3. The platform's checks run on that build: `scripts/check_apps.py`, `scripts/secret_paths.py`, `scripts/settings.py`
   (and `--write` when an app's operator settings changed the operations guide's table), `scripts/licenses.py check`,
   `scripts/mutation_paths.py`, the `rust` job with `compiled_apps_fit_together`, and the rehearsals the change touches
   (`scripts/local_ci.sh oneinput` for settings and secrets, `companions` for services, `oidc` for sign-in).
4. The pin lands in the platform with its notes: which apps at which versions, what the checks said, what was not run.

An app whose `main` moved by documentation alone (a README, screenshots) need not be pinned again.

## The release checklist (the app's developer)

- [ ] `version` in `app.yaml` raised as the change deserves; the crate's version left alone or raised, as you keep it.
- [ ] `CHANGELOG.md` has `## <version>`: what it needs, database and interface changes, what changed, what was run and
      what was not.
- [ ] `platform` and `requires` ranges right; every sibling whose range this release falls outside is released in the
      same batch.
- [ ] Database changes additive in this release ([14](14-operations-for-apps.md)); new SQL in new numbered files; no
      released file edited; backfills declared.
- [ ] Operator settings declared and read with `ferrum_kernel::env::var`; no secret anywhere but the vault
      ([07](07-stateless-apps.md), [08](08-secrets-and-the-vault.md)).
- [ ] New personal data declared (`personal_data`, `PersonalHeld`); new engine tables in `export_tables` or said why not
      ([12](12-privacy-and-data-duties.md)).
- [ ] `ui/dist` built from the sources it names in `.source` ([17](17-the-interface.md)).
- [ ] `scripts/app_ci.sh <app>` passes; mutants for new guarantees run; the real programs it talks to run, or the
      CHANGELOG says they were not ([15](15-testing-and-ci.md)).
- [ ] A walk on a running server for a release that changes flows people use, with what it found and fixed recorded
      ([docs/app-contract.md](../app-contract.md), *Reviewed*).
- [ ] README updated; `SECURITY.md` and `LICENSE` present ([22](22-readme-and-repository.md)).
- [ ] Commits carry no attribution lines.

The full list a developer walks before asking for a pin is [24](24-review-checklist.md).

## What the maintainers check before pinning

Everything the checks cannot see, read by a person:

- The contract's reviewed parts ([docs/app-contract.md](../app-contract.md), *Reviewed*): a home that says what needs
  attention, record pages, roles for the jobs people do, pages for people outside, reports, upgrades that keep what was
  made before working, phone width and accessibility basics, a walk on a running server.
- Every item marked "reviewed, not checked" in [13](13-security-rules.md), *What goes to review*: public routes,
  webhooks, new roles and rights, `allow_private`, `system_tx` in a request path, an HTTP client of the app's own,
  statements built with `format!`.
- The CHANGELOG against the diff: nothing changed that it does not say, nothing claimed that was not run.
- The two-version window for the app's SQL and metadata, which no script checks for an app.
- That the batch fits: every compiled app's `requires` meets its siblings' versions.

The maintainers' own checklist is [24](24-review-checklist.md).

## Checked by

- `scripts/check_apps.py`: manifest fields, licence, README, CHANGELOG entry for the version, scenario test, `ui/dist`,
  icons, operator settings.
- `compiled_apps_fit_together` (`server/src/packages.rs`): ranges, contributions, icons, report names, routes.
- `scripts/licenses.py check`; `scripts/secret_paths.py`; `scripts/settings.py`; `scripts/mutation_paths.py`.
- The CHANGELOG's honesty, semantic versioning, the window for an app's own changes: reviewed, not checked.

## See also

- [03 The manifest](03-the-manifest.md), [15 Testing and CI](15-testing-and-ci.md),
  [22 README and repository](22-readme-and-repository.md), [24 Review checklist](24-review-checklist.md),
  [14 Operations for apps](14-operations-for-apps.md)
- [docs/plan/apps.md](../plan/apps.md); [docs/api.md](../api.md), *Apps* and *App stores*;
  [docs/licenses.md](../licenses.md)
