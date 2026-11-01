# Support and compatibility policy

> [!IMPORTANT]
> **Draft, not in force.** Support windows are a commitment to customers, and only the project's owner
> can make one. The rules below describe how releases work today; the numbers marked *proposed* await
> that decision. Until then, no version of Ferrum carries a support promise.

## Versions

Every commit that passes CI and the cluster qualification is a release, tagged by its commit and named by
image digest. `latest` moves only after that qualification. Packages and apps carry their own versions.

## Which version is supported

*Proposed*: the latest monthly release line and the one before it receive fixes; security fixes reach
both for 12 months after a line is cut. A long-term line, if there is one, is named in advance.

`GET /api/version` says what a server runs. `ferrum task doctor` reports on its health.

## Upgrades

| Path | What holds |
|---|---|
| To the next release | Tested on every merge: the smoke test upgrades the last published image in place, with data. |
| Skipping releases | Migrations run in order, so this works, and is not tested for every pair. |
| Rolling, with replicas | Each migration must work with the release before it (expand, then contract later). `ferrum task upgrade_back` covers what ran after an upgrade that had to be rolled back. |
| Downgrading | Not supported. Restore from a backup. |

Back up first. [docs/operations.md](operations.md#upgrades) has the steps.

## Compatibility

| Surface | Promise |
|---|---|
| REST API (`/api/v1`) | Fields are added, not removed or changed in meaning, within a major version. *Proposed*: at least 6 months' notice before removal, with a `Deprecation` header. |
| Events and webhooks | The envelope carries `schema_version`; consumers must ignore fields they do not know ([docs/events.md](events.md)). |
| Plugin host interface | Versioned by `api_version`. An app declaring an interface the server does not offer is refused at installation, not at run time. |
| Native hooks and `ferrum-kernel` | No promise: they change with the code they are compiled with. |
| Metadata (packages, overlays) | New fields have defaults. A package needing a newer platform says so. |
| Customizations | Survive upgrades. Where an upgrade changes what one replaced, administrators are told ([docs/operations.md](operations.md)). |

## Security updates

Report vulnerabilities as [SECURITY.md](../SECURITY.md) says. Fixes go to the supported lines, and advisories
accepted for now are recorded with an owner and a review date in
[docs/registry/advisories.yaml](registry/advisories.yaml).

## End of support

A release past its support window keeps running. It gets no fixes, and upgrading from it may need passing
through a supported release first. *Proposed*: the end of a line is announced at least 3 months ahead.
