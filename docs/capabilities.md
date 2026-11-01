# Capabilities

Generated from [docs/registry](registry/README.md) by `scripts/registry.py generate`; edit the registry, not this.
This is what Ferrum can claim, and how it knows. *Basis* says how far an entry was established:
**tested** (tests run in this audit), **read** (code read), **probe** (keyword probe).

Nothing is `Verified complete` until its tests have run against a released digest and a person has
reviewed it. An entry resting on a keyword probe is a lead, not a finding.

| Status | Requirements |
|---|---|
| Implemented, qualification incomplete | 99 |
| Partially implemented | 54 |
| Missing | 47 |
| Blocked by external dependency | 9 |
| **All** | **209** |

## Platform

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-PLATFORM-001 | Modular monolith with a Rust kernel and apps as crates | Implemented, qualification incomplete | read |  |
| FERRUM-PLATFORM-002 | Authoritative ownership of each shared concept | Partially implemented | read | Person, party, product and project are shared packages; employee, payment and consent are not yet one owned model across apps |
| FERRUM-PLATFORM-003 | Controlled merging of duplicate customers with aliases | Implemented, qualification incomplete | tested | Customer and Supplier opt in; apps with their own people or companies have not; Duplicates are found by equal fields, not fuzzy matching; A merge is not undone; ERP's statements read merged ids only once it asks ferrum_kernel::merge::merged_ids; External identifiers wait for the connector platform |
| FERRUM-PLATFORM-004 | Typed cross-application contracts with stated idempotency and failure behaviour | Partially implemented | read | Apps call ERP's engine as a crate, without a published contract for each operation |
| FERRUM-PLATFORM-005 | Hook dependencies and ordering checked at startup | Implemented, qualification incomplete | tested | The mechanism exists; apps' hooks have not declared names or fills yet, so they run in registration order |
| FERRUM-PLATFORM-006 | Isolation profiles: shared cells, dedicated, on-premises, disconnected | Partially implemented | read | Single-server and Kubernetes deployments exist; cells and a disconnected profile do not |
| FERRUM-PLATFORM-007 | Strong types at consequential boundaries | Partially implemented | read | Money and tax go through ferrum-calc with decimals; documents stay JSON-shaped maps |
| FERRUM-PLATFORM-008 | Explicit document state machine with per-state field rules | Partially implemented | read | Draft, submitted and cancelled are general; approved, amended and archived vary by app |
| FERRUM-PLATFORM-009 | Custom fields and entities with conflict detection | Implemented, qualification incomplete | tested | Customizations made before this have no record of what they replaced, and are not compared; Compares property changes and added fields; rules, workflows and print templates are not compared |
| FERRUM-PLATFORM-010 | Schema evolution: expand, migrate, contract, resumable backfills | Partially implemented | read | Upgrade steps exist; resumable backfills and validation reports do not |
| FERRUM-PLATFORM-011 | Equivalent integrity on every mutation path | Partially implemented | read | Not audited path by path; app maintenance jobs that write SQL directly need a review |
| FERRUM-PLATFORM-012 | Transactional outbox with idempotent consumers | Implemented, qualification incomplete | tested |  |
| FERRUM-PLATFORM-013 | Event envelope: origin, correlation, causation, trace context, versions | Implemented, qualification incomplete | tested | Tasks an event handler spawns start without a trace |
| FERRUM-PLATFORM-014 | Tenant fairness and priority classes for events | Implemented, qualification incomplete | tested | Fairness covers the outbox and plugins; apps' own background jobs (Campaigns sending, upkeep) have none yet; Bulk paths must set the bulk priority; importer and bulk.rs do not yet |
| FERRUM-PLATFORM-015 | Dead letters with inspection and audited replay | Implemented, qualification incomplete | tested | An API only: no administrator screen yet |
| FERRUM-PLATFORM-016 | Durable jobs with leases, cancellation and poison isolation | Implemented, qualification incomplete | tested | Nothing schedules a job for later or repeats one; apps' periodic work stays in their maintenance; Plugin schedules and app maintenance are not moved onto it yet |
| FERRUM-PLATFORM-017 | External effects run after commit | Implemented, qualification incomplete | tested | Enforced for plugins; native hooks are held to it by review (IRP and VIES read; other connectors not) |
| FERRUM-PLATFORM-018 | No transaction held across an external call | Implemented, qualification incomplete | tested |  |
| FERRUM-PLATFORM-019 | Stateful cross-system workflows with compensation | Missing | probe |  |

