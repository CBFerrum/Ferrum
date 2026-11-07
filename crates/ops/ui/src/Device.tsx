// A device's life: where it is in it, what can happen next, and what it last said, with how old that
// is. An old value is not the current state, so it is never shown as if it were.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link, useParams } from "react-router";
import { api, docPath, formatDateTime, refusal, relativeTime, t, isolate } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, Dialog, EmptyState, ErrorState, Field, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { acceptance, enrollment, latest, recordStep, setLifecycle, startRun, type RunDoc, type StepResult } from "./data";
import { Tabs } from "./Tabs";
import { canEnroll, canRetire, expectation, named, nextSteps, reading, typed } from "./words";

interface DeviceDoc {
  id: string;
  device_name: string;
  device_code: string;
  kind: string;
  lifecycle: string;
  gateway?: string | null;
  version: number;
}

function Code({ device, onClose }: { device: string; onClose: () => void }) {
  const q = useQuery({ queryKey: ["ops", "code", device], queryFn: () => enrollment(device), staleTime: Infinity, gcTime: 0 });
  return (
    <Dialog open title={t("Enrollment code")} onClose={onClose} footer={<Button onClick={onClose}>{t("Done")}</Button>}>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : (
        <div className="stack">
          <p className="muted">{t("Give this to the site once. It is shown only now, and works once, until {value}.", { value: formatDateTime(q.data.expires_at) })}</p>
          <pre className="ops-json" dir="ltr">ferrum-site enroll --code {q.data.code}</pre>
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
      toast(t("Done"));
      void qc.invalidateQueries({ queryKey: ["ops"] });
      onClose();
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title={label}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t("Keep it as it is")}</Button>
          <Button variant="primary" disabled={!reason.trim()} loading={move.isPending} onClick={() => move.mutate()}>
            {isolate(label)}
          </Button>
        </>
      }
    >
      <Field label={t("Why")} htmlFor={`${id}-why`} required>
        <input id={`${id}-why`} className="input" value={reason} onChange={(e) => setReason(e.target.value)} />
      </Field>
    </Dialog>
  );
}

const OUTCOME_TONE = { Pending: "neutral", Passed: "green", Failed: "red" } as const;

