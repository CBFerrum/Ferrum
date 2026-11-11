// What a command's state means, in words a person on the floor would use. A state says only what
// was established: these words must never claim more than that.

import type { Tone } from "@ferrum/kit";
import { t as tr } from "@ferrum/client";

export interface Word {
  label: string;
  tone: Tone;
  says: string;
}

export const STATES: Record<string, Word> = {
  Proposed: { get label() { return tr("Waiting for approval"); }, tone: "amber", get says() { return tr("Nothing has been sent. A person decides."); } },
  Refused: { get label() { return tr("Refused"); }, tone: "neutral", get says() { return tr("It was not approved, and never left."); } },
  Authorized: { get label() { return tr("Approved"); }, tone: "blue", get says() { return tr("It may go, and has not yet."); } },
  Queued: { get label() { return tr("Waiting"); }, tone: "blue", get says() { return tr("Waiting for its site, or on it. Nothing has started as far as anyone knows."); } },
  Expired: { get label() { return tr("Too late"); }, tone: "neutral", get says() { return tr("Its deadline passed before it started. It did not reach the equipment."); } },
  Accepted: { get label() { return tr("Accepted"); }, tone: "blue", get says() { return tr("The equipment took it. That is not the same as doing it."); } },
  Rejected: { get label() { return tr("Declined"); }, tone: "neutral", get says() { return tr("The equipment declined it. Nothing happened."); } },
  Executing: { get label() { return tr("Running"); }, tone: "blue", get says() { return tr("The equipment says it has started."); } },
  Completed: { get label() { return tr("Finished"); }, tone: "green", get says() { return tr("The equipment says it finished."); } },
  Failed: { get label() { return tr("Failed"); }, tone: "red", get says() { return tr("It stopped without finishing. Part of it may have happened."); } },
  Aborted: { get label() { return tr("Stopped"); }, tone: "amber", get says() { return tr("It was stopped on request."); } },
  OutcomeUnknown: { get label() { return tr("Nobody knows"); }, tone: "red", get says() { return tr("Nobody can say what happened. Nothing retries this: look at the equipment, then say what you found."); } },
};

export const state = (s: string): Word => STATES[s] ?? { label: s, tone: "neutral", says: "" };

/** How a report was taken: an unchanged state is not an ignored report. */
export const STEPS: Record<string, string> = {
  move: "",
  get keep() { return tr("Heard, and changed nothing: it was late or a repeat"); },
  get conflict() { return tr("A second, different ending. The first stands until someone looks"); },
  get refuse() { return tr("Refused: it came from a gateway that no longer holds this"); },
};

/** A device's life stage by the stored id (the lifecycle options of Device): in service is green, set aside for safety is red. */
export const LIFE_TONES: Record<string, Tone> = { Active: "green", Quarantined: "red" };

const ISSUERS: Record<string, string> = {
  get person() { return tr("A person"); },
  get engine() { return tr("An application"); },
  get automation() { return tr("An automation"); },
  get ai() { return tr("An agent's proposal"); },
};

/** The words of a device's life, its acceptance steps and their outcomes (as the server names them). */
const NAMED: Record<string, () => string> = {
  Imported: () => tr("Imported"),
  Claimed: () => tr("Claimed"),
  Provisioned: () => tr("Provisioned"),
  Commissioned: () => tr("Commissioned"),
  Active: () => tr("Active"),
  Maintenance: () => tr("In maintenance"),
  Quarantined: () => tr("Quarantined"),
  Retired: () => tr("Retired"),
  Read: () => tr("Read"),
  Command: () => tr("Command"),
  Manual: () => tr("Manual"),
  Pending: () => tr("Pending"),
  Passed: () => tr("Passed"),
  Failed: () => tr("Failed"),
};

/** A name the server gives (a life stage, a step's kind, an outcome) in the page's words. */
export const named = (name: string) => NAMED[name]?.() ?? name;

