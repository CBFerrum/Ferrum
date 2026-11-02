# Plan

Workstreams in dependency order, from the handoff's section 27. Scope is not reduced by this order: what comes
later still comes. Security, accessibility, documentation and tests belong to every workstream.

| Workstream | Depends on | First deliverable | Gate |
|---|---|---|---|
| W00 Baseline | | This directory | G0 |
| W01 Identity, policy, control plane | W00 | `ferrum-app-cloud`: projects, environments, operations, canonical digests, the policy decision, approvals, a controller loop | G1 |
| W02 Resources | W01 | Requests, instances, bindings; PostgreSQL and object providers against real servers | G2 |
| W03 Code | W01, W02 | The Gitea provider against a real Gitea: repositories, keys, protection, webhooks, backup and restore | G3 |
| W04 Build | W01 to W03 | Build intents, a Docker sandbox, a fair scheduler, a trusted publisher, provenance | G3 |
| W05 Deploy | W04, W02 | Releases, approvals, `ferrum-serve`, previews, domain claims | G4 |
| W06 SSR and workers | W05 | One Node profile, cache rules, rolling release | G5 |
| W07 Access | W01, W05, W06 | Catalog, grants, enforcement acknowledgments, proxy identity | G5 |
| W08 Distributed execution | W00 to W02 | Committed event cursor, connection ownership, charging sessions | G6 |
| W09 Corporate operations | W02, W05, W07, W08 | Metering, budgets, partner delegation, recovery drills | G7 |
| W10 Qualification | All | Golden journeys against exact artifacts, evidence bundle | G8 |

## What waits on someone else

Nothing here is simulated to get past it. Each stays open in [blockers.yaml](../registry/blockers.yaml).

| Blocker | Owner | Safe behaviour meanwhile |
|---|---|---|
| Production domains and DNS (BLOCK-PRODUCTION-DOMAINS) | Platform owner | Test domains only |
| Provider contracts and managed-use rights (BLOCK-PROVIDER-CONTRACTS) | Legal and commercial | Adapters off for that deployment mode |
| Timescale licensing (BLOCK-TIMESCALE-LICENSE) | Legal and platform | PostgreSQL, or customer-owned |
| Real IdP, network, MDM and PAM tenants (BLOCK-REAL-TENANTS, BLOCK-IDP-TENANT) | Security | Contract tests, labelled software-only |
| Payment provider sandbox (BLOCK-PAYMENT-KEYS) | Business owner | No real charges |
| Independent security review (BLOCK-SECURITY-REVIEW) | Security | No such claim |
| Accessibility audit (BLOCK-ACCESSIBILITY-AUDIT) | Product | Automated checks only |
| Chargers and equipment (BLOCK-EV-HARDWARE, BLOCK-INDUSTRIAL-HARDWARE) | Integrator | Simulators; no physical control |
| Residency and retention policy (BLOCK-RESIDENCY-POLICY) | Legal | Conservative defaults; transfers refused |
| CDN, WAF and PAM tier (BLOCK-EDGE-SERVICE-TIER) | Commercial | Limits stated |
| LTS and support (BLOCK-LTS-SUPPORT) | Engineering | No SLA promised |
| Partner terms (BLOCK-LEGAL-REVIEW) | Legal | Configurable program; open-source rights stay separate |
