/// A known agent binary with display metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownAgent {
    pub binary: &'static str,
    pub display_name: &'static str,
    /// CLI args to append when accept-edits mode is requested.
    /// `None` means this agent does not support the feature.
    pub accept_edits_args: Option<&'static [&'static str]>,
    /// Whether this agent has been tested with clust.
    pub tested: bool,
}

/// Built-in agent registry.
pub const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent {
        binary: "claude",
        display_name: "Claude Code",
        accept_edits_args: Some(&["--permission-mode", "acceptEdits"]),
        tested: true,
    },
    KnownAgent {
        binary: "opencode",
        display_name: "Open Code",
        accept_edits_args: None,
        tested: true,
    },
    KnownAgent {
        binary: "aider",
        display_name: "Aider",
        accept_edits_args: None,
        tested: false,
    },
    KnownAgent {
        binary: "codex",
        display_name: "Codex",
        accept_edits_args: None,
        tested: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_agents_contains_expected_entries() {
        let binaries: Vec<&str> = KNOWN_AGENTS.iter().map(|a| a.binary).collect();
        assert!(binaries.contains(&"claude"));
        assert!(binaries.contains(&"opencode"));
        assert!(binaries.contains(&"aider"));
        assert!(binaries.contains(&"codex"));
    }

    #[test]
    fn accept_edits_args_for_claude_returns_args() {
        let args = accept_edits_args_for("claude");
        assert_eq!(args, Some(["--permission-mode", "acceptEdits"].as_slice()));
    }

    #[test]
    fn accept_edits_args_for_agent_without_support_returns_none() {
        assert_eq!(accept_edits_args_for("aider"), None);
        assert_eq!(accept_edits_args_for("opencode"), None);
        assert_eq!(accept_edits_args_for("codex"), None);
    }

    #[test]
    fn accept_edits_args_for_unknown_agent_returns_none() {
        assert_eq!(accept_edits_args_for("unknown-binary"), None);
        assert_eq!(accept_edits_args_for(""), None);
    }

    #[test]
    fn tested_agents_are_correct() {
        let tested: Vec<&str> = KNOWN_AGENTS
            .iter()
            .filter(|a| a.tested)
            .map(|a| a.binary)
            .collect();
        assert!(tested.contains(&"claude"));
        assert!(tested.contains(&"opencode"));
        assert_eq!(tested.len(), 2);
    }
}
