# Ferrum frontend: no-code UI customization and analytics

Status: plan, 2026-10-29.

This document lists everything a Frappe/ERPNext desk user or administrator can shape without writing code. For each
capability it records what Ferrum has today and what Ferrum will build. The areas are forms, lists and views,
reports, dashboards, spaces, print, web forms, collaboration, navigation, data tools, theming, mobile, and logic
builders.

* **Section 2 is the acceptance checklist for the new UI.** An item is done when its *Ferrum target* cell is true in
  the running product and it meets the rules in §4.
* **Section 3 orders the work.** Each task is one PR-sized piece of work and names the API it needs.

**Reading references.**

| Prefix | Path |
|---|---|
| `fjs/` | `upstream/frappe/frappe/public/js/frappe/` |
| `pjs/` | `upstream/frappe/frappe/public/js/` |
| `fr/` | `upstream/frappe/frappe/` |
| `en/` | `upstream/erpnext/erpnext/` |

Ferrum paths are relative to `ferrum/`. Frappe is `develop` at `e30f4da` (2026-10-28, v17.0.0-dev). Appendix A
describes the method and sources.

**Naming.** In Ferrum, "workspace" already means a tenant ([apps.md](apps.md)). Frappe's desk *Workspace* (a home
page made of blocks) is therefore called a **Space** here.

---

## 1. Frappe's customization model

### 1.1 Where customization lives

| Mechanism | What it stores | How it is applied | Source |
|---|---|---|---|
| DocType JSON | Standard fields, settings and layout. Section, Column and Tab Breaks are themselves fields | Synced from app files on `bench migrate` | `fr/core/doctype/docfield/docfield.json` |
| Custom Field | One row per added field (`dt`, `insert_after`, every docfield property) | Merged into meta. Saving it runs ALTER TABLE inside the request | `fr/custom/doctype/custom_field/` |
| Property Setter | `(doc_type, field_name or row_name, property) → value`, for DocField, DocType, DocType Link, DocType Action and DocType State. A customized layout is stored as one `field_order` JSON | Merged into meta. Fields are then re-sorted by heuristic (`fr/model/meta.py:559`) | `fr/custom/doctype/property_setter/` |
| Customize Form | One-doctype UI over Custom Field + Property Setter (≈40 doctype properties, ≈60 field properties, links, actions, states), with the Form Builder embedded. Actions: Reset Layout, Reset All Customizations, Trim Table, Export Customizations (developer mode only, "Sync on Migrate", `fr/modules/utils.py:69-91`) | Writes Custom Field and Property Setter rows | `fr/custom/doctype/customize_form/` |
| DocType Layout | Alternative layouts per doctype: `condition`, `based_on` (inheritance), child-table layouts, default print format and email template | Chosen per document by `_resolve_layout` in `fjs/form/form.js` and the `?layout=` route parameter | `fr/core/doctype/doctype_layout/`, `pjs/layout_builder/` |
| Client Script | JS per doctype and view (Form or List) | Shipped as source strings inside the meta payload, then run in the browser | `fr/custom/doctype/client_script/`, `fr/desk/form/meta.py:100-160` |
| Server Script | Restricted Python for doc events, API, scheduler, permission query and workflow task | `safe_exec` | `fr/core/doctype/server_script/` |
| App code | `{doctype}.js`, `_list.js`, `_calendar.js`, `_tree.js`, `_dashboard.py`. ERPNext ships 288 form controllers, 74 list files, 8 calendar files, 17 tree files and 68 dashboard files | Sent in the same meta payload as Client Scripts | `en/*/doctype/*/` |
| View-config doctypes | List View Settings, List Filter (saved layouts), Kanban Board, Calendar View, Report, Dashboard Chart, Number Card, Dashboard, Workspace (plus the Custom Workspace overlay), Workspace Sidebar, Sidebar, Custom Sidebar, Dock, Desktop Icon/Layout/Settings, Print Format, Letter Head, Print Style, Print Format Snippet, Web Form, Form Tour, Module Onboarding, Document Template | One doctype per surface, each with its own sharing flags (`is_public`, `private`, `for_user`, `is_standard`, roles) | `fr/desk/doctype/`, `fr/printing/doctype/`, `fr/website/doctype/` |
| Per-user settings | Last view, columns, filters, sort, group-by, page length, kanban and calendar choice | Held in a Redis hash and flushed to `__UserSettings` by an hourly job | `fr/model/utils/user_settings.py:13-60`, `fr/hooks.py:343` |
| Navigation arrangements | Sidebar and dock in two layers (site and user), desktop layout | One editor shared by those surfaces; each stores its layers in its own doctypes | `fjs/ui/sidebar/arrangement_editor.js` |

### 1.2 Weak spots

