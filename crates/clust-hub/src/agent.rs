use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use portable_pty::{CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Mutex};

// ---------------------------------------------------------------------------
// ReplayBuffer — per-agent ring buffer of raw PTY output
// ---------------------------------------------------------------------------

/// Default replay buffer capacity: 512 KB per agent.
const REPLAY_BUFFER_CAPACITY: usize = 512 * 1024;

/// Ring buffer that stores recent PTY output bytes for replay on late attach.
///
/// Uses `std::sync::Mutex` (not tokio) because it is accessed from the
/// blocking PTY reader thread. The critical section is just a memcpy.
pub struct ReplayBuffer {
    data: VecDeque<u8>,
    capacity: usize,
}

impl Default for ReplayBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayBuffer {
    pub fn new() -> Self {
        Self {
            data: VecDeque::with_capacity(REPLAY_BUFFER_CAPACITY),
            capacity: REPLAY_BUFFER_CAPACITY,
        }
    }

    /// Append bytes, evicting the oldest if over capacity.
    pub fn push(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
        if self.data.len() > self.capacity {
            let excess = self.data.len() - self.capacity;
            self.data.drain(..excess);
        }
    }

    /// Return a copy of all buffered bytes.
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.iter().copied().collect()
    }
}

/// Shared hub state, accessible from all IPC handler tasks.
pub type SharedHubState = Arc<Mutex<HubState>>;

/// Top-level hub state holding all running agents and terminal sessions.
#[derive(Default)]
pub struct HubState {
    pub agents: HashMap<String, AgentEntry>,
    pub terminals: HashMap<String, TerminalEntry>,
    pub default_agent: Option<String>,
    pub bypass_permissions: bool,
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
        self.bypass_permissions = crate::db::get_bypass_permissions(&conn);
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
    /// Ring buffer of recent PTY output for replay on late attach.
    pub replay_buffer: Arc<std::sync::Mutex<ReplayBuffer>>,
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
    /// Path to the per-spawn settings file (Stop hook for "exit when done").
    /// Removed when the agent process exits.
    pub settings_path: Option<std::path::PathBuf>,
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
pub fn generate_agent_id<V>(existing: &HashMap<String, V>) -> String {
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
    pub plan_mode: bool,
    pub allow_bypass: bool,
    pub hub: String,
    pub repo_path: Option<String>,
    pub branch_name: Option<String>,
    pub is_worktree: bool,
    /// When true and the resolved agent binary advertises a Stop hook, the hub
    /// writes a per-spawn settings file and passes `--settings <path>` so the
    /// agent terminates itself at its first natural stopping point.
    pub exit_when_done: bool,
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
    if params.plan_mode {
        if let Some(args) = clust_ipc::agents::plan_mode_args_for(&binary) {
            for arg in args {
                cmd.arg(arg);
            }
        }
        if state.bypass_permissions {
            if let Some(args) = clust_ipc::agents::bypass_permissions_args_for(&binary) {
                for arg in args {
                    cmd.arg(arg);
                }
            }
        } else if params.allow_bypass {
            if let Some(args) = clust_ipc::agents::allow_bypass_args_for(&binary) {
                for arg in args {
                    cmd.arg(arg);
                }
            }
        }
    } else if state.bypass_permissions {
        if let Some(args) = clust_ipc::agents::bypass_permissions_args_for(&binary) {
            for arg in args {
                cmd.arg(arg);
            }
        }
    } else if params.accept_edits {
        if let Some(args) = clust_ipc::agents::accept_edits_args_for(&binary) {
            for arg in args {
                cmd.arg(arg);
            }
        }
    }

    let settings_path = if params.exit_when_done && clust_ipc::agents::supports_stop_hook(&binary) {
        match write_exit_when_done_settings(&id) {
            Ok(path) => {
                cmd.arg("--settings");
                cmd.arg(&path);
                Some(path)
            }
            Err(e) => {
                eprintln!("[hub] exit-when-done settings injection failed for agent {id}: {e}");
                None
            }
        }
    } else {
        None
    };

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

    let (output_tx, _) = broadcast::channel::<AgentEvent>(1024);
    let replay_buffer = Arc::new(std::sync::Mutex::new(ReplayBuffer::new()));

    // Start background task to read PTY output and broadcast to subscribers
    spawn_pty_reader(
        reader,
        child,
        output_tx.clone(),
        replay_buffer.clone(),
        id.clone(),
        shared_state,
    );

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
        replay_buffer,
        attached_count: Arc::new(AtomicUsize::new(0)),
        client_sizes: HashMap::new(),
        current_pty_size: (params.cols, params.rows),
        active_client_id: None,
        next_client_id: AtomicU64::new(0),
        repo_path: params.repo_path,
        branch_name: params.branch_name,
        is_worktree: params.is_worktree,
        settings_path,
    };

