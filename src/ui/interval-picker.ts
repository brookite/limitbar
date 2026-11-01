/** The refresh-period control: a custom themed dropdown matching the widget UI. */

import { ui } from "./strings";

/** The backend sends 0 to mean "only when asked". */
export const MANUAL = 0;

export interface IntervalPickerView {
  readonly root: HTMLElement;
  setValue(seconds: number): void;
}

export function createIntervalPicker(
  choices: number[],
  current: number,
  onChange: (seconds: number) => void,
): IntervalPickerView {
  const root = document.createElement("div");
  root.className = "interval";

  const trigger = document.createElement("button");
  trigger.type = "button";
  trigger.className = "interval__trigger";
  trigger.title = ui.intervalLabel;
  trigger.setAttribute("aria-label", ui.intervalLabel);
  trigger.setAttribute("aria-haspopup", "listbox");
  trigger.setAttribute("aria-expanded", "false");

  const label = document.createElement("span");
  label.className = "interval__value";

  const chevron = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  chevron.setAttribute("viewBox", "0 0 12 12");
  chevron.setAttribute("width", "10");
  chevron.setAttribute("height", "10");
  chevron.setAttribute("fill", "none");
  chevron.setAttribute("stroke", "currentColor");
  chevron.setAttribute("stroke-width", "1.5");
  chevron.setAttribute("stroke-linecap", "round");
  chevron.setAttribute("stroke-linejoin", "round");
  chevron.setAttribute("aria-hidden", "true");
  chevron.classList.add("interval__chevron");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", "M3 4.5 6 7.5 9 4.5");
  chevron.append(path);

  trigger.append(label, chevron);

  const menu = document.createElement("div");
  menu.className = "interval__menu";
  menu.setAttribute("role", "listbox");
  menu.hidden = true;

  let selected = current;
  let open = false;
  const items = new Map<number, HTMLButtonElement>();

  function formatLabel(seconds: number): string {
    return seconds === MANUAL ? ui.manual : ui.seconds(seconds);
  }

  function syncSelection(seconds: number): void {
    selected = seconds;
    label.textContent = formatLabel(seconds);
    for (const [value, btn] of items) {
      const active = value === seconds;
      btn.classList.toggle("is-selected", active);
      btn.setAttribute("aria-selected", String(active));
    }
  }

  function setOpen(next: boolean): void {
    if (open === next) return;
    open = next;
    menu.hidden = !open;
    root.classList.toggle("is-open", open);
    trigger.setAttribute("aria-expanded", String(open));
    if (open) {
      items.get(selected)?.focus();
    }
  }

  for (const seconds of choices) {
    const option = document.createElement("button");
    option.type = "button";
    option.className = "interval__option";
    option.setAttribute("role", "option");
    option.textContent = formatLabel(seconds);
    option.addEventListener("click", () => {
      const changed = selected !== seconds;
      syncSelection(seconds);
      setOpen(false);
      trigger.focus();
      if (changed) onChange(seconds);
    });
    items.set(seconds, option);
    menu.append(option);
  }

  syncSelection(current);

  trigger.addEventListener("click", () => setOpen(!open));

  root.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && open) {
      event.preventDefault();
      setOpen(false);
      trigger.focus();
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (!open) {
        setOpen(true);
        return;
      }
      const index = choices.indexOf(selected);
      const delta = event.key === "ArrowDown" ? 1 : -1;
      const nextIndex = (index + delta + choices.length) % choices.length;
      const nextValue = choices[nextIndex]!;
      syncSelection(nextValue);
      items.get(nextValue)?.focus();
      onChange(nextValue);
    }
  });

  document.addEventListener("pointerdown", (event) => {
    if (open && !root.contains(event.target as Node)) {
      setOpen(false);
    }
  });

  window.addEventListener("blur", () => setOpen(false));

  root.append(trigger, menu);

  return {
    root,
    setValue(seconds) {
      syncSelection(seconds);
    },
  };
}