| Area | Weak spot | Evidence |
|---|---|---|
| Reliability | Each property has one slot, and the last write wins. A customized layout freezes `field_order`, so new upstream fields are placed by heuristic | `property_setter.py:37-40` ([analysis-erpnext.md](analysis-erpnext.md) #12); `fr/model/meta.py:559` |
| Reliability | Moving customizations between sites takes developer mode, an export into an app, and a migrate. There is no diff, no review, and no per-change rollback (only "Reset All") | `fr/modules/utils.py:69-91` |
| Reliability | Personal settings live in Redis and reach the database only hourly. A cache flush loses them | `fr/model/utils/user_settings.py`, `fr/hooks.py:343` |
| Reliability | Client Scripts and app JS patch DOM and jQuery internals. Upgrades break them silently, and errors appear only in the browser | `fr/desk/form/meta.py:100-160`. In ERPNext, 101 of 288 form controllers add buttons and 88 toggle fields |
| Performance | Saving a Custom Field runs ALTER TABLE inside the web request (table locks, full rewrites for NOT NULL) | `custom_field.py:220-230` (analysis #4) |
| Performance | Lists run COUNT and GROUP BY on every refresh. On large tables the desk silently adds a "last 90 days" filter, and settings exist to switch counts, stats and auto-refresh off | `fjs/list/list_view.js:151-171`, `fr/desk/doctype/list_view_settings/` |
| Performance | About 428 JS and 106 Vue files (≈141k lines) form a global-namespace desk. The meta payload ships controller source for each doctype | `pjs/`; REA run recorded in analysis-erpnext.md |
| Mobile | The form, workspace, print and kanban builders are desktop drag-and-drop UIs. v17 phone support is a bottom bar plus sheets. There is no offline mode, only an online/offline toast | `fjs/ui/mobile_nav.js`, `fjs/dom.js:446-458` |
| Consistency | There are four condition languages. `eval:` JS runs through `new Function`, client side only. Python is used by server scripts, notifications and web-form dynamic filters. Charts and cards use filter JSON, and print uses Jinja | `fjs/form/layout.js:874`, `fjs/utils/utils.js:1198-1214` |
| Consistency | Old and new generations run side by side: kanban v1 and v2 (`use_kanban_v2`), the classic and Vue print builders (`print_format_builder_beta`), frappe-charts and chart islands joined by a lossy adapter, and the jQuery desk next to frappe-ui elements | `fjs/views/kanban/`, `fjs/views/kanban_v2/`, `fr/printing/page/print_format_builder/`, `fjs/ui/island/chart/legacy_config.js` |
| Consistency | One entity's field list is configured in 7 places: form, layout, list settings, saved layouts, kanban cards, report columns and web-form fields. Each has its own personal-versus-global rules and its own reset | §1.1 |
| Security | `depends_on`, `mandatory_depends_on` and `read_only_depends_on` run only in the browser. The server checks only `reqd`, so API clients and web-form posts bypass them | analysis #3; `fr/website/doctype/web_form/web_form.py` |
| Security | Query Reports run raw SQL without row filters. Server Scripts and Jinja can read any table. `ignore_permissions=True` appears at 427 call sites | analysis #2 |
| Security | Custom HTML Blocks inject a `<script>` into the desk page; the shadow DOM isolates styles, not script. Client Scripts run with the user's session | `fjs/dom.js:402-424` |

### 1.3 Ferrum today (baseline)

**Web** (`web/src`, React 19 + TanStack Query):

* **Shell:** module sidebar, Ctrl/Cmd+K palette, `/` to search, notifications with live updates, offline banner, PWA install.
* **Views:** a generic `ListView` (list, board, calendar and timeline, chosen from field heuristics).
* **FormView:**
  * sections, CEL rules and live `_calc`;
  * child grid, lifecycle, workflow and custom actions;
  * HTML print;
  * an audit diff with a hash-chain check;
  * a Discussion tab.
* **Customize:** field table, add field, set property, add rule, a list of ops that can be switched on and off, and
  change-set export/import.
* **ReportView:** code-defined reports with parameters, totals, tree indent and CSV.
* **Admin:** users, roles, field groups, scopes, packages, webhooks, automations, plugins and API keys.
* **Apps:** the app store.

**Metadata:**

* **Overlay ops:**
  * `add_field`, `set_property`, `add_rule`, `add_options`;
  * `set_entity_property`, which covers `form` (sections with `tab`/`visible_if`) and `list`;
  * `define_entity`, `set_workflow`, `set_print_template`;
  * `add_action` (package-only).

  These live in `crates/meta/src/types.rs`.
* **Storage:** ops are stored in order in `meta_overlay` and validated before they are stored.
* **Cache:** replicas drop their cached schema on `pg_notify` (`crates/kernel/src/customize.rs`).

**Biggest gaps:**

* no layout designer;
* no saved views and no personal settings;
* no bulk actions;
* no report builder;
* no charts, number cards, dashboards or Spaces;
* one HTML print template per entity;
* no web forms.

### 1.4 Design primitives the targets rely on

| Id | Primitive | Design |
|---|---|---|
| U1 | **Layers** | Package → tenant → role → user. Every UI object is metadata with a layer and provenance. Builders show who set each value (core, tenant, role, you) |
| U2 | **Change-sets** | Tenant and role edits are typed ops in `meta_overlay`, grouped into named change-sets with a diff. They can be applied or reverted as a unit, exported and imported (exists: `ferrum.changeset/1`), and later promoted from a sandbox. Rollback means deactivating ops. It is never all-or-nothing |
| U3 | **UI objects** | One store and one API, `/api/ui/{kind}`, for: view (list/table/board/calendar/gantt/map/gallery/tree/pivot), form_view, space, dashboard, chart, card, print_layout, letterhead, web_form, nav, tour, checklist, template, schedule, banner and statement_template. Tenant and role layers persist as `put_ui_object` ops, so they join change-sets. The user layer persists in `ui_object` with an owner. Each object has an ETag and roles |
| U4 | **Prefs** | `ui_pref (tenant, user, scope, data, version)` in Postgres, read and written through `/api/prefs`. Writes are debounced and immediate, so preferences follow the user to every device and replica. Each scope can be reset to its default |
| U5 | **CEL everywhere** | One expression language covers conditions, defaults, computed values, indicators, value expressions and thresholds. The server evaluates it and the browser mirrors it (`web/src/lib/expr.ts` exists). There is no `eval:` JS |
| U6 | **Semantic query** | `POST /api/query` and `GET /api/v1/{E}/_aggregate` compile link paths, child aggregates, group-by, measures and pivot to SQL. They always apply the viewer's RLS and field groups. They route to replicas with `statement_timeout`, and results are cached per (query, scope, data version) |
| U7 | **Builders are metadata editors** | Each builder has a live preview of a real or sample record, rendered by the server (`_calc`, print preview). It also has an undo/redo op stack, an autosaved draft, a "Move to…" menu or keyboard path for every drag, and it works at 375 px |
| U8 | **Realtime & concurrency** | `/ws/live` events (exists) plus presence. Every record and UI object is versioned with `If-Match` (exists for records). A stale edit gets a 409 with a rebase, never a silent overwrite |
| U9 | **AI over MCP** | MCP tools read UI metadata, run semantic queries and *propose* change-sets. AI output is always a previewed draft. A human applies it, and the agent is recorded as the actor |
| U10 | **Sandboxed extension** | Tenants get no browser JS and no global CSS. Custom UI comes from plugin Web Components in sandboxed iframes with a typed message API ([architecture.md](architecture.md) §7.2) |

---

## 2. Inventory and targets

Status in the middle column: **exists**, **partial** or **missing**.

### 2.1 Forms

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| FM1 | **Drag-and-drop Form Builder** (`pjs/form_builder/`):<br>• tabs → sections → columns → fields, with drag between them;<br>• add field with a type search; property panel; Fetch From and link-filter editors;<br>• "Show Preview" read-only mode;<br>• undo/redo (`store.js:411`, 50 steps, Ctrl+Z / Ctrl+Shift+Z);<br>• standard fields can only be hidden;<br>• embedded in DocType, Customize Form, Web Form and DocType Layout | **missing**. `Customize.tsx` edits fields in a table. Layout changes only through a raw `set_entity_property form` op. `FormView.tsx` ignores section `tab`/`visible_if` (the web `Section` type lacks them) | **Form Designer (U7).**<br>• Canvas built from the real FormView components, with a field palette and an inspector.<br>• Emits fine-grained ops (`place_field`, `add_section`, `set_section`, `add_tab`) keyed to stable section ids.<br>• Live preview on a real record via `_calc`.<br>• Undo/redo over unsaved ops; Save = one change-set; every drag also has a "Move to…" menu.<br>**Better:** package upgrades merge into a customized layout instead of being frozen by `field_order` |
| FM2 | **Customize Form, doctype properties** (`fr/custom/doctype/customize_form/customize_form.json`):<br>• fields: title, image, subject and sender fields; search fields; sort; `default_view` (force); quick entry; editable grid; track changes and views; allow copy/import/auto-repeat; max attachments; public attachments; protect attached files; email append-to; link display mode; title and image in links; preview popup; translated doctype; naming rule; queue in background; grid page length and search threshold; default print format and email template;<br>• actions: Reset Layout, Reset All, Export Customizations, Export Custom Permissions, Trim Table, Customize Child Table | **partial**. `set_entity_property` covers label, label_plural, description, icon, title_field, list, search_fields and form (`crates/meta/src/effective.rs:288`). Change-sets export/import. Each op can be switched off | Add `image_field`, `default_view`, `quick_entry`, `max_attachments`, `track_views`, `link_display` (number, title or both), `preview_fields` and `success_actions`.<br>"Reset" deactivates the selected ops rather than everything.<br>Trim Table is not needed: custom fields are `ext` JSONB with no DDL.<br>Each property shows its layer of origin |
| FM3 | **Field properties (≈60)** (`customize_form_field.json`):<br>• label; `fieldtype` changes only inside `ALLOWED_FIELDTYPE_CHANGE` (`customize_form.py:853`); options; default; description; placeholder;<br>• reqd, unique, read_only, hidden; the 4 `*depends_on` conditions;<br>• fetch_from and fetch_if_empty; link_filters; permlevel;<br>• in list view, standard filter, global search, preview; bold;<br>• precision, length, min/max, non_negative;<br>• allow_on_submit, set_only_once, no_copy;<br>• print_hide, print_width, grid columns;<br>• quick entry, bulk edit, mask, sort_options, button_color, translatable, remember last value | **partial**. `set_property` accepts label, description, required, hidden, read_only, in_list, in_filter, default, default_expr, visible_if, required_if, read_only_if, max_length, min and max. `add_options` exists ([api.md](../api.md)) | Widen the allow-list:<br>• placeholder, precision, fetch_from, link_filters;<br>• in_search, in_preview, bold, grid_width;<br>• allow_on_submit, set_once, no_copy, print_hide, quick_entry, mask, perm_group;<br>• `set_options` (reorder or rename, with a data-migration preview);<br>• safe type widening (data→text, int→decimal), checked against stored data.<br>CEL editor with field autocomplete and "test on this record". Every rule is enforced on the server |
| FM4 | **Custom Field**: `insert_after` plus every property. Saving runs ALTER TABLE (`custom_field.py:220-230`) | **exists**. `add_field` with `after`, `x_` namespace, stored in `ext` JSONB with no DDL, validated before it is stored (`crates/kernel/src/customize.rs`) | Add "index this field" (an expression-index job) and "promote to column" for dedicated tenants.<br>Show an impact panel (views, reports, rules and templates that use the field) before hide or removal (T95). |
| FM5 | **Field types** (`docfield.json`, `fjs/form/controls/`): Autocomplete, Attach, Attach Image, Attachment Gallery, Barcode, Button, Check, Code, Color, Currency, Data, Date, Datetime, Duration, Dynamic Link, Float, Geolocation, Heading, HTML, HTML Editor, Icon, Image, Int, JSON, Link, Long/Small Text, Markdown Editor, Password, Percent, Phone, Rating, Read Only, Select, Signature, Table, Table MultiSelect, Text, Text Editor (Quill with @mentions), Time, Vue Component | **partial**. 21 types (`crates/meta/src/types.rs`): data, text, markdown, code, int, decimal, currency, percent, date, datetime, check, select, link, table, json, email, phone, url, attach, color, secret | Add time, duration, rating, image (with thumbnail), gallery, multi-select link, dynamic link, rich text (sanitized HTML), signature, geolocation (GeoJSON), barcode/QR with camera scan (MO3), and display-only heading/info.<br>Plugin field widgets run as sandboxed Web Components (U10) instead of "Vue Component" |
| FM6 | **Conditional logic**: `depends_on`, `mandatory_depends_on`, `read_only_depends_on` and `collapsible_depends_on`, evaluated in the browser with `eval:` and `new Function` (`fjs/form/layout.js:874`). The server checks only `reqd` | **exists** for fields: CEL `visible_if`, `required_if` and `read_only_if` are server-enforced and mirrored by `web/src/lib/expr.ts`.<br>**missing** in the web app: section and tab `visible_if` are in the metadata but not rendered | Render section and tab `visible_if`; add `collapsed_if`.<br>The server ignores `required` for fields inside hidden sections.<br>A "Why is this hidden/required?" popover shows the CEL and its current inputs |
| FM7 | **Conditional layouts** (DocType Layout, `fr/core/doctype/doctype_layout/`): condition, `based_on` inheritance, child-table layouts, default print format and email template; resolved by `_resolve_layout` (`fjs/form/form.js`); visual builder in `pjs/layout_builder/` | **missing**. One `form` per entity | `form_view` objects (U3) with `applies_if` (CEL), `roles`, `device` (phone or desktop) and `based_on`.<br>The designer shows which view applies to a given record.<br>The phone view is derived automatically (one column, tabs become an accordion) unless overridden |
| FM8 | **Quick Entry and create-from-link**: `fjs/form/quick_entry.js` (mandatory and `allow_in_quick_entry` fields, "Edit Full Form"); Link control "Create a new {0}" and an advanced search dialog (`fjs/form/controls/link.js`, `fjs/form/link_selector.js`) | **missing**. `LinkInput` searches with link filters only (`web/src/app/widgets.tsx`) | A quick-create drawer driven by `quick_entry` fields (default: required fields plus flagged ones).<br>Opened from any link input with the typed text prefilled; it returns the new id to the parent form.<br>An advanced picker offers filters and columns |
| FM9 | **Child-table grid** (`fjs/form/grid.js`, `grid_row.js`, `grid_pagination.js`, `grid_import.js`):<br>• inline grid and row form;<br>• add multiple, duplicate, delete selected or all, move to row N, drag reorder, bulk edit rows;<br>• per-user columns with width and sticky ("Configure Columns");<br>• pagination and search past a row threshold;<br>• keyboard shortcuts (`form.js:242-283`);<br>• import rows from CSV, Excel or Google Sheets, with mapping and "Fix Issues" | **partial**. `TableGrid` (`web/src/app/widgets.tsx`): inline edit, row dialog, Alt+Up/Down reorder, per-cell errors, totals via `_calc` | A spreadsheet-grade grid ([architecture.md](architecture.md) §16):<br>• virtualized rows;<br>• paste a block from Excel or Sheets with typed coercion and a preview of rejected cells;<br>• fill-down; multi-row select for bulk edit, duplicate or delete;<br>• per-user column layout (U4);<br>• row import through the DT1 pipeline; scan-to-add-row (MO3).<br>**Better:** totals are bit-identical to the server (ferrum-calc WASM) |
| FM10 | **Form toolbar and shortcuts** (`fjs/form/toolbar.js:374-735`):<br>• Duplicate, Rename (editable title), Jump to field, Show Links, Print, Email, Reload, Delete;<br>• Remind Me, Follow, Copy to clipboard, Undo/Redo (`fjs/form/undo_manager.js`), Repeat, New;<br>• View Audit Trail, Request via Web Form, Templates, Settings, Customize;<br>• keys: Ctrl+S, Ctrl+P, Ctrl+Shift+> / < for next and previous record (`form.js:205-240`) | **partial**. Save (Ctrl+S), submit/cancel/amend, workflow and custom actions, HTML print, delete, reload, audit tab and a navigation guard (`web/src/app/FormView.tsx`) | Add duplicate (honours `no_copy`), previous/next within the originating saved view, jump to field, and copy link.<br>Add email (PD6), templates (FM13), reminders (CO7), field-level undo/redo, and a local draft autosave that survives reload and going offline.<br>The command palette lists every form action with its shortcut |
| FM11 | **Connections, links and previews**:<br>• `fjs/form/dashboard.js`: linked-document counts grouped from `_dashboard.py` and DocType Links, heatmap, stats, progress;<br>• `fjs/form/linked_with.js` (Show Links);<br>• `fjs/ui/link_preview.js` (hover popover, `show_preview_popup`);<br>• `fjs/form/info_card.js`;<br>ERPNext ships 68 `*_dashboard.py` | **missing** | A Connections panel derived from real foreign keys, so no hand-written `_dashboard.py`. It shows counts per referencing entity (`_aggregate`), with a prefilled "new linked record" button; groups can be hidden or reordered by ops.<br>Hovering a link shows a card of `in_preview` fields, limited to what the viewer may read |
| FM12 | **Declarative actions and states**: DocType Action (Server Action or Route, group, hidden) and DocType State (title and colour) edited in Customize Form (`fr/core/doctype/doctype_action/`, `doctype_state/`).<br>In ERPNext, 101 of 288 form controllers add custom buttons in code | **partial**. `ActionDef` (docstatus, permission, confirm, CEL condition) comes from packages; `add_action` is package-only. Badges show only docstatus and workflow state | Tenant `add_action`, bound to an automation or plugin endpoint, with a parameters dialog, confirm text, CEL condition, group and icon.<br>`set_indicators` (CEL → tone and label) drives the form header, list rows, board cards and calendar.<br>This replaces `get_indicator` code, found in 67 of 74 ERPNext `_list.js` files |
| FM13 | **Document templates** (`fr/desk/doctype/document_template/`, `fjs/form/template_manager.js`): save the current values, apply them to a new document, private or shared | **missing** | Templates are U3 objects (kind `template`, user or shared). They are applied through `_calc`, so defaults and computed fields recalculate. Fields that no longer exist are reported, not silently dropped |
| FM14 | **Tours and in-form help**: Form Tour (`fr/desk/doctype/form_tour/`: steps by field or CSS selector, conditions, next on click, cross-view UI tours), `fjs/form/form_tour.js`, `pjs/onboarding_tours/` | **missing** | Tours are metadata that reference field names, not CSS selectors, so they survive redesigns.<br>AI drafts a tour from a form view (LA5).<br>Field descriptions and help links show inline |
| FM15 | **Naming**: naming rule, `autoname`, expressions, and the Naming Series editor (`fjs/ui/naming_series.js`, DocType Settings → Naming in `fjs/form/doctype_settings/tabs/naming.js`) | **partial**. `NumberingDef` (template, gapless, on insert or submit, date field) comes from packages; tenants cannot edit it | A `set_numbering` op with a live "next number" preview per company and fiscal year.<br>Numbers are assigned at submit by default, so deleted drafts leave no gaps. Gapless numbering applies only where the law requires it |
| FM16 | **Per-doctype settings hub**: DocType Settings dialog (`fjs/form/doctype_settings/registry.js`) with General, Naming, Workflow, Print Formats, Roles, User Permissions, Notifications, Email Templates and Global Search | **partial**. A Customize page per entity (fields, rules, change list) and the Admin roles page | One "Entity settings" hub with tabs: Fields, Layout, Views, Rules, Actions, Workflow, Numbering, Print, Notifications, Permissions and Search.<br>Each tab edits typed metadata and shows provenance and the pending change-set |
| FM17 | **Permissions UI**:<br>• Role Permission Manager (`fr/core/page/permission_manager/`): permlevels, if_owner, read/write/create/delete/submit/cancel/amend/report/export/import/share/print/email;<br>• User Permissions (`fr/core/doctype/user_permission/`);<br>• the list "Restrictions" indicator (`list_view.js` `show_restrictions`) | **partial**. Admin roles: entity × action (none, own or all), field groups for read and write, record and company scopes (`web/src/app/Admin.tsx`) | The same matrix plus **"view as user"**: render a form or list exactly as a chosen user sees it.<br>Explain in plain words why a record or field is hidden (scope and RLS predicate) |
| FM18 | **Client Scripts** (`fr/custom/doctype/client_script/`), shipped as source strings (`fr/desk/form/meta.py:100-160`).<br>In ERPNext, `set_query` appears in 141 controllers and toggle/`set_df_property` in 88 | **missing**, by design | No tenant browser JS. Declarative replacements:<br>• `link_filters` replaces `set_query`;<br>• CEL rules, defaults and computed fields replace toggles;<br>• actions and automations replace custom buttons;<br>• real custom UI only through U10.<br>The importer converts common patterns automatically and flags the rest |
| FM19 | **Presence and concurrent edits**: `fjs/form/form_viewers.js` ("X is viewing or typing") and the modified-document reload prompt (`form.js` `check_reload`) | **partial**. Optimistic versions (`If-Match`, 409) and a conflict banner driven by live events (`FormView.tsx`) | Presence avatars plus "X is editing field Y".<br>On a conflict, a field-level 3-way merge: non-overlapping changes merge automatically, and overlapping ones are shown side by side. Nobody has to reload and lose work |
| FM20 | **After-save flow and recurrence**: `fjs/form/success_action.js` (next steps after submit) and Auto Repeat (`fr/automation/doctype/auto_repeat/`, toolbar "Repeat") | **missing** | Success actions per entity (print, email, new, open linked) as metadata.<br>Recurring documents run as an automation schedule (LA2) |

### 2.2 Lists and views

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| LV1 | **List columns and settings**:<br>• List View Settings (`fr/desk/doctype/list_view_settings/`): fields, and switches to turn off count, sidebar stats, auto-refresh, comment count, automatic recency filters and scrolling; allow inline edit; show tags;<br>• `fjs/list/list_settings.js`: pick fields, widths, reset;<br>• drag column borders to resize | **partial**. Columns come from `list.columns` or `in_list`, with sortable headers, a count, infinite scroll and live refresh (`web/src/app/ListView.tsx`). The tenant can set `list` by op. There are no per-user columns or widths | A tenant list-settings op (columns, widths, sort, count mode, tags, inline edit) with user overrides in prefs (U4).<br>The column picker includes link-path columns (`customer.territory`) and child aggregates (line count or sum) |
| LV2 | **Saved views ("Layouts")**: List Filter (`fr/desk/doctype/list_filter/`: filters, columns, sort, for_user, is_standard, module, order, route signature) and `fjs/list/list_filter/` (Your Layouts and Global Layouts, manage, duplicate) | **missing**. Filters live only in the URL | Views (U3) at personal, team (role), tenant and package level.<br>A view holds its kind, filters, columns, sort, grouping and kind-specific options.<br>Each role gets a default view; views can be pinned to the sidebar and shared as links.<br>"Promote to team" goes through review (U2) |
| LV3 | **Filters** (`fjs/ui/filters/filter.js`, `filter_list.js`):<br>• operators: `=`, `!=`, like and not like, in and not in, is set and not set, `>`, `<`, `>=`, `<=`, Between, Timespan, descendants of and ancestors of;<br>• a standard filter bar from `in_standard_filter`;<br>• conditions are AND-only; child-table fields can be filtered;<br>• filters travel in the URL | **partial**. Standard filters for `in_filter` fields (link, select, check, date range) in the URL. The server DSL supports comparisons, in, like, between, is, child paths and link paths ([api.md](../api.md)) | A filter builder with AND/OR groups (the server DSL gains `{"or": [...]}`).<br>Relative dates (`@today`, `@this_month`, `@last 90 days`, fiscal periods) and the tokens `@me` and `@my_team`.<br>Link-path and child conditions in the UI, tree operators (ltree), typed value editors.<br>Natural-language filters via MCP, shown as a preview (LA5).<br>Never a silent auto-filter: count modes (LV14) keep lists fast instead |
| LV4 | **Sidebar facets** (`fjs/list/list_sidebar_group_by.js`): Assigned To, Created By, Tags and chosen fields, with counts. "Customize Quick Filters" (`list_view.js:2720`) stores the choice in user settings | **missing** | A facet rail from `_aggregate` (U6), with counts that respect RLS, configurable per view.<br>Above 100k rows counts are approximate, with "exact" on click.<br>On phones the facets become chips |
| LV5 | **Bulk actions** (`fjs/list/bulk_operations.js`, `list_view.js` `get_actions_menu_items`): Edit (set a field), Delete, Submit, Cancel, Print and Bulk PDF (background above 25), Assign To, Clear Assignment, Apply Assignment Rule, Add Tags, Export, Copy to Clipboard, workflow actions | **missing**. No row selection | Select a page, or every row matching the filter, then call `_bulk`.<br>A dry-run count comes first. Each row returns its own result.<br>Large sets run as a background job with progress over `/ws/live`.<br>Each row is audited, and a bulk edit can be undone from the audit versions |
| LV6 | **Inline edit and keyboard**: `allow_edit` in list settings; editable report cells; list keyboard navigation (`list_view.js` `setup_keyboard_navigation`) | **missing** | Inline cell edit with `If-Match`.<br>The same keyboard grid model in every view: arrows or j/k to move, x to select, Enter to open, e to edit |
| LV7 | **View switcher** (`fjs/list/list_view_select.js`): List, Report, Dashboard, Kanban, Calendar, Gantt, Image, Inbox, Map, Tree, File | **partial**. List, board, calendar and timeline, chosen from field heuristics (`web/src/lib/views.ts`) | Every view kind runs over one query model: list, table (report), board, calendar, timeline/gantt, gallery, map, tree and pivot.<br>The kinds offered come from metadata, and each saved view stores its kind |
| LV8 | **Kanban**:<br>• Kanban Board (`fr/desk/doctype/kanban_board/`): column field; columns with colour, archive and order; card and preview fields; swimlane fields; title and image field; assignees, tags, footer date, filters, private;<br>• `fjs/views/kanban_v2/`: column virtualization, bulk move, inline add, swimlanes, and a "saved in another column" conflict notice;<br>• v1 is still shipped | **partial**. `BoardView` (`web/src/app/Views.tsx`): select-field columns, drag plus a "Move to" menu, 3 card fields, a 500-record cap | Board options live in the saved view:<br>• column field (select, link or workflow state);<br>• column order, colours, hidden columns and WIP limits;<br>• swimlanes; a card template (title, image, fields, assignees, tags, indicator);<br>• per-column count and sum (`_aggregate`);<br>• per-column paging with no cap.<br>**Better:** drops are validated against workflow transitions and permissions, and illegal targets are disabled. Cards can be moved by keyboard |
| LV9 | **Calendar**: Calendar View (`fr/desk/doctype/calendar_view/`) plus `{doctype}_calendar.js` (8 in ERPNext). `fjs/views/calendar/calendar.js` has month, week and day; drag to reschedule; range-select to create; hide weekends | **partial**. Month grid and day list on one date field (`Views.tsx` `CalendarView`) | Week, day and agenda views; start/end and all-day events.<br>Drag or resize to reschedule (version-checked); range-select to create.<br>Colour comes from indicators. Several views can overlay. An ICS feed per view uses a signed URL |
| LV10 | **Gantt** (`fjs/views/gantt/gantt_view.js`): view modes from quarter-day to month; progress; drag dates | **partial**. `TimelineView` draws bars from start/end pairs | Drag and resize, a progress field, dependencies (a link to the predecessor), zoom levels, a today line, and critical path plus baseline for projects |
| LV11 | **Tree** (`fjs/views/treeview.js`): expand and collapse, add child, move, rename, delete, rebuild, show disabled. Used for nested-set doctypes (17 ERPNext `_tree.js`) | **missing**. There is no `tree` entity kind yet ([architecture.md](architecture.md) §4.1 plans ltree) | A `tree` kind (ltree path) and a tree view: lazy children, drag to re-parent with a cycle check, descendant counts, and tree filter operators |
| LV12 | **Map, image, inbox and file views**: `fjs/views/map/map_view.js` (Leaflet; Geolocation or lat/long), `fjs/views/image/image_view.js`, `fjs/views/inbox/inbox_view.js`, `fjs/views/file/file_view.js` | **missing** | A map view (MapLibre, clustering, bounding-box filter), a gallery view on image fields, and a files view (DT6).<br>The inbox is replaced by threads linked to records (CO10) |
| LV13 | **Row presentation**: `{doctype}_list.js` provides `get_indicator`, `add_fields`, formatters and buttons (74 files in ERPNext, 67 with `get_indicator`). Rows show tags, comment counts and likes | **partial**. Rows show the title, docstatus and workflow badges, and type-based cell formatting | A declarative row template: indicator rules (FM12), cell formatters (progress, avatar, rating, currency in document currency, relative date) and a secondary line.<br>A "limited to your scope" note names the scopes applied |
| LV14 | **Scale**: list virtualization (`fjs/list/list_view_virtualization.js`), switches to turn counts off, and the large-table recency filter | **partial**. Cursor (keyset) pagination, 50-row pages | Virtualized rows. A count mode per view: exact, estimated (planner statistics) or none.<br>Target p95 under 150 ms (server) for a filtered, sorted page over 1M rows ([PLAN.md](PLAN.md) benchmark) |
| LV15 | **Realtime list**: `list_view.js` `setup_realtime_updates` | **exists**. `useLiveEntity` refresh (`web/src/lib/live.ts`) | Rows update in place with a highlight. A "N new records" pill replaces jumping the scroll |

### 2.3 Reports and analytics

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| RA1 | **Report Builder** (`fjs/views/reports/report_view.js`):<br>• pick columns, including child-table and linked fields; insert, remove, reorder, resize;<br>• sort; filter area plus inline column filters;<br>• group-by with several aggregates (count, sum, avg, min, max; `fjs/ui/group_by/group_by.js`); totals row;<br>• chart (x axis, y fields, type; `:551`); inline cell edit;<br>• Save, Save As, Delete (Report `json`); export every row to CSV or Excel; print;<br>• Setup Auto Email; split-view side panel (`link_side_panel.js`) | **missing**. Reports exist only as code (`ReportSource`) | A Report Builder over `/api/query` (U6):<br>• fields via link paths and child aggregates; date-bucket grouping; several measures; totals and subtotals;<br>• a pivot toggle (RA8); calculated columns (CEL); conditional formatting; chart panel; drill-down to records.<br>Saved as a `view` (kind table) in a layer.<br>**Better:** it always runs with the viewer's RLS and field groups, never the creator's |
| RA2 | **Query and Script reports**:<br>• Report (`fr/core/doctype/report/report.json`): SQL, Python or Custom; filter and column tables; roles; prepared report; timeout; total row; snapshot;<br>• `fjs/views/reports/query_report.js`: dependent and mandatory filters, summary cards, chart from data, tree expand/collapse, related reports, translated data, masked fields, execution time, access log;<br>ERPNext ships 181 Script, 8 Query and 6 Report Builder reports | **partial**. `GET /api/reports/{name}` runs code reports with parameters (link, date, select, check) and returns columns, rows, totals and `_indent`/`_bold`, with CSV download (`web/src/app/ReportView.tsx`). There are 10 reports (trial balance, GL, P&L, balance sheet, AR, AP, stock balance and ledger, sales and purchase registers) | Report contract v2:<br>• parameters with `depends_on` (CEL); a `chart` hint; `summary` cards;<br>• `drill` (row → filtered view or record); column `link_to`;<br>• tree rows with expand and collapse.<br>The tenant layer never runs raw SQL; Frappe Query Reports skip row permissions (`fr/core/doctype/report/report.py`, analysis #2). Package reports use the permission-aware query builder |
| RA3 | **Background reports and snapshots**: prepared reports with queue dedupe (`query_report.js:1015-1128`), Prepared Report (`fr/core/doctype/prepared_report/`: status, peak memory), snapshot reports | **missing** | `POST /api/reports/{name}/runs` starts a replica job with progress over `/ws/live`. Identical parameters share one run.<br>Results are cached per (parameters, viewer scope).<br>Snapshots are immutable and hash-chained (T101), which is auditor-grade |
| RA4 | **Report customization**: "Add Column" from a linked doctype, "Save as Custom Report" (`query_report.js:2172-2227`), pick columns | **missing** | An `add_report_column` op (a link path from a result column).<br>Saved report variants (hidden columns, default parameters) are views |
| RA5 | **Charts and cards from reports**: "Set Chart", "Add Chart to Dashboard", "Create Card" (`query_report.js:269-466`) | **missing** | Any report, list or query result becomes a chart or card in one click and can be added to a dashboard or Space (DS1–DS3) |
| RA6 | **Scheduled reports**: Auto Email Report (`fr/email/doctype/auto_email_report/`): daily, weekdays, weekly or monthly; HTML, XLSX, CSV or PDF; dynamic date period; send only if there is data | **missing** | Schedules on any saved report, view or dashboard.<br>**Better:** each recipient gets a render made with *their own* permissions, not the creator's. Delivered through the outbox (email or webhook); can skip empty results; each send is audited |
| RA7 | **Export and print**: export dialog (CSV or Excel, with indentation, filters and hidden columns; `query_report.js:1955-1988`), `print_grid.html` and `print_tree.html`, `max_report_rows` | **partial**. CSV only | Styled XLSX: frozen header, real numbers, exact decimals.<br>PDF through Typst with a letterhead. Every export is logged in the audit trail |
| RA8 | **Pivot**: nothing generic in the desk. Frappe has group-by only, and ERPNext pivots are bespoke (`en/selling/page/sales_funnel/`) | **missing** | A pivot view (rows × columns × measures, subtotals, drag fields), aggregated on the server through `/api/query` `pivot`.<br>Perspective re-pivots up to 1M cells in the browser ([architecture.md](architecture.md) §14) |
| RA9 | **Financial statement templates**: ERPNext Financial Report Template (`en/accounts/doctype/financial_report_template/`). Each row has a data source (account data, formula or API), balance type, indent, bold/italic, hide-if-empty, reverse sign, chart inclusion and colour | **partial**. P&L, balance sheet and trial balance are fixed code reports | A statement template designer: lines map to account ranges, tags or dimensions, plus formulas.<br>Multi-ledger, multi-currency and period comparison; drill to journal lines.<br>Templates are versioned metadata in change-sets |
| RA10 | **Analytics permissions**: `is_public` and roles on charts and cards, Report roles, `ignore_permissions` at 427 call sites, raw-SQL reports | **partial**. Reports run through kernel permissions, and every table has RLS | One rule for every analytic (report, chart, card, facet, export, schedule): it executes as the viewer with RLS and field groups.<br>Sharing an analytic never shares data the viewer cannot read. Masked fields stay masked in drill-downs |

### 2.4 Dashboards and Spaces

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| DS1 | **Dashboard charts**:<br>• Dashboard Chart (`fr/desk/doctype/dashboard_chart/dashboard_chart.json`): Count, Sum, Average, Group By, Custom (Dashboard Chart Source) or Report; timeseries with timespan and interval; top-N groups; heatmap; Line, Bar, Percentage, Pie, Donut or Heatmap; several y-axes; colour; static and dynamic filters; public flag and roles; currency; values over chart; empty-state text; `last_synced_on` cache;<br>• `fjs/widgets/chart_widget.js`: date range, refresh, reset, per-user config;<br>• Vue chart islands (`fjs/ui/island/chart/`) | **missing** | Chart = a query (U6) plus an encoding: line, area, bar, stacked, pie, donut, heatmap, funnel or KPI trend.<br>Time buckets include fiscal periods. A compare-to-previous series. Drill-through to a filtered view.<br>Cached per (query, scope, data version) and refreshed live (debounced) when the entity changes.<br>A builder with live preview |
| DS2 | **Number cards**: Number Card (`fr/desk/doctype/number_card/number_card.json`): document count, sum, avg, min or max; a report field; a custom method; filters; percentage change over an interval; colours; currency; full number. `fjs/widgets/number_card_widget.js` ("since last month") | **missing** | A KPI card with a comparison window, sparkline, and target and thresholds (CEL → tone). Clicking opens the filtered list. Formatting uses the tenant's currency and precision |
| DS3 | **Dashboards**: Dashboard (`fr/desk/doctype/dashboard/`), page `fr/core/page/dashboard_view/`, per-user chart config (Dashboard Settings), and per-doctype list dashboards (`fjs/views/dashboard/dashboard_view.js`: Customize, Reset, Save) | **missing** | A dashboard object on a responsive 12-column grid (drag and resize; stacked on phones).<br>Global filters (company, date range, dimension) are bound to widgets. Per-user overrides (U4) can be reset.<br>Kiosk/TV mode, scheduled snapshots (RA6), and an entity dashboard tab beside the list |
| DS4 | **Workspace block editor**:<br>• Workspace (`fr/desk/doctype/workspace/workspace.json`): EditorJS content stored in a Long Text; shortcuts with count filter and format; link cards; charts; number cards; quick lists; custom blocks; public or private; parent; roles; icon; colour; sequence; onboarding;<br>• editor `fjs/views/workspace/workspace.js` and `blocks/`: header sizes, paragraph, card, chart, number card, shortcut, quick list, custom HTML, onboarding, spacer;<br>• Manage Workspaces, "My Space", Reset to Standard | **partial**. Home shows module links, recent documents per entity and My Work (`web/src/app/Home.tsx`) | **Spaces:** a typed block list instead of editor JSON stored in a Long Text.<br>• Blocks: heading, markdown, link group, shortcut with a live count from a saved view, embedded view (mini list or board), chart, card, checklist, plugin block (U10).<br>• Responsive grid; personal, team, tenant and package layers; role targeting; versions; inline editing with undo |
| DS5 | **Workspace overlays**: Custom Workspace (`fr/desk/doctype/custom_workspace/`): Inherit/Visible/Hidden, roles added or removed, icon, colour, sequence, content override | **missing** (no Spaces yet) | Comes free with U1: tenant ops edit a package Space by block id, upgrades merge per block, and "reset to standard" deactivates the ops |
| DS6 | **Custom HTML blocks** (`fr/desk/doctype/custom_html_block/`: html, script, style, roles), whose script runs in the page (`fjs/dom.js:402-424`) | **missing** | Plugin blocks only: sandboxed iframes with typed data access (U10). Static content uses the markdown block |
| DS7 | **Onboarding**: Module Onboarding and Onboarding Step (`fr/desk/doctype/module_onboarding/`, `onboarding_step/`). Actions: create entry, update settings, form tour, view report, go to page, watch video. Steps complete by checking a field value; roles apply. `fjs/widgets/onboarding_widget.js`. ERPNext: 9 onboardings, 69 steps | **missing** | A checklist block whose steps complete from events (for example, the first submitted Sales Invoice) via the outbox. Per role and resumable. Packages ship checklists, and tenants can edit them |

### 2.5 Print and documents

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| PD1 | **Visual print designer** (`pjs/print_format_builder/`):<br>• sections with resizable columns; field chips; tables (column config, Sr No., striped, split across pages);<br>• blocks: linked field, repeater, barcode/QR, image, HTML/Jinja, Typst (experimental), page break, spacer, divider, text;<br>• layers panel, clipboard, snippets (Print Format Snippet);<br>• per-block visibility conditions (`composables/useConditions.js`); letterhead zone editor;<br>• undo (`stores/useLayoutHistory.js`), autosaved drafts (`stores/useDraftSave.js`), versions (`stores/useVersions.js`), "Save & Apply";<br>• preview with a picked record (`composables/usePreviewDoc.js`);<br>• the classic builder is still at `fr/printing/page/print_format_builder/` | **partial**. One MiniJinja HTML template per entity (`set_print_template`). `GET /api/v1/{E}/{id}/print` returns HTML | A designer that emits one structured layout (JSON blocks), compiled both to Typst (PDF) and to MiniJinja (HTML and email).<br>The preview is the real server render of a chosen record, so the PDF is pixel-identical.<br>Blocks: field; table (columns, totals, repeated header, keep-together); linked value; repeater; image; barcode/QR; text with field tokens; page break; conditional (CEL).<br>Undo/redo, drafts, and versions published as change-set entries.<br>Amounts are formatted by the calc engine |
| PD2 | **Code templates**:<br>• Print Format (`fr/printing/doctype/print_format/print_format.json`): Jinja or JS; HTML and CSS; raw printing commands; fonts and colours; margins;<br>• paper sizes A3–A5, Letter, Legal, 80 mm and 58 mm receipts, custom; page-number position;<br>• PDF generator chrome, PlutoPrint or Typst; formats for DocType or Report;<br>• draft and published state;<br>• Print Format Field Template (per-field Jinja) | **exists**, but only one per entity: a MiniJinja HTML template | Several named templates per entity (`add_print_template`: name, language, default, roles).<br>A code editor (MiniJinja or Typst) with live preview and lint.<br>Receipt and label presets (barcode sheets). Escaping is on by default, and templates cannot run SQL |
| PD3 | **Letterheads and styles**: Letter Head (`fr/printing/doctype/letter_head/`: image or HTML header and footer, scripts, sizes, alignment, default, DocType or Report, custom CSS), Print Style (CSS presets), Print Heading | **missing** | A letterhead per company (multi-company), built from template partials that use brand tokens (TB1). Styles are token sets, not free CSS |
| PD4 | **Print settings and print view**:<br>• Print Settings (`fr/printing/doctype/print_settings/`): send as PDF, repeat header and footer, default letterhead, allow draft or cancelled printing, draft heading, table page breaks, page size, font, print server, raw printing, bulk limit;<br>• print view (`fr/printing/page/print/print.js`): format, letterhead, language, PDF, printer mapping, network printer, set default format, new format from the current one | **partial**. HTML opens in a new window (`openPrint`) | A print view with template, letterhead and language switches and PDF download. Draft and cancelled stamps. Defaults per user and role.<br>Thermal and raw printing (ESC/POS) come from a plugin, not core |
| PD5 | **Bulk print** (`fjs/list/bulk_operations.js`: Bulk PDF Export, background above 25 documents, page size) | **missing** | `POST /api/print/bulk` starts a job that returns a merged PDF or a zip, with progress over `/ws/live` |
| PD6 | **Email composer** (`fjs/views/communication.js`): To/CC/BCC, templates, attach the document print (format, language, letterhead), attach files, schedule send, undo send, read receipt, send me a copy | **missing**. Automations can send email | A composer on any record: MiniJinja templates with field tokens, a PDF of the chosen template attached, scheduled sending through the outbox. The thread is stored in the record's Discussion (CO10) |

### 2.6 Web forms and portal

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| WP1 | **Web Form designer**:<br>• Web Form (`fr/website/doctype/web_form/web_form.json`) is edited in the Form Builder's web-form mode (`pjs/form_builder/store.js:211-341`): Get Fields, up to 10 pages;<br>• Web Form Field: required, the `*depends_on` conditions, read-only, default, options, max length or value, placeholder, precision | **missing** | A web form is a `form_view` with `audience: public` or `portal`, built in the same designer, with multiple steps.<br>**Better:** CEL conditions are enforced on the server at submit (Frappe's run in the browser only). The field subset is an allow-list, so unlisted fields cannot be posted |
| WP2 | **Web form behaviour** (`fjs/web_form/web_form.js`, `web_form_list.js`):<br>• route, published, login required, anonymous;<br>• allow edit, multiple, delete, print, comments, attachments; allow incomplete;<br>• success title, message and URL; intro; banner; meta tags; custom CSS and client script;<br>• breadcrumbs; sidebar; hide navbar and footer; embed domains and embed code;<br>• `condition_json`; dynamic filters in Python; document permissions; a list for submitters | **missing** | The same settings as typed options.<br>• Save and resume with a signed token; i18n; theming via tokens (TB1).<br>• Embedding via iframe with a CSP `frame-ancestors` allow-list.<br>• Rate limits and a honeypot on anonymous forms; file type and size limits.<br>• The submitter's list is a portal view (WP4) |
| WP3 | **Request links**: Web Form Request (`fr/website/doctype/web_form_request/`: key, expiry, values) and "Request via Web Form" on a document (`fjs/form/web_form_request.js`) | **missing** | Signed, expiring, single-use links that let an outsider fill chosen fields of one existing record (for example supplier KYC). Each change is audited with the link as actor |
| WP4 | **Portal**: Portal Settings (`fr/website/doctype/portal_settings/`: menu items with roles, default role, home), website sidebar, ERPNext portal pages (orders, invoices, quotations), Web Page and Web Template blocks | **missing**. Plugins can serve public routes (`/api/public/{tenant}/{plugin}/…`) | A portal shell for external users (customer, supplier, employee) that reuses list, form and print views with `audience: portal`.<br>RLS scopes rows to the user's party.<br>Self-service steps (pay, download PDF, raise a ticket) are actions |

### 2.7 Collaboration

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| CO1 | **Comments and mentions**: Quill editor with @mentions (`fjs/form/controls/text_editor.js`, `quill-mention/`); edit and delete in the timeline (`fjs/form/footer/form_timeline.js`) | **exists**. Discussion tab with comments, @mentions, edit and delete (`web/src/app/Collab.tsx`, `/api/v1/{E}/{id}/comments`) | Add reactions, threaded replies, mentions of roles and teams, comments pinned to a field, and markdown with attachments |
| CO2 | **Assignments**:<br>• `fjs/form/sidebar/assign_to.js`: users, user groups, assign to me, priority, due date, comment; creates a ToDo;<br>• list assign, clear and apply rule;<br>• Assignment Rule (`fr/automation/doctype/assignment_rule/`): round robin, load balancing, by field or weighted; conditions to assign, unassign and close; working days; due date from a field | **partial**. Assign with note, due date and priority; close; My Work (`Collab.tsx`, `Home.tsx`). No groups and no rules | Assign to a user, role or team.<br>Assignment rules are automation actions (round robin, load-based, field-based) with CEL conditions.<br>SLA due dates, and a workload board of assignments by person |
| CO3 | **Attachments** (`fjs/form/sidebar/attachments.js`: multi-upload, public or private, limits, preview, copy URL, delete), Attachment Gallery field, protected attachments | **exists**. List, upload, download and delete (`Collab.tsx`, `/api/files`) | Inline previews (images, PDF), drag-and-drop anywhere on the form, per-entity type and size limits, thumbnails, file versions and signed URLs |
| CO4 | **Tags** (`fjs/ui/tags.js`, `tag_editor.js`): `#tag` search, tag filter | **exists**. Tags per record and `/api/tags/{entity}` | Tag facet (LV4), tag colours, bulk tagging (LV5) |
| CO5 | **Sharing** (`fjs/form/sidebar/share.js`): share with a user or everyone (read, write, submit, share); share-key expiry | **missing** | Record grants (user or role, read or write, optional expiry) stored as policy rows that RLS enforces.<br>Read-only share links: signed, expiring, and watermarked when printed |
| CO6 | **Follow and digests** (`fr/desk/form/document_follow.py`: hourly or daily digest email) | **partial**. Follow toggle (`/api/v1/{E}/{id}/follow`) | Notifications for followed records, plus a daily digest option (CO9) |
| CO7 | **Likes and reminders**: `fjs/ui/like.js` ("Liked by me"), `fjs/form/reminders.js` (30 min, 1 h, 4 h, 1 day or custom) | **missing** | Reminders (`POST /api/v1/{E}/{id}/reminders`) delivered as notifications and push.<br>Likes are dropped deliberately; comment reactions replace them |
| CO8 | **Timeline** (`fjs/form/footer/form_timeline.js`, `version_timeline_content_builder.js`): versions ("changed X from A to B"), views, attachments, assignments, emails, likes, workflow; View Audit Trail | **partial**. Separate Discussion and Activity tabs. Activity shows audit diffs and hash-chain validity (`FormView.tsx` `AuditPanel`) | One timeline (`GET /api/v1/{E}/{id}/timeline`) merging audit, comments, assignments, emails and workflow, with filters.<br>**Better:** the audit keeps its tamper-evidence badge |
| CO9 | **Notifications**:<br>• panel (`fjs/ui/notifications/notifications.js`): notifications, upcoming events, mark all read, categories;<br>• Notification Settings (`fr/desk/doctype/notification_settings/`): email for mentions, assignments, shares, event reminders and threads; subscribed documents; per-type preferences | **partial**. Bell with live updates and mark-as-read (`Shell.tsx`, `/api/notifications`) | Preferences per type × channel (in-app, email, push), quiet hours, per-record mute, a daily digest, and web push (MO4) |
| CO10 | **Email on records**: `fjs/views/communication.js`, the Inbox view, Communication, and email append-to (subject and sender fields) | **missing** | Inbound mail is routed to records (by reply token or rule). The outbound composer is PD6. Threads appear in the Discussion tab |
| CO11 | **Presence** (`fjs/form/form_viewers.js`) | **missing** | Presence on forms, lists and builders ("X is editing this view") over `/ws/live` |

### 2.8 Navigation and personalization

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| NP1 | **Search and command bar** (`fjs/ui/toolbar/awesome_bar.js`, `search.js`, `search_utils.js`, `fuzzy_match.js`):<br>• jump to doctypes, reports, pages and workspaces;<br>• "New {0}", "Find {0} in {1}", `#tag`, recent items;<br>• global full-text search (Global Search Settings, `in_global_search`);<br>• Ctrl+K and Ctrl+G (`fjs/ui/keyboard.js:217-226`) | **partial**. Palette (Ctrl/Cmd+K and `/`) with entities, reports, "New X", and search within the current entity (`Shell.tsx` `NavPicker`) | Global search across entities (`GET /api/search`, Postgres full-text plus trigram on `search_fields`, permission-filtered).<br>Recent items; actions on the open record; natural-language commands answered by the assistant (LA5) |
| NP2 | **Keyboard shortcuts** (`fjs/ui/keyboard.js`: global, page and grid groups, Shift+/ help; `fjs/ui/alt_keyboard_shortcuts.js`: Alt shows menu accelerators) | **partial**. Ctrl/Cmd+K, `/`, Ctrl+S and Esc | One shortcut registry with a `?` help overlay, user remapping (U4), and the same keys in every view |
| NP3 | **Sidebar, dock and desktop**:<br>• doctypes: Workspace Sidebar, Sidebar, Custom Sidebar, Dock, Desktop Icon, Desktop Layout, Desktop Settings (`fr/desk/doctype/`);<br>• items: links to a DocType, Page, Report, Workspace, Dashboard or URL (with filters, route options and a target tab); sections; spacers; groups; indent; collapsible; new tab;<br>• Arrangement Editor (`fjs/ui/sidebar/arrangement_editor.js`): site and user layers, eye to hide, preview, reset, export to app | **partial**. The sidebar is generated from package modules (`Shell.tsx` `Sidebar`) | Navigation as layered metadata (package → tenant → role → user), edited in one arrangement editor: drag, eye to hide, rename, add a saved view or URL, preview, reset per layer.<br>Count badges come from saved views; roles have defaults (NP5) |
| NP4 | **User preferences** (`fjs/ui/user_settings_dialog.js`):<br>• profile; theme light, dark or automatic (`fjs/ui/theme_switcher.js`); compact or full width;<br>• dock pinned or floating; show sidebar, search bar and My Space;<br>• form dashboard, timeline and navigation buttons; list sidebar, view switcher and bulk actions;<br>• report split view; email signature, copy to self and read receipts;<br>• language, time zone, session defaults, shortcuts | **partial**. Change password, API keys, connect an AI assistant, install the app, sign out and sign out everywhere (`Shell.tsx` `UserMenu`). Theme follows the OS only | A Preferences page backed by `/api/prefs` (U4): theme (light, dark, system, high-contrast), density, locale (language, time zone, number and date formats), session defaults (company, warehouse, fiscal year), notifications (CO9) and shortcuts. Changes apply live on every device |
| NP5 | **Role-based experience**: Role (desk access, home page); User toggles (search bar, notifications, list sidebar, bulk actions, view switcher, form sidebar, timeline, dashboard); Module Profile; Role Profile; default workspace and app (`fr/core/doctype/role/`, `user/`, `module_profile/`) | **partial**. Permissions hide entities and modules | Experience profiles: UI ops at the role layer that set the default Space, hide modules and actions, pick simplified form views (FM7) and default saved views |
| NP6 | **Personalization storage** (`fr/model/utils/user_settings.py`): last view, columns, filters, sort, group-by, page length and kanban/calendar choice, held in Redis with an hourly database flush | **missing**. State lives only in the URL | A `ui_pref` table (U4) written immediately and readable on every replica. Reset per scope. **Better:** never lost on a cache flush |
| NP7 | **Recents and home**: Route History (`fr/desk/doctype/route_history/`), desktop page (`pjs/desktop_icons.bundle.js`), My Space | **partial**. Recent documents per key entity and My Work (`Home.tsx`) | Per-user recents across entities (`GET /api/recents`), pinned records and views, and a personal Space |
| NP8 | **Split view**: side panel previews (`fjs/ui/side_panel.js`, `fjs/views/reports/link_side_panel.js`, the `report_split_view` user setting) and breadcrumbs (`fjs/views/breadcrumbs.js`) | **missing** | A record drawer that opens any link as the full editable form, not a read-only preview. A master–detail split view for lists on wide screens |
| NP9 | **Languages**: user language; Translation doctype (`fr/core/doctype/translation/`); translated doctype values (`fjs/views/translation_manager.js`); RTL | **missing**. The UI is English only; numbers and dates are formatted with `Intl` (`web/src/lib/format.ts`) | ICU message catalogs for UI strings; per-locale label ops (`set_property label` with a locale); opt-in translated master-data fields; RTL layout |

### 2.9 Data tools

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| DT1 | **Data Import**:<br>• Data Import (`fr/core/doctype/data_import/data_import.json`): insert, update, or insert-or-update; a file or a Google Sheets URL; template options; value mappings; skipped rows; submit after import; mute emails; delimiter sniffing; logs;<br>• wizard (`pjs/data_import_wizard.bundle.js`): Config → Preview → Fix issues → Import (`fjs/data_import/import_preview.js`, `import_tree_preview.js`) | **partial**. `POST /api/v1/{E}/_import` takes a JSON array (up to 5000) and returns ids and per-row errors (HTTP 207) | A wizard:<br>• upload CSV or XLSX, parsed on the server (`_import/parse`, calamine);<br>• map columns, with fuzzy and AI suggestions;<br>• dry run (`_import?dry_run=1`, mode insert, update or upsert, `key=`), with errors shown inline per row and editable;<br>• a background job with progress;<br>• a batch id to undo the import (delete what was created, revert updates from the audit).<br>Child rows are supported; links resolve by number or title |
| DT2 | **Data Export** (`fr/core/doctype/data_export/`: fields including child tables, filters, Excel or CSV; `fjs/data_import/data_exporter.js`); list and report Export | **partial**. CSV of the list with the current filters (`GET /api/v1/{E}/_export`) | A field picker (link paths and child rows), XLSX, and a template that round-trips into DT1. Exports are audited and checked against the `export` permission |
| DT3 | **Bulk update**: Bulk Update (`fr/desk/doctype/bulk_update/`: field, value, condition, limit) and list Bulk Edit | **missing** | `_bulk` update by filter or by ids, with CEL value expressions.<br>A dry-run count and sample diff come first. Runs as a job, is audited, and can be undone |
| DT4 | **Child-table import** (`fjs/form/grid_import.js`: CSV, Excel or Google Sheets into the grid, with mapping and fixes) | **missing** | The DT1 pipeline aimed at one child table of a draft, plus paste (FM9) |
| DT5 | **Duplicate, rename, merge and restore**: `copy_doc` (`no_copy`), rename with merge (`fr/model/rename_doc.py`), restoring deleted documents (`fr/core/doctype/deleted_document/`) | **partial**. UUID keys, so renaming is never needed; drafts can be deleted. There is no duplicate, merge or restore UI | Duplicate (FM10). Merge two masters: foreign keys are re-pointed in one transaction and audited. Restore deleted drafts and masters from the audit log within a retention window |
| DT6 | **File manager** (`fjs/views/file/file_view.js`: folders, upload, zip import and export, cut and paste) | **missing**. `/api/files` upload and download only | A Files view of attachments by record and entity, with search, size and owner. Storage usage per tenant; bulk download as zip |

### 2.10 Theming and branding

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| TB1 | **Desk theme** (`fjs/ui/theme_switcher.js`: light, dark or automatic; `desk_theme` on User) | **partial**. Dark mode follows `prefers-color-scheme` (`web/src/styles.css`); there is no switch | Design tokens (CSS variables) with light, dark and high-contrast themes, switched by the user (NP4).<br>Tenant brand tokens (TB2) apply to desk, portal, web forms, print and email from one source. Contrast is checked against WCAG AA on save |
| TB2 | **Brand and chrome**: System Settings `app_name`; Navbar Settings (`fr/core/doctype/navbar_settings/`: logo, settings and help menu items, announcement); Website Settings (brand HTML, favicon, splash image, footer, login page options) | **missing**. Only generic tenant settings exist (`GET`/`PUT /api/admin/settings`) | Tenant branding: name, logos for light and dark, favicon, accent colour, login text and image, help links. Validated and previewed live. Per-company logos for print |
| TB3 | **Website Theme** (`fr/website/doctype/website_theme/`: SCSS variables, Google font, rounded corners, shadows and gradients, custom SCSS) | **missing** | Covered by the TB1 tokens for portal and web forms. No free SCSS, which keeps upgrades safe. Fonts are self-hosted |
| TB4 | **CSS/HTML/JS injection**: web form custom CSS, letterhead CSS and scripts, Custom HTML Block script, `head_html` | **missing**, by design | No global injection. Styling uses tokens; custom visuals come only from sandboxed plugin components (U10) |
| TB5 | **Announcements**: Navbar Settings announcement widget, `fjs/ui/site_banners.js` | **missing** | Tenant banners with an audience (roles), a schedule, and dismissal remembered in prefs |

### 2.11 Mobile

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| MO1 | **Phone navigation**: bottom tab bar (`fjs/ui/mobile_nav.js`), phone-only pages (`fr/desk/page/search/`, `notifications/`, `profile/`), bottom sheets (`fjs/ui/components/bottom_sheet.js`) | **partial**. Responsive layout with a collapsible sidebar, PWA install and an offline banner (`Shell.tsx`, `web/src/lib/pwa.ts`) | A mobile-first shell: bottom bar (Home, Search, Notifications, Me), sheets for menus and filters, 44 px touch targets, safe areas. Every builder works at 375 px (U7) |
| MO2 | **Offline**: none; only an online/offline toast (`fjs/dom.js:446-458`) | **partial**. The service worker caches the app shell; data always comes from the network (`web/public/sw.js`) | Offline reads of opened records and chosen saved views (IndexedDB, scoped by permissions).<br>Drafts are saved locally. Queued creates and updates replay with `Idempotency-Key` and version checks. Conflicts go to a review queue ([architecture.md](architecture.md) §16) |
| MO3 | **Device capture**: camera (`fjs/ui/capture.js`), barcode scanner (`fjs/scanner/`), Barcode, Geolocation (`fjs/form/controls/geolocation.js`) and Signature fields | **missing** | Native `BarcodeDetector` plus the camera: scan to search and scan to add a row.<br>Photos attach compressed. A geolocation stamp and a signature pad |
| MO4 | **Push** (`fr/push_notification.py`: a relay for Frappe's mobile apps) | **missing** | Web Push (VAPID) from the PWA for mentions, assignments and approvals. Approvals can be actioned from the notification itself |
| MO5 | **Phone forms and lists**: desktop layouts squeezed to fit; builders are desktop-only | **partial**. Responsive CSS for forms and lists | An automatic phone form view (FM7) with a sticky action bar. Lists show as cards with swipe actions (approve, assign) and pull to refresh |

### 2.12 Logic, automation and AI builders (UI surfaces)

| # | Frappe capability (source) | Ferrum today | Ferrum target (design; why better) |
|---|---|---|---|
| LA1 | **Workflow builder**:<br>• `pjs/workflow_builder/`: Vue Flow canvas with state and action nodes, transition edges, properties;<br>• Workflow (`fr/workflow/doctype/workflow/`): states with docstatus and edit roles; transitions with roles, conditions and self-approval; email to creator; transition tasks; action confirmation | **partial**. A `set_workflow` op (states, transitions with roles, CEL condition, `not_creator`) and transitions in the form. No visual editor | A visual designer (React Flow) that emits `set_workflow`.<br>Simulate a record through its transitions.<br>Workflows are versioned, and documents already in flight stay on their version |
| LA2 | **Automations and notifications**:<br>• Automation Flow (`fr/automation/doctype/automation_flow/`): triggers on create, update, field change, delete, submit and cancel, a date offset, cron or a custom event; filters; conditions; relationships; actions; run-as; stop on error; logs of runs and steps;<br>• canvas (`upstream/frappe/ui/src/components/AutomationCanvas/`);<br>• Notification (`fr/email/doctype/notification/`): email, webhook, system or SMS; days before or after; value change; recipients; template; attached print | **partial**. Automations (event, CEL condition, actions to set fields, create a document, notify or email) and webhooks, in Admin (`web/src/app/Admin.tsx`) | An automation canvas with date, cron and field-change triggers.<br>A test mode against a chosen record. A run log with replay.<br>Recurring documents (FM20) and assignment (CO2) are actions |
| LA3 | **Scripts**: Server Script (`fr/core/doctype/server_script/`: restricted Python) and Client Script (FM18) | **partial**. Sandboxed WASM plugins bound to entity × stage (`/api/admin/plugins`); CEL rules | JS/TS snippets compiled to WASM (Javy) with a typed host API.<br>The editor's types come from the effective schema. A dry run on a sample record |
| LA4 | **App and page builder**: Frappe Studio (`fr/studio/`, `upstream/frappe/studio/frontend/`): drag-and-drop frappe-ui components, data sources, page scripts, an AI assistant, export to app source | **missing**. `crates/studio` is a stub app (described as analytics, report builder, charts, cards, dashboards and workspaces, but with no entities yet) | The Ferrum Studio app hosts charts, cards, dashboards, Spaces and the Report Builder.<br>Custom pages are Spaces built from Ferrum blocks (views, forms, charts, plugin blocks), not a free component builder: less to secure and upgrade |
| LA5 | **AI in the desk**:<br>• Flow (`fr/flow/`, `upstream/frappe/flow/`, `pjs/flow_launcher.bundle.js`): an assistant that knows doctypes, respects permissions, builds DocTypes and web forms, and runs agents, tools, RAG and triggers;<br>• an MCP server (`fr/mcp/`) | **partial**. An MCP server with record, report, collaboration and audit tools (`crates/mcp/src/lib.rs`), OAuth 2.1 for hosted clients, and "Connect an AI assistant" (`Shell.tsx`). No customization tools and no in-app assistant | New MCP tools: `describe_ui`, `query` (U6), `propose_changeset` (ops become a draft change-set with a preview link) and `explain_access`.<br>An in-app assistant panel uses the same tools.<br>**Better:** AI never applies schema or UI changes without human approval, and every AI action is audited with the agent as actor |

---

## 3. Build order

Tier 0 unblocks everything else. Tier 1 reaches Frappe parity for daily use, area by area. Tier 2 goes beyond Frappe.
Each task is one PR. "Covers" points to the rows in §2 that define its acceptance.

### Tier 0: foundations

| # | Task | API | Covers |
|---|---|---|---|
| T01 | Render form tabs and section `visible_if` in FormView (add `tab` and `visible_if` to the web `Section` type). The server skips `required` for fields in hidden sections | `GET /api/meta/{E}` (`form[].tab`, `form[].visible_if` are already served by `crates/kernel/src/describe.rs`) | FM1, FM6 |
| T02 | Preferences store: `ui_pref` table with a version; a `usePref(scope)` hook with debounced writes | `GET /api/prefs?scope=`, `PUT /api/prefs/{scope}` | NP6, NP4 |
| T03 | UI object store. Tenant and role layers become `put_ui_object` ops in `meta_overlay`; the user layer goes in `ui_object` with an owner; ETag and roles on each object | `GET/POST /api/ui/{kind}`, `GET/PUT/DELETE /api/ui/{kind}/{id}` | U3, LV2 |
| T04 | Filter DSL: OR groups, relative dates, `@me` and `@my_team`, with one shared test suite for Rust and TS | `GET /api/v1/{E}?filters=` (`{"or": [...]}`, `@today`, `@last 90 days`) | LV3 |
| T05 | Aggregation endpoint that applies RLS and field groups, with `statement_timeout` and an approximate mode | `GET /api/v1/{E}/_aggregate?group_by=&measures=&filters=&approx=` | LV4, LV8, DS1, DS2 |
| T06 | Bulk endpoint: ids or a filter; update, delete, submit, cancel, assign, tag or a workflow action; per-row results. Above 200 rows it runs as a job with progress on `/ws/live` | `POST /api/v1/{E}/_bulk` | LV5, DT3 |
| T07 | Widen the overlay ops:<br>• `set_property` allow-list (FM3) and `set_entity_property` keys (FM2);<br>• `place_field`, `add_section`, `set_section`, `add_tab`;<br>• `set_indicators`, tenant `add_action`, `set_numbering`;<br>• an atomic batch | `POST /api/customize`, `POST /api/customize/batch` | FM1–FM3, FM7, FM12, FM15 |
| T08 | Named change-sets: group ops, show the diff, apply or revert as a unit; export and import keep the grouping | `GET/POST /api/changesets`, `POST /api/changesets/{id}/revert` | U2 |
| T09 | Presence messages (join, leave, editing a field) for records and UI objects | `/ws/live` message `presence` | FM19, CO11 |
| T10 | Semantic query: link-path fields, child aggregates, filters, date-bucket group-by, measures, pivot, sort and limit. Routed to a replica, with RLS applied | `POST /api/query` | RA1, RA8, DS1 |

### Tier 1: parity

| # | Task | API | Covers |
|---|---|---|---|
| T11 | Form Designer: canvas, palette, drag plus a "Move to…" menu, preview on a record, an undo/redo op stack, save as one change-set | T07, `POST /api/v1/{E}/_calc` | FM1 |
| T12 | Field inspector for every allowed property, with a CEL editor (autocomplete, test on a record) | T07, `GET /api/meta/{E}` | FM3, FM6 |
| T13 | Entity settings hub with tabs and provenance per property | `GET /api/meta/{E}`, `GET /api/customize` | FM2, FM16 |
| T14 | Field types, batch 1: time, duration, rating, image, multi-select link, dynamic link (meta, kernel validation and widgets) | `crates/meta` `FieldType`, `GET /api/meta/{E}` | FM5 |
| T15 | Field types, batch 2: sanitized rich text, signature, geolocation, barcode/QR | as T14 | FM5, MO3 |
| T16 | Quick-create drawer opened from link inputs | `POST /api/v1/{E}`; meta `quick_entry` | FM8 |
| T17 | Form views with `applies_if`, roles and device, plus the automatic phone layout | T03 kind `form_view`; `GET /api/meta/{E}` returns the applicable `form_views` | FM7, MO5 |
| T18 | Toolbar parity: duplicate (`no_copy`), previous/next in the originating view, jump to field, copy link | `GET /api/v1/{E}` (view filters) | FM10 |
| T19 | Connections panel and hover previews | `GET /api/v1/{E}/{id}/links`, `GET /api/v1/{E}/{id}?fields=` | FM11 |
| T20 | Declarative indicators everywhere, and tenant actions with parameter dialogs | T07, `POST /api/v1/{E}/{id}/actions/{action}` | FM12, LV13 |
| T21 | Grid v2: virtualized rows, block paste, fill-down, multi-row bulk edit, per-user columns | T02, `_calc` | FM9 |
| T22 | Document templates | T03 kind `template`, `_calc` | FM13 |
| T23 | Numbering editor with a next-number preview | T07, `GET /api/meta/{E}/numbering/preview` | FM15 |
| T24 | Draft autosave (IndexedDB) and field-level undo/redo in forms | client only | FM10, MO2 |
| T25 | Tours: tour objects keyed by field names, a player, and an AI-draft hook | T03 kind `tour` | FM14 |
| T26 | Success actions per entity (print, email, new, open linked) after save or submit | T07 `set_entity_property success_actions` | FM20 |
| T27 | Importer converts Client Scripts and `eval:` conditions (`set_query` → `link_filters`, toggles → CEL) and reports what it could not convert | importer emits `ferrum.changeset/1` → `POST /api/customize/import` | FM18 |
| T28 | Field-level 3-way merge on a version conflict | `GET /api/v1/{E}/{id}/audit`, `PATCH` with `If-Match` | FM19 |
| T29 | "View as user" and permission explanations | `GET /api/v1/{E}/{id}/explain?as=`, `GET /api/meta/{E}?as=` | FM17, RA10 |
| T30 | Row selection and the bulk actions menu | T06 | LV5, CO4 |
| T31 | Column picker, resize and reorder (user level) and list settings (tenant level) | T02, T07 `set_entity_property list` | LV1 |
| T32 | Filter builder UI: AND/OR, relative dates, link paths | T04 | LV3 |
| T33 | Saved views: save, update and delete at personal, team and tenant level; a default per role; the switcher lists saved views; share links | T03 kind `view` | LV2, LV7 |
| T34 | Facet rail | T05 | LV4, CO4 |
| T35 | Inline cell edit and list keyboard navigation | `PATCH /api/v1/{E}/{id}` with `If-Match` | LV6 |
| T36 | Board v2: board options, swimlanes, column counts and sums, workflow-aware drops, per-column paging | T03, T05, `POST /api/v1/{E}/{id}/workflow/{action}` | LV8 |
| T37 | Calendar v2: week, day and agenda; drag or resize to reschedule; range to create; colours | T04, `PATCH` | LV9 |
| T38 | Gantt v2: drag and resize, progress, dependencies, zoom | `PATCH`, view options | LV10 |
| T39 | `tree` entity kind (ltree, move with a cycle check) and a tree view | `GET /api/v1/{E}?parent=`, `POST /api/v1/{E}/{id}/move` | LV11 |
| T40 | Gallery and map views | list API, `_aggregate`, filter op `within` (bounding box) | LV12 |
| T41 | List virtualization and count modes | `GET /api/v1/{E}?count=exact`, `estimate` or `none` | LV14 |
| T42 | Realtime row patching and the "N new" pill | `/ws/live` | LV15 |
| T43 | Report Builder UI, saved as a `view` of kind table | T10, T03 | RA1 |
| T44 | Report contract v2 (`depends_on` parameters, chart, summary, drill, tree rows), rendered in ReportView | `GET /api/reports`, `GET /api/reports/{name}` | RA2 |
| T45 | XLSX and PDF export for lists and reports | `GET /api/v1/{E}/_export?format=xlsx&fields=`, `GET /api/reports/{name}?format=xlsx` or `pdf` | RA7, DT2 |
| T46 | Background report runs with dedupe and progress | `POST /api/reports/{name}/runs`, `GET /api/reports/runs/{id}` | RA3 |
| T47 | Report variants and added columns | new op `add_report_column` via `POST /api/customize`; T03 kind `view` | RA4 |
| T48 | Scheduled deliveries rendered as each recipient | T03 kind `schedule`, outbox | RA6 |
| T49 | Pivot view | T10 `pivot` | RA8 |
| T50 | Financial statement template designer | T03 kind `statement_template`; finance `ReportSource` | RA9 |
| T51 | Chart object, builder and renderer; "Add to dashboard/Space" from lists and reports | T03 kind `chart`, T10, T05 | DS1, RA5 |
| T52 | Number card with comparison and thresholds | T03 kind `card`, T05 | DS2 |
| T53 | Dashboards: grid, global filters, per-user overrides, kiosk mode | T03 kind `dashboard`, T02 | DS3 |
| T54 | Spaces (typed blocks, layers, role targeting), replacing Home | T03 kind `space` | DS4, DS5, NP7, LA4 |
| T55 | Onboarding checklists completed by events | T03 kind `checklist`, outbox events | DS7 |
| T56 | Layered navigation arrangement editor | T03 kind `nav` | NP3 |
| T57 | Typst PDF rendering and a print view (template, letterhead and language switches) | `GET /api/v1/{E}/{id}/print?format=pdf&template=&lang=&letterhead=` | PD4 |
| T58 | Several named print templates per entity, with defaults | new op `add_print_template` via `POST /api/customize` | PD2 |
| T59 | Letterheads per company, and brand tokens in print | T03 kind `letterhead` | PD3 |
| T60 | Visual print designer: layout → Typst and MiniJinja, server preview, drafts, versions, publish | T03 kind `print_layout`, `POST /api/print/preview` | PD1 |
| T61 | Bulk print job | `POST /api/print/bulk` | PD5 |
| T62 | Email composer with templates, a PDF attachment and scheduled sending | `POST /api/v1/{E}/{id}/email` | PD6, CO10 |
| T63 | Public web form runtime: schema, submit, server-side CEL, allow-listed fields, rate limit | `GET/POST /api/forms/{tenant}/{route}` | WP1, WP2 |
| T64 | Web form designer (the same designer, audience public, pages, success settings, embed) | T03 kind `web_form` | WP1, WP2 |
| T65 | Signed request links for one record | `POST /api/v1/{E}/{id}/request_link` | WP3 |
| T66 | Portal shell for party users (RLS party scope) with list, form and print views | record API with views of `audience: portal` | WP4 |
| T67 | Unified timeline | `GET /api/v1/{E}/{id}/timeline` | CO8 |
| T68 | Record sharing (user or role, with expiry) and share links | `POST/DELETE /api/v1/{E}/{id}/shares` | CO5 |
| T69 | Reminders | `POST /api/v1/{E}/{id}/reminders` | CO7 |
| T70 | Notification preferences and digests | `GET/PUT /api/notifications/settings` | CO9, CO6 |
| T71 | Assign to roles and teams; assignment rules as automation actions | `/api/admin/automations` action `assign` | CO2 |
| T72 | Attachment previews and drop-anywhere upload | `/api/files` | CO3 |
| T73 | Comments v2: reactions, threaded replies, role and team mentions, comments pinned to a field | `/api/v1/{E}/{id}/comments` (`parent_id`, `field`, reactions) | CO1 |
| T74 | Global search | `GET /api/search?q=&entities=` | NP1 |
| T75 | Preferences page: theme, density, locale, session defaults | T02 | NP4, TB1 |
| T76 | Shortcut registry, help overlay and remapping | T02 | NP2 |
| T77 | Role experience profiles | T03 role layer | NP5 |
| T78 | Record drawer and split view | existing record API | NP8 |
| T79 | i18n: message catalogs and per-locale label ops | T07 `set_property label` with a locale | NP9 |
| T80 | Import wizard: parse, map, dry run, background job, undo | `POST /api/v1/{E}/_import/parse`, `POST /api/v1/{E}/_import?dry_run=1&mode=&key=`, `DELETE /api/imports/{batch}` | DT1, DT4 |
| T81 | Export field picker including child rows | T45 | DT2 |
| T82 | Bulk update by filter, with a dry run | T06 | DT3 |
| T83 | Merge masters and restore deleted records | `POST /api/v1/{E}/{id}/merge`, `POST /api/v1/{E}/_restore` | DT5 |
| T84 | Files view | `GET /api/files?entity=&q=` | DT6 |
| T85 | Theme tokens and a tenant branding editor with a contrast check | `PUT /api/admin/settings` (`brand.*`) | TB1–TB3 |
| T86 | Tenant banners | T03 kind `banner` | TB5 |
| T87 | Mobile shell (bottom bar, sheets) and a 375 px pass over every builder | none | MO1, MO5 |
| T88 | Camera scanning and capture | `/api/files`, list search | MO3 |
| T89 | Visual workflow designer and simulation | T07 `set_workflow`, `POST /api/v1/{E}/_simulate_workflow` | LA1 |
| T90 | Automation canvas, date and cron triggers, run log | `/api/admin/automations`, `GET /api/admin/automations/{id}/runs` | LA2, FM20 |
| T91 | Script snippets: JS/TS compiled to WASM (Javy), typed host API from the effective schema, dry run on a sample record | `POST /api/admin/plugins` (`source`, `lang`), `POST /api/admin/plugins/{id}/dry_run` | LA3 |

### Tier 2: beyond Frappe

| # | Task | API | Covers |
|---|---|---|---|
| T92 | MCP customization tools: `describe_ui`, `query`, `propose_changeset`, `explain_access` | `POST /mcp` | LA5 |
| T93 | In-app assistant panel on the same tools. Natural-language filters, reports and charts return previews, never direct changes | `POST /mcp` | LA5, LV3, RA1 |
| T94 | Sandboxes: clone tenant metadata (plus an optional anonymized sample), promote a change-set, roll back | `POST /api/sandboxes`, `POST /api/changesets/{id}/promote` | U2 |
| T95 | Impact analysis before hiding, removing or retyping a field: which views, reports, rules and templates use it | `GET /api/customize/impact?entity=&field=` | FM4 |
| T96 | Upgrade preview: apply new package versions to a sandbox and list broken references | `POST /api/sandboxes/{id}/upgrade_preview` | U2 |
| T97 | Personal-to-team promotion with review | `POST /api/ui/{kind}/{id}/promote` | LV2 |
| T98 | Offline: read cache of saved views; queued writes with idempotency keys (extend `Idempotency-Key` from create to `PATCH` and actions) | record API | MO2 |
| T99 | Web Push with approve-from-notification | `POST /api/push/subscriptions` | MO4 |
| T100 | Plugin UI slots (field widget, form panel, Space block) as sandboxed iframes with typed messaging | plugin manifest `ui`, `/api/ext/{plugin}` | U10, FM5, DS6, TB4 |
| T101 | Report snapshots: immutable and hash-chained | `POST /api/reports/{name}/snapshots` | RA3 |
| T102 | Customization usage analytics (unused fields and views) with cleanup suggestions | `GET /api/customize/usage` | FM4 |
| T103 | ICS feed per calendar view (signed URL) | `GET /api/ics/{token}` | LV9 |

---

## 4. Rules every item must meet

* **Permissions.** The UI shows exactly what the API returns for the viewer. Nothing is hidden on the client that the
  server would have to protect. Analytics, exports and schedules run as the viewer.
* **Builders (U7).**
  * live preview from the server;
  * undo/redo;
  * unsaved-changes guard;
  * provenance per value;
  * the result is a change-set entry;
  * a 409 leads to a rebase;
  * every drag has a keyboard and menu equivalent.
* **Devices.**
  * works at 375 px wide and in dark mode;
  * honours `prefers-reduced-motion`;
  * touch targets are at least 44 px.
* **Budgets** (server, p95):
  * a filtered, sorted list page over 1M rows: under 150 ms;
  * `_aggregate` with `approx`: under 500 ms;
  * a form opening on a mid-range phone: under 1 s to interactive.
* **Accessibility.** WCAG 2.2 AA: labels, focus order, focus return after dialogs. Reuse the patterns in `web/src/ui/kit.tsx`.
* **Tests.** Pure logic (filters, views, CEL, layout ops) gets vitest unit tests next to the code, like the existing
  `web/src/lib/*.test.ts`. Ops get effective-schema tests in `crates/meta`. Each builder gets one end-to-end flow.
* **i18n.** All user-facing strings go through the message catalog once T79 lands.

---

## Appendix A. Method and sources

* **Frappe and ERPNext.** `develop` clones from 2026-10-28 under `upstream/`. Capabilities were read from the
  desk JS and Vue (`fjs/`, `pjs/`), the doctype JSON of each configuration doctype (fields listed by script), and
  the Python controllers where behaviour mattered (customize form, user settings, meta sort, web form).
* **Graphify.** `upstream/graphify-out/` holds only the AST cache. The full graph is in
  `ERP/graphify-out/graph.json` (55,861 nodes, 131,429 edges). It was used to rank desk modules by degree and to
  choose which files to read. The top hubs are:

  | Degree | Module |
  |---|---|
  | 633 | `form.js` |
  | 614 | `list_view.js` |
  | 524 | `query_report.js` |
  | 516 | `grid_import.js` |
  | 508 | `kanban_v2/kanban_page.js` |
  | 440 | `ui/sidebar/sidebar.js` |
  | 439 | `kanban_board.bundle.js` |
  | 360 | `grid.js` |
  | 330 | `report_view.js` |
  | 321 | `filter_list.js` |
  | 295 | `print_format_builder/utils.js` |
  | 257 | `workspace.js` |

* **REA.** `upstream/rea` (rea-agents 6.8.0) has no `node_modules` or `dist`. Its static JavaScript analyzer needs
  `npm install` and a TypeScript build first, which needs the network, so it was not run in this pass. The earlier
  REA run recorded in [analysis-erpnext.md](analysis-erpnext.md) found 439 files and no ES-module graph. This
  inventory used Frappe's source and the Graphify graph directly.
* **ERPNext's use of the desk features.** These counts are the parity load an ERPNext import has to carry.

  | Artifact | Count |
  |---|---|
  | Workspaces | 15 |
  | Workspace sidebars | 14 |
  | Desktop icons | 24 |
  | Dashboard charts | 44 |
  | Number cards | 47 |
  | Chart sources | 3 |
  | Reports | 195 (181 Script, 8 Query, 6 Report Builder) |
  | Print formats | 71 (39 builder, 24 Jinja, 8 standard) |
  | Web forms | 3 |
  | Module onboardings | 9, with 69 steps |
  | Form tours | 2 |
  | `_list.js` files | 74 (67 define `get_indicator`) |
  | Form controllers | 288 (141 `set_query`, 101 custom buttons, 88 field toggles, 113 server calls) |
  | `_calendar.js` files | 8 |
  | `_tree.js` files | 17 |
  | `_dashboard.py` files | 68 |

  Most of this code exists to set indicators, link filters, buttons and field toggles. The declarative targets in
  FM3, FM12 and FM18 cover those cases without browser code.
* **Ferrum.** `web/src` (every app and lib file), `docs/api.md`, `docs/plan/architecture.md`, `docs/plan/apps.md`,
  `crates/meta/src/types.rs` and `effective.rs`, `crates/kernel/src/customize.rs` and `describe.rs`,
  `crates/mcp/src/lib.rs`, and `crates/studio`.
