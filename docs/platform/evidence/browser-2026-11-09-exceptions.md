# A browser on a running server, 9 November 2026: what needs a person (FEO-017)

The walk server was a debug build of the server crate without its compiled-in apps
(`--no-default-features`), from this change's branch on Ferrum main c1815c2. Its interface was built from
Ferrum-Frontend's `feo/exceptions` branch on main 833854a. The database was the walk's own
(`ferrum_feo_walk`), on port 5193, and the development mailer was a stub.

Workspace `walk` was made with `ferrum task create_tenant`, against that checked database, and seeded
straight into it with one of each thing that gives up:
- a `SalesInvoice.submitted` event, dead after five attempts ("handler billing.sync failed: connection
  refused"), whose payload held a value marked "never shown";
- a webhook, "Accounting bridge" to `bridge.example.test`, with a `Customer.created` delivery dead after
  eight attempts on HTTP 503;
- a `platform.export` job, failed after three attempts ("object storage unavailable").

The administrator signed in at `accounts.localhost:5193`. The first sign-in asks for a code emailed to
the address. The stub mailer sends nothing, so the walk set the pending code's hash in its own database
to a known code and typed that.

## What was walked

| Step | What happened |
|---|---|
| Administration, Needs a person | The tab is last in Administration's row. It shows three cards. **Events handlers gave up on** lists the dead event: its topic, the record kind, 5 attempts, why it stopped and how long ago, with **Replay**. **Webhook deliveries that gave up** lists Accounting bridge, `Customer.created`, 8 attempts and "HTTP 503: Service Unavailable", with **Replay**. **Jobs that failed** lists `platform.export`, 3 attempts and why, with **Try again**. No payload appears anywhere: the API sends none. |
| Replay the event | The toast read "Queued again" and the card became "No events waiting". In the database the event was no longer dead, its attempts had started again, and it had been handled. The replay is in the audit trail (`OutboxEvent`, `replay`). |
| Try the job again | "Queued again", then "No failed jobs". The export ran as its requester, the administrator. It finished this time, and the bell showed "Workspace export finished". |
| Replay the delivery | "Queued again", then "No deliveries waiting". The delivery went back to pending and was tried again; this endpoint does not exist, so it waits on its backoff. The replay is in the audit trail (`WebhookDelivery`, `replay`). |
| Buttons for screen readers | Each button names what it acts on, for example "Replay: SalesInvoice.submitted" and "Try again: platform.export". |

## What it found

| Finding | Now |
|---|---|
| `/admin?tab=exceptions` arrived at `/admin/` without its query: the shell's slash redirect (`shell_core`) dropped it, so a link to a tab opened on Users. | Fixed in this change: the redirect keeps the query, as the alias redirect below it already did |
| The first build's notes sat against the cards' edges, and the cards touched. | Fixed before commit: the cards are in a `stack` and the notes in a `card-body`, as in the other tabs |
| The first sign-in's code can only be read from mail, which a development server with the stub mailer does not send. | Recorded: a walk sets the code in its own database |
