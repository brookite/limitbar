//! Desktop shell. Everything interesting lives in `limitbar-core`; this file only
//! wires it to a window and exposes it to the frontend.

use limitbar_core::model::{Snapshot, INTERVAL_CHOICES};
use limitbar_core::providers::Monitor;
use limitbar_core::settings::{self, Settings};
use tauri::{Manager, State};
use tokio::sync::Mutex;

mod window_state;

struct AppState {
    monitor: Mutex<Monitor>,
}

#[tauri::command]
async fn fetch_limits(state: State<'_, AppState>) -> Result<Snapshot, String> {
    let mut monitor = state.monitor.lock().await;
    Ok(monitor.refresh().await)
}

#[tauri::command]
fn load_settings() -> Settings {
    settings::load()
}

#[tauri::command]
fn save_settings(settings: Settings) -> Result<Settings, String> {
    let sanitized = settings.sanitized();
    settings::save(&sanitized).map_err(|err| err.to_string())?;
    Ok(sanitized)
}

/// The refresh periods the picker offers, so the menu and the validation floor
/// can never drift apart.
#[tauri::command]
fn interval_choices() -> Vec<u64> {
    INTERVAL_CHOICES.to_vec()
}

#[tauri::command]
fn set_always_on_top(window: tauri::Window, pinned: bool) -> Result<(), String> {
    window
        .set_always_on_top(pinned)
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn close_window(window: tauri::Window) -> Result<(), String> {
    window.close().map_err(|err| err.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let saved = settings::load();
            let tracker = window_state::Tracker::default();
            if let Some(webview) = app.get_webview_window("main") {
                let window = webview.as_ref().window();
                if let Err(error) = window_state::restore(&window) {
                    eprintln!("limitbar: could not restore window state: {error}");
                    let _ = window.center();
                }
                tracker.remember(&window);
                let _ = window.set_always_on_top(saved.always_on_top);
                window.show()?;
            }
            app.manage(tracker);
            app.manage(AppState {
                monitor: Mutex::new(Monitor::new(limitbar_core::default_providers())),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            let Some(tracker) = window.try_state::<window_state::Tracker>() else {
                return;
            };
            match event {
                tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) => {
                    tracker.remember(window)
                }
                tauri::WindowEvent::CloseRequested { .. } => tracker.save(window),
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            fetch_limits,
            load_settings,
            save_settings,
            interval_choices,
            set_always_on_top,
            close_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running limitbar");
}