## Identity

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-IDENTITY-001 | Local accounts, passkeys, TOTP, OIDC single sign-on | Implemented, qualification incomplete | tested |  |
| FERRUM-IDENTITY-002 | Session and token revocation across replicas | Implemented, qualification incomplete | tested |  |
| FERRUM-IDENTITY-003 | Deprovisioning with work handed over | Implemented, qualification incomplete | tested | No app handles ferrum.user.deprovisioned yet, so Drive shares and helpdesk queues stay put |
| FERRUM-IDENTITY-004 | SCIM 2.0 people and groups | Implemented, qualification incomplete | tested | Not yet run against a real Okta or Entra tenant |
| FERRUM-IDENTITY-005 | OIDC back-channel logout | Implemented, qualification incomplete | tested | Ends all the person's sessions, not only the one the token names; Not tried against a real identity provider |
| FERRUM-IDENTITY-006 | SAML single sign-on | Missing | probe |  |
| FERRUM-IDENTITY-007 | Linking accounts only on proof of ownership | Implemented, qualification incomplete | read |  |
| FERRUM-IDENTITY-008 | Guests, portal people, service accounts, scoped keys | Partially implemented | read | API key scopes are dav and scim; there are no per-entity key scopes |
| FERRUM-IDENTITY-009 | One policy system across tenant | Partially implemented | read | Department, branch and time-limited grants are not modelled |
| FERRUM-IDENTITY-010 | Row rules beyond equality | Implemented, qualification incomplete | tested | Ranges and days from today; no expressions across fields |
| FERRUM-IDENTITY-011 | Explicit reference-disclosure policy for linked titles | Implemented, qualification incomplete | tested | The default stays open; no package has marked its sensitive entities yet; Covers titles on records; counts, suggestions and search snippets are not audited against it |
| FERRUM-IDENTITY-012 | Field masking and export restrictions | Partially implemented | tested | Masks exist; no field declares one yet in the shipped packages; Export restrictions are per entity (can_export), not per field |
| FERRUM-IDENTITY-013 | Policy explanation and simulation | Implemented, qualification incomplete | tested | An API only: no administrator screen yet; Simulates other roles, not other rules |
| FERRUM-IDENTITY-014 | Segregation of duties and maker-checker | Partially implemented | read | Approvers can be barred from their own documents; no rule across roles or automations |
| FERRUM-IDENTITY-015 | Authorization rechecked when delayed work runs | Partially implemented | tested | Mail, push and jobs recheck when they run; automations still run as the system |
| FERRUM-IDENTITY-016 | Browser session architecture with CSRF defence | Implemented, qualification incomplete | read | Bearer tokens on pages remain readable by script; CSP is the defence |
| FERRUM-IDENTITY-017 | Secrets sealed at rest with bound context | Implemented, qualification incomplete | tested |  |
| FERRUM-IDENTITY-018 | Key rotation while old values exist | Implemented, qualification incomplete | tested | One key per server, not per tenant; apps' signing keys kept elsewhere are not covered |
| FERRUM-IDENTITY-019 | Key-management service integration | Missing | probe |  |
| FERRUM-IDENTITY-020 | OWASP ASVS mapping and independent security testing | Blocked by external dependency | read | docs/security.md states the model; no independent test has been done |

## Privacy

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-PRIVACY-001 | Packages declare personal data | Implemented, qualification incomplete | read | Match by email field only |
| FERRUM-PRIVACY-002 | Relationship-aware discovery: identities, attachments, messages | Partially implemented | tested | Found by email, under every address held, with attached files; not by name or phone; The apps' new declarations are on local branches, unpushed; packages that declare nothing are named in each answer, not searched; Search words and analytics exports are not reached |
| FERRUM-PRIVACY-003 | Restriction of processing as its own operation | Implemented, qualification incomplete | tested | Stops queued mail under every address the person has had; Campaigns and hosted mail must ask Kernel::may_contact on their own delivery paths |
| FERRUM-PRIVACY-004 | Legal holds | Implemented, qualification incomplete | tested | Holds apply to the platform's erasure and deletion; apps' own SQL deletes do not check them yet |
| FERRUM-PRIVACY-005 | Retention rules by data category | Implemented, qualification incomplete | tested | Rules are by entity, set per workspace; packages suggest none; Reaches 5000 records a rule a day |
| FERRUM-PRIVACY-006 | Deletion journal reapplied after a restore | Implemented, qualification incomplete | tested | The restore is simulated in the test; not yet rehearsed in deploy/smoke.sh; Restricted-processing has no journal because it has no operation yet |
| FERRUM-PRIVACY-007 | Immutable evidence without duplicated personal data | Partially implemented | read | Audit diffs of records are redacted on erasure, keeping hashes; other audit payloads are not reviewed |
| FERRUM-PRIVACY-008 | Evidence pack for compliance reviews | Partially implemented | tested | A workspace's pack holds its access review, outside parties, personal data declarations, retention and the state of its history; The control inventory, advisories and licences are in this repository; incident records and backup reports are the operator's and are not in it; No jurisdiction profiles; what a pack satisfies is for a reviewer to say |

