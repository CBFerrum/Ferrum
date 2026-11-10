# Builder first

The program that makes industries configuration rather than apps: [the plan](../plan/builder-first.md),
its [decisions](decisions.md), what the apps write for themselves today ([inventory](inventory.md)), and
where each requirement stands ([traceability](traceability.md), generated from
[docs/registry/bf.yaml](../registry/bf.yaml)).

Requirements are `BF-*`, one acceptance case each, in [catalog.json](catalog.json). Their states are the
platform program's ([docs/platform/README.md](../platform/README.md)): `verified_software` needs the tests
that show it, a state beyond that needs a release digest and an independent reviewer, and every row says
what it does not yet do.

Gates land in order: BF0, then BF1 to BF4, BF5 to BF8, and BF9. Each lands as pull requests merged after
local CI at the tested head, with its rows here and a deliberate break for each new invariant.

## Inventory

```
scripts/builder_inventory.py <folder of ferrum-app-* checkouts> --write
```

searches each checkout's committed tree and writes [inventory.md](inventory.md). Clone the app
repositories into one folder first (`gh repo list CBFerrum` lists them).
