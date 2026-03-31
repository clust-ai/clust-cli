#![allow(dead_code)]

use std::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{self, Duration};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DiffLineKind {
    FileHeader,
    HunkHeader,
    Context,
    Add,
    Delete,
    FileMetadata,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub file_idx: usize,
}

#[derive(Clone, Debug)]
pub struct ParsedDiff {
    pub lines: Vec<DiffLine>,
    /// Index into `lines` where each file's diff starts (the "diff --git" line).
    pub file_start_indices: Vec<usize>,
    pub file_names: Vec<String>,
}

pub enum DiffEvent {
    Updated(ParsedDiff),
    Error(String),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

pub fn parse_unified_diff(raw: &str) -> ParsedDiff {
    let mut lines = Vec::new();
    let mut file_start_indices = Vec::new();
    let mut file_names = Vec::new();

    let mut file_idx: usize = 0;
    let mut old_line: usize = 0;
    let mut new_line: usize = 0;
    let mut in_file = false;

    for text in raw.lines() {
        if text.starts_with("diff --git ") {
            if in_file {
                file_idx += 1;
            }
            in_file = true;
            file_start_indices.push(lines.len());

            // Extract file name from "diff --git a/path b/path"
            let name = text
                .strip_prefix("diff --git a/")
                .and_then(|rest| rest.split(" b/").next())
                .unwrap_or(text)
                .to_string();
            file_names.push(name);

            lines.push(DiffLine {
                kind: DiffLineKind::FileHeader,
                content: text.to_string(),
                old_lineno: None,
                new_lineno: None,
                file_idx,
            });
        } else if text.starts_with("--- ") || text.starts_with("+++ ") || text.starts_with("index ") {
            lines.push(DiffLine {
                kind: DiffLineKind::FileMetadata,
                content: text.to_string(),
                old_lineno: None,
                new_lineno: None,
                file_idx,
            });
        } else if text.starts_with("@@ ") {
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            if let Some((old_start, new_start)) = parse_hunk_header(text) {
                old_line = old_start;
                new_line = new_start;
            }
            lines.push(DiffLine {
                kind: DiffLineKind::HunkHeader,
                content: text.to_string(),
                old_lineno: None,
                new_lineno: None,
                file_idx,
            });
        } else if let Some(stripped) = text.strip_prefix('+') {
            lines.push(DiffLine {
                kind: DiffLineKind::Add,
                content: stripped.to_string(),
                old_lineno: None,
                new_lineno: Some(new_line),
                file_idx,
            });
            new_line += 1;
        } else if let Some(stripped) = text.strip_prefix('-') {
            lines.push(DiffLine {
                kind: DiffLineKind::Delete,
                content: stripped.to_string(),
                old_lineno: Some(old_line),
                new_lineno: None,
                file_idx,
            });
            old_line += 1;
        } else if let Some(stripped) = text.strip_prefix(' ') {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                content: stripped.to_string(),
                old_lineno: Some(old_line),
                new_lineno: Some(new_line),
                file_idx,
            });
            old_line += 1;
            new_line += 1;
        } else if text == "\\ No newline at end of file" {
            lines.push(DiffLine {
                kind: DiffLineKind::FileMetadata,
                content: text.to_string(),
                old_lineno: None,
                new_lineno: None,
                file_idx,
            });
        } else if in_file {
            // Context line without leading space (shouldn't happen in well-formed diff,
            // but handle gracefully)
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                content: text.to_string(),
                old_lineno: Some(old_line),
                new_lineno: Some(new_line),
                file_idx,
            });
            old_line += 1;
            new_line += 1;
        }
    }

    ParsedDiff {
        lines,
        file_start_indices,
        file_names,
    }
}

/// Parse "@@ -old_start[,old_count] +new_start[,new_count] @@" into (old_start, new_start).
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    // Strip the leading "@@ " and trailing " @@..."
    let inner = line.strip_prefix("@@ ")?;
    let range_part = inner.split(" @@").next()?;
    let mut parts = range_part.split_whitespace();

    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;

    let old_start: usize = old_part.split(',').next()?.parse().ok()?;
    let new_start: usize = new_part.split(',').next()?.parse().ok()?;

    Some((old_start, new_start))
}

// ---------------------------------------------------------------------------
// Background refresh task
// ---------------------------------------------------------------------------

pub fn spawn_diff_task(
    working_dir: String,
    tx: mpsc::Sender<DiffEvent>,
    stop_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::task::spawn(diff_refresh_loop(working_dir, tx, stop_rx))
}

