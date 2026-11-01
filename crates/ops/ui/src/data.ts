// The ledger's endpoints, and the shapes they answer with.

import { api } from "@ferrum/client";

export interface Domain {
  domain: string;
  epoch: number;
  holder: string | null;
  holder_name: string | null;
  last_seen_at: string | null;
  silent_secs: number | null;
  health: Health | null;
}

export interface Health {
  version?: string;
  stopped?: string | null;
  backlog_secs?: number;
  queued_commands?: number;
  unknown_outcomes?: number;
  telemetry_shed?: number;
  disk_free_bytes?: number;
  adapters?: Record<string, string>;
}

export interface SiteRow {
  id: string;
  name: string;
  code: string;
  status: string;
  max_offline_hours: number;
  domains: Domain[];
  waiting: number;
  running: number;
  needs_person: number;
  devices: Record<string, number> | null;
}

export interface CommandRow {
  id: string;
  site_id: string;
  domain: string;
  target_id: string;
  target_code: string | null;
  target_name: string | null;
  operation: string;
  state: string;
  issued_at: string;
  expires_at: string;
  issued_kind: string;
  issued_by_email: string | null;
  conflict: boolean;
  cancel_requested_at: string | null;
  sent_at: string | null;
  correlation_id: string | null;
}

export interface HistoryRow {
  seq: number;
  state: string;
  step: string;
  at_source: string | null;
  received_at: string;
  epoch: number | null;
  detail: unknown;
  evidence: unknown;
}

export interface CommandFull extends CommandRow {
  payload: unknown;
  outcome: unknown;
  history: HistoryRow[];
}

const base = "/api/app/ops";

export const overview = () => api.get<{ sites: SiteRow[]; server_time: string }>(`${base}/overview`);
export const commands = (query: Record<string, string | number>) => api.get<{ commands: CommandRow[] }>(`${base}/commands`, query).then((r) => r.commands);
export const command = (id: string) => api.get<CommandFull>(`${base}/commands/${id}`);
export const act = (id: string, verb: "approve" | "refuse" | "cancel" | "reconcile", body: unknown = {}) => api.post<{ state: string | null }>(`${base}/commands/${id}/${verb}`, body);
export const enrollment = (device: string) => api.post<{ code: string; expires_at: string }>(`${base}/devices/${device}/enrollment`, {});
export const setLifecycle = (device: string, to: string, reason: string) => api.post<{ lifecycle: string }>(`${base}/devices/${device}/lifecycle`, { to, reason });
export const latest = (device: string) => api.get<{ metrics: { metric: string; value: number; at: string; age_secs: number }[] }>(`${base}/devices/${device}/latest`).then((r) => r.metrics);
export const setPin = (site: string, email: string, pin: string) => api.post(`${base}/operators/pin`, { site, email, pin });
export const transfer = (domain: string, to: string, reason: string) => api.post<{ epoch: number }>(`${base}/authority/transfer`, { domain, to, reason });
