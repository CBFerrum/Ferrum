# Incidents

What happens when something goes wrong in production. This has not been used: no tabletop has been run and no
named people hold these roles (`docs/platform/services.md`). Until they do, this is a draft.

## Severity

| | What it means | Who is told | Update every |
|---|---|---|---|
| SEV-1 | One workspace can see another's data, a signing key is out, a safety path is affected, or everyone is down | Everyone on call, security, whoever speaks for the company | 30 minutes |
| SEV-2 | One workspace or one service is down, or something is being lost | On call | 1 hour |
| SEV-3 | Something is degraded and has a way round it | On call, in working hours | When it changes |

## Who does what

One person holds each of these, and says so at the start. On a small team one person may hold two, never the first and last.

| Role | Does |
|---|---|
| Incident lead | Decides. Keeps the timeline. Does not touch anything |
| Operator | Changes things, and says what before doing it |
| Communications | Tells customers. Says what is known, what is not, and when the next word will come |
| Scribe | Writes down what was seen and done, with times |

## In order

1. Say it is an incident, and its severity. Raising one and standing it down costs little.
2. Stop it getting worse. Each of these has its own page and is safe to do before the cause is known:
   - A workspace reading another's data: suspend the workspaces involved, and keep everything.
   - A key out: `POST /keys/compromised`, then replace trust files (`ferrum-app-cloud/docs/operating-deploy.md`, Keys).
   - A dependency down: writes stop by themselves where they should. Say so, and do not route around it.
   - A site or project being misused: `ferrum task cloud_restrict`.
   - A restore: nothing goes out until a person releases it (`docs/operations.md`, After restoring a backup).
3. Keep the evidence before changing it: the audit trail (it cannot be edited), `ferrum task doctor`, logs by correlation id.
4. Tell customers what affects them, in plain words, by the channel they have. A leak of personal data has legal deadlines, and
   whoever speaks for the company decides what they are.
5. Fix it, and say when it is fixed and how that is known.

## Afterwards

Within five working days, for every SEV-1 and SEV-2: what happened, a timeline, what let it happen, what made it worse or
better, and who is doing what by when. It says what the system did and never whose fault it was. What it changes goes into the
tracker, and anything customers were told goes back to them.

## A drill

Run these with people before relying on any of it, and write down what did not work.

| Drill | What it exercises |
|---|---|
| A workspace reports seeing another's invoice | Containment without destroying evidence, the first hour of communication |
| A signing key found in a public bucket | Declaring it, trust files on every node, who to tell |
| The database provider is out for three hours | Saying nothing is lost, what stops, how to tell when it is back |
