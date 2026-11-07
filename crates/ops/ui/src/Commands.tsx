// Every command, newest first, with the state in the address so a link shows the same list.

import { useQuery } from "@tanstack/react-query";
import { Link, useSearchParams } from "react-router";
import { formatDateTime, t, isolate } from "@ferrum/client";
import { Badge, EmptyState, ErrorState, PageLoading, useShellTitle } from "@ferrum/kit";
import { commands } from "./data";
import { Tabs } from "./Tabs";
import { state, STATES } from "./words";

export function Commands() {
  useShellTitle(t("Commands"));
  const [params, setParams] = useSearchParams();
  const only = params.get("state") ?? "";
  const q = useQuery({ queryKey: ["ops", "commands", only], queryFn: () => commands(only ? { state: only, limit: 200 } : { limit: 200 }), refetchInterval: 15_000 });
  return (
    <div className="ops-page">
      <Tabs />
      <label className="ops-filter">
        {t("Show")}{isolate(" ")}
        <select className="select" value={only} onChange={(e) => setParams(e.target.value ? { state: e.target.value } : {})}>
          <option value="">{t("Everything")}</option>
          {Object.entries(STATES).map(([s, w]) => (
            <option key={s} value={s}>
              {isolate(w.label)}
            </option>
          ))}
        </select>
      </label>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : q.data.length === 0 ? (
        <EmptyState title={only ? t("None like that") : t("No commands yet")} icon="list" />
      ) : (
        <ul className="ops-list card">
          {q.data.map((c) => (
            <li key={c.id} className="ops-row">
              <div className="ops-row-body">
                <Link to={`/c/${c.id}`}>
                  {isolate(c.operation)}
                  {c.target_code ? ` · ${c.target_code}` : ""}
                </Link>{isolate(" ")}
                <Badge tone={state(c.state).tone}>{isolate(state(c.state).label)}</Badge>
                <div className="faint">
                  {isolate(formatDateTime(c.issued_at))} · {isolate(c.domain)}
                </div>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
