pub mod agent;
pub mod db;
pub mod ipc;
pub mod repo;
pub mod tray;

#[derive(Debug)]
pub enum PoolEvent {
    Shutdown,
}
