/// A known agent binary with display metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownAgent {
    pub binary: &'static str,
    pub display_name: &'static str,
    /// CLI args to append when accept-edits mode is requested.
    /// `None` means this agent does not support the feature.
    pub accept_edits_args: Option<&'static [&'static str]>,
}

/// Built-in agent registry.
pub const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent {
        binary: "claude",
        display_name: "Claude",
        accept_edits_args: Some(&["--permission-mode", "acceptEdits"]),
    },
    KnownAgent {
        binary: "opencode",
        display_name: "OpenCode",
        accept_edits_args: None,
    },
    KnownAgent {
        binary: "aider",
        display_name: "Aider",
        accept_edits_args: None,
    },
    KnownAgent {
        binary: "codex",
        display_name: "Codex",
        accept_edits_args: None,
    },
];

/// Look up the accept-edits CLI args for a known agent binary.
/// Returns `None` if the binary is unknown or does not support the feature.
pub fn accept_edits_args_for(binary: &str) -> Option<&'static [&'static str]> {
    KNOWN_AGENTS
        .iter()
        .find(|a| a.binary == binary)
        .and_then(|a| a.accept_edits_args)
}
