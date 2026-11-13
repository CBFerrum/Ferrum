# Forms: references

**Compared with:** formbricks/formbricks `fa289168d436` (apps/web, packages) against ferrum-app-forms 0.1.0 (working tree, `9b21e66`) and the records frame in ferrum (`81b7f7c`). **Date:** 2026-11-01

## What Ferrum has

- One engine for forms, surveys and quizzes: `Form` with 21 question types as child rows, `FormResponse`, `FormsSettings`, 2 roles, 3 reports (`packages/forms/entities/forms.yaml`, `packages/forms/package.yaml`, `src/reports.rs`).
- Every answer checked and normalised on the server; CEL `visible_if` over earlier answers for a question or a whole page; the definition checked as a whole on save, every problem returned at once (`src/answers.rs`, `src/logic.rs`, `src/form.rs`).
- Response rules: open and close window, response limit, one per email or user, anonymous mode, honeypot and minimum fill time, idempotent session token (`src/response.rs`, `src/token.rs`); quizzes with points, passing score and a signed result link; uploads checked by extension and first bytes (`src/files.rs`).
- A response can create a record of any entity through a field map checked against the live schema; owners are notified, respondents can get a throttled autoresponse (`src/mapping.rs`, `FormsSink` in `src/response.rs`).
- Analytics as JSON and CSV: per-question distributions, NPS, Likert, matrix, ranking, daily trend, completion rate (responses / views), paged text answers (`src/analytics.rs`, owner routes in `src/site.rs`).
- Respondents get JSON only: `GET /f/{slug}`, `POST /f/{slug}/responses`, `GET /results/{token}` (`src/site.rs`). Nothing in the app renders a page (no HTML in `src/`, no `ui/`); the missing interface is FERRUM-UX-004 in `ferrum/docs/registry/backlog.md`.
- In the records frame the builder is the generic form: a four-column question grid with a per-row dialog; `options`, `matrix_rows`, `default_value` and `correct_answer` are `json` fields typed as raw JSON; a response's `answers` shows as a JSON block (`ferrum/web/src/app/widgets.tsx` TableGrid and JsonInput, `FormView.tsx`). No action declares `params`, so "Enter a Response" posts an empty body (`forms.yaml`, `FormView.tsx` runAction). Read from code, not run.

## What the references do that Ferrum does not

Short paths: `editor/...` and `link/...` start at `apps/web/modules/survey/`; `summary/...` and `responses/...` at `apps/web/app/(app)/workspaces/[workspaceId]/surveys/[surveyId]/(analysis)/`; other `modules/...` at `apps/web/`.