## Experience

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-UX-001 | Launcher, bar search, notifications, account switching, administration | Implemented, qualification incomplete | read | Tested per feature in three browsers in earlier work; no standing end-to-end suite in CI |
| FERRUM-UX-002 | Command palette | Implemented, qualification incomplete | read | Finds, opens and creates; it does not run actions on the record in front of you; Tried in one browser; no standing browser test |
| FERRUM-UX-003 | Help center | Missing | probe |  |
| FERRUM-UX-004 | Application completeness contract per app | Partially implemented | read | Every app has a hand-crafted interface, listed under FERRUM-UX-009; no written contract says what each must cover |
| FERRUM-UX-005 | WCAG 2.2 AA qualification | Blocked by external dependency | read | No automated accessibility checks in CI yet |
| FERRUM-UX-006 | Translation, right-to-left, locale formatting | Missing | probe | No translation infrastructure was found in the core; money and dates format per workspace |
| FERRUM-UX-007 | Mobile and offline policy with leases | Partially implemented | read | POS and Docs work offline; no shared policy for what is cached, who sees it or when it expires |
| FERRUM-UX-008 | The record frame: filters, side panel, import, merging, related records, shared views | Implemented, qualification incomplete | tested | Tried by hand in one browser; no standing browser test; A merge writes the chosen values, then merges: two steps, so a failed merge leaves those values on the record that stays; Related counts ask each linking list in turn, which for a much-used record is dozens of requests; only when the tab is opened; A shared view's columns are the view's; someone who did not share it saves a copy to change them |
| FERRUM-UX-009 | A hand-crafted interface for every app, written against reference products | Implemented, qualification incomplete | tested | Each app's tests render its screens to markup; tried by hand in one browser as far as docs/references/README.md says, and no standing browser test exists; Each app lists what was left out in docs/references/README.md; No accessibility audit, translation or independent usability review |

