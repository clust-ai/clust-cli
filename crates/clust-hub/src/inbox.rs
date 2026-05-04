//! Background watcher for orchestrator inbox directories.
//!
//! Each orchestrator agent has an inbox at `~/.clust/inbox/<orch-id>/` that
//! contains `orch.json` (sidecar metadata, written at spawn) and — once the
//! orchestrator finishes — `manifest.json` with `"complete": true` listing the
//! emitted batch JSON files.
//!
//! The watcher walks the inbox root from disk (not from in-memory state), so
//! a hub restart cannot lose a completed manifest. To claim a manifest for
//! import it atomically renames `manifest.json` to `manifest.processing`
//! BEFORE doing any DB writes. This serves two purposes:
//!   1. Other scans on the same tick can't double-import.
//!   2. If the hub crashes mid-import, the marker survives and the next scan
//!      resumes (insertion is idempotent via the orchestrator_id/batch_file
//!      unique key).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use clust_ipc::batch_json::{BatchJson, OrchestratorManifest};
use tokio::sync::Notify;

use crate::agent::SharedHubState;
use crate::batch::{HubBatchEntry, HubBatchStatus, HubTaskEntry};
use crate::orchestrator::{self, COMMIT_SUFFIX};
use crate::orchestrator_validate;

const MANIFEST_FILENAME: &str = "manifest.json";
const MANIFEST_PROCESSING_FILENAME: &str = "manifest.processing";
const ERROR_LOG_FILENAME: &str = "error.log";

/// Inboxes older than this with no usable manifest are GC'd at startup.
const STALE_INBOX_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Spawn the polling watcher. Ticks every 5s and is also nudged immediately
/// whenever an orchestrator agent exits.
pub fn spawn_inbox_watcher(state: SharedHubState) {
    tokio::spawn(async move {
        let signal = {
            let hub = state.lock().await;
            hub.inbox_scan_signal.clone()
        };
        run_watcher(state, signal).await;
    });
}

async fn run_watcher(state: SharedHubState, signal: Arc<Notify>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = signal.notified() => {}
        }
        scan_inboxes(&state).await;
    }
}

/// Walk the inbox root and try to import any inbox with a complete manifest
/// (or a leftover `manifest.processing` marker from a crashed prior run).
async fn scan_inboxes(state: &SharedHubState) {
    let inbox_root = clust_ipc::clust_dir().join("inbox");
    let inbox_dirs = list_pending_inboxes(&inbox_root);
    for inbox_dir in inbox_dirs {
        try_import_inbox(state, &inbox_dir).await;
    }
}

/// List inbox dirs that may need processing: direct children of the inbox
/// root, excluding `.processed` and any name starting with `.`. Caller still
/// has to check whether each dir contains a usable manifest.
fn list_pending_inboxes(inbox_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(inbox_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        out.push(entry.path());
    }
    out
}

/// Try to claim and import a single inbox. Idempotent: if another scan
/// already claimed it, we skip silently.
async fn try_import_inbox(state: &SharedHubState, inbox_dir: &Path) {
    let processing_path = inbox_dir.join(MANIFEST_PROCESSING_FILENAME);
    let manifest_path = inbox_dir.join(MANIFEST_FILENAME);

    // Resumable case: a previous scan crashed after renaming but before
    // archiving. Re-import (DB inserts are deduped by orchestrator_id +
    // batch_file).
    let manifest = if processing_path.exists() {
        match read_manifest(&processing_path) {
            Ok(m) => m,
            Err(e) => {
                let _ = write_error_log(inbox_dir, &[format!("manifest unreadable: {e}")]);
                return;
            }
        }
    } else {
        let m = match read_manifest(&manifest_path) {
            Ok(m) => m,
            Err(_) => return, // partial write, missing, or invalid JSON — try next tick
        };
        if !m.complete {
            return;
        }
        // Atomic claim: rename manifest.json → manifest.processing. Failure
        // means someone else got it first, or the file vanished — skip.
        if fs::rename(&manifest_path, &processing_path).is_err() {
            return;
        }
        m
    };

    process_manifest(state, inbox_dir, &manifest).await;
}