| Capability | Where in the reference | Ferrum today | Worth building | Size |
|---|---|---|---|---|
| Respondent page: one page at a time, progress bar, Back, welcome and ending cards, progress kept in the browser, offline queue | `packages/surveys/src/components/general/survey.tsx`, `apps/web/modules/survey/link/components/survey-renderer.tsx` | JSON only (`src/site.rs`); no page | must | L |
| Visual builder: type palette, draggable cards, live phone/desktop preview, auto-save badge | `apps/web/modules/survey/editor/components/survey-editor.tsx`, `elements-view.tsx`, `block-card.tsx`, `apps/web/modules/ui/components/preview-survey/index.tsx` | Question grid and raw-JSON cells (`FormView.tsx`, `widgets.tsx`) | must | L |
| Results screens: card per question, response table with a column per question, drawer with prev/next, tags | `summary/components/SummaryPage.tsx`, `responses/components/ResponseTable.tsx`, `ResponseCardModal.tsx`, `modules/analysis/components/SingleResponseCard/components/ResponseTagsWrapper.tsx` | Endpoints and 3 reports only (`src/analytics.rs`, `src/reports.rs`); `answers` is a JSON block | must | M |
| Logic that moves: jump to a page or ending, variables, make-required, condition builder | `editor/components/logic-editor.tsx`, `logic-editor-conditions.tsx`, `logic-editor-actions.tsx`, `packages/types/surveys/logic.ts` | Show/hide only, written as CEL text (`src/logic.rs`) | should | M |
| Partial responses, per-question drop-off and time | `Response.finished`, `ttc` in `packages/database/schema/main.prisma`; `summary/components/SummaryDropOffs.tsx` | Completed responses only; completion = responses / daily views (`src/analytics.rs`, `forms_view` in `packages/forms/sql/001_forms.sql`) | should | M |
| Hidden fields, recall of earlier answers in later text, variables | `editor/components/embedded-data-card.tsx`, `modules/survey/link/lib/hidden-fields.ts`, `apps/web/lib/utils/recall.ts` | `allow_prefill` fills a visible answer only (`forms.yaml`) | should | S |
| Share panel: link, QR, website and email embed, custom slug | `summary/components/shareEmbedModal/*` | Slug field only (`forms.yaml`) | should | S |
| Welcome card, several endings chosen by logic, call-to-action | `editor/components/edit-welcome-card.tsx`, `edit-ending-card.tsx` | One `confirmation_message` and `redirect_url` (`forms.yaml`) | should | S |
| Gates: verify email, PIN, single-use links, reCAPTCHA | `editor/components/response-options-card.tsx`, `link/lib/verify-email-gate.ts`, `pin-token.ts`, `single-use-link.ts` | Honeypot, fill time, one per email or user (`src/response.rs`) | should (email, single-use); later (captcha) | M |
| Several languages, per-language text, translation helper | `modules/survey/multi-language-surveys/*`, `modules/ee/ai-translation` | None; registry says "No localization" (`ferrum/docs/registry/apps.yaml`) | should | L |
| Template gallery | `apps/web/app/lib/templates.ts`, `modules/survey/templates` | `duplicate` action only (`forms.yaml`) | should | S |
| Styling: colours, background, custom CSS | `editor/components/styling-view.tsx`, `modules/custom-css` | `accent_color`, `logo` (`src/form.rs` `public()`) | later | M |
| More question types: consent, call-to-action, picture choice, address, contact info, scheduling embed, CSAT, CES | `packages/types/surveys/constants.ts` | 21 types, none of these (`src/model.rs` `Kind`) | later | S each |
| Quotas per answer segment | `apps/web/modules/ee/quotas` (Enterprise licence) | Total `response_limit` only (`forms.yaml`) | later | M |
| In-app and website intercept surveys with an SDK and segments; pushes to Sheets, Notion, Airtable, Slack | `packages/js-core`, `modules/ee/contacts`, `apps/web/app/(app)/workspaces/[workspaceId]/settings/workspace/integrations/*` | Platform webhooks and automations push out (`ferrum/docs/events.md`) | no | - |

## Screens to hand-craft

1. **Respondent page** (renders what `src/site.rs` already serves). Purpose: the screen every respondent sees; today there is none.
   - Layout: one centred column; logo and accent colour from `theme` (`src/form.rs` `public()`); title and description; one page at a time (the runs between `page_break` rows); slim progress bar; Back / Next / Send bar; ending panel with `confirmation_message` or the redirect; the quiz result view for `/results/{token}`; `closed_message` for closed or full forms.
   - Seen in the reference: Back through a history stack, optional auto-advance on rating and NPS, a hide-Back option, progress and unsent answers saved in the browser, time per question (`survey.tsx`, `response-options-card.tsx`).
   - Proposed: errors under the question named by `field` in the 422; answers kept in session storage, because the server promises "your answers stay" when a token expires (`src/site.rs`); a double click is safe (the same token replays, `src/token.rs`); the honeypot input and open time sent as the server expects.
   - Show and hide run live in the browser, but `ferrum/web/src/lib/expr.ts` resolves only `doc`, `parent`, `user`, `old`, `today` and `now` and treats functions and loops as unknown. `answers` must be added there; the server stays the judge (`src/logic.rs`).
   - Data: has all of it (form, theme, token, prefill, results). Lacks partial saves, several endings, translations. Serve it as a self-contained page like Sign's (`ferrum-app-sign/src/site.rs`) or from a public-site host (`ferrum/docs/plan/apps-ui.md` section 7.3).
2. **Form builder.** Purpose: write questions, pages, logic and settings without JSON.
   - Layout: top bar (title, status chip Draft / Published / Closed, Save, Publish, Duplicate); tabs Questions | Settings | Share; left a stack of page cards holding question cards, right a live preview with a phone/desktop switch (`survey-editor.tsx`, `survey-editor-tabs.tsx`). "Add question" opens a palette of the 21 types.
   - Seen in the reference: collapsible cards with a drag handle (`block-card.tsx`); preview refreshed 300 ms after the last edit (`PREVIEW_DEBOUNCE_MS` in `survey-editor.tsx`); deleting a question used in recall or a quota is stopped with a message, and one used in logic opens a warning first (`elements-view.tsx`).
   - Proposed: collapsed card shows type icon, label and required mark; Alt+Up/Down moves a card (the grid already has the keys, `widgets.tsx`); one option per line (the server accepts lines, `src/form.rs`); a logic row builder (question, operator, value) that writes CEL into `visible_if`, with "edit as CEL"; server errors carry `"row": ["questions", n]` and belong on that card; the same delete warning for `email_question` and mapping rows; the mapping step lists target fields from `/api/meta` with the type rules of `src/mapping.rs`.
   - Data: all in `Form.questions` and validated on save (`src/form.rs`); `@dnd-kit` is already in `ferrum/web/package.json` (used by `designer/Canvas.tsx`). Nothing missing on the server.
