mod cli;
mod context_menu;
mod create_agent_modal;
mod format;
mod ipc;
mod output_filter;
mod overview;
mod hub_launcher;
mod scroll_break;
mod terminal;
pub mod terminal_emulator;
mod theme;
mod ui;
mod version;

use clap::Parser;
use std::io::{self, Write};

use clust_ipc::{CliMessage, HubMessage};
use format::{format_attached, format_started};

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

    // Validate hub name if provided on top-level flag
    if let Some(ref p) = args.hub {
        if let Err(e) = cli::validate_hub_name(p) {
            eprintln!(
                "\n  {}✘{} {}invalid hub name: {e}{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            std::process::exit(1);
        }
    }
    let hub_name = args
        .hub
        .clone()
        .unwrap_or_else(|| clust_ipc::DEFAULT_HUB.into());

    // Subcommand: ui (also triggered by `clust .`)
    if matches!(args.command, Some(cli::Commands::Ui)) || args.prompt.as_deref() == Some(".") {
        if let Err(e) = ui::run(&hub_name) {
            eprintln!("  {}ui error: {e}{}", theme::ERROR, theme::RESET);
            std::process::exit(1);
        }
        return;
    }

    // Subcommand: ls
    if let Some(cli::Commands::Ls { select, hub }) = args.command {
        // Validate hub filter if provided
        if let Some(ref p) = hub {
            if let Err(e) = cli::validate_hub_name(p) {
                eprintln!(
                    "\n  {}✘{} {}invalid hub name: {e}{}\n",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                std::process::exit(1);
            }
        }
        handle_ls(select, hub).await;
        return;
    }

    // Subcommand: wt / worktree
    if let Some(cli::Commands::Wt(cli::WtArgs { repo, command })) = args.command {
        match command {
            cli::WtCommands::Ls => handle_wt_ls(repo).await,
            cli::WtCommands::Add(add_args) => {
                handle_wt_add(
                    repo,
                    add_args.name,
                    add_args.base_branch,
                    add_args.checkout,
                    add_args.prompt,
                )
                .await;
            }
            cli::WtCommands::Rm(rm_args) => {
                handle_wt_rm(repo, rm_args.delete_local, rm_args.branch, rm_args.force).await;
            }
            cli::WtCommands::Info(info_args) => {
                handle_wt_info(repo, info_args.name).await;
            }
        }
        return;
    }

    // Subcommand: repo
    if let Some(cli::Commands::Repo {
        add,
        remove,
        stop,
    }) = args.command
    {
        if add {
            handle_add().await;
        } else if remove {
            handle_repo_remove().await;
        } else if stop {
            handle_repo_stop().await;
        }
        return;
    }

    // Flag: -s / --stop (no value = stop hub, with value = stop agent)
    if let Some(ref id_or_empty) = args.stop {
        println!();
        if id_or_empty.is_empty() {
            // No ID → stop the hub
            let spinner = spin("stopping clust hub");
            // Count unique hubs for pluralization
            let hub_count = ipc::count_hubs().await;
            match ipc::try_connect().await {
                Ok(mut stream) => match ipc::send_stop(&mut stream).await {
                    Ok(()) => {
                        let label = if hub_count > 1 {
                            "clust hubs stopped"
                        } else {
                            "clust hub stopped"
                        };
                        stop_spin(spinner, label);
                    }
                    Err(e) => {
                        stop_spin_err(spinner, &format!("failed to stop clust hub: {e}"));
                        std::process::exit(1);
                    }
                },
                Err(_) => {
                    stop_spin(spinner, "clust hub is not running");
                }
            }
        } else {
            // ID provided → stop specific agent
            let spinner = spin(&format!("stopping agent {id_or_empty}"));
            match ipc::try_connect().await {
                Ok(mut stream) => match ipc::send_stop_agent(&mut stream, id_or_empty).await {
                    Ok(()) => stop_spin(spinner, &format!("agent {id_or_empty} stopped")),
                    Err(e) => {
                        stop_spin_err(spinner, &format!("failed to stop agent {id_or_empty}: {e}"));
                        std::process::exit(1);
                    }
                },
                Err(_) => {
                    stop_spin(spinner, "clust hub is not running");
                }
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
    handle_start(
        args.prompt,
        args.background,
        args.accept_edits,
        args.use_agent,
        hub_name,
        None,
    )
    .await;
}

/// Check if a default agent is configured. If not, show the first-run picker.
///
/// Returns `Some(Some(binary))` if a default exists or the user selected one,
/// `Some(None)` if the hub is unreachable (let handle_start report the error),
/// or `None` if the user cancelled.
async fn check_default_and_prompt() -> Option<Option<String>> {
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(_) => return Some(None), // Can't reach hub, let handle_start report the error
    };

    if clust_ipc::send_message(&mut stream, &CliMessage::GetDefault)
        .await
        .is_err()
    {
        return Some(None);
    }

    let current = match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::DefaultAgent { agent_binary }) => agent_binary,
        _ => return Some(None), // Unexpected response, proceed with hub's default
    };

    if current.is_some() {
        return Some(current); // Pass the existing default through
    }

    // No default set — first-run prompt
    let installed = installed_agents();

    // If exactly one agent is installed, auto-select it
    if installed.len() == 1 {
        let binary = installed[0].binary.to_string();
        let mut set_ok = false;
        if let Ok(mut s) = ipc::connect_to_hub().await {
            if clust_ipc::send_message(
                &mut s,
                &CliMessage::SetDefault {
                    agent_binary: binary.clone(),
                },
            )
            .await
            .is_ok()
            {
                match clust_ipc::recv_message::<HubMessage>(&mut s).await {
                    Ok(HubMessage::Ok) => set_ok = true,
                    Ok(HubMessage::Error { message }) => {
                        eprintln!(
                            "  {}✘{} {}failed to set default: {message}{}",
                            theme::ERROR,
                            theme::RESET,
                            theme::TEXT_PRIMARY,
                            theme::RESET,
                        );
                    }
                    _ => {}
                }
            }
        }
        if !set_ok {
            return None;
        }
        println!(
            "  {}✔{} {}default agent set to {binary}{}\n",
            theme::SUCCESS,
            theme::RESET,
            theme::TEXT_PRIMARY,
            theme::RESET,
        );
        return Some(Some(binary));
    }

    print_logo();
    let result = run_default_selector(&installed, None, "pick a default agent to get started");

    match result {
        DefaultPickerResult::Selected(binary) => {
            // Persist the choice (new connection)
            let mut set_ok = false;
            if let Ok(mut s) = ipc::connect_to_hub().await {
                if clust_ipc::send_message(
                    &mut s,
                    &CliMessage::SetDefault {
                        agent_binary: binary.clone(),
                    },
                )
                .await
                .is_ok()
                {
                    match clust_ipc::recv_message::<HubMessage>(&mut s).await {
                        Ok(HubMessage::Ok) => set_ok = true,
                        Ok(HubMessage::Error { message }) => {
                            eprintln!(
                                "  {}✘{} {}failed to set default: {message}{}",
                                theme::ERROR,
                                theme::RESET,
                                theme::TEXT_PRIMARY,
                                theme::RESET,
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
/// If `working_dir_override` is provided, use it instead of cwd.
async fn handle_start(
    prompt: Option<String>,
    background: bool,
    accept_edits: bool,
    use_agent: Option<String>,
    hub: String,
    working_dir_override: Option<String>,
) {
    // If --use was provided, use that agent directly; otherwise check/prompt for default
    let agent_binary = if let Some(agent) = use_agent {
        Some(agent)
    } else {
        let agent_override = check_default_and_prompt().await;
        if agent_override.is_none() {
            return; // User cancelled first-run picker
        }
        agent_override.unwrap()
    };

    let working_dir = working_dir_override.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into())
    });

    if background {
        println!();
    }

    let spinner = if background {
        Some(spin("starting agent"))
    } else {
        None
    };

    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            if let Some(s) = spinner {
                stop_spin_err(s, &format!("failed to connect to hub: {e}"));
            } else {
                eprintln!(
                    "\n  {}✘{} {}failed to connect to hub: {e}{}\n",
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
            hub,
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

    let response: HubMessage = clust_ipc::recv_message(&mut stream)
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
        HubMessage::AgentStarted { id, agent_binary } => {
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
            // Exit immediately to avoid tokio runtime shutdown blocking on
            // the orphaned stdin blocking-read thread.
            std::process::exit(0);
        }
        HubMessage::Error { message } => {
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
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "\n  {}✘{} {}failed to connect to hub: {e}{}\n",
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

    let response: HubMessage = clust_ipc::recv_message(&mut stream)
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
        HubMessage::AgentAttached { id, agent_binary } => {
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
            // Exit immediately to avoid tokio runtime shutdown blocking on
            // the orphaned stdin blocking-read thread.
            std::process::exit(0);
        }
        HubMessage::Error { message } => {
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
async fn handle_ls(select: bool, hub: Option<String>) {
    if select {
        handle_select(hub).await;
        return;
    }

    // Non-interactive: try_connect (no auto-spawn)
    let mut stream = match ipc::try_connect().await {
        Ok(s) => s,
        Err(_) => {
            println!(
                "\n  {}✘{} {}hub not running{}\n",
                theme::ERROR,
                theme::RESET,
                theme::TEXT_PRIMARY,
                theme::RESET,
            );
            return;
        }
    };

    clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { hub: hub.clone() })
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

    let response: HubMessage = clust_ipc::recv_message(&mut stream)
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
        HubMessage::AgentList { mut agents } => {
            println!();
            if agents.is_empty() {
                println!(
                    "  {}no running agents{}",
                    theme::TEXT_SECONDARY,
                    theme::RESET,
                );
            } else if hub.is_some() {
                // Filtered to a single hub — flat display, no header
                print_agent_table(&agents);
            } else {
                // All hubs — group by hub name
                agents.sort_by(|a, b| a.hub.cmp(&b.hub).then(a.started_at.cmp(&b.started_at)));
                let mut current_hub: Option<&str> = None;
                for agent in &agents {
                    if current_hub != Some(&agent.hub) {
                        if current_hub.is_some() {
                            println!(); // blank line between hubs
                        }
                        println!("  {}{}{}", theme::ACCENT, agent.hub, theme::RESET,);
                        print_agent_header();
                        current_hub = Some(&agent.hub);
                    }
                    print_agent_row(agent);
                }
            }
            println!();
        }
        HubMessage::Error { message } => {
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

fn print_agent_header() {
    println!(
        "  {}{:<8} {:<12} {:<10} {:<14} ATTACHED{}",
        theme::TEXT_TERTIARY,
        "ID",
        "AGENT",
        "STATUS",
        "STARTED",
        theme::RESET,
    );
}

fn print_agent_row(agent: &clust_ipc::AgentInfo) {
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

fn print_agent_table(agents: &[clust_ipc::AgentInfo]) {
    print_agent_header();
    for agent in agents {
        print_agent_row(agent);
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
async fn handle_select(hub: Option<String>) {
    // Fetch agent list (hub might not be running)
    let agents = match ipc::try_connect().await {
        Ok(mut stream) => {
            if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { hub: hub.clone() })
                .await
                .is_err()
            {
                vec![]
            } else {
                match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
                    Ok(HubMessage::AgentList { agents }) => agents,
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
        SelectAction::NewAgent => {
            let hub_name = hub.unwrap_or_else(|| clust_ipc::DEFAULT_HUB.into());
            handle_start(None, false, false, None, hub_name, None).await;
        }
    }
}

fn run_selector(agents: &[clust_ipc::AgentInfo]) -> SelectAction {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let item_count = 2 + agents.len(); // cancel + agents + new agent
    let mut selected: usize = 0;
    let mut stdout = io::stdout();

    // Spacing
    writeln!(stdout).unwrap();
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
                selected = selected.saturating_sub(1);
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
        writeln!(stdout, "\x1b[2K").unwrap();
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

/// Return only those known agents whose binary is found in PATH.
fn installed_agents() -> Vec<&'static clust_ipc::agents::KnownAgent> {
    clust_ipc::agents::KNOWN_AGENTS
        .iter()
        .filter(|a| which::which(a.binary).is_ok())
        .collect()
}

/// Handle `clust -d`: show interactive picker to set the default agent.
async fn handle_default_picker() {
    println!();
    let spinner = spin("connecting to hub");

    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => {
            stop_spin(spinner, "connected");
            s
        }
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
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

    let current = match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::DefaultAgent { agent_binary }) => agent_binary,
        _ => None,
    };

    let installed = installed_agents();
    let result = run_default_selector(&installed, current.as_deref(), "set default agent");

    match result {
        DefaultPickerResult::Cancel => {}
        DefaultPickerResult::Selected(binary) => {
            // Persist the choice (new connection since the previous one is consumed)
            let mut set_ok = false;
            if let Ok(mut s) = ipc::connect_to_hub().await {
                if clust_ipc::send_message(
                    &mut s,
                    &CliMessage::SetDefault {
                        agent_binary: binary.clone(),
                    },
                )
                .await
                .is_ok()
                {
                    match clust_ipc::recv_message::<HubMessage>(&mut s).await {
                        Ok(HubMessage::Ok) => set_ok = true,
                        Ok(HubMessage::Error { message }) => {
                            eprintln!(
                                "  {}✘{} {}failed to set default: {message}{}\n",
                                theme::ERROR,
                                theme::RESET,
                                theme::TEXT_PRIMARY,
                                theme::RESET,
                            );
                        }
                        _ => {}
                    }
                }
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
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
            }
        }
    }
}

async fn handle_add() {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let spinner = spin("adding repository");
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    if let Err(e) =
        clust_ipc::send_message(&mut stream, &CliMessage::RegisterRepo { path: working_dir }).await
    {
        stop_spin_err(spinner, &format!("failed to send register: {e}"));
        std::process::exit(1);
    }

    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::RepoRegistered { name, .. }) => {
            stop_spin(spinner, &format!("repository '{name}' registered"));
        }
        Ok(HubMessage::Error { message }) => {
            stop_spin_err(spinner, &message);
            std::process::exit(1);
        }
        _ => {
            stop_spin_err(spinner, "unexpected response from hub");
            std::process::exit(1);
        }
    }
    println!();
}

async fn handle_repo_remove() {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    // Yellow warning
    println!(
        "  {}⚠ this will stop all agents working in this repo and remove it from clust{}",
        theme::WARNING,
        theme::RESET
    );
    println!();

    // Confirmation prompt
    eprint!(
        "  {}are you sure? [y/N]{} ",
        theme::TEXT_PRIMARY,
        theme::RESET
    );
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() || !matches!(input.trim(), "y" | "Y") {
        println!("\n  {}cancelled{}", theme::TEXT_SECONDARY, theme::RESET);
        println!();
        return;
    }
    println!();

    let spinner = spin("removing repository");
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    match ipc::send_unregister_repo(&mut stream, &working_dir).await {
        Ok((name, stopped)) => {
            if stopped > 0 {
                stop_spin(
                    spinner,
                    &format!(
                        "repository '{name}' removed ({stopped} agent{} stopped)",
                        if stopped == 1 { "" } else { "s" }
                    ),
                );
            } else {
                stop_spin(spinner, &format!("repository '{name}' removed"));
            }
        }
        Err(e) => {
            stop_spin_err(spinner, &format!("{e}"));
            std::process::exit(1);
        }
    }
    println!();
}

async fn handle_repo_stop() {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let spinner = spin("stopping repo agents");
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    match ipc::send_stop_repo_agents(&mut stream, &working_dir).await {
        Ok(count) => {
            if count > 0 {
                stop_spin(
                    spinner,
                    &format!("{count} agent{} stopped", if count == 1 { "" } else { "s" }),
                );
            } else {
                stop_spin(spinner, "no agents running in this repo");
            }
        }
        Err(e) => {
            stop_spin_err(spinner, &format!("{e}"));
            std::process::exit(1);
        }
    }
    println!();
}

// ── Worktree handlers ──────────────────────────────────────────────

/// Detect the current worktree's branch name from the working directory path.
/// Looks for the `.clust/worktrees/{serialized_branch}` convention.
fn detect_current_worktree_branch(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    for ancestor in path.ancestors() {
        if let Some(parent) = ancestor.parent() {
            if parent.ends_with(".clust/worktrees") {
                let dir_name = ancestor.file_name()?.to_str()?;
                return Some(dir_name.replace("__", "/"));
            }
        }
    }
    None
}

async fn handle_wt_ls(repo_name: Option<String>) {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let spinner = spin("listing worktrees");
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    if let Err(e) = clust_ipc::send_message(
        &mut stream,
        &CliMessage::ListWorktrees {
            working_dir: if repo_name.is_some() {
                None
            } else {
                Some(working_dir.clone())
            },
            repo_name: repo_name.clone(),
        },
    )
    .await
    {
        stop_spin_err(spinner, &format!("failed to send request: {e}"));
        std::process::exit(1);
    }

    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::WorktreeList {
            repo_name: name,
            worktrees,
            ..
        }) => {
            stop_spin(
                spinner,
                &format!(
                    "{} worktree{}",
                    worktrees.len(),
                    if worktrees.len() == 1 { "" } else { "s" }
                ),
            );
            println!();
            if worktrees.is_empty() {
                println!(
                    "  {}no worktrees{}",
                    theme::TEXT_SECONDARY,
                    theme::RESET
                );
            } else {
                println!(
                    "  {}{}{}",
                    theme::ACCENT,
                    name,
                    theme::RESET
                );
                println!();
                println!(
                    "  {}  {:<24} {:<8} STATUS{}",
                    theme::TEXT_TERTIARY,
                    "BRANCH",
                    "AGENTS",
                    theme::RESET
                );
                for wt in &worktrees {
                    let indicator = if wt.is_main { "●" } else { " " };
                    let branch_color = if wt.is_main {
                        theme::ACCENT_BRIGHT
                    } else {
                        theme::TEXT_PRIMARY
                    };
                    let status = if wt.is_dirty { "dirty" } else { "clean" };
                    let status_color = if wt.is_dirty {
                        theme::WARNING
                    } else {
                        theme::TEXT_SECONDARY
                    };
                    let agent_count = wt.active_agents.len();

                    println!(
                        "  {}{}{} {}{:<24}{} {}{:<8}{} {}{}{}",
                        theme::ACCENT,
                        indicator,
                        theme::RESET,
                        branch_color,
                        wt.branch_name,
                        theme::RESET,
                        theme::TEXT_SECONDARY,
                        agent_count,
                        theme::RESET,
                        status_color,
                        status,
                        theme::RESET,
                    );
                }
            }
        }
        Ok(HubMessage::Error { message }) => {
            stop_spin_err(spinner, &message);
            std::process::exit(1);
        }
        _ => {
            stop_spin_err(spinner, "unexpected response from hub");
            std::process::exit(1);
        }
    }
    println!();
}

async fn handle_wt_add(
    repo_name: Option<String>,
    name: String,
    base_branch: Option<String>,
    checkout: bool,
    prompt: Option<String>,
) {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let spinner = spin(&format!("creating worktree '{name}'"));
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    if let Err(e) = clust_ipc::send_message(
        &mut stream,
        &CliMessage::AddWorktree {
            working_dir: if repo_name.is_some() {
                None
            } else {
                Some(working_dir.clone())
            },
            repo_name: repo_name.clone(),
            branch_name: name.clone(),
            base_branch,
            checkout_existing: checkout,
        },
    )
    .await
    {
        stop_spin_err(spinner, &format!("failed to send request: {e}"));
        std::process::exit(1);
    }

    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::WorktreeAdded { path, .. }) => {
            stop_spin(spinner, &format!("worktree '{name}' created at {path}"));

            // If -p/--prompt was provided, start an agent in the worktree
            if let Some(prompt_text) = prompt {
                let actual_prompt = if prompt_text.is_empty() {
                    None
                } else {
                    Some(prompt_text)
                };
                println!();
                handle_start(
                    actual_prompt,
                    true, // background — don't attach
                    false,
                    None,
                    clust_ipc::DEFAULT_HUB.into(),
                    Some(path),
                )
                .await;
            }
        }
        Ok(HubMessage::Error { message }) => {
            stop_spin_err(spinner, &message);
            std::process::exit(1);
        }
        _ => {
            stop_spin_err(spinner, "unexpected response from hub");
            std::process::exit(1);
        }
    }
    println!();
}

async fn handle_wt_rm(
    repo_name: Option<String>,
    delete_local: bool,
    branch: Option<String>,
    force: bool,
) {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    // Determine target branch
    let branch_name = match branch {
        Some(b) => b,
        None => match detect_current_worktree_branch(&working_dir) {
            Some(b) => b,
            None => {
                eprintln!(
                    "  {}✘{} {}not inside a clust worktree; use -b to specify the branch{}",
                    theme::ERROR,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    theme::RESET,
                );
                println!();
                std::process::exit(1);
            }
        },
    };

    let spinner = spin(&format!("removing worktree '{branch_name}'"));
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    if let Err(e) = clust_ipc::send_message(
        &mut stream,
        &CliMessage::RemoveWorktree {
            working_dir: if repo_name.is_some() {
                None
            } else {
                Some(working_dir.clone())
            },
            repo_name: repo_name.clone(),
            branch_name: branch_name.clone(),
            delete_local_branch: delete_local,
            force,
        },
    )
    .await
    {
        stop_spin_err(spinner, &format!("failed to send request: {e}"));
        std::process::exit(1);
    }

    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::WorktreeRemoved {
            stopped_agents, ..
        }) => {
            let mut msg = format!("worktree '{branch_name}' removed");
            if stopped_agents > 0 {
                msg.push_str(&format!(
                    " ({stopped_agents} agent{} stopped)",
                    if stopped_agents == 1 { "" } else { "s" }
                ));
            }
            stop_spin(spinner, &msg);
        }
        Ok(HubMessage::Error { message }) => {
            stop_spin_err(spinner, &message);
            std::process::exit(1);
        }
        _ => {
            stop_spin_err(spinner, "unexpected response from hub");
            std::process::exit(1);
        }
    }
    println!();
}

async fn handle_wt_info(repo_name: Option<String>, name: String) {
    println!();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let spinner = spin("fetching worktree info");
    let mut stream = match ipc::connect_to_hub().await {
        Ok(s) => s,
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to connect to hub: {e}"));
            std::process::exit(1);
        }
    };

    if let Err(e) = clust_ipc::send_message(
        &mut stream,
        &CliMessage::GetWorktreeInfo {
            working_dir: if repo_name.is_some() {
                None
            } else {
                Some(working_dir.clone())
            },
            repo_name: repo_name.clone(),
            branch_name: name.clone(),
        },
    )
    .await
    {
        stop_spin_err(spinner, &format!("failed to send request: {e}"));
        std::process::exit(1);
    }

    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::WorktreeInfoResult { info }) => {
            stop_spin(spinner, &format!("worktree '{}'", info.branch_name));
            println!();

            let status = if info.is_dirty { "dirty" } else { "clean" };
            let status_color = if info.is_dirty {
                theme::WARNING
            } else {
                theme::SUCCESS
            };

            println!(
                "  {}branch{}     {}{}{}",
                theme::TEXT_SECONDARY,
                theme::RESET,
                theme::TEXT_PRIMARY,
                info.branch_name,
                theme::RESET,
            );
            println!(
                "  {}path{}       {}{}{}",
                theme::TEXT_SECONDARY,
                theme::RESET,
                theme::TEXT_PRIMARY,
                info.path,
                theme::RESET,
            );
            println!(
                "  {}status{}     {}{}{}",
                theme::TEXT_SECONDARY,
                theme::RESET,
                status_color,
                status,
                theme::RESET,
            );

            let agent_count = info.active_agents.len();
            println!(
                "  {}agents{}     {}{}{}",
                theme::TEXT_SECONDARY,
                theme::RESET,
                theme::TEXT_PRIMARY,
                if agent_count == 0 {
                    "none".to_string()
                } else {
                    format!(
                        "{agent_count} running"
                    )
                },
                theme::RESET,
            );

            for agent in &info.active_agents {
                println!(
                    "               {}{}{}  {}{}{}  {}{}{}",
                    theme::ACCENT,
                    agent.id,
                    theme::RESET,
                    theme::TEXT_PRIMARY,
                    agent.agent_binary,
                    theme::RESET,
                    theme::TEXT_SECONDARY,
                    format_started(&agent.started_at),
                    theme::RESET,
                );
            }
        }
        Ok(HubMessage::Error { message }) => {
            stop_spin_err(spinner, &message);
            std::process::exit(1);
        }
        _ => {
            stop_spin_err(spinner, "unexpected response from hub");
            std::process::exit(1);
        }
    }
    println!();
}

/// Interactive default agent selector.
///
/// Shows known agents with a checkmark on the current default, plus a "Custom..."
/// option for entering an arbitrary command.
fn run_default_selector(
    installed: &[&clust_ipc::agents::KnownAgent],
    current: Option<&str>,
    header: &str,
) -> DefaultPickerResult {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};

    // Items: cancel + installed agents + custom
    let item_count = 2 + installed.len();
    let mut selected: usize = 1; // Start on first agent, not cancel
    let mut stdout = io::stdout();

    // Header
    write!(
        stdout,
        "  {}{}{}\n\n",
        theme::TEXT_SECONDARY,
        header,
        theme::RESET,
    )
    .unwrap();
    stdout.flush().unwrap();

    let _guard = RawModeGuard::new();

    render_default_selector(&mut stdout, installed, current, selected, item_count);

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
                selected = selected.saturating_sub(1);
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
        render_default_selector(&mut stdout, installed, current, selected, item_count);
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
    } else if selected <= installed.len() {
        DefaultPickerResult::Selected(installed[selected - 1].binary.to_string())
    } else {
        // Custom: prompt for input
        read_custom_agent()
    }
}

fn render_default_selector(
    stdout: &mut io::Stdout,
    installed: &[&clust_ipc::agents::KnownAgent],
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
        } else if i <= installed.len() {
            // Installed agent
            let agent = installed[i - 1];
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
            let untested_tag = if !agent.tested {
                format!(" {}UNTESTED{}", theme::WARNING_TEXT, theme::RESET)
            } else {
                String::new()
            };
            format!(
                "{}{}{} {}({}){}{untested_tag}{check}",
                name_color,
                agent.display_name,
                theme::RESET,
                bin_color,
                agent.binary,
                theme::RESET,
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
                current.is_some() && !installed.iter().any(|a| Some(a.binary) == current);
            let check = if is_custom_current {
                format!(
                    " {}✔{} {}({}){}",
                    theme::SUCCESS,
                    theme::RESET,
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
        theme::TEXT_SECONDARY,
        theme::RESET,
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
