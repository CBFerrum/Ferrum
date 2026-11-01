// What a command's state means, in words a person on the floor would use. A state says only what
// was established: these words must never claim more than that.

import type { Tone } from "@ferrum/kit";

export interface Word {
  label: string;
  tone: Tone;
  says: string;
}

export const STATES: Record<string, Word> = {
  Proposed: { label: "Waiting for approval", tone: "amber", says: "Nothing has been sent. A person decides." },
  Refused: { label: "Refused", tone: "neutral", says: "It was not approved, and never left." },
  Authorized: { label: "Approved", tone: "blue", says: "It may go, and has not yet." },
  Queued: { label: "Waiting", tone: "blue", says: "Waiting for its site, or on it. Nothing has started as far as anyone knows." },
  Expired: { label: "Too late", tone: "neutral", says: "Its deadline passed before it started. It did not reach the equipment." },
  Accepted: { label: "Accepted", tone: "blue", says: "The equipment took it. That is not the same as doing it." },
  Rejected: { label: "Declined", tone: "neutral", says: "The equipment declined it. Nothing happened." },
  Executing: { label: "Running", tone: "blue", says: "The equipment says it has started." },
  Completed: { label: "Finished", tone: "green", says: "The equipment says it finished." },
  Failed: { label: "Failed", tone: "red", says: "It stopped without finishing. Part of it may have happened." },
  Aborted: { label: "Stopped", tone: "amber", says: "It was stopped on request." },
  OutcomeUnknown: { label: "Nobody knows", tone: "red", says: "Nobody can say what happened. Nothing retries this: look at the equipment, then say what you found." },
};

export const state = (s: string): Word => STATES[s] ?? { label: s, tone: "neutral", says: "" };

/** How a report was taken: an unchanged state is not an ignored report. */
export const STEPS: Record<string, string> = {
  move: "",
  keep: "Heard, and changed nothing: it was late or a repeat",
  conflict: "A second, different ending. The first stands until someone looks",
  refuse: "Refused: it came from a gateway that no longer holds this",
};

/** How long a gateway has been silent, and how worried to be. It may be working offline: silence
 *  is not failure, so the worst it says is that nobody can see the site. */
export function silence(secs: number | null | undefined): { label: string; tone: Tone } {
  if (secs === null || secs === undefined) return { label: "Never heard from", tone: "neutral" };
  if (secs < 120) return { label: "Connected", tone: "green" };
  if (secs < 3600) return { label: `Silent for ${Math.round(secs / 60)} min`, tone: "amber" };
  if (secs < 86_400) return { label: `Silent for ${Math.round(secs / 3600)} h`, tone: "red" };
  return { label: `Silent for ${Math.round(secs / 86_400)} days`, tone: "red" };
}

export interface LifecycleStep {
  to: string;
  label: string;
  /** Moves it out of what it is doing, so it asks first. */
  serious?: boolean;
}

/** What can happen next to a device. Enrolling is separate: it takes a code, and a site to use it. */
export function nextSteps(lifecycle: string): LifecycleStep[] {
  switch (lifecycle) {
    case "Imported":
      return [{ to: "Claimed", label: "Claim it" }];
    case "Provisioned":
      return [{ to: "Commissioned", label: "Commission it" }, { to: "Quarantined", label: "Quarantine", serious: true }];
    case "Commissioned":
      return [{ to: "Active", label: "Put it in service" }, { to: "Quarantined", label: "Quarantine", serious: true }];
    case "Active":
      return [{ to: "Maintenance", label: "Take out for maintenance" }, { to: "Quarantined", label: "Quarantine", serious: true }];
    case "Maintenance":
      return [{ to: "Active", label: "Put back in service" }, { to: "Quarantined", label: "Quarantine", serious: true }];
    default:
      return [];
  }
}

export const canEnroll = (lifecycle: string) => lifecycle === "Claimed" || lifecycle === "Quarantined";

export const canRetire = (lifecycle: string) => lifecycle !== "Retired";
