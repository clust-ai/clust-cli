use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use portable_pty::{CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Mutex};

/// Shared hub state, accessible from all IPC handler tasks.
pub type SharedHubState = Arc<Mutex<HubState>>;

/// Top-level hub state holding all running agents.
#[derive(Default)]
pub struct HubState {
    pub agents: HashMap<String, AgentEntry>,
    pub default_agent: Option<String>,
    pub db: Option<rusqlite::Connection>,
}

impl HubState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize the SQLite database and load the default agent from config.
    pub fn init_db(&mut self) -> Result<(), String> {
        let conn = crate::db::open_or_create()?;
        self.default_agent = crate::db::get_default_agent(&conn);
        self.db = Some(conn);
        Ok(())
    }
}

/// A running agent managed by the hub.
pub struct AgentEntry {
    pub id: String,
    pub agent_binary: String,
    pub started_at: String,
    pub working_dir: String,
    pub hub: String,
    pub pid: Option<u32>,
    pub pty_master: Box<dyn MasterPty + Send>,
    pub pty_writer: Box<dyn std::io::Write + Send>,
    pub output_tx: broadcast::Sender<AgentEvent>,
    pub attached_count: Arc<AtomicUsize>,
    /// Per-client terminal sizes: client_id → (cols, rows).
    pub client_sizes: HashMap<u64, (u16, u16)>,
    /// Current PTY dimensions, used to skip redundant resize calls.
    pub current_pty_size: (u16, u16),
    /// The client that most recently sent a resize or input event.
    pub active_client_id: Option<u64>,
    /// Monotonic counter for assigning unique client IDs.
    pub(crate) next_client_id: AtomicU64,
    /// Git repository root path (if working_dir is inside a git repo).
    pub repo_path: Option<String>,
    /// Git branch the agent is working on.
    pub branch_name: Option<String>,
    /// Whether the agent's working_dir is a git worktree checkout.
    pub is_worktree: bool,
}

impl AgentEntry {
    /// Allocate a unique client ID for a newly attached session.
    pub fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Resize the PTY only if the requested size differs from the current size.
    pub fn resize_pty_if_needed(&mut self, cols: u16, rows: u16) -> bool {
        if self.current_pty_size == (cols, rows) {
            return false;
        }
        let result = self.pty_master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if result.is_ok() {
            self.current_pty_size = (cols, rows);
            true
        } else {
            false
        }
    }
}

/// Events broadcast from an agent's PTY reader to all attached clients.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    Output(Vec<u8>),
    Exited(i32),
    HubShutdown,
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

/// Resolve which agent binary to use: explicit override takes precedence,
/// then the hub's configured default, otherwise error.
pub fn resolve_agent_binary(
    agent_binary: Option<String>,
    default_agent: &Option<String>,
) -> Result<String, String> {
    agent_binary
        .or_else(|| default_agent.clone())
        .ok_or_else(|| "no default agent configured".to_string())
}

/// Parameters for spawning a new agent inside a PTY.
///
/// Git info (`repo_path`, `branch_name`, `is_worktree`) should be pre-computed
/// by the caller BEFORE acquiring the state lock, to avoid holding the lock
/// during potentially slow git operations.
pub struct SpawnAgentParams {
    pub prompt: Option<String>,
    pub agent_binary: Option<String>,
    pub working_dir: String,
    pub cols: u16,
    pub rows: u16,
    pub accept_edits: bool,
    pub hub: String,
    pub repo_path: Option<String>,
    pub branch_name: Option<String>,
    pub is_worktree: bool,
}

/// Spawn a new agent process inside a PTY.
///
/// Returns the agent ID on success. The agent is added to `state.agents` and
/// a background task is started to read PTY output and broadcast it.
pub fn spawn_agent(
    state: &mut HubState,
    params: SpawnAgentParams,
    shared_state: SharedHubState,
) -> Result<(String, String), String> {
    let binary = resolve_agent_binary(params.agent_binary, &state.default_agent)?;
    let id = generate_agent_id(&state.agents);

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: params.rows,
            cols: params.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("PTY open failed: {e}"))?;

    let mut cmd = CommandBuilder::new(&binary);
    if let Some(ref p) = params.prompt {
        cmd.arg(p);
    }
    if params.accept_edits {
        if let Some(args) = clust_ipc::agents::accept_edits_args_for(&binary) {
            for arg in args {
                cmd.arg(arg);
            }
        }
    }
    cmd.cwd(&params.working_dir);

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
        working_dir: params.working_dir,
        hub: params.hub,
        pid,
        pty_master: pair.master,
        pty_writer: writer,
        output_tx,
        attached_count: Arc::new(AtomicUsize::new(0)),
        client_sizes: HashMap::new(),
        current_pty_size: (params.cols, params.rows),
        active_client_id: None,
        next_client_id: AtomicU64::new(0),
        repo_path: params.repo_path,
        branch_name: params.branch_name,
        is_worktree: params.is_worktree,
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
    state: SharedHubState,
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
            let mut hub = state.lock().await;
            hub.agents.remove(&agent_id);
        });
    });
}

