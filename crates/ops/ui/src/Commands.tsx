// Every command, newest first, with the state in the address so a link shows the same list.

import { useQuery } from "@tanstack/react-query";
import { Link, useSearchParams } from "react-router";
import { formatDateTime } from "@ferrum/client";
import { Badge, EmptyState, ErrorState, PageLoading, useShellTitle } from "@ferrum/kit";
import { commands } from "./data";
import { Tabs } from "./Tabs";
import { state, STATES } from "./words";

export function Commands() {
  useShellTitle("Commands");
  const [params, setParams] = useSearchParams();
  const only = params.get("state") ?? "";
  const q = useQuery({ queryKey: ["ops", "commands", only], queryFn: () => commands(only ? { state: only, limit: 200 } : { limit: 200 }), refetchInterval: 15_000 });
  return (
    <div className="ops-page">
      <Tabs />
      <label className="ops-filter">
        Show{" "}
        <select className="select" value={only} onChange={(e) => setParams(e.target.value ? { state: e.target.value } : {})}>
          <option value="">Everything</option>
          {Object.entries(STATES).map(([s, w]) => (
            <option key={s} value={s}>
              {w.label}
            </option>
          ))}
        </select>
      </label>
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : q.data.length === 0 ? (
        <EmptyState title={only ? "None like that" : "No commands yet"} icon="send" />
      ) : (
        <ul className="ops-list card">
          {q.data.map((c) => (
            <li key={c.id} className="ops-row">
              <div className="ops-row-body">
                <Link to={`/c/${c.id}`}>{c.operation}</Link> <Badge tone={state(c.state).tone}>{state(c.state).label}</Badge>
                <div className="faint">
                  {formatDateTime(c.issued_at)} · {c.domain}
                </div>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
