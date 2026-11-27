//! "Open with…": the applications registered for a file's type (freedesktop MIME
//! associations via GLib's `gio`), and launching one of them.

use std::path::{Path, PathBuf};

pub struct App {
    pub desktop_file: PathBuf,
    pub name: String,
}

fn mime_of(path: &str, file_type: &str) -> String {
    if file_type.contains('/') {
        return file_type.to_string();
    }
    std::process::Command::new("xdg-mime")
        .args(["query", "filetype", path])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"))];
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(sys.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs
}

fn find_desktop_file(id: &str) -> Option<PathBuf> {
    // ids like "org.gnome.Totem.desktop"; "kde4-foo.desktop" maps to kde4/foo.desktop
    for dir in data_dirs() {
        let p = dir.join("applications").join(id);
        if p.is_file() {
            return Some(p);
        }
        if let Some((prefix, rest)) = id.split_once('-') {
            let p = dir.join("applications").join(prefix).join(rest);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn desktop_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut name = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if line == "NoDisplay=true" || line == "Hidden=true" {
            return None;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.to_string());
        }
    }
    name
}

/// Applications that can open this file (recommended ones first).
pub fn apps_for(path: &str, file_type: &str) -> Vec<App> {
    let mime = mime_of(path, file_type);
    if mime.is_empty() {
        return Vec::new();
    }
    let out = std::process::Command::new("gio").args(["mime", &mime]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let mut ids: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.ends_with(".desktop") && !l.contains(' ') && !ids.iter().any(|i| i == l) {
            ids.push(l.to_string());
        }
    }
    ids.into_iter()
        .filter_map(|id| {
            let file = find_desktop_file(&id)?;
            let name = desktop_name(&file)?;
            Some(App { desktop_file: file, name })
        })
        .collect()
}

pub fn launch(desktop_file: &Path, file: &str) -> bool {
    let mut cmd = std::process::Command::new("gio");
    crate::desktop::child_env(&mut cmd);
    cmd.arg("launch").arg(desktop_file).arg(file).spawn().is_ok()
}
