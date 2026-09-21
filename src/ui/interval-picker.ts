/** The refresh-period control: a deliberately tiny select. */

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
  const root = document.createElement("label");
  root.className = "interval";
  root.title = ui.intervalLabel;

  const select = document.createElement("select");
  select.className = "interval__select";
  select.setAttribute("aria-label", ui.intervalLabel);

  for (const seconds of choices) {
    const option = document.createElement("option");
    option.value = String(seconds);
    option.textContent = seconds === MANUAL ? ui.manual : ui.seconds(seconds);
    select.append(option);
  }
  select.value = String(current);

  select.addEventListener("change", () => onChange(Number(select.value)));

  root.append(select);

  return {
    root,
    setValue(seconds) {
      select.value = String(seconds);
    },
  };
}