/** One step of a running run, as the person on site records it. */
function Record({ run, step }: { run: string; step: StepResult }) {
  const id = useId();
  const qc = useQueryClient();
  const [value, setValue] = useState("");
  const [note, setNote] = useState("");
  const checks = step.kind !== "Manual" && step.expected && Object.keys(step.expected).length > 0;
  const save = useMutation({
    mutationFn: (seen: { value?: unknown; passed?: boolean }) => recordStep(run, step.step_id, { ...seen, note: note.trim() || undefined }),
    onSuccess: (r) => {
      qc.setQueryData(["ops", "run", run], r);
      void qc.invalidateQueries({ queryKey: ["ops", "acceptance"] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <div className="stack">
      {checks ? (
        <Field label={step.kind === "Read" ? t("What it read") : t("What the point reads after the command")} htmlFor={`${id}-v`} required>
          <input id={`${id}-v`} className="input" value={value} onChange={(e) => setValue(e.target.value)} />
        </Field>
      ) : null}
      <Field label={t("Note")} htmlFor={`${id}-n`}>
        <input id={`${id}-n`} className="input" value={note} onChange={(e) => setNote(e.target.value)} />
      </Field>
      <div className="ops-row-actions">
        {checks ? (
          <Button variant="primary" disabled={!value.trim()} loading={save.isPending} onClick={() => save.mutate({ value: typed(value) })}>
            {t("Record it")}
          </Button>
        ) : (
          <>
            <Button variant="primary" loading={save.isPending} onClick={() => save.mutate({ passed: true })}>
              {t("It holds")}
            </Button>
            <Button loading={save.isPending} onClick={() => save.mutate({ passed: false })}>
              {t("It does not")}
            </Button>
          </>
        )}
      </div>
    </div>
  );
}

function Run({ id }: { id: string }) {
  const q = useQuery({ queryKey: ["ops", "run", id], queryFn: () => api.get<RunDoc>(docPath("AcceptanceRun", id)) });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const next = q.data.status === "Running" ? q.data.results.find((r) => r.outcome === "Pending") : undefined;
  return (
    <ol className="stack">
      {q.data.results.map((r) => (
        <li key={r.id}>
          <div>
            <strong>{isolate(r.step_id)}</strong> <span className="faint">{named(r.kind)}</span> <Badge tone={OUTCOME_TONE[r.outcome]}>{named(r.outcome)}</Badge>
          </div>
          <div className="faint">
            {isolate(expectation(r.expected))}
            {r.observed !== null && r.observed !== undefined ? t(" · seen {value}", { value: JSON.stringify(r.observed) }) : ""}
            {r.recorded_by ? ` · ${r.recorded_by}` : ""}
            {r.note ? ` · ${r.note}` : ""}
          </div>
          {next?.id === r.id ? <Record run={id} step={r} /> : null}
        </li>
      ))}
    </ol>
  );
}

/** The procedures an app's solution has for this device's profile: each passed on site before commissioning. */
function Acceptance({ device }: { device: string }) {
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["ops", "acceptance", device], queryFn: () => acceptance(device) });
  const start = useMutation({
    mutationFn: (procedure: string) => startRun(device, procedure),
    onSuccess: (r) => {
      qc.setQueryData(["ops", "run", r.id], r);
      void qc.invalidateQueries({ queryKey: ["ops", "acceptance", device] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  if (q.isPending || q.isError || q.data.procedures.length === 0) return null;
  return (
    <section className="card card-pad stack">
      <h3>{t("Acceptance on site")}</h3>
      <p className="muted">{t("Commissioning waits for a passed run of each. A pass records what was checked here, and certifies nothing beyond it.")}</p>
      {q.data.procedures.map((p) => {
        const running = q.data.runs.find((r) => r.procedure === p.id && r.status === "Running");
        const last = q.data.runs.find((r) => r.procedure === p.id && r.status !== "Running");
        return (
          <div key={p.id} className="stack">
            <div className="ops-row-actions">
              <strong>{isolate(p.name)}</strong>
              <Badge tone={p.passed ? "green" : "amber"}>{p.passed ? t("Passed") : t("Not passed yet")}</Badge>
              {last && last.status === "Failed" ? <span className="faint">{t("Last run failed {value}", { value: relativeTime(last.finished_at ?? last.created_at) })}</span> : null}
              {running ? null : (
                <Button loading={start.isPending && start.variables === p.id} onClick={() => start.mutate(p.id)}>
                  {p.passed ? t("Run it again") : t("Start a run")}
                </Button>
              )}
            </div>
            {running ? <Run id={running.id} /> : null}
          </div>
        );
      })}
    </section>
  );
}

export function DevicePage() {
  const { id = "" } = useParams();
  const q = useQuery({ queryKey: ["ops", "device", id], queryFn: () => api.get<DeviceDoc>(docPath("Device", id)) });
  const metrics = useQuery({ queryKey: ["ops", "latest", id], queryFn: () => latest(id), refetchInterval: 15_000 });
  const [asking, setAsking] = useState<{ to: string; label: string } | null>(null);
  const [code, setCode] = useState(false);
  useShellTitle(q.data?.device_name ?? t("Device"));
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const d = q.data;
  return (
    <div className="ops-page">
      <Tabs />
      <header className="ops-site-head">
        <div>
          <h2>{isolate(d.device_name)}</h2>
          <div className="faint">
            {isolate(d.kind)} · {isolate(d.device_code)}
          </div>
        </div>
        <Link to={`/e/Device/${d.id}`}>{t("Open the record")}</Link>
      </header>
      <section className="card card-pad stack">
        <div>
          <h3>{t("Where it is in its life")}</h3>
          <Badge tone={d.lifecycle === "Active" ? "green" : d.lifecycle === "Quarantined" ? "red" : "neutral"}>{named(d.lifecycle)}</Badge>
        </div>
        {d.lifecycle === "Quarantined" ? <Alert tone="warning">{t("It may still report, and it gets no work. It comes back only by enrolling a new key.")}</Alert> : null}
        <div className="ops-row-actions">
          {canEnroll(d.lifecycle) ? (
            <Button variant="primary" onClick={() => setCode(true)}>
              {t("Enroll a key")}
            </Button>
          ) : null}
          {nextSteps(d.lifecycle, d.gateway).map((s) => (
            <Button key={s.to} variant={s.serious ? "default" : "primary"} onClick={() => setAsking(s)}>
              {isolate(s.label)}
            </Button>
          ))}
          {canRetire(d.lifecycle) ? (
            <Button
              onClick={async () => {
                const ok = await confirmDialog({ title: t("Retire this device?"), message: t("Its keys stop working, for good. The asset it belonged to keeps its history."), confirmLabel: t("Retire it"), danger: true });
                if (ok) setAsking({ to: "Retired", label: t("Retire it") });
              }}
            >
              {t("Retire")}
            </Button>
          ) : null}
        </div>
      </section>
      <section className="card card-pad">
        <h3>{t("What it last said")}</h3>
        {metrics.isPending ? (
          <PageLoading />
        ) : metrics.isError ? (
          <ErrorState error={metrics.error} onRetry={() => void metrics.refetch()} />
        ) : metrics.data.length === 0 ? (
          <EmptyState title={t("Nothing yet")} icon="chart" />
        ) : (
          <ul className="ops-facts ops-metrics">
            {metrics.data.map((m) => (
              <li key={m.metric}>
                <strong>{isolate(m.metric)}</strong> {isolate(reading(m.value))} <span className={m.age_secs > 600 ? "ops-stale" : "faint"}>{isolate(relativeTime(m.at))}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
      <Acceptance device={d.id} />
      {code ? <Code device={d.id} onClose={() => setCode(false)} /> : null}
      {asking ? <Why device={d.id} to={asking.to} label={asking.label} onClose={() => setAsking(null)} /> : null}
    </div>
  );
}
