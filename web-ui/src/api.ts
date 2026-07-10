const BASE = "";

function authHeaders(): Record<string, string> {
  const token = localStorage.getItem("dora_api_token");
  const headers: Record<string, string> = {};
  if (token) headers["Authorization"] = `Bearer ${token}`;
  return headers;
}

async function get<T>(path: string, params?: Record<string, string>): Promise<T> {
  const url = new URL(path, window.location.origin);
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      if (v) url.searchParams.set(k, v);
    }
  }
  const res = await fetch(`${BASE}${url.pathname}${url.search}`, {
    headers: authHeaders(),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`${res.status}: ${body}`);
  }
  return res.json();
}

export async function post<T>(path: string, body?: Record<string, unknown>): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status}: ${text}`);
  }
  return res.json();
}

export interface HealthResponse {
  status: string;
  request_id: string;
}

export interface ReadinessResponse {
  status: "ready" | "not_ready";
  checks: { name: string; status: string; message?: string }[];
  request_id: string;
}

export interface ServerInfo {
  id: string;
  version: string;
  started_at: string;
  mode: string;
  api: { version: string; auth: string[] };
}

export interface MetricsSummary {
  uptime_seconds: number;
  in_flight: number;
  dhcpv4: ProtocolMetrics;
  dhcpv6: ProtocolMetrics;
}

export interface ProtocolMetrics {
  messages_received: number;
  messages_sent: number;
  errors: number;
}

export interface PaginationMeta {
  limit: number;
  offset: number;
  total: number;
  count: number;
  filters: Record<string, unknown>;
  sort: string[];
}

export interface V4Lease {
  family: "v4";
  state: string;
  ip: string;
  network: string;
  client_id?: string;
  expires_at?: string;
  source?: string;
}

export interface V6Lease {
  family: "v6";
  state: string;
  lease_type: string;
  ip?: string;
  prefix?: string;
  network: string;
  client_id?: string;
  iaid?: number;
  expires_at?: string;
  source?: string;
}

export interface V4LeaseListResponse {
  meta: PaginationMeta;
  items: V4Lease[];
}

export interface V6LeaseListResponse {
  meta: PaginationMeta;
  items: V6Lease[];
}

export interface V4Reservation {
  family: "v4";
  ip: string;
  network?: string;
  source: string;
  match: Record<string, unknown>;
}

export interface V6Reservation {
  family: "v6";
  ip?: string;
  prefix?: string;
  network?: string;
  source: string;
  match: Record<string, unknown>;
}

export interface V4ReservationListResponse {
  meta: PaginationMeta;
  items: V4Reservation[];
}

export interface V6ReservationListResponse {
  meta: PaginationMeta;
  items: V6Reservation[];
}

export interface ConfigDocument {
  version: string;
  redacted: boolean;
  document: Record<string, unknown>;
}

export interface ConfigCandidate {
  candidate_id: string;
  status: "staged" | "validating" | "valid" | "invalid" | "activated" | "superseded";
  created_at: string;
  activated_at?: string;
  message?: string;
  validation?: { level: string; path?: string; message: string }[];
  document?: Record<string, unknown>;
}

export interface ConfigCandidateListResponse {
  meta: PaginationMeta;
  items: ConfigCandidate[];
}

export async function del<T>(path: string, body?: Record<string, unknown>): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    method: "DELETE",
    headers: { ...authHeaders(), "Content-Type": "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`${res.status}: ${text}`);
  }
  return res.json();
}

export const api = {
  health: () => get<HealthResponse>("/health"),
  ready: () => get<ReadinessResponse>("/ready"),
  server: () => get<ServerInfo>("/v1/server"),
  metricsSummary: () => get<MetricsSummary>("/v1/metrics/summary"),
  leasesV4: (params?: Record<string, string>) =>
    get<V4LeaseListResponse>("/v1/leases/v4", params),
  leasesV6: (params?: Record<string, string>) =>
    get<V6LeaseListResponse>("/v1/leases/v6", params),
  reservationsV4: (params?: Record<string, string>) =>
    get<V4ReservationListResponse>("/v1/reservations/v4", params),
  reservationsV6: (params?: Record<string, string>) =>
    get<V6ReservationListResponse>("/v1/reservations/v6", params),
  config: () => get<ConfigDocument>("/v1/config"),
  configCandidates: (params?: Record<string, string>) =>
    get<ConfigCandidateListResponse>("/v1/config/candidates", params),
  configCandidate: (id: string) =>
    get<ConfigCandidate>(`/v1/config/candidates/${id}`),
  releaseLease: (family: "v4" | "v6", ip: string) =>
    post<{ message: string }>("/v1/actions/release-lease", { family, ip }),
  activateConfig: (candidateId: string) =>
    post<{ message: string }>("/v1/actions/activate-config", { candidate_id: candidateId }),
};
