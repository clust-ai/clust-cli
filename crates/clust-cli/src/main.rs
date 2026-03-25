mod cli;
mod ipc;
mod pool_launcher;
mod theme;
mod ui;

use clap::Parser;
use std::io::{self, Write};

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

    if let Some(cli::Commands::Ui) = args.command {
        if let Err(e) = ui::run() {
            eprintln!("  {}ui error: {e}{}", theme::ERROR, theme::RESET);
            std::process::exit(1);
        }
        return;
    }

    if args.stop {
        println!();
        let spinner = spin("stopping clust pool");
        match ipc::try_connect().await {
            Ok(mut stream) => {
                match ipc::send_stop(&mut stream).await {
                    Ok(()) => stop_spin(spinner, "clust pool stopped"),
                    Err(e) => {
                        stop_spin_err(spinner, &format!("failed to stop clust pool: {e}"));
                        std::process::exit(1);
                    }
                }
            }
            Err(_) => {
                stop_spin(spinner, "clust pool is not running");
            }
        }
        return;
    }

    // Default: print logo, start pool, exit
    print_logo();
    let spinner = spin("starting clust pool");

    match ipc::connect_to_pool().await {
        Ok(_stream) => {
            stop_spin(spinner, "clust pool is running");
        }
        Err(e) => {
            stop_spin_err(spinner, &format!("failed to start clust pool: {e}"));
            std::process::exit(1);
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
                theme::ACCENT, SPINNER[i % SPINNER.len()], theme::RESET,
                theme::TEXT_SECONDARY, msg, theme::RESET,
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
        theme::SUCCESS, theme::RESET,
        theme::TEXT_PRIMARY, msg, theme::RESET,
    );
}

fn stop_spin_err(handle: tokio::task::JoinHandle<()>, msg: &str) {
    handle.abort();
    print!("\r\x1b[2K");
    eprintln!(
        "  {}\u{2718}{} {}{}{}\n",
        theme::ERROR, theme::RESET,
        theme::TEXT_PRIMARY, msg, theme::RESET,
    );
}
