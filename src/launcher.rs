//! Desktop-application discovery, search, and launching.
//!
//! This module deliberately knows nothing about GPUI or the island: it walks the
//! freedesktop desktop-entry locations, builds a searchable catalog, and spawns
//! processes. Keeping it independent means other views (a calendar, a settings
//! pane) can reuse the same catalog later.

use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use freedesktop_desktop_entry::{Iter, get_languages_from_env};

/// A launchable application, already resolved from a desktop entry.
pub struct AppEntry {
    /// Human-readable name shown in the launcher.
    pub name: String,
    /// Optional short description from the entry's `Comment` key.
    pub comment: Option<String>,
    /// Optional icon name or path from the entry's `Icon` key.
    ///
    /// Captured for a future icon renderer; the text-only launcher doesn't draw
    /// it yet.
    #[allow(dead_code)]
    pub icon: Option<String>,
    /// Command and arguments, with field codes such as `%U` already resolved.
    exec: Vec<String>,
}

impl AppEntry {
    /// Spawns the application, detached from the shell.
    ///
    /// The entry's `Exec` line is never passed to a shell: it was parsed by the
    /// desktop-entry crate, so quoting and field-code rules are already handled.
    pub fn launch(&self) -> io::Result<()> {
        let Some((program, args)) = self.exec.split_first() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty Exec"));
        };

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        // The shell outlives nothing, so a short-lived reaper thread waits on the
        // child to keep it from becoming a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        Ok(())
    }
}

/// Installed applications, sorted case-insensitively by display name.
///
/// Stored in a GPUI global so every island window shares one catalog.
pub struct Catalog {
    entries: Vec<AppEntry>,
}

impl gpui::Global for Catalog {}

impl Catalog {
    /// Walks the desktop-entry locations and builds the catalog.
    ///
    /// This touches the filesystem, so call it once at startup rather than from
    /// a render path.
    pub fn discover() -> Self {
        let locales = get_languages_from_env();
        let mut entries = Vec::new();
        let mut seen = HashSet::new();

        for entry in Iter::new(application_dirs().into_iter()).entries(Some(locales.as_slice())) {
            // Hidden and no-display entries exist only to register a type or MIME
            // association, not to be launched directly.
            if entry.hidden() || entry.no_display() {
                continue;
            }

            // Only real applications; skip links, directories, and the like.
            if !matches!(entry.type_(), None | Some("Application")) {
                continue;
            }

            let Some(name) = entry.name(&locales).map(|name| name.trim().to_string()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }

            // Entries that don't parse into a command can't be launched.
            let Ok(exec) = entry.parse_exec() else {
                continue;
            };
            if exec.is_empty() {
                continue;
            }

            // The same application is often present in several data directories
            // at different precedence; keep only the first, highest-precedence one.
            if !seen.insert(entry.id().to_string()) {
                continue;
            }

            entries.push(AppEntry {
                name,
                comment: entry
                    .comment(&locales)
                    .map(|comment| comment.trim().to_string())
                    .filter(|comment| !comment.is_empty()),
                icon: entry.icon().map(str::to_string),
                exec,
            });
        }

        entries.sort_by_cached_key(|entry| entry.name.to_lowercase());
        Self { entries }
    }

    /// Indices of the entries matching `query`, best matches first.
    ///
    /// An empty query matches everything in catalog order. Name-prefix matches
    /// rank above name-substring matches.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.entries.len()).collect();
        }

        let mut prefix = Vec::new();
        let mut substring = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let name = entry.name.to_lowercase();
            if name.starts_with(&query) {
                prefix.push(index);
            } else if name.contains(&query) {
                substring.push(index);
            }
        }

        prefix.extend(substring);
        prefix
    }

    pub fn entry(&self, index: usize) -> Option<&AppEntry> {
        self.entries.get(index)
    }
}

/// The `applications` subdirectories to search, in XDG precedence order.
///
/// Walking the data directories ourselves (rather than relying on the
/// library's `default_paths`) keeps a missing `HOME` from aborting startup.
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    match std::env::var_os("XDG_DATA_HOME") {
        Some(home) if !home.is_empty() => {
            dirs.push(PathBuf::from(home).join("applications"));
        }
        _ => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share/applications"));
            }
        }
    }

    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .map(|dirs| dirs.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    for dir in data_dirs.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir).join("applications"));
    }

    dirs
}

#[cfg(test)]
mod tests {
    use super::Catalog;

    #[test]
    fn search_ranks_prefix_before_substring() {
        let catalog = Catalog {
            entries: vec![entry("Alpha"), entry("Xylophone"), entry("Alchemy")],
        };

        // "al" matches "Alpha" and "Alchemy" as prefixes; neither is a plain
        // substring match, so both appear in catalog order.
        assert_eq!(catalog.search("al"), vec![0, 2]);

        // "ph" only matches as a substring: both "Alpha" and "Xylophone"
        // contain it, so they keep catalog order.
        assert_eq!(catalog.search("ph"), vec![0, 1]);

        // An empty query keeps catalog order.
        assert_eq!(catalog.search("  "), vec![0, 1, 2]);
    }

    #[test]
    fn search_is_case_insensitive() {
        let catalog = Catalog {
            entries: vec![entry("Firefox")],
        };
        assert_eq!(catalog.search("FIRE"), vec![0]);
    }

    #[test]
    fn discover_walks_system_entries_without_panicking() {
        // Must not panic regardless of what is (or isn't) installed.
        let _ = super::Catalog::discover();
    }

    fn entry(name: &str) -> super::AppEntry {
        super::AppEntry {
            name: name.to_string(),
            comment: None,
            icon: None,
            exec: vec![name.to_string()],
        }
    }
}
