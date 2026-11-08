# W7: Intelligence (8 November 2026)

Software evidence only, from a new public repository, CBFerrum/ferrum-app-intelligence (AGPL-3.0-only),
created for this workstream as decided on 8 November. Built against Ferrum's main (`2a3c246`) and Metrics
`c2b0b66`, merged as CBFerrum/ferrum-app-intelligence#1 (`3f16218`, Intelligence 0.1.0), and compiled into
the server by default (feature `intelligence`, which needs `metrics`).

No model was called. Questions are understood by the app's rules; a model provider plugs in behind the same
`Provider` trait and is held to the same typed request, and runs against one wait for a key
(BLOCK-MODEL-KEY). Nothing here claims anything about a model's answers.

## What ran

```bash
cargo test                                   # the rules' unit test; evals_it against a real database
cargo clippy --all-targets -- -D warnings; cargo fmt --all -- --check
# in Ferrum, with Intelligence pinned at 3f16218
cargo clippy --locked -p ferrum-server -- -D warnings   # every default feature, intelligence among them
python3 scripts/registry.py check
```

## What the evaluations showed (`tests/evals_it.rs`)

Fixed questions: "What is Company customers?" is answered 3 for a director and 2 for a representative who
made two, each with the plan said first ("with your own rights to the records it counts"), who certified
the metric, its fingerprint, lineage and when it was worked out. "Average company terms" is 60 for the
director and 45 for the representative. "Average company terms by month for the last 6 months" is a series
of six months, the current one partial; "company customers weekly" a weekly one.

Adversarial questions: injected instructions naming no metric are declined with the metrics that can be
asked about, and no record reaches the answer. A question that names a metric and asks to delete every
customer and approve all releases reads the metric only, says the rest was not acted on, and every customer
is still there. Five hundred months is declined before anything runs. "How many customers" is asked back
between Customers (companies) and Customers (people). A metric in draft is not there to answer from; one
deprecated since is never answered or offered; a composed metric whose part went declines with the reason.
A provider that fails ends in a plain no: nothing is guessed. Someone without a role that reads metrics is
refused, and so are an empty question and one over 500 characters. Each person reads only their own kept
questions, which record what was answered, asked back and declined. A proposal is a draft Solution kept by
a Solution Author, failing its first check until its trigger is chosen; anyone else is refused, and only
the draft exists: nothing was released or set running.

## Limits

- No model: the rules understand questions; model-backed understanding and its evaluations wait for a key.
- One step per question: there is no agent loop, conversation memory, chart rendering of its own, sharing,
  subscription or export in Intelligence yet (Metrics delivers followed metrics).
- Nothing learns from feedback (FLC-AI-007).
