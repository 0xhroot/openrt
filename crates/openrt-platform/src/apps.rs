//! Application discovery from `.desktop` entries (system + user dirs).
//! Read-only: never writes to `~/.local/share/applications`.

use std::path::{Path, PathBuf};

use crate::config_scan::{parse_desktop_entry, AppEntry};

/// Well-known desktop entry search paths.
pub fn desktop_entry_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(h) = home {
        dirs.push(h.join(".local/share/applications"));
    }
    dirs
}

/// Scan directories for `.desktop` entries. Missing dirs are skipped.
pub fn discover_apps(dirs: &[PathBuf]) -> Vec<AppEntry> {
    let mut apps = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(app) = parse_desktop_entry(&text, &path.to_string_lossy()) {
                if !apps
                    .iter()
                    .any(|a: &AppEntry| a.name == app.name || a.command == app.command)
                {
                    apps.push(app);
                }
            }
        }
    }
    apps
}

/// Discover from the live host.
pub fn discover_host_apps() -> Vec<AppEntry> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    discover_apps(&desktop_entry_dirs(home.as_deref()))
}

/// Find the best app entry for a query (case-insensitive name/id match).
pub fn find_app<'a>(apps: &'a [AppEntry], query: &str) -> Option<&'a AppEntry> {
    let q = query.to_ascii_lowercase();
    apps.iter()
        .find(|a| {
            a.name.to_ascii_lowercase() == q
                || a.id.to_ascii_lowercase() == q
                || a.command.to_ascii_lowercase() == q
        })
        .or_else(|| {
            apps.iter().find(|a| {
                a.name.to_ascii_lowercase().contains(&q) || a.id.to_ascii_lowercase().contains(&q)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_from_fixture_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "openrt-p7-apps-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("foot.desktop"),
            "[Desktop Entry]\nType=Application\nName=Foot Terminal\nExec=foot\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("skip.desktop"),
            "[Desktop Entry]\nType=Link\nName=Nope\nURL=http://x\n",
        )
        .unwrap();
        let apps = discover_apps(&[tmp.clone()]);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Foot Terminal");
        assert!(find_app(&apps, "foot").is_some());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