    state.agents.insert(id.clone(), entry);
    Ok((id, binary_name))
}

/// Parameters for `create_worktree_and_spawn_agent`.
pub struct CreateWorktreeParams<'a> {
    pub state: &'a SharedHubState,
    pub repo_path: &'a str,
    pub target_branch: Option<&'a str>,
    pub new_branch: Option<&'a str>,
    pub prompt: Option<String>,
    pub agent_binary: Option<String>,
    pub plan_mode: bool,
    pub allow_bypass: bool,
    pub hub: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub exit_when_done: bool,
}

/// Create a git worktree and spawn an agent in it.
/// Returns `(agent_id, agent_binary, working_dir)` on success.
pub async fn create_worktree_and_spawn_agent(
    params: CreateWorktreeParams<'_>,
) -> Result<(String, String, String), String> {
    let CreateWorktreeParams {
        state,
        repo_path,
        target_branch,
        new_branch,
        prompt,
        agent_binary,
        plan_mode,
        allow_bypass,
        hub,
        cols,
        rows,
        exit_when_done,
    } = params;
    let sanitized_new = new_branch.map(clust_ipc::branch::sanitize_branch_name);
    let branch_name = sanitized_new
        .as_deref()
        .or(target_branch)
        .ok_or("either target_branch or new_branch must be provided")?
        .to_string();

    let repo_root = std::path::Path::new(repo_path);
    let checkout_existing = new_branch.is_none();
    let base = if new_branch.is_some() {
        target_branch
    } else {
        None
    };

    let worktree_path = crate::repo::add_worktree(repo_root, &branch_name, base, checkout_existing)
        .await
        .map_err(|e| {
            if e.contains("already checked out") {
                format!(
                    "Branch '{}' is already checked out and cannot be used as a worktree.",
                    branch_name
                )
            } else {
                e
            }
        })?;

    let working_dir = worktree_path.to_string_lossy().into_owned();

    let (wt_repo_path, wt_branch_name, is_worktree) =
        match crate::repo::detect_git_root(&working_dir) {
            Some(root) => {
                let rp = root.to_string_lossy().into_owned();
                let (bn, iw) = crate::repo::detect_branch_and_worktree(&working_dir);
                (Some(rp), bn.or(Some(branch_name)), iw)
            }
            None => (Some(repo_path.to_string()), Some(branch_name), true),
        };

    let result = {
        let mut hub_state = state.lock().await;
        spawn_agent(
            &mut hub_state,
            SpawnAgentParams {
                prompt,
                agent_binary,
                working_dir: working_dir.clone(),
                cols,
                rows,
                accept_edits: false,
                plan_mode,
                allow_bypass,
                hub: hub.to_string(),
                repo_path: wt_repo_path,
                branch_name: wt_branch_name,
                is_worktree,
                exit_when_done,
            },
            state.clone(),
        )
    };

    match result {
        Ok((id, binary)) => {
            let hub_state = state.lock().await;
            if let Some(ref db) = hub_state.db {
                let name = std::path::Path::new(repo_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| repo_path.to_string());
                let _ = crate::db::register_repo(db, repo_path, &name, "");
            }
            Ok((id, binary, working_dir))
        }
        Err(e) => Err(e),
    }
}

