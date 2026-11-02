import { describe, expect, it } from "vitest";
import type { Robot } from "./data";
import { fit, grown, placed } from "./map";

const robot = (id: string, attributes: Record<string, unknown>, metrics: Robot["metrics"] = {}): Robot => ({ id, code: id, name: id, lifecycle: "Active", site: null, state: null, running: 0, metrics, attributes });

describe("the map", () => {
  it("takes a position from an attribute or a metric, and leaves out a robot with none", () => {
    const maps = placed([
      robot("a", { "position.x": 1, "position.y": 2, "position.map": "hall" }),
      robot("b", {}, { "position.x": { value: 3, at: "" }, "position.y": { value: 4, at: "" } }),
      robot("c", {}),
    ]);
    expect(maps.get("hall")!.map((p) => p.robot.id)).toEqual(["a"]);
    expect(maps.get("")![0]).toMatchObject({ x: 3, y: 4 });
    expect([...maps.values()].flat()).toHaveLength(2);
  });

  it("keeps a lone robot in the middle, and north up", () => {
    const one = fit(grown(null, [{ x: 5, y: 5 }]), 100, 100, 10);
    expect(one({ x: 5, y: 5 })).toEqual({ x: 50, y: 50 });
    const two = fit(grown(null, [{ x: 0, y: 0 }, { x: 10, y: 10 }]), 100, 100, 10);
    expect(two({ x: 10, y: 10 }).y).toBeLessThan(two({ x: 0, y: 0 }).y);
  });

  it("does not rescale as a robot moves inside what it has seen", () => {
    const seen = grown(null, [{ x: 0, y: 0 }, { x: 10, y: 10 }]);
    expect(grown(seen, [{ x: 5, y: 5 }])).toEqual(seen);
    expect(grown(seen, [{ x: 20, y: 5 }]).maxX).toBe(20);
  });
});