/// Terminate a single agent by ID.
///
/// Sends SIGTERM, waits 3 seconds, then SIGKILL if still alive.
/// The existing PTY reader handles cleanup when the process exits.
pub async fn stop_agent(state: &SharedHubState, id: &str) -> Result<(), String> {
    let pid = {
        let hub = state.lock().await;
        let entry = hub
            .agents
            .get(id)
            .ok_or_else(|| format!("agent {id} not found"))?;
        entry
            .pid
            .ok_or_else(|| format!("agent {id} has no PID"))?
    };

    // SIGTERM
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    // Wait 3 seconds for graceful exit
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // SIGKILL if still alive
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    Ok(())
}

/// Terminate all running agents during hub shutdown.
///
/// 1. Notify all attached CLI clients via broadcast channels
/// 2. SIGTERM all agent processes
/// 3. Wait 3 seconds for graceful exit
/// 4. SIGKILL any remaining agents
pub async fn shutdown_agents(state: &SharedHubState) {
    let pids: Vec<u32>;
    {
        let hub = state.lock().await;
        pids = hub.agents.values().filter_map(|e| e.pid).collect();

        // Notify all attached clients that the hub is shutting down
        for entry in hub.agents.values() {
            let _ = entry.output_tx.send(AgentEvent::HubShutdown);
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
                    hub: clust_ipc::DEFAULT_HUB.into(),
                    pid: None,
                    pty_master: create_dummy_pty_master(),
                    pty_writer: Box::new(std::io::sink()),
                    output_tx: broadcast::channel(1).0,
                    attached_count: Arc::new(AtomicUsize::new(0)),
                    client_sizes: HashMap::new(),
                    current_pty_size: (80, 24),
                    active_client_id: None,
                    next_client_id: AtomicU64::new(0),
                    repo_path: None,
                    branch_name: None,
                    is_worktree: false,
                },
            );
        }
        let id = generate_agent_id(&existing);
        assert!(!existing.contains_key(&id));
        assert_eq!(id.len(), 6);
    }

    #[test]
    fn hub_state_new_defaults() {
        let state = HubState::new();
        assert!(state.agents.is_empty());
        assert_eq!(state.default_agent, None);
        assert!(state.db.is_none());
    }

    // ── resolve_agent_binary tests ──────────────────────────────────

    #[test]
    fn resolve_explicit_binary_overrides_default() {
        let result = resolve_agent_binary(Some("aider".into()), &Some("claude".into()));
        assert_eq!(result, Ok("aider".into()));
    }

    #[test]
    fn resolve_falls_back_to_default_when_none() {
        let result = resolve_agent_binary(None, &Some("aider".into()));
        assert_eq!(result, Ok("aider".into()));
    }

    #[test]
    fn resolve_errors_when_both_none() {
        let result = resolve_agent_binary(None, &None);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "no default agent configured");
    }

    #[test]
    fn resolve_explicit_binary_works_without_default() {
        let result = resolve_agent_binary(Some("opencode".into()), &None);
        assert_eq!(result, Ok("opencode".into()));
    }

    // ── stop_agent error path tests ──────────────────────────────────

    #[tokio::test]
    async fn stop_agent_not_found_returns_error() {
        let state: SharedHubState = Arc::new(Mutex::new(HubState::new()));
        let result = stop_agent(&state, "nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn stop_agent_no_pid_returns_error() {
        let state: SharedHubState = Arc::new(Mutex::new(HubState::new()));
        {
            let mut hub = state.lock().await;
            hub.agents.insert(
                "abc123".to_string(),
                AgentEntry {
                    id: "abc123".to_string(),
                    agent_binary: "test".into(),
                    started_at: "2026-01-01T00:00:00Z".into(),
                    working_dir: "/tmp".into(),
                    hub: clust_ipc::DEFAULT_HUB.into(),
                    pid: None,
                    pty_master: create_dummy_pty_master(),
                    pty_writer: Box::new(std::io::sink()),
                    output_tx: broadcast::channel(1).0,
                    attached_count: Arc::new(AtomicUsize::new(0)),
                    client_sizes: HashMap::new(),
                    current_pty_size: (80, 24),
                    active_client_id: None,
                    next_client_id: AtomicU64::new(0),
                    repo_path: None,
                    branch_name: None,
                    is_worktree: false,
                },
            );
        }
        let result = stop_agent(&state, "abc123").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no PID"));
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
