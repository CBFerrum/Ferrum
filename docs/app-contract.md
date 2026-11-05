# What every app gives

The completeness contract for an app of Ferrum's (specification §7.2, FERRUM-UX-004): what a
person, an administrator and the platform can count on from every app, whoever wrote it. The parts a
program can see are checked on every pull request by `scripts/check_apps.py`, over the apps the lock
compiles in; the rest is reviewed, and walked in a browser (docs/platform/evidence).

## Checked on every pull request

| Part | What it is |
|---|---|
| A repository of its own | `ferrum-app-<name>`, under a licence Ferrum may ship (AGPL-3.0 or compatible) |
| A manifest | `app.yaml` with its id, name, version, publisher, summary, categories, licence, homepage and where to ask for support |
| A description | The manifest names its README, which the store and the install dialog show |
| A history | `CHANGELOG.md` says what each version changed, the version in the manifest included, and the README's changelog table has that version's row |
| A scenario test | At least one test under `tests/` that runs the app's flows against a real PostgreSQL |
| An interface, when it has one | `ui/dist` built, with `.source` saying which sources and which Ferrum-Frontend it was built from |
| A launcher icon the shell draws | Every `shell.app` contribution names an icon `server/shell/runtime.js` knows |

## Reviewed

- **A home that says what needs attention**, and offers the next step when there is nothing yet:
  every home page is walked empty as well as full.
- **Record pages for what people work with**, through the platform's record page, so lists, forms,
  actions, history, comments and permissions behave the same in every app. An action says what it
  did.
- **Roles for the jobs people do**, each allowed what that job needs; sensitive fields in a field
  group (bank details, identity numbers), masked rather than hidden where a person needs to know
  there is one; a person's own records reachable through owner fields.
- **Pages for people outside the workspace** where they deal with it: customers, suppliers, partners,
  citizens, lessees, guests, each signed in to the portal and seeing only what their address
  belongs to.
- **Reports for the figures its users are asked for**, with the parameters they would choose.
- **Upgrades that keep what was made before them working**: a release that adds a standard account,
  a field or a default reaches records made before it (or says how to), and nothing that was
  installed is redone.
- **Phone width and accessibility basics**: a form's labels name their fields, the pages fit 375 px,
  and the colours hold in dark mode.
- **A walk on a running server** before a release that changes flows people use, with what it found
  and fixed recorded.

## How the check works

`scripts/check_apps.py` reads `cargo metadata` for the packages that come from CBFerrum's app
repositories, and for each that has an `app.yaml` beside its `Cargo.toml` (an app, or an add-on of
one) checks the parts above in that checkout. It prints each app and what it lacks, and fails if any
lacks something.