fn read_manifest(path: &Path) -> Result<OrchestratorManifest, String> {
    let contents = fs::read_to_string(path).map_err(|e| format!("failed to read manifest: {e}"))?;
    serde_json::from_str(&contents).map_err(|e| format!("invalid manifest JSON: {e}"))
}

/// Resolve the orchestrator metadata for an inbox: prefer the on-disk
/// sidecar, fall back to the live in-memory entry, fail otherwise.
async fn resolve_orch_metadata(
    state: &SharedHubState,
    inbox_dir: &Path,
) -> Result<orchestrator::OrchestratorEntry, String> {
    if let Ok(side) = orchestrator::read_sidecar(inbox_dir) {
        return Ok(orchestrator::OrchestratorEntry::from_sidecar(
            side,
            inbox_dir.to_path_buf(),
        ));
    }
    let orch_id = inbox_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "inbox dir has no name".to_string())?;
    let hub = state.lock().await;
    hub.orchestrators
        .get(orch_id)
        .cloned()
        .ok_or_else(|| format!("no sidecar and no live entry for orchestrator {orch_id}"))
}

/// Validate the orchestrator's output, import valid batches, then terminate
/// the orchestrator and archive the inbox. Inputs come from the on-disk
/// sidecar (or live in-memory entry as fallback) — `inbox_dir` is the only
/// thing the watcher knows for sure.
async fn process_manifest(
    state: &SharedHubState,
    inbox_dir: &Path,
    manifest: &OrchestratorManifest,
) {
    let entry = match resolve_orch_metadata(state, inbox_dir).await {
        Ok(e) => e,
        Err(e) => {
            let _ = write_error_log(inbox_dir, &[e]);
            // Don't archive — leave the inbox in place. A future hub may have
            // the live entry; gc_orphan_inboxes handles truly stale dirs.
            return;
        }
    };

    let parse_result = parse_batches(inbox_dir, &manifest.batches);

    let (mut batches, mut errors) = match parse_result {
        Ok(b) => (b, Vec::<String>::new()),
        Err(parse_errs) => (Vec::new(), parse_errs),
    };

    // One DB read covers both dedup (skip already-imported files) and
    // validator preparation (don't flag the orchestrator's own earlier
    // imports as title collisions).
    let already = imported_for_orch(state, &entry.id).await;

    let mut filtered_batches: Vec<(String, BatchJson)> = Vec::new();
    for (i, batch) in batches.drain(..).enumerate() {
        let filename = manifest
            .batches
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("batch-{i}.json"));
        if already.files.contains(&filename) {
            continue;
        }
        filtered_batches.push((filename, batch));
    }

    if errors.is_empty() {
        let existing_titles: HashSet<String> = {
            let hub = state.lock().await;
            hub.queued_batches
                .iter()
                .map(|b| b.title.clone())
                .filter(|t| !already.titles.contains(t))
                .collect()
        };
        let to_validate: Vec<BatchJson> = filtered_batches.iter().map(|(_, b)| b.clone()).collect();
        let validation_errors =
            orchestrator_validate::validate_orchestrator_output(&to_validate, &existing_titles);
        if !validation_errors.is_empty() {
            errors.extend(validation_errors);
        }
    }

    if !errors.is_empty() {
        let _ = write_error_log(inbox_dir, &errors);
        finalize(state, &entry, false).await;
        return;
    }

    // Import: append commit suffix, register each batch, then inject the
    // manager task once the batch_id is known.
    for (filename, mut batch) in filtered_batches {
        append_commit_suffix(&mut batch);
        if let Err(e) = register_orchestrator_batch(state, &entry, &batch, &filename).await {
            errors.push(format!(
                "import of '{}' failed: {e}",
                batch.title.as_deref().unwrap_or("(untitled)")
            ));
        }
    }

    if !errors.is_empty() {
        let _ = write_error_log(inbox_dir, &errors);
    }

    finalize(state, &entry, errors.is_empty()).await;
}

