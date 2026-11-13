# LMS: references

**Compared with:** frappe/lms `cf5a078c2c64` (lms/, frontend/src) against ferrum-app-lms 0.1.0 ("Learn", working tree, `cc098b9`) and the records frame in ferrum (`81b7f7c`). **Date:** 2026-11-01

## What Ferrum has

- 24 entities, 3 roles (LMS Manager, Instructor, Learner), 4 reports: categories, courses with instructors and prerequisites, chapters, lessons (article, video, document, quiz, assignment; free preview; required), learning paths, cohorts with live sessions, enrolments, lesson progress, question bank, quizzes, attempts, assignments, submissions, certificates (`packages/lms/entities/catalog.yaml`, `learning.yaml`, `assessment.yaml`, `packages/lms/package.yaml`, `src/reports.rs`).
- Enrolment rules (published, prerequisites with a cycle check, cohort capacity under a row lock), server-computed progress and completion, one certificate per completed enrolment (`src/enrollment.rs`, `src/catalog.rs`, `src/certificate.rs`).
- Quiz engine: question bank, random draw, shuffles, server deadline with a 15 s grace, attempt cap, exact scoring, answer keys never sent before submit, three reveal modes; assignments with due dates, late and resubmission rules, grading (`src/quiz.rs`, `src/grading.rs`, `src/assignment.rs`). Cohort sessions laid out in the cohort's time zone, reminders sent once (`src/cohort.rs`).
- JSON for UIs: public `/api/site/{tenant}/lms/*` (courses, categories, course page, free-preview lessons, certificate check) and signed-in `/api/app/lms/*` (catalog, course, lesson, dashboard, teaching, roster) in `src/site.rs`. The only HTML is the certificate view (`packages/lms/print/LmsCertificate.html`, `/certificates/{code}/view`).
- In the records frame a learner cannot learn: learners have no permission on lessons, chapters or quizzes (`package.yaml`, test in `src/lib.rs`); no action declares `params`, so `open_lesson`, `complete_lesson`, `start_quiz` and `save_answers` post an empty body (`learning.yaml`, `ferrum/web/src/app/FormView.tsx` runAction); attempt answers are read-only rows. Read from code, not run.

## What the references do that Ferrum does not

Short Frappe paths: `pages/...` and `components/...` start at `frontend/src/`; `doctype/...` at `lms/lms/`.

