//! Geometry is owned by the shell, in a separate file to avoid racing UI preferences.
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{Manager, PhysicalPosition, PhysicalSize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Rect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl Rect {
    fn fitted(&self, area: &Self, minimum: (u32, u32)) -> Self {
        let width = self.width.max(minimum.0).min(area.width);
        let height = self.height.max(minimum.1).min(area.height);
        Self {
            x: i64::from(self.x).clamp(
                i64::from(area.x),
                i64::from(area.x) + i64::from(area.width - width),
            ) as i32,
            y: i64::from(self.y).clamp(
                i64::from(area.y),
                i64::from(area.y) + i64::from(area.height - height),
            ) as i32,
            width,
            height,
        }
    }

    fn contains(&self, other: &Self) -> bool {
        other.width > 0
            && other.height > 0
            && other.x >= self.x
            && other.y >= self.y
            && i64::from(other.x) + i64::from(other.width)
                <= i64::from(self.x) + i64::from(self.width)
            && i64::from(other.y) + i64::from(other.height)
                <= i64::from(self.y) + i64::from(self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Screen {
    name: Option<String>,
    bounds: Rect,
    work_area: Rect,
    scale: f64,
    primary: bool,
}

impl Screen {
    fn from_monitor(monitor: &tauri::Monitor, primary: Option<&tauri::Monitor>) -> Self {
        Self {
            name: monitor.name().cloned(),
            bounds: Rect {
                x: monitor.position().x,
                y: monitor.position().y,
                width: monitor.size().width,
                height: monitor.size().height,
            },
            work_area: Rect {
                x: monitor.work_area().position.x,
                y: monitor.work_area().position.y,
                width: monitor.work_area().size.width,
                height: monitor.work_area().size.height,
            },
            scale: monitor.scale_factor(),
            primary: primary.is_some_and(|p| p.position() == monitor.position()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    bounds: Rect,
    screens: Vec<Screen>,
}

impl WindowState {
    fn can_restore(&self, screens: &[Screen]) -> bool {
        // Monitor enumeration order is not stable across launches.
        !screens.is_empty()
            && self.screens.len() == screens.len()
            && self.screens.iter().all(|screen| screens.contains(screen))
            && screens
                .iter()
                .any(|screen| screen.work_area.contains(&self.bounds))
    }
}

fn state_path() -> Option<std::path::PathBuf> {
    limitbar_core::settings::settings_path().map(|p| p.with_file_name("window-state.json"))
}

fn screens(window: &tauri::Window) -> tauri::Result<Vec<Screen>> {
    let primary = window.primary_monitor()?;
    Ok(window
        .available_monitors()?
        .iter()
        .map(|monitor| Screen::from_monitor(monitor, primary.as_ref()))
        .collect())
}

fn capture(window: &tauri::Window) -> Option<WindowState> {
    if window.is_minimized().ok()? || window.is_maximized().ok()? {
        return None;
    }
    let position = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    let screens = screens(window).ok()?;
    if screens.is_empty() {
        return None;
    }
    Some(WindowState {
        bounds: Rect {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        },
        screens,
    })
}

pub fn restore(window: &tauri::Window) -> tauri::Result<()> {
    let screens = screens(window)?;
    let config = window.app_handle().config();
    let Some(defaults) = config
        .app
        .windows
        .iter()
        .find(|w| w.label == window.label())
    else {
        return Ok(());
    };
    let saved = state_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|raw| serde_json::from_slice::<WindowState>(&raw).ok());
    let (bounds, screen) = if let Some(saved) = saved.filter(|s| s.can_restore(&screens)) {
        let screen = screens
            .iter()
            .find(|s| s.work_area.contains(&saved.bounds))
            .unwrap();
        (saved.bounds, screen)
    } else if let Some(screen) = screens
        .iter()
        .find(|screen| screen.primary)
        .or(screens.first())
    {
        let width = ((defaults.width * screen.scale).round() as u32).min(screen.work_area.width);
        let height = ((defaults.height * screen.scale).round() as u32).min(screen.work_area.height);
        (
            Rect {
                x: screen.work_area.x + ((screen.work_area.width - width) / 2) as i32,
                y: screen.work_area.y + ((screen.work_area.height - height) / 2) as i32,
                width,
                height,
            },
            screen,
        )
    } else {
        return Ok(());
    };
    let minimum = (
        ((defaults.min_width.unwrap_or(0.0) * screen.scale).round() as u32)
            .min(screen.work_area.width),
        ((defaults.min_height.unwrap_or(0.0) * screen.scale).round() as u32)
            .min(screen.work_area.height),
    );
    // Account for OS minimum-size constraints before clamping the position. Even
    // a hand-edited one-pixel window near the screen edge must remain reachable.
    let bounds = bounds.fitted(&screen.work_area, minimum);
    window.set_position(PhysicalPosition::new(bounds.x, bounds.y))?;
    window.set_min_size(Some(PhysicalSize::new(minimum.0, minimum.1)))?;
    window.set_size(PhysicalSize::new(bounds.width, bounds.height))?;
    window.set_position(PhysicalPosition::new(bounds.x, bounds.y))?;
    Ok(())
}

/// Keep the last normal geometry in memory, so closing a minimized/maximized
/// window does not persist OS placeholder coordinates or a full-screen widget.
#[derive(Default)]
pub struct Tracker(Mutex<Option<WindowState>>);

impl Tracker {
    pub fn remember(&self, window: &tauri::Window) {
        if let Some(state) = capture(window) {
            *self.0.lock().unwrap() = Some(state);
        }
    }

    pub fn save(&self, window: &tauri::Window) {
        self.remember(window);
        let state = self.0.lock().unwrap();
        let Some(state) = state.as_ref() else { return };
        let Some(path) = state_path() else { return };
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_vec_pretty(state)?)?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("limitbar: could not save window state: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: i32) -> Screen {
        Screen {
            name: Some(format!("screen-{x}")),
            bounds: Rect {
                x,
                y: 0,
                width: 1920,
                height: 1080,
            },
            work_area: Rect {
                x,
                y: 0,
                width: 1920,
                height: 1040,
            },
            scale: 1.0,
            primary: x == 0,
        }
    }

    fn state() -> WindowState {
        WindowState {
            bounds: Rect {
                x: -1800,
                y: 100,
                width: 340,
                height: 430,
            },
            screens: vec![screen(-1920), screen(0)],
        }
    }

    #[test]
    fn restores_negative_coordinates_and_ignores_enumeration_order() {
        let saved = state();
        assert!(saved.can_restore(&[screen(0), screen(-1920)]));
        let raw = serde_json::to_vec(&saved).unwrap();
        assert_eq!(serde_json::from_slice::<WindowState>(&raw).unwrap(), saved);
    }

    #[test]
    fn rejects_changed_monitor_configuration() {
        let saved = state();
        assert!(!saved.can_restore(&[screen(0)]));
        assert!(!saved.can_restore(&[]));
        for change in 0..6 {
            let mut screens = saved.screens.clone();
            match change {
                0 => screens[0].bounds.width = 1280,
                1 => screens[0].bounds.x = -2000,
                2 => screens[0].scale = 1.5,
                3 => screens[0].work_area.height = 1000,
                4 => screens[0].primary = true,
                _ => screens[0].name = Some("replacement".into()),
            }
            assert!(!saved.can_restore(&screens));
        }
    }

    #[test]
    fn rejects_offscreen_zero_oversized_and_overflowing_geometry() {
        for bounds in [
            Rect {
                x: 1900,
                y: 100,
                width: 340,
                height: 430,
            },
            Rect {
                x: 100,
                y: 900,
                width: 340,
                height: 430,
            },
            Rect {
                x: 100,
                y: 100,
                width: 0,
                height: 430,
            },
            Rect {
                x: 100,
                y: 100,
                width: 340,
                height: 0,
            },
            Rect {
                x: 0,
                y: 0,
                width: 4000,
                height: 430,
            },
            Rect {
                x: i32::MAX,
                y: 100,
                width: u32::MAX,
                height: 430,
            },
        ] {
            let saved = WindowState { bounds, ..state() };
            assert!(!saved.can_restore(&saved.screens));
        }
    }

    #[test]
    fn minimum_size_cannot_push_a_restored_window_offscreen() {
        let area = screen(0).work_area;
        let tiny = Rect {
            x: 1919,
            y: 1039,
            width: 1,
            height: 1,
        };
        let fitted = tiny.fitted(&area, (280, 180));
        assert_eq!((fitted.width, fitted.height), (280, 180));
        assert_eq!((fitted.x, fitted.y), (1640, 860));
        assert!(area.contains(&fitted));

        let small_screen = Rect {
            x: -200,
            y: 0,
            width: 200,
            height: 100,
        };
        assert!(small_screen.contains(&tiny.fitted(&small_screen, (280, 180))));
    }
}
