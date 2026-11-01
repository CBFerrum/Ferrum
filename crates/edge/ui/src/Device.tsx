// One device: its newest values, what it reports about itself, what is wrong with it and what it
// said lately. Asking for a value is a command like any other, so the screen shows it as asked for
// until the device says it is so.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { refusal, relativeTime } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { type Attribute, desire, device, newToken } from "./data";
import { Acknowledge } from "./Alarms";
import { Tabs } from "./Tabs";

const shown = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));

function Ask({ id, attribute, onClose }: { id: string; attribute: Attribute | null; onClose: () => void }) {
  const form = useId();
  const qc = useQueryClient();
  const [key, setKey] = useState(attribute?.key ?? "");
  const [value, setValue] = useState(attribute ? shown(attribute.desired ?? attribute.value) : "");
  const ask = useMutation({
    mutationFn: () => {
      let parsed: unknown = value;
      try {
        parsed = JSON.parse(value);
      } catch {
        // Words stay words.
      }
      return desire(id, key.trim(), parsed);
    },
    onSuccess: () => {
      toast("Asked. It shows as out of step until the device says it is so.");
      void qc.invalidateQueries({ queryKey: ["edge", "device", id] });
      onClose();
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title={attribute ? `Change ${attribute.key}` : "Ask for a value"}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Back</Button>
          <Button variant="primary" disabled={!key.trim()} loading={ask.isPending} onClick={() => ask.mutate()}>
            Ask for it
          </Button>
        </>
      }
    >
      <div className="stack">
        <p className="muted">This goes through the ledger as a command, so the device must have been commissioned for it.</p>
        {attribute ? null : (
          <Field label="Attribute" htmlFor={`${form}-k`} required>
            <input id={`${form}-k`} className="input" value={key} onChange={(e) => setKey(e.target.value)} />
          </Field>
        )}
        <Field label="Value" htmlFor={`${form}-v`} required>
          <input id={`${form}-v`} className="input" value={value} onChange={(e) => setValue(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

function Token({ id, onClose }: { id: string; onClose: () => void }) {
  const q = useQuery({ queryKey: ["edge", "token", id], queryFn: () => newToken(id), staleTime: Infinity, gcTime: 0 });
  return (
    <Dialog open title="A token for this device" onClose={onClose} footer={<Button onClick={onClose}>Done</Button>}>
      <div className="stack">
        {q.isPending ? (
          <PageLoading />
        ) : q.isError ? (
          <ErrorState error={q.error} />
        ) : (
          <>
            <Alert tone="warning">This is the only time it is shown, and it replaces the one before.</Alert>
            <code className="edge-token">{q.data.token}</code>
            <p className="muted">For a device with no gateway: it posts to this tenant's ingest address with this in the header x-ferrum-device-token.</p>
          </>
        )}
      </div>
    </Dialog>
  );
}

export function DevicePage() {
  const { id } = useParams() as { id: string };
  useShellTitle("Device");
  const q = useQuery({ queryKey: ["edge", "device", id], queryFn: () => device(id), refetchInterval: 10_000 });
  const [asking, setAsking] = useState<Attribute | "new" | null>(null);
  const [token, setToken] = useState(false);
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const d = q.data;
  const metrics = Array.isArray(d.latest) ? d.latest : (d.latest.metrics ?? []);
  return (
    <div className="edge-page">
      <Tabs />
      <div className="edge-bar">
        <Link className="btn btn-default" to={`/e/Device/${id}`}>
          Open the record
        </Link>
        <Button onClick={() => setAsking("new")}>Ask for a value…</Button>
        <Button onClick={() => setToken(true)}>New token…</Button>
      </div>
      {d.alarms.length ? (
        <section className="card card-pad stack" aria-label="Open alarms">
          <h2 className="edge-section">Alarms</h2>
          <ul className="edge-rows">
            {d.alarms.map((a) => (
              <li key={a.id}>
                <div className="edge-row-main">
                  <strong>{a.summary}</strong>
                  <div className="faint">Raised {relativeTime(a.raised_at)}</div>
                </div>
                <Badge tone={a.severity === "Critical" ? "red" : "amber"}>{a.severity}</Badge>
                {a.status === "Active" ? <Acknowledge id={a.id} /> : <Badge>{a.status}</Badge>}
              </li>
            ))}
          </ul>
        </section>
      ) : null}
      <section className="card card-pad stack" aria-label="Newest values">
        <h2 className="edge-section">Newest values</h2>
        {metrics.length === 0 ? (
          <EmptyState title="Nothing reported yet" icon="chart" />
        ) : (
          <dl className="edge-values">
            {metrics.map((m) => (
              <div key={m.metric}>
                <dt>{m.metric}</dt>
                <dd>{m.value.toLocaleString(undefined, { maximumFractionDigits: 3 })}</dd>
                <dd className="faint">{relativeTime(m.at)}</dd>
              </div>
            ))}
          </dl>
        )}
      </section>
      <section className="card card-pad stack" aria-label="Attributes">
        <h2 className="edge-section">What it says about itself</h2>
        {d.attributes.length === 0 ? (
          <p className="muted">Nothing yet.</p>
        ) : (
          <ul className="edge-rows">
            {d.attributes.map((a) => (
              <li key={a.key}>
                <div className="edge-row-main">
                  <strong>{a.key}</strong>
                  <div className="faint">
                    {shown(a.value)}
                    {a.in_step === false ? `, asked for ${shown(a.desired)}` : ""}
                  </div>
                </div>
                {a.in_step === false ? <Badge tone="amber">Out of step</Badge> : null}
                <Button size="sm" onClick={() => setAsking(a)}>
                  Change…
                </Button>
              </li>
            ))}
          </ul>
        )}
      </section>
      <section className="card card-pad stack" aria-label="Recent events">
        <h2 className="edge-section">What it said lately</h2>
        {d.events.length === 0 ? (
          <p className="muted">No events.</p>
        ) : (
          <ul className="edge-rows">
            {d.events.map((e, i) => (
              <li key={`${e.at}-${i}`}>
                <div className="edge-row-main">
                  <strong>{e.name}</strong>
                  <div className="faint">{shown(e.value)}</div>
                </div>
                <span className="faint">{relativeTime(e.at)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
      {asking ? <Ask id={id} attribute={asking === "new" ? null : asking} onClose={() => setAsking(null)} /> : null}
      {token ? <Token id={id} onClose={() => setToken(false)} /> : null}
    </div>
  );
}