## Apps

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-ERP-001 | Ledger, receivables, payables, invoices, notes, payments, allocations | Implemented, qualification incomplete | probe |  |
| FERRUM-ERP-002 | Bank reconciliation, budgets, period close, statements | Implemented, qualification incomplete | probe |  |
| FERRUM-ERP-003 | Multiple companies, currencies, fiscal years, dimensions | Implemented, qualification incomplete | probe | Dimensions are cost centers |
| FERRUM-ERP-004 | Parallel accounting books | Missing | probe |  |
| FERRUM-ERP-005 | Intercompany transactions, consolidation, eliminations | Missing | probe |  |
| FERRUM-ERP-006 | Accruals, deferred revenue, expense allocation | Partially implemented | probe | Recurring rules exist; deferred revenue and accruals as such were not found |
| FERRUM-ERP-007 | Tax localization for 26 countries as add-ons | Implemented, qualification incomplete | read | Rates are moving from code to editable tax profiles; No country's output has been reviewed by a tax professional |
| FERRUM-ERP-008 | Stock, transfers, serial and batch, expiry, picking, cycle counts | Implemented, qualification incomplete | probe |  |
| FERRUM-ERP-009 | Reposting of backdated stock | Implemented, qualification incomplete | read | A deliberate definition, not a repost: a backdated document is valued when it is posted and never re-values later ones, so history is not rewritten; a correction is a stock reconciliation or landed cost, on its own date |
| FERRUM-ERP-010 | Requisitions, RFQs, supplier quotations, scorecards | Implemented, qualification incomplete | probe |  |
| FERRUM-ERP-011 | Supplier portal | Partially implemented | probe | Quotation links exist; no signed-in supplier portal was found |
| FERRUM-ERP-012 | BOMs, routings, work orders, planning, job cards, quality | Implemented, qualification incomplete | probe |  |
| FERRUM-ERP-013 | Subcontracting | Implemented, qualification incomplete | read | Found by reading the code after an earlier probe missed it: a supplier makes goods from materials sent to a warehouse of ours; its tests were not rerun for this entry |
| FERRUM-CRM-001 | Leads, conversion, opportunities, territories, quotations | Implemented, qualification incomplete | probe |  |
| FERRUM-CRM-002 | Deduplication, forecasting, attribution, quotation approvals, CPQ | Partially implemented | tested | Leads merge, and a weighted forecast and quotation approvals exist; no campaign attribution model or CPQ configuration rules |
| FERRUM-PROJECTS-001 | Tasks, dependencies, milestones, timesheets, billing, profitability | Implemented, qualification incomplete | probe |  |
| FERRUM-PROJECTS-002 | Recurring tasks, resource allocation, portfolios, risks, change requests | Implemented, qualification incomplete | tested | An approved change request is a record: it moves no budget or dates; Capacity is a fixed 40-hour week |
| FERRUM-HELPDESK-001 | Tickets, SLA with business hours, internal notes, knowledge, satisfaction, portal | Implemented, qualification incomplete | probe |  |
| FERRUM-HELPDESK-002 | Escalations, collision handling, entitlements | Implemented, qualification incomplete | tested | Collisions are caught when the second agent saves; nobody sees that someone else is typing; Entitlements inform and do not refuse |
| FERRUM-HR-001 | People, attendance, leave, shifts, expenses, recruitment, appraisals | Implemented, qualification incomplete | probe |  |
| FERRUM-HR-002 | Payroll by jurisdiction with statutory reporting | Partially implemented | probe | No bank-payment file; statutory coverage not established; no independent fixtures |
| FERRUM-HR-003 | Training | Implemented, qualification incomplete | tested | Nothing reminds anyone when a certificate is about to lapse |
| FERRUM-POS-001 | Till, sessions, barcodes, split tenders, returns, loyalty, closing | Implemented, qualification incomplete | probe |  |
| FERRUM-POS-002 | Offline sales: versioned price book, device journal, re-pricing on arrival | Implemented, qualification incomplete | read | Stock is not preallocated to a till: the spec's overselling policy is unmet |
| FERRUM-POS-003 | Printers | Blocked by external dependency | probe | No device code found; receipts print through the browser |
| FERRUM-COMMERCE-001 | Catalog, cart, coupons, checkout, shipping, refunds, abandoned carts | Implemented, qualification incomplete | probe |  |
| FERRUM-COMMERCE-002 | Subscriptions | Missing | probe |  |
| FERRUM-COMMS-001 | Mail, shared mailboxes, chat, calendar, meetings, CalDAV and CardDAV | Implemented, qualification incomplete | probe |  |
| FERRUM-COMMS-002 | Captions, transcripts, recording, presence | Missing | probe |  |
| FERRUM-ASSETS-001 | Assets, depreciation, movements, maintenance, disposal | Implemented, qualification incomplete | probe |  |
| FERRUM-ASSETS-002 | Finance books that keep their own values | Partially implemented | probe | Finance books are labels only; Grouped assets are sold whole; repairs neither extend life nor consume parts |
| FERRUM-BOOKINGS-001 | Resources, buffers, capacity, waitlist, cancellation, guest pages | Implemented, qualification incomplete | probe |  |
| FERRUM-BOOKINGS-002 | Recurring appointments | Implemented, qualification incomplete | tested | On local branch bookings/series, unpushed; A guest gets a confirmation per occurrence; Staff repeat a booking; guests cannot yet from the public page |
| FERRUM-BOOKINGS-003 | Deposits and refunds | Missing | probe |  |
| FERRUM-BOOKINGS-004 | External calendar synchronization | Partially implemented | probe | Signed feeds and add-to-calendar files go out; nothing is read back, so outside appointments do not block slots |
| FERRUM-FIELD-001 | Work orders, dispatch, skills, contracts, checklists, signatures | Implemented, qualification incomplete | probe |  |
| FERRUM-FIELD-002 | Preventive maintenance and an offline technician app | Partially implemented | read | Maintenance plans make work orders, once per visit; there is no offline technician app |
| FERRUM-FIELD-003 | Road travel through a mapping provider | Blocked by external dependency | probe | Straight-line estimates, stated as such |
| FERRUM-FORMS-001 | Conditional logic, pages, spam controls, scoring, record mappings | Implemented, qualification incomplete | probe | No localization |
| FERRUM-LMS-001 | Courses, prerequisites, cohorts, quizzes, assignments, certificates | Implemented, qualification incomplete | probe | Learners see draft course titles and summaries |
| FERRUM-LMS-002 | SCORM, xAPI, LTI, and paid courses | Missing | probe |  |
| FERRUM-DRIVE-001 | Folders, versions, sharing, links, trash, quotas, previews, search, office editing | Implemented, qualification incomplete | probe | Public links outlive their creator's access |
| FERRUM-DOCS-001 | Documents with comments, suggestions, templates, version restore, exports | Implemented, qualification incomplete | probe |  |
| FERRUM-CAMPAIGNS-001 | Audiences, consent, suppression, segments, journeys, tests, bounce handling | Implemented, qualification incomplete | probe | Suppression is Campaigns' own, not shared by every send path; imports are set-based |
| FERRUM-SIGN-001 | Signer order, parallel routing, reminders, sealed PDF, audit certificate, verification | Implemented, qualification incomplete | probe | An electronic signature with an audit trail, not PAdES; no delegation |
| FERRUM-SIGN-002 | PAdES or a qualified signing provider | Blocked by external dependency | probe |  |
| FERRUM-STUDIO-001 | Reports, dashboards, spaces | Implemented, qualification incomplete | read | One child table per query; fields at most table.link.field deep; no comparison periods with pivots |

