# A browser on a running server, 6 November 2026: a nonprofit's year of giving

Nonprofits (FERRUM-INDUSTRY-011) had run only inside Nonprofit's suite, through the API. This walk
took a year's giving through its pages, on the walk server: a debug build of Ferrum PR 89 on one
machine, with its own database (`ferrum_walk`), the shell from Ferrum-Frontend a76295e and Nonprofit
0.1.7 as pinned. The workspace's clock is UTC and the browser's was India's. It ran from 10:04 to
19:58 UTC, with a break between the donation's submission and its cancelling.

One person, the workspace's administrator, worked as the office and its treasurer.

## What was walked

| Step | What it showed |
|---|---|
| Nonprofit installed | From the App Manager's Store. Its home: five figures at 0 (active and lapsed members, given this year, grants awaiting or awarded, volunteers' hours), with nothing saying that donations need the books named first |
| The books | Nonprofit Settings on its page: the company, Primary Bank for donations received and Other Income for their income, each picked from the chart; "Nonprofit Settings created" |
| A member | An annual membership type (60 for 12 months, as an item), and Ilse Brandt as a member, MEM-2026-00001, made through the API from the signed-in page. On the Memberships page, opened with both in its address: MSP-2026-00001 from 6 November, 60.00 to 5 November 2027. Its start date had to be typed: nothing defaulted to today |
| Its invoice | Invoice It: a draft sales invoice in ERP for 60 to Ilse Brandt, made a customer from the member, "Annual membership, 2026-11-06 to 2027-11-05". She stayed an Applicant until it is paid, as designed, and the home counted her nowhere |
| A donation | Harbour Trust, Pia Lund and a restricted Food bank fund through the API. On the Donations page, Pia's 500 to the food bank, dated by hand: DON-2026-00001, a draft. Submitted: JV-2026-00006, Primary Bank debited 500 and Other Income credited; the fund's received and Pia's given both 500 |
| Cancelled | "Cancelling reverses its postings and cannot be undone": the donation and JV-2026-00006 cancelled, the fund and Pia back to 0, "Donation cancelled" |
| A grant | Winter meals, 2,000 asked of Harbour Trust, Submitted. Refused, each saying why: a payment before the award ("is submitted: a grant is paid once awarded") and a payment from Pia ("is another funder's"). Awarded 1,500; Harbour Trust's 1,500, submitted, made it received |
| Volunteers | Jo Park, with a shift of 3 hours approved and 2 not: 3 hours; the second approved: 5; the first deleted: 2 |
| The home | Active members 0, lapsed 0, given in 2026 1,500, grants awaiting or awarded 1, volunteers' hours 2: right for what was made |

## What it found, and what changed

| Finding | Now |
|---|---|
| The home's figures were counted and added over one list each, asked for 1,000 rows. The server pages a list at 500 and says so only through a cursor, so a larger organisation saw short member counts and a short "Given in" | Nonprofit 0.1.8 (ferrum-app-nonprofit PR 2): members counted, donations and hours added over every page. The same cap was in Point of Sale's loyalty balance at the till (Point of Sale 0.4.4, PR 3), where a long-standing customer's points were added up over some entries, and in Objectives' review (0.5.2, PR 6) |
| "Given in" took the browser's year | The workspace's year |
| A member invoiced but not yet paid was in no figure | "Members not yet paid" |
| Before the books were named, nothing said a donation needs them | The home says to set them in Nonprofit Settings first |
| A membership, a donation and a volunteer's shift started with no date | Today, unless another is chosen |

## What it did not cover

- The membership paid, and the member becoming active by upkeep: that needs the invoice submitted and settled in ERP, walked in the order-to-cash journey. The suite tests the upkeep.
- A tax receipt for a donor and a grant's reports: neither has a page of its own beyond the records.
