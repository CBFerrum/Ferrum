# Files and storage

Files live in an S3-compatible object store, the second store beside PostgreSQL. An app never holds a store, a bucket,
keys or a path of its own: it keeps and reads files through the platform's file seam, and the platform counts them,
scans them, deletes them after its deletion lag, backs them up with the database, moves them with a workspace and copies
them into a sandbox. The code is `crates/kernel/src/files.rs` and `crates/kernel/src/outbox.rs` (`release_objects`);
the store is `server/src/objects.rs`; the operator's side is [docs/operations.md](../operations.md), *Object storage*;
the HTTP routes are [docs/api.md](../api.md), *Files*; the decisions are [docs/plan/stateless.md](../plan/stateless.md),
D6.

## Two kinds of file

| | A workspace's file | A file an app keeps for its own work |
|---|---|---|
| What | An attachment, a generated PDF, a mail's attachment: content of a record, the workspace's to hand out | A build artifact, a database's backup, a customer's managed object: the workspace's bytes, but not its content to hand out |
| Kept with | `Kernel::store_file` (or the upload routes) | The app's `FileKind` handle: `keep` |
| Tied to | A record (`entity`, `record_id`), or nothing until a record names it | The kind (`<app id>.<name>`) and an id of the app's choosing |
| Who reaches it | Whoever may read its record; a loose one its uploader and administrators | Nobody through the platform's routes, administrators included; only the declaring app's code and its site |
| In the workspace's export | Yes | No, and the export says so per kind (`files_left_out`) |
| Counted, scanned, deleted after the lag, moved, copied to a sandbox | Yes | Yes |

## A workspace's files

### Keeping one

```rust
// crates/kernel/src/files.rs
pub async fn store_file(&self, ctx: &Ctx, attach_to: Option<(&str, Uuid)>, file_name: &str, content_type: &str, data: &[u8]) -> Result<Value>
// → {id, url: "/files/<id>", file_name, size, sha256}: exactly what POST /api/files answers
```

What it does, in order:

1. With `attach_to`, checks that `ctx` may attach to that record: a member reads it (`get`); an outside person attaches
   as the upload route lets them.
2. Checks the workspace's storage quota (`Kernel::check_storage_quota`: the tenant's `quotas.storage_bytes` against the
   sum of its files): refused past it.
