//! Tab-completion for the focus-mode Terminal tab.
//!
//! Each `TerminalPanel` keeps its own `InputBuffer` — a local mirror of the
//! characters the user has typed since the last command boundary (Enter,
//! Ctrl+C, Ctrl+U, Ctrl+G). The mirror is imperfect: arrow-key navigation,
//! `Ctrl+W`, paste of multi-line input, etc. won't be reflected. That's an
//! accepted trade-off — the common case is "type a fresh command at an empty
//! prompt and press Tab", and any drift is recoverable by pressing Enter
//! (run/cancel the line) or Ctrl+U (clear).
//!
//! When the user presses Tab in Type mode, `compute_completions()` looks at
//! the buffer and returns:
//!   * Command candidates (PATH executables) when the prefix is the first
//!     word and doesn't look like a path.
//!   * Filesystem candidates (files + directories) otherwise, anchored at
//!     `working_dir`, `/`, or `~/` depending on the prefix.
//!
//! A single candidate is inserted inline. Multiple candidates open a popup
//! whose state lives on `FocusModeState::completion`. Zero candidates falls
//! back to forwarding the Tab byte to the PTY so the shell's own readline
//! completion still has a chance.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAX_BUFFER_LEN: usize = 1024;
const MAX_CANDIDATES: usize = 64;

/// Number of items shown at once in the completion popup.
pub const POPUP_VISIBLE_ROWS: usize = 8;

#[derive(Default, Clone)]
pub struct InputBuffer {
    text: String,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn push_char(&mut self, c: char) {
        if self.text.len() + c.len_utf8() <= MAX_BUFFER_LEN {
            self.text.push(c);
        }
    }

    pub fn push_str(&mut self, s: &str) {
        if self.text.len() + s.len() <= MAX_BUFFER_LEN {
            self.text.push_str(s);
        }
    }

    pub fn pop_char(&mut self) {
        self.text.pop();
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// Trailing `/` so the user can keep typing inside the directory.
    Directory,
    /// Trailing space.
    File,
    /// Trailing space.
    Command,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub display: String,
    pub kind: CompletionKind,
}

pub struct CompletionResult {
    pub prefix: String,
    pub items: Vec<CompletionItem>,
}

pub struct CompletionState {
    pub prefix: String,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub scroll: usize,
}

impl CompletionState {
    pub fn move_up(&mut self) {
        if self.selected == 0 {
            return;
        }
        self.selected -= 1;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 >= self.items.len() {
            return;
        }
        self.selected += 1;
        if self.selected >= self.scroll + POPUP_VISIBLE_ROWS {
            self.scroll = self.selected + 1 - POPUP_VISIBLE_ROWS;
        }
    }
}

/// Inspect `buffer` and produce candidates relevant to the last word.
///
/// Returns `None` when the buffer is empty, contains only trailing whitespace,
/// or no candidate matches — in which case the caller forwards the Tab byte
/// to the PTY and lets the shell handle it.
pub fn compute_completions(buffer: &str, working_dir: &str) -> Option<CompletionResult> {
    if buffer.is_empty() {
        return None;
    }

    // The last whitespace splits "word being completed" from "everything
    // before". Anything after the last space is the prefix. If the buffer
    // ends in a space, the prefix is empty and we have nothing to do.
    let last_space = buffer.rfind(' ');
    let prefix: String = match last_space {
        Some(idx) => buffer[idx + 1..].to_string(),
        None => buffer.to_string(),
    };
    if prefix.is_empty() {
        return None;
    }

    let is_first_word = last_space.is_none();
    let looks_like_path =
        prefix.starts_with('.') || prefix.starts_with('/') || prefix.starts_with('~');

    let items = if is_first_word && !looks_like_path {
        path_executables_starting_with(&prefix)
            .into_iter()
            .map(|name| CompletionItem {
                display: name,
                kind: CompletionKind::Command,
            })
            .collect()
    } else {
        path_completions(&prefix, working_dir)
    };

    if items.is_empty() {
        None
    } else {
        Some(CompletionResult { prefix, items })
    }
}

fn path_completions(prefix: &str, working_dir: &str) -> Vec<CompletionItem> {
    let (dir_to_read, leaf_prefix, display_anchor): (PathBuf, String, String) =
        if let Some(rest) = prefix.strip_prefix('~') {
            let Some(home) = dirs::home_dir() else {
                return Vec::new();
            };
            let after = rest.strip_prefix('/').unwrap_or(rest);
            split_dir_leaf(after, &home, "~/")
        } else if let Some(rest) = prefix.strip_prefix('/') {
            split_dir_leaf(rest, Path::new("/"), "/")
        } else {
            split_dir_leaf(prefix, Path::new(working_dir), "")
        };

    let entries = match std::fs::read_dir(&dir_to_read) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    // Show dotfiles only when the user has explicitly typed a leading `.`.
    let show_hidden = leaf_prefix.starts_with('.');

    let mut items: Vec<CompletionItem> = Vec::new();
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(&leaf_prefix) {
            continue;
        }
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir() || (t.is_symlink() && entry.path().is_dir()))
            .unwrap_or(false);
        let kind = if is_dir {
            CompletionKind::Directory
        } else {
            CompletionKind::File
        };
        items.push(CompletionItem {
            display: format!("{display_anchor}{name}"),
            kind,
        });
    }

    items.sort_by(|a, b| a.display.cmp(&b.display));
    items.truncate(MAX_CANDIDATES);
    items
}

