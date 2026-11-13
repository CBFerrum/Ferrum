# Cloud: references

**Compared with:** Forgejo v17.0.1, Gitea v28.3.0, GitLab (its documentation), Caddy, kamal-proxy, Pingora,
instant-acme, Coolify, Railpack, BuildKit, Knative Serving, Falco's rules, k6 and cargo-mutants, read on
2026-11-03 through their public repositories, against Ferrum Cloud `abda176` (`ferrum-app-cloud`, main).
None is in `referance/` yet: this disk has about 200 MB free and `referance/fetch.sh` stops under 1.5 GB.
Reference paths below are in each project's own repository.

Cloud was built from the platform program's acceptance cases without a reference set, so some of what it
built by hand is already done, and done better, by projects that are mature and open. The rule from here
(`referance/README.md`): run a mature project as a component, depend on it as a library, or adapt its code
where its licence allows, and write only what is left.

## What Cloud already takes from mature projects

Gitea as the forge, CloudNativePG for the platform's database, Envoy Gateway and cert-manager for the
platform's own hosts, gVisor for builds and services, verdaccio as the package mirror builds reach, and
`sqlx`, `axum`, `reqwest`, `opendal`, `ring` and `ed25519-dalek` in the code.

## What Cloud built that a mature project already does

| Cloud built | Where in Cloud | What does it already | Licence | How to take it | Size (S/M/L) |
|---|---|---|---|---|---|
| A storage quota per workspace: sizes read every five minutes, repositories archived while over (Cloud #8, #10) | `src/code/controller.rs` (`storage`), `src/code/api.rs` (`held_for_room`), `packages/cloud/sql/017_forge_storage.sql` | Forgejo's quota: groups and rules per user or organisation, refused at push, covering repositories, LFS, attachments, artifacts, packages and wikis | GPL-3.0, run as its own service | Run Forgejo instead of Gitea; one quota group per workspace through `/admin/quota/*`; remove the round and its columns | M |
| Certificates for customers' domains: none (FCP-WEB-007 is partial) | `src/deploy/domains.rs` proves ownership; nothing issues | Caddy's on-demand TLS: a certificate is obtained when a name is first asked for, only if an `ask` endpoint says yes, renewed and rate limited by Caddy | Apache-2.0 | Caddy in front of the serving node, its `ask` answered from Cloud's verified domains; tested against Pebble, Let's Encrypt's test CA | M |
| The serving node: host routing, static files, proxying to replicas, the previous release's files, WebSockets, limits (917 lines) | `serve/src/node.rs`, `serve/src/sync.rs` | Caddy (file server, reverse proxy, configuration through its admin API); kamal-proxy (zero-downtime deploys with automatic TLS); Pingora (a Rust framework for proxies) | Apache-2.0; MIT; Apache-2.0 | Keep it behind Caddy first; decide later whether Caddy or Pingora replaces it | M |
| Egress and mining watch: none (FCP-OBS-009 names it) | `src/abuse.rs` reads CPU only | Falco: kernel-level rules for outbound connections and known mining pools, per container | Apache-2.0 | Falco on the hosts; its alerts open a restriction with the evidence, through the existing stop and appeal | M |
| The soak's load generator | `ferrum/scripts/soak.py` | k6: scenarios per workspace, thresholds, its own report | AGPL-3.0 | Generate the load with k6; keep the outages and checks around it | S |
| Mutation tests as hand-written text swaps | `scripts/mutants.py` in both repositories | cargo-mutants: mutants generated from the code | MIT | Add it for generic mutants; keep the hand-written ones, which break a named guarantee | S |
| Builds: a profile's command in a locked-down container, files as the artifact | `src/build/sandbox.rs`, `src/build/controller.rs` | Railpack (detects the framework and builds an image on BuildKit); BuildKit (rootless, cached builds); Forgejo Actions | MIT; Apache-2.0; GPL-3.0 | Evaluate. Images instead of files would change what a release is | L |
| The runtime: `docker run` per service, health, drain | `src/deploy/runtime.rs` | Kubernetes Deployments with a gVisor RuntimeClass and NetworkPolicy; Knative Serving for revisions and traffic splits | Apache-2.0 | The way to the blocked qualified runtime with egress enforced (`BLOCK-SANDBOX-QUALIFICATION`) | L |
| A gate in front of the forge: clones and pushes take turns per workspace (118 lines) | `serve/src/gate.rs` | Nothing equal in Forgejo or Gitea; GitLab's Gitaly limits concurrency per repository | MIT (Gitaly) | Keep; read Gitaly's limiter before it grows | - |

What stays Cloud's own: its contract and policy (`contract/`), approvals and plans, operations and their
leases, metering and budgets, restrictions and appeals, and the console. That is what makes it Ferrum's
rather than a forge or a proxy.

## The forge: Gitea, Forgejo or GitLab

- **Gitea 28.3** has no quota per owner and no repository size limit (`custom/conf/app.example.ini`). The
  only bound is Git's own `receive.maxInputSize`, which Cloud sets.
- **GitLab:** the repository size limit is Premium and Ultimate only
  (`doc/administration/settings/account_and_limit_settings.md`), so its free edition has none either. It
  also runs as several services needing gigabytes of memory, and its `ee/` folder is not open.
- **Forgejo 17** has the quota in its free software:
  - `models/quota/` holds the groups, rules and usage;
  - `[quota]` is read in `modules/setting/quota.go`;
  - a push over quota is refused in `routers/private/hook_pre_receive.go` (`checkQuota`, "Quota exceeded");
  - the admin API is in `routers/api/v1/admin/quota*.go`, and `/orgs/{org}/quota` reads an organisation's usage.
- **Forgejo against Cloud's adapter.** Every path the adapter calls is in Forgejo's API
  (`templates/swagger/v1_json.tmpl`) but one: Gitea's repository tokens, `/repos/{owner}/{repo}/keys/tokens`.
  Forgejo restricts an access token to chosen repositories instead (`CreateAccessTokenOption.repositories`),
  so a machine user for each workspace would hold them.

D-08 chose Gitea and called Forgejo "an equal alternative, not both". The quota is now the difference.

## Order, and where each stands

1. **Forgejo with its quota**, and Cloud's storage round removed: done (Cloud #11, D-08). Forgejo also runs from its
   own manifest, checked in CI as it is configured.
2. **Caddy's on-demand TLS** for customers' domains, tried against Pebble, and **Prometheus's blackbox exporter** for
   an alert when one is not being renewed: Cloud #12.
3. **cargo-mutants** beside the hand-written mutants: on the contract crate first, where it found 82 changes no test
   noticed, among them the address guard's ranges, token lifetimes and a refusal of IPv6 imports without a port.
4. **Falco** for the egress and mining watch, with the Kubernetes runtime and not before. Today's runtime is Docker,
   for development and CI only (`ferrum-app-cloud:docs/operating-deploy.md`), and Falco watches the hosts that
   customers' workloads run on: wiring it to a runtime that will not run in production would prove nothing about the
   one that will.
5. **An evaluation of Railpack or BuildKit builds and of a Kubernetes runtime**, which is also the way to the blocked
   qualified runtime.

**k6, looked at and not taken for now.** The soak's checks are about windows of time around outages: which requests
failed while the database restarted, and how one workspace fared in the windows without one. k6 gives per-request
times only through its outputs, which for three hours of a soak are gigabytes on a CI runner, or through a Prometheus
to write them into. The soak keeps its own small load generator (`scripts/soak.py`, a few dozen lines), and k6 is
the choice once the soak runs against a deployment that has a Prometheus.
