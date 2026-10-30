# Collaboration like Google Workspace

Plan, 30 October 2026. It answers the request for "full collaborative features like Google
Workspace", and builds on [platform.md](platform.md) (Office in Drive, Mail, the plugin
platform) and [browser-editors.md](browser-editors.md).

## 1. Where Ferrum stands

| Google Workspace | Ferrum today | Plan |
|---|---|---|
| Co-editing documents, sheets and presentations live, with others' cursors | The online editor (Collabora or ONLYOFFICE over WOPI) co-edits Office files live. Browser editors are single-user, one session at a time. | C2: co-editing as the visible default. C3: documents co-edited natively. |
| Comments, suggestions and @mentions inside a document | In the online editor (comments, track changes) | C2 |
| Comments on files, @mentions | Drive comments (Commenter access), no mentions, no screen | **C1** |
| Sharing with people and groups as viewer, commenter or editor; links with expiry | Drive's server (people, teams, roles, links with expiry, password and download caps); no screen | **C1** |
| Request access | None | **C1** |
| Shared with me, Recent, Starred, Trash, search | Drive's server; no screens | **C1** |
| Version history: restore, download | Drive's server; no screen | **C1** |
| Activity: who did what | Drive's server; no screen | **C1** |
| Who is here now | Only the holder of an editor lock | **C1** |
| Notifications in the bell and by mail | The bell with live updates. Shares notify, but a Drive item in the bell opens a form only administrators may see. | **C1** |
| Comments, mentions, to-dos and followers on business records | Built (kernel collaboration) | |
| Chat, mail, shared calendars | Built (Comms) | |
| Calendar and contacts on phones and computers | Built (C5): CalDAV and CardDAV | |
| Video meetings | Built (C4): LiveKit, from chat channels, calendar events and Meet | |
| Forms | Built (ferrum-app-forms) | |
| People outside the workspace | Built (C5): guests by link, in meetings and Drive | |

## 2. Phases

### C1. Drive as a shared workspace

**Status, 30 October 2026: built** (Drive 0.4.1; the web app's Drive page, details panel and Share
dialog). Checked in Chrome with two people:
* a share as Commenter;
* the file under Shared with me;
* a comment with an @mention, told in the bell, and the bell opening the file in Drive;
* "Administrator is editing" in the other's list while one edits;
* a version restored;
* a request for access to a file the other could not see.

Drive's server already did most of this. The web app got its screens, and the server got four
things.

**Server (Drive 0.5.0).**
* **@mentions in file comments.** A comment names people; those who may see the file are told in
  the bell and by mail. The file's owner is told of comments by others.
* **Presence.** The editor pages report every 30 seconds that someone has a file open, viewing or
  editing.
  * Drive answers who has been there in the last 90 seconds, for files the asker may see.
  * Old reports are cleared by Drive's upkeep.
* **Requesting access.**
  * A person who opens a link to an item they cannot see may ask for access. Drive tells the
    item's owner (or a team folder's managers) in the bell and by mail.
  * The asker always gets the same answer, so the request reveals nothing.
  * One request per item and person counts per day.
* **Drive items in the bell** open in Drive.

**Web app.**
* Drive's own navigation: My files, Shared with me, Recent, Starred, Trash, and search.
* A menu on every item: Open, Open with, Share, Details, Star, Rename, Download, Move to trash,
  and Restore or Delete forever in Trash.
* **The Share dialog.**
  * People, with a role: Viewer, Commenter, Editor or Manager.
  * Current shares, with their removal.
  * Links: access, expiry, and copy.
* **Details.**
  * Who is here now.
  * Comments with @mentions.
  * Versions: restore and download.
  * Activity.
* The editor pages report presence.

**Done when:**
* in two browsers, one person shares a file with another as Commenter;
* the other sees it under Shared with me, comments with an @mention, and the first is told in the
  bell;
* both see each other in the file's details while one edits;
* a version is restored;
* Drive's tests cover mentions, presence and requests.

### C2. Co-editing first

* "Open" uses the online editor where it runs, already. When others are present, the details and
  "Open with" say so: join them there.
* The editor page shows who else is in the session, from the online editor's own messages
  (`View_Added` and `View_Removed`). It hands its version history and share buttons to Drive's panels
  (`UI_FileVersions`, `UI_Share`).
* Browser editors stay single-user. While someone holds a file in one, others are told so and
  offered the online editor in read-only.

### C3. Documents co-edited natively

The Google Docs experience without an editor server: a Ferrum document type, edited live by
several people in the web app.

**How.**
* A CRDT: `yrs` (MIT; Yjs in Rust) on the server.
* Clients sync over Ferrum's WebSockets with the Yjs protocol.
* Updates are stored in PostgreSQL and passed between replicas the way the live channel is.
* The editor is an open-source rich-text editor with Yjs bindings (TipTap/ProseMirror or Lexical,
  MIT), with cursors and names.
* Comments and suggestions live in the same document.
* Exports go to PDF through Gotenberg (from HTML) and to DOCX.

**Order.** A spike first: two replicas, ten editors, a lost connection and a restart.

**Spike, 30 October 2026: go** ([spike-c3-yjs.md](spike-c3-yjs.md)). Ten and thirty editors on two
replicas stayed in step:
* a few milliseconds across replicas;
* offline edits merged;
* nothing lost when a replica was killed or both restarted;
* the document read back from PostgreSQL alone.

