//! Persisted IDE preferences — the recent-files list and the chosen theme.
//!
//! Stored as small text files under `%LOCALAPPDATA%\LocusIDE\` so they survive
//! across sessions and don't require the install directory to be writable
//! (the release folder may sit in Program Files). Best-effort: any I/O error is
//! swallowed (a missing/unwritable config just means "no recent files / the
//! default theme") — preferences must never break the editor.

#![cfg(windows)]

use std::path::{Path, PathBuf};

/// Most-recent-first cap. Plenty for a File menu; keeps the file tiny.
const RECENT_MAX: usize = 12;

/// `%LOCALAPPDATA%\LocusIDE`, created on demand. `None` if the env var is
/// absent (then preferences are simply not persisted).
fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let dir = PathBuf::from(base).join("LocusIDE");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

fn recent_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("recent.txt"))
}
fn theme_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("theme.txt"))
}

/// The recent-files list, most-recent first. Entries that no longer exist on
/// disk are dropped, so the menu never offers a dead path.
pub fn recent_load() -> Vec<PathBuf> {
    let Some(path) = recent_file() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .take(RECENT_MAX)
        .collect()
}

/// Record `path` as the most-recently-opened file: move it to the front,
/// de-duplicate (case-insensitively, since Windows paths are), cap the list,
/// and persist. Best-effort.
pub fn recent_record(path: &Path) {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut list = recent_load();
    list.retain(|p| {
        let pc = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        !pc.as_os_str().eq_ignore_ascii_case(canon.as_os_str())
    });
    list.insert(0, canon);
    list.truncate(RECENT_MAX);
    if let Some(file) = recent_file() {
        let body: String = list
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(file, body);
    }
}

/// The persisted theme index (0 = default). Out-of-range or missing → 0.
pub fn theme_load() -> usize {
    theme_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

/// Persist the chosen theme index. Best-effort.
pub fn theme_save(idx: usize) {
    if let Some(file) = theme_file() {
        let _ = std::fs::write(file, idx.to_string());
    }
}
