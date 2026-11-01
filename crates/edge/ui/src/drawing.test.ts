import { describe, expect, it } from "vitest";
import { add, arrange, change, connect, disconnect, firstScript, inputsOf, parseFlow, portsOf, reaches, remove, sayEffect, scriptBody, STARTS, summary, waitFor, whyNot, wirePath, type Flow } from "./drawing";

const chain: Flow = {
  nodes: [
    { id: "a", kind: "metric", metric: "t", x: 0, y: 0 },
    { id: "b", kind: "compare", op: ">", value: 1, x: 0, y: 0 },
    { id: "c", kind: "alarm", name: "x", severity: "Warning", x: 0, y: 0 },
  ],
  edges: [
    { from: "a", to: "b" },
    { from: "b", port: "yes", to: "c" },
  ],
};

describe("drawing", () => {
  it("will not go round in a circle", () => {
    expect(reaches(chain, "a", "c")).toBe(true);
    expect(whyNot(chain, { from: "c", to: "a" })).toContain("circle");
    expect(whyNot(chain, { from: "a", to: "a" })).toContain("itself");
    expect(connect(chain, { from: "c", to: "a" })).toBe(chain);
  });

  it("does not join two things the same way twice", () => {
    expect(whyNot(chain, { from: "a", to: "b" })).toContain("already");
    expect(whyNot(chain, { from: "b", port: "no", to: "c", input: "clear" })).toBeNull();
  });

  it("takes a step's wires away with it", () => {
    const left = remove(chain, "b");
    expect(left.nodes.map((n) => n.id)).toEqual(["a", "c"]);
    expect(left.edges).toEqual([]);
    expect(disconnect(chain, chain.edges[0]!).edges).toHaveLength(1);
  });

  it("puts a new step where nothing else is, and numbers it", () => {
    const one = add({ nodes: [], edges: [] }, "metric");
    const two = add(one.flow, "metric");
    expect([one.id, two.id]).toEqual(["metric-1", "metric-2"]);
    const [a, b] = two.flow.nodes;
    expect(a!.y === b!.y && a!.x === b!.x).toBe(false);
  });

  it("clears a setting that was emptied, and a switch's wires with a case that went away", () => {
    const n = change(chain, "a", { device: "P" });
    expect(n.nodes[0]!.device).toBe("P");
    expect("device" in change(n, "a", { device: "" }).nodes[0]!).toBe(false);
    const sw: Flow = { nodes: [{ id: "s", kind: "switch", cases: ["A", "B"], x: 0, y: 0 }], edges: [{ from: "s", port: "A", to: "x" }, { from: "s", port: "B", to: "y" }] };
    expect(portsOf(sw.nodes[0]!)).toEqual(["A", "B", "other"]);
    expect(change(sw, "s", { cases: ["B"] }).edges.map((e) => e.port)).toEqual(["B"]);
  });

  it("lays a flow out left to right", () => {
    const tidy = arrange(chain);
    const [a, b, c] = tidy.nodes;
    expect(a!.x).toBeLessThan(b!.x);
    expect(b!.x).toBeLessThan(c!.x);
  });

  it("gives a combine the inputs that were wired to it", () => {
    const f: Flow = { nodes: [{ id: "k", kind: "combine", x: 0, y: 0 }], edges: [{ from: "a", to: "k", input: "v" }, { from: "b", to: "k", input: "a" }] };
    expect(inputsOf(f, f.nodes[0]!)).toEqual(["v", "a"]);
    expect(inputsOf(chain, chain.nodes[2]!)).toEqual(["in", "clear"]);
  });

  it("keeps a wire level where it leaves and arrives", () => {
    expect(wirePath({ x: 0, y: 0 }, { x: 100, y: 50 })).toBe("M0,0 C50,0 50,50 100,50");
  });

  it("starts again from a draft it cannot draw", () => {
    expect(parseFlow("not finished yet")).toEqual({ nodes: [], edges: [] });
    expect(parseFlow(null)).toEqual({ nodes: [], edges: [] });
    expect(parseFlow(JSON.stringify(chain)).nodes).toHaveLength(3);
  });
});

describe("words", () => {
  it("says what a step has been set to", () => {
    expect(summary(chain.nodes[1]!)).toBe("above 1");
    expect(summary({ id: "h", kind: "hold", for: "30s", x: 0, y: 0 })).toBe("For 30s");
    expect(summary({ id: "m", kind: "metric", metric: "", x: 0, y: 0 })).toContain("Say which");
  });

  it("says what a simulation did", () => {
    expect(sayEffect({ effect: "alarm", raised: true, name: "Hot", device: "P1" })).toBe("Raises “Hot” on P1");
    expect(sayEffect({ effect: "alarm", raised: false, name: "Hot", device: "P1" })).toBe("Clears “Hot” on P1");
    expect(sayEffect({ effect: "command", operation: "fan.start", device: "P1" })).toContain("decides");
  });
});

describe("simulating", () => {
  it("starts from the flow's own sources", () => {
    expect(firstScript(chain)).toEqual([{ at: 0, device: "DEVICE-1", what: "metric", name: "t", value: "1" }]);
    const wild: Flow = { nodes: [{ id: "s", kind: "state", device: "PRESS-*", x: 0, y: 0 }], edges: [] };
    expect(firstScript(wild)[0]!.device).toBe("PRESS-1");
  });

  it("writes a script the way the server reads it", () => {
    expect(scriptBody([{ at: 5, device: "P", what: "metric", name: "t", value: "90" }])).toEqual([{ at: 5, device: "P", metric: "t", value: 90 }]);
    expect(scriptBody([{ at: 0, device: "P", what: "state", name: "", value: "Running" }])).toEqual([{ at: 0, device: "P", state: "Running" }]);
    expect(scriptBody([{ at: 0, device: "P", what: "event", name: "e", value: "words" }])).toEqual([{ at: 0, device: "P", event: "e", value: "words" }]);
  });

  it("runs the clock long enough to see a hold fire", () => {
    const f: Flow = { nodes: [{ id: "h", kind: "hold", for: "2m", x: 0, y: 0 }], edges: [] };
    expect(waitFor(f, [{ at: 10, device: "P", what: "metric", name: "t", value: "1" }])).toBe(131);
  });
});

describe("ways to begin", () => {
  it("are all drawn so that nothing dangles", () => {
    for (const s of STARTS) {
      const ids = new Set(s.flow.nodes.map((n) => n.id));
      for (const e of s.flow.edges) {
        expect(ids.has(e.from) && ids.has(e.to)).toBe(true);
      }
    }
  });
});
