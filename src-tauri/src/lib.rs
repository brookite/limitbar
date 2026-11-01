//! Desktop shell. Everything interesting lives in `limitbar-core`; this file only
//! wires it to a window and exposes it to the frontend.

use limitbar_core::model::{Snapshot, INTERVAL_CHOICES};
use limitbar_core::providers::Monitor;
use limitbar_core::settings::{self, Settings};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State};
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
fn minimize_window(window: tauri::Window) -> Result<(), String> {
    window.minimize().map_err(|err| err.to_string())
}

#[tauri::command]
fn close_window(window: tauri::Window) -> Result<(), String> {
    if let Some(tracker) = window.try_state::<window_state::Tracker>() {
        tracker.save(&window);
    }
    window.hide().map_err(|err| err.to_string())
}

fn show_main_window(app: &AppHandle) {
    if let Some(webview) = app.get_webview_window("main") {
        let window = webview.as_ref().window();
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(webview) = app.get_webview_window("main") {
        let window = webview.as_ref().window();
        if let Some(tracker) = app.try_state::<window_state::Tracker>() {
            tracker.save(&window);
        }
        let _ = window.hide();
    }
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, "open", "Open LimitBar", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "hide", "Hide LimitBar", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &hide_item, &separator, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("LimitBar")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "hide" => hide_main_window(app),
            "quit" => {
                if let Some(webview) = app.get_webview_window("main") {
                    let window = webview.as_ref().window();
                    if let Some(tracker) = app.try_state::<window_state::Tracker>() {
                        tracker.save(&window);
                    }
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(webview) = app.get_webview_window("main") {
                    let window = webview.as_ref().window();
                    let visible = window.is_visible().unwrap_or(false)
                        && !window.is_minimized().unwrap_or(false);
                    if visible {
                        hide_main_window(app);
                    } else {
                        show_main_window(app);
                    }
                }
            }
        });

    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    if std::env::var_os("GDK_BACKEND").is_none() && std::env::var_os("DISPLAY").is_some() {
        // Wayland compositors ignore client-initiated `set_always_on_top` and `set_position`.
        // Running the widget via XWayland enables both pinning and geometry restoration.
        std::env::set_var("GDK_BACKEND", "x11");
    }

    tauri::Builder::default()
        .setup(|app| {
            let saved = settings::load();
            let tracker = window_state::Tracker::default();
            if let Some(webview) = app.get_webview_window("main") {
                let window = webview.as_ref().window();
                if let Some(icon) = app.default_window_icon().cloned() {
                    let _ = window.set_icon(icon);
                }
                if let Err(error) = window_state::restore(&window) {
                    eprintln!("limitbar: could not restore window state: {error}");
                    let _ = window.center();
                }
                tracker.remember(&window);
                let _ = window.set_always_on_top(saved.always_on_top);
                window.show()?;
            }
            if let Err(error) = setup_tray(app) {
                eprintln!("limitbar: could not create tray icon: {error}");
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
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    tracker.save(window);
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            fetch_limits,
            load_settings,
            save_settings,
            interval_choices,
            set_always_on_top,
            minimize_window,
            close_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running limitbar");
}