## Ecosystem

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-ECOSYSTEM-001 | Knowledge and intranet | Partially implemented | probe | Helpdesk's knowledge base only |
| FERRUM-ECOSYSTEM-002 | IT service management | Missing | probe |  |
| FERRUM-ECOSYSTEM-003 | IT asset and configuration management | Missing | probe |  |
| FERRUM-ECOSYSTEM-004 | Integration Hub | Missing | probe |  |
| FERRUM-ECOSYSTEM-005 | Social media management | Missing | probe |  |
| FERRUM-ECOSYSTEM-006 | Website and CMS | Partially implemented | probe | Commerce's storefront pages only |
| FERRUM-ECOSYSTEM-007 | Customer, supplier and partner portals | Partially implemented | probe | A portal shell with tickets and invoices; no supplier or partner portal |
| FERRUM-ECOSYSTEM-008 | Contract lifecycle management | Missing | probe |  |
| FERRUM-ECOSYSTEM-009 | Subscription and revenue operations | Partially implemented | probe | Finance's subscriptions; no dunning-to-entitlement loop |
| FERRUM-ECOSYSTEM-010 | Governed enterprise analytics | Missing | probe |  |
| FERRUM-ECOSYSTEM-011 | Portfolio and objectives | Missing | probe |  |
| FERRUM-ECOSYSTEM-012 | Digital asset management | Missing | probe |  |
| FERRUM-ECOSYSTEM-013 | Developer Hub | Partially implemented | probe | A CLI and documentation; no hub |
| FERRUM-ECOSYSTEM-014 | Governance and risk | Missing | probe |  |
| FERRUM-INDUSTRY-001 | Hospitality: properties, rates, stays, folios, night audit | Missing | probe |  |
| FERRUM-INDUSTRY-002 | Restaurants: menus, tables, kitchen flow, delivery | Missing | probe |  |
| FERRUM-INDUSTRY-003 | Retail and distribution: replenishment, omnichannel, franchise | Missing | probe |  |
| FERRUM-INDUSTRY-004 | Education: admissions, students, fees, timetables, examinations | Missing | probe |  |
| FERRUM-INDUSTRY-005 | Healthcare administration: scheduling, consent, billing | Missing | probe |  |
| FERRUM-INDUSTRY-006 | Manufacturing: planning, quality, traceability, shop floor | Partially implemented | probe | Shop-floor execution exists (FERRUM-IND-011) and has run on simulated equipment only |
| FERRUM-INDUSTRY-007 | Construction and professional services: estimates, variations, site work | Missing | probe |  |
| FERRUM-INDUSTRY-008 | Property and facilities: leases, charges, inspections, portals | Missing | probe |  |
| FERRUM-INDUSTRY-009 | Logistics and fleets: dispatch, routes, proof of delivery | Missing | probe |  |
| FERRUM-INDUSTRY-010 | Government: cases, permits, citizen portals | Missing | probe |  |
| FERRUM-INDUSTRY-011 | Nonprofits: members, donations, grants, volunteers | Missing | probe |  |
| FERRUM-INDUSTRY-012 | EV charging and IoT: devices, sessions, tariffs, settlement | Partially implemented | probe | Devices and telemetry are real (FERRUM-IND-004, FERRUM-IND-005); charging is an example plugin shaped like OCPP, with no OCPP conformance |

