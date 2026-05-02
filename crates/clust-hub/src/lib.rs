pub mod agent;
pub mod batch;
pub mod db;
pub mod inbox;
pub mod ipc;
pub mod orchestrator;
pub mod orchestrator_validate;
pub mod repo;
pub mod tray;

#[derive(Debug)]
pub enum HubEvent {
    Shutdown,
}

/// Abstraction for signaling the main loop to shut down.
/// In GUI mode, wraps tao's EventLoopProxy. In headless mode, wraps a tokio channel.
pub trait ShutdownSignal: Send + Sync + 'static {
    fn signal_shutdown(&self);
}

/// GUI mode: wraps tao::event_loop::EventLoopProxy<HubEvent>.
pub struct TaoShutdownSignal {
    proxy: tao::event_loop::EventLoopProxy<HubEvent>,
}

impl TaoShutdownSignal {
    pub fn new(proxy: tao::event_loop::EventLoopProxy<HubEvent>) -> Self {
        Self { proxy }
    }
}

impl ShutdownSignal for TaoShutdownSignal {
    fn signal_shutdown(&self) {
        let _ = self.proxy.send_event(HubEvent::Shutdown);
    }
}

/// Headless mode: wraps a tokio unbounded sender.
pub struct TokioShutdownSignal {
    tx: tokio::sync::mpsc::UnboundedSender<()>,
}

impl TokioShutdownSignal {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<()>) -> Self {
        Self { tx }
    }
}

impl ShutdownSignal for TokioShutdownSignal {
    fn signal_shutdown(&self) {
        let _ = self.tx.send(());
    }
}

/// Returns true if a GUI display server appears available.
/// On macOS, always returns true. On Linux, checks for X11/Wayland env vars.
pub fn has_display() -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
