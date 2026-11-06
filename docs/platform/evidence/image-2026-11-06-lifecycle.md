# The released image, 6 November 2026: a workspace from sign-up to deletion

The tenant lifecycle (FERRUM-JOURNEY-012) had been walked in the kernel's suite (`closure_it`) and
once in a browser on a server built on one machine ([4 November](browser-2026-11-04.md)). This time it
ran on the image CI publishes: `ghcr.io/cbferrum/ferrum:latest`, digest
`sha256:cb3318e8b8726b67eef86d5b5298bf83868116977f90f695de3a5bd31fbf32dc`, built at 01:58 UTC from
main at 5a6c4b7 (PR 81), for amd64. It was pulled from GHCR and run with `deploy/docker-compose.yml`
under Docker on one Mac (colima, amd64 through Rosetta), with the stack's own Postgres 17, Mailpit
for mail over SMTP, the base domain `localhost` on port 8080, and two plans in `FERRUM_PLANS`: free
(3 people, 100 MB of files, 100,000 assistant tokens) and team (50 people). Nothing in the image was
changed.

Rhea, a new customer, did her part through the pages, from 06:23 to 06:33 UTC. The operator's
part ran as `ferrum task` inside the container, against the stack's database.

## What was walked

| Step | What it showed |
|---|---|
| Sign-up | At `accounts.localhost:8080/signup`, her name and e-mail: "We emailed a code to rhea@lighthouse.test". The code came over SMTP to Mailpit; with it and a password her Ferrum account was made |
| Her workspace | Named Lighthouse Cafe, its address filled in as `lighthouse-cafe`; made with her as its administrator and listed on her account; opened on its own host, `lighthouse-cafe.localhost:8080`, at "Set up this workspace" |
| The plan's quotas | Administration, Workspace: the free plan, People 1 of 3, Files 0 of 100 MB, Assistant 0 of 100,000 tokens. Omar and Sana added from Users (3 of 3); a fourth refused on the page: "this workspace has room for 3 people on its plan: turn someone off, or ask for a larger plan" |
| A larger plan | The operator's `set_plan slug:lighthouse-cafe plan:team`; the fourth, Tomas, then added; Workspace read the team plan, People 4 of 50, no limit on files or tokens |
| A record | The company, Lighthouse Cafe Ltd, made from its page |
| Closing it | Administration, "Close this workspace", its address typed to confirm: "Close lighthouse-cafe? Everyone can still read and export it, and nobody can change anything. It is kept for 30 days, after that its operator may delete it." Closed: "Closed. It is read-only, and kept until 2026-12-06" |
| Read-only | A new unit of measure refused on save, the workspace being read-only; the company still read through the API (200) |
| Deleting it | `delete_tenant` before its date refused: "lighthouse-cafe is kept until 2026-12-06". The keep date was then moved in the database to 5 November, to stand for the 30 days passing. The dry run listed what would go (4 people, the company, 23 audit events and the rest) and deleted nothing. Applied, it stopped to ask for `GRANT SET ON PARAMETER session_replication_role TO ferrum`, as [the operations guide](../../operations.md) says; granted, it deleted the workspace (recorded in `tenant_deletion`), and the grant was revoked |
| After | Rhea's account outlives the workspace: at the accounts host, "No workspace yet". `lighthouse-cafe.localhost:8080` reads "No workspace here" |

## What it found, and what changed

| Finding | Now |
|---|---|
| The closed workspace's notice named who closed it by an id: "closed by its administrator, 01a1af88-…" | It names their e-mail |
| A closed workspace refused a save with "read-only until its account is settled", which reads as an unpaid bill | "this workspace is read-only now that its account is closed or restricted; you can still read and export everything" |
| `account_state slug:lighthouse-cafe state:closed retain_until:` with a past day refused with only "validation failed" | Every task's refusal gives each field's reason: "retain_until: a date after today: a closed workspace is kept for a while" |

## What it does not show

- One machine, amd64 under emulation: no deployment, TLS, real DNS or mail provider, and no arm64
  image.
- The 30 days were stood in for by moving the date in the database; the wait itself was not lived.
- No payment: the operator moved the plan (BLOCK-PAYMENT-KEYS).
- The fixes above are in the next image, not in this one; they were checked on a debug build.
