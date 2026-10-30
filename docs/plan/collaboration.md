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
| Chat, mail, shared calendars | Built (Comms) | C5: calendar and contacts sync |
| Video meetings | None | C4 |
| Forms | Built (ferrum-app-forms) | |
| People outside the workspace | Public links only | C5 |

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

### C4. Meetings

* LiveKit (Apache-2.0) as a separate service, like Collabora: Ferrum issues its tokens.
* A meeting from a calendar event or a chat.
* The web app uses LiveKit's open-source components.

### C5. Beyond the workspace

* Calendar and contacts sync over CalDAV and CardDAV.
* Guests: people outside the workspace, signed in by link and limited to what is shared with them.

## 3. Not building

* An engine of our own for co-editing Office files: Collabora and ONLYOFFICE do it.
* Co-editing in the browser editors: they are single-user apps, and a fork must drop their names.
  Their capability port (upstream-requests.md) may change what is possible.
* Offline editing.
* A second notification system: the bell and mail are the platform's.
