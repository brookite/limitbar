/**
 * One agent's block: a heading with a health dot, then its gauges — or, when there
 * is nothing to draw, a sentence saying why.
 *
 * The section is rebuilt in place rather than recreated so the window does not jump
 * as an agent flips between working and failing.
 */

import type { AgentSnapshot, Gauge } from "../model";
import { healthOf } from "../model";
import { createGauge, type GaugeView } from "./gauge";
import { formatDuration, statusMessage, ui } from "./strings";

export interface SectionView {
  readonly root: HTMLElement;
  update(agent: AgentSnapshot, now: number): void;
  tick(now: number): void;
}

export function createSection(agent: AgentSnapshot): SectionView {
  const root = document.createElement("section");
  root.className = "agent";

  const head = document.createElement("header");
  head.className = "agent__head";

  const dot = document.createElement("span");
  dot.className = "agent__dot";

  const name = document.createElement("span");
  name.className = "agent__name";

  const note = document.createElement("span");
  note.className = "agent__note";

  head.append(dot, name, note);

  const bars = document.createElement("div");
  bars.className = "agent__bars";

  const message = document.createElement("p");
  message.className = "agent__message";

  root.append(head, bars, message);

  const gauges = new Map<string, GaugeView>();

  const view: SectionView = {
    root,
    update(next, now) {
      name.textContent = next.displayName;
      root.dataset.health = healthOf(next.status);

      const parts: string[] = [];
      if (next.note) parts.push(next.note);
      if (next.status.kind === "stale") parts.push(staleness(next.status.lastOkAt, now));
      note.textContent = parts.join(" · ");

      syncGauges(bars, gauges, next.gauges);

      const text = statusMessage(next.status, next.displayName);
      message.textContent = text;
      // A transient failure keeps the dimmed bars *and* explains itself; a
      // configuration problem has no bars, so the sentence stands alone.
      message.hidden = text === "";
      view.tick(now);
    },
    tick(now) {
      for (const gauge of gauges.values()) gauge.tick(now);
    },
  };

  view.update(agent, Math.floor(Date.now() / 1000));
  return view;
}

function staleness(lastOkAt: number, now: number): string {
  const age = now - lastOkAt;
  return age < 60 ? ui.updatedJustNow : ui.updatedAgo(formatDuration(age));
}

/** Reconciles the live gauge list against the incoming one, reusing what it can. */
function syncGauges(host: HTMLElement, views: Map<string, GaugeView>, incoming: Gauge[]): void {
  const seen = new Set<string>();

  for (const gauge of incoming) {
    seen.add(gauge.id);
    const existing = views.get(gauge.id);
    if (existing) {
      existing.update(gauge);
      host.append(existing.root);
    } else {
      const created = createGauge(gauge);
      views.set(gauge.id, created);
      host.append(created.root);
    }
  }

  for (const [id, view] of views) {
    if (!seen.has(id)) {
      view.root.remove();
      views.delete(id);
    }
  }
}