| Capability | Where in the reference | Ferrum today | Worth building | Size |
|---|---|---|---|---|
| Learner course player: chapter sidebar with progress, tick and lock per lesson, Prev/Next, mark complete, resume | `frontend/src/pages/Lesson.vue`, `components/StudentLessonSidebar.vue`, `components/LessonNavButtons.vue` | JSON only (`/api/app/lms/lessons/{id}`, enrolment `outline`: `src/site.rs`, `src/enrollment.rs`); learners cannot read `LmsLesson` | must | L |
| Quiz player: timer, attempts left, question chips, unattempted warning, result and attempt history | `frontend/src/components/Quiz.vue`, `pages/QuizPage.vue`, `pages/QuizSubmission.vue` | Engine only (`src/quiz.rs`); attempt answers are read-only rows (`LmsAttemptAnswer` in `learning.yaml`) | must | M |
| Course builder: outline with drag reorder of chapters and lessons, inline rename, lesson editor with auto-save | `frontend/src/pages/Courses/CourseEditor.vue`, `components/CourseOutline.vue`, `components/ChapterRow.vue`, `pages/LessonForm.vue`, `components/BlockEditor.vue` | Chapter and Lesson are separate lists ordered by a `position` number; no reorder action (`src/catalog.rs`) | must | L |
| Public catalog and course page: cards, filters, enrol button, outline, reviews | `frontend/src/pages/Courses/Courses.vue`, `CourseDetail.vue`, `components/CourseCard.vue` | JSON only (`/api/site/{tenant}/lms/courses` and `/courses/{slug}`, `src/site.rs`) | must | M |
| Learner home: continue a course, upcoming live classes, my courses | `frontend/src/pages/Home/StudentHome.vue`, `components/ContinueCoursePanel.vue` | JSON ready (`/api/app/lms/dashboard`, `src/site.rs`); no screen | must | S |
| Instructor dashboard: number cards, student table with progress, per-lesson completion, grading queue | `frontend/src/pages/Courses/CourseDashboard.vue`, `pages/AssignmentSubmissions.vue`, `components/Submissions/SubmissionsPage.vue` | JSON (`/teaching`, roster) and 4 reports (`src/reports.rs`); no screen | should | M |
| Lessons that unlock in order | `lms/lms/utils.py` (`compute_locked_lessons`), `frontend/src/components/ChapterRow.vue` | None; `open_lesson` never refuses (`src/enrollment.rs`) | should | S |
| Quiz window, negative marking, proctoring log | `lms/lms/doctype/lms_quiz/lms_quiz.json`, `frontend/src/components/ProctoringMonitor.vue`, `lms/lms/doctype/lms_quiz_violation_log` | None (`LmsQuiz` in `assessment.yaml`) | later (window, negative marking); no (proctoring) | S |
| Lesson notes and discussion | `frontend/src/components/Notes/Notes.vue`, `components/Discussions.vue` | None in the app; records have a Discussion tab (`ferrum/web/src/app/Collab.tsx`) but learners cannot open lessons | later | M |
| Course reviews and ratings | `lms/lms/doctype/lms_course_review`, `frontend/src/components/CourseReviews.vue` | None | later | S |
| Live classes through Zoom or Google Meet, batch timetable | `lms/lms/doctype/lms_live_class`, `lms_zoom_settings`, `lms_google_meet_settings`, `lms_batch_timetable` | Sessions with a pasted `join_url`, reminders once (`LmsCohortSession` in `catalog.yaml`, `src/cohort.rs`) | later (needs provider keys) | M |
| Paid courses, coupons | `lms/lms/doctype/lms_payment`, `lms_coupon`, `lms/lms/payments.py` | Paid courses through the store (Paid Courses connector, `packages/lms_payments`, `src/payments.rs`, Learn 0.3.0); no coupons | coupons later | S |
| SCORM packages | `frontend/src/pages/SCORMChapter.vue` | SCORM 1.2 and 2004 through scorm-again (`src/scorm.rs`, `ui/src/Scorm.tsx`, Learn 0.4.0), also xAPI (`src/xapi.rs`) and LTI 1.3 (`src/lti.rs`, 0.5.0); no SCORM 2004 sequencing | done | — |
| Video watch time and skip guard | `lms/lms/doctype/lms_video_watch_duration`, `frontend/src/pages/Lesson.vue` (`allowSkipUpTo`) | None; `src/video.rs` only checks the host | later | M |
| Programming exercises with a code runner, badges, jobs board, evaluator-led certification | `frontend/src/components/ProgrammingExercises/`, `lms/lms/code_runner.py`, `lms/lms/doctype/lms_badge`, `frontend/src/pages/Jobs.vue`, `lms/lms/doctype/lms_certificate_evaluation` | None | no | - |

## Screens to hand-craft

1. **Learner course player.** Purpose: where learners spend their time; "what next" must be obvious.
   - Layout (Frappe `Lesson.vue`): content on the left, course sidebar on the right (a bottom sheet on phones). Sidebar: progress bar with "Completed N%", chapters as disclosure groups with lesson counts, a tick on finished lessons, the current one highlighted, a lock with the tooltip "Complete the previous lessons to unlock this one" (`StudentLessonSidebar.vue`). Content: title, markdown body, video from an allowed host, document viewer, inline quiz or assignment panel, Prev / Next at the foot, Notes and Discussion tabs.
   - Interactions: Next completes the lesson and moves on in one click (`complete_lesson` is idempotent); resume at `last_lesson`; the bar updates at once (Frappe listens to the socket event `update_lesson_progress`); free-preview lessons open without enrolment and offer Enrol; quiz and assignment lessons complete themselves when passed (server); Left and Right keys.
   - Data: has `outline` (chapters, lessons, completed flags, `next_lesson`), `/api/app/lms/lessons/{id}`, `complete_lesson`, `start_quiz`, submissions. Lacks a lock flag, notes, video position.
2. **Quiz player.** Purpose: sit a timed quiz fairly and read the result.
   - Layout (Frappe `Quiz.vue`): header with title, attempts left and a timer badge that turns amber, then red, near the end (`timerTheme`); a row of question chips (answered or not); one question card (single, multiple, true/false, short answer); Prev / Next / Submit; a result panel with score, percentage, pass or fail, a per-question review when `show_answers` allows, and the attempt history.
   - Interactions: call `save_answers` shortly after each change so a closed tab resumes (`start_quiz` returns `resumed`); count down from the server's `deadline` and `seconds_left`, not the device clock; Submit warns about unanswered questions, as Frappe does; lock the form after submit; auto-submit at zero (the server closes the attempt 15 s later anyway).
   - Data: complete: `QuizState`, attempt rows with options as `[{key, text}]`, `seconds_left`, the `submit_answers` result, `review`. Lacks negative marking, a quiz window, proctoring.
