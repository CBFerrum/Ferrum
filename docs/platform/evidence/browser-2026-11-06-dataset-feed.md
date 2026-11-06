# A browser on a running server, 6 November 2026: a governed dataset and the warehouse feed

Metrics' datasets and its warehouse feed (FERRUM-ECOSYSTEM-010) had run only inside its suite.
This walk took them on the walk server: Ferrum PR 89 at 66e3097, with its own database
(`ferrum_walk`), the shell from Ferrum-Frontend a76295e and Metrics 0.3.1 as pinned, at 21:05 UTC.
Metrics was installed in an earlier walk, with one metric certified then (Open IT incidents).

One person, the workspace's administrator, worked as the dataset's author. The feed was read with
the signed-in session rather than a warehouse's API key.

## What was walked

| Step | What it showed |
|---|---|
| The page | /kpis/: "Certified metrics", with Open IT incidents at 2 incidents, and "Certified datasets: No dataset is certified yet", with how a warehouse loads them |
| A dataset | On the New dataset page: "Customers by group", domain Sales, and its definition typed as a Studio query in JSON (customers grouped by their group's name, counted). Saved as a draft owned by the administrator, its lineage worked out: "Customer; CustomerGroup (by customer_group): group_name" |
| Its feed, as its owner | `/api/app/metrics/dataset?name=Customers by group&format=csv`: a file named Customers_by_group.csv, marked `x-ferrum-dataset-hash: draft`, with the columns `customer_group.group_name,count` and two rows: 11 customers without a group and 1 Individual. As `ndjson`, the same rows as JSON lines |
| Certifying it | Refused to its owner: "a dataset is certified by someone other than its owner" |
| The values feed | `/api/app/metrics/values?since=2026-11-01T00:00:00Z`: the certified metric's kept value from 4 November as a JSON line, with its definition's hash, and `x-ferrum-next-since` at that time |

## What it found

| Finding | Now |
|---|---|
| A dataset's definition, like a metric's, is typed as a Studio query in JSON on its page: there is no builder, so a person must know the query's shape | Recorded as a limit of FERRUM-ECOSYSTEM-010. Studio's builders are its charts' own and take no other record's field |
| The feed's columns are the definition's field paths (`customer_group.group_name`, `count`), not the labels it gives them ("Group", "Customers") | As designed: a warehouse loads stable names, and the labels are the page's |

## What it did not cover

- A steward certifying the dataset, a second person loading it with their own rights, and a key
  for a warehouse: the walk had one person. The suite tests each, including the rows each
  person's rights give.
