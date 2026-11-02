// Edge's endpoints, and the shapes they answer with.

import { api, docPath } from "@ferrum/client";

export interface LinkHealth {
  address: string;
  definition: string;
  up: boolean;
  devices: number;
  messages: number;
  readings: number;
  dropped: number;
  reconnects: number;
  last_error: string | null;
}

export interface Gateway {
  id: string;
  code: string;
  name: string;
  lifecycle: string;
  site: string | null;
  last_seen_at: string | null;
  silent_secs: number | null;
  links: LinkHealth[];
  problems: string[];
  runtime: string | null;
  config_version: string | null;
  devices: number;
}

export interface Stranger {
  gateway: string;
  gateway_name: string;
  code: string;
  last_seen_at: string;
}

export interface Overview {
  gateways: Gateway[];
  alarms: Record<string, number>;
  devices: { active: number; defined: number; definitions: number } | Record<string, number>;
  strangers: Stranger[];
}

export interface DeviceRow {
  id: string;
  code: string;
  name: string;
  kind: string;
  lifecycle: string;
  site: string | null;
  gateway: string | null;
  definition: string | null;
  alarms: number;
}

export interface Attribute {
  key: string;
  value: unknown;
  reported_at: string | null;
  desired: unknown;
  desired_at: string | null;
  in_step: boolean | null;
}

export interface AlarmRow {
  id: string;
  summary: string;
  severity: string;
  status: string;
  raised_at: string;
  cleared_at?: string | null;
  acknowledged_by?: string | null;
  note?: string | null;
  value?: number | null;
  device?: string;
  code?: string | null;
  flow?: string | null;
  rule?: string | null;
}

export interface DeviceDetail {
  latest: { metrics?: { metric: string; value: number; at: string; age_secs?: number }[] } | { metric: string; value: number; at: string }[];
  attributes: Attribute[];
  alarms: AlarmRow[];
  events: { name: string; value: unknown; at: string }[];
}

export interface Robot {
  id: string;
  code: string;
  name: string;
  lifecycle: string;
  site: string | null;
  state: string | null;
  running: number;
  metrics: Record<string, { value: number; at: string }>;
  attributes: Record<string, unknown>;
}

export interface Template {
  name: string;
  about: string;
  group: string;
  body: string;
}

export interface Tried {
  ok: boolean;
  error?: string;
  decoded?: { kind: string; device: string | null; name?: string; value: unknown; unit?: string | null }[];
  schemes?: string[];
}

export interface Moment {
  at: number;
  effects: { effect: string; [key: string]: unknown }[];
  trace: { node: string; device: string | null; value: unknown; ports: string[] }[];
  problems: string[];
}

export interface Simulated {
  ok: boolean;
  error?: string;
  digest?: string;
  moments?: Moment[];
}

export interface FlowDoc {
  id: string;
  version: number;
  flow_name: string;
  status: "Draft" | "In Review" | "Approved";
  live: boolean | null;
  revision: number | null;
  site: string | null;
  body: string | null;
  published_body: string | null;
  simulated_digest: string | null;
  submitted_by: string | null;
  approved_by: string | null;
  approved_at: string | null;
  activated_by: string | null;
  activated_at: string | null;
  review_note: string | null;
  notes: string | null;
  _titles?: Record<string, string>;
}

export interface DefinitionDoc {
  id: string;
  version: number;
  definition_name: string;
  status: "Draft" | "Published";
  revision: number | null;
  body: string;
  published_body: string | null;
  notes: string | null;
}

const base = "/api/app/edge";

export const overview = () => api.get<Overview>(`${base}/overview`);
export const devices = (q: string) => api.get<{ devices: DeviceRow[] }>(`${base}/devices`, q ? { q } : {});
export const device = (id: string) => api.get<DeviceDetail>(`${base}/devices/${id}`);
export const desire = (id: string, key: string, value: unknown) => api.put<{ command: string }>(`${base}/devices/${id}/desired`, { key, value });
export const deviceRecord = (id: string) => api.get<{ device_name: string; device_code: string; kind: string; operations?: { operation: string }[] }>(docPath("Device", id));
export const sendCommand = (target: string, operation: string, payload: unknown, expires_in_secs: number) =>
  api.post<{ id: string; state: string }>("/api/app/ops/commands", { target, operation, payload, expires_in_secs });
export const newToken = (id: string) => api.post<{ token: string }>(`${base}/devices/${id}/token`, {});
export const alarms = (all: boolean) => api.get<{ alarms: AlarmRow[] }>(`${base}/alarms`, all ? { all: 1 } : {});
export const acknowledge = (id: string, note: string) => api.post(`${base}/alarms/${id}/acknowledge`, { note: note || null });
export const robots = () => api.get<{ robots: Robot[] }>(`${base}/robots`);
export const claim = (gateway: string, code: string, name: string, definition: string | null) => api.post<{ id: string }>(`${base}/strangers/claim`, { gateway, code, name, definition });
export const templates = () => api.get<{ templates: Template[] }>(`${base}/templates`);
export const tryDefinition = (body: string, topic: string, sample: string, hex: boolean) => api.post<Tried>(`${base}/definitions/try`, hex ? { body, topic, sample_hex: sample } : { body, topic, sample });
export const tryFlow = (body: string, script: unknown[] = [], until = 0) => api.post<Simulated>(`${base}/flows/try`, { body, script, until });
export const simulateFlow = (id: string, script: unknown[], until: number) => api.post<Simulated>(`${base}/flows/${id}/simulate`, { script, until });

export const flowList = () =>
  api.get<{ rows: FlowDoc[] }>("/api/v1/EdgeFlow", { fields: "flow_name,status,live,revision,site,submitted_by,approved_by,activated_at", limit: 200, order_by: "flow_name asc", titles: true });
export const flowDoc = (id: string) => api.get<FlowDoc>(docPath("EdgeFlow", id), { titles: true });
export const newFlow = (name: string, body: string) => api.post<{ id: string }>("/api/v1/EdgeFlow", { flow_name: name, body });
export const saveFlow = (id: string, version: number, patch: Partial<FlowDoc>) => api.patch<FlowDoc>(docPath("EdgeFlow", id), patch, { ifMatch: version });
export const flowAction = (id: string, name: string, params: unknown = {}) => api.post(docPath("EdgeFlow", id, "actions", name), params);

export const definitionList = () => api.get<{ rows: DefinitionDoc[] }>("/api/v1/DeviceDefinition", { fields: "definition_name,status,revision", limit: 200, order_by: "definition_name asc" });
export const definitionDoc = (id: string) => api.get<DefinitionDoc>(docPath("DeviceDefinition", id));
export const newDefinition = (name: string, body: string) => api.post<{ id: string }>("/api/v1/DeviceDefinition", { definition_name: name, body });
export const saveDefinition = (id: string, version: number, patch: Partial<DefinitionDoc>) => api.patch<DefinitionDoc>(docPath("DeviceDefinition", id), patch, { ifMatch: version });
export const publishDefinition = (id: string) => api.post<{ revision: number }>(docPath("DeviceDefinition", id, "actions", "publish"), {});
