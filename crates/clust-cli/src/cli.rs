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
    Ls,
    /// Open the Clust terminal UI
    Ui,
}