## Extensions

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-CONNECT-001 | Shared connector infrastructure: credentials, mapping, sync, reconciliation | Implemented, qualification incomplete | tested | Tested against stand-in providers only: no real provider uses it yet; Refresh tokens for a person-authorized OAuth app are not brokered: only client credentials are; Deletions do not go out to providers; A provider that says wait is left alone per connection; there is no budget shared across connections; Large media still goes through the plugin bridge; streaming by reference is not built; Egress is not restricted per connection |
| FERRUM-CONNECT-002 | Connector catalog with what each provider's API really offers | Missing | read |  |
| FERRUM-CONNECT-003 | Payment gateways | Blocked by external dependency | tested | Stripe, Razorpay and PayPal tested against stand-ins and, for Stripe and PayPal, their published API definitions; never against a live or sandbox account here; Money that cannot be booked becomes a Payment Exception a person decides; a gateway cannot approve one; None is certified |
| FERRUM-CONNECT-004 | Tax and e-invoicing networks | Blocked by external dependency | read | Adapters written to published specifications; none has run against a live or sandbox provider here |
| FERRUM-EXT-001 | Four extension classes with their security models | Partially implemented | read | Metadata, WASM and native exist; external services are not a declared class |
| FERRUM-EXT-002 | Plugin resource limits | Implemented, qualification incomplete | tested |  |
| FERRUM-EXT-003 | Cancellation that stops consequential work | Implemented, qualification incomplete | tested |  |
| FERRUM-EXT-004 | Bounded compilation and module cache | Implemented, qualification incomplete | read | The bound itself is not exercised by a test |
| FERRUM-EXT-005 | Emergency disable | Implemented, qualification incomplete | tested |  |
| FERRUM-EXT-006 | SDK, CLI, templates, testing utilities | Partially implemented | read | Not published to a package registry: @ferrum packages still reach into the frontend source, and publishing needs the owner's npm account; Examples for AI tools and industry packs are missing |
| FERRUM-EXT-007 | Compatibility policy and permission review on upgrade | Partially implemented | read | Installation plans show permissions; docs/support-policy.md and the certification levels in docs/plan/integrations.md are drafts the owner has not put in force |
| FERRUM-EXT-008 | Publisher onboarding, checks, review, signing, revocation | Implemented, qualification incomplete | probe |  |
| FERRUM-EXT-009 | Monetization: plans, purchases, payouts, refunds, taxes | Missing | probe |  |
| FERRUM-EXT-010 | Licensing of each artifact reviewed | Blocked by external dependency | read | Core AGPL-3.0-or-later; the SDK's terms and plugin obligations need counsel |
| FERRUM-EXT-011 | Credential broker: a plugin uses a secret without reading it | Implemented, qualification incomplete | tested | Bearer, basic, HMAC checks and OAuth client credentials; no authorization-code or refresh-token flow; Not reviewed by an independent security tester |

