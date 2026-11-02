// Where robots say they are, laid out on a screen.

import type { Robot } from "./data";

export interface Placed {
  robot: Robot;
  x: number;
  y: number;
  theta: number | null;
}

/** A position may arrive as an attribute or as a metric, whichever suited the definition. */
function reading(r: Robot, key: string): number | null {
  const v = r.attributes[key] ?? r.metrics[key]?.value;
  const n = typeof v === "number" ? v : typeof v === "string" ? Number(v) : NaN;
  return Number.isFinite(n) ? n : null;
}

/** Robots with a position, by the map each is on. A robot with none is simply not drawn. */
export function placed(robots: Robot[]): Map<string, Placed[]> {
  const maps = new Map<string, Placed[]>();
  for (const robot of robots) {
    const x = reading(robot, "position.x");
    const y = reading(robot, "position.y");
    if (x === null || y === null) continue;
    const map = typeof robot.attributes["position.map"] === "string" ? (robot.attributes["position.map"] as string) : "";
    maps.set(map, [...(maps.get(map) ?? []), { robot, x, y, theta: reading(robot, "position.theta") }]);
  }
  return maps;
}

export interface Bounds {
  minX: number;
  maxX: number;
  minY: number;
  maxY: number;
}

/** The area robots have been seen in. It only grows: a map that rescaled with each move would make
 * every robot appear to jump. */
export function grown(bounds: Bounds | null, list: { x: number; y: number }[]): Bounds {
  return list.reduce(
    (b, p) => ({ minX: Math.min(b.minX, p.x), maxX: Math.max(b.maxX, p.x), minY: Math.min(b.minY, p.y), maxY: Math.max(b.maxY, p.y) }),
    bounds ?? { minX: Infinity, maxX: -Infinity, minY: Infinity, maxY: -Infinity },
  );
}

/** Scales positions to fit, keeping the proportions, with north up. */
export function fit({ minX, maxX, minY, maxY }: Bounds, width: number, height: number, margin: number) {
  // One robot, or several in a line, must not divide by nothing.
  const span = Math.max(maxX - minX, maxY - minY, 1);
  const scale = Math.min(width - 2 * margin, height - 2 * margin) / span;
  const ox = (width - (maxX - minX) * scale) / 2;
  const oy = (height - (maxY - minY) * scale) / 2;
  return (p: { x: number; y: number }) => ({ x: ox + (p.x - minX) * scale, y: height - oy - (p.y - minY) * scale });
}
