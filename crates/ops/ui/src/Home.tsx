// Every site at a glance. What matters first: can we see it, is it working, and does anything need
// a person. A silent gateway may be working perfectly well offline, so the screen says what it does
// not know rather than guessing.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link } from "react-router";
import { errorMessage, relativeTime } from "@ferrum/client";
import { Alert, Badge, Button, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { LinkInput } from "@ferrum/records";
import { type Domain, overview, type SiteRow, transfer } from "./data";
import { Tabs } from "./Tabs";
import { silence } from "./words";

const megabytes = (bytes?: number) => (bytes === undefined ? null : `${Math.round(bytes / (1 << 20))} MB free`);

function Transfer({ domain, onClose }: { domain: Domain; onClose: () => void }) {
  const id = useId();
  const qc = useQueryClient();
  const [to, setTo] = useState<string | null>(null);
  const [reason, setReason] = useState("");
  const move = useMutation({
    mutationFn: () => transfer(domain.domain, to!, reason),
    onSuccess: () => {
      toast("Moved. The old gateway has been told to stop.");
      void qc.invalidateQueries({ queryKey: ["ops"] });
      onClose();
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  return (
    <Dialog
      open
      title="Move this site to another gateway"
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Keep it as it is</Button>
          <Button variant="primary" disabled={!to || !reason.trim()} loading={move.isPending} onClick={() => move.mutate()}>
            Move it
          </Button>
        </>
      }
    >
      <div className="stack">
        <Alert tone="warning">The old gateway stops being believed at once. Anything it may have started becomes “Nobody knows”, and someone has to look at the equipment. Do this only when the old gateway is really gone.</Alert>
        <Field label="New gateway" htmlFor={`${id}-to`} required>
          <LinkInput to="Device" id={`${id}-to`} value={to} required linkFilters={{ kind: "Gateway" }} onChange={setTo} />
        </Field>
        <Field label="Why" htmlFor={`${id}-why`} required>
          <input id={`${id}-why`} className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

function DomainRow({ d }: { d: Domain }) {
  const [moving, setMoving] = useState(false);
  const seen = silence(d.silent_secs);
  const h = d.health;
  return (
    <div className="ops-domain">
      <div className="ops-domain-head">
        <div>
          <strong>{d.holder_name ?? "No gateway"}</strong> <Badge tone={seen.tone}>{d.holder ? seen.label : "Nobody holds this"}</Badge>
          <div className="faint">{d.last_seen_at ? `Last heard ${relativeTime(d.last_seen_at)}` : "Not heard from yet"}</div>
        </div>
        <Button size="sm" onClick={() => setMoving(true)}>
          Move…
        </Button>
      </div>
      {h?.stopped ? <Alert tone="error">{h.stopped}</Alert> : null}
      <ul className="ops-facts">
        {h?.version ? <li>Runtime {h.version}</li> : null}
        {h?.disk_free_bytes ? <li>{megabytes(h.disk_free_bytes)}</li> : null}
        {h?.telemetry_shed ? <li>{h.telemetry_shed} measurements given up</li> : null}
        {Object.entries(h?.adapters ?? {}).map(([name, state]) => (
          <li key={name}>
            {name}: {state}
          </li>
        ))}
      </ul>
      {moving ? <Transfer domain={d} onClose={() => setMoving(false)} /> : null}
    </div>
  );
}

function Site({ s }: { s: SiteRow }) {
  return (
    <section className="card card-pad ops-site" aria-label={s.name}>
      <header className="ops-site-head">
        <div>
          <h2>{s.name}</h2>
          <div className="faint">Can run alone for {s.max_offline_hours} h</div>
        </div>
        {s.needs_person ? (
          <Link to="/attention" className="ops-need">
            {s.needs_person} {s.needs_person === 1 ? "needs" : "need"} a person
          </Link>
        ) : null}
      </header>
      <dl className="ops-counts">
        <div>
          <dt>Waiting</dt>
          <dd>{s.waiting}</dd>
        </div>
        <div>
          <dt>Running</dt>
          <dd>{s.running}</dd>
        </div>
      </dl>
      {s.domains.length === 0 ? <p className="muted">No gateway has enrolled here yet.</p> : s.domains.map((d) => <DomainRow key={d.domain} d={d} />)}
      {s.devices ? (
        <div className="ops-life">
          {Object.entries(s.devices).map(([state, n]) => (
            <Badge key={state} tone={state === "Active" ? "green" : state === "Quarantined" ? "red" : "neutral"}>
              {n} {state.toLowerCase()}
            </Badge>
          ))}
        </div>
      ) : null}
    </section>
  );
}

export function Home() {
  useShellTitle("Operations");
  const q = useQuery({ queryKey: ["ops", "overview"], queryFn: overview, refetchInterval: 15_000 });
  const sites = q.data?.sites ?? [];
  const attention = sites.reduce((n, s) => n + s.needs_person, 0);
  return (
    <div className="ops-page">
      <Tabs attention={attention} />
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : sites.length === 0 ? (
        <EmptyState title="No sites yet" icon="factory">
          <Link to="/e/Site/new">Add your first site</Link>
        </EmptyState>
      ) : (
        <div className="ops-sites">
          {sites.map((s) => (
            <Site key={s.id} s={s} />
          ))}
        </div>
      )}
    </div>
  );
}
