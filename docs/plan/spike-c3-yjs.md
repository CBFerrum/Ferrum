# Spike C3: documents co-edited live through Ferrum's replicas (go, with one fix in yrs)

30 October 2026. The spike of C3 in [collaboration.md](collaboration.md). The question: can one
Yjs document be edited live by many people through several Ferrum replicas, with PostgreSQL as the
only store, through lost connections and restarts, with `yrs` (Yjs in Rust) on the server?

What ran:
* `spikes/yjs_server`: a Yjs sync server in Rust (yrs 0.28.0, axum 0.8, sqlx 0.9), two replicas
  against one PostgreSQL 17;
* `spikes/yjs_harness.mjs`: editors in Node with Yjs 13.6.33 and y-websocket 3.1.0, half of them on
  each replica.

To repeat it:

    createdb -O ferrum yjs_spike
    (cd spikes/yjs_server && cargo build --release)
    npm i yjs@13.6.33 y-protocols@1.0.7 y-websocket@3.1.0 ws@8.21.3
    SPIKE_BIN=spikes/yjs_server/target/release/yjs-spike node spikes/yjs_harness.mjs

`EDITORS=30` runs thirty editors; `SPLIT_PAIRS=1` lets edits fall inside emoji (see Found, 2).

## Verdict

Go. Every scenario held once two things were right:
* **The server counts text in UTF-16**, as Yjs clients do (`OffsetKind::Utf16`). yrs counts UTF-8
  bytes by default, and then misreads clients' changes around any character beyond ASCII.
* **yrs splits emoji the way Yjs does.** It does not yet: an upstream fix is open (yrs#715, below).
  With that fix applied to yrs 0.28.0, every scenario passes, emoji split or not.

## Results

Ten editors, each typing every 50–150 ms (inserts and deletes anywhere, ~100 changes a second in
all), on two replicas:

| Scenario | Result |
|---|---|
| Steady, 15 s | every editor and both replicas hold the same text 8 ms after the last keystroke (1,505 changes) |
| Latency on one replica | median 0.6 ms, p95 1.0 ms |
| Latency across replicas (stored, NOTIFY, read back) | median 3.9 ms, p95 6.6 ms |
| A lost connection: one editor offline 5 s, everyone typing | merged; the same everywhere 119 ms after it reconnects |
| A replica killed while people type, back 4 s later | nothing lost (2,918 of 2,918 changes); the same everywhere 8 ms after typing stops |
| Both replicas restarted | a newcomer reads the same text and all 2,918 changes from PostgreSQL alone |
| Who is here (awareness) | every editor sees all ten, across replicas |
| Thirty editors (~220 changes a second) | the same everywhere 27 ms after typing stops; across replicas median 3.4 ms, p95 7 ms; nothing lost |
| Storage | 2,783 stored changes: 283 rows plus one 93 KB snapshot (a snapshot every 500 changes) |
| Writing one change | 4–5 ms: one transaction with the document's sequence (row lock), the change and the NOTIFY |

Each editor also appends to a list nobody deletes from; its length is how "nothing lost" is
checked. Fewer changes are stored than made: an editor's offline changes arrive as one.

## The design that held

* **One sequence per document.** A change is stored in a transaction that takes the next number from
  the document's row, so a document's changes commit in their order, and a replica reading "after
  number N" never skips one.
* **Relay first, store next.** A replica sends a change to its own clients at once and then stores
  it. A change can therefore be stored before one it builds on. That is harmless: yrs and Yjs hold a
  change whose predecessor is missing until it arrives, and snapshots carry such changes along.
* **Replicas tell each other with NOTIFY** (the document only, never the change), and each reads the
  changes after the last one it saw. A replica that lost its listener reads everything it missed
  when it is back. Awareness (cursors, names) rides in the NOTIFY itself and is never stored.
* **Snapshots.** Every 500 changes the replica that stored the 500th folds them into the snapshot,
  under a per-document advisory lock, and deletes them. Loading reads the snapshot and the changes
  after it in one repeatable-read transaction.
* **Memory.** A replica keeps a document while someone on it has it open.

## Found

1. **UTF-16 offsets.** Without `OffsetKind::Utf16`, the server's copy drifted from the clients' at the
   first "ε" or "日本", and a snapshot made from it could not be read by Yjs.
2. **Emoji split in two.** Yjs lets an edit fall between the two halves of a surrogate pair (an emoji)
   and replaces each half with U+FFFD. yrs 0.28 keeps the whole emoji on one side, so its lengths
   no longer match: its copy drifts, and a snapshot written from it is unreadable by Yjs. In the run
   with split emoji, the restarted document came back with 7 of 2,900 changes.
   * Reproduction: `spikes/yjs_server/examples/surrogate_split.rs`.
   * Upstream: issue y-crdt#714 (open since 14 October 2026) and pull request y-crdt#715, which makes
     yrs split like Yjs; it waits for review. `spikes/yjs_server/yrs-surrogate-split.patch` is the
     same change, applied to yrs 0.28.0 for the passing runs.
   * Editors normally move by whole characters, but a server must not trust that.
3. **Writes are the ceiling.** At 4–5 ms per change under the document's lock, one document takes
   roughly 200 changes a second before writes queue. People did not wait (relay comes first), but
   C3 should gather a document's changes for ~50 ms and store them as one row.
4. **Relay before store** means a change lives only in the clients until it is stored. When a replica
   died in between, the clients still had it and sent it again on reconnecting.
5. **Awareness is whatever a client says.** A client can call itself anyone; C3 sets the name and
   colour from the session.

## Not covered

* Browsers and a real editor: the harness edits text directly. Rich text (ProseMirror, TipTap through
  y-prosemirror) is the same CRDT with different types.
* Signing in and permissions: the spike has neither.
* Documents of several megabytes, days of uptime, hundreds of editors on one document.
* PostgreSQL failing over.

## For C3

* **yrs with the fix.** Until a release includes y-crdt#715, Ferrum builds yrs 0.28.0 with
  `yrs-surrogate-split.patch` (vendored, MIT), and drops it when the release lands.
* **In the platform**, so any package can host documents:
  * a WebSocket per open document;
  * the session token in the first message, as on `/ws/live`;
  * the rooms, storage tables and NOTIFY channel shown here;
  * the owning package decides who may read or write each document. Read-only clients' changes
    are refused, and access is checked again every minute.
* **Drive owns the documents**: a document is a Drive file, so sharing, Shared with me, trash,
  search, presence, comments and versions all apply. A version is a snapshot taken when the last
  editor leaves, or every ten minutes of editing.
* **The editor**: TipTap with y-prosemirror and collaboration cursors (MIT). Names and colours come
  from the server.
* **Limits**: 2 MB per message, a cap per document, a rate per connection, and changes gathered for
  ~50 ms per document.
* **Exports**: HTML to PDF through the PDF renderer; DOCX after.