async fn diff_refresh_loop(
    working_dir: String,
    tx: mpsc::Sender<DiffEvent>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut interval = time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = stop_rx.changed() => break,
        }

        let dir = working_dir.clone();
        let result = tokio::task::spawn_blocking(move || run_git_diff(&dir)).await;

        let event = match result {
            Ok(Ok(raw)) => DiffEvent::Updated(parse_unified_diff(&raw)),
            Ok(Err(e)) => DiffEvent::Error(e),
            Err(e) => DiffEvent::Error(format!("task join error: {e}")),
        };

        if tx.send(event).await.is_err() {
            break; // receiver dropped
        }
    }
}

fn run_git_diff(working_dir: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(working_dir)
        .output()
        .map_err(|e| format!("failed to run git diff: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git diff failed: {stderr}"));
    }

    String::from_utf8(output.stdout).map_err(|e| format!("invalid utf8 in git output: {e}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
index abc1234..def5678 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,5 +1,6 @@
 fn main() {
-    println!(\"hello\");
+    println!(\"hello world\");
+    println!(\"goodbye\");
     let x = 1;
     let y = 2;
 }
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1,3 +1,3 @@
 # Project
-Old description
+New description
 More text";

    #[test]
    fn parse_file_count() {
        let diff = parse_unified_diff(SAMPLE_DIFF);
        assert_eq!(diff.file_names.len(), 2);
        assert_eq!(diff.file_names[0], "src/main.rs");
        assert_eq!(diff.file_names[1], "README.md");
    }

    #[test]
    fn parse_file_start_indices() {
        let diff = parse_unified_diff(SAMPLE_DIFF);
        assert_eq!(diff.file_start_indices.len(), 2);
        assert_eq!(diff.lines[diff.file_start_indices[0]].kind, DiffLineKind::FileHeader);
        assert_eq!(diff.lines[diff.file_start_indices[1]].kind, DiffLineKind::FileHeader);
    }

    #[test]
    fn parse_line_kinds() {
        let diff = parse_unified_diff(SAMPLE_DIFF);
        let kinds: Vec<DiffLineKind> = diff.lines.iter().map(|l| l.kind).collect();

        // First file: FileHeader, FileMetadata(index), FileMetadata(---), FileMetadata(+++),
        //             HunkHeader, Context, Delete, Add, Add, Context, Context, Context
        assert_eq!(kinds[0], DiffLineKind::FileHeader);
        assert_eq!(kinds[1], DiffLineKind::FileMetadata); // index
        assert_eq!(kinds[2], DiffLineKind::FileMetadata); // ---
        assert_eq!(kinds[3], DiffLineKind::FileMetadata); // +++
        assert_eq!(kinds[4], DiffLineKind::HunkHeader);
        assert_eq!(kinds[5], DiffLineKind::Context);      // fn main()
        assert_eq!(kinds[6], DiffLineKind::Delete);        // -println
        assert_eq!(kinds[7], DiffLineKind::Add);           // +println hello world
        assert_eq!(kinds[8], DiffLineKind::Add);           // +println goodbye
        assert_eq!(kinds[9], DiffLineKind::Context);       // let x
        assert_eq!(kinds[10], DiffLineKind::Context);      // let y
        assert_eq!(kinds[11], DiffLineKind::Context);      // }
    }

    #[test]
    fn parse_line_numbers() {
        let diff = parse_unified_diff(SAMPLE_DIFF);
        // First hunk: @@ -1,5 +1,6 @@
        // Context " fn main()" => old=1, new=1
        assert_eq!(diff.lines[5].old_lineno, Some(1));
        assert_eq!(diff.lines[5].new_lineno, Some(1));
        // Delete "- println(\"hello\")" => old=2
        assert_eq!(diff.lines[6].old_lineno, Some(2));
        assert_eq!(diff.lines[6].new_lineno, None);
        // Add "+ println(\"hello world\")" => new=2
        assert_eq!(diff.lines[7].old_lineno, None);
        assert_eq!(diff.lines[7].new_lineno, Some(2));
        // Add "+ println(\"goodbye\")" => new=3
        assert_eq!(diff.lines[8].old_lineno, None);
        assert_eq!(diff.lines[8].new_lineno, Some(3));
        // Context " let x" => old=3, new=4
        assert_eq!(diff.lines[9].old_lineno, Some(3));
        assert_eq!(diff.lines[9].new_lineno, Some(4));
    }

    #[test]
    fn parse_hunk_header_basic() {
        assert_eq!(parse_hunk_header("@@ -1,5 +1,6 @@"), Some((1, 1)));
    }

    #[test]
    fn parse_hunk_header_no_count() {
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
    }

    #[test]
    fn parse_hunk_header_with_context() {
        assert_eq!(
            parse_hunk_header("@@ -10,6 +10,8 @@ fn some_function()"),
            Some((10, 10))
        );
    }

    #[test]
    fn parse_empty_diff() {
        let diff = parse_unified_diff("");
        assert!(diff.lines.is_empty());
        assert!(diff.file_start_indices.is_empty());
        assert!(diff.file_names.is_empty());
    }
}
