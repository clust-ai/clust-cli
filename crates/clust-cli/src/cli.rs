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
    },
    /// Open the Clust terminal UI
    Ui,
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
        assert!(matches!(cli.command, Some(Commands::Ls { select: false })));
    }

    #[test]
    fn parse_ls_select_short() {
        let cli = Cli::try_parse_from(["clust", "ls", "-i"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Ls { select: true })));
    }

    #[test]
    fn parse_ls_select_long() {
        let cli = Cli::try_parse_from(["clust", "ls", "--select"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Ls { select: true })));
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
    fn parse_invalid_flag_errors() {
        assert!(Cli::try_parse_from(["clust", "--nonsense"]).is_err());
    }
}