Two conditions, both settled:
* the server counts text in UTF-16, as Yjs clients do;
* yrs must split emoji like Yjs. The fix is open upstream (y-crdt#715); Ferrum builds yrs with
  it until a release does.

The build (next):
* a platform WebSocket per open document, signed in like `/ws/live`;
* changes in one sequence per document, gathered for ~50 ms;
* NOTIFY between replicas, and a snapshot every 500 changes;
* documents as Drive files, so sharing, versions, comments, presence and search apply;
* TipTap with y-prosemirror as the editor;
* names and colours set by the server.

### C4. Meetings

**Status, 30 October 2026: built** (Comms 0.5.0; the web app's Meet pages; `deploy/meet.yml`).
Checked in Chrome with two people, fake cameras and microphones, against LiveKit 1.13.10:
* a room started from Meet, joined by its link: each sees and hears the other;
* leaving and rejoining;
* a call from a chat channel, announced once in the channel, joined from that message;
* a meeting added to a calendar event by its organizer and joined by the guest from the calendar;
* the call of a private channel refused to someone outside it;
* a phone-width screen.

**How.**
* LiveKit (Apache-2.0) runs as a service of its own, like Collabora. Browsers reach it directly;
  Ferrum signs its access tokens (HS256 with a key and secret both know) and never calls it.
* A meeting is `{kind}/{id}`: a chat channel (whoever may read it), a calendar event (its
  organizer and guests, anyone for a public one) or a room (anyone in the workspace with the link).
  LiveKit rooms are named after the workspace, so workspaces can share a server.
* The web app uses LiveKit's open-source components (PreJoin, VideoConference): camera,
  microphone, screen sharing and the meeting's chat.
* The smoke test runs LiveKit beside the Compose stack: it accepts Ferrum's passes and refuses
  forged ones, and the web app's CSP lets pages reach it.

**Not built:** recording, end-to-end encryption, dial-in, removing someone from a call in
progress, and guests from outside the workspace (C5).

### C5. Beyond the workspace

**Status, 30 October 2026: built** (Comms 0.6.0, Drive 0.5.0, the platform's app passwords and
`/api/dav`; the web app's calendar-apps dialog, guest pages and Drive's link page). Checked:
* a real CalDAV client (python-caldav 2.1.2): discovery from the server's root, the calendar, an
  event created, found by date, read, changed and deleted;
* CardDAV as DAVx⁵ walks it: `/.well-known/carddav`, the principal, the address book home, tags
  and ETags, cards by multiget; a CRM contact with two emails, typed phones, company and title;
* Comms' tests: an organizer's app, a guest's answer from theirs, stale answers kept, a moved
  occurrence detached once, reports, refusals, address books; meeting guest links signed, revoked
  and renewed;
* Drive's tests: a guest's link mailed, replaced, refused for deactivated colleagues, shown in the
  external sharing report.

**Calendar and contacts apps (CalDAV, CardDAV).** People add their Ferrum account to the calendar
and contacts apps of their phones and computers: Apple Calendar and Contacts, Thunderbird, DAVx⁵
on Android, Outlook through a CalDAV add-in.
* **Signing in.** An app password: an API key that works only for these apps (scope `dav`), made
  and revoked in the web app. The apps send it with the person's email over HTTP Basic.
* **Platform.** Ferrum serves `/api/dav/{package}/…`: it checks the app password and hands the
  request to the package as the signed-in person. `/.well-known/caldav` and
  `/.well-known/carddav` lead to Comms.
* **Calendar.** One calendar per person: the events they organize and those they are invited to
  and have not declined, from the last 400 days and every repeating one.
  * Organizers create, change and delete events from their apps; Ferrum sends the invitations,
    as it does from the web app.
  * A guest's answer (accept, maybe, decline) becomes their RSVP; deleting an invitation declines
    it.
  * A change to one occurrence of a repeating event becomes an event of its own, and the
    occurrence leaves the series: Ferrum has no changes to single occurrences.
* **Contacts.** Two address books, read-only: Colleagues (the people of the workspace) and
  Contacts (the CRM contacts the person may read, where CRM is installed).
* **Sync** by collection tags and ETags. Not built: sync-collection reports, scheduling inboxes
  (Ferrum sends invitations itself), more than one calendar.

**Guests.** People outside the workspace, by link, limited to what is shared with them.
* **Meetings.** An event's outside guests get a personal link in their invitation, and a room has
  a guest link to share. Guests type their name and join as "Name (guest)". Removing a guest from
  the event, or turning the room's link off, stops the link.
* **Drive.** Sharing with an email outside the workspace sends that person a personal link to the
  item, under the workspace's rules for public links (whether allowed, expiry, password). The
  Share dialog lists them as guests and removes them.
* Not built: guest accounts, chat with guests, a lobby where guests wait to be let in.

## 3. Not building

* An engine of our own for co-editing Office files: Collabora and ONLYOFFICE do it.
* Co-editing in the browser editors: they are single-user apps, and a fork must drop their names.
  Their capability port (upstream-requests.md) may change what is possible.
* Offline editing.
* A second notification system: the bell and mail are the platform's.