3. **Course builder.** Purpose: let instructors build and reorder content quickly.
   - Layout (Frappe `CourseEditor.vue`): a 70/30 split, lesson editor on the left (an empty state until a lesson is chosen) and the outline on the right (a bottom sheet on phones); top bar with status chip, Preview as learner, Publish.
   - Interactions: drag lessons within and across chapters and drag chapters; inline chapter rename; auto-save about 800 ms after typing and Ctrl+S (`LessonForm.vue`); the lesson type decides the fields (video URL with the allowed-host hint from `src/video.rs`, file upload, quiz or assignment picker with "create new"); "Free preview" and "Required" toggles on the row; a Publish checklist from the server's rules (one lesson at least; quiz lessons need questions, `packages/lms/README.md`).
   - Data: chapters, lessons, `position`, quiz and assignment links exist; Tiptap and `@dnd-kit` are in `ferrum/web/package.json`. Lacks an atomic `reorder` action (moving means several edits of `position`) and a way to ask what blocks publishing without trying.
4. **Instructor dashboard and grading.** Purpose: see who is stuck and clear the queue.
   - Layout (Frappe `CourseDashboard.vue`): number cards (enrolled, average completion, lessons; rating once it exists), a searchable student table with progress bars, a progress distribution ring, completions per lesson; a grading view with the learner's answer or file, score, feedback, outcome (Graded or Needs Revision) and Next.
   - Interactions: sort by last activity; click a learner for lesson-by-lesson progress and quiz attempts; paste emails to enrol (`enroll_learners`, up to 500); grade and jump to the next submission (`grade`).
   - Data: has `/api/app/lms/teaching`, `/teaching/courses/{course}/roster` (quizzes passed, best %, submissions to grade), 4 reports, `grade`, `review`. Lacks per-lesson completion counts (derive from `LmsLessonProgress`) and ratings.
5. **Public catalog, course page, certificate check.** Purpose: discovery and proof. Layout: card grid (cover, level, hours, lessons, certificate mark) with search, category, level and tag filters and "load more"; a course page with hero, Enrol showing the exact blockers (prerequisites, cohort seats), outline with preview lessons, instructors, cohorts with places; a code box that opens `/certificates/{code}/view`. Interactions: server-rendered for search engines (`ferrum/docs/plan/apps-ui.md` section 7.3); blocker text from `enroll_blockers`. Data: all exists as JSON (`src/site.rs`); no ratings or related courses.

## What Ferrum already does better

- Prerequisites with a cycle check and the unmet ones listed when a learner tries to enrol (`src/catalog.rs`, `enroll_blockers` in `src/site.rs`); Frappe has none (no "prerequisite" in `lms/` or `frontend/src` outside translations).
- Certificates: gapless numbers, a 160-bit verification code, revocation with a reason that the public check reports, a restylable print format (`LmsCertificate` in `learning.yaml`, `print/LmsCertificate.html`). Frappe's `lms/lms/doctype/lms_certificate/lms_certificate.json` has `published` and `expiry_date` but no revoked status; the expiry is something Ferrum lacks.
- Exact decisions: the pass test works on decimals (`score x 100 >= pass % x max`, `src/grading.rs`), the server clock has a stated 15 s grace (`src/quiz.rs` `GRACE_SECONDS`), attempt caps and cohort capacity are decided under row locks (`src/enrollment.rs`). Frappe also withholds answers from its quiz payload (`lms/lms/utils.py` `get_quiz_with_questions`), so secrecy is not the difference; how it handles races: not verified.
- Everything a learner, instructor or agent does is a record or an action of the generic API and MCP, with record scopes for instructors (`packages/lms/README.md`), and learners' data is declared for export and erasure (`personal_data` in `packages/lms/package.yaml`).

## Licence notes

- Frappe LMS: AGPL-3.0 text in `license.txt`; `package.json` says AGPL-3.0-or-later and `lms/hooks.py` says `app_license = "AGPL"`. Such code can be adapted into Ferrum's AGPL-3.0-only with notices; this plan reimplements from behaviour instead.
- No enterprise-licensed folder at the top level (`lms/`, `frontend/`, `e2e/`, `docker/`); inside every directory: not verified.
- Front-end dependencies (EditorJS, vuedraggable, frappe-ui and others), the translations in `lms/translations` and the Frappe name and logo were not checked. Payments, SCORM and Zoom or Google Meet carry their own provider terms.
- Ferrum LMS is AGPL-3.0-only (`ferrum-app-lms/Cargo.toml`, `LICENSE`). Nothing here is pasted from the references.
