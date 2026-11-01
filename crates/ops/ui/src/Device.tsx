// A device's life: where it is in it, what can happen next, and what it last said, with how old that
// is. An old value is not the current state, so it is never shown as if it were.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, errorMessage, formatDateTime, relativeTime } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { enrollment, latest, setLifecycle } from "./data";
import { Tabs } from "./Tabs";
import { canEnroll, canRetire, nextSteps } from "./words";

interface DeviceDoc {
  id: string;
  device_name: string;
  device_code: string;
  kind: string;
  lifecycle: string;
  version: number;
}

function Code({ device, onClose }: { device: string; onClose: () => void }) {
  const q = useQuery({ queryKey: ["ops", "code", device], queryFn: () => enrollment(device), staleTime: Infinity, gcTime: 0 });
  return (
    <Dialog open title="Enrollment code" onClose={onClose} footer={<Button onClick={onClose}>Done</Button>}>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : (
        <div className="stack">
          <p className="muted">Give this to the site once. It is shown only now, and works once, until {formatDateTime(q.data.expires_at)}.</p>
          <pre className="ops-json">ferrum-site enroll --code {q.data.code}</pre>
        </div>
      )}
    </Dialog>
  );
}

function Why({ to, label, device, onClose }: { to: string; label: string; device: string; onClose: () => void }) {
  const id = useId();
  const qc = useQueryClient();
  const [reason, setReason] = useState("");
  const move = useMutation({
    mutationFn: () => setLifecycle(device, to, reason),
    onSuccess: () => {
      toast("Done");
      void qc.invalidateQueries({ queryKey: ["ops"] });
      onClose();
    },
    onError: (e) => toast(errorMessage(e), "error"),
  });
  return (
    <Dialog
      open
      title={label}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Keep it as it is</Button>
          <Button variant="primary" disabled={!reason.trim()} loading={move.isPending} onClick={() => move.mutate()}>
            {label}
          </Button>
        </>
      }
    >
      <Field label="Why" htmlFor={`${id}-why`} required>
        <input id={`${id}-why`} className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
      </Field>
    </Dialog>
  );
}

export function DevicePage() {
  const { id = "" } = useParams();
  const q = useQuery({ queryKey: ["ops", "device", id], queryFn: () => api.get<DeviceDoc>(docPath("Device", id)) });
  const metrics = useQuery({ queryKey: ["ops", "latest", id], queryFn: () => latest(id), refetchInterval: 15_000 });
  const [asking, setAsking] = useState<{ to: string; label: string } | null>(null);
  const [code, setCode] = useState(false);
  useShellTitle(q.data?.device_name ?? "Device");
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const d = q.data;
  return (
    <div className="ops-page">
      <Tabs />
      <header className="ops-site-head">
        <div>
          <h2>{d.device_name}</h2>
          <div className="faint">
            {d.kind} · {d.device_code}
          </div>
        </div>
        <Link to={`/e/Device/${d.id}`}>Open the record</Link>
      </header>
      <section className="card card-pad stack">
        <div>
          <h3>Where it is in its life</h3>
          <Badge tone={d.lifecycle === "Active" ? "green" : d.lifecycle === "Quarantined" ? "red" : "neutral"}>{d.lifecycle}</Badge>
        </div>
        {d.lifecycle === "Quarantined" ? <Alert tone="warning">It may still report, and it gets no work. It comes back only by enrolling a new key.</Alert> : null}
        <div className="ops-row-actions">
          {canEnroll(d.lifecycle) ? (
            <Button variant="primary" onClick={() => setCode(true)}>
              Enroll a key
            </Button>
          ) : null}
          {nextSteps(d.lifecycle).map((s) => (
            <Button key={s.to} variant={s.serious ? "default" : "primary"} onClick={() => setAsking(s)}>
              {s.label}
            </Button>
          ))}
          {canRetire(d.lifecycle) ? (
            <Button
              onClick={async () => {
                const ok = await confirmDialog({ title: "Retire this device?", message: "Its keys stop working, for good. The asset it belonged to keeps its history.", confirmLabel: "Retire it", danger: true });
                if (ok) setAsking({ to: "Retired", label: "Retire it" });
              }}
            >
              Retire
            </Button>
          ) : null}
        </div>
      </section>
      <section className="card card-pad">
        <h3>What it last said</h3>
        {metrics.isPending ? (
          <PageLoading />
        ) : metrics.isError ? (
          <ErrorState error={metrics.error} onRetry={() => void metrics.refetch()} />
        ) : metrics.data.length === 0 ? (
          <EmptyState title="Nothing yet" icon="activity" />
        ) : (
          <ul className="ops-facts ops-metrics">
            {metrics.data.map((m) => (
              <li key={m.metric}>
                <strong>{m.metric}</strong> {m.value} <span className={m.age_secs > 600 ? "ops-stale" : "faint"}>{relativeTime(m.at)}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
      {code ? <Code device={d.id} onClose={() => setCode(false)} /> : null}
      {asking ? <Why device={d.id} to={asking.to} label={asking.label} onClose={() => setAsking(null)} /> : null}
    </div>
  );
}
