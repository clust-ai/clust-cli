use std::sync::Arc;

use tokio::sync::Mutex;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{MenuEvent, MenuId};

use clust_pool::PoolEvent;
use clust_pool::agent::SharedPoolState;

fn main() {
    let state = Arc::new(Mutex::new(clust_pool::agent::PoolState::new()));

    let event_loop = EventLoopBuilder::<PoolEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Channel for triggering async shutdown from the main thread (tray quit)
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // Spawn tokio runtime on a background thread for the IPC server and signal handlers
    let state_clone = state.clone();
    let signal_proxy = proxy.clone();
    let signal_state = state.clone();
    let tray_shutdown_state = state.clone();
    let tray_shutdown_proxy = proxy.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async move {
            // Spawn signal handler for clean shutdown on SIGTERM/SIGINT
            tokio::spawn(handle_signals(signal_proxy, signal_state));

            // Spawn handler for tray quit → async agent shutdown
            tokio::spawn(async move {
                if shutdown_rx.recv().await.is_some() {
                    clust_pool::agent::shutdown_agents(&tray_shutdown_state).await;
                    let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;
                    let _ = tray_shutdown_proxy.send_event(PoolEvent::Shutdown);
                }
            });

            clust_pool::ipc::run_ipc_server(proxy, state_clone).await;
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
                    // Trigger async agent shutdown; PoolEvent::Shutdown arrives when done
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
                let (icon, qid) = clust_pool::tray::create_tray_icon();
                tray_icon_holder = Some(icon);
                quit_id = Some(qid);
            }
            Event::UserEvent(PoolEvent::Shutdown) => {
                shutdown(&mut tray_icon_holder, control_flow);
            }
            _ => {}
        }
    });
}

fn shutdown(
    tray_icon_holder: &mut Option<tray_icon::TrayIcon>,
    control_flow: &mut ControlFlow,
) {
    tray_icon_holder.take();
    let _ = std::fs::remove_file(clust_ipc::socket_path());
    *control_flow = ControlFlow::Exit;
    std::process::exit(0);
}

async fn handle_signals(
    proxy: tao::event_loop::EventLoopProxy<PoolEvent>,
    state: SharedPoolState,
) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }

    clust_pool::agent::shutdown_agents(&state).await;
    let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;
    let _ = proxy.send_event(PoolEvent::Shutdown);
}
