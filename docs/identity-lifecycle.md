# Identity lifecycle

How people arrive in a workspace and how they leave it, by hand or through an identity provider.
Sign-in itself (passwords, passkeys, two-factor sign-in, OpenID Connect) is in
[security.md](security.md).

## When someone leaves

`POST /api/admin/users/{id}/deprovision` with `{"reason": "...", "reassign_to": "<person's id>"}`, or
the identity provider turning them off. In one transaction:

| What | Result |
|---|---|
| Their account | Inactive, so no sign-in path works: password, passkey, emailed code, single sign-on, OAuth. |
| Browser sessions | Ended. Every replica drops its cached copy within moments, through PostgreSQL `NOTIFY`, and a replica that lost that connection clears everything when it reconnects. |
| Open sockets | Live updates and shared documents close at their next session recheck, within a minute. |
| API keys, app passwords | Revoked for good. Turning the person on again does not bring them back. |
| OAuth grants | Refresh tokens revoked, codes spent. |
| Notifications on devices | Push subscriptions removed. Queued mail to them is not sent. |
| Open assignments | Move to `reassign_to`, or the workspace's `deprovision_reassign_to` setting (a person's id). Without either they stay open, and the answer counts them. |
| What packages say they are named on | Moves to the successor right after, in batches (`handover` in a package's manifest): open tickets, leads, tasks. What is finished keeps the name of who did it. |
| Anything an app does beyond that | `ferrum.user.deprovisioned` is published for it: Drive's links and locks, say. |

Their row stays, so their name stays on what they did. The audit trail records the reason and the
counts, by the person's id, never their email.

It refuses your own account, a plugin's service account, and a workspace's last administrator, so
that nobody locks a workspace out. Repeating it is harmless.

With no successor, nothing moves and the answer counts the open assignments. Name one later with
`POST /api/admin/users/{id}/hand-over` `{"to": "<person's id>", "reason": "..."}`.

**What it cannot do.** Files already downloaded, and offline copies on a device, cannot be reached
from here.

## SCIM provisioning

SCIM 2.0 ([RFC 7643](https://www.rfc-editor.org/rfc/rfc7643), [RFC 7644](https://www.rfc-editor.org/rfc/rfc7644))
at `/api/scim/v2`: `Users`, `Groups`, `ServiceProviderConfig`, `ResourceTypes`, `Schemas`.

**Connecting a provider.** Make a dedicated administrator, used by nothing else: its key stops
working when that account does, and SCIM refuses to turn off the account it acts as. Signed in as
that administrator:

```bash
curl -X POST https://erp.example.com/api/auth/api_keys -H 'Authorization: Bearer <session>' \
  -H 'Content-Type: application/json' -d '{"name": "Okta", "scope": "scim"}'
```

Give the provider the tenant URL `https://erp.example.com/api/scim/v2` and that key as its bearer
token. The key opens SCIM and nothing else, and no other key opens SCIM.

**People.** `userName` is the person's email, the identity everything else goes by. It must be an
email address, so map the provider's `userPrincipalName` or `email` to it. SCIM reaches members
only, never portal people, guests or plugins' service accounts, and cannot make anyone an
administrator.

| Provider does | Ferrum does |
|---|---|
| `POST /Users` | Creates a member with no usable password: they sign in through the provider. |
| `GET /Users?filter=...` | Finds people. `eq` on `userName`, `externalId`, `id`, `emails.value` and `active`, joined by `and`. Anything else is an `invalidFilter` error, never ignored. |
| `PUT`, `PATCH /Users/{id}` | Updates email, name and `externalId`. Other attributes are accepted and ignored. |
| `active: false` | Everything under *When someone leaves*. |
| `active: true` | Turns them on again, with no old keys. |
| `DELETE /Users/{id}` | The same, and hides them: SCIM answers 404 from then on. Their row stays. Creating them again brings them back with their history. |

**Groups** are the provider's, and grant roles only as an administrator maps them. A new group gets
the role of the same name if there is one, and otherwise grants nothing.
`GET /api/admin/scim/groups` lists them; `PUT /api/admin/scim/groups/{id}/roles` with
`{"roles": [...]}` maps one, at once for everyone in it. A role given by hand stays when someone
leaves a group that also grants it. Plugins' roles are never granted through groups.

**Conformance.** Tested in `crates/kernel/tests/lifecycle_it.rs` with the request shapes Okta and
Entra document, including Entra's capitalized operations and string booleans. It has not yet been
run against a real provider's tenant.

**Not offered:** bulk operations, sorting, ETags, password changes, `PATCH` on `emails` (`userName`
is the address), and filter operators other than `eq`.
