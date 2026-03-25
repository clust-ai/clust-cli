use std::sync::Arc;

use tokio::sync::Mutex;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};

use clust_pool::PoolEvent;

fn main() {
    let state = Arc::new(Mutex::new(clust_pool::agent::PoolState::new()));

    let event_loop = EventLoopBuilder::<PoolEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Spawn tokio runtime on a background thread for the IPC server
    let state_clone = state.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(clust_pool::ipc::run_ipc_server(proxy, state_clone));
    });

    // Tray icon must live as long as the event loop
    let mut tray_icon_holder: Option<tray_icon::TrayIcon> = None;

    // Run the tao event loop on the main thread (required for macOS tray icon)
    event_loop.run(move |event, _event_loop, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                // Hide dock icon — must be called at runtime after Cocoa app is initialized
                #[cfg(target_os = "macos")]
                {
                    use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
                    _event_loop.set_activation_policy_at_runtime(ActivationPolicy::Accessory);
                }
                tray_icon_holder = Some(clust_pool::tray::create_tray_icon());
            }
            Event::UserEvent(PoolEvent::Shutdown) => {
                tray_icon_holder.take();
                *control_flow = ControlFlow::Exit;
                std::process::exit(0);
            }
            _ => {}
        }
    });
}
