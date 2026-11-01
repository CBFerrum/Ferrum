// What the gateways reach. The search lives in the URL, so a link to it works.

import { useQuery } from "@tanstack/react-query";
import { Link, useSearchParams } from "react-router";
import { Badge, EmptyState, ErrorState, PageLoading, statusTone, useDebounced, useShellTitle } from "@ferrum/kit";
import { devices } from "./data";
import { Tabs } from "./Tabs";

export function Devices() {
  useShellTitle("Devices");
  const [params, setParams] = useSearchParams();
  const text = params.get("q") ?? "";
  const q = useDebounced(text, 250);
  const list = useQuery({ queryKey: ["edge", "devices", q], queryFn: () => devices(q), refetchInterval: 30_000 });
  return (
    <div className="edge-page">
      <Tabs />
      <div className="edge-bar">
        <input
          className="input edge-search"
          type="search"
          aria-label="Find a device"
          placeholder="Find a device"
          value={text}
          onChange={(e) => setParams(e.target.value ? { q: e.target.value } : {}, { replace: true })}
        />
        <Link className="btn btn-default" to="/e/Device/new">
          Add a device
        </Link>
      </div>
      {list.isPending ? (
        <PageLoading />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : list.data.devices.length === 0 ? (
        <EmptyState title={text ? "Nothing matches that" : "No devices yet"} icon="dashboard">
          {text ? null : <Link to="/e/Device/new">Add your first device</Link>}
        </EmptyState>
      ) : (
        <ul className="card edge-rows edge-rows-pad">
          {list.data.devices.map((d) => (
            <li key={d.id}>
              <Link to={`/d/${d.id}`} className="edge-row-main">
                <strong>{d.name}</strong>
                <div className="faint">
                  {d.code}
                  {d.definition ? `, read by ${d.definition}` : ""}
                  {d.gateway ? `, through ${d.gateway}` : ""}
                </div>
              </Link>
              {d.alarms ? <Badge tone="red">{d.alarms === 1 ? "1 alarm" : `${d.alarms} alarms`}</Badge> : null}
              <Badge tone={statusTone(d.lifecycle)}>{d.lifecycle}</Badge>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