3. Scans it with the malware scanner when one is configured (below).
4. Keeps it under `tenants/{tenant}/{id}` (a new UUIDv7 id), with its name cleaned (no control characters, no `/` or `\`,
   200 characters at most, `file.bin` when nothing is left) and its SHA-256. A type a browser would run (`text/html`,
   `application/xhtml+xml`, `image/svg+xml`, JavaScript) is kept as `application/octet-stream` (`ACTIVE_TYPES`).

It takes the whole file in memory. Use it for what the app makes or receives: a mail's attachments (Comms), a rendered
document. People's uploads go through the platform's routes (`POST /api/files`, 25 MB; direct or in parts up to 2 GB:
[docs/api.md](../api.md), *Files*), which stream.

### Reading one

| Call | What it gives | Refused |
|---|---|---|
| `Kernel::file_info(ctx, id)` | `FileInfo { name, content_type, size, .. }`, to answer with the file without reading it | Not found unless `ctx` may download it |
| `Kernel::read_file(ctx, id)` | `(name, content_type, bytes)` | Also for a file over `files::MAX_READ_WHOLE` (256 MiB): "… is larger than 256 MB, more than is read whole: it is downloaded as a stream" |
| `Kernel::copy_file(ctx, id, attach_to)` | A new file with the same bytes on another record; deleting either leaves the other | As `read_file`, then as `store_file` |

Who may download (`Kernel::may_download`, `may_download_file`): a file tied to a record, whoever may read the record (an
approver too while it waits for them); a file tied to none, its uploader, administrators and the system; an outside
person only a file of a record they read that is shared with them, that they uploaded, or that a field of the record as
they read it names. Anything else is *not found*, the same answer as a file that is not there, so an id never tells
whether a file exists.

### Answering with one from the app's site

A file of any size goes out as a stream, never whole in a replica's memory:

```rust
// crates/kernel/src/hooks.rs
SiteResponse::file(200, &info.content_type, vec![("content-disposition".into(), "attachment".into())], file_id)
```

The server streams the file named in the `x-ferrum-stream-file` header and sends that header on to nobody. It sends the
file only when it is this workspace's; and a file of an app's kind only from a site of the app that declared it
(`Kernel::site_may_send`). Your site decides who may have it: check the caller's right first, as `file_info` does for a
member.

A site's request body is capped by `Site::max_body` (1 MiB by default, at most 1 GiB) and, per route,
`Site::max_body_for(method, path)`: raise it only on the routes that take files. Drive's `drive.max_file_mb` setting is
an example of making that limit the operator's ([07](07-stateless-apps.md)).

### Attach fields and how files are tied

An `attach` field holds `/files/<id>`. When a record is saved, each file its attach fields (its child tables' included)
newly name is tied to it in the save's transaction (`claim_files`):

- a file tied to no record is tied to this one when the saver uploaded it, or is an administrator or the system;
- a file already tied to a record stays there, and may be named by anyone who may read that record;
- naming any other file is refused, `403`: "<Entity> names a file you cannot download". An id is no way to read a file.
- an id naming no file, or a file of an app's kind, is not tied, and gives nothing away.

In the app's own code, inside a transaction:

- `Kernel::attach_file_tx(ctx, tx, id, entity, record)` ties a loose upload of the caller's (or any, for the system) to a
  record the caller can read.
- `Kernel::attach_files_tx(ctx, tx, &ids, entity, record)` takes the files going out with, or kept on, a record (a
  reply's attachments): each already on that record, or a loose upload of the caller's, which it ties; anything else
  refuses them all, "file X is not there, is not yours, or belongs to another record".

Portal people and guests may upload at most 20 loose files an hour (`Kernel::count_loose_upload`); members are not
counted.

### Deleting one

| Call | When |
|---|---|
| `Kernel::remove_file(ctx, id)` | A person deletes a file they may delete (its uploader, or someone who can change its record). The row goes; the object is deleted through the store |
| `outbox::release_objects(tx, tenant, &keys)` | The app deletes `file_object` rows in its own transaction: the objects are deleted once that transaction commits, never if it rolls back, retried while the store fails. Only keys under `tenants/{tenant}/`; anything else is refused |

Either way the store keeps the object for `files_delete_after_days` first (0 by default; an operator sets it at least
`backup_reach_days`), so a database restored to any moment its backups reach finds every file it names
(`Objects::remove`, `server/src/objects.rs:256`). A name is never used again for other bytes. Never delete an object
another way: you would break the restore.

### Public files and short links

- **Public files**: `POST /api/files` with `public=true` (PNG, JPEG, WebP, GIF, AVIF and PDF only: logos, product
  photos, brochures) are served without signing in at `/files/public/{id}`, cached for ever, with a sandboxing Content
  Security Policy. Use them only for what is meant for everyone.
- **A file pulled by a service**: a connection's call can give a service a short-lived public address of a file,
  `/files/pull/{id}?s=…` ([BD-64](../builder/decisions.md), [11](11-integrations.md)).
- **Direct links**: with `files_direct_links` on, a download answers `307` to a link the store signed, valid at most 300
  seconds; never for an entity whose reads are recorded, an outside person, a type a browser would run, or while files
  move. Off by default. Your code does nothing for it.

## Files an app keeps for its own work: `FileKind`

What an app keeps for its own work goes in the platform's store as a *kind* of file the app declares
([docs/app-contract.md](../app-contract.md), *Files an app keeps for its own work*). Cloud keeps build artifacts,
resources' backups and managed objects so, and holds no store of its own since 0.1.21.

**Declaring.** Inside the app's registration, once per kind:

```rust
// ferrum-app-cloud/src/files.rs, called from Cloud's register inside hooks.app("cloud", …)
let kinds = [h.file_kind("artifact"), h.file_kind("backup"), h.file_kind("object")];   // cloud.artifact, …
```

A name is lowercase letters, digits and `_`, beginning with a letter, at most 40 characters. A kind declared outside an
app's registration, declared twice, or with a bad name stops the server (`HookRegistry::file_kind`,
`crates/kernel/src/hooks.rs:415`). Keep the handles as you keep the vault handle (Cloud keys them by its site's address,
`KINDS`).

**Its handle** (`files::FileKind`):

| Call | What it does |
|---|---|
| `keep(k, tenant, id, file_name, content_type, &data)` | Keeps a file of the kind in the workspace's storage, tied to `id` (the artifact, the backup): quota checked and scanned as an upload. `{id, file_name, size, sha256}`: no URL, since no route of the platform's serves it |
| `info(k, tenant, file)` | Name, type and size, to answer with it from the app's site (`SiteResponse::file`). Not found when it is not this kind's |
| `read(k, tenant, file)` | The bytes, up to 256 MiB; a larger one is streamed instead |
| `release(k, &mut tx, tenant, &files)` | Lets them go in the caller's transaction: rows now, bytes after the deletion lag once it commits. Another kind's, or none, are left alone |
| `kind()` | `<app id>.<name>`, what `file_object.entity` holds |

A handle used with a kernel that did not declare its kind is refused: "the kind of file X was not declared to this
server".

**What follows from a kind** (enforced in `files.rs`, `may_download`, `claim_files`, `site_may_send`):

- The platform's routes refuse it to everyone, administrators included, as a file that is not there: no download,
  copy, deletion, print or tie through an attach field, and no record's listing names it. Another app's handle finds
  nothing of it. A kind whose app left the build stays out of reach.
- Its only way out is the app's: `SiteResponse::file` from the declaring app's site, to the people the app's own rules
  let have it. The workspace's export leaves the kind out and says so (`files_left_out`: files, bytes, why), so **the app
  owes its customer a way to have them**: a backup's download, an artifact, an object, from its own pages.
- It is the workspace's: the quota, the deletion lag, a move, a sandbox, the workspace's deletion, the doctor and the
  restore drill treat it as any file. A person's erasure does not touch it: a kind's files are no person's record.

**Large ones.** `keep` takes bytes in memory and `read` refuses over 256 MiB. Cloud keeps a large file in parts of at
most `MAX_READ_WHOLE` (`PART`), names each part `name.0`, `name.1`, …, reads each back and compares length and SHA-256
before naming it anywhere, and finds a part kept before by name and digest so that a repeated attempt keeps no second
copy (`ferrum-app-cloud/src/files.rs`, `keep`). Copy that pattern; the seam has no streamed write for apps yet.

## Scanning

With `FERRUM_CLAMAV_ADDR` set, every upload passes through ClamAV before it is stored, `store_file` and
`FileKind::keep` included. What it finds is refused, not stored, and audited as `malware_refused`; with the scanner down
an upload fails `503` until it is back; a file past the scanner's limit is stored marked `unscanned`
([docs/operations.md](../operations.md), *Malware scanning*). An app shows the refusal to the person as it comes; it does
not retry it on its own.

## What never to do

| Never | Why | What catches it |
|---|---|---|
| A bucket, store client or object-store keys of the app's own | Its files would escape the deletion lag, the restore drill, moves, sandboxes, quotas and erasure | Reviewed, not checked. `scripts/one_input.sh` starts the server with its apps on `DATABASE_URL` alone |
| A path on local disk for anything that is not scratch | Gone with the container; a second replica does not see it | Reviewed, not checked ([07](07-stateless-apps.md)) |
| A key outside `tenants/{tenant}/` | A move and a sandbox refuse a file kept under a key that does not name its workspace | `release_objects` refuses it; the copier refuses the workspace |
| Delete an object directly, or reuse a name for other bytes | A restore would find the wrong bytes, or none | The raw store is not public (`pub(crate)`) |
| Read a large file whole | A replica's memory | `read_file` and `FileKind::read` refuse past 256 MiB |
| Serve a stored file inline with its own type | It could run script in the app's origin | Active types are stored as `application/octet-stream`; downloads carry `nosniff`, `attachment` and a sandboxing policy |
| Hand out an app's kind through a record's attach field | It is not the workspace's content | `claim_files` ignores it |

## Not built yet

From [docs/plan/stateless.md](../plan/stateless.md), *D6, other programs' stores*: the seam has no streamed write for
apps (`store_file` and `keep` take the whole file), and nothing adopts an object another program wrote into the store as
a file (meeting recordings by LiveKit Egress need it; Comms keeps a location string until then). The store's lifecycle
rule taking effect, IRSA and workload identity in a real cloud were not proved here.

## Checked by

- `crates/kernel/tests/file_kinds_it.rs`, and the file-kind mutants in `scripts/mutants.py` (`file-kind-…`).
- `scripts/object_store_rehearsal.sh` (`scripts/local_ci.sh objects`, run when named): real S3 servers, files moved,
  two replicas, direct links, the restore drill.
- `scripts/check_apps.py` does not look at files; holding no store of the app's own is reviewed, not checked.

## See also

- [07 Stateless apps](07-stateless-apps.md), [12 Privacy and data duties](12-privacy-and-data-duties.md),
  [04 Data and records](04-data-and-records.md), [21 APIs and docs](21-apis-and-docs.md)
- [docs/app-contract.md](../app-contract.md), *Files an app keeps for its own work*; [docs/api.md](../api.md), *Files*;
  [docs/operations.md](../operations.md), *Object storage*, *Malware scanning*
