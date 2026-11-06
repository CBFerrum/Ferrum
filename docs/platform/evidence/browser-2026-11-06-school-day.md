# A browser on a running server, 6 November 2026: a school's day, from admission to marks

Education (FERRUM-INDUSTRY-004) had run only inside its own suite, through the API. This walk took a
school's day through its pages, on the walk server: a debug build of Ferrum PR 89 on one machine,
with its own database (`ferrum_walk`), the shell from Ferrum-Frontend a76295e and Education 0.1.8 as
pinned. The workspace's clock is UTC and the browser's was India's. It ran from 09:52 to 10:02 UTC.

One person, the workspace's administrator, worked as the office, the admissions officer and the teacher.

## What was walked

| Step | What it showed |
|---|---|
| Education installed | From the App Manager's Store. Its home, the school office: "Today's classes (0)", "None today", "Applicants waiting 0", "Fees invoiced, not yet paid 0", with nothing saying what to set up |
| Setting up | Made through the API from the signed-in page: the settings (the company fees are invoiced from), the year 2026-27, Mathematics, Grade 6 with it, a tuition item, an instructor, Room 101 and a letter scale (A from 85%, B from 70%, C from 50%) |
| An applicant | On the Applicants page, opened with the program and year in its address: Lea Novak, with her guardian Mara Novak. APP-2026-00001, Applied |
| Admission | Approve ("Approve: done"), then Admit: STU-2026-00001, Lea Novak, Active, with Mara Novak as her guardian, enrolled in Grade 6 for 2026-27 on 6 November |
| Fees | A fee structure, 1,200 tuition and 150 books, totalled 1,350. On the Fees page, FEE-2026-00001 for Lea took 1,350; Invoice It made a draft sales invoice in ERP to Mara Novak, made a customer from the guardian and kept on the student. It was due 30 November, with a line for each component naming Lea. The fees were Invoiced, and the home counted one |
| The timetable | Groups 6A (Lea) and 6B. Mathematics for 6A, 09:00 to 10:00 in Room 101 with Mr Daniel Weiss. 6B at 09:30 in the same room was refused: "the room has another class then (09:00-10:00)". 6A again at 10:00, back to back, was taken. The home listed today's two classes as "09:00 to 10:00" and "10:00 to 11:00", with nothing saying which classes they were |
| Attendance | Lea present at the first class. Refused: a second attendance for her ("has their attendance for this class already"), and Tom Berg, a student outside 6A ("is not in this class's group"). Each attendance is a record made on its own |
| Marks | Unit test 1 for 6A, out of 50, on the letter scale. Lea's 42 was 84%, a B. Corrected to 43, it was regraded to 86%, an A. 55 was refused: "is between 0 and the assessment's maximum, 50.000000000" |

## What it found, and what changed

| Finding | Now |
|---|---|
| Attendance was one record per student, each made on its own page: a class of 25 meant 25 forms | Education 0.2.0 (ferrum-app-education PR 3): a roll call for each class, opened from today's classes. It lists the class's group, each present unless marked otherwise, and saves the new and changed marks at once |
| Today's classes were listed by their times alone | Each shows its course, group, room and instructor |
| Today was the browser's calendar day, not the workspace's | The workspace's day (`todayIso`) |
| The counts of applicants waiting and fees unpaid were the rows of one page of 200 | Counted |
| An office with nothing set up showed empty lists | It says to set the company, then a year and a program |
| A score's limit read "is between 0 and the assessment's maximum, 50.000000000" | "must be between 0 and the assessment's maximum, 50" |
| Approve and Admit said only "done" | Ferrum PR 89: an action that says nothing is described from its answer, its new status and what it made |

## What it did not cover

- Separate people as admissions officer, office and teacher: the walk ran as the administrator. The suite tests who may do what.
- A rejection with its reason, fees paid through ERP, and a personal-data request. The suite tests each.
