pub mod agent;
pub mod ipc;
pub mod tray;

#[derive(Debug)]
pub enum PoolEvent {
    Shutdown,
}
