use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "clust", version, about = "Agent manager CLI")]
pub struct Cli {
    /// Start agent without attaching (returns the agent ID)
    #[arg(short = 'b', long = "background")]
    pub background: bool,

    /// Attach to an existing agent by its 6-char ID
    #[arg(short = 'a', long = "attach")]
    pub attach: Option<String>,

    /// Stop: without value stops the hub daemon; with a 6-char ID stops that agent
    #[arg(short = 's', long = "stop", num_args = 0..=1, default_missing_value = "")]
    pub stop: Option<String>,

    /// Interactive picker to set the default agent
    #[arg(short = 'd', long = "default")]
    pub default: bool,

    /// Auto-accept edits (agent-specific, e.g. --permission-mode acceptEdits for Claude)
    #[arg(short = 'e', long = "accept-edits")]
    pub accept_edits: bool,

    /// Use a specific agent for this session (does not change the default)
    #[arg(short = 'u', long = "use")]
    pub use_agent: Option<String>,

    /// Assign the agent to a named hub (snake_case; default: default_hub)
    #[arg(short = 'H', long = "hub")]
    pub hub: Option<String>,

    /// Initial prompt for the agent
    pub prompt: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all running agents in the hub
    Ls {
        /// Interactive selector: navigate with arrow keys, Enter to confirm
        #[arg(short = 'i', long = "select")]
        select: bool,

        /// Filter agents by hub name
        #[arg(short = 'H', long = "hub")]
        hub: Option<String>,
    },
    /// Open the Clust terminal UI
    Ui,
    /// Repository management
    Repo {
        /// Register the current directory's git repository for tracking
        #[arg(short = 'R', long = "register")]
        register: bool,

        /// Remove a repository from clust tracking (stops all agents first)
        #[arg(short = 'r', long = "remove")]
        remove: bool,

        /// Stop all agents running on the current repository
        #[arg(short = 's', long = "stop")]
        stop: bool,
    },
}

