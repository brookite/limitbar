/** One limit as a labelled progress bar. Knows nothing about which agent it came from. */

import type { Gauge } from "../model";
import { formatDuration, ui } from "./strings";

export interface GaugeView {
  readonly root: HTMLElement;
  update(gauge: Gauge): void;
  /** Re-renders only the countdown, so the reset time stays live between fetches. */
  tick(now: number): void;
}

export function createGauge(gauge: Gauge): GaugeView {
  const root = document.createElement("div");
  root.className = "gauge";

  const head = document.createElement("div");
  head.className = "gauge__head";

  const label = document.createElement("span");
  label.className = "gauge__label";

  const percent = document.createElement("span");
  percent.className = "gauge__percent";

  head.append(label, percent);

  const track = document.createElement("div");
  track.className = "gauge__track";
  const fill = document.createElement("div");
  fill.className = "gauge__fill";
  track.append(fill);

  const reset = document.createElement("div");
  reset.className = "gauge__reset";

  root.append(head, track, reset);

  let current = gauge;

  const view: GaugeView = {
    root,
    update(next) {
      current = next;
      label.textContent = next.label;
      percent.textContent = `${formatPercent(next.usedPercent)}%`;
      root.dataset.severity = next.severity;
      fill.style.width = `${Math.min(100, Math.max(0, next.usedPercent))}%`;
      track.setAttribute("role", "progressbar");
      track.setAttribute("aria-valuenow", String(Math.round(next.usedPercent)));
      track.setAttribute("aria-valuemin", "0");
      track.setAttribute("aria-valuemax", "100");
      track.setAttribute("aria-label", next.label);
      view.tick(Math.floor(Date.now() / 1000));
    },
    tick(now) {
      if (current.resetsAt === null) {
        reset.textContent = "";
        return;
      }
      const remaining = current.resetsAt - now;
      reset.textContent =
        remaining <= 0 ? ui.resetPassed : ui.resetsIn(formatDuration(remaining));
    },
  };

  view.update(gauge);
  return view;
}

/** Whole numbers read better on a small bar; a fraction only shows below 10%. */
function formatPercent(value: number): string {
  return value < 10 ? value.toFixed(1).replace(/\.0$/, "") : String(Math.round(value));
}
