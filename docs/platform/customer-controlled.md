# Running in a customer's own network

What an operator can do there, what it needs, and what happens when it is cut off from everything. This describes
the software. No customer environment has been run, so nothing here has been qualified (`FCP-CLOUD-008`).

## What it depends on

| Needs | For | Without it |
|---|---|---|
| PostgreSQL 17 | Everything | Nothing works |
| Object storage | Files, artifacts, copies | Those stop. A filesystem path works for one machine |
| A container runtime | Builds and running services | Builds and those services stop. Sites keep being served |
| Gitea, a static binary | Code | Code stops. Sites keep being served |
| Outbound internet | Nothing that is required | See below |

Nothing asks a licence server. Every dependency is open source and can be run inside: `docs/licenses.md` lists the licence
of everything compiled in, and a provider with other terms is refused until its terms are approved (FCP-RES-014).

## When it is cut off from the internet

| Function | What happens | Evidence |
|---|---|---|
| Sign-in with passwords and passkeys | Goes on | `passkey_it` |
| Sign-in through an outside identity provider | Stops for those people | By design |
| Business records, reports, workflows | Go on | They have no outside dependency |
| Sites on `ferrum-serve` | Go on, from what is on disk, including after a restart | `serving_it` |
| Members' sites with the management plane unreachable | Stop after an hour | `serving_it` |
| Gateways and what they run | Go on, under their own authority, and report later | Edge `recovery_it` |
| Mail, webhooks and connectors to outside | Wait, and go out afterwards | Outbox tests |
| The assistant, when its model is outside | Stops, and says so | `assistant_egress_it` |
| Store apps and updates | Nothing new arrives. A signed bundle can be carried in | `docs/operations.md`, `ferrum task app_make_available` |
| Certificates from a public authority | Stop renewing, and expire on their date | Not exercised |

## What an operator can do there

| | With what | Recorded |
|---|---|---|
| Read a workspace's data | Nothing by default. A grant from the customer, with a level and an expiry (`docs/support-access.md`) | Yes, and the customer can read it |
| Change how it runs | The deployment's own access, which the customer controls here | Their own |
| Reach plaintext | Whoever runs the database and the keys can. In this profile that is the customer | Theirs to decide |

Being a partner of ours grants nothing in a customer's workspace. Each customer grants named access (`FCP-CLOUD-004`).

Nobody here can honestly claim that an operator of a machine cannot read what that machine processes. Customer-held keys mean
data is sealed from people without them, and not from whoever runs the process using them.
