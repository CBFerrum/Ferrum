// What only a person can settle: work waiting for approval, commands nobody can account for, and
// commands that ended two ways. Nothing here retries anything: a person looks, then says.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { Link } from "react-router";
import { formatDateTime, refusal, t, isolate } from "@ferrum/client";
import { CheckIcon } from "lucide-react";
import { Badge, Button, Dialog, Field, LoadFailed, PageLoading, StatePanel, toast } from "@ferrum/kit";
import { act, type CommandRow, commands } from "./data";
import { Screen } from "./Tabs";
import { state } from "./words";

const ENDINGS = [
  { to: "Completed", get label() { return t("It finished"); } },
  { to: "Failed", get label() { return t("It did not finish"); } },
  { to: "Aborted", get label() { return t("It was stopped"); } },
];

function Settle({ c, onClose }: { c: CommandRow; onClose: () => void }) {
  const id = useId();
  const qc = useQueryClient();
  const [to, setTo] = useState("");
  const [evidence, setEvidence] = useState("");
  const settle = useMutation({
    mutationFn: () => act(c.id, "reconcile", { to, evidence }),
    onSuccess: () => {
      toast(t("Recorded"));
      void qc.invalidateQueries({ queryKey: ["ops"] });
      onClose();
    },
    onError: (e) => toast(refusal(e), "error"),
  });
  return (
    <Dialog
      open
      title={t("What did you find?")}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>{t("Not yet")}</Button>
          <Button variant="primary" disabled={!to || !evidence.trim()} loading={settle.isPending} onClick={() => settle.mutate()}>
            {t("Record it")}
          </Button>
        </>
      }
    >
      <div className="stack">
        <p className="muted">{t("Look at the equipment first. If it needs doing again, that is a new command: this only says what happened.")}</p>
        <fieldset className="ops-choices">
          <legend>{t("How it ended")}</legend>
          {ENDINGS.map((e) => (
            <label key={e.to}>
              <input type="radio" name={`${id}-to`} checked={to === e.to} onChange={() => setTo(e.to)} /> {isolate(e.label)}
            </label>
          ))}
        </fieldset>
        <Field label={t("What you found")} htmlFor={`${id}-ev`} required>
          <textarea id={`${id}-ev`} className="textarea" rows={3} value={evidence} onChange={(e) => setEvidence(e.target.value)} />
        </Field>
      </div>
    </Dialog>
  );
}

function Row({ c }: { c: CommandRow }) {
  const qc = useQueryClient();
  const [settling, setSettling] = useState(false);
  const word = state(c.state);
  const done = () => void qc.invalidateQueries({ queryKey: ["ops"] });
  const approve = useMutation({ mutationFn: () => act(c.id, "approve"), onSuccess: done, onError: (e) => toast(refusal(e), "error") });
  const refuse = useMutation({ mutationFn: () => act(c.id, "refuse", { reason: "Refused" }), onSuccess: done, onError: (e) => toast(refusal(e), "error") });
  return (
    <li className="ops-row">
      <div className="ops-row-body">
        <Link to={`/c/${c.id}`}>{isolate(c.operation)}</Link> <Badge tone={c.conflict ? "red" : word.tone}>{c.conflict ? t("Two endings") : word.label}</Badge>
        {c.issued_kind === "ai" ? <Badge tone="purple">{t("Proposed by AI")}</Badge> : null}
        <div className="faint">
          {c.target_code ? `${c.target_code} · ` : ""}
          {isolate(formatDateTime(c.issued_at))} · {isolate(c.domain)}
        </div>
        <p className="muted">{c.conflict ? t("It reported two different endings. The first stands until someone looks.") : word.says}</p>
        {c.correlation_id ? <p className="muted">{isolate(c.correlation_id)}</p> : null}
      </div>
      <div className="ops-row-actions">
        {c.state === "Proposed" ? (
          <>
            <Button variant="primary" loading={approve.isPending} onClick={() => approve.mutate()}>
              {t("Approve")}
            </Button>
            <Button loading={refuse.isPending} onClick={() => refuse.mutate()}>
              {t("Refuse")}
            </Button>
          </>
        ) : (
          <Button variant="primary" onClick={() => setSettling(true)}>
            {t("Say what happened")}
          </Button>
        )}
      </div>
      {settling ? <Settle c={c} onClose={() => setSettling(false)} /> : null}
    </li>
  );
}

export function Attention() {
  const q = useQuery({ queryKey: ["ops", "attention"], queryFn: () => commands({ needs_person: 1, limit: 200 }), refetchInterval: 15_000 });
  return (
    <Screen section="attention" attention={q.data?.length}>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <LoadFailed error={q.error} onRetry={() => void q.refetch()} />
      ) : q.data.length === 0 ? (
        <StatePanel kind="empty" icon={CheckIcon} title={t("Nothing needs you")}>
          {t("Everything on the floor is accounted for.")}
        </StatePanel>
      ) : (
        <ul className="ops-list card">
          {q.data.map((c) => (
            <Row key={c.id} c={c} />
          ))}
        </ul>
      )}
    </Screen>
  );
}
