use std::collections::{HashMap, HashSet};

use clust_ipc::batch_json::BatchJson;

/// Validate an orchestrator's emitted batches before importing them.
///
/// `existing_batch_titles` is the set of batch titles already in the hub —
/// `depends_on` references that match an existing batch are accepted.
///
/// Returns a list of human-readable errors. Empty list = valid.
pub fn validate_orchestrator_output(
    batches: &[BatchJson],
    existing_batch_titles: &HashSet<String>,
) -> Vec<String> {
    let mut errors = Vec::new();

    if batches.is_empty() {
        errors.push("orchestrator manifest references no batches".to_string());
        return errors;
    }

    // Index titles for cross-reference + duplicate detection.
    let mut title_counts: HashMap<&str, usize> = HashMap::new();
    for batch in batches {
        if let Some(title) = batch.title.as_deref() {
            *title_counts.entry(title).or_insert(0) += 1;
        }
    }
    let titles: HashSet<&str> = title_counts.keys().copied().collect();

    let mut all_branches: HashSet<&str> = HashSet::new();

    for (idx, batch) in batches.iter().enumerate() {
        let label_owned: String;
        let label: &str = match batch.title.as_deref() {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                label_owned = format!("batch[{idx}]");
                &label_owned
            }
        };

        match batch.title.as_deref() {
            None => errors.push(format!("{label}: title is required")),
            Some(t) if t.trim().is_empty() => {
                errors.push(format!("{label}: title is empty"));
            }
            Some(t) => {
                if title_counts.get(t).copied().unwrap_or(0) > 1 {
                    errors.push(format!("{label}: duplicate title '{t}'"));
                }
                if existing_batch_titles.contains(t) {
                    errors.push(format!(
                        "{label}: title '{t}' collides with an existing hub batch"
                    ));
                }
            }
        }

        if batch.tasks.is_empty() {
            errors.push(format!("{label}: must have at least one task"));
        }

        for (ti, task) in batch.tasks.iter().enumerate() {
            if task.is_manager {
                errors.push(format!(
                    "{label}: task[{ti}] sets is_manager (reserved internal flag)"
                ));
            }
            if task.prompt.trim().is_empty() {
                errors.push(format!("{label}: task[{ti}] prompt is empty"));
            }
            if !is_valid_branch_name(&task.branch) {
                errors.push(format!(
                    "{label}: task[{ti}] invalid branch name '{}'",
                    task.branch
                ));
            }
            if task.branch.starts_with("manager/") {
                errors.push(format!(
                    "{label}: task[{ti}] branch starts with reserved 'manager/' prefix"
                ));
            }
            if !all_branches.insert(task.branch.as_str()) {
                errors.push(format!(
                    "{label}: task[{ti}] branch '{}' duplicated across orchestrator output",
                    task.branch
                ));
            }
        }

        for dep in &batch.depends_on {
            if !titles.contains(dep.as_str()) && !existing_batch_titles.contains(dep) {
                errors.push(format!(
                    "{label}: depends_on references unknown batch '{dep}'"
                ));
            }
        }
    }

    if has_cycle(batches) {
        errors.push("dependency graph has a cycle".to_string());
    }

    errors
}

