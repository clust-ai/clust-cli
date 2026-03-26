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

    /// Stop: without value stops the pool daemon; with a 6-char ID stops that agent
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

    /// Assign the agent to a named pool (snake_case; default: default_pool)
    #[arg(short = 'p', long = "pool")]
    pub pool: Option<String>,

    /// Initial prompt for the agent
    pub prompt: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List all running agents in the pool
    Ls {
        /// Interactive selector: navigate with arrow keys, Enter to confirm
        #[arg(short = 'i', long = "select")]
        select: bool,

        /// Filter agents by pool name
        #[arg(short = 'p', long = "pool")]
        pool: Option<String>,
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

/// Validate a pool name follows snake_case: starts with a lowercase ASCII letter,
/// followed by zero or more lowercase ASCII letters, digits, or underscores.
pub fn validate_pool_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("pool name cannot be empty".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() {
        return Err(format!(
            "pool name must start with a lowercase letter, got '{first}'"
        ));
    }
    if name.ends_with('_') {
        return Err("pool name must not end with an underscore".into());
    }
    if name.contains("__") {
        return Err("pool name must not contain consecutive underscores".into());
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' {
            return Err(format!(
                "pool name must be snake_case (lowercase, digits, underscores), found '{c}'"
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
    fn parse_stop_no_value_stops_pool() {
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
                pool: None
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
                pool: None
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
                pool: None
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

    // ── Pool flag tests ──────────────────────────────────────────────

    #[test]
    fn parse_pool_short() {
        let cli = Cli::try_parse_from(["clust", "-p", "my_feature"]).unwrap();
        assert_eq!(cli.pool.as_deref(), Some("my_feature"));
    }

    #[test]
    fn parse_pool_long() {
        let cli = Cli::try_parse_from(["clust", "--pool", "my_feature"]).unwrap();
        assert_eq!(cli.pool.as_deref(), Some("my_feature"));
    }

    #[test]
    fn parse_pool_with_prompt() {
        let cli = Cli::try_parse_from(["clust", "-p", "my_feature", "fix bug"]).unwrap();
        assert_eq!(cli.pool.as_deref(), Some("my_feature"));
        assert_eq!(cli.prompt.as_deref(), Some("fix bug"));
    }

    #[test]
    fn parse_no_args_pool_is_none() {
        let cli = Cli::try_parse_from(["clust"]).unwrap();
        assert!(cli.pool.is_none());
    }

    #[test]
    fn parse_ls_pool_short() {
        let cli = Cli::try_parse_from(["clust", "ls", "-p", "my_feature"]).unwrap();
        match cli.command {
            Some(Commands::Ls { select, pool }) => {
                assert!(!select);
                assert_eq!(pool.as_deref(), Some("my_feature"));
            }
            _ => panic!("expected Ls command"),
        }
    }

    #[test]
    fn parse_ls_pool_long() {
        let cli = Cli::try_parse_from(["clust", "ls", "--pool", "my_feature"]).unwrap();
        match cli.command {
            Some(Commands::Ls { select, pool }) => {
                assert!(!select);
                assert_eq!(pool.as_deref(), Some("my_feature"));
            }
            _ => panic!("expected Ls command"),
        }
    }

    #[test]
    fn parse_ls_pool_with_select() {
        let cli = Cli::try_parse_from(["clust", "ls", "-i", "-p", "my_feature"]).unwrap();
        match cli.command {
            Some(Commands::Ls { select, pool }) => {
                assert!(select);
                assert_eq!(pool.as_deref(), Some("my_feature"));
            }
            _ => panic!("expected Ls command"),
        }
    }

    // ── Pool name validation tests ───────────────────────────────────

    #[test]
    fn validate_pool_name_valid() {
        assert!(validate_pool_name("default_pool").is_ok());
        assert!(validate_pool_name("my_feature").is_ok());
        assert!(validate_pool_name("a").is_ok());
        assert!(validate_pool_name("pool123").is_ok());
        assert!(validate_pool_name("my_pool_2").is_ok());
    }

    #[test]
    fn default_pool_constant_passes_validation() {
        assert!(validate_pool_name(clust_ipc::DEFAULT_POOL).is_ok());
    }

    #[test]
    fn validate_pool_name_empty() {
        assert!(validate_pool_name("").is_err());
    }

    #[test]
    fn validate_pool_name_starts_with_uppercase() {
        assert!(validate_pool_name("MyPool").is_err());
    }

    #[test]
    fn validate_pool_name_starts_with_digit() {
        assert!(validate_pool_name("123pool").is_err());
    }

    #[test]
    fn validate_pool_name_contains_uppercase() {
        assert!(validate_pool_name("myPool").is_err());
    }

    #[test]
    fn validate_pool_name_contains_hyphen() {
        assert!(validate_pool_name("my-pool").is_err());
    }

    #[test]
    fn validate_pool_name_starts_with_underscore() {
        assert!(validate_pool_name("_pool").is_err());
    }

    #[test]
    fn validate_pool_name_trailing_underscore() {
        assert!(validate_pool_name("pool_").is_err());
        assert!(validate_pool_name("my_feature_").is_err());
    }

    #[test]
    fn validate_pool_name_consecutive_underscores() {
        assert!(validate_pool_name("my__pool").is_err());
        assert!(validate_pool_name("a__b__c").is_err());
    }
}