/// Titles + source filenames of batches already imported under this
/// orchestrator id. A single DB read used for both re-import dedup and to
/// stop the validator from flagging the orchestrator's own earlier imports
/// as title collisions.
async fn imported_for_orch(state: &SharedHubState, orch_id: &str) -> crate::db::ImportedBatches {
    let hub = state.lock().await;
    let Some(ref db) = hub.db else {
        return crate::db::ImportedBatches::default();
    };
    crate::db::imported_batches_for_orch(db, orch_id).unwrap_or_default()
}

fn parse_batches(inbox: &Path, filenames: &[String]) -> Result<Vec<BatchJson>, Vec<String>> {
    let mut out = Vec::with_capacity(filenames.len());
    let mut errors = Vec::new();
    for name in filenames {
        if name.contains("..") || name.contains('/') {
            errors.push(format!("manifest filename '{name}' is not allowed"));
            continue;
        }
        let path = inbox.join(name);
        let contents = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("read '{name}': {e}"));
                continue;
            }
        };
        match serde_json::from_str::<BatchJson>(&contents) {
            Ok(b) => out.push(b),
            Err(e) => errors.push(format!("parse '{name}': {e}")),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

fn append_commit_suffix(batch: &mut BatchJson) {
    let new_suffix = match batch.suffix.take() {
        Some(existing) if !existing.trim().is_empty() => {
            format!("{existing}\n\n{COMMIT_SUFFIX}")
        }
        _ => COMMIT_SUFFIX.to_string(),
    };
    batch.suffix = Some(new_suffix);
}

/// Build the in-memory batch entry, persist it, and add to hub state.
/// Returns the assigned batch id. `source_filename` is the name of the JSON
/// file inside the orchestrator inbox (e.g. "batch-001.json"); it's stored on
/// the batch row so `DeleteBatch` can clean up the archived file.
async fn register_orchestrator_batch(
    state: &SharedHubState,
    orch: &orchestrator::OrchestratorEntry,
    batch: &BatchJson,
    source_filename: &str,
) -> Result<String, String> {
    let title = batch
        .title
        .clone()
        .ok_or_else(|| "batch is missing a title".to_string())?;

    // Allocate a batch id (collision-free against current state).
    let batch_id = {
        let hub = state.lock().await;
        let mut id;
        loop {
            id = format!("b{:05x}", rand::random::<u32>() & 0xFFFFF);
            if !hub.queued_batches.iter().any(|b| b.id == id) {
                break;
            }
        }
        id
    };

    // Build worker tasks first.
    let mut hub_tasks: Vec<HubTaskEntry> = batch
        .tasks
        .iter()
        .map(|t| HubTaskEntry {
            branch_name: t.branch.clone(),
            prompt: t.prompt.clone(),
            status: crate::batch::HubTaskStatus::Idle,
            agent_id: None,
            use_prefix: t.use_prefix,
            use_suffix: t.use_suffix,
            plan_mode: t.plan_mode || batch.plan_mode,
            is_manager: false,
        })
        .collect();

    // Inject the manager task at the front, now that we know the batch id.
    let worker_branches: Vec<String> = hub_tasks.iter().map(|t| t.branch_name.clone()).collect();
    let manager_task =
        orchestrator::build_manager_task(&batch_id, &title, &orch.target_branch, &worker_branches);
    hub_tasks.insert(0, manager_task);

    let launch_mode = batch
        .launch_mode
        .clone()
        .filter(|s| s == "auto" || s == "manual")
        .unwrap_or_else(|| "auto".to_string());

    let depends_on = batch.depends_on.clone();
    let max_concurrent = batch.max_concurrent;
    let plan_mode = batch.plan_mode;
    let allow_bypass = batch.allow_bypass;
    let prompt_prefix = batch.prefix.clone();
    let prompt_suffix = batch.suffix.clone();

    let depends_on_json = serde_json::to_string(&depends_on).unwrap_or_else(|_| "[]".to_string());
    let task_data: Vec<crate::db::InsertTaskRow> = hub_tasks
        .iter()
        .map(|t| {
            (
                t.branch_name.clone(),
                t.prompt.clone(),
                t.use_prefix,
                t.use_suffix,
                t.plan_mode,
                t.is_manager,
            )
        })
        .collect();

    // Persist to DB and push to in-memory state under one lock so partial
    // states (DB has it, memory doesn't, or vice versa) can't be observed.
    {
        let mut hub = state.lock().await;
        if let Some(ref db) = hub.db {
            let row = crate::db::QueuedBatchRow {
                id: batch_id.clone(),
                title: title.clone(),
                repo_path: orch.repo_path.clone(),
                target_branch: orch.target_branch.clone(),
                max_concurrent,
                prompt_prefix: prompt_prefix.clone(),
                prompt_suffix: prompt_suffix.clone(),
                plan_mode,
                allow_bypass,
                agent_binary: None,
                hub: clust_ipc::DEFAULT_HUB.to_string(),
                scheduled_at: None,
                status: "idle".to_string(),
                launch_mode: launch_mode.clone(),
                depends_on: depends_on_json,
                orchestrator_id: Some(orch.id.clone()),
                orchestrator_batch_file: Some(source_filename.to_string()),
            };
            crate::db::insert_queued_batch(db, &row, &task_data)?;
        }

        let entry = HubBatchEntry {
            id: batch_id.clone(),
            title,
            repo_path: orch.repo_path.clone(),
            target_branch: orch.target_branch.clone(),
            max_concurrent,
            prompt_prefix,
            prompt_suffix,
            plan_mode,
            allow_bypass,
            agent_binary: None,
            hub: clust_ipc::DEFAULT_HUB.to_string(),
            tasks: hub_tasks,
            scheduled_at: None,
            status: HubBatchStatus::Idle,
            launch_mode,
            depends_on,
        };
        hub.queued_batches.push(entry);
    }

    Ok(batch_id)
}

fn write_error_log(inbox: &Path, errors: &[String]) -> Result<(), String> {
    let path = inbox.join(ERROR_LOG_FILENAME);
    let body = errors.join("\n");
    fs::write(&path, body).map_err(|e| format!("write error log: {e}"))
}

/// Stop the orchestrator agent (best effort — may already be gone) and
/// archive the inbox.
async fn finalize(state: &SharedHubState, entry: &orchestrator::OrchestratorEntry, success: bool) {
    let _ = crate::agent::stop_agent(state, &entry.agent_id).await;
    {
        let mut hub = state.lock().await;
        hub.orchestrators.remove(&entry.id);
    }
    let _ = archive_inbox(&entry.inbox_dir, success);
}

fn archive_inbox(inbox_dir: &Path, _success: bool) -> Result<(), String> {
    let processed = orchestrator::processed_dir();
    fs::create_dir_all(&processed).map_err(|e| format!("create processed dir: {e}"))?;
    let name = inbox_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("orphaned");
    let dest = processed.join(name);
    if dest.exists() {
        // Should be essentially impossible (orch ids are random 12-char hex);
        // if it happens, fold into the existing dir so JSON files aren't lost.
        return merge_into_existing(inbox_dir, &dest);
    }
    fs::rename(inbox_dir, &dest).map_err(|e| format!("archive inbox: {e}"))?;
    Ok(())
}

fn merge_into_existing(src: &Path, dst: &Path) -> Result<(), String> {
    let Ok(entries) = fs::read_dir(src) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let target = dst.join(entry.file_name());
        let _ = fs::rename(entry.path(), &target);
    }
    let _ = fs::remove_dir(src);
    Ok(())
}

