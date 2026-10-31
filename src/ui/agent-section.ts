/**
 * One agent's block: a heading with a health dot, then its gauges — or, when there
 * is nothing to draw, a sentence saying why.
 *
 * The section is rebuilt in place rather than recreated so the window does not jump
 * as an agent flips between working and failing.
 */

import type { AgentSnapshot, Gauge, UsageDetail } from "../model";
import { healthOf } from "../model";
import { createGauge, type GaugeView } from "./gauge";
import { detailLabel, detailValue, formatDuration, statusMessage, ui } from "./strings";

export interface SectionView {
  readonly root: HTMLElement;
  update(agent: AgentSnapshot, now: number): void;
  tick(now: number): void;
}

export function createSection(agent: AgentSnapshot, options: {
  collapsed: boolean;
  onCollapsedChange(collapsed: boolean): void;
}): SectionView {
  const root = document.createElement("section");
  root.className = "agent";

  const head = document.createElement("button");
  head.type = "button";
  head.className = "agent__head";

  const dot = document.createElement("span");
  dot.className = "agent__dot";

  const name = document.createElement("span");
  name.className = "agent__name";

  const note = document.createElement("span");
  note.className = "agent__note";

  const chevron = document.createElement("span");
  chevron.className = "agent__chevron";
  chevron.textContent = "›";
  chevron.setAttribute("aria-hidden", "true");
  head.append(dot, name, note, chevron);

  const content = document.createElement("div");
  content.className = "agent__content";
  content.id = `agent-content-${agent.id}`;
  head.setAttribute("aria-controls", content.id);
  let collapsed = options.collapsed;
  function renderCollapsed(): void {
    content.hidden = collapsed;
    head.setAttribute("aria-expanded", String(!collapsed));
    const label = collapsed ? ui.expandAgent(name.textContent ?? "") : ui.collapseAgent(name.textContent ?? "");
    head.setAttribute("aria-label", label);
    head.title = label;
  }
  head.addEventListener("click", () => {
    collapsed = !collapsed;
    renderCollapsed();
    options.onCollapsedChange(collapsed);
  });

  const bars = document.createElement("div");
  bars.className = "agent__bars";

  const message = document.createElement("p");
  message.className = "agent__message";

  const extras = document.createElement("details");
  extras.className = "agent__extras";
  const summary = document.createElement("summary");
  summary.textContent = ui.additionalInfo;
  const details = document.createElement("dl");
  details.className = "agent__details";
  extras.append(summary, details);
  let detailRows: { entry: UsageDetail; row: HTMLElement; expiry: HTMLElement }[] = [];
  content.append(bars, extras, message);
  root.append(head, content);

  const gauges = new Map<string, GaugeView>();

  const view: SectionView = {
    root,
    update(next, now) {
      name.textContent = next.displayName;
      renderCollapsed();
      root.dataset.health = healthOf(next.status);

      const parts: string[] = [];
      if (next.note) parts.push(next.note);
      if (next.status.kind === "stale") parts.push(staleness(next.status.lastOkAt, now));
      note.textContent = parts.join(" · ");

      syncGauges(bars, gauges, next.gauges);
      detailRows = (next.details ?? []).map(entry => {
        const row = document.createElement("div");
        row.className = "agent__detail";
        const label = document.createElement("dt");
        label.textContent = detailLabel(entry.label);
        label.title = label.textContent;
        const value = document.createElement("dd");
        value.textContent = detailValue(entry.value);
        const expiry = document.createElement("dd");
        expiry.className = "agent__detail-expiry";
        row.append(label, value, expiry);
        return { entry, row, expiry };
      });
      details.replaceChildren(...detailRows.map(d => d.row));

      const text = statusMessage(next.status, next.displayName);
      message.textContent = text;
      // A transient failure keeps the dimmed bars *and* explains itself; a
      // configuration problem has no bars, so the sentence stands alone.
      message.hidden = text === "";
      view.tick(now);
    },
    tick(now) {
      for (const gauge of gauges.values()) gauge.tick(now);
      for (const { entry, row, expiry } of detailRows) {
        row.hidden = entry.expiresAt !== null && entry.expiresAt <= now;
        expiry.hidden = entry.expiresAt === null;
        expiry.textContent = entry.expiresAt === null ? "" : ui.expiresIn(formatDuration(entry.expiresAt - now));
      }
      extras.hidden = detailRows.every(d => d.row.hidden);
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
