# A browser on a running server, 7 November 2026: definitions built with Studio's builder

The 6 November walk of the governed datasets (FERRUM-ECOSYSTEM-010) had to type a dataset's
definition as a Studio query in JSON. A field may now say how forms edit it (`widget`, Ferrum
PR 91). Metrics 0.3.2 marks both definitions `studio_query`, and Ferrum-Frontend PR 45 shows
Studio's report builder there.

The walk had two parts, both on the walk server's database (`ferrum_walk`), in workspace walk,
signed in as its administrator:
1. Before PR 91 was on the server: Ferrum main 138554d with the shell from PR 45's branch, the
   field's `widget` set in the page's cached metadata, since that server did not send it yet.
2. On this PR's build (691ecb1, the shell from Ferrum-Frontend a64416a, Metrics 0.3.2), with
   nothing set by hand.

## What was walked

| Step | What it showed |
|---|---|
| Studio not installed | Studio's endpoints answered 404 ("package studio not found"): they serve only a workspace that has Studio, and none of the walk server's did. PR 45 meets this by writing the query as JSON, saying that it is built here once Studio is installed. Studio was installed from the App Manager's store for the rest of the walk |
| A dataset built | New dataset: Customer chosen (its list's columns came first, with their rows beside them), then Summary, grouped by Customer Group › Group Name, counted. Beside it: (not set) 11, Individual 1, 12 in all, the same as the 6 November walk's JSON |
| Saved | "Customers by group, built", domain Sales, a draft: its definition `{"entity": "Customer", "group_by": [{"field": "customer_group.group_name"}], "measures": [{"fn": "count"}]}`, its lineage worked out as before: "Customer; CustomerGroup (by customer_group): group_name" |
| Reopened | In the builder as saved, Save disabled until something changed. "Write it as JSON" showed the same query, and "Build it instead" went back |
| On this PR's build | `/api/meta/Dataset` and `/api/meta/Metric` give their definitions `widget: studio_query`, and the forms open on the builder with nothing set by hand |
| A metric | New metric on Customer, Summary, Count: 12 beside it. With a second measure added, Create was refused under the field: "a metric is one measure, ungrouped". With it removed, "Customers, built" was saved: `{"entity": "Customer", "measures": [{"fn": "count"}]}`, uses Customer |

## What it found, and what changed

| Finding | Now |
|---|---|
| Without Studio installed, the builder had nothing to offer: its list of what to report on came back empty | PR 45 (its second commit): the field is written as JSON there, saying why |
| Before anything was chosen, the preview said "No rows match… widen the period", as if a query had run | PR 45: it says to choose what the query is about |

## What it did not cover

- A steward certifying the built dataset, and a second person reading it: the 6 November walk and
  the suite cover them, and the definition is the same query.
