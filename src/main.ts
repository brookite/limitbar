/**
 * Bootstrap and the refresh loop.
 *
 * Two clocks run here: the network poll on the chosen period, and a one-second
 * local tick that keeps every "resets in …" countdown moving without touching the
 * network. Polling stops entirely while the window is hidden, and resumes with one
 * immediate fetch when it comes back.
 */

import {
  fetchLimits,
  intervalChoices,
  loadSettings,
  saveSettings,
  setAlwaysOnTop,
} from "./api";
import type { AgentSnapshot, Settings, Snapshot } from "./model";
import { createSection, type SectionView } from "./ui/agent-section";
import { createIntervalPicker, MANUAL } from "./ui/interval-picker";
import { createTitlebar } from "./ui/titlebar";
import { formatDuration, ui } from "./ui/strings";
import "./style.css";

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) throw new Error("#app is missing from index.html");

let settings: Settings = { intervalSecs: 45, alwaysOnTop: false };
let pollTimer: number | undefined;
let inFlight = false;
let lastFetchedAt: number | null = null;

const sections = new Map<string, SectionView>();

const body = document.createElement("main");
body.className = "sections";

const empty = document.createElement("p");
empty.className = "empty";
empty.textContent = ui.noAgents;
empty.hidden = true;

// In manual mode the numbers age on screen until you ask for more, so their age
// has to be visible. On a timer it is never more than one period old, and saying
// so would only add noise.
const freshness = document.createElement("footer");
freshness.className = "freshness";
freshness.hidden = true;

const titlebar = createTitlebar({
  pinned: false,
  onPinChange: (pinned) => void persist({ ...settings, alwaysOnTop: pinned }),
  onRefreshNow: () => void refresh(),
});

app.append(titlebar.root, body, empty, freshness);

void start();

async function start(): Promise<void> {
  settings = await loadSettings();

  // The window may have been restored pinned; keep the button and the window in step.
  if (settings.alwaysOnTop) await setAlwaysOnTop(true);

  const choices = await intervalChoices();
  const picker = createIntervalPicker(choices, settings.intervalSecs, (seconds) => {
    void persist({ ...settings, intervalSecs: seconds }).then(() => {
      schedule();
      renderFreshness();
    });
  });
  titlebar.controls.prepend(picker.root);

  // Rebuild the titlebar's pin state from disk rather than assuming it was off.
  if (settings.alwaysOnTop) {
    const pin = titlebar.root.querySelector<HTMLButtonElement>(".iconbtn--pin");
    pin?.classList.add("is-active");
    pin?.setAttribute("aria-pressed", "true");
  }

  document.addEventListener("visibilitychange", onVisibilityChange);
  window.setInterval(tick, 1000);

  // Manual means manual, including at launch: starting the widget must not be a
  // way to send a request you did not ask for. The footer says so until you do.
  if (settings.intervalSecs === MANUAL) {
    renderFreshness();
  } else {
    await refresh();
  }
  schedule();
}

async function persist(next: Settings): Promise<void> {
  // A failed write must not cost the user the change they just made: the setting
  // still applies for this session, it simply will not be remembered.
  try {
    settings = await saveSettings(next);
  } catch (error) {
    console.error("limitbar: could not save settings", error);
    settings = next;
  }
}

function schedule(): void {
  stop();
  if (settings.intervalSecs === MANUAL) return;
  pollTimer = window.setInterval(() => void refresh(), settings.intervalSecs * 1000);
}

function stop(): void {
  if (pollTimer !== undefined) window.clearInterval(pollTimer);
  pollTimer = undefined;
}

function onVisibilityChange(): void {
  if (document.hidden) {
    // Nobody is looking; there is no reason to keep asking the APIs.
    stop();
    return;
  }
  // Coming back into view is not a request in manual mode.
  if (settings.intervalSecs !== MANUAL) void refresh();
  schedule();
}

async function refresh(): Promise<void> {
  // The backend holds throttled agents back on its own, but there is no point
  // queueing a second call behind a slow one either.
  if (inFlight) return;
  inFlight = true;
  titlebar.setBusy(true);
  try {
    const snapshot = await fetchLimits();
    lastFetchedAt = snapshot.fetchedAt;
    render(snapshot);
  } catch (error) {
    console.error("limitbar: refresh failed", error);
  } finally {
    inFlight = false;
    titlebar.setBusy(false);
  }
}

function render(snapshot: Snapshot): void {
  const now = Math.floor(Date.now() / 1000);
  const seen = new Set<string>();

  for (const agent of snapshot.agents) {
    seen.add(agent.id);
    upsert(agent, now);
  }

  for (const [id, section] of sections) {
    if (!seen.has(id)) {
      section.root.remove();
      sections.delete(id);
    }
  }

  empty.hidden = snapshot.agents.length > 0;
  renderFreshness();
}

function renderFreshness(): void {
  freshness.hidden = settings.intervalSecs !== MANUAL;
  if (freshness.hidden) return;

  if (lastFetchedAt === null) {
    freshness.textContent = ui.neverUpdated;
    return;
  }
  const age = Math.floor(Date.now() / 1000) - lastFetchedAt;
  freshness.textContent =
    age < 60 ? ui.lastUpdatedJustNow : ui.lastUpdated(formatDuration(age));
}

function upsert(agent: AgentSnapshot, now: number): void {
  const existing = sections.get(agent.id);
  if (existing) {
    existing.update(agent, now);
    body.append(existing.root);
    return;
  }
  const created = createSection(agent);
  sections.set(agent.id, created);
  body.append(created.root);
}

function tick(): void {
  if (document.hidden) return;
  const now = Math.floor(Date.now() / 1000);
  for (const section of sections.values()) section.tick(now);
  renderFreshness();
}
