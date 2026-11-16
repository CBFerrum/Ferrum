# 22. The README and the repository

What every app repository holds besides its code, what its README says and in what order, how the banners and
screenshots are made, and what a README must never claim. Some of it is checked on every pull request of the platform
(`scripts/check_apps.py`); the rest is the house style the 13 Nov 2026 rewrite of every app's README followed.

## What every app repository has

| File | What | Checked |
|---|---|---|
| `README.md` | the description the store and the install dialog show; the manifest names it (`description: README.md`) | `check_apps.py`: "app.yaml names no description file it has"; the README lists the manifest's version in a changelog table or points at `CHANGELOG.md` |
| `CHANGELOG.md` | a `## <version>` heading for every version, the manifest's included, saying what changed, what it needs (platform version, migrations) and what was not run | `check_apps.py`: "CHANGELOG.md says nothing of `<version>`"; an add-on's history is its repository's |
| `LICENSE` | the licence's full text. Apps are AGPL-3.0 (`license: AGPL-3.0-only` in Drive's manifest); the check accepts AGPL-3.0, GPL-3.0, LGPL-3.0, MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, MPL-2.0 | `check_apps.py`: "`<licence>` is not a licence Ferrum ships" |
| `app.yaml` | `id`, `name`, `version`, `publisher`, `summary`, `categories`, `license`, `homepage`, `support` (an add-on may take `license`, `homepage`, `support`, `description` from its parent) | `check_apps.py`: "app.yaml has no `<field>`" ([03-the-manifest.md](03-the-manifest.md)) |
| `tests/*_it.rs` | at least one scenario test against a real PostgreSQL (an add-on's may be its parent's) | `check_apps.py`: "no scenario test (tests/*_it.rs)" |
| `ui/` and `ui/dist/` | the interface and its built files with `.source`, when the app has one | `check_apps.py`: "`ui/dist` is not built (index.html and .source)" |
| `docs/` | `docs/assets/` (banners, `screens/`), and the app's own guides: API, operating notes, a user guide where it has one | not checked |
| `SECURITY.md` | how to report a vulnerability privately, and the known weak points | not checked. Only Cloud has one (`ferrum-app-cloud/SECURITY.md`) of the 37 app repositories read on 16 Nov 2026; every app should |
| `.github/workflows/ci.yml` | the CI the app would run on GitHub; `scripts/app_ci.sh` runs the same steps on the developer's machine and reads the test database variables and `RUST_MIN_STACK` from it, so keep it accurate | read by `app_ci.sh` |
| `Cargo.toml`, `build.rs`, `rustfmt.toml` | the crate; `build.rs` embeds `packages/` and `ui/dist` | the build |

Cloud's `docs/` is the fullest: `api.md`, `openapi.json` and `sdk.md` generated from its route table, `security.md`,
`user-guide.md`, and one `operating-*.md` per area (access, builds, code, deploy, recovery, resources, settings).

### SECURITY.md

Copy the shape of Cloud's:

```markdown
# Security

Please do not open a public issue for a vulnerability. Report it through GitHub's private vulnerability reporting on
this repository, with what you did and what happened. See [docs/security.md](docs/security.md) for what this is
designed to withstand and what is known not to be enough.

## <a known weak point, in its own section>
What it is, who can see it and for how long, what limits it, and how to put it right.
```

The known weak points are the point: Cloud's says exactly how long the forge administrator's password is on a command
line and who can read it there.

### CHANGELOG.md

One `##` heading per version, newest first, with the version as a word on its own (the check looks for
`^## .*\b<version>\b`). Under it: what a person or an operator notices, what the release needs (the platform version,
a migration, a setting), what was run to check it and what was not. Drive 0.18.0's entry starts "Needs platform 0.3.9
(Ferrum `22d082f7` or later) … No database change, no interface change." Releasing is
[16-releasing.md](16-releasing.md).

## The README, in order

The 13 Nov 2026 rewrite gave every app's README one shape (the pipeline's guide is `README-GUIDE.md` in the
maintainers' `docs-work/readmes`, outside every repository). Drive's `README.md` is a complete example.

1. **The head**, centred: the banner as a `<picture>` (`docs/assets/banner-dark.svg` for a dark theme,
   `banner-light.svg` otherwise), badges (version from `app.yaml` linking to `CHANGELOG.md`, licence linking to
   `LICENSE`, "runs on Ferrum", Rust, PostgreSQL, React and TypeScript when it has `ui/`, a test count only if you
   counted it), one bold sentence of what it promises, two or three sentences on who it is for and what it replaces,
   and a line of links to the sections.
2. **Why** the app: five to seven points that are true for it (one data set with the rest of Ferrum, self-hosted,
   AGPL, extendable in Studio, works offline: only what is so).
3. **At a glance**: a two-column table of version, kind (own interface or records screens), what it adds (entities,
   roles, reports, print formats, menus, counted from `packages/`), what it depends on, what it works with, languages
   (from `ui/src/locales`), tests (counted), licence.
4. **Screenshots**: a hero, then a two-column gallery of the rest, then phone shots, each a `<picture>` with the dark
   source and the light `<img>`, a bold title and a one-sentence caption of what a person does there.
5. **Features**, by area: every entity, workflow and its states, rule, report, print format, portal page, notification,
   import and export, role, offline behaviour, language, accessibility, API and event. Depth is the point.
