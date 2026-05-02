//! Background watcher for orchestrator inbox directories.
//!
//! Each live orchestrator agent has an inbox at `~/.clust/inbox/<orch-id>/`.
//! When the orchestrator writes `manifest.json` with `"complete": true`, this
//! watcher imports the referenced batch JSON files (validating them, injecting
//! per-batch manager tasks, appending the commit suffix), terminates the
//! orchestrator agent, and archives the inbox under `.processed/`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clust_ipc::batch_json::{BatchJson, OrchestratorManifest};

use crate::agent::SharedHubState;
use crate::batch::{HubBatchEntry, HubBatchStatus, HubTaskEntry};
use crate::orchestrator::{self, COMMIT_SUFFIX};
use crate::orchestrator_validate;

/// Spawn the polling watcher. Runs every 5 seconds (matches batch timer).
pub fn spawn_inbox_watcher(state: SharedHubState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            scan_inboxes(&state).await;
        }
    });
}

/// Visit each registered orchestrator's inbox; if a manifest is ready, import.
async fn scan_inboxes(state: &SharedHubState) {
    // Snapshot ids without holding the lock during disk I/O.
    let orch_ids: Vec<String> = {
        let hub = state.lock().await;
        hub.orchestrators.keys().cloned().collect()
    };

    for orch_id in orch_ids {
        let entry = {
            let hub = state.lock().await;
            hub.orchestrators.get(&orch_id).cloned()
        };
        let Some(entry) = entry else { continue };

        let manifest_path = entry.inbox_dir.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        let manifest = match read_manifest(&manifest_path) {
            Ok(m) => m,
            Err(_) => continue, // partial write — try again next tick
        };
        if !manifest.complete {
            continue;
        }

        // Take ownership: remove entry now so we don't import twice.
        {
            let mut hub = state.lock().await;
            hub.orchestrators.remove(&orch_id);
        }

        process_manifest(state, &entry, &manifest).await;
    }
}

fn read_manifest(path: &Path) -> Result<OrchestratorManifest, String> {
    let contents = fs::read_to_string(path).map_err(|e| format!("failed to read manifest: {e}"))?;
    serde_json::from_str(&contents).map_err(|e| format!("invalid manifest JSON: {e}"))
}

/// Validate the orchestrator's output, import valid batches, then terminate
/// the orchestrator and archive the inbox.
async fn process_manifest(
    state: &SharedHubState,
    entry: &orchestrator::OrchestratorEntry,
    manifest: &OrchestratorManifest,
) {
    let parse_result = parse_batches(&entry.inbox_dir, &manifest.batches);

    let (mut batches, mut errors) = match parse_result {
        Ok(b) => (b, Vec::<String>::new()),
        Err(parse_errs) => (Vec::new(), parse_errs),
    };

    if errors.is_empty() {
        let existing_titles: HashSet<String> = {
            let hub = state.lock().await;
            hub.queued_batches.iter().map(|b| b.title.clone()).collect()
        };
        let validation_errors =
            orchestrator_validate::validate_orchestrator_output(&batches, &existing_titles);
        if !validation_errors.is_empty() {
            errors.extend(validation_errors);
        }
    }

    if !errors.is_empty() {
        let _ = write_error_log(&entry.inbox_dir, &errors);
        finalize(state, entry, false).await;
        return;
    }

    // Import: append commit suffix, register each batch, then inject the
    // manager task once the batch_id is known.
    let mut imported_ids: Vec<String> = Vec::new();
    for batch in batches.iter_mut() {
        append_commit_suffix(batch);
        match register_orchestrator_batch(state, entry, batch).await {
            Ok(id) => imported_ids.push(id),
            Err(e) => errors.push(format!(
                "import of '{}' failed: {e}",
                batch.title.as_deref().unwrap_or("(untitled)")
            )),
        }
    }

    if !errors.is_empty() {
        let _ = write_error_log(&entry.inbox_dir, &errors);
    }

    finalize(state, entry, errors.is_empty()).await;
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
/// Returns the assigned batch id.
async fn register_orchestrator_batch(
    state: &SharedHubState,
    orch: &orchestrator::OrchestratorEntry,
    batch: &BatchJson,
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

    // Persist to DB before pushing to in-memory state.
    {
        let hub = state.lock().await;
        if let Some(ref db) = hub.db {
            let depends_on_json =
                serde_json::to_string(&depends_on).unwrap_or_else(|_| "[]".to_string());
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
            };
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
            crate::db::insert_queued_batch(db, &row, &task_data)?;
        }
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

    {
        let mut hub = state.lock().await;
        hub.queued_batches.push(entry);
    }

    Ok(batch_id)
}

fn write_error_log(inbox: &Path, errors: &[String]) -> Result<(), String> {
    let path = inbox.join("error.log");
    let body = errors.join("\n");
    fs::write(&path, body).map_err(|e| format!("write error log: {e}"))
}

/// Stop the orchestrator agent and archive the inbox.
async fn finalize(state: &SharedHubState, entry: &orchestrator::OrchestratorEntry, success: bool) {
    let _ = crate::agent::stop_agent(state, &entry.agent_id).await;
    let _ = archive_inbox(&entry.inbox_dir, success);
}

fn archive_inbox(inbox_dir: &Path, _success: bool) -> Result<(), String> {
    let processed = orchestrator::processed_dir();
    fs::create_dir_all(&processed).map_err(|e| format!("create processed dir: {e}"))?;
    let name = inbox_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("orphaned");
    let mut dest = processed.join(name);
    let mut counter = 1;
    while dest.exists() {
        dest = processed.join(format!("{name}-{counter}"));
        counter += 1;
    }
    fs::rename(inbox_dir, &dest).map_err(|e| format!("archive inbox: {e}"))?;
    Ok(())
}

/// On hub startup, move any inbox dirs that don't correspond to a live
/// orchestrator into `.processed/orphaned-*` so they don't accumulate.
pub fn gc_orphan_inboxes(state: &mut crate::agent::HubState) {
    let inbox_root: PathBuf = clust_ipc::clust_dir().join("inbox");
    let Ok(entries) = fs::read_dir(&inbox_root) else {
        return;
    };
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
        if state.orchestrators.contains_key(name_str.as_ref()) {
            continue;
        }
        let processed = orchestrator::processed_dir();
        if fs::create_dir_all(&processed).is_err() {
            continue;
        }
        let mut dest = processed.join(format!("orphaned-{name_str}"));
        let mut counter = 1;
        while dest.exists() {
            dest = processed.join(format!("orphaned-{name_str}-{counter}"));
            counter += 1;
        }
        let _ = fs::rename(entry.path(), dest);
    }
}
