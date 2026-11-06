# A browser on a running server, 6 November 2026 (afternoon): Digital Assets' view page, Property's owners' portal, Website on its own domain

The walk server (port 5191) ran PR 88's build: DAM 0.5.0, Property 0.6.0 and Website 0.1.3, with
the shell at Frontend 3d36821. The walk used workspace walk-intranet in the desktop app's browser
pane (Chromium), signed in as its administrator, and then as a portal person who signed in with an
e-mailed code.

| What | Where | Seen |
|---|---|---|
| Installing from the App Manager | Apps → Store | Digital Assets 0.5.0 installed Drive 0.14.2 with it; Property 0.6.0 installed ERP 0.14.4 with it. The install dialog listed the new entities |
| A PDF seen whole | DAM → library → Spring brief → `/dam/view/{asset}` | A one-page PDF uploaded to Drive and made an asset, then submitted for review on its record. It was listed under Waiting for review, and its title opened the view page. That page showed "In Review" and a link to the record, with Chromium's PDF viewer in the frame showing "Spring brief: page one", page 1 of 1. `GET /api/app/dam/preview/{asset}` answered 200 `application/pdf`, `inline; filename="spring-brief.pdf"`, `cache-control: private, no-store`, `x-content-type-options: nosniff` and `cross-origin-resource-policy: same-origin`, with the 597 bytes stored |
| A video played | DAM → Launch clip → view page | A 2.4 s WebM (320×180, recorded from a canvas in the page) played in the page's `<video controls>` from the preview route: ready state 4, 1.5 s played, no error. Seeking ahead and Safari wait for byte ranges (Ferrum PR 87) |
| Domains offers only sites that serve pages | Administration → Domains → Add a domain | "Serves" offered This workspace, The portal, and Website's public site. Digital Assets and Property, whose handlers serve no pages, were not offered (PR 85) |
| A `.localhost` domain working at once | Administration → Domains | `walk-site.localhost` for Website's public site was listed Working, checked now, with no database edit (PR 88) |
| The public site on its own domain | `http://walk-site.localhost:5191/` | The home page with its menu (About us, Blog), the post listed with its day, and the footer, styled by `/assets/site.css` (21 rules loaded; the earlier nip.io host's CSS was blocked by the pane itself). The post rendered its Markdown (bold, a link to About us). `/blog` 200; `/robots.txt` names `http://walk-site.localhost:5191/sitemap.xml`; `/sitemap.xml` lists the domain's addresses; an unknown page is 404 |
| Properties you own, for someone who owns none | Portal → Properties you own, as the administrator | "No property here names you as its owner" |
| Properties you own, for the owner | Portal, Olive Owner signed in with a code | A building made through Property's form with Owner olive@owners.test, its two units, and a lease of A1 to Lena Lessee activated. Olive was added as a portal person in Administration → Users and signed in with the e-mailed code. Properties you own showed Quay Works, 1 of 2 units let: A1 Let to Lena Lessee until 2027-09-30 at 1,200 monthly, and A2 Vacant |

Found and fixed:

| Found | Fix |
|---|---|
| A building's Owner field reads as if the address alone lets its owner in. The portal sends codes only to portal people, whom an administrator adds under Users | Property's Owner description now says to add them as a portal person (next Property release) |
