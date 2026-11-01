/**
 * The window has no system decoration, so it brings its own: a drag strip, the
 * refresh picker, a pin toggle and a close button.
 */

import { closeWindow, minimizeWindow, setAlwaysOnTop } from "../api";
import { ui } from "./strings";

export interface TitlebarOptions {
  pinned: boolean;
  onPinChange(pinned: boolean): void;
  onRefreshNow(): void;
}

export interface TitlebarView {
  readonly root: HTMLElement;
  /** Slot for the interval picker, filled by the caller. */
  readonly controls: HTMLElement;
  setBusy(busy: boolean): void;
}

export function createTitlebar(options: TitlebarOptions): TitlebarView {
  const root = document.createElement("header");
  root.className = "titlebar";
  // Anywhere not covered by a control drags the window.
  root.setAttribute("data-tauri-drag-region", "");

  const title = document.createElement("span");
  title.className = "titlebar__title";
  title.setAttribute("data-tauri-drag-region", "");
  title.textContent = "LimitBar";

  const controls = document.createElement("div");
  controls.className = "titlebar__controls";

  const refresh = iconButton("refresh", ui.refreshNow, refreshIcon());
  refresh.addEventListener("click", () => options.onRefreshNow());

  let pinned = options.pinned;
  const pin = iconButton("pin", pinned ? ui.pinOn : ui.pinOff, pinIcon());
  pin.setAttribute("aria-pressed", String(pinned));
  pin.classList.toggle("is-active", pinned);
  pin.addEventListener("click", async () => {
    pinned = !pinned;
    pin.setAttribute("aria-pressed", String(pinned));
    pin.classList.toggle("is-active", pinned);
    pin.title = pinned ? ui.pinOn : ui.pinOff;
    await setAlwaysOnTop(pinned);
    options.onPinChange(pinned);
  });

  const minimize = iconButton("minimize", ui.minimize, minimizeIcon());
  minimize.addEventListener("click", () => void minimizeWindow());

  const close = iconButton("close", ui.close, closeIcon());
  close.addEventListener("click", () => void closeWindow());

  controls.append(refresh, pin, minimize, close);
  root.append(title, controls);

  return {
    root,
    controls,
    setBusy(busy) {
      refresh.classList.toggle("is-busy", busy);
      refresh.disabled = busy;
    },
  };
}

function iconButton(name: string, title: string, icon: SVGElement): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = `iconbtn iconbtn--${name}`;
  button.title = title;
  button.setAttribute("aria-label", title);
  button.append(icon);
  return button;
}

function svg(paths: string[], extra: Record<string, string> = {}): SVGElement {
  const NS = "http://www.w3.org/2000/svg";
  const node = document.createElementNS(NS, "svg");
  node.setAttribute("viewBox", "0 0 16 16");
  node.setAttribute("width", "13");
  node.setAttribute("height", "13");
  node.setAttribute("fill", "none");
  node.setAttribute("stroke", "currentColor");
  node.setAttribute("stroke-width", "1.5");
  node.setAttribute("stroke-linecap", "round");
  node.setAttribute("stroke-linejoin", "round");
  node.setAttribute("aria-hidden", "true");
  for (const [key, value] of Object.entries(extra)) node.setAttribute(key, value);
  for (const d of paths) {
    const path = document.createElementNS(NS, "path");
    path.setAttribute("d", d);
    node.append(path);
  }
  return node;
}

const refreshIcon = () =>
  svg(["M13.5 8a5.5 5.5 0 1 1-1.6-3.9", "M13.5 2.5v3h-3"]);

// A pushpin: head, shaft, point.
const pinIcon = () => svg(["M9.8 1.8 14.2 6.2", "M11 3 8.2 5.2 4 6.4l5.6 5.6 1.2-4.2L13 5.2", "M6.6 9.4 2.5 13.5"]);

const minimizeIcon = () => svg(["M3.5 8.5h9"]);

const closeIcon = () => svg(["M3.5 3.5 12.5 12.5", "M12.5 3.5 3.5 12.5"]);
