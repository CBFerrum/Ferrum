# Privacy and data duties

What an app owes when a person asks for their data or its erasure, when a workspace is exported, when retention ends,
when a workspace moves, is copied as a sandbox or is deleted. The platform does most of it by itself for records of
entities; an app owes the rest for what it keeps elsewhere: its own tables, its own files, the programs it runs, the
places outside the installation it reaches. The code is `crates/kernel/src/privacy.rs`, `retention.rs`, `export.rs`,
`sandbox.rs`, `workspace_move.rs`, `account.rs`; the operator's side is [docs/operations.md](../operations.md),
*Personal-data requests*, *Moving one workspace*, *A sandbox of a workspace*, *Closing and deleting a workspace*.

## At a glance

| When | What the platform does by itself | What the app declares or does |
|---|---|---|
| A person asks for their data | Finds every record the installed packages declare as theirs, under every address they have had; exports them, their files and their account; names packages that declare nothing | Declares `personal_data` for its entities; implements `PersonalHeld` for its own tables |
| A person is erased | Blanks the declared fields, deletes the files of those records, redacts the fields' past values in the audit trail, takes the records out of their events (outbox and webhook deliveries; such an event is not replayed), anonymises the account; journals the erasure in object storage and erases again after a restore | The same declarations; `PersonalHeld::erase`, idempotent; listens to `ferrum.privacy.erased` for what it keeps outside |
| Processing is restricted, or consent withdrawn | Stops its own automated messages to the person | Asks `Kernel::may_contact` / `may_contact_for` before reaching out on its own paths |
| A retention rule ends | Deletes or anonymises records of an entity after so many days, daily, as the system, in batches | Declares `retain` where records must stay, with the reason |
| A workspace is exported | Every record, declared engine tables, attached files, the audit trail, the configuration; names every table and kind of file it left out | Declares `export_tables` (no secret in them); offers its own kinds of file from its own pages |
| A workspace moves | Copies every row with `tenant_id`, its files and its vault; keeps the workspace's id | Keeps `tenant_id` on every table, no key unique across workspaces, files under `tenants/{tenant}/` |
| A sandbox is made | Copies under a new id, rewriting the id everywhere; nothing it does reaches outside; the vault by each secret's rule | Calls out only through the kernel's client; treats what does not open as "not here" |
| A workspace is deleted | Deletes every row that names it and its files, in one transaction; refuses while an app holds something outside | `Leaving::held_outside` for what it holds outside the database; a way to let it go |

## Personal data in records: `personal_data`

Each package says where it keeps people's data, in `package.yaml` (`ferrum_meta::PersonalDataDef`,
`crates/meta/src/types.rs`):

| Key | What it is |
|---|---|
| `entity` | The entity |
| `match` | The field holding the person's address; their phone field where records hold no address; a link to a record another declaration finds; or `table.field` for people held in a child table's rows (only those rows are theirs) |
| `erase` | The fields an erasure blanks (a placeholder where a value is required); a child table listed loses its rows. Empty with `retain` |
| `retain` | Why these records stay when someone asks for erasure: they are exported, never blanked, and named in the answer |
| `where` | Values other fields must hold for a record to count |
| `phone` | A field with their number: asked with a number, records with the same digits (however written) are theirs |
| `name` | A field with their name: records holding it are listed as *possibly* theirs, for a person to judge, never erased for it |

Fields an erasure can blank are of the erasable types: Data, Text, Markdown, Email, Phone, Url, Date, Json and Table
(`ERASABLE_TYPES`). A real one, from Comms (`packages/comms/package.yaml`):

```yaml
personal_data:
  - {entity: Communication, match: sender, erase: [sender, sender_name, subject, content, text_content, attachments]}
  - {entity: ChatMessage, match: sender, erase: [sender, body]}
  - {entity: EmailGroupMember, match: email, erase: [email, member_name]}
  - {entity: CalendarFeed, match: person, erase: [person]}
```

The catalog refuses a declaration for an entity outside the package's dependencies
(`crates/meta/src/catalog.rs`). A package that declares nothing is named in every answer as declaring nothing, so a
silence is never taken for a search: declare, or say in the README why the app holds no one's data.

**Submitted documents** keep the copies of names they carry, as bookkeeping law requires; declare `retain` with the
law's reason rather than leaving them out.

**Work a person is named on** moves to whoever takes over when they leave: `handover` in `package.yaml`
(`HandoverDef`: `entity`, `field`, `where`), usually on open records only ([05](05-permissions-and-people.md)).

## Personal data in the app's own tables: `PersonalHeld`

What an app keeps in tables of its own (not entities) the platform cannot see. Implement
`privacy::PersonalHeld` and register it with `HookRegistry::add_personal_held`:

```rust
// crates/kernel/src/privacy.rs
#[async_trait::async_trait]
pub trait PersonalHeld: Send + Sync {
    fn app(&self) -> &'static str;
    async fn report(&self, tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String]) -> Result<Vec<Value>>;
    async fn erase(&self, tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String], reason: &str) -> Result<u64>;
}
```

Comms' meeting attendance (`ferrum-app-comms/src/meetings.rs`), registered in `lib.rs` with
`hooks.add_personal_held(Arc::new(meetings::Attendance))`:

```rust
async fn report(&self, tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String]) -> Result<Vec<Value>> {
    let seen: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_object('meeting', room_id, 'joined_at', joined_at, 'last_seen', last_seen) from comms_attendance \
         where tenant_id = $1 and (user_id = $2 or lower(email) = any($3)) order by joined_at limit 10000")
        .bind(tenant).bind(account).bind(emails.iter().map(|e| e.to_lowercase()).collect::<Vec<_>>())
        .fetch_all(&mut **tx).await?;
    Ok(if seen.is_empty() { vec![] } else { vec![json!({"what": "When they were in meetings made by other apps", "times": seen})] })
}

async fn erase(&self, tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String], _reason: &str) -> Result<u64> {
    let gone = sqlx::query("delete from comms_attendance where tenant_id = $1 and (user_id = $2 or lower(email) = any($3))")
        .bind(tenant).bind(account).bind(emails.iter().map(|e| e.to_lowercase()).collect::<Vec<_>>())
        .execute(&mut **tx).await?;
    Ok(gone.rows_affected())
}
```

- `emails` is every address the person has had; `account` is their user id in the workspace, when they have one. Match
  on both, case-insensitively.
- `report` gives one entry per kind of thing, worded for a person reading the report.
- `erase` runs in the erasure's transaction, and again when a restore replays the erasure (`Kernel::reapply_erasure`):
  doing it twice must change nothing. Delete, or blank, never "mark as erased" with the data still there.
- What the app holds outside the database (a row in a program beside the server, an account on a forge) it erases from
  its event sink on `ferrum.privacy.erased` (`privacy::ERASED_TOPIC`), published in the erasure's transaction.

## Restriction, consent and opt-outs

- **Restriction of processing** keeps someone's data and stops using it: no automated mail goes to them, under any address
  they have had. Before an app reaches a person on any path the platform does not send for it, it asks
  `Kernel::may_contact(tx, tenant, email)` (transactional) or `Kernel::may_contact_for(tx, tenant, email,
  Purpose::Marketing)` (`crates/kernel/src/retention.rs`). Messages sent with `messages::send` are asked for you.
- **Consent to marketing**: a promotional template (`category: promotional`) gets a signed opt-out link (`opt_out`) and
  is not sent to whoever withdrew. An erased person's opt-out is kept as a digest of the address, so they are not sent
  marketing again after an erasure.
- **Legal holds** stop erasure and deletion of what they cover. An entity's table refuses the deletion of a held row
  whoever's SQL asks (the `ferrum_held` trigger the schema sync puts on each, `crates/kernel/src/ddl.rs`); an app's own
  table has no such trigger, so check the hold before deleting a person's data there yourself.

## Retention

Workspace administrators make retention rules per entity: after so many days, matching records are deleted or
anonymised, daily, as the system, a bounded number at a time (`crates/kernel/src/retention.rs`). A legal hold stops a
rule; what a package `retain`s for a stated reason cannot have a rule that deletes it; a record that cannot be deleted
(submitted, linked to) is counted, not forced. An app's own tables are not reached by retention rules: if the app keeps
personal data there for a time, say how long in its README and delete it in its upkeep.

## The workspace's export

An export is the copy a customer takes with them (`crates/kernel/src/export.rs`): every record as JSON lines with its
child rows, the engine tables packages declare, every attached file, the audit trail and the configuration. It never
holds passwords, secret values or sealed settings, and its manifest names every workspace table it left out
(`tables_left_out`) and every kind of file (`files_left_out`).

- **`export_tables`** in `package.yaml` lists the app's engine tables to carry whole (journals, stock moves). Whole rows:
  none of these may hold a secret. A table with `tenant_id` not named is listed as left out; say in the README why it is,
  or name it.
- **An app's kinds of file** are left out ([09](09-files-and-storage.md)): the app owes the customer a way to have them
  from its own pages (a backup's download, an artifact, an object).
- An export's parts are deleted after a few days, or sooner by an erasure, which no copy outlives.
- There is no import of an export in the platform.

## A workspace moves

A move copies one workspace to another installation and keeps its id ([docs/operations.md](../operations.md), *Moving
one workspace*). The copier takes every table with a `tenant_id`, the workspace's files, and its vault (through a
transfer key the operator gives). So:

- Every table of the app that holds a workspace's data has `tenant_id` ([04](04-data-and-records.md)).
- No key is unique across workspaces: a unique key includes `tenant_id`. A copy refuses a table with a key unique across
  workspaces (`crates/kernel/src/sandbox.rs`), and a workspace named outside its own tables.
- Files are under `tenants/{tenant}/…`, as the seam keeps them; a file under another key refuses the copy.
- What the app sealed with its vault handle opens at the target (the vault travels; the id is the same).
- What the app holds outside the installation (a database at a provider, a forge's repositories) does not move by
  itself: say in the app's operations notes what an operator does.
- After a move, sign-ins the target carried end for programs beside the server ([10](10-companions-and-sign-in.md)).

**Restores.** When a restore is noticed, each app is asked to set aside work it had in hand that may or may not have
happened since: `recovery::Restored` (`set_aside(tx, tenant: Option<Uuid>)`, none for every workspace), registered with `HookRegistry::add_restored`; its
`kept()` names tables that record what has happened and must not be unlearnt when one workspace goes back (a key revoked
since stays revoked).

## A sandbox is made

A sandbox is a copy of a live workspace under a new id, for trying an upgrade or a change
([docs/operations.md](../operations.md), *A sandbox of a workspace*; `crates/kernel/src/sandbox.rs`):

- Every place the workspace's id appears is rewritten: every row's `tenant_id`, any other value holding it, and file keys.
- **Nothing it does reaches outside.** Mail, pushes and webhooks are set aside; calls through the kernel's outbound client,
  connections, connectors and plugins' fetches are refused, saying "sandbox". An app's call through its own HTTP client
  is not stopped: so never make one ([11](11-integrations.md)). Ask `Kernel::outward(tenant)` before anything else
  that would leave the installation (Cloud's forge sign-in skips every workspace it refuses).
- **Its vault** has keys of its own; each of the live workspace's secrets is copied by its rule's `sandbox`: `blank` (the
  default), `regenerate` or `keep` ([08](08-secrets-and-the-vault.md)). Choose each secret's on purpose.
- **What the app sealed** with `Vault::seal` for the live workspace does not open in the sandbox (another scope, other
  keys): `open` answers `Invalid`. Treat it as absent; Comms honours no room's old guest link in a sandbox
  (`src/keys.rs`, `guest_key`).
- A value with the form of one sealed outside the vault, in any column, is blanked (the command lists the columns;
  `not_sealed:` keeps one that holds no sealed value).
- The app's secret fields are not copied; its kinds of file are.

## A workspace is deleted

`ferrum task delete_tenant` first says what would go and stops at a legal hold or anything an app holds outside the
database; with `apply:yes` it deletes the files, then every row that names the workspace, in one transaction
([docs/operations.md](../operations.md), *Closing and deleting a workspace*). An app that made things outside (Cloud's
databases and repositories) implements `account::Leaving`:

```rust
// crates/kernel/src/account.rs
#[async_trait]
pub trait Leaving: Send + Sync {
    async fn held_outside(&self, tx: &mut Tx, tenant: Uuid) -> Result<Vec<String>>;
}
// in register: h.add_leaving(Arc::new(projects::Holdings));   // Cloud
```

and gives the operator a way to let go of them first (Cloud: `ferrum task cloud_retire slug:<workspace> apply:yes`).
While a workspace is closed (read-only for everyone), an app winds down what it runs: Cloud stops each project's sites
and keeps what it held. Backups taken before still hold the workspace until they expire.

## Regions

Where a region can fail over, a region writes only while it holds the right to; without it every transaction is
read-only, writes answer `503`, and work that reaches outside does not run ([docs/operations.md](../operations.md),
*Regions*). An app's code treats a refused write like any other refusal. An app that keeps everything in the database
and the store moves with the installation; one that keeps data at a provider of its own says in which region.

## Checked by

- The catalog: personal-data declarations within the package's dependencies.
- The platform's suites: `crates/kernel/tests/privacy_it.rs` and `retention_it.rs` (with their mutants in
  `scripts/mutants.py`), the move and sandbox rehearsals (`scripts/move_rehearsal.sh`, `scripts/local_ci.sh move`).
- The copier at every move and sandbox: tables with keys unique across workspaces, files outside the workspace's keys.
- An app's `PersonalHeld`, `Restored` and `Leaving`: its own suite. Whether an app declares all the personal data it
  keeps: reviewed, not checked.

## See also

- [04 Data and records](04-data-and-records.md), [05 Permissions and people](05-permissions-and-people.md),
  [08 Secrets and the vault](08-secrets-and-the-vault.md), [09 Files and storage](09-files-and-storage.md),
  [11 Integrations](11-integrations.md), [14 Operations for apps](14-operations-for-apps.md)
- [docs/operations.md](../operations.md), *Personal-data requests*; [docs/security.md](../security.md), *Data*;
  [docs/registry/data-governance.md](../registry/data-governance.md)
