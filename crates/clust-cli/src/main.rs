mod cli;
mod ipc;
mod pool_launcher;
mod terminal;
mod theme;
mod ui;
mod version;

use clap::Parser;
use std::io::{self, Write};

use clust_ipc::{CliMessage, PoolMessage};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn print_logo() {
    use theme::*;
    // Box inner width = 46 visible chars between │ and │
    println!();
    println!("  {TEXT_TERTIARY}┌──────────────────────────────────────────────┐{RESET}");
    println!("  {TEXT_TERTIARY}│{RESET}                                              {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{ACCENT}   ██████╗██╗     ██╗   ██╗███████╗████████╗{RESET}  {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{ACCENT}  ██╔════╝██║     ██║   ██║██╔════╝╚══██╔══╝{RESET}  {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{ACCENT_BRIGHT}  ██║     ██║     ██║   ██║███████╗   ██║{RESET}     {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{ACCENT_BRIGHT}  ██║     ██║     ██║   ██║╚════██║   ██║{RESET}     {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{ACCENT}  ╚██████╗███████╗╚██████╔╝███████║   ██║{RESET}     {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{ACCENT}   ╚═════╝╚══════╝ ╚═════╝ ╚══════╝   ╚═╝{RESET}     {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{RESET}                                              {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}│{RESET}  {TEXT_TERTIARY}░░{TEXT_SECONDARY}▒▒{TEXT_PRIMARY}▓▓██████████████████████████████{TEXT_SECONDARY}▓▓{TEXT_TERTIARY}▒▒░░{RESET}  {TEXT_TERTIARY}│{RESET}");
    println!("  {TEXT_TERTIARY}└──────────────────────────────────────────────┘{RESET}");
    println!();
}

#[tokio::main]
async fn main() {
    let args = cli::Cli::parse();

    // Subcommand: ui
    if let Some(cli::Commands::Ui) = args.command {
        if let Err(e) = ui::run() {
            eprintln!("  {}ui error: {e}{}", theme::ERROR, theme::RESET);
            std::process::exit(1);
        }
        return;
    }

    // Subcommand: ls
    if let Some(cli::Commands::Ls) = args.command {
        handle_ls().await;
        return;
    }

    // Flag: --stop
    if args.stop {
        println!();
        let spinner = spin("stopping clust pool");
        match ipc::try_connect().await {
            Ok(mut stream) => match ipc::send_stop(&mut stream).await {
                Ok(()) => stop_spin(spinner, "clust pool stopped"),
                Err(e) => {
                    stop_spin_err(spinner, &format!("failed to stop clust pool: {e}"));
                    std::process::exit(1);
                }
            },
            Err(_) => {
                stop_spin(spinner, "clust pool is not running");
            }
        }
        return;
    }

    // Flag: --attach <ID>
    if let Some(ref id) = args.attach {
        handle_attach(id.clone()).await;
        return;
    }

    // Default: start an agent and attach (or -b for background)
    handle_start(args.prompt, args.background).await;
}