/// On hub startup, archive abandoned inboxes whose grace window has expired.
pub fn gc_orphan_inboxes(_state: &mut crate::agent::HubState) {
    let inbox_root: PathBuf = clust_ipc::clust_dir().join("inbox");
    let processed = orchestrator::processed_dir();
    gc_orphan_inboxes_in(&inbox_root, &processed, STALE_INBOX_GRACE);
}

/// Inner helper for `gc_orphan_inboxes`. Conservative compared to the prior
/// implementation:
///   - Never touch a dir with a complete `manifest.json` or a leftover
///     `manifest.processing` marker — the watcher will pick those up.
///   - Only archive dirs that have no usable manifest AND are older than
///     `grace` (mtime), so a still-running orchestrator from a previous hub
///     session isn't yanked out from under itself.
fn gc_orphan_inboxes_in(inbox_root: &Path, processed_root: &Path, grace: Duration) {
    let Ok(entries) = fs::read_dir(inbox_root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();

        if path.join(MANIFEST_PROCESSING_FILENAME).exists() {
            continue;
        }
        if let Ok(m) = read_manifest(&path.join(MANIFEST_FILENAME)) {
            if m.complete {
                continue;
            }
        }

        let mtime = meta.modified().unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or_default();
        if age < grace {
            continue;
        }
        if fs::create_dir_all(processed_root).is_err() {
            continue;
        }
        let mut dest = processed_root.join(format!("orphaned-{name_str}"));
        let mut counter = 1;
        while dest.exists() {
            dest = processed_root.join(format!("orphaned-{name_str}-{counter}"));
            counter += 1;
        }
        let _ = fs::rename(&path, dest);
    }
}

