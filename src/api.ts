/** Typed wrappers around the Rust commands. */

import { invoke } from "@tauri-apps/api/core";
import type { Settings, Snapshot } from "./model";

export const fetchLimits = (): Promise<Snapshot> => invoke<Snapshot>("fetch_limits");

export const loadSettings = (): Promise<Settings> => invoke<Settings>("load_settings");

export const saveSettings = (settings: Settings): Promise<Settings> =>
  invoke<Settings>("save_settings", { settings });

export const intervalChoices = (): Promise<number[]> => invoke<number[]>("interval_choices");

export const setAlwaysOnTop = (pinned: boolean): Promise<void> =>
  invoke("set_always_on_top", { pinned });

export const minimizeWindow = (): Promise<void> => invoke("minimize_window");

export const closeWindow = (): Promise<void> => invoke("close_window");
