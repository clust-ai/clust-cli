//! Batch JSON schema. Shared between `clust-cli` (for human-authored imports)
//! and `clust-hub` (for orchestrator-emitted imports).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatchJson {
    /// Optional batch title. Falls back to auto-naming ("Batch N") if omitted
    /// for human imports; required for orchestrator output.
    pub title: Option<String>,
    /// Optional prompt prefix prepended to every task prompt.
    pub prefix: Option<String>,
    /// Optional prompt suffix appended to every task prompt.
    pub suffix: Option<String>,
    /// Launch mode: "auto" (default) or "manual".
    pub launch_mode: Option<String>,
    /// Max concurrent agents (auto mode only). Null/omitted = unlimited.
    pub max_concurrent: Option<usize>,
    /// Whether agents start in plan mode.
    #[serde(default)]
    pub plan_mode: bool,
    /// Whether agents can bypass permission prompts.
    #[serde(default)]
    pub allow_bypass: bool,
    /// The tasks to create in this batch.
    pub tasks: Vec<TaskJson>,
    /// Optional list of batch titles this batch depends on.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskJson {
    /// Branch name for the worktree.
    pub branch: String,
    /// The prompt for the agent.
    pub prompt: String,
    /// Whether the batch prompt prefix is applied to this task. Defaults to `true`.
    #[serde(default = "default_true")]
    pub use_prefix: bool,
    /// Whether the batch prompt suffix is applied to this task. Defaults to `true`.
    #[serde(default = "default_true")]
    pub use_suffix: bool,
    /// Whether this task starts in plan mode. Defaults to batch-level plan_mode if omitted.
    #[serde(default)]
    pub plan_mode: bool,
    /// Reserved internal flag — auto-injected manager tasks set this to true.
    /// Human-authored JSON must NOT set this; orchestrator validation rejects it.
    #[serde(default)]
    pub is_manager: bool,
}

fn default_true() -> bool {
    true
}

/// Manifest written by an orchestrator agent to signal "I'm done — import these batches."
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OrchestratorManifest {
    pub version: u32,
    #[serde(default)]
    pub complete: bool,
    pub batches: Vec<String>,
}