/// Validate a hub name follows snake_case: starts with a lowercase ASCII letter,
/// followed by zero or more lowercase ASCII letters, digits, or underscores.
pub fn validate_hub_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("hub name cannot be empty".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return Err(format!(
            "hub name must start with a lowercase letter, got '{first}'"
        ));
    }
    if name.ends_with('_') {
        return Err("hub name must not end with an underscore".into());
    }
    if name.contains("__") {
        return Err("hub name must not contain consecutive underscores".into());
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' {
            return Err(format!(
                "hub name must be snake_case (lowercase, digits, underscores), found '{c}'"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_no_args() {
        let cli = Cli::try_parse_from(["clust"]).unwrap();
        assert!(!cli.background);
        assert!(cli.stop.is_none());
        assert!(cli.attach.is_none());
        assert!(!cli.default);
        assert!(!cli.accept_edits);
        assert!(cli.prompt.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_stop_short() {
        let cli = Cli::try_parse_from(["clust", "-s", "abc123"]).unwrap();
        assert_eq!(cli.stop.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_stop_long() {
        let cli = Cli::try_parse_from(["clust", "--stop", "abc123"]).unwrap();
        assert_eq!(cli.stop.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_stop_no_value_stops_hub() {
        let cli = Cli::try_parse_from(["clust", "-s"]).unwrap();
        assert_eq!(cli.stop.as_deref(), Some(""));
    }

    #[test]
    fn parse_stop_long_no_value() {
        let cli = Cli::try_parse_from(["clust", "--stop"]).unwrap();
        assert_eq!(cli.stop.as_deref(), Some(""));
    }

    #[test]
    fn parse_background_short() {
        let cli = Cli::try_parse_from(["clust", "-b"]).unwrap();
        assert!(cli.background);
    }

    #[test]
    fn parse_background_long() {
        let cli = Cli::try_parse_from(["clust", "--background"]).unwrap();
        assert!(cli.background);
    }

    #[test]
    fn parse_attach_short() {
        let cli = Cli::try_parse_from(["clust", "-a", "abc123"]).unwrap();
        assert_eq!(cli.attach.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_attach_long() {
        let cli = Cli::try_parse_from(["clust", "--attach", "abc123"]).unwrap();
        assert_eq!(cli.attach.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_default_short() {
        let cli = Cli::try_parse_from(["clust", "-d"]).unwrap();
        assert!(cli.default);
    }

    #[test]
    fn parse_default_long() {
        let cli = Cli::try_parse_from(["clust", "--default"]).unwrap();
        assert!(cli.default);
    }

    #[test]
    fn parse_prompt() {
        let cli = Cli::try_parse_from(["clust", "do something"]).unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("do something"));
    }

    #[test]
    fn parse_subcommand_ls() {
        let cli = Cli::try_parse_from(["clust", "ls"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Ls {
                select: false,
                hub: None
            })
        ));
    }

    #[test]
    fn parse_ls_select_short() {
        let cli = Cli::try_parse_from(["clust", "ls", "-i"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Ls {
                select: true,
                hub: None
            })
        ));
    }

    #[test]
    fn parse_ls_select_long() {
        let cli = Cli::try_parse_from(["clust", "ls", "--select"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Ls {
                select: true,
                hub: None
            })
        ));
    }

    #[test]
    fn parse_subcommand_ui() {
        let cli = Cli::try_parse_from(["clust", "ui"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Ui)));
    }

    #[test]
    fn parse_background_with_prompt() {
        let cli = Cli::try_parse_from(["clust", "-b", "run tests"]).unwrap();
        assert!(cli.background);
        assert_eq!(cli.prompt.as_deref(), Some("run tests"));
    }

    #[test]
    fn parse_dot_as_prompt() {
        let cli = Cli::try_parse_from(["clust", "."]).unwrap();
        assert_eq!(cli.prompt.as_deref(), Some("."));
    }

    #[test]
    fn parse_accept_edits_short() {
        let cli = Cli::try_parse_from(["clust", "-e"]).unwrap();
        assert!(cli.accept_edits);
    }

    #[test]
    fn parse_accept_edits_long() {
        let cli = Cli::try_parse_from(["clust", "--accept-edits"]).unwrap();
        assert!(cli.accept_edits);
    }

    #[test]
    fn parse_accept_edits_with_prompt() {
        let cli = Cli::try_parse_from(["clust", "-e", "fix the bug"]).unwrap();
        assert!(cli.accept_edits);
        assert_eq!(cli.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn parse_accept_edits_with_background() {
        let cli = Cli::try_parse_from(["clust", "-e", "-b", "run tests"]).unwrap();
        assert!(cli.accept_edits);
        assert!(cli.background);
    }

    #[test]
    fn parse_use_short() {
        let cli = Cli::try_parse_from(["clust", "-u", "opencode"]).unwrap();
        assert_eq!(cli.use_agent.as_deref(), Some("opencode"));
    }

    #[test]
    fn parse_use_long() {
        let cli = Cli::try_parse_from(["clust", "--use", "opencode"]).unwrap();
        assert_eq!(cli.use_agent.as_deref(), Some("opencode"));
    }

    #[test]
    fn parse_use_with_prompt() {
        let cli = Cli::try_parse_from(["clust", "-u", "opencode", "fix the bug"]).unwrap();
        assert_eq!(cli.use_agent.as_deref(), Some("opencode"));
        assert_eq!(cli.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn parse_use_with_background() {
        let cli = Cli::try_parse_from(["clust", "-u", "opencode", "-b"]).unwrap();
        assert_eq!(cli.use_agent.as_deref(), Some("opencode"));
        assert!(cli.background);
    }

    #[test]
    fn parse_no_args_use_is_none() {
        let cli = Cli::try_parse_from(["clust"]).unwrap();
        assert!(cli.use_agent.is_none());
    }

    // ── Repo subcommand tests ───────────────────────────────────────

    #[test]
    fn parse_repo_register_short() {
        let cli = Cli::try_parse_from(["clust", "repo", "-R"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(register);
                assert!(!remove);
                assert!(!stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_repo_register_long() {
        let cli = Cli::try_parse_from(["clust", "repo", "--register"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(register);
                assert!(!remove);
                assert!(!stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_repo_remove_short() {
        let cli = Cli::try_parse_from(["clust", "repo", "-r"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(!register);
                assert!(remove);
                assert!(!stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_repo_remove_long() {
        let cli = Cli::try_parse_from(["clust", "repo", "--remove"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(!register);
                assert!(remove);
                assert!(!stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_repo_stop_short() {
        let cli = Cli::try_parse_from(["clust", "repo", "-s"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(!register);
                assert!(!remove);
                assert!(stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_repo_stop_long() {
        let cli = Cli::try_parse_from(["clust", "repo", "--stop"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(!register);
                assert!(!remove);
                assert!(stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_repo_no_flags() {
        let cli = Cli::try_parse_from(["clust", "repo"]).unwrap();
        match cli.command {
            Some(Commands::Repo { register, remove, stop }) => {
                assert!(!register);
                assert!(!remove);
                assert!(!stop);
            }
            _ => panic!("expected Repo command"),
        }
    }

    #[test]
    fn parse_invalid_flag_errors() {
        assert!(Cli::try_parse_from(["clust", "--nonsense"]).is_err());
    }

    // ── Hub flag tests ──────────────────────────────────────────────

    #[test]
    fn parse_hub_short() {
        let cli = Cli::try_parse_from(["clust", "-H", "my_feature"]).unwrap();
        assert_eq!(cli.hub.as_deref(), Some("my_feature"));
    }

    #[test]
    fn parse_hub_long() {
        let cli = Cli::try_parse_from(["clust", "--hub", "my_feature"]).unwrap();
        assert_eq!(cli.hub.as_deref(), Some("my_feature"));
    }

    #[test]
    fn parse_hub_with_prompt() {
        let cli = Cli::try_parse_from(["clust", "-H", "my_feature", "fix bug"]).unwrap();
        assert_eq!(cli.hub.as_deref(), Some("my_feature"));
        assert_eq!(cli.prompt.as_deref(), Some("fix bug"));
    }

    #[test]
    fn parse_no_args_hub_is_none() {
        let cli = Cli::try_parse_from(["clust"]).unwrap();
        assert!(cli.hub.is_none());
    }

    #[test]
    fn parse_ls_hub_short() {
        let cli = Cli::try_parse_from(["clust", "ls", "-H", "my_feature"]).unwrap();
        match cli.command {
            Some(Commands::Ls { select, hub }) => {
                assert!(!select);
                assert_eq!(hub.as_deref(), Some("my_feature"));
            }
            _ => panic!("expected Ls command"),
        }
    }

    #[test]
    fn parse_ls_hub_long() {
        let cli = Cli::try_parse_from(["clust", "ls", "--hub", "my_feature"]).unwrap();
        match cli.command {
            Some(Commands::Ls { select, hub }) => {
                assert!(!select);
                assert_eq!(hub.as_deref(), Some("my_feature"));
            }
            _ => panic!("expected Ls command"),
        }
    }

    #[test]
    fn parse_ls_hub_with_select() {
        let cli = Cli::try_parse_from(["clust", "ls", "-i", "-H", "my_feature"]).unwrap();
        match cli.command {
            Some(Commands::Ls { select, hub }) => {
                assert!(select);
                assert_eq!(hub.as_deref(), Some("my_feature"));
            }
            _ => panic!("expected Ls command"),
        }
    }

    // ── Hub name validation tests ───────────────────────────────────

    #[test]
    fn validate_hub_name_valid() {
        assert!(validate_hub_name("default_hub").is_ok());
        assert!(validate_hub_name("my_feature").is_ok());
        assert!(validate_hub_name("a").is_ok());
        assert!(validate_hub_name("hub123").is_ok());
        assert!(validate_hub_name("my_hub_2").is_ok());
    }

    #[test]
    fn default_hub_constant_passes_validation() {
        assert!(validate_hub_name(clust_ipc::DEFAULT_HUB).is_ok());
    }

    #[test]
    fn validate_hub_name_empty() {
        assert!(validate_hub_name("").is_err());
    }

    #[test]
    fn validate_hub_name_starts_with_uppercase() {
        assert!(validate_hub_name("MyHub").is_err());
    }

    #[test]
    fn validate_hub_name_starts_with_digit() {
        assert!(validate_hub_name("123hub").is_err());
    }

    #[test]
    fn validate_hub_name_contains_uppercase() {
        assert!(validate_hub_name("myHub").is_err());
    }

    #[test]
    fn validate_hub_name_contains_hyphen() {
        assert!(validate_hub_name("my-hub").is_err());
    }

    #[test]
    fn validate_hub_name_starts_with_underscore() {
        assert!(validate_hub_name("_hub").is_err());
    }

    #[test]
    fn validate_hub_name_trailing_underscore() {
        assert!(validate_hub_name("hub_").is_err());
        assert!(validate_hub_name("my_feature_").is_err());
    }

    #[test]
    fn validate_hub_name_consecutive_underscores() {
        assert!(validate_hub_name("my__hub").is_err());
        assert!(validate_hub_name("a__b__c").is_err());
    }
}