/// Split a path-like `after_anchor` (already stripped of `~`/`/` prefix) into
/// the directory to read, the leaf-name prefix to filter by, and the display
/// prefix that, when concatenated with each entry name, reconstructs what the
/// user sees on screen.
fn split_dir_leaf(
    after_anchor: &str,
    base: &Path,
    display_anchor: &str,
) -> (PathBuf, String, String) {
    if let Some(idx) = after_anchor.rfind('/') {
        let dir_part = &after_anchor[..idx];
        let leaf = after_anchor[idx + 1..].to_string();
        let dir_path = if dir_part.is_empty() {
            base.to_path_buf()
        } else {
            base.join(dir_part)
        };
        let display = format!("{display_anchor}{dir_part}/");
        (dir_path, leaf, display)
    } else {
        (
            base.to_path_buf(),
            after_anchor.to_string(),
            display_anchor.to_string(),
        )
    }
}

/// PATH is walked once per process. Re-walking on every Tab would burn a lot
/// of syscalls, and PATH almost never changes mid-session.
fn cached_path_executables() -> &'static HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut set: HashSet<String> = HashSet::new();
        let path = std::env::var_os("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let read = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for entry in read.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    set.insert(name.to_string());
                }
            }
        }
        set
    })
}

fn path_executables_starting_with(prefix: &str) -> Vec<String> {
    let cache = cached_path_executables();
    let mut out: Vec<String> = cache
        .iter()
        .filter(|name| name.starts_with(prefix))
        .cloned()
        .collect();
    out.sort();
    out.truncate(MAX_CANDIDATES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile_like::TempDir;

    // Tiny tempdir helper without pulling a new crate. Created beneath
    // std::env::temp_dir() with a unique-ish name and removed on drop.
    mod tempfile_like {
        use std::path::{Path, PathBuf};

        pub struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            pub fn new(tag: &str) -> std::io::Result<Self> {
                let mut p = std::env::temp_dir();
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                p.push(format!("clust-tc-{tag}-{stamp}-{}", std::process::id()));
                std::fs::create_dir_all(&p)?;
                Ok(Self { path: p })
            }

            pub fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    #[test]
    fn empty_buffer_yields_none() {
        assert!(compute_completions("", "/tmp").is_none());
        assert!(compute_completions("   ", "/tmp").is_none());
    }

    #[test]
    fn buffer_ending_in_space_yields_none() {
        assert!(compute_completions("ls ", "/tmp").is_none());
    }

    #[test]
    fn path_completion_lists_matching_entries() {
        let tmp = TempDir::new("path").unwrap();
        fs::write(tmp.path().join("alpha.txt"), b"a").unwrap();
        fs::write(tmp.path().join("alphabet.txt"), b"a").unwrap();
        fs::create_dir(tmp.path().join("alphasrc")).unwrap();
        fs::write(tmp.path().join("beta.txt"), b"b").unwrap();

        let wd = tmp.path().to_string_lossy().into_owned();
        let res = compute_completions("ls al", &wd).expect("some matches");
        assert_eq!(res.prefix, "al");
        let names: Vec<_> = res.items.iter().map(|i| i.display.as_str()).collect();
        assert!(names.contains(&"alpha.txt"));
        assert!(names.contains(&"alphabet.txt"));
        assert!(names.contains(&"alphasrc"));
        assert!(!names.iter().any(|n| n == &"beta.txt"));

        let dir_kind = res
            .items
            .iter()
            .find(|i| i.display == "alphasrc")
            .map(|i| i.kind.clone())
            .unwrap();
        assert_eq!(dir_kind, CompletionKind::Directory);
    }

    #[test]
    fn path_completion_into_subdirectory() {
        let tmp = TempDir::new("subdir").unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src").join("foo.rs"), b"//").unwrap();
        fs::write(tmp.path().join("src").join("foobar.rs"), b"//").unwrap();
        fs::write(tmp.path().join("src").join("bar.rs"), b"//").unwrap();

        let wd = tmp.path().to_string_lossy().into_owned();
        let res = compute_completions("vim src/foo", &wd).expect("matches");
        assert_eq!(res.prefix, "src/foo");
        let names: Vec<_> = res.items.iter().map(|i| i.display.as_str()).collect();
        assert!(names.contains(&"src/foo.rs"));
        assert!(names.contains(&"src/foobar.rs"));
        assert!(!names.iter().any(|n| n == &"src/bar.rs"));
    }

    #[test]
    fn dotfiles_only_shown_when_prefix_starts_with_dot() {
        let tmp = TempDir::new("dot").unwrap();
        fs::write(tmp.path().join(".hidden"), b"").unwrap();
        fs::write(tmp.path().join("visible"), b"").unwrap();

        let wd = tmp.path().to_string_lossy().into_owned();

        // Bare prefix → no dotfile.
        let res = compute_completions("ls v", &wd).expect("match");
        assert!(res.items.iter().any(|i| i.display == "visible"));
        assert!(compute_completions("ls h", &wd).is_none());

        // Dot prefix → dotfile shown.
        let res = compute_completions("ls .h", &wd).expect("match");
        assert!(res.items.iter().any(|i| i.display == ".hidden"));
    }

    #[test]
    fn absolute_path_completion_uses_root_anchor() {
        // Working dir is irrelevant here — the prefix starts with `/`, so we
        // walk from the root.
        let res = compute_completions("cat /us", "/tmp").expect("matches under /usr");
        assert_eq!(res.prefix, "/us");
        // We should at least get the standard `/usr` entry on POSIX systems.
        assert!(
            res.items.iter().any(|i| i.display == "/usr"),
            "expected /usr in {:?}",
            res.items.iter().map(|i| &i.display).collect::<Vec<_>>()
        );
    }

    #[test]
    fn first_word_path_like_does_path_completion() {
        let tmp = TempDir::new("firstpath").unwrap();
        fs::create_dir(tmp.path().join("scripts")).unwrap();
        fs::write(tmp.path().join("scripts").join("run.sh"), b"#!/bin/sh").unwrap();

        let wd = tmp.path().to_string_lossy().into_owned();
        let res = compute_completions("./scrip", &wd).expect("match");
        assert_eq!(res.prefix, "./scrip");
        assert!(res.items.iter().any(|i| i.display == "./scripts"));
    }

    #[test]
    fn completion_state_navigation() {
        let mut state = CompletionState {
            prefix: "x".to_string(),
            items: (0..20)
                .map(|i| CompletionItem {
                    display: format!("x{i:02}"),
                    kind: CompletionKind::File,
                })
                .collect(),
            selected: 0,
            scroll: 0,
        };

        // Down past the visible window scrolls.
        for _ in 0..POPUP_VISIBLE_ROWS {
            state.move_down();
        }
        assert_eq!(state.selected, POPUP_VISIBLE_ROWS);
        assert_eq!(state.scroll, 1);

        // Up back to the top resets scroll.
        for _ in 0..state.selected {
            state.move_up();
        }
        assert_eq!(state.selected, 0);
        assert_eq!(state.scroll, 0);

        // Down can't pass the last item.
        for _ in 0..50 {
            state.move_down();
        }
        assert_eq!(state.selected, state.items.len() - 1);
    }

    #[test]
    fn input_buffer_respects_max_len() {
        let mut buf = InputBuffer::new();
        for _ in 0..(MAX_BUFFER_LEN + 100) {
            buf.push_char('a');
        }
        assert_eq!(buf.as_str().len(), MAX_BUFFER_LEN);
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn input_buffer_handles_unicode() {
        let mut buf = InputBuffer::new();
        buf.push_char('é');
        buf.push_str("こ");
        assert_eq!(buf.as_str(), "éこ");
        buf.pop_char();
        assert_eq!(buf.as_str(), "é");
    }
}
