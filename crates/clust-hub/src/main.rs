use std::sync::Arc;

use tokio::sync::Mutex;

use clust_hub::agent::SharedHubState;
use clust_hub::ShutdownSignal;

fn main() {
    let state = Arc::new(Mutex::new(clust_hub::agent::HubState::new()));

    if clust_hub::has_display() {
        run_gui_mode(state);
    } else {
        run_headless_mode(state);
    }
}

/// Run with tao event loop and tray icon (macOS, Linux with display).
fn run_gui_mode(state: SharedHubState) {
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::menu::{MenuEvent, MenuId};

    use clust_hub::HubEvent;

    let event_loop = EventLoopBuilder::<HubEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let shutdown_signal: Arc<dyn ShutdownSignal> =
        Arc::new(clust_hub::TaoShutdownSignal::new(proxy));

    // Channel for triggering async shutdown from the main thread (tray quit)
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Spawn tokio runtime on a background thread for the IPC server and signal handlers
    let ipc_signal = shutdown_signal.clone();
    let ipc_state = state.clone();
    let signal_signal = shutdown_signal.clone();
    let signal_state = state.clone();
    let tray_shutdown_state = state.clone();
    let tray_shutdown_signal = shutdown_signal.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async move {
            // Spawn signal handler for clean shutdown on SIGTERM/SIGINT
            tokio::spawn(handle_signals(signal_signal, signal_state));

            // Spawn handler for tray quit → async agent shutdown
            tokio::spawn(async move {
                if shutdown_rx.recv().await.is_some() {
                    clust_hub::agent::shutdown_agents(&tray_shutdown_state).await;
                    let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;
                    tray_shutdown_signal.signal_shutdown();
                }
            });

            clust_hub::ipc::run_ipc_server(ipc_signal, ipc_state).await;
        });
    });

    // Tray icon and quit menu item ID must live as long as the event loop
    let mut tray_icon_holder: Option<tray_icon::TrayIcon> = None;
    let mut quit_id: Option<MenuId> = None;

    // Run the tao event loop on the main thread (required for macOS tray icon)
    event_loop.run(move |event, _event_loop, control_flow| {
        *control_flow = ControlFlow::Wait;

        // Check for tray menu events (e.g. Quit clicked)
        if let Some(ref qid) = quit_id {
            if let Ok(menu_event) = MenuEvent::receiver().try_recv() {
                if menu_event.id == *qid {
                    // Trigger async agent shutdown; HubEvent::Shutdown arrives when done
                    let _ = shutdown_tx.send(());
                }
            }
        }

        match event {
            Event::NewEvents(StartCause::Init) => {
                // Hide dock icon — must be called at runtime after Cocoa app is initialized
                #[cfg(target_os = "macos")]
                {
                    use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
                    _event_loop.set_activation_policy_at_runtime(ActivationPolicy::Accessory);
                }
                match clust_hub::tray::create_tray_icon() {
                    Ok((icon, qid)) => {
                        tray_icon_holder = Some(icon);
                        quit_id = Some(qid);
                    }
                    Err(e) => {
                        eprintln!("warning: tray icon unavailable: {e}");
                    }
                }
            }
            Event::UserEvent(HubEvent::Shutdown) => {
                shutdown_gui(&mut tray_icon_holder, control_flow);
            }
            _ => {}
        }
    });
}

/// Run as a pure daemon without GUI (headless Linux).
fn run_headless_mode(state: SharedHubState) {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    rt.block_on(async move {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let shutdown_signal: Arc<dyn ShutdownSignal> =
            Arc::new(clust_hub::TokioShutdownSignal::new(shutdown_tx));

        // Spawn signal handler
        let signal_signal = shutdown_signal.clone();
        let signal_state = state.clone();
        tokio::spawn(handle_signals(signal_signal, signal_state));

        // Spawn IPC server
        let ipc_signal = shutdown_signal.clone();
        let ipc_state = state.clone();
        tokio::spawn(async move {
            clust_hub::ipc::run_ipc_server(ipc_signal, ipc_state).await;
        });

        // Block until shutdown signal received
        shutdown_rx.recv().await;

        // Clean up
        let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;
    });
}

fn shutdown_gui(
    tray_icon_holder: &mut Option<tray_icon::TrayIcon>,
    control_flow: &mut tao::event_loop::ControlFlow,
) {
    tray_icon_holder.take();
    let _ = std::fs::remove_file(clust_ipc::socket_path());
    *control_flow = tao::event_loop::ControlFlow::Exit;
    std::process::exit(0);
}

async fn handle_signals(
    shutdown_signal: Arc<dyn ShutdownSignal>,
    state: SharedHubState,
) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }

    clust_hub::agent::shutdown_agents(&state).await;
    let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;
    shutdown_signal.signal_shutdown();
}