## Ai

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-AI-001 | Assistant acts as the person with writes approved in the conversation | Implemented, qualification incomplete | tested | Approval is bound to the stored call, the record's version and 30 minutes; estimated financial impact is not shown |
| FERRUM-AI-002 | Shared AI gateway: providers, allowlists, regions, spending limits | Partially implemented | tested | One provider protocol (Anthropic's, or a proxy that speaks it); no regional routing; Budgets are in tokens: no prices are invented; Usage is metered per workspace, person and model |
| FERRUM-AI-003 | Egress policy separate from read permission | Implemented, qualification incomplete | tested | Packages must mark entities `internal` and fields with perm_group: HR's are marked, others not yet reviewed; Fields free of any group go, whatever they hold |
| FERRUM-AI-004 | Authorization-aware retrieval | Partially implemented | read | Tools run as the person, so reads are theirs; no retrieval index, so caches and embeddings do not arise yet |
| FERRUM-AI-005 | Evaluation datasets and prompt-injection tests | Partially implemented | tested | An adversarial test with a model that obeys injected instructions; no evaluation against real models, no grounded-answer datasets, no recorded prompt versions |
| FERRUM-AI-006 | Domain-specific assistants with sources shown | Missing | probe |  |

## Cloud

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-CLOUD-001 | Control plane separate from customer workloads | Missing | probe |  |
| FERRUM-CLOUD-002 | Tenant lifecycle: signup, provisioning, plans, closure | Partially implemented | read | Tenants are created by a task or API; no signup, plans or closure |
| FERRUM-CLOUD-003 | Metering, billing, entitlements | Partially implemented | tested | Metering and account states exist; prices, invoices and payment do not; Storage is the only enforced quota; Connector and communication usage are not metered |
| FERRUM-CLOUD-004 | Fleet inventory, staged upgrades, customer-facing status | Missing | probe |  |
| FERRUM-PARTNER-001 | Partner console | Missing | probe |  |
| FERRUM-PARTNER-002 | Layered support access that expires and is visible to the customer | Implemented, qualification incomplete | tested | The request log is written by the HTTP layer, which the suite exercises directly rather than over HTTP; No partner console shows these: they are API routes |
| FERRUM-PARTNER-003 | Program terms separate from open-source rights | Blocked by external dependency | read |  |

## Operations

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-OPS-001 | Local development and single-server deployment | Implemented, qualification incomplete | read |  |
| FERRUM-OPS-002 | High availability on Kubernetes | Implemented, qualification incomplete | read |  |
| FERRUM-OPS-003 | Dedicated, on-premises and disconnected profiles | Partially implemented | read | No registry mirroring or offline install procedure |
| FERRUM-OPS-004 | Installer preflight and a doctor command | Implemented, qualification incomplete | tested | preflight.sh is checked for syntax, not run on a clean host; Does not send mail, so a mailer's credentials are not proven |
| FERRUM-OPS-005 | Reference workloads and measured capacity | Partially implemented | read | A short load test; no published reference workload or targets |
| FERRUM-OPS-006 | Safe large files: resumable uploads, limits | Partially implemented | read | Direct uploads to S3 up to 2 GiB; apps read files into memory |
| FERRUM-OPS-007 | Malware scanning of uploads | Implemented, qualification incomplete | tested | Tested against a stand-in speaking clamd's protocol, not a real ClamAV; A file is scanned once, on upload; nothing rescans when signatures change; The direct-upload path is not covered by a test |
| FERRUM-OPS-008 | Disaster recovery rehearsed into a clean environment | Implemented, qualification incomplete | read | Compose and kind rehearsals; zone, region and ransomware procedures not written |
| FERRUM-OPS-009 | Structured logs and basic metrics | Partially implemented | read | No OpenTelemetry traces; job, outbox and connection health are metrics, but not per-app business failures such as a posting that failed |
| FERRUM-RELEASE-001 | Build once and promote the qualified digest | Implemented, qualification incomplete | read | The workflows have not run yet: they need a push to main |
| FERRUM-RELEASE-002 | Provenance and SBOM attested at promotion | Implemented, qualification incomplete | read | Not run yet; requires the images to be attested by CI |
| FERRUM-RELEASE-003 | Release manifest of every component | Partially implemented | read | Written by hand at this audit; not yet generated in CI |
| FERRUM-RELEASE-004 | Accepted advisories with owner and review date | Implemented, qualification incomplete | tested |  |
| FERRUM-RELEASE-005 | Published support and compatibility policy | Partially implemented | read | A draft: support windows are the owner's commitment to make, and none is in force |
| FERRUM-RELEASE-006 | Base images pinned by digest | Implemented, qualification incomplete | read | Compose and Kubernetes still name some third-party images by tag |
| FERRUM-IND-001 | Sockets, events and connections hold what they claim | Implemented, qualification incomplete | tested | Socket delivery stays best effort, as it says: physical work never goes through it |
| FERRUM-IND-002 | A durable command ledger with one state machine | Implemented, qualification incomplete | tested | No soak under production-like load |
| FERRUM-IND-003 | One authority per domain, fenced by epoch, moved by a person | Implemented, qualification incomplete | tested | No automatic failover, by design |
| FERRUM-IND-004 | Device identity: enrolled keys, a lifecycle, quarantine | Implemented, qualification incomplete | tested | No key rotation in place: quarantine and enroll; No PKI: mTLS terminates at a proxy the customer runs |
| FERRUM-IND-005 | Telemetry with quality, in day partitions, with rollups | Implemented, qualification incomplete | tested | Not measured against the strategy's 20,000 samples a second |
| FERRUM-IND-006 | A site runtime that survives crashes, outages and restores | Implemented, qualification incomplete | tested | Crash-tested by killing the binary on a laptop, not by power loss on gateway hardware; A contracted offline duration has not been run |
| FERRUM-IND-007 | Adapters: MQTT 5, VDA 5050 2.x, Modbus TCP, simulator, operator tasks | Implemented, qualification incomplete | tested | Real sockets against stand-ins: no real broker under load, no vehicle, no controller |
| FERRUM-IND-008 | OPC UA, Sparkplug B, ROS 2, Open-RMF, VDA 5050 3.0.0 | Missing | tested | Recorded as unsupported, with what each would take |
| FERRUM-IND-009 | Warehouse execution on ERP's stock, with one balance | Implemented, qualification incomplete | tested | No zone, cluster or goods-to-person picking, substitutions, cartonization or carrier connectors |
| FERRUM-IND-010 | Robotic moves in seven steps: arriving is not delivering | Implemented, qualification incomplete | tested | Walked through on a simulated fleet only |
| FERRUM-IND-011 | Execution on ERP's work orders: release gates, genealogy, OEE | Implemented, qualification incomplete | tested | Discrete only: batch, continuous and energy per run are partner work |
| FERRUM-IND-012 | Line supply: a released run brings its material | Implemented, qualification incomplete | tested | Leftovers stay at the line until someone moves them |
| FERRUM-IND-013 | Quality holds that hold, across the warehouse and the floor | Implemented, qualification incomplete | tested |  |
| FERRUM-IND-014 | Maintenance: meters, conditions, lockout, return to service | Implemented, qualification incomplete | tested | A lockout here is a record, not a physical lock |
| FERRUM-IND-015 | A deterministic scheduler with an explained result | Implemented, qualification incomplete | tested | A bounded policy, not an optimizer |
| FERRUM-IND-016 | Agents look and propose, and nothing else | Implemented, qualification incomplete | tested | Autonomy levels A0 to A2 only. Predictive maintenance, vision and the rest of the portfolio are not built |
| FERRUM-IND-017 | Operations, warehouse and floor interfaces | Implemented, qualification incomplete | tested | Walked through by hand on a real server at phone width and in dark mode; no standing browser tests, no accessibility audit, English only |
| FERRUM-IND-018 | An industrial solution package in .fapp | Missing | tested | Apps declare dependencies and permissions today, not connector capabilities, device profiles or acceptance packages |
| FERRUM-IND-019 | Signed site updates | Missing | tested | Take the binary from a trusted source and check its digest |
| FERRUM-IND-020 | Threat model, standards scope and safety boundaries | Partially implemented | read | No hazard interface work, no IEC 62443 assessment, no independent review |
| FERRUM-IND-021 | Partner fleet health, support grants, certification tiers | Missing | tested | Nothing built. Support access exists for workspaces (docs/support-access.md) |

## Verification

| Id | Requirement | Status | Basis | Known limitations |
|---|---|---|---|---|
| FERRUM-VERIFY-001 | Tests detect broken invariants | Implemented, qualification incomplete | tested | Covers this work's invariants, not the whole platform |
| FERRUM-VERIFY-002 | Independent domain fixtures for finance and payroll | Missing | read |  |
| FERRUM-JOURNEY-001 | Lead to cash | Partially implemented | tested | Runs through the kernel's API, not through the interface; no independent reviewer prepared the figures |
| FERRUM-JOURNEY-002 | Procure to pay | Partially implemented | tested | Runs through the kernel's API, not through the interface; no independent reviewer prepared the figures |
| FERRUM-JOURNEY-003 | Hire to retire | Missing | read |  |
| FERRUM-JOURNEY-004 | Support to resolution | Partially implemented | tested | Inside one app's suite, through its API, not through the interface; no independent reviewer |
| FERRUM-JOURNEY-005 | Project to profit | Partially implemented | tested | Inside one app's suite, through its API, not through the interface; no independent reviewer |
| FERRUM-JOURNEY-006 | Online order to return | Missing | read |  |
| FERRUM-JOURNEY-007 | Field work to invoice | Partially implemented | tested | Inside one app's suite, through its API, not through the interface; no independent reviewer |
| FERRUM-JOURNEY-008 | Document to signature | Partially implemented | tested | Inside one app's suite, through its API, not through the interface; no independent reviewer |
| FERRUM-JOURNEY-009 | Marketing to consent enforcement | Partially implemented | tested | Inside one app's suite, through its API, not through the interface; no independent reviewer |
| FERRUM-JOURNEY-010 | Hospitality stay | Missing | read |  |
| FERRUM-JOURNEY-011 | Learning completion | Partially implemented | tested | Inside one app's suite, through its API, not through the interface; no independent reviewer |
| FERRUM-JOURNEY-012 | Tenant lifecycle | Missing | read |  |
| FERRUM-JOURNEY-013 | Partner support | Missing | read |  |
| FERRUM-JOURNEY-014 | AI-assisted operation | Missing | read |  |

