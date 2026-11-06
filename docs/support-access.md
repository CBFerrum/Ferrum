# Support access

A workspace's administrators give support access, in layers. Nobody at Ferrum or a partner holds a
standing way in through the product.

| Level | Reaches | Longest |
|---|---|---|
| `health` | Versions, packages, queue sizes, and counts of active people and stored bytes | 30 days |
| `diagnostics` | That, and what failed: job and event errors, without their payloads | 7 days |
| `assistance` | Named entities, read-only unless the customer allows writing | 3 days |
| `incident` | Named entities, or all of them, read and write if allowed. Needs a confirmed sign-in | 1 day |

`POST /api/admin/support` `{level, grantee, entities, can_write, reason, hours}` answers an API key,
shown once. It runs as an account of its own with its own role, so it goes through every check anyone's
request does, and its changes are on the audit trail under its own name. It is never an administrator:
it cannot manage people, keys or other grants, and cannot extend itself.

A grant ends when it expires, and `DELETE /api/admin/support/{id}` ends it at once. `GET /api/admin/support`
lists every grant, past and present, and `GET /api/admin/support/{id}/requests` every request made with
one, reads included. Administrators are told when a grant is made and when it is first used.

`GET /api/support/report` is what `health` and `diagnostics` keys reach. At every level the report also
counts what the workspace uses: its active people and the bytes its files take (`usage`). An operator
charges for usage beyond a plan from these. They are counts only, never who the people are or what the
files hold.

## What this does not say

This limits what the product gives support. It is not a claim that nobody else can reach the data.

Whoever runs the servers can reach what is on them. Someone with the database's credentials reads every
workspace, because row-level security binds the application's role and not an administrator of the
database. Someone with `FERRUM_SECRET_KEY` as well opens sealed values. On Ferrum Cloud that is the
operator, and on a partner's hosting, the partner. Backups hold the same data. A customer who needs
stronger separation needs a dedicated deployment they control, with keys they hold.

Partner staff get no access by being partners: only what a customer grants them here.

## Account states and usage

Where a workspace's account stands is the operator's to set: `ferrum task account_state slug:acme
state:restricted note:"Invoice overdue"`.

| State | What it means |
|---|---|
| `active` | Everything works. |
| `grace` | Everything works, and `GET /api/admin/usage` says why it is in grace. |
| `restricted` | Read-only for everyone. Reading, reporting and exporting the workspace still work. |

A billing problem never touches data, and export is a right, not a feature. What happens after that (how
long a restricted workspace is kept, and when it is deleted) is a commercial decision, and no code
deletes a workspace for it.

`GET /api/admin/usage` shows storage, people, AI tokens and jobs now, and day by day: readings are kept in
`usage_day`, so a bill can be explained later from the same numbers. Ferrum sets no prices. A storage
quota is the operator's, in `tenant.quotas.storage_bytes`; reaching it refuses new files and nothing else.
Operations adds two more: `measurements_per_day`, past which gateways hold their measurements until
there is room, and `devices`, past which no new device can be added. Commands, leases and reports go on
either way.
