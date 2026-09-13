//! Passive workdir + gitmeta watcher behind the Source Control auto-refresh.
//!
//! The SCM view used to re-run `git status` on a fixed timer (and only while
//! focused). This module replaces the timer with OS file events — FSEvents on
//! macOS via `notify`: ~zero CPU while idle, instant when something actually
//! changes. Events only mark repos dirty; the caller still runs the real
//! `git status` on a background thread and collects it with `try_recv`, so
//! the TUI thread never blocks on git.
//!
//! Two watch classes per repo:
//! - the workdir root (recursive): catches edits from anywhere — editors,
//!   agents in other panes, checkouts. `.git/` noise is skipped here;
//! - the gitdir key paths (`HEAD`, `index`, `refs/`): catches commits,
//!   stages and branch moves, which never touch the workdir.
//!
//! Filtering is best-effort on purpose: anything that slips through only
//! causes one extra `git status` run, and status output stays the ground
//! truth. Only the top-level `.gitignore` (+ `.git/info/exclude`) is loaded;
//! a nested `.gitignore` miss likewise just costs one spare refresh.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// File-event debounce while the pane is focused: snappy, yet a save burst
/// still coalesces into one refresh.
pub const WATCH_DEBOUNCE_FOCUSED: std::time::Duration = std::time::Duration::from_millis(400);
/// Same while unfocused: a visible view still refreshes, just less eagerly.
pub const WATCH_DEBOUNCE_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
/// `pane.list` focus probes are throttled to this; the watcher itself needs
/// no focus state to collect events.
pub const FOCUS_PROBE_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Directory/file names that can never affect `git status` and are pure
/// event noise (build output, vendored envs, editor state).
const SKIP_NAMES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "dist",
    "build",
    ".idea",
    ".vscode",
    ".DS_Store",
];

/// One location handed to `notify`, and the repo it feeds.
struct WatchEntry {
    /// Exact watched path (file or dir).
    path: PathBuf,
    /// Whether children are included (dirs only).
    recursive: bool,
    /// Owning repo root.
    repo: PathBuf,
    /// Workdir entries filter through gitignore; gitmeta entries are always
    /// relevant (a `HEAD` rewrite IS the change).
    gitignore: Option<ignore::gitignore::Gitignore>,
}

pub struct WorkdirWatcher {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    entries: Vec<WatchEntry>,
    dirty: HashSet<PathBuf>,
    last_event: Option<Instant>,
    overflow: bool,
}

