use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use portable_pty::{CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Mutex};

/// Shared pool state, accessible from all IPC handler tasks.
pub type SharedPoolState = Arc<Mutex<PoolState>>;

/// Top-level pool state holding all running agents.
pub struct PoolState {
    pub agents: HashMap<String, AgentEntry>,
    pub default_agent: String,
}

impl PoolState {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            default_agent: "claude".to_string(),
        }
    }
}

/// A running agent managed by the pool.
pub struct AgentEntry {
    pub id: String,
    pub agent_binary: String,
    pub started_at: String,
    pub working_dir: String,
    pub pid: Option<u32>,
    pub pty_master: Box<dyn MasterPty + Send>,
    pub pty_writer: Box<dyn std::io::Write + Send>,
    pub output_tx: broadcast::Sender<AgentEvent>,
    pub attached_count: Arc<AtomicUsize>,
}

/// Events broadcast from an agent's PTY reader to all attached clients.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    Output(Vec<u8>),
    Exited(i32),
    PoolShutdown,
}

/// Generate a unique 6-character hex agent ID.
pub fn generate_agent_id(existing: &HashMap<String, AgentEntry>) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    loop {
        let bytes: [u8; 3] = rng.gen();
        let id = format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2]);
        if !existing.contains_key(&id) {
            return id;
        }
    }
}

/// Spawn a new agent process inside a PTY.
///
/// Returns the agent ID on success. The agent is added to `state.agents` and
/// a background task is started to read PTY output and broadcast it.
pub fn spawn_agent(
    state: &mut PoolState,
    prompt: Option<String>,
    agent_binary: Option<String>,
    working_dir: String,
    cols: u16,
    rows: u16,
    shared_state: SharedPoolState,
) -> Result<(String, String), String> {
    let binary = agent_binary.unwrap_or_else(|| state.default_agent.clone());
    let id = generate_agent_id(&state.agents);

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("PTY open failed: {e}"))?;

    let mut cmd = CommandBuilder::new(&binary);
    if let Some(ref p) = prompt {
        cmd.arg(p);
    }
    cmd.cwd(&working_dir);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn failed: {e}"))?;

    let pid = child.process_id();

    // Child owns the slave side; drop our handle.
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take writer failed: {e}"))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone reader failed: {e}"))?;

    let (output_tx, _) = broadcast::channel::<AgentEvent>(256);

    // Start background task to read PTY output and broadcast to subscribers
    spawn_pty_reader(reader, child, output_tx.clone(), id.clone(), shared_state);

    let started_at = chrono::Utc::now().to_rfc3339();
    let binary_name = binary.clone();

    let entry = AgentEntry {
        id: id.clone(),
        agent_binary: binary,
        started_at,
        working_dir,
        pid,
        pty_master: pair.master,
        pty_writer: writer,
        output_tx,
        attached_count: Arc::new(AtomicUsize::new(0)),
    };

    state.agents.insert(id.clone(), entry);
    Ok((id, binary_name))
}

/// Background task that reads from the PTY master and broadcasts output.
///
/// Runs on a blocking thread because `portable-pty` provides `std::io::Read`.
/// On EOF (agent exit), waits for exit code, broadcasts `AgentEvent::Exited`,
/// and removes the agent from shared state.
fn spawn_pty_reader(
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send>,
    output_tx: broadcast::Sender<AgentEvent>,
    agent_id: String,
    state: SharedPoolState,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = output_tx.send(AgentEvent::Output(buf[..n].to_vec()));
                }
                Err(_) => break,
            }
        }

        // Agent exited — get the exit code
        let exit_code = match child.wait() {
            Ok(status) => {
                if status.success() {
                    0
                } else {
                    1
                }
            }
            Err(_) => -1,
        };

        let _ = output_tx.send(AgentEvent::Exited(exit_code));

        // Remove agent from shared state
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            let mut pool = state.lock().await;
            pool.agents.remove(&agent_id);
        });
    });
}

/// Terminate all running agents during pool shutdown.
///
/// 1. Notify all attached CLI clients via broadcast channels
/// 2. SIGTERM all agent processes
/// 3. Wait 3 seconds for graceful exit
/// 4. SIGKILL any remaining agents
pub async fn shutdown_agents(state: &SharedPoolState) {
    let pids: Vec<u32>;
    {
        let pool = state.lock().await;
        pids = pool.agents.values().filter_map(|e| e.pid).collect();

        // Notify all attached clients that the pool is shutting down
        for entry in pool.agents.values() {
            let _ = entry.output_tx.send(AgentEvent::PoolShutdown);
        }
    }

    if pids.is_empty() {
        return;
    }

    // Send SIGTERM to all agents
    for &pid in &pids {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }

    // Wait 3 seconds for graceful exit
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // SIGKILL any agents still alive
    for &pid in &pids {
        // kill(pid, 0) checks if process exists without sending a signal
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_is_6_char_hex() {
        let existing = HashMap::new();
        let id = generate_agent_id(&existing);
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn agent_id_avoids_collisions() {
        let mut existing = HashMap::new();
        // Fill with many IDs to test collision avoidance
        for i in 0..100 {
            let id = format!("{:06x}", i);
            existing.insert(
                id.clone(),
                AgentEntry {
                    id: id.clone(),
                    agent_binary: "test".into(),
                    started_at: "2026-01-01T00:00:00Z".into(),
                    working_dir: "/tmp".into(),
                    pid: None,
                    pty_master: create_dummy_pty_master(),
                    pty_writer: Box::new(std::io::sink()),
                    output_tx: broadcast::channel(1).0,
                    attached_count: Arc::new(AtomicUsize::new(0)),
                },
            );
        }
        let id = generate_agent_id(&existing);
        assert!(!existing.contains_key(&id));
        assert_eq!(id.len(), 6);
    }

    #[test]
    fn pool_state_new_defaults() {
        let state = PoolState::new();
        assert!(state.agents.is_empty());
        assert_eq!(state.default_agent, "claude");
    }

    /// Helper: create a real PTY master for testing structs that need one.
    fn create_dummy_pty_master() -> Box<dyn MasterPty + Send> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty for test");
        drop(pair.slave);
        pair.master
    }
}
