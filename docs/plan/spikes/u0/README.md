# Spike U0 apps

Two tiny apps with interfaces of their own, for checking the shell in browsers
([apps-ui.md](../../apps-ui.md) §17, U0; results in [spike-u0-apps-ui.md](../../spike-u0-apps-ui.md)).
They need no build step: their `ui/dist` is plain HTML, CSS and JavaScript.

* **Deals** (`/deals/`): the shell API from an app's side: `ready`, title and breadcrumbs, its
  own client-side routes, a `pick note` intent, `navigate`, a leave guard, a companion window and
  the live channel.
* **Notes** (`/notes/`): handles `pick note` (`/notes/pick`, in the intent dialog) and is the
  companion window's page (`/notes/call`).

Pack and install them on a development server (unsigned uploads allowed):

    cargo run -q -p ferrum-apps --example pack -- docs/plan/spikes/u0/deals target/u0-deals.fapp
    cargo run -q -p ferrum-apps --example pack -- docs/plan/spikes/u0/notes target/u0-notes.fapp

then upload both from Apps (or `POST /api/admin/apps/upload`) and install them.