/// Delete a single batch's source JSON file from the archived inbox at
/// `~/.clust/inbox/.processed/<orch_id>/<filename>`. If that was the last
/// `*.json` file in the dir (besides the manifest itself), also delete the
/// manifest, error log, and the dir itself. All operations are best-effort:
/// missing files do not produce an error.
pub fn delete_batch_source_file(orch_id: &str, filename: &str) {
    let processed = orchestrator::processed_dir();
    delete_batch_source_file_in(&processed, orch_id, filename);
}

fn delete_batch_source_file_in(processed_root: &Path, orch_id: &str, filename: &str) {
    if orch_id.is_empty() || filename.is_empty() {
        return;
    }
    if orch_id.contains('/') || orch_id.contains("..") {
        return;
    }
    if filename.contains('/') || filename.contains("..") {
        return;
    }
    let dir = processed_root.join(orch_id);
    let target = dir.join(filename);
    let _ = fs::remove_file(&target);

    let mut has_other_json = false;
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let n = e.file_name();
            let ns = n.to_string_lossy();
            if ns == MANIFEST_FILENAME
                || ns == MANIFEST_PROCESSING_FILENAME
                || ns == orchestrator::SIDECAR_FILENAME
                || ns == ERROR_LOG_FILENAME
            {
                continue;
            }
            if ns.ends_with(".json") {
                has_other_json = true;
                break;
            }
        }
    }
    if !has_other_json {
        let _ = fs::remove_file(dir.join(MANIFEST_FILENAME));
        let _ = fs::remove_file(dir.join(MANIFEST_PROCESSING_FILENAME));
        let _ = fs::remove_file(dir.join(orchestrator::SIDECAR_FILENAME));
        let _ = fs::remove_file(dir.join(ERROR_LOG_FILENAME));
        let _ = fs::remove_dir(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn complete_manifest_json() -> &'static str {
        r#"{"version":1,"complete":true,"batches":["batch-001.json"]}"#
    }

    #[test]
    fn gc_skips_dir_with_complete_manifest() {
        let root = tempfile::tempdir().unwrap();
        let inbox_root = root.path().join("inbox");
        let processed = inbox_root.join(".processed");
        let live = inbox_root.join("oabcdef123456");
        write(&live.join(MANIFEST_FILENAME), complete_manifest_json());

        // Force the dir mtime to be ancient so only the manifest check matters.
        gc_orphan_inboxes_in(&inbox_root, &processed, Duration::from_secs(0));

        // Should still be in place — the watcher must get a chance to import it.
        assert!(live.exists(), "complete-manifest dir was archived");
    }

    #[test]
    fn gc_skips_dir_with_processing_marker() {
        let root = tempfile::tempdir().unwrap();
        let inbox_root = root.path().join("inbox");
        let processed = inbox_root.join(".processed");
        let live = inbox_root.join("o111111111111");
        write(&live.join(MANIFEST_PROCESSING_FILENAME), "{}");

        gc_orphan_inboxes_in(&inbox_root, &processed, Duration::from_secs(0));

        assert!(live.exists(), "processing-marker dir was archived");
    }

    #[test]
    fn gc_archives_stale_empty_inbox() {
        let root = tempfile::tempdir().unwrap();
        let inbox_root = root.path().join("inbox");
        let processed = inbox_root.join(".processed");
        let stale = inbox_root.join("ostale1234567");
        fs::create_dir_all(&stale).unwrap();
        // Drop a junk file so we can recognize the dir post-archive.
        write(&stale.join("note.txt"), "no manifest here");

        gc_orphan_inboxes_in(&inbox_root, &processed, Duration::from_secs(0));

        assert!(!stale.exists(), "stale inbox was not archived");
        let archived = processed.join("orphaned-ostale1234567");
        assert!(
            archived.exists(),
            "stale inbox not in expected archive path"
        );
        assert!(archived.join("note.txt").exists());
    }

    #[test]
    fn gc_leaves_fresh_inbox_within_grace() {
        let root = tempfile::tempdir().unwrap();
        let inbox_root = root.path().join("inbox");
        let processed = inbox_root.join(".processed");
        let fresh = inbox_root.join("ofresh1234567");
        fs::create_dir_all(&fresh).unwrap();

        // Long grace — the just-created dir must not be touched.
        gc_orphan_inboxes_in(&inbox_root, &processed, Duration::from_secs(3600));

        assert!(
            fresh.exists(),
            "fresh inbox archived inside its grace window"
        );
    }

    #[test]
    fn delete_removes_file_and_keeps_dir_when_other_batches_remain() {
        let root = tempfile::tempdir().unwrap();
        let processed = root.path().to_path_buf();
        let orch = processed.join("o222222222222");
        write(&orch.join(MANIFEST_FILENAME), complete_manifest_json());
        write(&orch.join("batch-001.json"), "{}");
        write(&orch.join("batch-002.json"), "{}");

        delete_batch_source_file_in(&processed, "o222222222222", "batch-001.json");

        assert!(!orch.join("batch-001.json").exists());
        assert!(orch.join("batch-002.json").exists());
        assert!(orch.exists(), "dir removed even though batch-002 remains");
    }

    #[test]
    fn delete_removes_dir_when_last_batch_gone() {
        let root = tempfile::tempdir().unwrap();
        let processed = root.path().to_path_buf();
        let orch = processed.join("o333333333333");
        write(&orch.join(MANIFEST_FILENAME), complete_manifest_json());
        write(&orch.join(orchestrator::SIDECAR_FILENAME), "{}");
        write(&orch.join("batch-001.json"), "{}");

        delete_batch_source_file_in(&processed, "o333333333333", "batch-001.json");

        assert!(!orch.exists(), "dir not pruned after last batch deletion");
    }

    #[test]
    fn delete_rejects_path_traversal() {
        let root = tempfile::tempdir().unwrap();
        let processed = root.path().to_path_buf();
        let orch = processed.join("o444444444444");
        write(&orch.join("batch-001.json"), "{}");
        let outside = root.path().join("outside.json");
        write(&outside, "should survive");

        delete_batch_source_file_in(&processed, "o444444444444", "../outside.json");
        assert!(outside.exists(), "path traversal was not rejected");

        delete_batch_source_file_in(&processed, "../escape", "batch-001.json");
        assert!(
            orch.join("batch-001.json").exists(),
            "orch_id traversal not rejected"
        );
    }
}