/** Who asked for a command: the person if there was one. */
export const issuer = (kind: string, email: string | null | undefined) => email ?? ISSUERS[kind] ?? kind;

/** What is worth showing of a step's detail: who issued it is on the command already. */
export function said(detail: unknown): unknown {
  if (typeof detail !== "object" || detail === null) return detail;
  const left = Object.entries(detail).filter(([k, v]) => k !== "by" && v !== null);
  return left.length ? Object.fromEntries(left) : null;
}

/** A reading as a person reads a dial. */
export const reading = (value: number) => String(Math.round(value * 100) / 100);

/** How long a gateway has been silent, and how worried to be. It may be working offline: silence
 *  is not failure, so the worst it says is that nobody can see the site. */
export function silence(secs: number | null | undefined): { label: string; tone: Tone } {
  if (secs === null || secs === undefined) return { label: tr("Never heard from"), tone: "neutral" };
  if (secs < 120) return { label: tr("Connected"), tone: "green" };
  if (secs < 3600) return { label: tr("Silent for {minutes} min", { minutes: Math.round(secs / 60) }), tone: "amber" };
  if (secs < 86_400) return { label: tr("Silent for {hours} h", { hours: Math.round(secs / 3600) }), tone: "red" };
  return { label: tr("Silent for {days} days", { days: Math.round(secs / 86_400) }), tone: "red" };
}

export interface LifecycleStep {
  to: string;
  label: string;
  /** Moves it out of what it is doing, so it asks first. */
  serious?: boolean;
}

/** What can happen next to a device. Enrolling is separate: it takes a code, and a site to use it. */
export function nextSteps(lifecycle: string, gateway?: string | null): LifecycleStep[] {
  switch (lifecycle) {
    case "Imported":
      return [{ to: "Claimed", label: tr("Claim it") }];
    case "Claimed":
      // Equipment behind a gateway has no key of its own: the gateway speaks for it.
      return gateway ? [{ to: "Provisioned", label: tr("It is reached through its gateway") }] : [];
    case "Provisioned":
      return [{ to: "Commissioned", label: tr("Commission it") }, { to: "Quarantined", label: tr("Quarantine"), serious: true }];
    case "Commissioned":
      return [{ to: "Active", label: tr("Put it in service") }, { to: "Quarantined", label: tr("Quarantine"), serious: true }];
    case "Active":
      return [{ to: "Maintenance", label: tr("Take out for maintenance") }, { to: "Quarantined", label: tr("Quarantine"), serious: true }];
    case "Maintenance":
      return [{ to: "Active", label: tr("Put back in service") }, { to: "Quarantined", label: tr("Quarantine"), serious: true }];
    case "Quarantined":
      return gateway ? [{ to: "Provisioned", label: tr("It is safe again, behind its gateway"), serious: true }] : [];
    default:
      return [];
  }
}

export const canEnroll = (lifecycle: string) => lifecycle === "Claimed" || lifecycle === "Quarantined";

export const canRetire = (lifecycle: string) => lifecycle !== "Retired";

/** What an acceptance step expects, in words: a range, a value, or what a person looks at. */
export function expectation(e: Record<string, unknown> | null | undefined): string {
  if (!e) return "";
  if (typeof e.instruction === "string") return e.instruction;
  if ("equals" in e) return tr("exactly {value}", { value: JSON.stringify(e.equals) });
  const min = typeof e.min === "number" ? e.min : null;
  const max = typeof e.max === "number" ? e.max : null;
  if (min !== null && max !== null) return tr("{min} to {max}", { min, max });
  if (min !== null) return tr("at least {min}", { min });
  if (max !== null) return tr("at most {max}", { max });
  return "";
}

/** What was typed for a reading, as the value it is: a number, true or false, or the text. */
export function typed(s: string): unknown {
  const t = s.trim();
  if (t === "true") return true;
  if (t === "false") return false;
  return t !== "" && !Number.isNaN(Number(t)) ? Number(t) : t;
}
