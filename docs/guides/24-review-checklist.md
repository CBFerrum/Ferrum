# 24. The review checklist

Two lists. The first is what an app's developer walks before asking for a pin (the platform's `Cargo.lock` taking the
app's new commit). The second is what a platform maintainer walks before pinning. Each item says how to check it and
which guide explains it. An item marked **(reviewed)** has no script: a person looks.

How the platform takes an app: the server depends on each official app's repository by git, on `branch = "main"`
(`server/Cargo.toml`), behind a cargo feature, and `Cargo.lock` holds the exact commit. "The pin is the lock":
`cargo update -p <crate> --precise <rev>` moves one app. `docs/registry/release-manifest.yaml` says what the lock builds and `scripts/registry.py check` holds it to the
lock.

## Before asking for a pin (the app's developer)

### The repository

- [ ] `app.yaml` has `id`, `name`, `version`, `publisher`, `summary`, `categories`, `license`, `homepage`, `support`
  and `description: README.md`. Run the platform's `python3 scripts/check_apps.py` with your checkout patched in, or
  read `scripts/check_apps.py` and check by hand. ([03](03-the-manifest.md), [22](22-readme-and-repository.md))
- [ ] The version is raised, and `CHANGELOG.md` has a `## <version>` entry that says what changed, what it needs (the
  platform version, migrations, settings) and what was and was not run. ([16](16-releasing.md))
- [ ] `requires` ranges include the siblings' current releases; `platform` is at least the version with what you use.
  ([03](03-the-manifest.md))
- [ ] `LICENSE` is AGPL-3.0 (or a licence `check_apps.py` accepts); new dependencies are under licences
  `scripts/licenses.py` accepts, and each was chosen after looking for a mature project to depend on.
  ([13](13-security-rules.md))
- [ ] `SECURITY.md` says how to report privately and names the known weak points **(reviewed)**.
- [ ] The README says what was run and what was not, its limits, and nothing it cannot point at **(reviewed)**.
  ([22](22-readme-and-repository.md), [20](20-writing-for-people.md))
- [ ] No attribution lines in commits; no local paths, names or internal ids in public files **(reviewed)**.

### The code

- [ ] Nothing from another app's tables; other apps' records through the kernel, with `requires`; another app's
  operations only through `uses:` and `Kernel::call_tx` **(reviewed)**. ([11](11-integrations.md))
- [ ] No country or sector rule in an engine: data or an add-on **(reviewed)**. ([01](01-what-an-app-is.md),
  [19](19-languages-and-formats.md))
- [ ] Tables of your own: `tenant_id`, row-level security enabled and forced, the `tenant_isolation` policy, names
  prefixed with the app; migrations additive **(reviewed)**. ([04](04-data-and-records.md))
- [ ] No new raw write to an `e_*` table, or one the review in `docs/registry/mutation-paths.md` now lists with its
  kind (`scripts/mutation_paths.py`). ([04](04-data-and-records.md))
- [ ] Every route of the app's `Site` that writes calls the gate and nothing else that writes, or is in
  `docs/registry/agent-parity.md` with its kind; every action a person can press has a contract with `effect`, `returns`
  and `completion` (`scripts/agent_parity.py --app .`). ([06](06-events-jobs-and-logic.md))
- [ ] Every route checks rights; "not found" for what the caller may not know exists; public routes authenticate by
  what they carry and count requests before any lookup **(reviewed)**. ([05](05-permissions-and-people.md),
  [21](21-apis-and-docs.md))
- [ ] Event handlers idempotent; no outside call from a document hook; long work in a job, process or saga with keys
  **(reviewed)**. ([06](06-events-jobs-and-logic.md))
- [ ] Operator settings declared in `operator_settings` and read with `ferrum_kernel::env::var` (`check_apps.py`);
  nothing on local disk; no session state on a pooled connection **(reviewed)**. ([07](07-stateless-apps.md))
- [ ] Secrets only through the app's vault handle; no cryptography crate or `unsafe`
  of the app's own (`scripts/secret_paths.py`); no secret in a setting, log, error, event or test output
  **(reviewed)**. ([08](08-secrets-and-the-vault.md))
- [ ] Files through the file store or a declared file kind; no store or keys of the app's own **(reviewed)**.
  ([09](09-files-and-storage.md))
- [ ] Outbound calls through connections and the kernel's client **(reviewed)**. ([11](11-integrations.md))
- [ ] Personal data declared (`personal_data`, `add_personal_held`), `handover` where a leaver's work passes on,
  `export_tables` free of secrets **(reviewed)**. ([12](12-privacy-and-data-duties.md))
- [ ] Errors: stable codes, plain messages, nothing leaked; unique indexes have a sentence **(reviewed)**.
  ([20](20-writing-for-people.md), [21](21-apis-and-docs.md))

### The tests

- [ ] `scripts/app_ci.sh <your checkout>` from the platform passes: `cargo fmt --check`, `cargo clippy --all-targets
  -D warnings`, the suite against PostgreSQL as the non-superuser `ferrum` role, then the interface's `check`,
  `typecheck` and `test`. ([15](15-testing-and-ci.md))
- [ ] At least one scenario test (`tests/*_it.rs`) runs the changed flows against a real PostgreSQL (`check_apps.py`
  wants one).
- [ ] The test of a new rule failed before the change; an invariant that matters has a mutant in the app's
  `scripts/mutants.py` where it keeps one (Cloud does) **(reviewed)**. ([15](15-testing-and-ci.md))
- [ ] What the tests do not reach (a real provider, a device, a browser walk) is written in the CHANGELOG
  **(reviewed)**.

### The interface (when the app has one)

