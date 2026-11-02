// Every gateway at a glance, and what each reaches. A silent gateway may be working perfectly well
// offline, so this says when it last spoke and what it said then, and does not guess at the rest.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useNavigate } from "react-router";
import { refusal, relativeTime } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { claim, type Gateway, type LinkHealth, overview, type Stranger } from "./data";
import { Tabs } from "./Tabs";

const count = (n: number) => n.toLocaleString();

function Connection({ link }: { link: LinkHealth }) {
  return (
    <li className="edge-link">
      <div className="edge-link-head">
        <Badge tone={link.up ? "green" : "red"}>{link.up ? "Connected" : "Not connected"}</Badge>
        <strong>{link.definition}</strong>
      </div>
      <code className="edge-address">{link.address}</code>
      <div className="faint">
        {link.devices} {link.devices === 1 ? "device" : "devices"}, {count(link.messages)} messages
        {link.dropped ? `, ${count(link.dropped)} dropped` : ""}
        {link.reconnects > 1 ? `, ${link.reconnects} reconnects` : ""}
      </div>
      {link.last_error ? <div className="edge-why">{link.last_error}</div> : null}
    </li>
  );
}

function GatewayCard({ g }: { g: Gateway }) {
  const heard = g.silent_secs !== null && g.silent_secs < 120;
  return (
    <section className="card card-pad edge-gateway" aria-label={g.name}>
      <header className="edge-head">
        <div>
          <h2>{g.name}</h2>
          <div className="faint">
            {g.site ?? "No site"}
            {g.runtime ? `, runtime ${g.runtime}` : ""}
          </div>
        </div>
        <Badge tone={heard ? "green" : "amber"}>{g.last_seen_at ? (heard ? "Heard from" : `Quiet since ${relativeTime(g.last_seen_at)}`) : "Not heard from yet"}</Badge>
      </header>
      {g.problems.map((p) => (
        <Alert key={p} tone="warning">
          {p}
        </Alert>
      ))}
      {g.links.length === 0 ? (
        <p className="muted">{g.devices ? `${g.devices} devices assigned. It has not reported its connections yet.` : "No devices are assigned to this gateway."}</p>
      ) : (
        <ul className="edge-links">
          {g.links.map((l) => (
            <Connection key={`${l.address}-${l.definition}`} link={l} />
          ))}
        </ul>
      )}
    </section>
  );
}

function Claim({ stranger, onClose }: { stranger: Stranger; onClose: () => void }) {
  const id = useId();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const [name, setName] = useState(stranger.code);
  const [definition, setDefinition] = useState<string | null>(null);
  const save = useMutation({
    mutationFn: () => claim(stranger.gateway, stranger.code, name.trim(), definition),
    onSuccess: (made) => {
      toast("Added. It goes through commissioning like any other device.");
      void qc.invalidateQueries({ queryKey: ["edge"] });
      navigate(`/d/${made.id}`);
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title={`Add ${stranger.code}`}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Not now</Button>
          <Button variant="primary" disabled={!name.trim()} loading={save.isPending} onClick={() => save.mutate()}>
            Add it
          </Button>
        </>
      }
    >
      <div className="stack">
        <p className="muted">
          {stranger.gateway_name} heard this on a shared connection. Adding it makes a record: nothing is sent to it until it has been commissioned.
        </p>
        <Field label="Name" htmlFor={`${id}-n`} required>
          <input id={`${id}-n`} className="input" value={name} onChange={(e) => setName(e.target.value)} />
        </Field>
        <Field label="Definition" htmlFor={`${id}-d`} hint="How it is read, if not the one it was heard by.">
          <LinkInput to="DeviceDefinition" id={`${id}-d`} value={definition} onChange={setDefinition} />
        </Field>
      </div>
    </Dialog>
  );
}

function Strangers({ list }: { list: Stranger[] }) {
  const [claiming, setClaiming] = useState<Stranger | null>(null);
  if (list.length === 0) return null;
  return (
    <section className="card card-pad stack" aria-label="Devices nobody has added">
      <h2 className="edge-section">Heard, and nobody has added them</h2>
      <ul className="edge-rows">
        {list.map((s) => (
          <li key={`${s.gateway}-${s.code}`}>
            <div className="edge-row-main">
              <strong>{s.code}</strong>
              <div className="faint">
                By {s.gateway_name}, {relativeTime(s.last_seen_at)}
              </div>
            </div>
            <Button size="sm" onClick={() => setClaiming(s)}>
              Add…
            </Button>
          </li>
        ))}
      </ul>
      {claiming ? <Claim stranger={claiming} onClose={() => setClaiming(null)} /> : null}
    </section>
  );
}

export function Home() {
  useShellTitle("IoT Edge");
  const q = useQuery({ queryKey: ["edge", "overview"], queryFn: overview, refetchInterval: 15_000 });
  const raised = Object.values(q.data?.alarms ?? {}).reduce((n, c) => n + c, 0);
  const critical = q.data?.alarms.Critical ?? 0;
  return (
    <div className="edge-page">
      <Tabs alarms={raised} />
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : (
        <>
          {critical ? (
            <Alert tone="error" action={<Link to="/alarms">See them</Link>}>
              {critical} critical {critical === 1 ? "alarm is" : "alarms are"} open.
            </Alert>
          ) : null}
          <Strangers list={q.data.strangers} />
          {q.data.gateways.length === 0 ? (
            <EmptyState title="No gateways yet" icon="dashboard">
              <p className="muted">A gateway is a device of kind Gateway, enrolled in Operations.</p>
              <Link to="/e/Device/new">Add one</Link>
            </EmptyState>
          ) : (
            <div className="edge-cards">
              {q.data.gateways.map((g) => (
                <GatewayCard key={g.id} g={g} />
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}
