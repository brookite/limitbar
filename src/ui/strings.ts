/**
 * Every piece of prose the widget shows, in one place, so the interface language
 * is a single-file change.
 *
 * Gauge labels are deliberately not here: they come from the provider and match
 * the wording the agents' own `/usage` and `/status` output uses.
 */

import type { AgentStatus } from "../model";

export const ui = {
  pinOn: "Unpin window",
  pinOff: "Keep window on top",
  close: "Close",
  refreshNow: "Refresh now",
  intervalLabel: "Refresh interval",
  manual: "Manual",
  seconds: (n: number) => `${n} s`,
  /** Shown only in manual mode, where the numbers age until you ask for more. */
  lastUpdated: (text: string) => `Updated ${text} ago`,
  lastUpdatedJustNow: "Updated just now",
  neverUpdated: "Not updated yet",
  noAgents: "No agents found",
  updatedJustNow: "updated just now",
  updatedAgo: (text: string) => `updated ${text} ago`,
  resetsIn: (text: string) => `resets in ${text}`,
  resetPassed: "resetting soon",
  additionalInfo: "More details",
  collapseAgent: (name: string) => `Collapse ${name} section`,
  expandAgent: (name: string) => `Expand ${name} section`,
  reorderAgent: (name: string) => `Reorder ${name}: drag or use Up/Down arrow keys`,
  expiresIn: (text: string) => `expires in ${text}`,
};

/** A sentence explaining a section's state, plus what to do about it. */
export function statusMessage(status: AgentStatus, displayName: string): string {
  switch (status.kind) {
    case "ok":
      return "";
    case "stale":
      return statusMessage(status.because, displayName);
    case "notInstalled":
      return `${displayName} is not installed`;
    case "notSignedIn":
      return "Not signed in — open the agent and sign in";
    case "sessionExpired":
      return "Session expired — open the agent to refresh it";
    case "unauthorized":
      return "Credentials rejected — sign in to the agent again";
    case "rateLimited":
      return `Rate limited — retry ${atTime(status.retryAt)}`;
    case "serviceUnavailable":
      return `Service unavailable (HTTP ${status.httpStatus}), retrying`;
    case "offline":
      return `Cannot connect to ${status.host}`;
    case "timeout":
      return `Request timed out after ${status.afterSecs} s`;
    case "unexpectedResponse":
      return "Unexpected API response — LimitBar may need an update";
    case "apiKeyMode":
      return "API key mode: subscription limits do not apply";
    case "noLimitsReported":
      return "No limits reported for this plan";
    case "notRunning":
      return `Open ${displayName} to read its limits`;
    case "secureStorage":
      return "The agent protects this token in secure storage — automatic access is unavailable";
    case "missingScope":
      return "The token needs user:profile permission to read limits — sign in through Claude";
    case "unsupportedAuthMode":
      return "This authentication method is not supported yet";
    case "credentialsChanged":
      return "Credentials changed — refresh usage";
  }
}

function atTime(epochSeconds: number): string {
  const when = new Date(epochSeconds * 1000);
  return `at ${when.getHours().toString().padStart(2, "0")}:${when
    .getMinutes()
    .toString()
    .padStart(2, "0")}`;
}

/** Compact, human duration: `2 h 14 min`, `45 min`, `30 s`. */
export function formatDuration(totalSeconds: number): string {
  const secs = Math.max(0, Math.floor(totalSeconds));
  if (secs < 60) return `${secs} s`;

  const minutes = Math.floor(secs / 60);
  if (minutes < 60) return `${minutes} min`;

  const hours = Math.floor(minutes / 60);
  const remainingMinutes = minutes % 60;
  if (hours < 24) {
    return remainingMinutes === 0 ? `${hours} h` : `${hours} h ${remainingMinutes} min`;
  }

  const days = Math.floor(hours / 24);
  const remainingHours = hours % 24;
  return remainingHours === 0 ? `${days} d` : `${days} d ${remainingHours} h`;
}
