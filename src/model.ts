/**
 * Mirror of the Rust domain model in `core/src/model.rs`.
 *
 * Nothing below mentions a specific agent: the UI renders whatever sections and
 * gauges the backend hands it.
 */

export type Severity = "normal" | "warning" | "critical";

export type AgentStatus =
  | { kind: "ok" }
  | { kind: "stale"; lastOkAt: number; because: AgentStatus }
  | { kind: "notInstalled" }
  | { kind: "notSignedIn" }
  | { kind: "sessionExpired" }
  | { kind: "unauthorized" }
  | { kind: "rateLimited"; retryAt: number }
  | { kind: "serviceUnavailable"; httpStatus: number }
  | { kind: "offline"; host: string }
  | { kind: "timeout"; afterSecs: number }
  | { kind: "unexpectedResponse" }
  | { kind: "apiKeyMode" }
  | { kind: "noLimitsReported" };

export interface Gauge {
  id: string;
  label: string;
  usedPercent: number;
  resetsAt: number | null;
  severity: Severity;
}

export interface AgentSnapshot {
  id: string;
  displayName: string;
  status: AgentStatus;
  gauges: Gauge[];
  note: string | null;
}

export interface Snapshot {
  fetchedAt: number;
  agents: AgentSnapshot[];
}

export interface Settings {
  intervalSecs: number;
  alwaysOnTop: boolean;
}

/** The dot next to a section heading. */
export type Health = "ok" | "stale" | "error" | "idle";

export function healthOf(status: AgentStatus): Health {
  switch (status.kind) {
    case "ok":
      return "ok";
    case "stale":
      return "stale";
    case "notInstalled":
    case "notSignedIn":
    case "apiKeyMode":
    case "noLimitsReported":
      return "idle";
    default:
      return "error";
  }
}
