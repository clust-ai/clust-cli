/// A known agent binary with display metadata.
pub struct KnownAgent {
    pub binary: &'static str,
    pub display_name: &'static str,
}

/// Built-in agent registry.
pub const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent {
        binary: "claude",
        display_name: "Claude",
    },
    KnownAgent {
        binary: "opencode",
        display_name: "OpenCode",
    },
    KnownAgent {
        binary: "aider",
        display_name: "Aider",
    },
    KnownAgent {
        binary: "codex",
        display_name: "Codex",
    },
];
