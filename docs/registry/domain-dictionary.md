# Domain dictionary

What each shared word means in Ferrum, and who owns it. An app extends a shared record; it does not
keep its own copy. Where this says *gap*, the rule is not yet what the specification asks.

| Concept | Meaning | Owner | Stored as |
|---|---|---|---|
| Tenant, workspace | One customer's isolated data. "Workspace" is the word people see. | Kernel | `tenant`; row-level security on every table |
| Company, legal entity | A legal entity inside a workspace. Events call it `legal_entity`. | `core` | `Company` |
| User, person | Someone who signs in to a workspace: `member`, `guest`, `portal` or `service`. | Kernel | `app_user` |
| Identity | One person across workspaces (shapes B and C): a user of the directory workspace. | Kernel | `app_user.identity_id` |
| Contact, Address | Someone you deal with, who may never sign in. | `contacts` | `Contact`, `Address` |
| Party | A customer or supplier. | `parties` | `Customer`, `Supplier` |
| Employee | HR's employment record. *Gap:* linked to a user by value, not one person model. | HR | `Employee` |
| Item, product | What is bought, sold or stocked. Price lists and stock extend it. | `products` | `Item` |
| Project | Shared identity; Projects and ERP extend it. | `project_records` | `Project` |
| File | Stored bytes with a checksum. Drive files, attachments and signed PDFs are all these. | Kernel | `file_object` |
| Document | A record with a lifecycle. In Drive, a document edited together. Say which. | Kernel, Drive | Entity tables; `.fdoc` |
| Posting | The only way money reaches the ledger. Apps go through ERP's engine and keep no ledger. | ERP | `gl_entry` |
| Stock movement | The only way stock changes. | ERP | `stock_ledger_entry` |
| Payment | Commerce's gateway payment and ERP's Payment Entry are not one thing. *Gap:* no canonical payment service. | ERP, Commerce | |
| Consent, suppression | Campaigns' own. *Gap:* not shared by every path that sends. | Campaigns | `MarketingContact` |
| Role, permission | Roles grant actions on entities, with rules on rows and groups of fields. | Kernel | `role_permission` |
| Scope | Which records of an entity a person may reach, such as their companies. | Kernel | `user_scope` |
| Group | An identity provider's group. Grants only roles mapped to it. | Kernel | `scim_group` |
| Event | A fact written with the change that caused it. | Kernel | `outbox`, [events.md](../events.md) |
| App, package, plugin | An app is what is installed; a package its data model; a plugin its sandboxed code. | Platform | [plugins.md](../plugins.md) |

**Identifiers.** Records have UUIDs, v7 where time order helps; documents have human numbers. A
person's stable identity is their id; email is an attribute that can change. *Gap:* personal-data
requests still match by email.

**Money** is decimal, never binary floating point, including in the browser, where
`ferrum-calc` runs as WebAssembly. **Dates** are local dates in the workspace's time zone;
instants are UTC.