/// Validate a git branch name: ASCII letters/digits and `/-_.` only,
/// no leading `-`, no `..`, no trailing `/`. Not a complete `git check-ref-format`
/// implementation — just enough to catch typical mistakes.
pub fn is_valid_branch_name(s: &str) -> bool {
    if s.is_empty() || s.starts_with('-') || s.ends_with('/') {
        return false;
    }
    if s.contains("..") || s.contains(' ') || s.contains('~') || s.contains('^') || s.contains(':')
    {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

/// DFS cycle detection over the orchestrator-output DAG.
fn has_cycle(batches: &[BatchJson]) -> bool {
    let title_to_idx: HashMap<&str, usize> = batches
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.title.as_deref().map(|t| (t, i)))
        .collect();

    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Unvisited,
        InStack,
        Done,
    }

    let mut marks = vec![Mark::Unvisited; batches.len()];

    fn dfs(
        idx: usize,
        batches: &[BatchJson],
        title_to_idx: &HashMap<&str, usize>,
        marks: &mut [Mark],
    ) -> bool {
        marks[idx] = Mark::InStack;
        for dep in &batches[idx].depends_on {
            if let Some(&dep_idx) = title_to_idx.get(dep.as_str()) {
                match marks[dep_idx] {
                    Mark::InStack => return true,
                    Mark::Unvisited => {
                        if dfs(dep_idx, batches, title_to_idx, marks) {
                            return true;
                        }
                    }
                    Mark::Done => {}
                }
            }
        }
        marks[idx] = Mark::Done;
        false
    }

    for i in 0..batches.len() {
        if marks[i] == Mark::Unvisited && dfs(i, batches, &title_to_idx, &mut marks) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use clust_ipc::batch_json::TaskJson;

    fn task(branch: &str, prompt: &str) -> TaskJson {
        TaskJson {
            branch: branch.to_string(),
            prompt: prompt.to_string(),
            use_prefix: true,
            use_suffix: true,
            plan_mode: false,
            is_manager: false,
        }
    }

    fn batch(title: &str, tasks: Vec<TaskJson>, depends_on: Vec<String>) -> BatchJson {
        BatchJson {
            title: Some(title.to_string()),
            prefix: None,
            suffix: None,
            launch_mode: None,
            max_concurrent: None,
            plan_mode: false,
            allow_bypass: false,
            tasks,
            depends_on,
        }
    }

    #[test]
    fn valid_simple() {
        let b = vec![batch(
            "Models",
            vec![task("feat/x/models/users", "do users")],
            vec![],
        )];
        let existing = HashSet::new();
        let errs = validate_orchestrator_output(&b, &existing);
        assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
    }

    #[test]
    fn rejects_empty_prompt() {
        let b = vec![batch("X", vec![task("feat/x/y", "  ")], vec![])];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("prompt is empty")));
    }

    #[test]
    fn rejects_is_manager() {
        let mut t = task("feat/x/y", "do it");
        t.is_manager = true;
        let b = vec![batch("X", vec![t], vec![])];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("reserved")));
    }

    #[test]
    fn rejects_manager_branch() {
        let b = vec![batch("X", vec![task("manager/something", "do it")], vec![])];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("manager/")));
    }

    #[test]
    fn rejects_duplicate_branch() {
        let b = vec![
            batch("A", vec![task("feat/x/y", "do")], vec![]),
            batch("B", vec![task("feat/x/y", "do")], vec![]),
        ];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("duplicated")));
    }

    #[test]
    fn rejects_invalid_branch_name() {
        let b = vec![batch("X", vec![task("bad branch~name", "do")], vec![])];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("invalid branch name")));
    }

    #[test]
    fn rejects_dangling_depends_on() {
        let b = vec![batch(
            "X",
            vec![task("feat/x/y", "do")],
            vec!["NotThere".to_string()],
        )];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("unknown batch")));
    }

    #[test]
    fn accepts_existing_batch_dep() {
        let b = vec![batch(
            "X",
            vec![task("feat/x/y", "do")],
            vec!["Older".to_string()],
        )];
        let mut existing = HashSet::new();
        existing.insert("Older".to_string());
        let errs = validate_orchestrator_output(&b, &existing);
        assert!(errs.is_empty(), "got: {errs:?}");
    }

    #[test]
    fn rejects_cycle() {
        let b = vec![
            batch("A", vec![task("feat/a", "do")], vec!["B".to_string()]),
            batch("B", vec![task("feat/b", "do")], vec!["A".to_string()]),
        ];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("cycle")));
    }

    #[test]
    fn rejects_self_loop() {
        let b = vec![batch(
            "A",
            vec![task("feat/a", "do")],
            vec!["A".to_string()],
        )];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("cycle")));
    }

    #[test]
    fn rejects_duplicate_title() {
        let b = vec![
            batch("X", vec![task("feat/a", "do")], vec![]),
            batch("X", vec![task("feat/b", "do")], vec![]),
        ];
        let errs = validate_orchestrator_output(&b, &HashSet::new());
        assert!(errs.iter().any(|e| e.contains("duplicate title")));
    }

    #[test]
    fn rejects_collision_with_existing() {
        let b = vec![batch("Existing", vec![task("feat/a", "do")], vec![])];
        let mut existing = HashSet::new();
        existing.insert("Existing".to_string());
        let errs = validate_orchestrator_output(&b, &existing);
        assert!(errs.iter().any(|e| e.contains("collides")));
    }

    #[test]
    fn branch_validation() {
        assert!(is_valid_branch_name("feat/x"));
        assert!(is_valid_branch_name("a-b_c.d/e"));
        assert!(!is_valid_branch_name(""));
        assert!(!is_valid_branch_name("-foo"));
        assert!(!is_valid_branch_name("foo/"));
        assert!(!is_valid_branch_name("foo..bar"));
        assert!(!is_valid_branch_name("foo bar"));
        assert!(!is_valid_branch_name("foo:bar"));
    }
}
