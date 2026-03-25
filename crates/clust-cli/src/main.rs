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
    if let Some(cli::Commands::Ls { select }) = args.command {
        handle_ls(select).await;
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

/// List all running agents, or open interactive selector with `--select`.
async fn handle_ls(select: bool) {
    if select {
        handle_select().await;
        return;
    }

    // Non-interactive: try_connect (no auto-spawn)
    let mut stream = match ipc::try_connect().await {
        Ok(s) => s,
        Err(_) => {
            println!(
                "\n  {}✘{} {}pool not running{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            return;
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

/// Result of the interactive selector.
enum SelectAction {
    Cancel,
    Attach(String),
    NewAgent,
}

/// Ensures raw mode and cursor visibility are restored on drop.
struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Self {
        crossterm::terminal::enable_raw_mode().unwrap();
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?25l").unwrap(); // hide cursor
        stdout.flush().unwrap();
        RawModeGuard
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b[?25h"); // show cursor
        let _ = stdout.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Interactive agent selector.
async fn handle_select() {
    // Fetch agent list (pool might not be running)
    let agents = match ipc::try_connect().await {
        Ok(mut stream) => {
            if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents)
                .await
                .is_err()
            {
                vec![]
            } else {
                match clust_ipc::recv_message::<PoolMessage>(&mut stream).await {
                    Ok(PoolMessage::AgentList { agents }) => agents,
                    _ => vec![],
                }
            }
        }
        Err(_) => vec![],
    };

    let action = run_selector(&agents);

    match action {
        SelectAction::Cancel => {}
        SelectAction::Attach(id) => handle_attach(id).await,
        SelectAction::NewAgent => handle_start(None, false).await,
    }
}

fn run_selector(agents: &[clust_ipc::AgentInfo]) -> SelectAction {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let item_count = 2 + agents.len(); // cancel + agents + new agent
    let mut selected: usize = 0;
    let mut stdout = io::stdout();

    // Spacing
    write!(stdout, "\n").unwrap();
    stdout.flush().unwrap();

    let _guard = RawModeGuard::new();

    // Initial render
    render_selector(&mut stdout, agents, selected, item_count);

    loop {
        if !event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => continue,
        };
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Up => {
                if selected > 0 {
                    selected -= 1;
                }
            }
            KeyCode::Down => {
                if selected < item_count - 1 {
                    selected += 1;
                }
            }
            KeyCode::Enter => break,
            KeyCode::Esc | KeyCode::Char('q') => {
                selected = 0; // cancel
                break;
            }
            _ => continue,
        }

        // Move cursor back up and re-render
        write!(stdout, "\x1b[{}A", item_count).unwrap();
        render_selector(&mut stdout, agents, selected, item_count);
    }

    // Erase the selector lines
    write!(stdout, "\x1b[{}A", item_count).unwrap();
    for _ in 0..item_count {
        write!(stdout, "\x1b[2K\n").unwrap();
    }
    write!(stdout, "\x1b[{}A", item_count).unwrap();
    stdout.flush().unwrap();

    // _guard drops here, restoring terminal

    if selected == 0 {
        SelectAction::Cancel
    } else if selected <= agents.len() {
        SelectAction::Attach(agents[selected - 1].id.clone())
    } else {
        SelectAction::NewAgent
    }
}

fn render_selector(
    stdout: &mut io::Stdout,
    agents: &[clust_ipc::AgentInfo],
    selected: usize,
    item_count: usize,
) {
    for i in 0..item_count {
        let is_selected = i == selected;
        let (prefix, label_color) = if is_selected {
            (
                format!("  {}▸{} ", theme::ACCENT, theme::RESET),
                theme::TEXT_PRIMARY,
            )
        } else {
            ("    ".to_string(), theme::TEXT_TERTIARY)
        };

        let label = if i == 0 {
            "cancel".to_string()
        } else if i <= agents.len() {
            let agent = &agents[i - 1];
            format!("{}  {}", agent.id, agent.agent_binary)
        } else {
            "new agent +".to_string()
        };

        write!(
            stdout,
            "\x1b[2K{}{}{}{}\r\n",
            prefix, label_color, label, theme::RESET,
        )
        .unwrap();
    }
    stdout.flush().unwrap();
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