- [ ] `ui/dist` built from the sources as committed and committed with them (`cli.mjs check`: "dist/ matches its
  sources"). ([17](17-the-interface.md))
- [ ] `cli.mjs check` passes: base CSS, catalogs, size budgets, and the UI lint with no finding.
- [ ] Kit components only; no forked primitive; tokens only; type roles only (the lint).
- [ ] Every page's states walked on a running server: loading, empty (first use), no matches, denied, offline,
  failed; the home walked empty and full (`docs/app-contract.md`, *Reviewed*) **(reviewed)**.
- [ ] Every page at 375px wide, in light and dark, and in Arabic (right to left) **(reviewed)**.
- [ ] Every new left-menu row has an icon; a new entity's concept resolves in the kit's dictionary or the entity
  declares `icon:` **(reviewed; the kit's `navIconName.test.tsx` holds the installed apps' entities when the kit's
  list is regenerated)**.
- [ ] Every word through `t("…")`; no concatenated sentence; numbers and moments formatted; the app's catalogs
  updated for new words (`cli.mjs check` holds only that entries match) **(reviewed)**.
  ([19](19-languages-and-formats.md))
- [ ] The accessibility checklist of [18-accessibility.md](18-accessibility.md) walked for each changed screen, and
  what was checked recorded with the app version and the Frontend commit **(reviewed)**.
- [ ] If an acceptance scenario names the app (`acceptance.json`), its exercise walked and its evidence kept
  **(reviewed)**.

### The walk

- [ ] For a release that changes flows people use: walked on a running server with the platform pinned at its
  current lock and the app patched in, as each role that uses it; what was found and fixed recorded in the CHANGELOG
  (`docs/app-contract.md`, *Reviewed*: "A walk on a running server") **(reviewed)**.
- [ ] Upgrade from the previous release on a database that has its data: records made before still work, nothing
  installed is redone **(reviewed)**. ([14](14-operations-for-apps.md))

## Before pinning (the platform's maintainer)

### The pin itself

- [ ] Move only the app's own crate: `cargo update -p ferrum-app-<name> --precise <rev>`; check that no other crate
  in `Cargo.lock` moved unless that was the point (`git diff Cargo.lock`).
- [ ] The server's check of what compiled apps require of each other passes (`compiled_apps_fit_together`,
  `server/src/packages.rs`): a range that stops below a sibling's pinned release is refused; ask the app to widen it.
- [ ] `docs/registry/release-manifest.yaml` regenerated (`python3 scripts/registry.py generate`) and
  `python3 scripts/registry.py check` passes.

### The contract and the audits

- [ ] `python3 scripts/check_apps.py`: every app "ok"; no manifest's operator settings "not sound".
- [ ] `python3 scripts/settings.py` (with `scripts/test_settings.py`): every variable read is declared; the
  environment-only baseline did not grow.
- [ ] `python3 scripts/secret_paths.py` (with `scripts/test_secret_paths.py`): no new path past the vault; the
  baseline only shrank.
- [ ] `python3 scripts/mutation_paths.py`: every raw write to an entity table is in the review with a kind.
- [ ] `python3 scripts/licenses.py check`: no licence nobody accepted; `docs/licenses.md` regenerated when the
  dependencies changed.
- [ ] `python3 scripts/mutants.py --check`: every mutant still finds its text.

### Running it

- [ ] The app's own `scripts/app_ci.sh` run against this platform checkout passed (ask for its output, or run it).
- [ ] The platform builds with the new lock and its suites pass for what the change touches (targeted checks for one
  merge; the whole `scripts/local_ci.sh` at the end of a batch or a release).
- [ ] Where the app reads settings or secrets: `scripts/one_input.sh` (`scripts/local_ci.sh oneinput`) with the apps,
  started with `DATABASE_URL` alone.
- [ ] Where the app declares a service or a companion's sign-in: the kernel's `companions_it` and
  `scripts/oidc_rehearsal.sh` as they apply.
- [ ] Where the app changes its interface: the frontend the platform pins (`FRONTEND_REV` in the `Dockerfile`) is the
  one `ui/dist/.source` names, or one the app was checked against.
- [ ] A walk of the changed flows on a running server, recorded **(reviewed)**.

### What the maintainer reads

- [ ] The CHANGELOG entry: what it needs is true of this platform; what was not run is said **(reviewed)**.
- [ ] New migrations: additive, tenancy and row-level security on new tables **(reviewed)**.
- [ ] New routes: rights checked, not-found versus refused right, limits on public ones **(reviewed)**.
- [ ] New outbound calls, secrets, file kinds, companions: through the platform's seams **(reviewed)**.
- [ ] New dependencies: needed, maintained, licence accepted **(reviewed)**.
- [ ] Nothing a country's or a sector's in an engine **(reviewed)**.
- [ ] The pin's commit message says what moved and what was run; no attribution lines **(reviewed)**.

## Checked by

The scripts named in each item: `scripts/check_apps.py`, `settings.py`, `secret_paths.py`, `mutation_paths.py`,
`agent_parity.py`, `licenses.py`, `mutants.py`, `registry.py`, `app_ci.sh`, `local_ci.sh`, `one_input.sh`,
`oidc_rehearsal.sh`; the server's `compiled_apps_fit_together`; Ferrum-Frontend's `packages/build/cli.mjs check`.
Every item marked **(reviewed)** is a person's.

## See also

[23-dos-and-donts.md](23-dos-and-donts.md), [15-testing-and-ci.md](15-testing-and-ci.md),
[16-releasing.md](16-releasing.md), [README.md](README.md); `docs/app-contract.md`; `docs/registry/README.md`.