/// Write a per-agent settings file that registers a `Stop` hook firing
/// `clust internal stop-hook`, which terminates the parent agent process so
/// the task transitions to Done as soon as the model finishes responding.
///
/// The file lives at `<clust_dir>/agents/<agent_id>/settings.json` and is
/// passed to the agent via `--settings <path>`. It is removed when the agent
/// exits (see the PTY reader's cleanup path).
fn write_exit_when_done_settings(agent_id: &str) -> Result<std::path::PathBuf, String> {
    let dir = clust_ipc::clust_dir().join("agents").join(agent_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create agent settings dir: {e}"))?;
    let path = dir.join("settings.json");
    let cmd = stop_hook_command()?;
    let json = format!(
        "{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":\"{}\"}}]}}]}}}}",
        cmd.replace('\\', "\\\\").replace('"', "\\\"")
    );
    std::fs::write(&path, json).map_err(|e| format!("failed to write agent settings file: {e}"))?;
    Ok(path)
}

/// Resolve the absolute command string the agent's Stop hook should invoke.
/// Prefers a `clust` binary sitting next to the running `clust-hub` so users
/// who installed both into `~/.clust/bin/` get the expected behavior even when
/// the hook runs without inheriting `PATH`.
fn stop_hook_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("failed to resolve current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "current_exe has no parent dir".to_string())?;
    let candidate = dir.join("clust");
    let cli = if candidate.exists() {
        candidate
    } else {
        std::path::PathBuf::from("clust")
    };
    Ok(format!("{} internal stop-hook", cli.display()))
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
    replay_buf: Arc<std::sync::Mutex<ReplayBuffer>>,
    agent_id: String,
    state: SharedHubState,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    replay_buf.lock().unwrap().push(&chunk);
                    let _ = output_tx.send(AgentEvent::Output(chunk));
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

        // Remove agent from shared state and cascade-kill any terminals this
        // agent spawned so long-lived processes (dev servers, etc.) don't
        // linger after the agent dies. If the agent fulfilled a scheduled
        // task, mark it Complete in the same lock window so dependents start
        // getting unblocked immediately on the next scheduler tick.
        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            let (terminal_ids, settings_path) = {
                let mut hub = state.lock().await;
                let removed = hub.agents.remove(&agent_id);
                if let Some(ref conn) = hub.db {
                    let _ =
                        crate::db::mark_scheduled_task_complete_by_agent(conn, &agent_id);
                }
                let tids: Vec<String> = hub
                    .terminals
                    .values()
                    .filter(|t| t.agent_id.as_deref() == Some(agent_id.as_str()))
                    .map(|t| t.id.clone())
                    .collect();
                (tids, removed.and_then(|a| a.settings_path))
            };
            if let Some(path) = settings_path {
                let _ = std::fs::remove_file(&path);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
            for tid in terminal_ids {
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = stop_terminal(&state, &tid).await;
                });
            }
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
        entry.pid.ok_or_else(|| format!("agent {id} has no PID"))?
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

// ---------------------------------------------------------------------------
// Terminal session management
// ---------------------------------------------------------------------------

/// A running terminal shell session managed by the hub.
pub struct TerminalEntry {
    pub id: String,
    pub working_dir: String,
    pub pid: Option<u32>,
    pub pty_master: Box<dyn MasterPty + Send>,
    pub pty_writer: Box<dyn std::io::Write + Send>,
    pub output_tx: broadcast::Sender<AgentEvent>,
    pub replay_buffer: Arc<std::sync::Mutex<ReplayBuffer>>,
    pub attached_count: Arc<AtomicUsize>,
    pub client_sizes: HashMap<u64, (u16, u16)>,
    pub current_pty_size: (u16, u16),
    pub active_client_id: Option<u64>,
    pub(crate) next_client_id: AtomicU64,
    /// Agent that spawned this terminal, if any. When the agent exits the
    /// terminal is killed alongside it so child processes (dev servers, etc.)
    /// do not linger.
    pub agent_id: Option<String>,
}

impl TerminalEntry {
    pub fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

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

/// Generate a unique terminal ID with "t" prefix.
pub fn generate_terminal_id(existing: &HashMap<String, TerminalEntry>) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    loop {
        let bytes: [u8; 3] = rng.gen();
        let id = format!("t{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2]);
        if !existing.contains_key(&id) {
            return id;
        }
    }
}

/// Spawn a terminal shell session inside a PTY.
pub fn spawn_terminal(
    state: &mut HubState,
    working_dir: String,
    cols: u16,
    rows: u16,
    agent_id: Option<String>,
    shared_state: SharedHubState,
) -> Result<String, String> {
    let id = generate_terminal_id(&state.terminals);

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("PTY open failed: {e}"))?;

    let mut cmd = CommandBuilder::new(&shell);
    cmd.arg("-l");
    cmd.cwd(&working_dir);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn failed: {e}"))?;

    let pid = child.process_id();
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("take writer failed: {e}"))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("clone reader failed: {e}"))?;

    let (output_tx, _) = broadcast::channel::<AgentEvent>(1024);
    let replay_buffer = Arc::new(std::sync::Mutex::new(ReplayBuffer::new()));

    spawn_terminal_pty_reader(
        reader,
        child,
        output_tx.clone(),
        replay_buffer.clone(),
        id.clone(),
        shared_state,
    );

    let entry = TerminalEntry {
        id: id.clone(),
        working_dir,
        pid,
        pty_master: pair.master,
        pty_writer: writer,
        output_tx,
        replay_buffer,
        attached_count: Arc::new(AtomicUsize::new(0)),
        client_sizes: HashMap::new(),
        current_pty_size: (cols, rows),
        active_client_id: None,
        next_client_id: AtomicU64::new(0),
        agent_id,
    };

    state.terminals.insert(id.clone(), entry);
    Ok(id)
}

