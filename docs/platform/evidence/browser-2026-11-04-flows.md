# A browser on a running server, 4 November 2026: the apps' flows and their portal pages

The second walk of the day, after [the apps' home pages](browser-2026-11-04-apps.md). The server was
built in debug from this PR's branch, with the apps pinned in its lock, against its own database
(`ferrum_walk`, never `ferrum_dev`): `FERRUM_BASE_DOMAIN=localhost`, a bootstrap token and secret key
made for the run, the development mailer (mail stays in the outbox, where the sign-in codes were read),
and `FERRUM_WEBHOOKS_ALLOW_PRIVATE`, so a connection could reach a stand-in on this machine. The shell's
interface was Ferrum-Frontend built on this machine: PR 18's branch, then main once PRs 18 to 20 had merged.

Six people took part, each signed in through the pages: the workspace's administrator and Dana, a
second member holding the approving roles (both with Ferrum accounts at `accounts.localhost`, Dana's
set up with the code from the outbox), and four portal people who signed in with e-mailed codes: Pat, a
partner's member; Cass, a customer's contact; Cyril, a citizen; and Lena, a lessee.

The Mastodon account was a stand-in (a short Python server speaking the parts of Mastodon's API the
connector uses), and eu-1's tooling was a loop posting its readings: neither is the real thing.

## What was walked

| App | Flow | What it showed |
|---|---|---|
| Control Plane | A release recorded, then qualified | Qualifying it as who recorded it refused ("a release is qualified by someone other than who recorded it"); Dana qualified it |
| | A partner, certified | Refused without a certification date; saved with one, its member's e-mail lowercased |
| | A cell's token replaced, the cell reporting | Down after 80 quiet minutes; healthy on a report with the new token; a wrong token refused |
| | A hosted workspace with no cell | Placed in eu-1 by itself |
| | An incident told in updates | Created only with its cell and a first update; a status change without an update saying it refused; the public status, signed out, showed both updates and no customer |
| | A rollout of the release | Started by Dana; the wave opened in eu-1's maintenance window and set its desired release (the cell reported "it runs 0.4.0, not the release it should"); upgraded, soaked an hour while healthy, and done at 09:03 UTC, the rollout with it |
| Partner portal (Pat) | Your customers | The portfolio with health, plan, renewal and support access, a note on the missing health key, a case raised (one open case after), onboarding refused with "no shared cell in eu has room" as eu-1 was full |
| Service portal (Cass) | Your service | Her workspace on eu-1 with its maintenance window, the incident and both its updates, and Pat's case with its due time by her Premium plan |
| Social | A Mastodon connection | Listed with what it can do; saved, then tested ("Working"); its history (reads and sends) shown |
| | A post to review, approved, out at its time | A link without `https://` refused; approving one's own post refused; Dana approved; at 08:10 UTC it went out as a status with its link, and the post became published with the network's id |
| | The inbox | A comment and a direct message read in, routed to the account; answered, and the answers reached the stand-in |
| | What posts did | Four engagements (three favourites, a boost) on the brand's line |
| Government | A permit type, then a citizen's application (Cyril) | Applied on Your services, the map uploaded, submitted; Dana took it up, approved it with a note and issued it; Cyril saw his permit valid until 4 December |
| Governance and Risk | A risk and its acceptance | Scored on the matrix (inherent 20, current 15, High); accepted by Dana, who does not own it |
| Objectives | An objective, a key result, a check-in | The check-in moved the key result to 50% and the objective with it |
| IT Services | An incident | High impact and urgency gave P1, due four hours after it opened |
| ERP | A company and its chart of accounts | 55 accounts; then the rent's invoice submitted |
| Property | Installed through the App Manager, then a lease | The plan listed what it adds; the lease activated and its first month invoiced |
| Lessee portal (Lena) | Your home | Her lease, the rent invoiced, a repair asked for (MR-2026-00001, open); the invoice opened on Your invoices once issued |
| Metrics | A metric of open IT incidents | Certifying it as its owner refused; Dana, a data steward, certified it; recorded, it read 1 with its lineage |
| Contracts | A contract to its signature | Submitted for approval above the threshold; Dana approved it; its signature is below |
| Digital Assets | An image from Drive to its use | Uploaded to Drive, made an asset (its kind, type, size and digest read from the file), submitted and approved; used in advertising refused for its editorial-only licence ("an editorial-only asset is not used to sell"), used on the web recorded |

## What it found, and what changed

| Found | Fixed in |
|---|---|
| A record whose title field is a link showed the linked record's id as its heading or subtitle (a risk acceptance's risk, a rollout's release, a lease's unit) | Ferrum-Frontend PR 18 (`recordTitle` and the form's heading take `_titles`); the Control Plane's console asked for titles too |
| Every input on the Control Plane's portal pages had no accessible name: apps wrap inputs in the kit's `Field` without `htmlFor` | Ferrum-Frontend PR 18: `Field` names a single control itself |
| A Social account named its connection by an id that no page shows | ferrum-app-social 0.2.1: by its name, as the Connections page shows it |
| A message had no way to be answered from it | Social 0.2.1: Reply on the message; 0.2.2: its box no longer starts with the message's own text |
| Answering a direct message on Mastodon went out as a public status, which would publish a private conversation and, naming no one, not reach its author | Social 0.2.2: a reply is as open as what it answers and names its author |
| The partner page told a portal person who is no partner's member "Something went wrong… Try again" | ferrum-app-control 0.1.1: says the page is for a partner's members |
| "1 months" on the citizens' page | ferrum-app-government 0.1.1 |
| The lessee's rent linked to a draft invoice, which Your invoices does not show ("invoice not found") | ferrum-app-property 0.1.1: linked once issued; a draft says it is not issued yet |
| Send for Signature was offered for an approved contract without its PDF, then asked to attach it, which an approved contract refuses | ferrum-app-contracts 0.1.1: offered only with the PDF, which the field says to attach before submitting |
| A connection's tiles waited after "Sync now"; "1 records"; an install plan printed "Native: true" | Ferrum-Frontend PRs 19 and 20: the list looks again every 3 seconds while work waits; "1 record"; "Built into the server: it runs in the server itself, not in a sandbox" |
| An objective's owner was left empty and the save refused | ferrum-app-objectives 0.1.1: whoever records it, unless they name someone |

After the fixes, the server was rebuilt with the new pins and the walk repeated each: the rollout headed
"0.5.0" and the console named it so; the partner page's eleven fields were named; Cyril read "1 month"
and, on the partner page, that it is for a partner's members; Lena's invoice opened (SINV-2026-00001,
EUR 950.00 due); a new direct message from Kim was answered with Reply, whose box was empty, and went out
as a direct status naming @kim; the approved contract without its PDF no longer offered signature.

The onboarding refusal looked swallowed at first; it was a toast that had faded before the page was
read, and shows ("no shared cell in eu has room").

## What it does not show

- Each app bundles its own record pages, so the Frontend fixes reach an app when it is built again.
  All 36 apps with interfaces were rebuilt against Ferrum-Frontend 80ff179 as patch releases and this
  PR's lock takes them; after the rebuild, Governance and Risk's acceptance read "Single cell for the
  EU region" under its number.
- Payment, a fee invoiced for a permit, a contract's signature through Sign, a recovery request, a
  statement, a dispute, phone widths, dark mode, and a screen reader: not walked.
- The stand-ins: Mastodon and the cell's tooling were small programs on this machine, not the real ones.