impl WorkdirWatcher {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let watcher = RecommendedWatcher::new(
            move |res| {
                tx.send(res).ok();
            },
            notify::Config::default(),
        )
        .expect("platform file watcher");
        Self {
            watcher,
            rx,
            entries: Vec::new(),
            dirty: HashSet::new(),
            last_event: None,
            overflow: false,
        }
    }

    /// Align watched locations with the currently known repos. `repos` maps
    /// each repo root to its resolved gitdir (None = not resolved yet).
    /// Re-resolving gitdirs is the caller's job — this only diffs paths, so
    /// calling it every tick is cheap.
    pub fn sync_roots(&mut self, repos: &[(PathBuf, Option<PathBuf>)]) {
        let mut desired: Vec<WatchEntry> = Vec::new();
        for (root, git_dir) in repos {
            desired.push(WatchEntry {
                path: root.clone(),
                recursive: true,
                repo: root.clone(),
                gitignore: load_ignores(root),
            });
            let Some(git_dir) = git_dir else { continue };
            for meta in [git_dir.join("HEAD"), git_dir.join("index")] {
                if meta.is_file() {
                    desired.push(WatchEntry {
                        path: meta,
                        recursive: false,
                        repo: root.clone(),
                        gitignore: None,
                    });
                }
            }
            let refs = git_dir.join("refs");
            if refs.is_dir() {
                desired.push(WatchEntry {
                    path: refs,
                    recursive: true,
                    repo: root.clone(),
                    gitignore: None,
                });
            }
        }
        let wanted: HashSet<(PathBuf, bool)> = desired
            .iter()
            .map(|e| (e.path.clone(), e.recursive))
            .collect();
        self.entries.retain(|e| {
            let keep = wanted.contains(&(e.path.clone(), e.recursive));
            if !keep {
                self.watcher.unwatch(&e.path).ok();
            }
            keep
        });
        let have: HashSet<(PathBuf, bool)> = self
            .entries
            .iter()
            .map(|e| (e.path.clone(), e.recursive))
            .collect();
        for entry in desired {
            if have.contains(&(entry.path.clone(), entry.recursive)) {
                continue;
            }
            let mode = if entry.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            // Missing paths (fresh repo without `index`, packed-refs without
            // `refs/`) are simply skipped; the next sync retries them.
            if self.watcher.watch(&entry.path, mode).is_ok() {
                self.entries.push(entry);
            }
        }
    }

    /// Repo roots currently covered by the workdir (recursive) watches. The
    /// caller uses this to avoid re-resolving gitdirs every tick.
    pub fn watched_workdirs(&self) -> HashSet<PathBuf> {
        self.entries
            .iter()
            .filter(|e| e.recursive && e.gitignore.is_some())
            .map(|e| e.repo.clone())
            .collect()
    }

    /// Drain pending OS events into the dirty set. Cheap: no IPC, no git.
    pub fn poll(&mut self) {
        while let Ok(res) = self.rx.try_recv() {
            let Ok(event) = res else {
                self.overflow = true;
                continue;
            };
            if !relevant_kind(&event.kind) {
                continue;
            }
            for path in &event.paths {
                if let Some(repo) = self.owner_repo(path) {
                    self.dirty.insert(repo);
                    self.last_event = Some(Instant::now());
                }
            }
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.overflow || !self.dirty.is_empty()
    }

    /// When the last relevant event arrived (for debouncing).
    pub fn last_event(&self) -> Option<Instant> {
        self.last_event
    }

    /// Consume the dirty set. An overflow counts as dirty for every repo the
    /// caller passes in (lost events must never lose an update).
    pub fn take_dirty(&mut self, all: impl FnOnce() -> Vec<PathBuf>) -> Vec<PathBuf> {
        if self.overflow {
            self.overflow = false;
            self.dirty.clear();
            return all();
        }
        self.dirty.drain().collect()
    }

    /// Longest-prefix owner: nested repos attribute events to the innermost
    /// root, mirroring the independent per-repo decoration rule.
    fn owner_repo(&self, path: &Path) -> Option<PathBuf> {
        let mut best: Option<(&WatchEntry, usize)> = None;
        for entry in &self.entries {
            let parent = if entry.recursive {
                &entry.path
            } else {
                // File watches deliver the file itself; match its parent so
                // sibling noise can never sneak in.
                let Some(parent) = entry.path.parent() else {
                    continue;
                };
                if path != entry.path && path.parent() != Some(parent) {
                    continue;
                }
                parent
            };
            if !path.starts_with(parent) {
                continue;
            }
            if !entry.recursive && path != entry.path {
                continue;
            }
            let len = entry.path.as_os_str().len();
            if best.is_none_or(|(_, best_len)| len > best_len) {
                best = Some((entry, len));
            }
        }
        let (entry, _) = best?;
        if entry.gitignore.is_some() && path_skipped(path, &entry.repo, entry.gitignore.as_ref()) {
            return None;
        }
        Some(entry.repo.clone())
    }
}

/// Pure filter shared by the watcher and the unit tests.
fn path_skipped(
    path: &Path,
    repo: &Path,
    gitignore: Option<&ignore::gitignore::Gitignore>,
) -> bool {
    if path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| SKIP_NAMES.contains(&name))
    }) {
        return true;
    }
    // Outside the repo entirely (symlink escapes, `/tmp` editors): git
    // status for this repo cannot see it.
    if !path.starts_with(repo) {
        return true;
    }
    gitignore.is_some_and(|gi| gi.matched(path, false).is_ignore())
}

fn load_ignores(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    let mut added = false;
    for candidate in [root.join(".gitignore"), root.join(".git/info/exclude")] {
        if candidate.is_file() {
            builder.add(candidate);
            added = true;
        }
    }
    if !added {
        return None;
    }
    builder.build().ok()
}

fn relevant_kind(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) => true,
        // chmod-only noise never changes status output.
        EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        EventKind::Remove(_) => true,
        // Opaque platform event: assume something happened (full refresh,
        // still cheap and rare).
        EventKind::Other => true,
        // Access (open/close) and uninteresting internal events.
        _ => false,
    }
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
