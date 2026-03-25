mod cli;
mod ipc;
mod pool_launcher;
mod terminal;
mod theme;
mod ui;
mod version;

use chrono::{DateTime, Local, Utc};
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

    // Subcommand: ui (also triggered by `clust .`)
    if matches!(args.command, Some(cli::Commands::Ui)) || args.prompt.as_deref() == Some(".") {
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

    // Flag: --stop <ID>
    if let Some(ref id) = args.stop {
        println!();
        let spinner = spin(&format!("stopping agent {id}"));
        match ipc::try_connect().await {
            Ok(mut stream) => match ipc::send_stop_agent(&mut stream, id).await {
                Ok(()) => stop_spin(spinner, &format!("agent {id} stopped")),
                Err(e) => {
                    stop_spin_err(spinner, &format!("failed to stop agent {id}: {e}"));
                    std::process::exit(1);
                }
            },
            Err(_) => {
                stop_spin(spinner, "clust pool is not running");
            }
        }
        return;
    }

    // Flag: --stop-pool
    if args.stop_pool {
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

    // Flag: --default
    if args.default {
        handle_default_picker().await;
        return;
    }

    // Flag: --attach <ID>
    if let Some(ref id) = args.attach {
        handle_attach(id.clone()).await;
        return;
    }

    // Default: start an agent and attach (or -b for background)
    handle_start(args.prompt, args.background, args.accept_edits).await;
}

/// Check if a default agent is configured. If not, show the first-run picker.
///
/// Returns `Some(Some(binary))` if a default exists or the user selected one,
/// `Some(None)` if the pool is unreachable (let handle_start report the error),
/// or `None` if the user cancelled.
async fn check_default_and_prompt() -> Option<Option<String>> {
    let mut stream = match ipc::connect_to_pool().await {
        Ok(s) => s,
        Err(_) => return Some(None), // Can't reach pool, let handle_start report the error
    };

    if clust_ipc::send_message(&mut stream, &CliMessage::GetDefault)
        .await
        .is_err()
    {
        return Some(None);
    }

    let current = match clust_ipc::recv_message::<PoolMessage>(&mut stream).await {
        Ok(PoolMessage::DefaultAgent { agent_binary }) => agent_binary,
        _ => return Some(None), // Unexpected response, proceed with pool's default
    };

    if current.is_some() {
        return Some(current); // Pass the existing default through
    }

    // No default set — first-run prompt
    print_logo();
    let result = run_default_selector(None, "pick a default agent to get started");

    match result {
        DefaultPickerResult::Selected(binary) => {
            // Persist the choice (new connection)
            let mut set_ok = false;
            if let Ok(mut s) = ipc::connect_to_pool().await {
                if clust_ipc::send_message(
                    &mut s,
                    &CliMessage::SetDefault {
                        agent_binary: binary.clone(),
                    },
                )
                .await
                .is_ok()
                {
                    match clust_ipc::recv_message::<PoolMessage>(&mut s).await {
                        Ok(PoolMessage::Ok) => set_ok = true,
                        Ok(PoolMessage::Error { message }) => {
                            eprintln!(
                                "  {}✘{} {}failed to set default: {message}{}",
                                theme::ERROR, theme::RESET, theme::TEXT_PRIMARY, theme::RESET,
                            );
                        }
                        _ => {}
                    }
                }
            }
            if !set_ok {
                return None; // Treat as cancelled if we couldn't persist
            }
            println!(
                "  {}✔{} {}default agent set to {binary}{}\n",
                theme::SUCCESS,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            Some(Some(binary))
        }
        DefaultPickerResult::Cancel => None,
    }
}

/// Start a new agent. If background is false, attach to it.
async fn handle_start(prompt: Option<String>, background: bool, accept_edits: bool) {
    // Check if a default agent is configured; prompt on first run
    let agent_override = check_default_and_prompt().await;
    if agent_override.is_none() {
        return; // User cancelled first-run picker
    }
    let agent_binary = agent_override.unwrap();

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
            agent_binary,
            working_dir,
            cols: term_cols,
            rows: agent_rows,
            accept_edits,
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
        PoolMessage::AgentAttached { id, agent_binary } => {
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
                    let started = format_started(&agent.started_at);
                    let attached = format_attached(agent.attached_clients);
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
                        started,
                        theme::RESET,
                        theme::TEXT_SECONDARY,
                        attached,
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

/// Format an attachment count into a human-readable string.
fn format_attached(count: usize) -> String {
    if count == 1 {
        "1 terminal".to_string()
    } else {
        format!("{count} terminals")
    }
}

/// Format an RFC 3339 timestamp into a human-readable relative time string.
fn format_started(rfc3339: &str) -> String {
    let Ok(dt) = rfc3339.parse::<DateTime<Utc>>() else {
        return rfc3339.to_string();
    };
    let local = dt.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%b %d %H:%M").to_string()
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
        SelectAction::NewAgent => handle_start(None, false, false).await,
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
        let prefix = if is_selected {
            format!("  {}▸{} ", theme::ACCENT, theme::RESET)
        } else {
            "    ".to_string()
        };

        let line = if i == 0 {
            let color = if is_selected {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_TERTIARY
            };
            format!("{color}cancel{}", theme::RESET)
        } else if i <= agents.len() {
            let agent = &agents[i - 1];
            let started = format_started(&agent.started_at);
            let attached = format_attached(agent.attached_clients);
            let (text_color, status_color) = if is_selected {
                (theme::TEXT_PRIMARY, theme::SUCCESS)
            } else {
                (theme::TEXT_TERTIARY, theme::TEXT_TERTIARY)
            };
            format!(
                "{}{:<8}{} {}{:<12}{} {}{:<10}{} {}{:<14}{} {}{}{}",
                theme::ACCENT,
                agent.id,
                theme::RESET,
                text_color,
                agent.agent_binary,
                theme::RESET,
                status_color,
                "running",
                theme::RESET,
                text_color,
                started,
                theme::RESET,
                text_color,
                attached,
                theme::RESET,
            )
        } else {
            let color = if is_selected {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_TERTIARY
            };
            format!("{color}new agent +{}", theme::RESET)
        };

        write!(stdout, "\x1b[2K{}{}\r\n", prefix, line).unwrap();
    }
    stdout.flush().unwrap();
}

// ── Default agent picker ─────────────────────────────────────────────

enum DefaultPickerResult {
    Cancel,
    Selected(String),
}

/// Handle `clust -d`: show interactive picker to set the default agent.
async fn handle_default_picker() {
    println!();
    let spinner = spin("connecting to pool");

    let mut stream = match ipc::connect_to_pool().await {
        Ok(s) => {
            stop_spin(spinner, "connected");
            s
        }
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to pool: {e}"));
            std::process::exit(1);
        }
    };

    // Get current default
    clust_ipc::send_message(&mut stream, &CliMessage::GetDefault)
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "  {}✘{} {}failed to get default: {e}{}",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        });

    let current = match clust_ipc::recv_message::<PoolMessage>(&mut stream).await {
        Ok(PoolMessage::DefaultAgent { agent_binary }) => agent_binary,
        _ => None,
    };

    let result = run_default_selector(current.as_deref(), "set default agent");

    match result {
        DefaultPickerResult::Cancel => {}
        DefaultPickerResult::Selected(binary) => {
            // Persist the choice (new connection since the previous one is consumed)
            let mut set_ok = false;
            match ipc::connect_to_pool().await {
                Ok(mut s) => {
                    if clust_ipc::send_message(
                        &mut s,
                        &CliMessage::SetDefault {
                            agent_binary: binary.clone(),
                        },
                    )
                    .await
                    .is_ok()
                    {
                        match clust_ipc::recv_message::<PoolMessage>(&mut s).await {
                            Ok(PoolMessage::Ok) => set_ok = true,
                            Ok(PoolMessage::Error { message }) => {
                                eprintln!(
                                    "  {}✘{} {}failed to set default: {message}{}\n",
                                    theme::ERROR, theme::RESET, theme::TEXT_PRIMARY, theme::RESET,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                Err(_) => {}
            }
            if set_ok {
                println!(
                    "  {}✔{} {}default agent set to {binary}{}\n",
                    theme::SUCCESS,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
            } else {
                eprintln!(
                    "  {}✘{} {}failed to set default agent{}\n",
                    theme::ERROR, theme::RESET, theme::TEXT_PRIMARY, theme::RESET,
                );
            }
        }
    }
}

/// Interactive default agent selector.
///
/// Shows known agents with a checkmark on the current default, plus a "Custom..."
/// option for entering an arbitrary command.
fn run_default_selector(current: Option<&str>, header: &str) -> DefaultPickerResult {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let known = clust_ipc::agents::KNOWN_AGENTS;
    // Items: cancel + known agents + custom
    let item_count = 2 + known.len();
    let mut selected: usize = 1; // Start on first agent, not cancel
    let mut stdout = io::stdout();

    // Header
    write!(
        stdout,
        "  {}{}{}\n\n",
        theme::TEXT_SECONDARY, header, theme::RESET,
    )
    .unwrap();
    stdout.flush().unwrap();

    let _guard = RawModeGuard::new();

    render_default_selector(&mut stdout, known, current, selected, item_count);

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

        write!(stdout, "\x1b[{}A", item_count).unwrap();
        render_default_selector(&mut stdout, known, current, selected, item_count);
    }

    // Erase the selector lines + header (item_count + 2 for header + blank line)
    let total_lines = item_count + 2;
    write!(stdout, "\x1b[{}A", item_count).unwrap();
    for _ in 0..total_lines {
        write!(stdout, "\x1b[2K\x1b[1A").unwrap();
    }
    write!(stdout, "\x1b[2K").unwrap();
    stdout.flush().unwrap();

    // _guard drops here, restoring terminal

    if selected == 0 {
        DefaultPickerResult::Cancel
    } else if selected <= known.len() {
        DefaultPickerResult::Selected(known[selected - 1].binary.to_string())
    } else {
        // Custom: prompt for input
        read_custom_agent()
    }
}

fn render_default_selector(
    stdout: &mut io::Stdout,
    known: &[clust_ipc::agents::KnownAgent],
    current: Option<&str>,
    selected: usize,
    item_count: usize,
) {
    for i in 0..item_count {
        let is_selected = i == selected;
        let prefix = if is_selected {
            format!("  {}▸{} ", theme::ACCENT, theme::RESET)
        } else {
            "    ".to_string()
        };

        let line = if i == 0 {
            // Cancel
            let color = if is_selected {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_TERTIARY
            };
            format!("{color}cancel{}", theme::RESET)
        } else if i <= known.len() {
            // Known agent
            let agent = &known[i - 1];
            let is_current = current == Some(agent.binary);
            let (name_color, bin_color) = if is_selected {
                (theme::TEXT_PRIMARY, theme::TEXT_SECONDARY)
            } else {
                (theme::TEXT_TERTIARY, theme::TEXT_TERTIARY)
            };
            let check = if is_current {
                format!(" {}✔{}", theme::SUCCESS, theme::RESET)
            } else {
                String::new()
            };
            format!(
                "{}{}{} {}({}){}{check}",
                name_color, agent.display_name, theme::RESET, bin_color, agent.binary, theme::RESET,
            )
        } else {
            // Custom
            let color = if is_selected {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_TERTIARY
            };
            // Show checkmark if current default is not a known agent
            let is_custom_current =
                current.is_some() && !known.iter().any(|a| Some(a.binary) == current);
            let check = if is_custom_current {
                format!(
                    " {}✔{} {}({}){}",
                    theme::SUCCESS, theme::RESET,
                    theme::TEXT_SECONDARY,
                    current.unwrap(),
                    theme::RESET,
                )
            } else {
                String::new()
            };
            format!("{color}Custom...{}{check}", theme::RESET)
        };

        write!(stdout, "\x1b[2K{}{}\r\n", prefix, line).unwrap();
    }
    stdout.flush().unwrap();
}

/// Read a custom agent command from the user in raw mode.
fn read_custom_agent() -> DefaultPickerResult {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let mut stdout = io::stdout();
    let mut buf = String::new();

    let _guard = RawModeGuard::new();

    // Show cursor for text input
    write!(stdout, "\x1b[?25h").unwrap();
    write!(
        stdout,
        "\r\x1b[2K  {}agent command:{} ",
        theme::TEXT_SECONDARY, theme::RESET,
    )
    .unwrap();
    stdout.flush().unwrap();

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
            KeyCode::Enter => {
                if !buf.is_empty() {
                    // Erase the input line
                    write!(stdout, "\r\x1b[2K").unwrap();
                    stdout.flush().unwrap();
                    return DefaultPickerResult::Selected(buf);
                }
            }
            KeyCode::Esc => {
                write!(stdout, "\r\x1b[2K").unwrap();
                stdout.flush().unwrap();
                return DefaultPickerResult::Cancel;
            }
            KeyCode::Backspace => {
                if buf.pop().is_some() {
                    write!(stdout, "\x08 \x08").unwrap();
                    stdout.flush().unwrap();
                }
            }
            KeyCode::Char(c) => {
                buf.push(c);
                write!(stdout, "{c}").unwrap();
                stdout.flush().unwrap();
            }
            _ => {}
        }
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
