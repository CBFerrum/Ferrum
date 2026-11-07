// A command's whole story: what was asked, everything said about it, and when each thing happened as
// against when it was heard, because a site that was offline tells its story late.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router";
import { formatDateTime, refusal, t, isolate } from "@ferrum/client";
import { Alert, Badge, Button, confirmDialog, ErrorState, PageLoading, toast, useShellTitle } from "@ferrum/kit";
import { act, command, type HistoryRow } from "./data";
import { Tabs } from "./Tabs";
import { issuer, said, state, STEPS } from "./words";

const shown = (v: unknown) => (v === null || v === undefined ? null : JSON.stringify(v, null, 2));

function Heard({ row }: { row: HistoryRow }) {
  const late = row.at_source ? (new Date(row.received_at).getTime() - new Date(row.at_source).getTime()) / 1000 : 0;
  return (
    <span className="faint">
      {isolate(formatDateTime(row.at_source ?? row.received_at))}
      {late > 60 ? t(" · heard {minutes} min later", { minutes: Math.round(late / 60) }) : ""}
    </span>
  );
}

export function CommandPage() {
  const { id = "" } = useParams();
  const qc = useQueryClient();
  const q = useQuery({ queryKey: ["ops", "command", id], queryFn: () => command(id), refetchInterval: 10_000 });
  useShellTitle(q.data?.operation ?? t("Command"));
  const cancel = useMutation({
    mutationFn: () => act(id, "cancel", { reason: "Stopped from Operations" }),
    onSuccess: (r) => {
      toast(r.state === "Aborted" ? t("Stopped") : t("Asked to stop. It keeps its state until the site says what happened."));
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
          <h2>{isolate(c.operation)}</h2>
          <Badge tone={word.tone}>{isolate(word.label)}</Badge> <span className="muted">{isolate(word.says)}</span>
        </div>
        {finished ? null : (
          <Button
            loading={cancel.isPending}
            onClick={async () => {
              const ok = await confirmDialog({ title: t("Ask it to stop?"), message: t("If it has started, this is a request, and it stays as it is until the site says how it ended."), confirmLabel: t("Ask it to stop") });
              if (ok) cancel.mutate();
            }}
          >
            {t("Stop")}
          </Button>
        )}
      </header>
      {c.cancel_requested_at && !finished ? <Alert tone="warning">{t("A stop was requested. Nothing has confirmed it yet.")}</Alert> : null}
      {c.state === "OutcomeUnknown" || c.conflict ? (
        <Alert tone="error" action={<Link to="/attention">{t("Settle it")}</Link>}>
          {t("A person has to look at this.")}
        </Alert>
      ) : null}
      <section className="card card-pad">
        <h3>{t("What was asked")}</h3>
        <dl className="ops-pairs">
          <dt>{t("Deadline")}</dt>
          <dd>{isolate(formatDateTime(c.expires_at))}</dd>
          <dt>{t("Device")}</dt>
          <dd>
            <Link to={`/d/${c.target_id}`}>{c.target_name ?? c.target_code ?? t("Open the device")}</Link>
          </dd>
          <dt>{t("By")}</dt>
          <dd>{isolate(issuer(c.issued_kind, c.issued_by_email))}</dd>
        </dl>
        <pre className="ops-json">{isolate(shown(c.payload) ?? "")}</pre>
      </section>
      <section className="card card-pad">
        <h3>{t("What happened")}</h3>
        <ol className="ops-story">
          {c.history.map((h) => (
            <li key={h.seq}>
              <Badge tone={h.step === "move" ? state(h.state).tone : "neutral"}>{isolate(state(h.state).label)}</Badge> <Heard row={h} />
              {STEPS[h.step] ? <div className="muted">{STEPS[h.step]}</div> : null}
              {shown(said(h.detail)) ? <pre className="ops-json">{isolate(shown(said(h.detail)) ?? "")}</pre> : null}
              {shown(h.evidence) ? (
                <>
                  <div className="faint">{t("Evidence")}</div>
                  <pre className="ops-json">{isolate(shown(h.evidence) ?? "")}</pre>
                </>
              ) : null}
            </li>
          ))}
        </ol>
      </section>
    </div>
  );
}
