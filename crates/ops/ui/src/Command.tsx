// A command's whole story: what was asked, everything said about it, and when each thing happened as
// against when it was heard, because a site that was offline tells its story late.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router";
import { formatDateTime, refusal } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, ErrorState, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { act, command, type HistoryRow } from "./data";
import { Tabs } from "./Tabs";
import { issuer, said, state, STEPS } from "./words";

const shown = (v: unknown) => (v === null || v === undefined ? null : JSON.stringify(v, null, 2));

function Heard({ row }: { row: HistoryRow }) {
  const late = row.at_source ? (new Date(row.received_at).getTime() - new Date(row.at_source).getTime()) / 1000 : 0;
  return (
    <span className="faint">
      {formatDateTime(row.at_source ?? row.received_at)}
      {late > 60 ? ` · heard ${Math.round(late / 60)} min later` : ""}
    </span>
  );
}

export function CommandPage() {
  const { id = "" } = useParams();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["ops", "command", id], queryFn: () => command(id), refetchInterval: 10_000 });
  useShellTitle(q.data?.operation ?? "Command");
  const cancel = useMutation({
    mutationFn: () => act(id, "cancel", { reason: "Stopped from Operations" }),
    onSuccess: (r) => {
      toast(r.state === "Aborted" ? "Stopped" : "Asked to stop. It keeps its state until the site says what happened.");
      void qc.invalidateQueries({ queryKey: ["ops"] });
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  if (q.isPending) return <PageLoading />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const c = q.data;
  const word = state(c.state);
  const finished = ["Refused", "Expired", "Rejected", "Completed", "Failed", "Aborted"].includes(c.state);
  return (
    <div className="ops-page">
      <Tabs />
      <header className="ops-site-head">
        <div>
          <h2>{c.operation}</h2>
          <Badge tone={word.tone}>{word.label}</Badge> <span className="muted">{word.says}</span>
        </div>
        {finished ? null : (
          <Button
            loading={cancel.isPending}
            onClick={async () => {
              const ok = await confirmDialog({ title: "Ask it to stop?", message: "If it has started, this is a request, and it stays as it is until the site says how it ended.", confirmLabel: "Ask it to stop" });
              if (ok) cancel.mutate();
            }}
          >
            Stop
          </Button>
        )}
      </header>
      {c.cancel_requested_at && !finished ? <Alert tone="warning">A stop was requested. Nothing has confirmed it yet.</Alert> : null}
      {c.state === "OutcomeUnknown" || c.conflict ? (
        <Alert tone="error" action={<Link to="/attention">Settle it</Link>}>
          A person has to look at this.
        </Alert>
      ) : null}
      <section className="card card-pad">
        <h3>What was asked</h3>
        <dl className="ops-pairs">
          <dt>Deadline</dt>
          <dd>{formatDateTime(c.expires_at)}</dd>
          <dt>Device</dt>
          <dd>
            <Link to={`/d/${c.target_id}`}>{c.target_name ?? c.target_code ?? "Open the device"}</Link>
          </dd>
          <dt>By</dt>
          <dd>{issuer(c.issued_kind, c.issued_by_email)}</dd>
        </dl>
        <pre className="ops-json">{shown(c.payload)}</pre>
      </section>
      <section className="card card-pad">
        <h3>What happened</h3>
        <ol className="ops-story">
          {c.history.map((h) => (
            <li key={h.seq}>
              <Badge tone={h.step === "move" ? state(h.state).tone : "neutral"}>{state(h.state).label}</Badge> <Heard row={h} />
              {STEPS[h.step] ? <div className="muted">{STEPS[h.step]}</div> : null}
              {shown(said(h.detail)) ? <pre className="ops-json">{shown(said(h.detail))}</pre> : null}
              {shown(h.evidence) ? (
                <>
                  <div className="faint">Evidence</div>
                  <pre className="ops-json">{shown(h.evidence)}</pre>
                </>
              ) : null}
            </li>
          ))}
        </ol>
      </section>
    </div>
  );
}
