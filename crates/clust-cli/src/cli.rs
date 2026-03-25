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

    /// Stop the pool daemon and all running agents
    #[arg(short = 's', long = "stop")]
    pub stop: bool,

    /// Set the global default agent binary (e.g., claude, aider)
    #[arg(short = 'd', long = "default")]
    pub default: Option<String>,

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
        assert!(!cli.stop);
        assert!(cli.attach.is_none());
        assert!(cli.default.is_none());
        assert!(cli.prompt.is_none());
        assert!(cli.command.is_none());
    }

    #[test]
    fn parse_stop_short() {
        let cli = Cli::try_parse_from(["clust", "-s"]).unwrap();
        assert!(cli.stop);
    }

    #[test]
    fn parse_stop_long() {
        let cli = Cli::try_parse_from(["clust", "--stop"]).unwrap();
        assert!(cli.stop);
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
        let cli = Cli::try_parse_from(["clust", "-d", "claude"]).unwrap();
        assert_eq!(cli.default.as_deref(), Some("claude"));
    }

    #[test]
    fn parse_default_long() {
        let cli = Cli::try_parse_from(["clust", "--default", "aider"]).unwrap();
        assert_eq!(cli.default.as_deref(), Some("aider"));
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
    fn parse_invalid_flag_errors() {
        assert!(Cli::try_parse_from(["clust", "--nonsense"]).is_err());
    }
}