/// Start a new agent. If background is false, attach to it.
async fn handle_start(prompt: Option<String>, background: bool) {
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    if background {
        println!();
    }

    let spinner = if background {
        Some(spin("starting agent"))
    } else {
        None
    };

    let mut stream = match ipc::connect_to_pool().await {
        Ok(s) => s,
        Err(e) => {
            if let Some(s) = spinner {
                stop_spin_err(s, &format!("failed to connect to pool: {e}"));
            } else {
                eprintln!(
                    "\n  {}✘{} {}failed to connect to pool: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
            }
            std::process::exit(1);
        }
    };

    // Get terminal size for PTY initialization (minus 1 row for status bar)
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let agent_rows = if background {
        term_rows
    } else {
        term_rows.saturating_sub(1).max(1)
    };

    clust_ipc::send_message(
        &mut stream,
        &CliMessage::StartAgent {
            prompt,
            agent_binary: None,
            working_dir,
            cols: term_cols,
            rows: agent_rows,
        },
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!(
            "\n  {}✘{} {}failed to send start: {e}{}\n",
            theme::ERROR,
            theme::RESET,
            theme::TEXT_PRIMARY,
            theme::RESET,
        );
        std::process::exit(1);
    });

    let response: PoolMessage =
        clust_ipc::recv_message(&mut stream)
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "\n  {}✘{} {}failed to read response: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                std::process::exit(1);
            });

    match response {
        PoolMessage::AgentStarted { id, agent_binary } => {
            if background {
                if let Some(s) = spinner {
                    stop_spin(s, &format!("agent {id} started"));
                }
                return;
            }
            // Attach to the new agent
            let (reader, writer) = stream.into_split();
            let session = terminal::AttachedSession::new(id, agent_binary, reader, writer);
            if let Err(e) = session.run().await {
                eprintln!(
                    "\n  {}✘{} {}session error: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                std::process::exit(1);
            }
        }
        PoolMessage::Error { message } => {
            if let Some(s) = spinner {
                stop_spin_err(s, &message);
            } else {
                eprintln!(
                    "\n  {}✘{} {}{message}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
            }
            std::process::exit(1);
        }
        _ => {}
    }
}

/// Attach to an existing agent by ID.
async fn handle_attach(id: String) {
    let mut stream = match ipc::connect_to_pool().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "\n  {}✘{} {}failed to connect to pool: {e}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        }
    };

    clust_ipc::send_message(&mut stream, &CliMessage::AttachAgent { id })
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "\n  {}✘{} {}failed to send attach: {e}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        });

    let response: PoolMessage =
        clust_ipc::recv_message(&mut stream)
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "\n  {}✘{} {}failed to read response: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                std::process::exit(1);
            });

    match response {
        PoolMessage::AgentStarted { id, agent_binary } => {
            let (reader, writer) = stream.into_split();
            let session = terminal::AttachedSession::new(id, agent_binary, reader, writer);
            if let Err(e) = session.run().await {
                eprintln!(
                    "\n  {}✘{} {}session error: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                std::process::exit(1);
            }
        }
        PoolMessage::Error { message } => {
            eprintln!(
                "\n  {}✘{} {}{message}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        }
        _ => {}
    }
}

/// List all running agents.
async fn handle_ls() {
    let mut stream = match ipc::connect_to_pool().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "\n  {}✘{} {}failed to connect to pool: {e}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        }
    };

    clust_ipc::send_message(&mut stream, &CliMessage::ListAgents)
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "\n  {}✘{} {}failed to send list: {e}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        });

    let response: PoolMessage =
        clust_ipc::recv_message(&mut stream)
            .await
            .unwrap_or_else(|e| {
                eprintln!(
                    "\n  {}✘{} {}failed to read response: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                std::process::exit(1);
            });

    match response {
        PoolMessage::AgentList { agents } => {
            println!();
            if agents.is_empty() {
                println!(
                    "  {}no running agents{}",
                    theme::TEXT_SECONDARY, theme::RESET,
                );
            } else {
                // Header
                println!(
                    "  {}{:<8} {:<12} {:<10} {:<14} {}{}",
                    theme::TEXT_TERTIARY,
                    "ID",
                    "AGENT",
                    "STATUS",
                    "STARTED",
                    "ATTACHED",
                    theme::RESET,
                );
                for agent in &agents {
                    println!(
                        "  {}{:<8}{} {}{:<12}{} {}{:<10}{} {}{:<14}{} {}{}{}",
                        theme::ACCENT,
                        agent.id,
                        theme::RESET,
                        theme::TEXT_PRIMARY,
                        agent.agent_binary,
                        theme::RESET,
                        theme::SUCCESS,
                        "running",
                        theme::RESET,
                        theme::TEXT_SECONDARY,
                        agent.started_at,
                        theme::RESET,
                        theme::TEXT_SECONDARY,
                        agent.attached_clients,
                        theme::RESET,
                    );
                }
            }
            println!();
        }
        PoolMessage::Error { message } => {
            eprintln!(
                "\n  {}✘{} {}{message}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        }
        _ => {}
    }
}

fn spin(msg: &str) -> tokio::task::JoinHandle<()> {
    let msg = msg.to_string();
    tokio::spawn(async move {
        let mut i = 0;
        loop {
            print!(
                "\r  {}{}{} {}{}{}",
                theme::ACCENT,
                SPINNER[i % SPINNER.len()],
                theme::RESET,
                theme::TEXT_SECONDARY,
                msg,
                theme::RESET,
            );
            io::stdout().flush().ok();
            i += 1;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
    })
}

fn stop_spin(handle: tokio::task::JoinHandle<()>, msg: &str) {
    handle.abort();
    print!("\r\x1b[2K");
    println!(
        "  {}\u{2714}{} {}{}{}\n",
        theme::SUCCESS,
        theme::RESET,
        theme::TEXT_PRIMARY,
        msg,
        theme::RESET,
    );
}

fn stop_spin_err(handle: tokio::task::JoinHandle<()>, msg: &str) {
    handle.abort();
    print!("\r\x1b[2K");
    eprintln!(
        "  {}\u{2718}{} {}{}{}\n",
        theme::ERROR,
        theme::RESET,
        theme::TEXT_PRIMARY,
        msg,
        theme::RESET,
    );
}
