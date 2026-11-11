import { describe, expect, it } from "vitest";
import { canEnroll, canRetire, expectation, issuer, LIFE_TONES, nextSteps, reading, said, silence, state, STATES, typed } from "./words";

describe("what a command says", () => {
  it("names the person who asked, or what kind of thing did", () => {
    expect(issuer("person", "ada@plant.test")).toBe("ada@plant.test");
    expect(issuer("engine", null)).toBe("An application");
    expect(issuer("new-kind", null)).toBe("new-kind");
  });

  it("drops what repeats the command, and keeps what a site said", () => {
    expect(said({ approval: null, by: "person" })).toBeNull();
    expect(said({ approval: "CAB-7", by: "person" })).toEqual({ approval: "CAB-7" });
    expect(said(null)).toBeNull();
  });

  it("reads a dial to two places", () => {
    expect(reading(63.06087946710097)).toBe("63.06");
    expect(reading(40)).toBe("40");
  });
});

describe("words for what was established", () => {
  it("never calls an accepted command done", () => {
    expect(state("Accepted").label).not.toMatch(/done|finished|complete/i);
    expect(state("Accepted").says).toMatch(/not the same/);
  });

  it("says plainly when nobody knows, and that nothing retries it", () => {
    expect(state("OutcomeUnknown").tone).toBe("red");
    expect(state("OutcomeUnknown").says).toMatch(/Nothing retries/);
  });

  it("has words for every state the ledger has", () => {
    for (const s of ["Proposed", "Refused", "Authorized", "Queued", "Expired", "Accepted", "Rejected", "Executing", "Completed", "Failed", "Aborted", "OutcomeUnknown"]) {
      expect(STATES[s]).toBeDefined();
    }
  });

  it("shows an unknown state as it is rather than hiding it", () => {
    expect(state("Mystery").label).toBe("Mystery");
  });
});

describe("a gateway's silence", () => {
  it("does not call silence failure, because a site may be working offline", () => {
    expect(silence(30).label).toBe("Connected");
    expect(silence(600).label).toBe("Silent for 10 min");
    expect(silence(7200).tone).toBe("red");
    expect(silence(null).label).toBe("Never heard from");
    expect(silence(200_000).label).toBe("Silent for 2 days");
  });
});

describe("a device's next steps", () => {
  it("cannot skip commissioning", () => {
    for (const from of ["Imported", "Claimed", "Provisioned"]) {
      expect(nextSteps(from).map((s) => s.to)).not.toContain("Active");
    }
    expect(nextSteps("Commissioned").map((s) => s.to)).toContain("Active");
  });

  it("reaches provisioned through its gateway when it has no key of its own", () => {
    expect(nextSteps("Claimed", "gw").map((s) => s.to)).toEqual(["Provisioned"]);
    expect(nextSteps("Claimed")).toEqual([]);
  });

  it("comes back from quarantine only by enrolling", () => {
    expect(nextSteps("Quarantined")).toEqual([]);
    expect(canEnroll("Quarantined")).toBe(true);
    expect(canEnroll("Active")).toBe(false);
  });

  it("ends at retired", () => {
    expect(nextSteps("Retired")).toEqual([]);
    expect(canRetire("Retired")).toBe(false);
  });
});

describe("acceptance on site", () => {
  it("says what a step expects", () => {
    expect(expectation({ min: 207, max: 253 })).toBe("207 to 253");
    expect(expectation({ min: 0 })).toBe("at least 0");
    expect(expectation({ equals: false })).toBe("exactly false");
    expect(expectation({ instruction: "The seal is intact" })).toBe("The seal is intact");
    expect(expectation(null)).toBe("");
  });

  it("takes a typed reading as the value it is", () => {
    expect(typed(" 231.4 ")).toBe(231.4);
    expect(typed("false")).toBe(false);
    expect(typed("OK")).toBe("OK");
    expect(typed("")).toBe("");
  });
});

describe("the colour of a state", () => {
  it("comes from the stored state id, and a state it does not know is neutral", () => {
    expect(state("Completed").tone).toBe("green");
    expect(state("Proposed").tone).toBe("amber");
    expect(state("Failed").tone).toBe("red");
    expect(state("Not completed").tone).toBe("neutral");
    expect(LIFE_TONES.Active).toBe("green");
    expect(LIFE_TONES.Quarantined).toBe("red");
    expect(LIFE_TONES.Maintenance).toBeUndefined();
  });
});
