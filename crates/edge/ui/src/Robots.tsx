// Robots, on a map of where they say they are. What is on a map here is reported, not measured: this
// is for people to see the fleet, and nothing that steers or stops a robot looks at it. Fleet
// managers own traffic.

import { useQuery } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Link } from "react-router";
import { relativeTime } from "@ferrum/client";
import { Badge, EmptyState, ErrorState, PageLoading, statusTone, useShellTitle } from "@ferrum/kit";
import { type Robot, robots } from "./data";
import { fit, type Placed, placed } from "./map";
import { Tabs } from "./Tabs";

function FleetMap({ list, chosen, choose }: { list: Placed[]; chosen: string | null; choose: (id: string) => void }) {
  const toScreen = useMemo(() => fit(list, 640, 360, 32), [list]);
  return (
    <svg className="edge-map" viewBox="0 0 640 360" role="img" aria-label={`${list.length} robots on a map of where they say they are`}>
      <rect x="0" y="0" width="640" height="360" className="edge-map-floor" />
      {list.map((r) => {
        const { x, y } = toScreen(r);
        const on = r.robot.id === chosen;
        const heading = r.theta === null ? null : { x: x + Math.cos(r.theta) * 16, y: y - Math.sin(r.theta) * 16 };
        return (
          <g key={r.robot.id} className={on ? "edge-robot edge-robot-on" : "edge-robot"} onClick={() => choose(r.robot.id)}>
            {heading ? <line x1={x} y1={y} x2={heading.x} y2={heading.y} /> : null}
            <circle cx={x} cy={y} r={on ? 10 : 8} />
            <text x={x + 13} y={y + 4}>
              {r.robot.code}
            </text>
          </g>
        );
      })}
    </svg>
  );
}

function Row({ r, on }: { r: Robot; on: boolean }) {
  const battery = r.metrics["battery.percent"]?.value;
  const when = Object.values(r.metrics)[0]?.at;
  return (
    <li className={on ? "edge-on" : undefined}>
      <Link to={`/d/${r.id}`} className="edge-row-main">
        <strong>{r.name}</strong>
        <div className="faint">
          {r.code}
          {r.site ? `, ${r.site}` : ""}
          {when ? `, heard ${relativeTime(when)}` : ""}
        </div>
      </Link>
      {battery !== undefined ? <Badge tone={battery < 20 ? "red" : "neutral"}>{Math.round(battery)}%</Badge> : null}
      {r.running ? <Badge tone="blue">Working</Badge> : null}
      <Badge tone={statusTone(r.state ?? r.lifecycle)}>{r.state ?? r.lifecycle}</Badge>
    </li>
  );
}

export function Robots() {
  useShellTitle("Robots");
  const q = useQuery({ queryKey: ["edge", "robots"], queryFn: robots, refetchInterval: 5_000 });
  const [chosen, choose] = useState<string | null>(null);
  const list = useMemo(() => q.data?.robots ?? [], [q.data]);
  const maps = useMemo(() => placed(list), [list]);
  return (
    <div className="edge-page">
      <Tabs />
      {q.isPending ? (
        <PageLoading />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : list.length === 0 ? (
        <EmptyState title="No robots yet" icon="dashboard">
          <Link to="/e/Device/new">Add a device of kind Robot</Link>
        </EmptyState>
      ) : (
        <>
          {[...maps.entries()].map(([name, robotsHere]) => (
            <section key={name} className="card card-pad stack" aria-label={name ? `Map ${name}` : "Map"}>
              {name ? <h2 className="edge-section">{name}</h2> : null}
              <FleetMap list={robotsHere} chosen={chosen} choose={choose} />
            </section>
          ))}
          <ul className="card edge-rows edge-rows-pad">
            {list.map((r) => (
              <Row key={r.id} r={r} on={r.id === chosen} />
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
