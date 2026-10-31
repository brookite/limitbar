//! Installation evidence is an executable or an installed app, never a config directory.
use std::path::{Path, PathBuf};

use super::creds::{first_non_empty_env, home_dir};

pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        first_non_empty_env(&["APPDATA"]).or_else(|| home_dir().map(|p| p.join("AppData/Roaming")))
    } else if cfg!(target_os = "macos") {
        home_dir().map(|p| p.join("Library/Application Support"))
    } else {
        first_non_empty_env(&["XDG_CONFIG_HOME"]).or_else(|| home_dir().map(|p| p.join(".config")))
    }
}

pub fn data_dir() -> Option<PathBuf> {
    first_non_empty_env(&["XDG_DATA_HOME"]).or_else(|| home_dir().map(|p| p.join(".local/share")))
}

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    true
}

fn launcher_exists(path: &Path) -> bool {
    if !executable(path) {
        return false;
    }
    // npm can leave a launcher after its package was removed. Check its target
    // without starting the agent (which could update itself or open a window).
    if path
        .extension()
        .is_some_and(|ext| ext == "ps1" || ext == "cmd" || ext == "bat")
    {
        if let Ok(raw) = std::fs::read_to_string(path) {
            let normalized = raw.replace('\\', "/");
            if let Some(start) = normalized.find("node_modules/") {
                let target = normalized[start..]
                    .split(['"', '\'', '\r', '\n'])
                    .next()
                    .unwrap_or_default();
                return path.parent().is_some_and(|dir| dir.join(target).is_file());
            }
        }
    }
    true
}

fn command_in(dirs: &[PathBuf], names: &[&str]) -> bool {
    dirs.iter().any(|dir| {
        names.iter().any(|name| {
            if launcher_exists(&dir.join(name)) {
                return true;
            }
            cfg!(windows)
                && ["exe", "cmd", "bat", "ps1"]
                    .iter()
                    .any(|ext| launcher_exists(&dir.join(format!("{name}.{ext}"))))
        })
    })
}

pub fn installed(id: &str) -> bool {
    let names: &[&str] = match id {
        "claude" => &["claude"],
        "codex" => &["codex"],
        "cursor" => &["cursor", "cursor-agent"],
        "gemini" => &["gemini"],
        "antigravity" => &["antigravity", "agy"],
        "opencode" => &["opencode"],
        _ => return false,
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = home_dir() {
        dirs.extend([
            home.join(".local/bin"),
            home.join(".cargo/bin"),
            home.join(".opencode/bin"),
        ]);
    }
    if cfg!(windows) {
        if let Some(roaming) = config_dir() {
            dirs.push(roaming.join("npm"));
        }
        if let Some(local) = first_non_empty_env(&["LOCALAPPDATA"]) {
            dirs.push(local.join("cursor-agent"));
            let app = match id {
                "cursor" => Some("cursor/Cursor.exe"),
                "antigravity" => Some("Antigravity/Antigravity.exe"),
                "opencode" => Some("OpenCode/OpenCode.exe"),
                _ => None,
            };
            if app.is_some_and(|app| executable(&local.join("Programs").join(app))) {
                return true;
            }
        }
        for root in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = first_non_empty_env(&[root]) {
                let app = match id {
                    "cursor" => Some("Cursor/Cursor.exe"),
                    "antigravity" => Some("Antigravity/Antigravity.exe"),
                    "opencode" => Some("OpenCode/OpenCode.exe"),
                    _ => None,
                };
                if app.is_some_and(|app| executable(&root.join(app))) {
                    return true;
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        let app = match id {
            "cursor" => Some("Cursor"),
            "antigravity" => Some("Antigravity"),
            "opencode" => Some("OpenCode"),
            _ => None,
        };
        if let Some(app) = app {
            let relative = format!("{app}.app/Contents/MacOS/{app}");
            if executable(&PathBuf::from("/Applications").join(&relative))
                || home_dir().is_some_and(|h| executable(&h.join("Applications").join(relative)))
            {
                return true;
            }
        }
    } else {
        dirs.extend([PathBuf::from("/usr/bin"), PathBuf::from("/usr/local/bin")]);
        if id == "cursor" {
            dirs.push(PathBuf::from("/opt/cursor"));
        }
        if id == "antigravity" {
            dirs.push(PathBuf::from("/usr/share/antigravity/bin"));
        }
    }
    command_in(&dirs, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_directory_is_not_an_installation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("gemini")).unwrap();
        assert!(!command_in(&[tmp.path().into()], &["gemini"]));
    }

    #[test]
    fn finds_an_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(if cfg!(windows) {
            "gemini.cmd"
        } else {
            "gemini"
        });
        std::fs::write(&path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(command_in(&[tmp.path().into()], &["gemini"]));
        assert!(!command_in(&[tmp.path().into()], &["opencode"]));
    }

    #[test]
    fn npm_launcher_requires_its_installed_target() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = tmp.path().join("opencode.ps1");
        std::fs::write(
            &launcher,
            r#"& "$basedir/node_modules/@opencode/cli/bin/opencode.exe" $args"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(!launcher_exists(&launcher));
        let target = tmp.path().join("node_modules/@opencode/cli/bin");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("opencode.exe"), "fixture").unwrap();
        assert!(launcher_exists(&launcher));
    }
}