6. **How it works**: one or two small Mermaid diagrams with a few sentences each.
7. **Data model**: entities, roles, reports, print formats, scheduled jobs.
8. **Integrations**: other Ferrum apps and outside services, each with what it needs (keys, accounts).
9. **How it compares**: four or five leaders of the segment (an open-source one where there is one) and a table of 20
   to 40 capabilities, marked has it, partly, no, not applicable; rows that favour the leaders as well as rows that
   favour Ferrum; where the app stands out and where the leaders are ahead; a note that the leaders' columns are from
   public information as of a stated month.
10. **Roadmap**: recently shipped (from the CHANGELOG), next up and later, as checklists.
11. **Install**: how a workspace gets it, what it needs first, the first steps after installing.
12. **Develop**: build, test with the test database variables the workflow uses, the interface's commands, checks.
13. **Project layout**: a short annotated tree.
14. **Changelog**, **Contributing**, **Licence**: short, with links; then a one-line footer.

The README guide asks for an emoji on every `##` and `###` heading, from a fixed set in `##` headings (others break
GitHub's anchors), and Mermaid kept small (14 nodes at most, labels with punctuation quoted, no HTML). The handbook's
own files use no emoji; a README does.

## Banners and screenshots: the toolkit

The tools are in the maintainers' working folder, not in any repository, and two of them carry absolute paths of the
machine they were written on. To use them, ask for the folder, and check the paths first.

| Tool | What it does |
|---|---|
| `docs-work/_toolkit/docgen/banner.mjs <config.json> <lucide icons dir>` | light and dark SVG banners from a title, a tagline, the app's colour and a Lucide icon |
| `docs-work/_toolkit/docgen/shots.mjs <plan.json> <out dir>` | framed light and dark WebP screenshots from a development server with the `demo` workspace |
| `docs-work/_toolkit/docgen/seed.mjs`, `drive-seed*.mjs`, `sign-seed.mjs`, `studio-seed.mjs` | fictional demo records for the screenshots |
| `docs-work/_toolkit/docgen/mmcheck.mjs <README.md>` | parses every Mermaid block with Mermaid itself |
| `docs-work/_toolkit/docgen/preview.mjs <repo dir> <owner/repo> <out.png>` | the README as GitHub renders it |
| `docs-work/_toolkit/docgen/build.py <repo> [--no-shots]` | the older (31 Oct 2026) whole-README builder from `facts.py` and `content.py`; its icon folder (`ICONS`) points at a checkout that no longer exists |
| `docs-work/readmes/_tools/assets.mjs [repo ...]` | the 13 Nov screenshots: from a captured set of the running suite, framed, saved as `docs/assets/screens/*-light.webp` and `*-dark.webp`, with a manifest per repository |
| `docs-work/readmes/_tools/check.py` | every image a README names exists, every `#anchor` resolves, HTML blocks balance, no blank line inside a table, and none of the words a public README must not hold |

Playwright for the scripts is in `docs-work/_toolkit/pw` (`npm install` there). Screenshots show only fictional
records; never take one of a real workspace.

## What a README must say

- **What was run and what was not**: which suites, against what (PostgreSQL 17, a real Forgejo, a fake payment
  provider), and what has never been run (on hardware, against a real tax authority, with a screen reader). The
  platform's own docs do this throughout (`docs/limits.md`; `docs/app-contract.md`, *The forge, as Cloud configures it*:
  "checked with the real Forgejo 17.0.1").
- **What it needs**: the platform range, the apps it requires, operator settings, services beside the server, keys
  the user must bring.
- **Its limits**, in plain words, where a person deciding to use it will see them.
- **How to report a vulnerability**, through `SECURITY.md`.

## What a README must never claim

- A feature the code does not have. Every claim is true in the code: read `app.yaml`, `packages/`, `src/`,
  `ui/src/`, `CHANGELOG.md` and `docs/` before writing it; when unsure, leave it out.
- A leader's feature you are not sure of, or a "has it" for Ferrum that is only partly so: the README guide calls a
  wrong "has it" the worst mistake there is.
- Numbers nobody counted: tests, entities, users, speed. Count them (`grep -c "#\[tokio::test\]\|#\[test\]"` for
  tests) or leave them out.
- "Accessible", "WCAG compliant", "GDPR compliant", "certified", "production ready", "secure" without the record that
  backs it ([18-accessibility.md](18-accessibility.md), [12-privacy-and-data-duties.md](12-privacy-and-data-duties.md)).
  Say what was checked.
- Relative dates ("recently", "last week"): use absolute ones.
- Anything private: internal row ids, local paths, people's names, the names of the tools or assistants that wrote it,
  workspace data in a screenshot. `check.py` looks for these.
- Hype: "revolutionary", "blazing", "seamless".

## Checked by

- `scripts/check_apps.py`: manifest fields, the description file, the licence, `CHANGELOG.md` and its version, the
  README's pointer to it, a scenario test, a built `ui/dist`, the launcher icon (`docs/app-contract.md`, *How the
  check works*).
- `docs-work/readmes/_tools/check.py` and `docs-work/_toolkit/docgen/mmcheck.mjs`, outside the repositories, run by
  hand.
- Reviewed, not checked: `SECURITY.md`, `docs/`, the README's sections and every claim in it.

## See also

- [03-the-manifest.md](03-the-manifest.md), [15-testing-and-ci.md](15-testing-and-ci.md),
  [16-releasing.md](16-releasing.md), [20-writing-for-people.md](20-writing-for-people.md),
  [21-apis-and-docs.md](21-apis-and-docs.md)
- `docs/app-contract.md`, `scripts/check_apps.py`
