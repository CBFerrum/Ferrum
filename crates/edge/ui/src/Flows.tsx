// The flows on this tenant, and where each stands on its way to running.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useNavigate } from "react-router";
import { refusal } from "@ferrum/client";
import { Badge, Button, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { type FlowDoc, flowList, newFlow } from "./data";
import { arrange, type Flow, STARTS } from "./drawing";
import { Tabs } from "./Tabs";

const EMPTY: Flow = { nodes: [], edges: [] };

function Start({ onClose }: { onClose: () => void }) {
  const id = useId();
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [choice, setChoice] = useState<number | null>(null);
  const make = useMutation({
    mutationFn: () => newFlow(name.trim(), JSON.stringify(choice === null ? EMPTY : arrange(STARTS[choice]!.flow))),
    onSuccess: (made) => navigate(`/flows/${made.id}`),
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title="A new flow"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={!name.trim()} loading={make.isPending} onClick={() => make.mutate()}>
            Start drawing
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="Name" htmlFor={`${id}-n`} required>
          <input id={`${id}-n`} className="input" value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        <fieldset className="edge-templates">
          <legend>Begin with</legend>
          <label className={choice === null ? "edge-template edge-on" : "edge-template"}>
            <input type="radio" name={`${id}-s`} checked={choice === null} onChange={() => setChoice(null)} />
            <span>
              <strong>An empty canvas</strong>
            </span>
          </label>
          {STARTS.map((s, i) => (
            <label key={s.name} className={choice === i ? "edge-template edge-on" : "edge-template"}>
              <input
                type="radio"
                name={`${id}-s`}
                checked={choice === i}
                onChange={() => {
                  setChoice(i);
                  setName((n) => n || s.name);
                }}
              />
              <span>
                <strong>{s.name}</strong>
                <span className="faint">{s.says}</span>
              </span>
            </label>
          ))}
        </fieldset>
      </div>
    </Dialog>
  );
}

function Stage({ f }: { f: FlowDoc }) {
  if (f.live) return <Badge tone="green">{f.status === "Draft" ? "Running, and being changed" : "Running"}</Badge>;
  return <Badge tone={f.status === "Approved" ? "blue" : f.status === "In Review" ? "amber" : "neutral"}>{f.status}</Badge>;
}

export function Flows() {
  useShellTitle("Flows");
  const q = useQuery({ queryKey: ["edge", "flows"], queryFn: flowList });
  const [starting, setStarting] = useState(false);
  return (
    <div className="edge-page">
      <Tabs />
      <div className="edge-bar">
        <Button variant="primary" onClick={() => setStarting(true)}>
          New flow
        </Button>
      </div>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : q.data.rows.length === 0 ? (
        <EmptyState title="No flows yet" icon="timeline">
          <p className="muted">A flow is logic you draw, which runs at the gateway with or without the link. It is simulated and reviewed before it runs, and the strongest thing it can do is ask for a command.</p>
        </EmptyState>
      ) : (
        <ul className="card edge-rows edge-rows-pad">
          {q.data.rows.map((f) => (
            <li key={f.id}>
              <Link to={`/flows/${f.id}`} className="edge-row-main">
                <strong>{f.flow_name}</strong>
                <div className="faint">
                  {f.site ? (f._titles?.site ?? "One site") : "Every site"}
                  {f.revision ? `, version ${f.revision}` : ""}
                </div>
              </Link>
              <Stage f={f} />
            </li>
          ))}
        </ul>
      )}
      {starting ? <Start onClose={() => setStarting(false)} /> : null}
    </div>
  );
}
