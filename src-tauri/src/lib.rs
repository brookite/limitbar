//! Desktop shell. Everything interesting lives in `limitbar-core`; this file only
//! wires it to a window and exposes it to the frontend.

use limitbar_core::model::{AgentSnapshot, AgentStatus, Severity, Snapshot, INTERVAL_CHOICES};
use limitbar_core::providers::Monitor;
use limitbar_core::settings::{self, Settings};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;

mod window_state;

const TRAY_ID: &str = "main-tray";

struct AppState {
    monitor: Mutex<Monitor>,
    last_snapshot: Mutex<Option<Snapshot>>,
}

#[tauri::command]
async fn fetch_limits(app: AppHandle, state: State<'_, AppState>) -> Result<Snapshot, String> {
    let snapshot = {
        let mut monitor = state.monitor.lock().await;
        monitor.refresh().await
    };
    *state.last_snapshot.lock().await = Some(snapshot.clone());
    update_tray_menu(&app, Some(&snapshot));
    Ok(snapshot)
}

#[tauri::command]
fn load_settings() -> Settings {
    settings::load()
}

#[tauri::command]
async fn save_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let sanitized = settings.sanitized();
    settings::save(&sanitized).map_err(|err| err.to_string())?;
    let snapshot = state.last_snapshot.lock().await.clone();
    update_tray_menu(&app, snapshot.as_ref());
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

fn format_duration(total_seconds: i64) -> String {
    let secs = total_seconds.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem_min = minutes % 60;
    if hours < 24 {
        return if rem_min == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {rem_min}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {rem_hours}h")
    }
}

fn tray_status_dot(status: &AgentStatus, gauges: &[limitbar_core::model::Gauge]) -> &'static str {
    match status {
        AgentStatus::Ok => {
            if gauges.iter().any(|g| g.severity == Severity::Critical) {
                "🔴"
            } else if gauges.iter().any(|g| g.severity == Severity::Warning) {
                "🟡"
            } else {
                "🟢"
            }
        }
        AgentStatus::Stale { .. } => "🟡",
        AgentStatus::NotInstalled
        | AgentStatus::NotSignedIn
        | AgentStatus::ApiKeyMode
        | AgentStatus::NoLimitsReported
        | AgentStatus::NotRunning
        | AgentStatus::SecureStorage => "⚪",
        _ => "🔴",
    }
}

fn tray_status_summary(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Ok => "",
        AgentStatus::Stale { because, .. } => tray_status_summary(because),
        AgentStatus::NotInstalled => "Not installed",
        AgentStatus::NotSignedIn => "Not signed in",
        AgentStatus::SessionExpired => "Session expired",
        AgentStatus::Unauthorized => "Unauthorized",
        AgentStatus::RateLimited { .. } => "Rate limited",
        AgentStatus::ServiceUnavailable { .. } => "Service unavailable",
        AgentStatus::Offline { .. } => "Offline",
        AgentStatus::Timeout { .. } => "Request timed out",
        AgentStatus::UnexpectedResponse => "Unexpected response",
        AgentStatus::ApiKeyMode => "API key mode",
        AgentStatus::NoLimitsReported => "No limits reported",
        AgentStatus::NotRunning => "Not running",
        AgentStatus::SecureStorage => "Protected in secure storage",
        AgentStatus::MissingScope => "Missing user:profile scope",
        AgentStatus::UnsupportedAuthMode => "Unsupported auth mode",
        AgentStatus::CredentialsChanged => "Credentials changed",
    }
}

fn ordered_agents(agents: &[AgentSnapshot]) -> Vec<AgentSnapshot> {
    let order = settings::load().agent_order;
    let mut list = agents.to_vec();
    list.sort_by_key(|a| {
        order
            .iter()
            .position(|id| id == &a.id)
            .unwrap_or(order.len())
    });
    list
}

fn build_tray_menu(
    app: &AppHandle,
    snapshot: Option<&Snapshot>,
) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;

    let open_item = MenuItem::with_id(app, "open", "Open LimitBar", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "hide", "Hide LimitBar", true, None::<&str>)?;
    let refresh_item = MenuItem::with_id(app, "refresh", "Refresh Now", true, None::<&str>)?;
    menu.append(&open_item)?;
    menu.append(&hide_item)?;
    menu.append(&refresh_item)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    match snapshot {
        Some(snap) if !snap.agents.is_empty() => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(snap.fetched_at);

            for (idx, agent) in ordered_agents(&snap.agents).iter().enumerate() {
                if idx > 0 {
                    menu.append(&PredefinedMenuItem::separator(app)?)?;
                }
                let dot = tray_status_dot(&agent.status, &agent.gauges);
                let header_label = match &agent.note {
                    Some(note) if !note.is_empty() => {
                        format!("{dot} {} ({note})", agent.display_name)
                    }
                    _ => format!("{dot} {}", agent.display_name),
                };
                let header = MenuItem::with_id(
                    app,
                    format!("agent:{}", agent.id),
                    header_label,
                    true,
                    None::<&str>,
                )?;
                menu.append(&header)?;

                for gauge in &agent.gauges {
                    let pct = format!("{:.1}", gauge.used_percent);
                    let pct = pct.strip_suffix(".0").unwrap_or(&pct);
                    let reset_suffix = match gauge.resets_at {
                        Some(at) if at > now => format!("  ·  {}", format_duration(at - now)),
                        _ => String::new(),
                    };
                    let line = format!("    {}: {}%{}", gauge.label, pct, reset_suffix);
                    let item = MenuItem::with_id(
                        app,
                        format!("gauge:{}:{}", agent.id, gauge.id),
                        line,
                        true,
                        None::<&str>,
                    )?;
                    menu.append(&item)?;
                }

                let status_text = tray_status_summary(&agent.status);
                if !status_text.is_empty() {
                    let item = MenuItem::with_id(
                        app,
                        format!("status:{}", agent.id),
                        format!("    {status_text}"),
                        false,
                        None::<&str>,
                    )?;
                    menu.append(&item)?;
                }
            }
        }
        _ => {
            let loading =
                MenuItem::with_id(app, "loading", "Loading limits…", false, None::<&str>)?;
            menu.append(&loading)?;
        }
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    menu.append(&quit_item)?;

    Ok(menu)
}

fn update_tray_menu(app: &AppHandle, snapshot: Option<&Snapshot>) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_tray_menu(app, snapshot) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let handle = app.handle();
    let menu = build_tray_menu(handle, None)?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("LimitBar")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window(app),
            "hide" => hide_main_window(app),
            "refresh" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = app.try_state::<AppState>() {
                        let snapshot = {
                            let mut monitor = state.monitor.lock().await;
                            monitor.refresh().await
                        };
                        *state.last_snapshot.lock().await = Some(snapshot.clone());
                        update_tray_menu(&app, Some(&snapshot));
                        let _ = app.emit("limits-updated", &snapshot);
                    }
                });
            }
            "quit" => {
                if let Some(webview) = app.get_webview_window("main") {
                    let window = webview.as_ref().window();
                    if let Some(tracker) = app.try_state::<window_state::Tracker>() {
                        tracker.save(&window);
                    }
                }
                app.exit(0);
            }
            other if other.starts_with("agent:") || other.starts_with("gauge:") => {
                show_main_window(app);
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
                last_snapshot: Mutex::new(None),
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