/// Background task that reads terminal PTY output and broadcasts it.
fn spawn_terminal_pty_reader(
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send>,
    output_tx: broadcast::Sender<AgentEvent>,
    replay_buf: Arc<std::sync::Mutex<ReplayBuffer>>,
    terminal_id: String,
    state: SharedHubState,
) {
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    replay_buf.lock().unwrap().push(&chunk);
                    let _ = output_tx.send(AgentEvent::Output(chunk));
                }
                Err(_) => break,
            }
        }

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

        let handle = tokio::runtime::Handle::current();
        handle.block_on(async {
            let mut hub = state.lock().await;
            hub.terminals.remove(&terminal_id);
        });
    });
}

/// Terminate a terminal session by ID.
pub async fn stop_terminal(state: &SharedHubState, id: &str) -> Result<(), String> {
    let pid = {
        let hub = state.lock().await;
        let entry = hub
            .terminals
            .get(id)
            .ok_or_else(|| format!("terminal {id} not found"))?;
        entry
            .pid
            .ok_or_else(|| format!("terminal {id} has no PID"))?
    };

    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

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
        let mut all_pids: Vec<u32> = hub.agents.values().filter_map(|e| e.pid).collect();
        all_pids.extend(hub.terminals.values().filter_map(|e| e.pid));
        pids = all_pids;

        // Notify all attached clients that the hub is shutting down
        for entry in hub.agents.values() {
            let _ = entry.output_tx.send(AgentEvent::HubShutdown);
        }
        for entry in hub.terminals.values() {
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
        let existing: HashMap<String, ()> = HashMap::new();
        let id = generate_agent_id(&existing);
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn agent_id_avoids_collisions() {
        // Use a value-less map so the test doesn't allocate 100 PTYs (CI
        // environments without a configured PTY device fail otherwise).
        let mut existing: HashMap<String, ()> = HashMap::new();
        for i in 0..100 {
            existing.insert(format!("{:06x}", i), ());
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
                    replay_buffer: Arc::new(std::sync::Mutex::new(ReplayBuffer::new())),
                    attached_count: Arc::new(AtomicUsize::new(0)),
                    client_sizes: HashMap::new(),
                    current_pty_size: (80, 24),
                    active_client_id: None,
                    next_client_id: AtomicU64::new(0),
                    repo_path: None,
                    branch_name: None,
                    is_worktree: false,
                    settings_path: None,
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
