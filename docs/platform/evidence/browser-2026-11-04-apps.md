# A browser on a running server, 4 November 2026: the apps compiled in today

The server was built on this machine, in debug, from this PR's branch (`platform/pins-control`) with its
default features, so every app compiled in today was in it: Government, Retail, IT Services, Governance and
Risk, Contracts, Objectives, Digital Assets, Metrics, the Control Plane and Social, beside the earlier ones.
It ran against a database of its own (`ferrum_walk`, never `ferrum_dev`) with these settings:
- `FERRUM_BASE_DOMAIN=localhost`;
- a bootstrap token and a secret key made for the run, kept out of the record;
- the development mailer, which keeps mail in the outbox.

The interfaces were Ferrum-Frontend main at c733db5 (PR 16, the launcher's icons) with PR 17 on top. A
workspace, *Walk*, was made through `POST /api/tenants` with the packages control, social, metrics,
objectives, dam, government, contracts, grc and itsm (and what they depend on). It was driven in the desktop
app's browser pane at `accounts.localhost:5191` and `walk.localhost:5191`.

## What was done, and what it showed

| Step | Result |
|---|---|
| Sign in as the workspace's administrator | A Ferrum account set up with the code from the outbox; the workspace's home |
| The launcher | Every app's tile drew its own icon: the Control Plane's `server` and Social's `share-2` among them |
| `/control/`, then a new cell through its form | The console with no rollouts or incidents; the form with its read-only fields and its token as a secret, saved sealed |
| A report from a cell's tooling, `POST /api/site/walk/control/report` | With the token: degraded, "disk at 95%". With another token: refused. The console showed the cell in its region, degraded, with the problem |
| `GET /api/site/walk/control/status`, signed out | The cell's state by region, no incidents, no maintenance, and no customer named |
| A hosted workspace through its form, its cell left empty | Placed in eu-1 by itself; the console counted 1 of 2 workspaces |
| `/social/`, `/kpis/`, `/objectives/`, `/dam/`, `/government/`, `/itsm/`, `/contracts/` | Each home page drew its queues or its empty states |
| `/grc/` | **Failed**: "unknown filter operator 'neq'" (see below) |

## What it found

- Governance and Risk's home asked for findings with the operator `neq`, which the platform's filters do not
  know (they take `!=`, `<>` or `ne`), so the page showed an error instead of its register. Property's home
  had the same mistake. Both now use `!=` (ferrum-app-grc 4755e20, ferrum-app-property 3501483), and after a
  rebuild the register showed. A scan of every app's interface found no other operator the platform does not
  take. The apps' scenario tests run their rules in Rust, not their pages' queries, so nothing caught it.
- With nothing in them, the Control Plane's cells, Metrics' certified metrics and Objectives' tree showed a
  bare heading or an empty card. Each now says there is nothing yet, and what comes first.

## What it does not show

- The portal pages (a partner's customers, a customer's service, the citizens' and lessees' pages): signing
  in as a portal user was not walked.
- Any flow past a first record: no rollout, incident, post, metric certification, objective or asset was
  taken through its steps here. Those steps are each app's scenario test, against PostgreSQL.
- Social's accounts, which need a connection, and the Connections page's new history, which needs a
  connector this build does not have.
- A deployment, the image, phone widths, dark mode or an accessibility check.