3. **Results (Summary and Responses).** Purpose: read the numbers, then work the responses.
   - Layout: date range and Export in the header; stat strip (responses, views, completion %, median time, pass rate or NPS); tabs. Summary: a card per question (bars for choices, 0-10 strip with promoters / passives / detractors, grid for matrix, ranked list, paged text with "show more"), trend chart (recharts is in `ferrum/web/package.json`). Responses: a column per question with a column chooser, filter chips, row opens a drawer with prev/next.
   - Seen in the reference: summary bars are buttons with a "click to filter" tooltip (`summary/components/ClickableBarSegment.tsx`); skipped questions are marked as skipped (`modules/analysis/components/SingleResponseCard/components/ElementSkip.tsx`).
   - Proposed: Mark reviewed, Spam and Create record inside the drawer (the actions exist); spam kept apart; export keeps the filters.
   - Data: `summary`, `answers/{key}`, `export.csv` exist (`src/analytics.rs`). Filtering by an answer value inside the JSON `answers`: not verified. Drop-off needs partial responses, which do not exist.
4. **Share and publish panel.** Purpose: put the form in front of people. Layout: a sheet with the link and copy button, QR, iframe snippet, email embed, slug field with live availability, access (Public / Internal), open and close times, limit, "preview as respondent" (`shareEmbedModal/*`). Interactions: warn before changing the slug of a published form; QR download. Data: `slug`, `status`, `opens_at`, `closes_at`, `access` exist; a public page (screen 1) and an admin list of sites allowed to frame it (planned in `apps-ui.md` section 7.3) do not.

## What Ferrum already does better

- Graded quizzes: points, exact passing score, all-or-nothing multi-select, a signed 30-day result link, correct answers never sent before submit (`packages/forms/README.md`, `src/response.rs`). The searched Formbricks files have none (`packages/types/surveys/elements.ts` has no points or correct-answer fields; no "quiz" in `packages/types` or `modules/survey`).
- Responses become typed business records: the field map is re-checked on every save against the live schema and the saver's rights, and the record is created once after commit (`src/mapping.rs`, `FormsSink`). Formbricks pushes to outside tools (Google Sheets, Notion, Airtable, Slack, webhooks under `settings/workspace/integrations/` in its workspace route folder); whether those check types: not verified.
- Owner-scoped access in the open app: a Forms User sees only the responses of forms they own (`if_owner` roles in `packages/forms/package.yaml`; every analytics query carries the caller's read scope, `src/analytics.rs`). In Formbricks teams and roles live under `apps/web/modules/ee/teams` and `role-management` (Enterprise licence).
- Personal-data export and erasure of respondents is declared by the package (`package.yaml` `personal_data`); an agent or a person can also enter a response by hand under the same checks (`record_response` in `forms.yaml`).

## Licence notes

- Formbricks root `LICENSE`: AGPL-3.0 by default; `packages/surveys`, `packages/survey-ui` and `packages/js-core` are MIT (their `package.json`); `apps/web/modules/ee/**` is under the Formbricks Enterprise licence (`apps/web/modules/ee/LICENSE`), usable in production only with a subscription. `referance/INDEX.md` shows NOASSERTION for this mix.
- Not portable: quotas, AI translation, contacts and segments, teams and roles, SSO, two-factor sign-in, white-label (all under `modules/ee`). Ferrum must write its own.
- Editor, logic, results and share code sits outside `ee` and is AGPL-3.0, which Ferrum's AGPL-3.0-only can absorb with notices. This plan reimplements from behaviour instead. The MIT renderer (`packages/surveys`) uses blocks and elements, not Ferrum's flat question list, so reuse would need a converter; keep its notice if ever copied.
- Multi-language code is outside `ee`, but whether the product gates it by licence key: not verified. Reference strings, images and the Formbricks name were not checked.
- Ferrum Forms is AGPL-3.0-only (`ferrum-app-forms/Cargo.toml`, `LICENSE`). Nothing here is pasted from the references.
