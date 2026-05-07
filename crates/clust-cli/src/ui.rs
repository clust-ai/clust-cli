use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
    ExecutableCommand,
};
use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame, Terminal,
};

use clust_ipc::{AgentInfo, CliMessage, HubMessage, RepoInfo};

use crate::{
    context_menu::{ContextMenu, ContextMenuItem, MenuResult},
    create_agent_modal::{CreateAgentModal, ModalResult},
    detached_agent_modal::{DetachedAgentModal, DetachedModalResult},
    edit_prompt_modal::{EditPromptModal, EditPromptResult},
    ipc,
    overview::{self, OverviewFocus, OverviewState},
    repo_modal::{RepoModal, RepoModalResult},
    schedule::{ScheduleAction, ScheduleState},
    schedule_modal::{ScheduleModalResult, ScheduleTaskModal},
    search_modal::{SearchModal, SearchResult},
    terminal_emulator, theme, version, window_view,
};

/// Maximum interval between two Esc presses to count as a "double-tap".
const DOUBLE_ESC_THRESHOLD: Duration = Duration::from_millis(400);

/// Returns `true` when two Esc presses arrive within [`DOUBLE_ESC_THRESHOLD`].
/// Always records the current instant so the next call can compare.
fn is_double_esc(last: &mut Option<Instant>) -> bool {
    let now = Instant::now();
    let double = last.is_some_and(|t| now.duration_since(t) < DOUBLE_ESC_THRESHOLD);
    *last = Some(now);
    double
}

#[allow(dead_code)]
enum AgentStartResult {
    Started {
        agent_id: String,
        agent_binary: String,
        working_dir: String,
        repo_path: Option<String>,
        branch_name: Option<String>,
        is_worktree: bool,
    },
    Failed(String),
}

enum StatusLevel {
    Error,
    Success,
}

struct StatusMessage {
    text: String,
    level: StatusLevel,
    created: Instant,
}

// ---------------------------------------------------------------------------
// Purge progress modal
// ---------------------------------------------------------------------------

enum PurgeEvent {
    Step(String),
    Done,
    Error(String),
}

struct PurgeProgress {
    repo_name: String,
    steps: Vec<String>,
    done: bool,
    error: Option<String>,
    rx: tokio::sync::mpsc::UnboundedReceiver<PurgeEvent>,
    started: Instant,
}

// ---------------------------------------------------------------------------
// Clone progress modal
// ---------------------------------------------------------------------------

enum CloneEvent {
    Step(String),
    Done,
    Error(String),
}

struct CloneProgress {
    url: String,
    steps: Vec<String>,
    done: bool,
    error: Option<String>,
    rx: tokio::sync::mpsc::UnboundedReceiver<CloneEvent>,
    started: Instant,
}

/// Sentinel path used for the "Add Repository" synthetic tree entry.
const ADD_REPO_SENTINEL: &str = "__add_repo__";

const SPINNER_CHARS: &[char] = &[
    '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}', '\u{2827}', '\u{2807}', '\u{280f}',
];

const LOGO_LINES: &[&str] = &[
    "██████╗ ██╗     ██╗   ██╗███████╗████████╗",
    "██╔═══╝ ██║     ██║   ██║██╔════╝╚══██╔══╝",
    "██║     ██║     ██║   ██║███████╗   ██║   ",
    "██║     ██║     ██║   ██║╚════██║   ██║   ",
    "╚██████╗███████╗╚██████╔╝███████║   ██║   ",
    " ╚═════╝╚══════╝ ╚═════╝ ╚══════╝   ╚═╝   ",
];

const AGENT_FETCH_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Tree selection state
// ---------------------------------------------------------------------------

/// Which level of the repo tree the user is navigating.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TreeLevel {
    Repo,     // Level 0: selecting between repositories
    Category, // Level 1: selecting Local/Remote within a repo
    Branch,   // Level 2: selecting a branch within a category
}

/// Active tab in the top-level tab bar.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ActiveTab {
    Repositories,
    Overview,
    Schedule,
}

impl ActiveTab {
    fn next(self) -> Self {
        match self {
            Self::Repositories => Self::Overview,
            Self::Overview => Self::Schedule,
            Self::Schedule => Self::Repositories,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Repositories => Self::Schedule,
            Self::Overview => Self::Repositories,
            Self::Schedule => Self::Overview,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Repositories => "Repositories",
            Self::Overview => "Overview",
            Self::Schedule => "Schedule",
        }
    }
}

// ---------------------------------------------------------------------------
// Click map – populated during rendering, consumed during mouse handling
// ---------------------------------------------------------------------------

/// Identifies which tree item a display line corresponds to.
#[derive(Clone, Debug)]
enum TreeClickTarget {
    Repo(usize),
    Category(usize, usize),
    Branch(usize, usize, usize),
}

/// Accumulates clickable regions during rendering so the mouse handler can
/// map a click position to a UI action.
#[derive(Default)]
pub(crate) struct ClickMap {
    // Tab bar
    tabs: Vec<(Rect, ActiveTab)>,

    // Repositories tab
    tree_items: Vec<TreeClickTarget>,
    tree_inner_area: Rect,

    // Overview tab
    pub(crate) overview_panels: Vec<(Rect, usize)>, // (area, global_panel_idx)
    pub(crate) overview_repo_buttons: Vec<(Rect, String)>, // (area, repo_path) — collapse toggle
    pub(crate) overview_agent_indicators: Vec<(Rect, usize)>, // (area, global_panel_idx) — focus agent

    // Focus mode
    pub(crate) focus_left_area: Rect,
    pub(crate) focus_right_area: Rect,
    pub(crate) focus_left_tabs: Vec<(Rect, overview::LeftPanelTab)>,
    focus_back_button: Rect,
    // Multi-terminal label strip (inside the Terminal tab)
    pub(crate) focus_terminal_labels: Vec<(Rect, usize)>, // (rect, terminal_idx)
    pub(crate) focus_terminal_new_button: Rect,           // [+] hit zone
    pub(crate) focus_terminal_content_area: Rect,         // active terminal vterm area

    // Terminal content areas (inner area excluding borders/header) for URL click
    pub(crate) overview_content_areas: Vec<(Rect, usize)>, // (content_area, panel_idx)
    pub(crate) focus_right_content_area: Rect,

    // Context menu overlay
    menu_modal_rect: Rect,
    menu_inner_rect: Rect,
}

// ---------------------------------------------------------------------------
// Active menu overlay
// ---------------------------------------------------------------------------

/// Possible actions in a branch context menu.
#[derive(Clone, Copy)]
enum BranchAction {
    StartAgent,
    StartAgentInPlace,
    Pull,
    StopAgents,
    OpenAgent,
    /// Open the branch's worktree (or repo root for HEAD) in the user's editor.
    /// If the branch has no worktree yet, one is created first.
    OpenInEditor,
    RemoveWorktree,
    DeleteBranch,
    RemoteStartAgent,
    RemoteCreateWorktree,
    DeleteRemoteBranch,
    CheckoutRemote,
    BaseWorktreeOff,
    DetachHead,
    CheckoutLocal,
}

/// Action to execute after user confirms in a confirmation dialog.
enum ConfirmedAction {
    PurgeRepo {
        repo_path: String,
    },
    /// Stop tracking the repo in clust; the folder on disk is left untouched.
    RemoveRepository {
        repo_path: String,
    },
    /// Stop tracking the repo AND delete the folder from disk.
    DeleteRepository {
        repo_path: String,
    },
    StartAgentDetach {
        repo_path: String,
        branch_name: String,
    },
    /// Delete a single scheduled task.
    DeleteScheduledTask {
        task_id: String,
    },
    /// Bulk-delete every scheduled task with the given status.
    ClearScheduledTasksByStatus {
        status: clust_ipc::ScheduledTaskStatus,
    },
}

/// Tracks which context menu is currently open.
enum ActiveMenu {
    /// Pick an agent to open in focus mode (from a branch with multiple agents).
    AgentPicker {
        agents: Vec<AgentInfo>,
        menu: ContextMenu,
    },
    /// Repo-level context menu (e.g. "Change Color").
    RepoActions {
        repo_path: String,
        menu: ContextMenu,
    },
    /// Color picker sub-menu for a repo.
    ColorPicker {
        repo_path: String,
        menu: ContextMenu,
    },
    /// Branch-level context menu (local branches only).
    BranchActions {
        repo_path: String,
        branch_name: String,
        is_head: bool,
        agents: Vec<AgentInfo>,
        actions: Vec<BranchAction>,
        menu: ContextMenu,
    },
    /// Confirmation dialog for destructive actions.
    ConfirmAction {
        action: ConfirmedAction,
        menu: ContextMenu,
    },
    /// Worktree cleanup dialog shown after stopping agents in a worktree.
    WorktreeCleanup {
        repo_path: String,
        branch_name: String,
        menu: ContextMenu,
    },
    /// Editor picker shown when multiple editors are installed.
    EditorPicker {
        target_path: String,
        repo_path: Option<String>,
        editors: Vec<crate::editor::DetectedEditor>,
        menu: ContextMenu,
    },
    /// "Remember this editor?" confirmation after selecting an editor.
    EditorRemember {
        repo_path: Option<String>,
        editor: crate::editor::DetectedEditor,
        menu: ContextMenu,
    },
}

/// Pick the agents that belong to the repo currently selected on the left
/// panel. Returns a stable-ordered list of agent IDs (sorted by `started_at`
/// then `id` for determinism) along with the empty-state to render if the
/// list is empty.
fn scoped_agent_ids<'a>(
    agents: &[AgentInfo],
    display_repos: &'a [RepoInfo],
    selection: &TreeSelection,
) -> (Vec<String>, window_view::EmptyKind<'a>) {
    let repo = match display_repos.get(selection.repo_idx) {
        Some(r) => r,
        None => return (Vec::new(), window_view::EmptyKind::Logo),
    };

    if repo.path == ADD_REPO_SENTINEL {
        return (Vec::new(), window_view::EmptyKind::Logo);
    }

    let mut matched: Vec<&AgentInfo> = if repo.path.is_empty() {
        agents.iter().filter(|a| a.repo_path.is_none()).collect()
    } else {
        agents
            .iter()
            .filter(|a| a.repo_path.as_deref() == Some(repo.path.as_str()))
            .collect()
    };

    matched.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));

    if matched.is_empty() {
        let empty = if repo.path.is_empty() {
            window_view::EmptyKind::NoDetached
        } else {
            window_view::EmptyKind::NoAgentsFor(repo.name.as_str())
        };
        return (Vec::new(), empty);
    }

    (
        matched.into_iter().map(|a| a.id.clone()).collect(),
        window_view::EmptyKind::Logo,
    )
}

// ---------------------------------------------------------------------------
// Tree selection state (left panel)
// ---------------------------------------------------------------------------

/// Tracks the user's cursor position within the three-level repo tree.
struct TreeSelection {
    level: TreeLevel,
    repo_idx: usize,
    category_idx: usize, // 0 = Local, 1 = Remote
    branch_idx: usize,
    /// Collapsed state per repo index.
    repo_collapsed: HashMap<usize, bool>,
    /// Collapsed state per (repo_idx, category_idx).
    category_collapsed: HashMap<(usize, usize), bool>,
}

impl Default for TreeSelection {
    fn default() -> Self {
        Self {
            level: TreeLevel::Repo,
            repo_idx: 0,
            category_idx: 0,
            branch_idx: 0,
            repo_collapsed: HashMap::new(),
            category_collapsed: HashMap::new(),
        }
    }
}

impl TreeSelection {
    /// Returns true if the selected repo is the synthetic "No Repository" entry.
    fn is_unlinked_repo(&self, repos: &[RepoInfo]) -> bool {
        repos.get(self.repo_idx).is_some_and(|r| r.path.is_empty())
    }

    /// Returns valid category indices (0=local, 1=remote) for the selected repo.
    fn visible_categories(&self, repos: &[RepoInfo]) -> Vec<usize> {
        let Some(repo) = repos.get(self.repo_idx) else {
            return vec![];
        };
        let mut cats = Vec::new();
        if !repo.local_branches.is_empty() {
            cats.push(0);
        }
        if !repo.remote_branches.is_empty() {
            cats.push(1);
        }
        cats
    }

    /// Returns the number of branches in the currently selected category.
    fn branch_count(&self, repos: &[RepoInfo]) -> usize {
        let Some(repo) = repos.get(self.repo_idx) else {
            return 0;
        };
        match self.category_idx {
            0 => repo.local_branches.len(),
            1 => repo.remote_branches.len(),
            _ => 0,
        }
    }

    /// Adjust indices to stay within bounds after data refresh.
    fn clamp(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            *self = Self::default();
            return;
        }
        self.repo_idx = self.repo_idx.min(repos.len() - 1);

        // "No Repository" has no categories — snap Category level to Repo
        if self.is_unlinked_repo(repos) {
            if self.level == TreeLevel::Category {
                self.level = TreeLevel::Repo;
            }
            let bc = repos[self.repo_idx].local_branches.len();
            if bc == 0 && self.level == TreeLevel::Branch {
                self.level = TreeLevel::Repo;
            } else if bc > 0 && self.level == TreeLevel::Branch {
                self.branch_idx = self.branch_idx.min(bc - 1);
            }
            return;
        }

        let cats = self.visible_categories(repos);
        if cats.is_empty() {
            self.level = TreeLevel::Repo;
            return;
        }
        if self.level != TreeLevel::Repo && !cats.contains(&self.category_idx) {
            self.category_idx = cats[0];
            if self.level == TreeLevel::Branch {
                self.level = TreeLevel::Category;
            }
        }

        let bc = self.branch_count(repos);
        if bc == 0 && self.level == TreeLevel::Branch {
            self.level = TreeLevel::Category;
        } else if bc > 0 {
            self.branch_idx = self.branch_idx.min(bc - 1);
        }
    }

    /// Returns the branch count for a specific category index within the given repo.
    fn branch_count_for(&self, repos: &[RepoInfo], cat_idx: usize) -> usize {
        let Some(repo) = repos.get(self.repo_idx) else {
            return 0;
        };
        match cat_idx {
            0 => repo.local_branches.len(),
            1 => repo.remote_branches.len(),
            _ => 0,
        }
    }

    /// Move to the last visible descendant of a repo (for move_up into previous repo).
    fn go_to_last_visible_of_repo(&mut self, repos: &[RepoInfo]) {
        if self.is_unlinked_repo(repos) {
            let bc = repos
                .get(self.repo_idx)
                .map_or(0, |r| r.local_branches.len());
            if bc > 0 && !self.is_repo_collapsed(self.repo_idx) {
                self.level = TreeLevel::Branch;
                self.category_idx = 0;
                self.branch_idx = bc - 1;
            }
            return;
        }
        if self.is_repo_collapsed(self.repo_idx) {
            return; // stay at Repo level
        }
        let cats = self.visible_categories(repos);
        if cats.is_empty() {
            return;
        }
        // Pick the last visible category and land on its deepest visible item
        if let Some(&cat) = cats.last() {
            if !self.is_category_collapsed(self.repo_idx, cat) {
                let bc = self.branch_count_for(repos, cat);
                if bc > 0 {
                    self.level = TreeLevel::Branch;
                    self.category_idx = cat;
                    self.branch_idx = bc - 1;
                    return;
                }
            }
            // Category collapsed or empty — land on its header
            self.level = TreeLevel::Category;
            self.category_idx = cat;
        }
    }

    /// Flat tree navigation: move to previous visible item.
    fn move_up(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                if self.repo_idx > 0 {
                    self.repo_idx -= 1;
                    self.go_to_last_visible_of_repo(repos);
                }
            }
            TreeLevel::Category => {
                let cats = self.visible_categories(repos);
                if let Some(pos) = cats.iter().position(|&c| c == self.category_idx) {
                    if pos > 0 {
                        // Previous category: go to its last branch if expanded, else its header
                        let prev_cat = cats[pos - 1];
                        if !self.is_category_collapsed(self.repo_idx, prev_cat) {
                            let bc = self.branch_count_for(repos, prev_cat);
                            if bc > 0 {
                                self.level = TreeLevel::Branch;
                                self.category_idx = prev_cat;
                                self.branch_idx = bc - 1;
                                return;
                            }
                        }
                        self.category_idx = prev_cat;
                    } else {
                        // First category → go to repo header
                        self.level = TreeLevel::Repo;
                    }
                }
            }
            TreeLevel::Branch => {
                if self.branch_idx > 0 {
                    self.branch_idx -= 1;
                } else if self.is_unlinked_repo(repos) {
                    // "No Repository" has no categories — go to repo header
                    self.level = TreeLevel::Repo;
                } else {
                    // First branch → go to category header
                    self.level = TreeLevel::Category;
                }
            }
        }
    }

    /// Flat tree navigation: move to next visible item.
    fn move_down(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                if self.is_repo_collapsed(self.repo_idx) {
                    // Collapsed repo → next repo
                    if self.repo_idx + 1 < repos.len() {
                        self.repo_idx += 1;
                    }
                } else if self.is_unlinked_repo(repos) {
                    // "No Repository" — go to first branch or next repo
                    let bc = repos
                        .get(self.repo_idx)
                        .map_or(0, |r| r.local_branches.len());
                    if bc > 0 {
                        self.level = TreeLevel::Branch;
                        self.category_idx = 0;
                        self.branch_idx = 0;
                    } else if self.repo_idx + 1 < repos.len() {
                        self.repo_idx += 1;
                    }
                } else {
                    let cats = self.visible_categories(repos);
                    if !cats.is_empty() {
                        self.level = TreeLevel::Category;
                        self.category_idx = cats[0];
                        self.branch_idx = 0;
                    } else if self.repo_idx + 1 < repos.len() {
                        self.repo_idx += 1;
                    }
                }
            }
            TreeLevel::Category => {
                let cats = self.visible_categories(repos);
                let pos = cats.iter().position(|&c| c == self.category_idx);
                if !self.is_category_collapsed(self.repo_idx, self.category_idx)
                    && self.branch_count(repos) > 0
                {
                    // Expanded with branches → descend to first branch
                    self.level = TreeLevel::Branch;
                    self.branch_idx = 0;
                } else if let Some(p) = pos {
                    if p + 1 < cats.len() {
                        // Next category
                        self.category_idx = cats[p + 1];
                        self.branch_idx = 0;
                    } else if self.repo_idx + 1 < repos.len() {
                        // Last category → next repo
                        self.repo_idx += 1;
                        self.level = TreeLevel::Repo;
                    }
                }
            }
            TreeLevel::Branch => {
                let bc = self.branch_count(repos);
                if self.branch_idx + 1 < bc {
                    self.branch_idx += 1;
                } else if self.is_unlinked_repo(repos) {
                    // "No Repository" last branch → next repo
                    if self.repo_idx + 1 < repos.len() {
                        self.repo_idx += 1;
                        self.level = TreeLevel::Repo;
                    }
                } else {
                    // Last branch in category → next category or next repo
                    let cats = self.visible_categories(repos);
                    if let Some(pos) = cats.iter().position(|&c| c == self.category_idx) {
                        if pos + 1 < cats.len() {
                            self.category_idx = cats[pos + 1];
                            self.level = TreeLevel::Category;
                            self.branch_idx = 0;
                        } else if self.repo_idx + 1 < repos.len() {
                            self.repo_idx += 1;
                            self.level = TreeLevel::Repo;
                        }
                    }
                }
            }
        }
    }

    /// Jump to the previous repo header (Shift+Up).
    fn jump_prev_repo(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() || self.repo_idx == 0 {
            return;
        }
        self.repo_idx -= 1;
        self.level = TreeLevel::Repo;
    }

    /// Jump to the next repo header (Shift+Down).
    fn jump_next_repo(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() || self.repo_idx + 1 >= repos.len() {
            return;
        }
        self.repo_idx += 1;
        self.level = TreeLevel::Repo;
    }

    /// Right arrow: descend one level deeper, or expand if collapsed.
    fn descend(&mut self, repos: &[RepoInfo]) {
        if repos.is_empty() {
            return;
        }
        match self.level {
            TreeLevel::Repo => {
                // Only descend if expanded; collapsed repos require Space to expand
                if !self.is_repo_collapsed(self.repo_idx) {
                    if self.is_unlinked_repo(repos) {
                        // Skip category level for "No Repository"
                        let bc = repos
                            .get(self.repo_idx)
                            .map_or(0, |r| r.local_branches.len());
                        if bc > 0 {
                            self.level = TreeLevel::Branch;
                            self.category_idx = 0;
                            self.branch_idx = 0;
                        }
                    } else {
                        let cats = self.visible_categories(repos);
                        if !cats.is_empty() {
                            self.level = TreeLevel::Category;
                            self.category_idx = cats[0];
                            self.branch_idx = 0;
                        }
                    }
                }
            }
            TreeLevel::Category => {
                // Only descend if expanded; collapsed categories require Space to expand
                if !self.is_category_collapsed(self.repo_idx, self.category_idx)
                    && self.branch_count(repos) > 0
                {
                    self.level = TreeLevel::Branch;
                    self.branch_idx = 0;
                }
            }
            TreeLevel::Branch => {} // already deepest
        }
    }

    /// Left arrow: navigate up one level (never collapses — use Space to toggle).
    fn ascend(&mut self, repos: &[RepoInfo]) {
        match self.level {
            TreeLevel::Repo => {} // already at top
            TreeLevel::Category => self.level = TreeLevel::Repo,
            TreeLevel::Branch => {
                if self.is_unlinked_repo(repos) {
                    // Skip category level for "No Repository"
                    self.level = TreeLevel::Repo;
                } else {
                    self.level = TreeLevel::Category;
                }
            }
        }
    }

    /// Toggle collapse state at the current level.
    fn toggle_collapse(&mut self) {
        match self.level {
            TreeLevel::Repo => {
                let entry = self.repo_collapsed.entry(self.repo_idx).or_insert(false);
                *entry = !*entry;
            }
            TreeLevel::Category => {
                let key = (self.repo_idx, self.category_idx);
                let entry = self.category_collapsed.entry(key).or_insert(false);
                *entry = !*entry;
            }
            TreeLevel::Branch => {} // leaf nodes, no collapse
        }
    }

    fn is_repo_collapsed(&self, repo_idx: usize) -> bool {
        *self.repo_collapsed.get(&repo_idx).unwrap_or(&false)
    }

    fn is_category_collapsed(&self, repo_idx: usize, cat_idx: usize) -> bool {
        // Remote branches (cat_idx 1) are collapsed by default
        let default = cat_idx == 1;
        *self
            .category_collapsed
            .get(&(repo_idx, cat_idx))
            .unwrap_or(&default)
    }
}

pub fn run(hub_name: &str) -> io::Result<()> {
    io::stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;
    io::stdout().execute(EnableMouseCapture)?;
    io::stdout().execute(EnableBracketedPaste)?;
    io::stdout().execute(EnableFocusChange)?;

    // Enable Kitty keyboard protocol so crossterm reports SUPER (Cmd) modifier
    // on mouse events. Gracefully degrades: terminals that don't support it
    // will simply not report the modifier.
    let kbd_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    if kbd_enhanced {
        let _ = io::stdout().execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        ));
    }

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if kbd_enhanced {
            let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
        }
        let _ = io::stdout().execute(DisableFocusChange);
        let _ = io::stdout().execute(DisableBracketedPaste);
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        hook(info);
    }));

    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let hub_running = block_on_async(async { ipc::connect_to_hub().await.is_ok() });

    let update_notice: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let notice_clone = update_notice.clone();
    std::thread::spawn(move || {
        if let Some(msg) = version::check_update() {
            *notice_clone.lock().unwrap() = Some(msg);
        }
    });

    let mut agents: Vec<AgentInfo> = Vec::new();
    let mut repos: Vec<RepoInfo> = Vec::new();
    let mut selection = TreeSelection::default();
    let mut active_tab = ActiveTab::Repositories;
    let mut last_agent_fetch = Instant::now() - Duration::from_secs(10);
    let mut last_repo_fetch = Instant::now() - Duration::from_secs(10);

    let mut active_menu: Option<ActiveMenu> = None;
    let mut hub_stopped = false;
    let mut hub_count: usize = 1;
    let mut worktree_cleanups: Vec<crate::worktree::WorktreeCleanup> = vec![];
    let mut pending_worktree_cleanups: Vec<crate::worktree::WorktreeCleanup> = vec![];
    let mut show_help = false;
    let mut overview_state = OverviewState::new();
    let mut focus_mode_state = overview::FocusModeState::new();
    let mut in_focus_mode = false;
    let mut status_message: Option<StatusMessage> = None;
    let mut mouse_captured = true;
    let mut bypass_permissions = fetch_bypass_permissions();
    let mut mouse_passthrough_until: Option<Instant> = None;
    let mut last_esc_press: Option<Instant> = None;
    let (init_cols, init_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut last_content_area = Rect {
        x: 0,
        y: 1, // tab bar
        width: init_cols,
        height: init_rows.saturating_sub(2), // tab bar + status bar
    };

    // Create-agent modal state
    let mut create_modal: Option<CreateAgentModal> = None;
    // Search-agent modal state
    let mut search_modal: Option<SearchModal> = None;
    // Agent ID to select in overview after next sync
    let mut pending_overview_select: Option<String> = None;
    // Detached (directory) agent modal state
    let mut detached_modal: Option<DetachedAgentModal> = None;
    // Purge progress modal state
    let mut purge_progress: Option<PurgeProgress> = None;
    // Repository create/clone modal state
    let mut repo_modal: Option<RepoModal> = None;
    // Schedule task modal state (Opt+S)
    let mut schedule_modal: Option<ScheduleTaskModal> = None;
    // Edit-prompt modal state (e on Inactive/Aborted task)
    let mut edit_prompt_modal: Option<EditPromptModal> = None;
    // Clone progress modal state
    let mut clone_progress: Option<CloneProgress> = None;
    // Cached list of installed editors (detected once at startup)
    let editors_cache = crate::editor::detect_installed_editors();
    let (agent_start_tx, mut agent_start_rx) = tokio::sync::mpsc::channel::<AgentStartResult>(16);
    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel::<StatusMessage>(4);

    // Schedule tab state
    let mut schedule_state = ScheduleState::new();
    let mut scheduled_tasks: Vec<clust_ipc::ScheduledTaskInfo> = Vec::new();
    let mut last_scheduled_fetch = Instant::now() - Duration::from_secs(10);
    let (scheduled_task_tx, mut scheduled_task_rx) =
        tokio::sync::mpsc::channel::<Vec<clust_ipc::ScheduledTaskInfo>>(8);

    loop {
        // Drain output events (non-blocking, runs regardless of tab)
        overview_state.drain_output_events();
        overview_state.drain_cached_terminal_events();
        focus_mode_state.drain_output_events();
        focus_mode_state.drain_diff_events();
        focus_mode_state.drain_compare_diff_events();
        focus_mode_state.drain_pr_events();
        focus_mode_state.drain_terminal_events();

        // Re-enable mouse capture after passthrough timer expires
        if let Some(deadline) = mouse_passthrough_until {
            if Instant::now() >= deadline {
                mouse_passthrough_until = None;
                mouse_captured = true;
                io::stdout().execute(EnableMouseCapture)?;
            }
        }

        // Immediate worktree cleanup prompt when agent exits in focus mode
        if in_focus_mode && active_menu.is_none() {
            if let Some(panel) = focus_mode_state.panel.as_mut() {
                if panel.exited && panel.is_worktree && !panel.worktree_cleanup_shown {
                    panel.worktree_cleanup_shown = true;
                    if let (Some(rp), Some(bn)) = (&panel.repo_path, &panel.branch_name) {
                        // Append rather than overwrite — other pending
                        // cleanups (e.g. from earlier batch removals) must
                        // still be surfaced after this one is dismissed.
                        pending_worktree_cleanups.extend(std::iter::once(
                            crate::worktree::WorktreeCleanup {
                                repo_path: rp.clone(),
                                branch_name: bn.clone(),
                            },
                        ));
                        active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
                    }
                }
            }
        }

        // Immediate worktree cleanup prompt when agent exits in overview mode
        if !in_focus_mode && active_tab == ActiveTab::Overview && active_menu.is_none() {
            for panel in overview_state.panels.iter_mut() {
                if panel.exited && panel.is_worktree && !panel.worktree_cleanup_shown {
                    panel.worktree_cleanup_shown = true;
                    if let (Some(rp), Some(bn)) = (&panel.repo_path, &panel.branch_name) {
                        pending_worktree_cleanups.push(crate::worktree::WorktreeCleanup {
                            repo_path: rp.clone(),
                            branch_name: bn.clone(),
                        });
                    }
                }
            }
            if !pending_worktree_cleanups.is_empty() {
                active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
            }
        }

        // Check for completed agent start requests (drain all pending). The
        // channel must be drained even when a modal is open so it doesn't grow
        // unbounded — but destructive state transitions (entering focus mode,
        // switching tabs) are skipped while a modal is up to avoid corrupting
        // modal state. The status message is still set so the user gets
        // feedback that the agent started.
        let any_modal_open = create_modal.is_some()
            || search_modal.is_some()
            || detached_modal.is_some()
            || repo_modal.is_some()
            || schedule_modal.is_some()
            || edit_prompt_modal.is_some();
        while let Ok(result) = agent_start_rx.try_recv() {
            match result {
                AgentStartResult::Started {
                    agent_id,
                    agent_binary,
                    working_dir,
                    repo_path,
                    branch_name,
                    is_worktree,
                } => {
                    if active_tab == ActiveTab::Overview {
                        // Stay in overview mode; select the agent after next sync
                        pending_overview_select = Some(agent_id.clone());
                    } else if !any_modal_open {
                        let fm_cols = (last_content_area.width * 40 / 100)
                            .saturating_sub(2)
                            .max(1);
                        let fm_rows = last_content_area.height.saturating_sub(3).max(1);
                        let existing_terminals = overview_state.take_agent_terminals(&agent_id);
                        focus_mode_state.open_agent(
                            &agent_id,
                            &agent_binary,
                            fm_cols,
                            fm_rows,
                            &working_dir,
                            repo_path.as_deref(),
                            branch_name.as_deref(),
                            is_worktree,
                            existing_terminals,
                        );
                        in_focus_mode = true;
                    } else {
                        // Modal is open — surface the agent in overview on the
                        // next render instead of entering focus mode mid-modal.
                        pending_overview_select = Some(agent_id.clone());
                    }
                    let label = branch_name.as_deref().unwrap_or(&working_dir);
                    status_message = Some(StatusMessage {
                        text: format!("Agent started in {label}"),
                        level: StatusLevel::Success,
                        created: Instant::now(),
                    });
                }
                AgentStartResult::Failed(msg) => {
                    status_message = Some(StatusMessage {
                        text: msg,
                        level: StatusLevel::Error,
                        created: Instant::now(),
                    });
                }
            }
        }

        // Check for async status messages (e.g. pull results)
        if let Ok(msg) = status_rx.try_recv() {
            status_message = Some(msg);
        }

        // Auto-dismiss status messages after 5 seconds
        if let Some(ref msg) = status_message {
            if msg.created.elapsed() >= Duration::from_secs(5) {
                status_message = None;
            }
        }

        // Drain purge progress events
        if let Some(ref mut pp) = purge_progress {
            while let Ok(event) = pp.rx.try_recv() {
                match event {
                    PurgeEvent::Step(step) => pp.steps.push(step),
                    PurgeEvent::Done => pp.done = true,
                    PurgeEvent::Error(msg) => {
                        pp.error = Some(msg);
                        pp.done = true;
                    }
                }
            }
        }

        // Drain clone progress events
        if let Some(ref mut cp) = clone_progress {
            while let Ok(event) = cp.rx.try_recv() {
                match event {
                    CloneEvent::Step(step) => cp.steps.push(step),
                    CloneEvent::Done => cp.done = true,
                    CloneEvent::Error(msg) => {
                        cp.error = Some(msg);
                        cp.done = true;
                    }
                }
            }
        }

        // Periodically fetch agent list and repo state from hub
        let mut agents_refreshed = false;
        if hub_running && last_agent_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            agents = fetch_agents();
            last_agent_fetch = Instant::now();
            agents_refreshed = true;
        }
        if hub_running && last_repo_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            repos = fetch_repos();
            last_repo_fetch = Instant::now();
            if in_focus_mode {
                focus_mode_state.update_compare_branches(&repos);
            }
        }
        // Periodically refresh the scheduled-task list. Spawned async so the
        // socket round-trip never blocks the event loop. The receiver is
        // drained at the top of every tick.
        if hub_running && last_scheduled_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            last_scheduled_fetch = Instant::now();
            let tx = scheduled_task_tx.clone();
            tokio::spawn(async move {
                let tasks = ipc::fetch_scheduled_tasks().await;
                let _ = tx.send(tasks).await;
            });
        }
        while let Ok(tasks) = scheduled_task_rx.try_recv() {
            scheduled_tasks = tasks.clone();
            schedule_state.sync_tasks(tasks, &repos);
        }
        // Drain pending PTY output for active scheduled tasks.
        schedule_state.drain_output_events();

        // Build display_repos: real repos + synthetic "No Repository" for unlinked agents
        let display_repos = {
            let mut dr = repos.clone();
            let unlinked: Vec<&AgentInfo> =
                agents.iter().filter(|a| a.repo_path.is_none()).collect();
            if !unlinked.is_empty() {
                dr.push(RepoInfo {
                    path: String::new(),
                    name: "No Repository".to_string(),
                    color: None,
                    editor: None,
                    local_branches: unlinked
                        .iter()
                        .map(|a| {
                            let dir_name = std::path::Path::new(&a.working_dir)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| a.working_dir.clone());
                            clust_ipc::BranchInfo {
                                name: format!("{} — {}", a.agent_binary, dir_name),
                                is_head: false,
                                active_agent_count: 1,
                                is_worktree: false,
                                is_remote: false,
                            }
                        })
                        .collect(),
                    remote_branches: vec![],
                });
            }
            // Always append the "Add Repository" action entry
            dr.push(RepoInfo {
                path: ADD_REPO_SENTINEL.to_string(),
                name: "Add Repository".to_string(),
                color: None,
                editor: None,
                local_branches: vec![],
                remote_branches: vec![],
            });
            dr
        };
        // Detect whether the previously-selected entry was a real repo (not
        // synthetic). Synthetic entries are "No Repository" (empty path) and
        // the "Add Repository" sentinel. We use this to nudge selection back
        // onto a real repo if `clamp` lands it on a synthetic one (e.g. after
        // deleting the last real repo at the tail of the list).
        let prev_was_real_repo = display_repos
            .get(selection.repo_idx)
            .is_some_and(|r| !r.path.is_empty() && r.path != ADD_REPO_SENTINEL);
        selection.clamp(&display_repos);
        if prev_was_real_repo {
            let landed_on_synthetic = display_repos
                .get(selection.repo_idx)
                .is_some_and(|r| r.path.is_empty() || r.path == ADD_REPO_SENTINEL);
            if landed_on_synthetic {
                // Find the closest real repo: scan backwards first, then
                // forwards. If none exist, leave the clamp result as-is.
                let real_idx = (0..selection.repo_idx)
                    .rev()
                    .find(|&i| {
                        display_repos
                            .get(i)
                            .is_some_and(|r| !r.path.is_empty() && r.path != ADD_REPO_SENTINEL)
                    })
                    .or_else(|| {
                        (selection.repo_idx + 1..display_repos.len()).find(|&i| {
                            display_repos
                                .get(i)
                                .is_some_and(|r| !r.path.is_empty() && r.path != ADD_REPO_SENTINEL)
                        })
                    });
                if let Some(idx) = real_idx {
                    selection.repo_idx = idx;
                    selection.clamp(&display_repos);
                }
            }
        }

        // Sync overview agent connections when agents are refreshed. The
        // panel set (lifecycle) is synced regardless of tab so the
        // Repositories-tab Window view can borrow live PTY panels by id.
        // The overview-grid resize is still gated to the Overview tab.
        if agents_refreshed {
            overview_state.sync_agent_set(&agents);
            if active_tab == ActiveTab::Overview {
                overview_state.resize_panels_to(last_content_area);
                if let Some(id) = pending_overview_select.take() {
                    overview_state.select_agent_by_id(&id);
                }
            }
        }

        let hub_status = hub_running;
        let notice = update_notice.lock().unwrap().clone();
        let cur_tab = active_tab;
        let cur_focus_mode = in_focus_mode;
        let overview_focus = overview_state.focus;
        let status_msg_ref = status_message.as_ref();
        let show_help_now = show_help;
        let mouse_captured_now = mouse_captured;
        let mouse_passthrough_now = mouse_passthrough_until.is_some();
        let menu_ref = &active_menu;
        let repo_colors: HashMap<String, String> = repos
            .iter()
            .filter_map(|r| r.color.as_ref().map(|c| (r.path.clone(), c.clone())))
            .collect();

        let mut click_map = ClickMap::default();
        let show_modal = create_modal.is_some()
            || detached_modal.is_some()
            || repo_modal.is_some()
            || schedule_modal.is_some()
            || edit_prompt_modal.is_some();
        let show_search = search_modal.is_some();
        let purge_ref = &purge_progress;
        let clone_ref = &clone_progress;

        terminal.draw(|frame| {
            let area = frame.area();

            // Top-level: header (1 row) + content area + status bar (1 row)
            let [header_area, content_area, status_area] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);

            last_content_area = content_area;

            if cur_focus_mode {
                render_focus_back_bar(
                    frame,
                    header_area,
                    &focus_mode_state,
                    cur_tab,
                    &mut click_map,
                    &repo_colors,
                );
                overview::render_focus_mode(
                    frame,
                    content_area,
                    &mut focus_mode_state,
                    &mut click_map,
                    &repo_colors,
                );
            } else {
                render_tab_bar(frame, header_area, cur_tab, &mut click_map);

                match cur_tab {
                    ActiveTab::Repositories => {
                        // Content: left (25%) + divider (1 col) + right (75%)
                        let [left_area, divider_area, right_area] = Layout::horizontal([
                            Constraint::Percentage(25),
                            Constraint::Length(1),
                            Constraint::Percentage(75),
                        ])
                        .areas(content_area);

                        render_left_panel(
                            frame,
                            left_area,
                            &display_repos,
                            &selection,
                            true,
                            &mut click_map,
                        );
                        render_divider(frame, divider_area);
                        let (scoped_ids, empty) =
                            scoped_agent_ids(&agents, &display_repos, &selection);
                        window_view::render(
                            frame,
                            right_area,
                            &mut overview_state,
                            &scoped_ids,
                            empty,
                            &repo_colors,
                        );
                    }
                    ActiveTab::Overview => {
                        overview::render_overview(
                            frame,
                            content_area,
                            &mut overview_state,
                            &mut click_map,
                            &repo_colors,
                            &repos,
                        );
                    }
                    ActiveTab::Schedule => {
                        schedule_state.render(frame, content_area, &repos, &repo_colors);
                    }
                }
            }

            // Resolve focused agent info for status bar
            let focused_agent_info: Option<(String, ratatui::style::Color, String)> =
                if cur_focus_mode {
                    state_to_agent_info(&focus_mode_state, &repo_colors)
                } else if let OverviewFocus::Terminal(idx) = overview_focus {
                    overview_state.panels.get(idx).and_then(|panel| {
                        let rp = panel.repo_path.as_ref()?;
                        let repo_display = std::path::Path::new(rp)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| rp.clone());
                        let repo_clr = repo_colors
                            .get(rp.as_str())
                            .map(|c| theme::repo_color(c))
                            .unwrap_or(theme::R_ACCENT);
                        let branch = panel.branch_name.clone().unwrap_or_default();
                        Some((repo_display, repo_clr, branch))
                    })
                } else {
                    None
                };

            let focus_hint = if cur_focus_mode {
                if focus_mode_state.left_tab == overview::LeftPanelTab::Terminal
                    && focus_mode_state.focus_side == overview::FocusSide::Left
                {
                    if focus_mode_state.terminal_input_focused {
                        FocusModeHint::TerminalType
                    } else {
                        FocusModeHint::TerminalNavigate
                    }
                } else {
                    FocusModeHint::Other
                }
            } else {
                FocusModeHint::Other
            };
            render_status_bar(
                frame,
                status_area,
                hub_status,
                &notice,
                hub_name,
                cur_tab,
                cur_focus_mode,
                overview_focus,
                focused_agent_info.as_ref(),
                status_msg_ref,
                mouse_captured_now,
                mouse_passthrough_now,
                bypass_permissions,
                focus_hint,
            );

            if let Some(ref menu_state) = menu_ref {
                let menu = match menu_state {
                    ActiveMenu::AgentPicker { ref menu, .. } => menu,
                    ActiveMenu::RepoActions { ref menu, .. } => menu,
                    ActiveMenu::ColorPicker { ref menu, .. } => menu,
                    ActiveMenu::BranchActions { ref menu, .. } => menu,
                    ActiveMenu::ConfirmAction { ref menu, .. } => menu,
                    ActiveMenu::WorktreeCleanup { ref menu, .. } => menu,
                    ActiveMenu::EditorPicker { ref menu, .. } => menu,
                    ActiveMenu::EditorRemember { ref menu, .. } => menu,
                };
                let (modal_rect, inner_rect) = menu.render(frame, content_area);
                click_map.menu_modal_rect = modal_rect;
                click_map.menu_inner_rect = inner_rect;
            }

            if show_help_now {
                render_help_overlay(frame, content_area, cur_tab, cur_focus_mode);
            }

            if show_modal {
                if let Some(ref modal) = create_modal {
                    modal.render(frame, content_area);
                }
                if let Some(ref modal) = detached_modal {
                    modal.render(frame, content_area);
                }
                if let Some(ref modal) = repo_modal {
                    modal.render(frame, content_area);
                }
                if let Some(ref modal) = schedule_modal {
                    modal.render(frame, content_area);
                }
                if let Some(ref modal) = edit_prompt_modal {
                    modal.render(frame, content_area);
                }
            }

            if show_search {
                if let Some(ref modal) = search_modal {
                    modal.render(frame, content_area);
                }
            }

            if let Some(ref pp) = *purge_ref {
                render_purge_progress(frame, content_area, pp);
            }
            if let Some(ref cp) = *clone_ref {
                render_clone_progress(frame, content_area, cp);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // F2: toggle mouse capture (global, never forwarded)
                    if key.code == KeyCode::F(2) {
                        mouse_captured = !mouse_captured;
                        mouse_passthrough_until = None;
                        if mouse_captured {
                            io::stdout().execute(EnableMouseCapture)?;
                        } else {
                            io::stdout().execute(DisableMouseCapture)?;
                        }
                        continue;
                    }

                    // Alt+M: temporarily disable mouse capture (re-enables after 5s)
                    if key.code == KeyCode::Char('m') && key.modifiers.contains(KeyModifiers::ALT) {
                        mouse_captured = false;
                        io::stdout().execute(DisableMouseCapture)?;
                        mouse_passthrough_until = Some(Instant::now() + Duration::from_secs(5));
                        continue;
                    }

                    // Purge progress modal: block all input, Esc dismisses when done
                    if purge_progress.is_some() {
                        if key.code == KeyCode::Esc
                            && purge_progress.as_ref().is_some_and(|pp| pp.done)
                        {
                            purge_progress = None;
                            last_repo_fetch = Instant::now() - Duration::from_secs(10);
                            last_agent_fetch = Instant::now() - Duration::from_secs(10);
                        }
                    // Cmd+1/Cmd+2: instant view switching (before menus/terminals)
                    } else if key.modifiers.contains(KeyModifiers::SUPER) {
                        match key.code {
                            KeyCode::Char('1') => {
                                active_menu = None;
                                if in_focus_mode {
                                    if let Some((aid, cache)) = focus_mode_state.detach() {
                                        overview_state.store_agent_terminals(aid, cache);
                                    } else {
                                        // detach() returned None: either no panel was attached
                                        // or there were no terminal panels to cache. Any
                                        // previously-stored cache for this agent in
                                        // overview_state is preserved as-is.
                                    }
                                    in_focus_mode = false;
                                }
                                active_tab = ActiveTab::Repositories;
                            }
                            KeyCode::Char('2') => {
                                active_menu = None;
                                if in_focus_mode {
                                    if let Some((aid, cache)) = focus_mode_state.detach() {
                                        overview_state.store_agent_terminals(aid, cache);
                                    } else {
                                        // See note above: preserve any existing cache.
                                    }
                                    in_focus_mode = false;
                                }
                                active_tab = ActiveTab::Overview;
                                if !overview_state.initialized {
                                    overview_state.sync_agents(&agents, last_content_area);
                                } else {
                                    overview_state.force_resize_all();
                                }
                            }
                            KeyCode::Char('3') => {
                                active_menu = None;
                                if in_focus_mode {
                                    if let Some((aid, cache)) = focus_mode_state.detach() {
                                        overview_state.store_agent_terminals(aid, cache);
                                    }
                                    in_focus_mode = false;
                                }
                                active_tab = ActiveTab::Schedule;
                                last_scheduled_fetch = Instant::now() - Duration::from_secs(10);
                            }
                            _ => {}
                        }
                    // Clone progress modal: block all input, Esc dismisses when done
                    } else if clone_progress.is_some() {
                        if key.code == KeyCode::Esc
                            && clone_progress.as_ref().is_some_and(|cp| cp.done)
                        {
                            clone_progress = None;
                            last_repo_fetch = Instant::now() - Duration::from_secs(10);
                            last_agent_fetch = Instant::now() - Duration::from_secs(10);
                        }
                    // Context menu overlay: intercept all keys when active
                    } else if let Some(ref mut menu_state) = active_menu {
                        let result = match menu_state {
                            ActiveMenu::AgentPicker { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::RepoActions { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::ColorPicker { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::BranchActions { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::ConfirmAction { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::WorktreeCleanup { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::EditorPicker { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                            ActiveMenu::EditorRemember { ref mut menu, .. } => {
                                menu.handle_key(key.code)
                            }
                        };
                        match result {
                            MenuResult::Selected(idx) => {
                                // Take ownership of the menu state to process the action
                                let taken = active_menu.take().unwrap();
                                match taken {
                                    ActiveMenu::AgentPicker {
                                        agents: picker_agents,
                                        ..
                                    } => {
                                        if let Some(agent) = picker_agents.get(idx) {
                                            let agent_id = agent.id.clone();
                                            let agent_binary = agent.agent_binary.clone();
                                            let working_dir = agent.working_dir.clone();
                                            let fm_cols = (last_content_area.width * 40 / 100)
                                                .saturating_sub(2)
                                                .max(1);
                                            let fm_rows =
                                                last_content_area.height.saturating_sub(3).max(1);
                                            let existing_terminals =
                                                overview_state.take_agent_terminals(&agent_id);
                                            focus_mode_state.open_agent(
                                                &agent_id,
                                                &agent_binary,
                                                fm_cols,
                                                fm_rows,
                                                &working_dir,
                                                agent.repo_path.as_deref(),
                                                agent.branch_name.as_deref(),
                                                agent.is_worktree,
                                                existing_terminals,
                                            );
                                            in_focus_mode = true;
                                        }
                                    }
                                    ActiveMenu::RepoActions { repo_path, .. } => {
                                        match idx {
                                            0 => {
                                                // "Change Color" → open color picker
                                                let items: Vec<ContextMenuItem> =
                                                    theme::REPO_COLOR_NAMES
                                                        .iter()
                                                        .map(|&name| ContextMenuItem {
                                                            label: name[0..1].to_uppercase()
                                                                + &name[1..],
                                                            color: Some(theme::repo_color(name)),
                                                        })
                                                        .collect();
                                                active_menu = Some(ActiveMenu::ColorPicker {
                                                    repo_path,
                                                    menu: ContextMenu::with_colors(
                                                        "Choose Color",
                                                        items,
                                                    ),
                                                });
                                            }
                                            1 => {
                                                // "Open in File System"
                                                open_in_file_system(&repo_path);
                                            }
                                            2 => {
                                                // "Open in Terminal"
                                                open_in_terminal(&repo_path);
                                            }
                                            3 => {
                                                // "Stop All Agents"
                                                // Collect worktree agents for this repo before stopping
                                                let repo_agents: Vec<_> = agents
                                                    .iter()
                                                    .filter(|a| {
                                                        a.repo_path.as_deref() == Some(&*repo_path)
                                                    })
                                                    .cloned()
                                                    .collect();
                                                stop_repo_agents_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                let cleanups =
                                                    crate::worktree::collect_worktree_cleanups(
                                                        &repo_agents,
                                                        &agents,
                                                    );
                                                if !cleanups.is_empty() {
                                                    pending_worktree_cleanups = cleanups;
                                                    active_menu = pop_worktree_cleanup_menu(
                                                        &mut pending_worktree_cleanups,
                                                    );
                                                }
                                            }
                                            4 => {
                                                // "Clean Stale Refs"
                                                clean_stale_refs_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            5 => {
                                                // "Detach"
                                                let tx = status_tx.clone();
                                                let rp = repo_path.clone();
                                                tokio::spawn(async move {
                                                    let mut stream = match ipc::try_connect().await
                                                    {
                                                        Ok(s) => s,
                                                        Err(e) => {
                                                            let _ = tx.send(StatusMessage {
                                                                    text: format!("Detach failed: hub connect error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            return;
                                                        }
                                                    };
                                                    let msg =
                                                        CliMessage::DetachHead { repo_path: rp };
                                                    if let Err(e) =
                                                        clust_ipc::send_message(&mut stream, &msg)
                                                            .await
                                                    {
                                                        let _ = tx.send(StatusMessage {
                                                            text: format!("Detach failed: send error: {e}"),
                                                            level: StatusLevel::Error,
                                                            created: Instant::now(),
                                                        }).await;
                                                        return;
                                                    }
                                                    match clust_ipc::recv_message::<HubMessage>(
                                                        &mut stream,
                                                    )
                                                    .await
                                                    {
                                                        Ok(HubMessage::HeadDetached) => {
                                                            let _ = tx
                                                                .send(StatusMessage {
                                                                    text: "HEAD detached"
                                                                        .to_string(),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                })
                                                                .await;
                                                        }
                                                        Ok(HubMessage::Error { message }) => {
                                                            let _ = tx
                                                                .send(StatusMessage {
                                                                    text: format!(
                                                                        "Detach failed: {message}"
                                                                    ),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                })
                                                                .await;
                                                        }
                                                        Ok(_) => {
                                                            let _ = tx.send(StatusMessage {
                                                                text: "Detach failed: unexpected hub response".to_string(),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                        }
                                                        Err(e) => {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Detach failed: recv error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                        }
                                                    }
                                                });
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            6 => {
                                                // "Purge" → open confirmation dialog
                                                active_menu = Some(ActiveMenu::ConfirmAction {
                                                    action: ConfirmedAction::PurgeRepo {
                                                        repo_path,
                                                    },
                                                    menu: ContextMenu::new(
                                                        "Purge Repository",
                                                        vec![
                                                            "Confirm".to_string(),
                                                            "Cancel".to_string(),
                                                        ],
                                                    )
                                                    .with_description(
                                                        "This will stop all agents, delete all\nworktrees, and delete all local branches.".to_string(),
                                                    ),
                                                });
                                            }
                                            7 => {
                                                // "Remove Repository" → open confirmation dialog
                                                active_menu = Some(ActiveMenu::ConfirmAction {
                                                    action: ConfirmedAction::RemoveRepository {
                                                        repo_path,
                                                    },
                                                    menu: ContextMenu::new(
                                                        "Remove Repository",
                                                        vec![
                                                            "Confirm".to_string(),
                                                            "Cancel".to_string(),
                                                        ],
                                                    )
                                                    .with_description(
                                                        "Stop tracking this repository in clust.\nThe folder on disk is left untouched.".to_string(),
                                                    ),
                                                });
                                            }
                                            8 => {
                                                // "Delete Repository" → open confirmation dialog
                                                active_menu = Some(ActiveMenu::ConfirmAction {
                                                    action: ConfirmedAction::DeleteRepository {
                                                        repo_path,
                                                    },
                                                    menu: ContextMenu::new(
                                                        "Delete Repository",
                                                        vec![
                                                            "Confirm".to_string(),
                                                            "Cancel".to_string(),
                                                        ],
                                                    )
                                                    .with_description(
                                                        "Stop tracking this repository AND permanently\ndelete the folder from disk. This cannot be undone.".to_string(),
                                                    ),
                                                });
                                            }
                                            _ => {}
                                        }
                                    }
                                    ActiveMenu::ColorPicker { repo_path, .. } => {
                                        if let Some(&color_name) = theme::REPO_COLOR_NAMES.get(idx)
                                        {
                                            set_repo_color_ipc(&repo_path, color_name);
                                            // Force repo refresh
                                            last_repo_fetch =
                                                Instant::now() - Duration::from_secs(10);
                                        }
                                    }
                                    ActiveMenu::BranchActions {
                                        repo_path,
                                        branch_name,
                                        is_head,
                                        agents: branch_agents,
                                        actions,
                                        ..
                                    } => {
                                        if let Some(&action) = actions.get(idx) {
                                            match action {
                                                BranchAction::StartAgent if is_head => {
                                                    active_menu = Some(ActiveMenu::ConfirmAction {
                                                        action: ConfirmedAction::StartAgentDetach {
                                                            repo_path,
                                                            branch_name,
                                                        },
                                                        menu: ContextMenu::new(
                                                            "Detach HEAD",
                                                            vec![
                                                                "Confirm".to_string(),
                                                                "Cancel".to_string(),
                                                            ],
                                                        )
                                                        .with_description(
                                                            "This will detach HEAD in your repo.\nThe branch will be moved to a worktree for the agent.".to_string(),
                                                        ),
                                                    });
                                                    continue;
                                                }
                                                BranchAction::StartAgent => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    let bp = bypass_permissions;
                                                    let (cols, rows) = crossterm::terminal::size()
                                                        .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: hub connect error: {e}")
                                                                    )).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::CreateWorktreeAgent {
                                                            repo_path: rp,
                                                            target_branch: Some(bn),
                                                            new_branch: None,
                                                            prompt: None,
                                                            agent_binary: None,
                                                            cols,
                                                            rows: rows.saturating_sub(2).max(1),
                                                            accept_edits: false,
                                                            plan_mode: bp,
                                                            allow_bypass: bp,
                                                            hub,
                                                            auto_exit: false,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(AgentStartResult::Failed(
                                                                format!("Agent create failed: send error: {e}")
                                                            )).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(
                                                                HubMessage::WorktreeAgentStarted {
                                                                    id,
                                                                    agent_binary,
                                                                    working_dir,
                                                                    repo_path,
                                                                    branch_name,
                                                                },
                                                            ) => {
                                                                let _ = tx
                                                                    .send(
                                                                        AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree: true,
                                                                        },
                                                                    )
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: {message}")
                                                                )).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    "Agent create failed: unexpected hub response".to_string()
                                                                )).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: recv error: {e}")
                                                                )).await;
                                                            }
                                                        }
                                                    });
                                                }
                                                BranchAction::StartAgentInPlace => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let rp = repo_path.clone();
                                                    let bp = bypass_permissions;
                                                    let (cols, rows) = crossterm::terminal::size()
                                                        .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: hub connect error: {e}")
                                                                    )).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::StartAgent {
                                                            prompt: None,
                                                            agent_binary: None,
                                                            working_dir: rp,
                                                            cols,
                                                            rows: rows.saturating_sub(2).max(1),
                                                            accept_edits: false,
                                                            plan_mode: bp,
                                                            allow_bypass: bp,
                                                            hub,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(AgentStartResult::Failed(
                                                                format!("Agent create failed: send error: {e}")
                                                            )).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::AgentStarted {
                                                                id,
                                                                agent_binary,
                                                                is_worktree,
                                                                repo_path,
                                                                branch_name,
                                                            }) => {
                                                                let working_dir = repo_path
                                                                    .clone()
                                                                    .unwrap_or_default();
                                                                let _ = tx
                                                                    .send(
                                                                        AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree,
                                                                        },
                                                                    )
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: {message}")
                                                                )).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    "Agent create failed: unexpected hub response".to_string()
                                                                )).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: recv error: {e}")
                                                                )).await;
                                                            }
                                                        }
                                                    });
                                                }
                                                BranchAction::Pull => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Pull failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::PullBranch {
                                                            repo_path: rp,
                                                            branch_name: bn.clone(),
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Pull failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::BranchPulled {
                                                                branch_name,
                                                                ..
                                                            }) => {
                                                                let _ = tx
                                                                    .send(StatusMessage {
                                                                        text: format!(
                                                                            "Pulled {branch_name}"
                                                                        ),
                                                                        level: StatusLevel::Success,
                                                                        created: Instant::now(),
                                                                    })
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Pull failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Pull failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Pull failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::StopAgents => {
                                                    let ids: Vec<String> = branch_agents
                                                        .iter()
                                                        .map(|a| a.id.clone())
                                                        .collect();
                                                    stop_agents_ipc(&ids);
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    // Queue worktree cleanup if applicable
                                                    let cleanups =
                                                        crate::worktree::collect_worktree_cleanups(
                                                            &branch_agents,
                                                            &agents,
                                                        );
                                                    if !cleanups.is_empty() {
                                                        pending_worktree_cleanups = cleanups;
                                                        active_menu = pop_worktree_cleanup_menu(
                                                            &mut pending_worktree_cleanups,
                                                        );
                                                    }
                                                }
                                                BranchAction::OpenAgent => {
                                                    if branch_agents.len() == 1 {
                                                        let agent = &branch_agents[0];
                                                        let fm_cols =
                                                            (last_content_area.width * 40 / 100)
                                                                .saturating_sub(2)
                                                                .max(1);
                                                        let fm_rows = last_content_area
                                                            .height
                                                            .saturating_sub(3)
                                                            .max(1);
                                                        let existing_terminals = overview_state
                                                            .take_agent_terminals(&agent.id);
                                                        focus_mode_state.open_agent(
                                                            &agent.id,
                                                            &agent.agent_binary,
                                                            fm_cols,
                                                            fm_rows,
                                                            &agent.working_dir,
                                                            agent.repo_path.as_deref(),
                                                            agent.branch_name.as_deref(),
                                                            agent.is_worktree,
                                                            existing_terminals,
                                                        );
                                                        in_focus_mode = true;
                                                    } else if branch_agents.len() > 1 {
                                                        let labels: Vec<String> = branch_agents
                                                            .iter()
                                                            .map(|a| {
                                                                format!(
                                                                    "{} ({})",
                                                                    a.agent_binary, a.id
                                                                )
                                                            })
                                                            .collect();
                                                        active_menu =
                                                            Some(ActiveMenu::AgentPicker {
                                                                menu: ContextMenu::new(
                                                                    "Open Agent",
                                                                    labels,
                                                                ),
                                                                agents: branch_agents,
                                                            });
                                                    }
                                                }
                                                BranchAction::OpenInEditor => {
                                                    if open_branch_in_editor(
                                                        &repo_path,
                                                        &branch_name,
                                                        &repos,
                                                        &mut active_menu,
                                                        &editors_cache,
                                                        &mut status_message,
                                                    ) {
                                                        last_repo_fetch = Instant::now()
                                                            - Duration::from_secs(10);
                                                    }
                                                }
                                                BranchAction::RemoveWorktree => {
                                                    remove_worktree_ipc(
                                                        &repo_path,
                                                        &branch_name,
                                                        false,
                                                        false,
                                                    );
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::DeleteBranch => {
                                                    delete_local_branch_ipc(
                                                        &repo_path,
                                                        &branch_name,
                                                    );
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::BaseWorktreeOff => {
                                                    if create_modal.is_some() {
                                                        // Modal already open — skip to avoid
                                                        // stacking. (Caller should close the
                                                        // existing modal first.)
                                                    } else if let Some(repo_info) = repos
                                                        .iter()
                                                        .find(|r| r.path == repo_path)
                                                        .cloned()
                                                    {
                                                        let modal =
                                                            CreateAgentModal::new_with_branch(
                                                                repos.clone(),
                                                                repo_info,
                                                                branch_name.clone(),
                                                            );
                                                        create_modal = Some(modal);
                                                    }
                                                }
                                                BranchAction::DetachHead => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Detach failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::DetachHead {
                                                            repo_path: rp,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Detach failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::HeadDetached) => {
                                                                let _ = tx
                                                                    .send(StatusMessage {
                                                                        text: "HEAD detached"
                                                                            .to_string(),
                                                                        level: StatusLevel::Success,
                                                                        created: Instant::now(),
                                                                    })
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Detach failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Detach failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Detach failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::CheckoutLocal => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Checkout failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::CheckoutLocalBranch {
                                                            repo_path: rp,
                                                            branch_name: bn.clone(),
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Checkout failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(
                                                                HubMessage::LocalBranchCheckedOut {
                                                                    branch_name,
                                                                },
                                                            ) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checked out {branch_name}"),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Checkout failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::RemoteStartAgent => {
                                                    if let Some(local) =
                                                        branch_name.split_once('/').map(|x| x.1)
                                                    {
                                                        let tx = agent_start_tx.clone();
                                                        let hub = hub_name.to_string();
                                                        let rp = repo_path.clone();
                                                        let remote_ref = branch_name.clone();
                                                        let local_name = local.to_string();
                                                        let bp = bypass_permissions;
                                                        let (cols, rows) =
                                                            crossterm::terminal::size()
                                                                .unwrap_or((80, 24));
                                                        tokio::spawn(async move {
                                                            let mut stream = match ipc::try_connect(
                                                            )
                                                            .await
                                                            {
                                                                Ok(s) => s,
                                                                Err(e) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                            format!("Agent create failed: hub connect error: {e}")
                                                                        )).await;
                                                                    return;
                                                                }
                                                            };
                                                            let msg =
                                                                CliMessage::CreateWorktreeAgent {
                                                                    repo_path: rp,
                                                                    target_branch: Some(remote_ref),
                                                                    new_branch: Some(local_name),
                                                                    prompt: None,
                                                                    agent_binary: None,
                                                                    cols,
                                                                    rows: rows
                                                                        .saturating_sub(2)
                                                                        .max(1),
                                                                    accept_edits: false,
                                                                    plan_mode: bp,
                                                                    allow_bypass: bp,
                                                                    hub,
                                                                    auto_exit: false,
                                                                };
                                                            if let Err(e) = clust_ipc::send_message(
                                                                &mut stream,
                                                                &msg,
                                                            )
                                                            .await
                                                            {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: send error: {e}")
                                                                )).await;
                                                                return;
                                                            }
                                                            match clust_ipc::recv_message::<HubMessage>(
                                                                &mut stream,
                                                            )
                                                            .await
                                                            {
                                                                Ok(HubMessage::WorktreeAgentStarted {
                                                                    id,
                                                                    agent_binary,
                                                                    working_dir,
                                                                    repo_path,
                                                                    branch_name,
                                                                }) => {
                                                                    let _ = tx
                                                                        .send(AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree: true,
                                                                        })
                                                                        .await;
                                                                }
                                                                Ok(HubMessage::Error { message }) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: {message}")
                                                                    )).await;
                                                                }
                                                                Ok(_) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                        "Agent create failed: unexpected hub response".to_string()
                                                                    )).await;
                                                                }
                                                                Err(e) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: recv error: {e}")
                                                                    )).await;
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                                BranchAction::RemoteCreateWorktree => {
                                                    if let Some(local) =
                                                        branch_name.split_once('/').map(|x| x.1)
                                                    {
                                                        add_worktree_ipc(
                                                            &repo_path,
                                                            local,
                                                            &branch_name,
                                                        );
                                                        last_repo_fetch = Instant::now()
                                                            - Duration::from_secs(10);
                                                    }
                                                }
                                                BranchAction::DeleteRemoteBranch => {
                                                    delete_remote_branch_ipc(
                                                        &repo_path,
                                                        &branch_name,
                                                    );
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::CheckoutRemote => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Checkout failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg =
                                                            CliMessage::CheckoutRemoteBranch {
                                                                working_dir: Some(rp),
                                                                repo_name: None,
                                                                remote_branch: bn.clone(),
                                                            };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Checkout failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::RemoteBranchCheckedOut { branch_name }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checked out {branch_name}"),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Checkout failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                            }
                                        }
                                    }
                                    ActiveMenu::ConfirmAction { action, .. } => {
                                        if idx == 0 {
                                            match action {
                                                ConfirmedAction::PurgeRepo { repo_path } => {
                                                    purge_progress =
                                                        Some(start_purge_async(&repo_path));
                                                }
                                                ConfirmedAction::RemoveRepository { repo_path } => {
                                                    unregister_repo_ipc(&repo_path);
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                ConfirmedAction::DeleteRepository { repo_path } => {
                                                    delete_repo_ipc(&repo_path, status_tx.clone());
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                ConfirmedAction::DeleteScheduledTask { task_id } => {
                                                    let refresh_tx = scheduled_task_tx.clone();
                                                    tokio::spawn(async move {
                                                        let _ = ipc::send_one_shot(
                                                            CliMessage::DeleteScheduledTask {
                                                                id: task_id,
                                                            },
                                                        )
                                                        .await;
                                                        let tasks =
                                                            ipc::fetch_scheduled_tasks().await;
                                                        let _ = refresh_tx.send(tasks).await;
                                                    });
                                                }
                                                ConfirmedAction::ClearScheduledTasksByStatus {
                                                    status,
                                                } => {
                                                    let refresh_tx = scheduled_task_tx.clone();
                                                    tokio::spawn(async move {
                                                        let _ = ipc::send_one_shot(
                                                            CliMessage::DeleteScheduledTasksByStatus {
                                                                status,
                                                            },
                                                        )
                                                        .await;
                                                        let tasks =
                                                            ipc::fetch_scheduled_tasks().await;
                                                        let _ = refresh_tx.send(tasks).await;
                                                    });
                                                }
                                                ConfirmedAction::StartAgentDetach {
                                                    repo_path,
                                                    branch_name,
                                                } => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let bp = bypass_permissions;
                                                    let (cols, rows) = crossterm::terminal::size()
                                                        .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: hub connect error: {e}")
                                                                    )).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::CreateWorktreeAgent {
                                                            repo_path,
                                                            target_branch: Some(branch_name),
                                                            new_branch: None,
                                                            prompt: None,
                                                            agent_binary: None,
                                                            cols,
                                                            rows: rows.saturating_sub(2).max(1),
                                                            accept_edits: false,
                                                            plan_mode: bp,
                                                            allow_bypass: bp,
                                                            hub,
                                                            auto_exit: false,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(AgentStartResult::Failed(
                                                                format!("Agent create failed: send error: {e}")
                                                            )).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(
                                                                HubMessage::WorktreeAgentStarted {
                                                                    id,
                                                                    agent_binary,
                                                                    working_dir,
                                                                    repo_path,
                                                                    branch_name,
                                                                },
                                                            ) => {
                                                                let _ = tx
                                                                    .send(
                                                                        AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree: true,
                                                                        },
                                                                    )
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: {message}")
                                                                )).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    "Agent create failed: unexpected hub response".to_string()
                                                                )).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: recv error: {e}")
                                                                )).await;
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    ActiveMenu::WorktreeCleanup {
                                        repo_path,
                                        branch_name,
                                        ..
                                    } => {
                                        match idx {
                                            1 => {
                                                // Discard worktree
                                                remove_worktree_ipc(
                                                    &repo_path,
                                                    &branch_name,
                                                    false,
                                                    true,
                                                );
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            2 => {
                                                // Discard worktree + branch
                                                remove_worktree_ipc(
                                                    &repo_path,
                                                    &branch_name,
                                                    true,
                                                    true,
                                                );
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            _ => {} // Keep
                                        }
                                        // Show next pending cleanup if any
                                        if let Some(m) = pop_worktree_cleanup_menu(
                                            &mut pending_worktree_cleanups,
                                        ) {
                                            active_menu = Some(m);
                                        }
                                    }
                                    ActiveMenu::EditorPicker {
                                        target_path,
                                        repo_path,
                                        editors,
                                        ..
                                    } => {
                                        if let Some(editor) = editors.get(idx).cloned() {
                                            crate::editor::open_in_editor(&editor, &target_path);
                                            if repo_path.is_some() {
                                                active_menu = Some(ActiveMenu::EditorRemember {
                                                    repo_path,
                                                    editor,
                                                    menu: ContextMenu::new(
                                                        "Remember this editor?",
                                                        vec![
                                                            "Just this time".to_string(),
                                                            "For this repository".to_string(),
                                                            "For all repositories".to_string(),
                                                        ],
                                                    ),
                                                });
                                            }
                                        }
                                    }
                                    ActiveMenu::EditorRemember {
                                        repo_path, editor, ..
                                    } => {
                                        match idx {
                                            1 => {
                                                // For this repository
                                                if let Some(rp) = repo_path {
                                                    set_repo_editor_ipc(&rp, &editor.binary);
                                                    if let Some(repo) =
                                                        repos.iter_mut().find(|r| r.path == rp)
                                                    {
                                                        repo.editor = Some(editor.binary);
                                                    }
                                                }
                                            }
                                            2 => {
                                                // For all repositories
                                                set_default_editor_ipc(&editor.binary);
                                            }
                                            _ => {} // Just this time
                                        }
                                    }
                                }
                            }
                            MenuResult::Dismissed => {
                                active_menu = None;
                                // If dismissed during worktree cleanup, show next if any
                                if let Some(m) =
                                    pop_worktree_cleanup_menu(&mut pending_worktree_cleanups)
                                {
                                    active_menu = Some(m);
                                }
                            }
                            MenuResult::None => {}
                        }
                    // Create-agent modal takes priority over all other input
                    } else if let Some(ref mut modal) = create_modal {
                        match modal.handle_key(key) {
                            ModalResult::Cancelled => {
                                create_modal = None;
                            }
                            ModalResult::Completed(output) => {
                                create_modal = None;
                                let tx = agent_start_tx.clone();
                                let hub = hub_name.to_string();
                                let bp = bypass_permissions;
                                let plan = output.plan_mode;
                                let auto_exit = output.auto_exit;
                                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                                tokio::spawn(async move {
                                    let mut stream = match ipc::try_connect().await {
                                        Ok(s) => s,
                                        Err(e) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(format!(
                                                    "Agent create failed: hub connect error: {e}"
                                                )))
                                                .await;
                                            return;
                                        }
                                    };
                                    let msg = CliMessage::CreateWorktreeAgent {
                                        repo_path: output.repo_path,
                                        target_branch: output.target_branch,
                                        new_branch: output.new_branch,
                                        prompt: output.prompt,
                                        agent_binary: None,
                                        cols,
                                        rows: rows.saturating_sub(2).max(1),
                                        accept_edits: false,
                                        plan_mode: plan,
                                        allow_bypass: bp,
                                        hub,
                                        auto_exit,
                                    };
                                    if let Err(e) = clust_ipc::send_message(&mut stream, &msg).await
                                    {
                                        let _ = tx
                                            .send(AgentStartResult::Failed(format!(
                                                "Agent create failed: send error: {e}"
                                            )))
                                            .await;
                                        return;
                                    }
                                    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
                                        Ok(HubMessage::WorktreeAgentStarted {
                                            id,
                                            agent_binary,
                                            working_dir,
                                            repo_path,
                                            branch_name,
                                        }) => {
                                            let _ = tx
                                                .send(AgentStartResult::Started {
                                                    agent_id: id,
                                                    agent_binary,
                                                    working_dir,
                                                    repo_path,
                                                    branch_name,
                                                    is_worktree: true,
                                                })
                                                .await;
                                        }
                                        Ok(HubMessage::Error { message }) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(format!(
                                                    "Agent create failed: {message}"
                                                )))
                                                .await;
                                        }
                                        Ok(_) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(
                                                    "Agent create failed: unexpected hub response"
                                                        .to_string(),
                                                ))
                                                .await;
                                        }
                                        Err(e) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(format!(
                                                    "Agent create failed: recv error: {e}"
                                                )))
                                                .await;
                                        }
                                    }
                                });
                            }
                            ModalResult::Pending => {}
                        }
                    // Schedule task modal (Opt+S) — multi-step. Mirrors create_modal.
                    } else if let Some(ref mut modal) = schedule_modal {
                        match modal.handle_key(key) {
                            ScheduleModalResult::Cancelled => {
                                schedule_modal = None;
                            }
                            ScheduleModalResult::Pending => {}
                            ScheduleModalResult::Completed(out) => {
                                schedule_modal = None;
                                let tx = status_tx.clone();
                                let refresh_tx = scheduled_task_tx.clone();
                                tokio::spawn(async move {
                                    let msg = clust_ipc::CliMessage::CreateScheduledTask {
                                        repo_path: out.repo_path,
                                        base_branch: out.base_branch,
                                        new_branch: out.new_branch,
                                        prompt: out.prompt,
                                        plan_mode: out.plan_mode,
                                        auto_exit: out.auto_exit,
                                        agent_binary: None,
                                        schedule: out.schedule,
                                        extra_agent_deps: out.extra_agent_deps,
                                    };
                                    match ipc::send_one_shot(msg).await {
                                        Ok(HubMessage::ScheduledTaskCreated { info }) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: format!(
                                                        "Scheduled task on {}",
                                                        info.branch_name
                                                    ),
                                                    level: StatusLevel::Success,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                        }
                                        Ok(HubMessage::Error { message }) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: format!("Schedule failed: {message}"),
                                                    level: StatusLevel::Error,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                        }
                                        Ok(_) => {}
                                        Err(e) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: format!("Schedule failed: {e}"),
                                                    level: StatusLevel::Error,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                        }
                                    }
                                    // Force a refresh on the next tick.
                                    let tasks = ipc::fetch_scheduled_tasks().await;
                                    let _ = refresh_tx.send(tasks).await;
                                });
                            }
                        }
                    // Edit-prompt modal (e on a task in Schedule tab).
                    } else if let Some(ref mut modal) = edit_prompt_modal {
                        match modal.handle_key(key) {
                            EditPromptResult::Cancelled => {
                                edit_prompt_modal = None;
                            }
                            EditPromptResult::Pending => {}
                            EditPromptResult::Submitted { task_id, prompt } => {
                                edit_prompt_modal = None;
                                let tx = status_tx.clone();
                                let refresh_tx = scheduled_task_tx.clone();
                                tokio::spawn(async move {
                                    let msg = clust_ipc::CliMessage::UpdateScheduledTaskPrompt {
                                        id: task_id,
                                        prompt,
                                    };
                                    if let Err(e) = ipc::send_one_shot(msg).await {
                                        let _ = tx
                                            .send(StatusMessage {
                                                text: format!("Update failed: {e}"),
                                                level: StatusLevel::Error,
                                                created: Instant::now(),
                                            })
                                            .await;
                                    }
                                    let tasks = ipc::fetch_scheduled_tasks().await;
                                    let _ = refresh_tx.send(tasks).await;
                                });
                            }
                        }
                    } else if let Some(ref mut modal) = search_modal {
                        match modal.handle_key(key) {
                            SearchResult::Cancelled => {
                                search_modal = None;
                            }
                            SearchResult::Selected(agent) => {
                                search_modal = None;
                                let fm_cols = (last_content_area.width * 40 / 100)
                                    .saturating_sub(2)
                                    .max(1);
                                let fm_rows = last_content_area.height.saturating_sub(3).max(1);
                                let existing_terminals =
                                    overview_state.take_agent_terminals(&agent.id);
                                focus_mode_state.open_agent(
                                    &agent.id,
                                    &agent.agent_binary,
                                    fm_cols,
                                    fm_rows,
                                    &agent.working_dir,
                                    agent.repo_path.as_deref(),
                                    agent.branch_name.as_deref(),
                                    agent.is_worktree,
                                    existing_terminals,
                                );
                                in_focus_mode = true;
                                active_tab = ActiveTab::Overview;
                            }
                            SearchResult::Pending => {}
                        }
                    // Detached (directory) agent modal takes priority
                    } else if let Some(ref mut modal) = detached_modal {
                        match modal.handle_key(key) {
                            DetachedModalResult::Cancelled => {
                                detached_modal = None;
                            }
                            DetachedModalResult::Completed(output) => {
                                detached_modal = None;
                                let tx = agent_start_tx.clone();
                                let hub = hub_name.to_string();
                                let bp = bypass_permissions;
                                let plan = output.plan_mode;
                                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                                let wd = output.working_dir.clone();
                                tokio::spawn(async move {
                                    let mut stream = match ipc::try_connect().await {
                                        Ok(s) => s,
                                        Err(e) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(format!(
                                                    "Agent start failed: hub connect error: {e}"
                                                )))
                                                .await;
                                            return;
                                        }
                                    };
                                    let msg = CliMessage::StartAgent {
                                        prompt: output.prompt,
                                        agent_binary: None,
                                        working_dir: wd.clone(),
                                        cols,
                                        rows: rows.saturating_sub(2).max(1),
                                        accept_edits: false,
                                        plan_mode: plan,
                                        allow_bypass: bp,
                                        hub,
                                    };
                                    if let Err(e) = clust_ipc::send_message(&mut stream, &msg).await
                                    {
                                        let _ = tx
                                            .send(AgentStartResult::Failed(format!(
                                                "Agent start failed: send error: {e}"
                                            )))
                                            .await;
                                        return;
                                    }
                                    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
                                        Ok(HubMessage::AgentStarted {
                                            id,
                                            agent_binary,
                                            is_worktree,
                                            repo_path,
                                            branch_name,
                                        }) => {
                                            let _ = tx
                                                .send(AgentStartResult::Started {
                                                    agent_id: id,
                                                    agent_binary,
                                                    working_dir: wd,
                                                    repo_path,
                                                    branch_name,
                                                    is_worktree,
                                                })
                                                .await;
                                        }
                                        Ok(HubMessage::Error { message }) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(format!(
                                                    "Agent start failed: {message}"
                                                )))
                                                .await;
                                        }
                                        Ok(_) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(
                                                    "Agent start failed: unexpected hub response"
                                                        .to_string(),
                                                ))
                                                .await;
                                        }
                                        Err(e) => {
                                            let _ = tx
                                                .send(AgentStartResult::Failed(format!(
                                                    "Agent start failed: recv error: {e}"
                                                )))
                                                .await;
                                        }
                                    }
                                });
                            }
                            DetachedModalResult::Pending => {}
                        }
                    } else if let Some(ref mut modal) = repo_modal {
                        match modal.handle_key(key) {
                            RepoModalResult::Cancelled => {
                                repo_modal = None;
                            }
                            RepoModalResult::CreateRepo(output) => {
                                repo_modal = None;
                                let tx = status_tx.clone();
                                tokio::spawn(async move {
                                    let mut stream = match ipc::try_connect().await {
                                        Ok(s) => s,
                                        Err(e) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: format!("Create failed: {e}"),
                                                    level: StatusLevel::Error,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                            return;
                                        }
                                    };
                                    let msg = CliMessage::CreateRepo {
                                        parent_dir: output.parent_dir,
                                        name: output.name,
                                    };
                                    if let Err(e) = clust_ipc::send_message(&mut stream, &msg).await
                                    {
                                        let _ = tx
                                            .send(StatusMessage {
                                                text: format!("Create failed: {e}"),
                                                level: StatusLevel::Error,
                                                created: Instant::now(),
                                            })
                                            .await;
                                        return;
                                    }
                                    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
                                        Ok(HubMessage::RepoCreated { name, .. }) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: format!("Created repository \"{name}\""),
                                                    level: StatusLevel::Success,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                        }
                                        Ok(HubMessage::Error { message }) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: message,
                                                    level: StatusLevel::Error,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                        }
                                        Err(e) => {
                                            let _ = tx
                                                .send(StatusMessage {
                                                    text: format!("Create failed: {e}"),
                                                    level: StatusLevel::Error,
                                                    created: Instant::now(),
                                                })
                                                .await;
                                        }
                                        _ => {}
                                    }
                                });
                                last_repo_fetch = Instant::now() - Duration::from_secs(10);
                            }
                            RepoModalResult::CloneRepo(output) => {
                                repo_modal = None;
                                clone_progress = Some(start_clone_async(
                                    &output.url,
                                    &output.parent_dir,
                                    output.name.as_deref(),
                                ));
                            }
                            RepoModalResult::Pending => {}
                        }
                    } else if key.code == KeyCode::Char('e')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+E opens create-agent modal
                        if !repos.is_empty() && create_modal.is_none() {
                            let modal = CreateAgentModal::new(repos.clone());
                            create_modal = Some(modal);
                            show_help = false;
                        }
                    } else if key.code == KeyCode::Char('s')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+S opens schedule-task modal.
                        // Hand the modal both the scheduled-task snapshot and
                        // the running-agent list so the dep picker can offer
                        // already-running Opt+E agents alongside scheduled
                        // tasks.
                        if !repos.is_empty() && schedule_modal.is_none() {
                            let modal = ScheduleTaskModal::new(
                                repos.clone(),
                                scheduled_tasks.clone(),
                                agents.clone(),
                            );
                            schedule_modal = Some(modal);
                            show_help = false;
                        }
                    } else if key.code == KeyCode::Char('d')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+D opens detached agent modal
                        if detached_modal.is_none() {
                            let modal = DetachedAgentModal::new();
                            detached_modal = Some(modal);
                            show_help = false;
                        }
                    } else if key.code == KeyCode::Char('f')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+F opens search-agent modal
                        if !agents.is_empty() && search_modal.is_none() {
                            search_modal = Some(SearchModal::new(agents.clone()));
                            show_help = false;
                        }
                    } else if key.code == KeyCode::Char('n')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+N opens new repository modal
                        if repo_modal.is_none() {
                            repo_modal = Some(RepoModal::new());
                            show_help = false;
                        }
                    } else if key.code == KeyCode::Char('v')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+V opens in editor. On the
                        // Repositories tab when a local branch is selected,
                        // route through open_branch_in_editor so a worktree
                        // is created on-demand for non-worktree branches.
                        let mut handled = false;
                        if !in_focus_mode
                            && active_tab == ActiveTab::Repositories
                            && selection.level == TreeLevel::Branch
                            && selection.category_idx == 0
                        {
                            if let Some(repo) = display_repos.get(selection.repo_idx) {
                                if !repo.path.is_empty() {
                                    if let Some(branch) =
                                        repo.local_branches.get(selection.branch_idx)
                                    {
                                        let rp = repo.path.clone();
                                        let bn = branch.name.clone();
                                        if open_branch_in_editor(
                                            &rp,
                                            &bn,
                                            &repos,
                                            &mut active_menu,
                                            &editors_cache,
                                            &mut status_message,
                                        ) {
                                            last_repo_fetch =
                                                Instant::now() - Duration::from_secs(10);
                                        }
                                        handled = true;
                                    }
                                }
                            }
                        }
                        if !handled {
                            let (target, rp) = resolve_editor_target(
                                in_focus_mode,
                                &focus_mode_state,
                                active_tab,
                                &overview_state,
                                &display_repos,
                                &selection,
                                &agents,
                            );
                            if let Some(target_path) = target {
                                trigger_open_in_editor(
                                    &target_path,
                                    rp.as_deref(),
                                    &repos,
                                    &mut active_menu,
                                    &editors_cache,
                                );
                            }
                        }
                    } else if key.code == KeyCode::Char('b')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+B toggles bypass permissions
                        bypass_permissions = !bypass_permissions;
                        set_bypass_permissions_ipc(bypass_permissions);
                        status_message = Some(StatusMessage {
                            text: if bypass_permissions {
                                "bypass permissions: on".to_string()
                            } else {
                                "bypass permissions: off".to_string()
                            },
                            level: StatusLevel::Success,
                            created: Instant::now(),
                        });
                    } else
                    // Focus mode: behavior depends on which side has focus
                    if in_focus_mode && focus_mode_state.is_active() {
                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        if focus_mode_state.focus_side == overview::FocusSide::Left {
                            // Left panel focused
                            if shift {
                                match key.code {
                                    KeyCode::Up => {
                                        // Exit focus mode
                                        if active_tab == ActiveTab::Overview
                                            && overview_state.initialized
                                        {
                                            overview_state.force_resize_all();
                                        }
                                        if let Some((aid, cache)) = focus_mode_state.detach() {
                                            overview_state.store_agent_terminals(aid, cache);
                                        } else {
                                            // detach() returned None: nothing new to cache.
                                            // Any previously-stored cache is preserved.
                                        }
                                        in_focus_mode = false;
                                    }
                                    KeyCode::Right => {
                                        focus_mode_state.focus_side = overview::FocusSide::Right;
                                    }
                                    KeyCode::BackTab => {
                                        focus_mode_state.left_tab =
                                            focus_mode_state.left_tab.prev();
                                    }
                                    KeyCode::PageUp
                                        if focus_mode_state.left_tab
                                            == overview::LeftPanelTab::Terminal =>
                                    {
                                        if let Some(panel) = focus_mode_state.current_terminal_mut()
                                        {
                                            let page = panel.vterm.rows();
                                            let max = panel.vterm.scrollback_len();
                                            panel.scroll_offset =
                                                (panel.scroll_offset + page).min(max);
                                        }
                                    }
                                    KeyCode::PageDown
                                        if focus_mode_state.left_tab
                                            == overview::LeftPanelTab::Terminal =>
                                    {
                                        if let Some(panel) = focus_mode_state.current_terminal_mut()
                                        {
                                            let page = panel.vterm.rows();
                                            panel.scroll_offset =
                                                panel.scroll_offset.saturating_sub(page);
                                        }
                                    }
                                    _ if focus_mode_state.left_tab
                                        == overview::LeftPanelTab::Terminal
                                        && focus_mode_state.terminal_input_focused =>
                                    {
                                        // Forward shifted keys to the active
                                        // terminal only when in Type mode.
                                        if let Some(bytes) =
                                            overview::input::key_event_to_bytes(&key)
                                        {
                                            focus_mode_state.send_terminal_input(bytes);
                                        }
                                    }
                                    _ => {}
                                }
                            } else {
                                match focus_mode_state.left_tab {
                                    overview::LeftPanelTab::Changes => {
                                        // tab bar (1) + hint bar (1) = 2 rows overhead
                                        let vh =
                                            last_content_area.height.saturating_sub(2) as usize;
                                        match key.code {
                                            KeyCode::Up => focus_mode_state.diff_cursor_up(vh),
                                            KeyCode::Down => focus_mode_state.diff_cursor_down(vh),
                                            KeyCode::Char('v') => {
                                                focus_mode_state.diff_toggle_anchor()
                                            }
                                            KeyCode::Enter => {
                                                focus_mode_state.diff_send_selection();
                                                focus_mode_state.diff_cancel_selection();
                                            }
                                            KeyCode::Esc => {
                                                focus_mode_state.diff_cancel_selection()
                                            }
                                            KeyCode::Tab => {
                                                focus_mode_state.diff_cancel_selection();
                                                focus_mode_state.left_tab =
                                                    focus_mode_state.left_tab.next();
                                            }
                                            _ => {}
                                        }
                                    }
                                    overview::LeftPanelTab::Compare => {
                                        match focus_mode_state.compare_picker.mode {
                                            overview::BranchPickerMode::Searching => {
                                                let changed =
                                                    focus_mode_state.compare_picker.handle_key(key);
                                                if changed {
                                                    focus_mode_state.start_compare_diff();
                                                }
                                            }
                                            overview::BranchPickerMode::Selected => {
                                                let vh = last_content_area.height.saturating_sub(3)
                                                    as usize;
                                                match key.code {
                                                    KeyCode::Up => {
                                                        focus_mode_state.compare_cursor_up(vh)
                                                    }
                                                    KeyCode::Down => {
                                                        focus_mode_state.compare_cursor_down(vh)
                                                    }
                                                    KeyCode::Char('v') => {
                                                        focus_mode_state.compare_toggle_anchor()
                                                    }
                                                    KeyCode::Enter => {
                                                        if focus_mode_state.compare_has_selection()
                                                        {
                                                            focus_mode_state
                                                                .compare_send_selection();
                                                            focus_mode_state
                                                                .compare_cancel_selection();
                                                        } else {
                                                            focus_mode_state
                                                                .compare_picker
                                                                .enter_search();
                                                        }
                                                    }
                                                    KeyCode::Esc => {
                                                        focus_mode_state.compare_cancel_selection()
                                                    }
                                                    KeyCode::Tab => {
                                                        focus_mode_state.compare_cancel_selection();
                                                        focus_mode_state.left_tab =
                                                            focus_mode_state.left_tab.next();
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                    overview::LeftPanelTab::Terminal => {
                                        // Two sub-modes: Navigate (default,
                                        // keys = TUI commands) and Type (keys
                                        // forwarded to the active PTY).
                                        // Ctrl+\ toggles in both directions.
                                        let is_ctrl_backslash =
                                            key.modifiers.contains(KeyModifiers::CONTROL)
                                                && matches!(key.code, KeyCode::Char('\\'));
                                        if is_ctrl_backslash {
                                            focus_mode_state.terminal_input_focused =
                                                !focus_mode_state.terminal_input_focused;
                                        } else if focus_mode_state.terminal_input_focused {
                                            // Type mode: forward everything to
                                            // the PTY (existing behaviour).
                                            if key.code == KeyCode::Esc {
                                                focus_mode_state.send_terminal_input(vec![0x1b]);
                                            } else if let Some(bytes) =
                                                overview::input::key_event_to_bytes(&key)
                                            {
                                                focus_mode_state.send_terminal_input(bytes);
                                            }
                                        } else {
                                            // Navigate mode: TUI commands.
                                            match key.code {
                                                KeyCode::Tab => {
                                                    focus_mode_state.left_tab =
                                                        focus_mode_state.left_tab.next();
                                                }
                                                KeyCode::Enter => {
                                                    focus_mode_state.terminal_input_focused = true;
                                                }
                                                KeyCode::Char(']') => {
                                                    focus_mode_state.next_terminal();
                                                }
                                                KeyCode::Char('[') => {
                                                    focus_mode_state.prev_terminal();
                                                }
                                                KeyCode::Char('n') => {
                                                    focus_mode_state.add_terminal();
                                                    // Land in Type mode on the
                                                    // freshly-spawned shell.
                                                    focus_mode_state.terminal_input_focused = true;
                                                }
                                                KeyCode::Char('x') => {
                                                    focus_mode_state.close_current_terminal();
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            // Right panel focused
                            if shift {
                                match key.code {
                                    KeyCode::Up => {
                                        // Exit focus mode
                                        if active_tab == ActiveTab::Overview
                                            && overview_state.initialized
                                        {
                                            overview_state.force_resize_all();
                                        }
                                        if let Some((aid, cache)) = focus_mode_state.detach() {
                                            overview_state.store_agent_terminals(aid, cache);
                                        } else {
                                            // detach() returned None: nothing new to cache.
                                            // Any previously-stored cache is preserved.
                                        }
                                        in_focus_mode = false;
                                    }
                                    KeyCode::Left if focus_mode_state.repo_path.is_some() => {
                                        focus_mode_state.focus_side = overview::FocusSide::Left;
                                        // Place cursor at top of visible viewport
                                        focus_mode_state.diff_cursor = focus_mode_state.diff_scroll;
                                        focus_mode_state.compare_cursor =
                                            focus_mode_state.compare_diff_scroll;
                                    }
                                    KeyCode::PageUp => {
                                        if let Some(panel) = &mut focus_mode_state.panel {
                                            let page = panel.vterm.rows();
                                            let max = panel.vterm.scrollback_len();
                                            panel.panel_scroll_offset =
                                                (panel.panel_scroll_offset + page).min(max);
                                        }
                                    }
                                    KeyCode::PageDown => {
                                        if let Some(panel) = &mut focus_mode_state.panel {
                                            let page = panel.vterm.rows();
                                            panel.panel_scroll_offset =
                                                panel.panel_scroll_offset.saturating_sub(page);
                                        }
                                    }
                                    _ => {
                                        if let Some(bytes) =
                                            overview::input::key_event_to_bytes(&key)
                                        {
                                            focus_mode_state.send_input(bytes);
                                        }
                                    }
                                }
                            } else if key.code == KeyCode::Esc {
                                // Forward Esc to agent process
                                focus_mode_state.send_input(vec![0x1b]);
                            } else if let Some(bytes) = overview::input::key_event_to_bytes(&key) {
                                focus_mode_state.send_input(bytes);
                            }
                        }
                    }
                    // When overview terminal is focused, intercept all keys
                    // except Shift+arrows — everything else goes to the agent.
                    else if active_tab == ActiveTab::Overview
                        && matches!(overview_state.focus, OverviewFocus::Terminal(_))
                    {
                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        if shift {
                            match key.code {
                                KeyCode::Up => overview_state.exit_terminal(),
                                KeyCode::Down => {
                                    // Enter focus mode with the focused agent
                                    if let OverviewFocus::Terminal(idx) = overview_state.focus {
                                        if let Some(panel) = overview_state.panels.get(idx) {
                                            let agent_id = panel.id.clone();
                                            let agent_binary = panel.agent_binary.clone();
                                            let found = agents.iter().find(|a| a.id == agent_id);
                                            let working_dir = found
                                                .map(|a| a.working_dir.clone())
                                                .unwrap_or_default();
                                            let repo_path = found.and_then(|a| a.repo_path.clone());
                                            let branch_name =
                                                found.and_then(|a| a.branch_name.clone());
                                            let is_wt =
                                                found.map(|a| a.is_worktree).unwrap_or(false);
                                            let fm_cols = (last_content_area.width * 40 / 100)
                                                .saturating_sub(2)
                                                .max(1);
                                            let fm_rows =
                                                last_content_area.height.saturating_sub(3).max(1);
                                            let existing_terminals =
                                                overview_state.take_agent_terminals(&agent_id);
                                            focus_mode_state.open_agent(
                                                &agent_id,
                                                &agent_binary,
                                                fm_cols,
                                                fm_rows,
                                                &working_dir,
                                                repo_path.as_deref(),
                                                branch_name.as_deref(),
                                                is_wt,
                                                existing_terminals,
                                            );
                                            in_focus_mode = true;
                                        }
                                    }
                                }
                                KeyCode::Left => {
                                    overview_state.focus_prev();
                                    overview_state.force_resize_focused();
                                }
                                KeyCode::Right => {
                                    overview_state.focus_next();
                                    overview_state.force_resize_focused();
                                }
                                KeyCode::PageUp => {
                                    overview_state.panel_scroll_up();
                                }
                                KeyCode::PageDown => {
                                    overview_state.panel_scroll_down();
                                }
                                _ => {
                                    // Shift+other key — forward to agent
                                    if let Some(bytes) = overview::input::key_event_to_bytes(&key) {
                                        overview_state.send_input(bytes);
                                    }
                                }
                            }
                        } else {
                            match key.code {
                                KeyCode::Esc => {
                                    if is_double_esc(&mut last_esc_press) {
                                        // Check for worktree cleanup before exiting terminal
                                        if let OverviewFocus::Terminal(idx) = overview_state.focus {
                                            if let Some(panel) = overview_state.panels.get_mut(idx)
                                            {
                                                if panel.exited
                                                    && panel.is_worktree
                                                    && !panel.worktree_cleanup_shown
                                                {
                                                    panel.worktree_cleanup_shown = true;
                                                    if let (Some(rp), Some(bn)) =
                                                        (&panel.repo_path, &panel.branch_name)
                                                    {
                                                        // Append rather than overwrite — earlier
                                                        // pending cleanups must remain queued.
                                                        pending_worktree_cleanups.extend(
                                                            std::iter::once(
                                                                crate::worktree::WorktreeCleanup {
                                                                    repo_path: rp.clone(),
                                                                    branch_name: bn.clone(),
                                                                },
                                                            ),
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        overview_state.exit_terminal();
                                        if let Some(m) = pop_worktree_cleanup_menu(
                                            &mut pending_worktree_cleanups,
                                        ) {
                                            active_menu = Some(m);
                                        }
                                    } else {
                                        // Single Esc — forward to agent process
                                        overview_state.send_input(vec![0x1b]);
                                    }
                                }
                                KeyCode::PageUp => overview_state.panel_scroll_up(),
                                KeyCode::PageDown => overview_state.panel_scroll_down(),
                                _ => {
                                    if let Some(bytes) = overview::input::key_event_to_bytes(&key) {
                                        overview_state.send_input(bytes);
                                    }
                                }
                            }
                        }
                    } else {
                        // Normal key handling (options bar, other tabs)
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Esc if is_double_esc(&mut last_esc_press) => {
                                break;
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break
                            }
                            KeyCode::Char('Q') => {
                                let mut names: Vec<&str> =
                                    agents.iter().map(|a| a.hub.as_str()).collect();
                                names.sort();
                                names.dedup();
                                hub_count = names.len().max(1);
                                // Collect worktree info before stopping
                                worktree_cleanups =
                                    crate::worktree::collect_worktree_cleanups(&agents, &agents);
                                block_on_async(async {
                                    if let Ok(mut stream) = ipc::try_connect().await {
                                        let _ = ipc::send_stop(&mut stream).await;
                                    }
                                });
                                hub_stopped = true;
                                break;
                            }
                            KeyCode::Tab => {
                                active_tab = active_tab.next();
                                if active_tab == ActiveTab::Overview {
                                    if !overview_state.initialized {
                                        overview_state.sync_agents(&agents, last_content_area);
                                    } else {
                                        overview_state.force_resize_all();
                                    }
                                }
                            }
                            KeyCode::BackTab => {
                                active_tab = active_tab.prev();
                                if active_tab == ActiveTab::Overview {
                                    if !overview_state.initialized {
                                        overview_state.sync_agents(&agents, last_content_area);
                                    } else {
                                        overview_state.force_resize_all();
                                    }
                                }
                            }
                            KeyCode::Char('?') => {
                                show_help = !show_help;
                            }
                            // Overview OptionsBar navigation
                            _ if active_tab == ActiveTab::Overview => {
                                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                                match key.code {
                                    KeyCode::Down if shift => {
                                        overview_state.enter_terminal();
                                        overview_state.force_resize_focused();
                                    }
                                    KeyCode::Left if shift => {
                                        overview_state.scroll_left();
                                    }
                                    KeyCode::Right if shift => {
                                        overview_state.scroll_right(last_content_area.width);
                                    }
                                    // Filter group navigation
                                    KeyCode::Left if !shift && overview_state.filter_cursor > 0 => {
                                        overview_state.filter_cursor -= 1;
                                    }
                                    KeyCode::Right if !shift => {
                                        let has_other =
                                            agents.iter().any(|a| a.repo_path.is_none());
                                        let group_count =
                                            repos.len() + if has_other { 1 } else { 0 };
                                        if group_count > 0
                                            && overview_state.filter_cursor + 1 < group_count
                                        {
                                            overview_state.filter_cursor += 1;
                                        }
                                    }
                                    KeyCode::Enter | KeyCode::Char(' ') => {
                                        // Toggle collapse for the selected repo group
                                        if overview_state.filter_cursor < repos.len() {
                                            if let Some(repo) =
                                                repos.get(overview_state.filter_cursor)
                                            {
                                                if overview_state
                                                    .collapsed_repos
                                                    .contains(&repo.path)
                                                {
                                                    overview_state
                                                        .collapsed_repos
                                                        .remove(&repo.path);
                                                } else {
                                                    overview_state
                                                        .collapsed_repos
                                                        .insert(repo.path.clone());
                                                }
                                            }
                                        } else {
                                            // "Other" group (empty string key)
                                            let key = String::new();
                                            if overview_state.collapsed_repos.contains(&key) {
                                                overview_state.collapsed_repos.remove(&key);
                                            } else {
                                                overview_state.collapsed_repos.insert(key);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            // Repositories tab navigation
                            _ if active_tab == ActiveTab::Repositories => {
                                match key.code {
                                    KeyCode::Enter => match selection.level {
                                        TreeLevel::Repo => {
                                            if let Some(repo) =
                                                display_repos.get(selection.repo_idx)
                                            {
                                                if repo.path == ADD_REPO_SENTINEL {
                                                    // Open the repo create/clone modal
                                                    if repo_modal.is_none() {
                                                        repo_modal = Some(RepoModal::new());
                                                        show_help = false;
                                                    }
                                                } else if !repo.path.is_empty() {
                                                    active_menu = Some(ActiveMenu::RepoActions {
                                                        repo_path: repo.path.clone(),
                                                        menu: ContextMenu::new(
                                                            &repo.name,
                                                            vec![
                                                                "Change Color".to_string(),
                                                                "Open in File System".to_string(),
                                                                "Open in Terminal".to_string(),
                                                                "Stop All Agents".to_string(),
                                                                "Clean Stale Refs".to_string(),
                                                                "Detach".to_string(),
                                                                "Purge".to_string(),
                                                                "Remove Repository".to_string(),
                                                                "Delete Repository".to_string(),
                                                            ],
                                                        ),
                                                    });
                                                }
                                            }
                                        }
                                        TreeLevel::Category => {
                                            // No action on Enter for categories (use Space)
                                        }
                                        TreeLevel::Branch => {
                                            // Open context menu for branches
                                            if let Some(repo) =
                                                display_repos.get(selection.repo_idx)
                                            {
                                                if repo.path.is_empty() {
                                                    // Skip "No Repository"
                                                } else if selection.category_idx == 0 {
                                                    // Local branch context menu
                                                    if let Some(branch) = repo
                                                        .local_branches
                                                        .get(selection.branch_idx)
                                                    {
                                                        let matching: Vec<AgentInfo> = agents
                                                            .iter()
                                                            .filter(|a| {
                                                                a.repo_path.as_deref()
                                                                    == Some(&*repo.path)
                                                                    && a.branch_name.as_deref()
                                                                        == Some(&*branch.name)
                                                            })
                                                            .cloned()
                                                            .collect();
                                                        let mut labels = Vec::new();
                                                        let mut actions = Vec::new();
                                                        if branch.active_agent_count > 0 {
                                                            labels.push("Open Agent".to_string());
                                                            actions.push(BranchAction::OpenAgent);
                                                        }
                                                        labels.push(open_in_editor_label(branch));
                                                        actions.push(BranchAction::OpenInEditor);
                                                        labels.push(
                                                            "Start Agent (worktree)".to_string(),
                                                        );
                                                        actions.push(BranchAction::StartAgent);
                                                        if branch.is_head {
                                                            labels.push(
                                                                "Start Agent (in place)"
                                                                    .to_string(),
                                                            );
                                                            actions.push(
                                                                BranchAction::StartAgentInPlace,
                                                            );
                                                            labels.push("Detach".to_string());
                                                            actions.push(BranchAction::DetachHead);
                                                        }
                                                        if !branch.is_head && !branch.is_worktree {
                                                            labels.push("Checkout".to_string());
                                                            actions
                                                                .push(BranchAction::CheckoutLocal);
                                                        }
                                                        labels
                                                            .push("Base Worktree Off".to_string());
                                                        actions.push(BranchAction::BaseWorktreeOff);
                                                        labels.push("Pull".to_string());
                                                        actions.push(BranchAction::Pull);
                                                        if branch.active_agent_count > 0 {
                                                            labels.push("Stop Agents".to_string());
                                                            actions.push(BranchAction::StopAgents);
                                                        }
                                                        if branch.is_worktree {
                                                            labels.push(
                                                                "Remove Worktree".to_string(),
                                                            );
                                                            actions
                                                                .push(BranchAction::RemoveWorktree);
                                                        }
                                                        labels.push("Delete Branch".to_string());
                                                        actions.push(BranchAction::DeleteBranch);
                                                        active_menu =
                                                            Some(ActiveMenu::BranchActions {
                                                                repo_path: repo.path.clone(),
                                                                branch_name: branch.name.clone(),
                                                                is_head: branch.is_head,
                                                                agents: matching,
                                                                actions,
                                                                menu: ContextMenu::new(
                                                                    &branch.name,
                                                                    labels,
                                                                ),
                                                            });
                                                    }
                                                } else if selection.category_idx == 1 {
                                                    // Remote branch context menu
                                                    if let Some(branch) = repo
                                                        .remote_branches
                                                        .get(selection.branch_idx)
                                                    {
                                                        let mut labels = Vec::new();
                                                        let mut actions = Vec::new();
                                                        labels.push(
                                                            "Checkout & Track Locally".to_string(),
                                                        );
                                                        actions.push(BranchAction::CheckoutRemote);
                                                        labels.push(
                                                            "Start Agent (checkout)".to_string(),
                                                        );
                                                        actions
                                                            .push(BranchAction::RemoteStartAgent);
                                                        labels.push("Create Worktree".to_string());
                                                        actions.push(
                                                            BranchAction::RemoteCreateWorktree,
                                                        );
                                                        labels.push(
                                                            "Delete Remote Branch".to_string(),
                                                        );
                                                        actions
                                                            .push(BranchAction::DeleteRemoteBranch);
                                                        active_menu =
                                                            Some(ActiveMenu::BranchActions {
                                                                repo_path: repo.path.clone(),
                                                                branch_name: branch.name.clone(),
                                                                is_head: false,
                                                                agents: vec![],
                                                                actions,
                                                                menu: ContextMenu::new(
                                                                    &branch.name,
                                                                    labels,
                                                                ),
                                                            });
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                                        selection.jump_prev_repo(&display_repos);
                                    }
                                    KeyCode::Down
                                        if key.modifiers.contains(KeyModifiers::SHIFT) =>
                                    {
                                        selection.jump_next_repo(&display_repos);
                                    }
                                    KeyCode::Up => selection.move_up(&display_repos),
                                    KeyCode::Down => selection.move_down(&display_repos),
                                    KeyCode::Right => selection.descend(&display_repos),
                                    KeyCode::Left => selection.ascend(&display_repos),
                                    KeyCode::Char(' ') => {
                                        selection.toggle_collapse();
                                    }
                                    _ => {}
                                }
                            }
                            // Schedule tab key routing — delegates to ScheduleState
                            // and then dispatches the resulting ScheduleAction.
                            // EnterFocusMode is handled inline because it
                            // needs access to focus_mode_state, the agents
                            // list, and last_content_area; everything else
                            // routes through dispatch_schedule_action.
                            _ if active_tab == ActiveTab::Schedule => {
                                let action = schedule_state.handle_key(key);
                                match action {
                                    ScheduleAction::EnterFocusMode { task_id } => {
                                        let task = schedule_state
                                            .tasks
                                            .iter()
                                            .find(|t| t.id == task_id)
                                            .cloned();
                                        if let Some(task) = task {
                                            if let Some(agent_id) = task.agent_id.as_deref() {
                                                let found =
                                                    agents.iter().find(|a| a.id == agent_id);
                                                let working_dir = found
                                                    .map(|a| a.working_dir.clone())
                                                    .unwrap_or_default();
                                                let repo_path = found
                                                    .and_then(|a| a.repo_path.clone())
                                                    .or_else(|| Some(task.repo_path.clone()));
                                                let branch_name = found
                                                    .and_then(|a| a.branch_name.clone())
                                                    .or_else(|| Some(task.branch_name.clone()));
                                                let is_wt = found
                                                    .map(|a| a.is_worktree)
                                                    .unwrap_or(true);
                                                let agent_binary = found
                                                    .map(|a| a.agent_binary.clone())
                                                    .unwrap_or_else(|| task.agent_binary.clone());
                                                let fm_cols = (last_content_area.width * 40
                                                    / 100)
                                                    .saturating_sub(2)
                                                    .max(1);
                                                let fm_rows = last_content_area
                                                    .height
                                                    .saturating_sub(3)
                                                    .max(1);
                                                let existing_terminals = overview_state
                                                    .take_agent_terminals(agent_id);
                                                focus_mode_state.open_agent(
                                                    agent_id,
                                                    &agent_binary,
                                                    fm_cols,
                                                    fm_rows,
                                                    &working_dir,
                                                    repo_path.as_deref(),
                                                    branch_name.as_deref(),
                                                    is_wt,
                                                    existing_terminals,
                                                );
                                                in_focus_mode = true;
                                            }
                                        }
                                    }
                                    other => {
                                        dispatch_schedule_action(
                                            other,
                                            &mut edit_prompt_modal,
                                            &mut active_menu,
                                            status_tx.clone(),
                                            scheduled_task_tx.clone(),
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                        // Dismiss help overlay on any non-? keypress
                        if key.code != KeyCode::Char('?') {
                            show_help = false;
                        }
                    }
                }
                Event::Paste(ref text) => {
                    if let Some(ref mut modal) = create_modal {
                        modal.handle_paste(text);
                    } else if let Some(ref mut modal) = schedule_modal {
                        modal.handle_paste(text);
                    } else if let Some(ref mut modal) = edit_prompt_modal {
                        modal.handle_paste(text);
                    } else if let Some(ref mut modal) = search_modal {
                        modal.handle_paste(text);
                    } else if let Some(ref mut modal) = detached_modal {
                        modal.handle_paste(text);
                    } else if let Some(ref mut modal) = repo_modal {
                        modal.handle_paste(text);
                    } else if in_focus_mode
                        && focus_mode_state.compare_picker.mode
                            == overview::BranchPickerMode::Searching
                    {
                        focus_mode_state.compare_picker.handle_paste(text);
                    } else if in_focus_mode
                        && focus_mode_state.is_active()
                        && focus_mode_state.focus_side == overview::FocusSide::Right
                    {
                        let mut bytes = Vec::new();
                        bytes.extend_from_slice(b"\x1b[200~");
                        bytes.extend_from_slice(text.as_bytes());
                        bytes.extend_from_slice(b"\x1b[201~");
                        focus_mode_state.send_input(bytes);
                    } else if in_focus_mode
                        && focus_mode_state.is_active()
                        && focus_mode_state.focus_side == overview::FocusSide::Left
                        && focus_mode_state.left_tab == overview::LeftPanelTab::Terminal
                    {
                        let mut bytes = Vec::new();
                        bytes.extend_from_slice(b"\x1b[200~");
                        bytes.extend_from_slice(text.as_bytes());
                        bytes.extend_from_slice(b"\x1b[201~");
                        focus_mode_state.send_terminal_input(bytes);
                    } else if active_tab == ActiveTab::Overview
                        && matches!(overview_state.focus, overview::OverviewFocus::Terminal(_))
                    {
                        let mut bytes = Vec::new();
                        bytes.extend_from_slice(b"\x1b[200~");
                        bytes.extend_from_slice(text.as_bytes());
                        bytes.extend_from_slice(b"\x1b[201~");
                        overview_state.send_input(bytes);
                    }
                }
                Event::Resize(cols, rows) => {
                    // Compute content area from the new dimensions directly
                    // to avoid using stale last_content_area.
                    let new_content_area = Rect {
                        x: 0,
                        y: 1, // tab bar
                        width: cols,
                        height: rows.saturating_sub(2), // tab bar + status bar
                    };
                    if active_tab == ActiveTab::Overview && !in_focus_mode {
                        overview_state.handle_resize(agents.len(), new_content_area);
                    }
                    if in_focus_mode {
                        let fm_cols = (new_content_area.width * 40 / 100).saturating_sub(2).max(1);
                        let fm_rows = new_content_area.height.saturating_sub(3).max(1);
                        focus_mode_state.handle_resize(fm_cols, fm_rows);
                        let term_cols = (new_content_area.width * 60 / 100).max(1);
                        let term_rows = new_content_area.height.saturating_sub(2).max(1);
                        focus_mode_state.handle_terminal_resize(term_cols, term_rows);
                    }
                }
                Event::FocusGained => {
                    // Re-assert panel sizes to the hub. The PTY may have been
                    // resized by another client while the window was unfocused.
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        let new_content_area = Rect {
                            x: 0,
                            y: 1,
                            width: cols,
                            height: rows.saturating_sub(2),
                        };
                        last_content_area = new_content_area;
                        if active_tab == ActiveTab::Overview && !in_focus_mode {
                            overview_state.handle_resize(agents.len(), new_content_area);
                            overview_state.force_resize_all();
                        }
                        if in_focus_mode && focus_mode_state.is_active() {
                            let fm_cols =
                                (new_content_area.width * 40 / 100).saturating_sub(2).max(1);
                            let fm_rows = new_content_area.height.saturating_sub(3).max(1);
                            focus_mode_state.handle_resize(fm_cols, fm_rows);
                            focus_mode_state.force_resize();
                            let term_cols = (new_content_area.width * 60 / 100).max(1);
                            let term_rows = new_content_area.height.saturating_sub(2).max(1);
                            focus_mode_state.handle_terminal_resize(term_cols, term_rows);
                        }
                    }
                }
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row,
                    modifiers,
                }) if mouse_captured => {
                    let pos = Position { x: column, y: row };

                    // Ignore clicks while purge progress is shown
                    if purge_progress.is_some() {
                        // swallow
                    } else if modifiers.contains(KeyModifiers::SUPER) {
                        if let Some(url) = find_url_at_click(
                            pos,
                            &click_map,
                            &mut overview_state,
                            &mut focus_mode_state,
                            in_focus_mode,
                            active_tab,
                        ) {
                            terminal_emulator::open_url(&url);
                        }
                    } else if active_menu.is_some() {
                        if click_map.menu_inner_rect.contains(pos) {
                            let idx = (row - click_map.menu_inner_rect.y) as usize;
                            let item_count = if let Some(menu) = active_menu.as_ref() {
                                match menu {
                                    ActiveMenu::AgentPicker { menu, .. } => menu.items.len(),
                                    ActiveMenu::RepoActions { menu, .. } => menu.items.len(),
                                    ActiveMenu::ColorPicker { menu, .. } => menu.items.len(),
                                    ActiveMenu::BranchActions { menu, .. } => menu.items.len(),
                                    ActiveMenu::ConfirmAction { menu, .. } => menu.items.len(),
                                    ActiveMenu::WorktreeCleanup { menu, .. } => menu.items.len(),
                                    ActiveMenu::EditorPicker { menu, .. } => menu.items.len(),
                                    ActiveMenu::EditorRemember { menu, .. } => menu.items.len(),
                                }
                            } else {
                                0
                            };
                            if idx < item_count {
                                // Highlight the clicked item then select it
                                if let Some(menu) = active_menu.as_mut() {
                                    match menu {
                                        ActiveMenu::AgentPicker { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::RepoActions { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::ColorPicker { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::BranchActions { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::ConfirmAction { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::WorktreeCleanup { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::EditorPicker { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                        ActiveMenu::EditorRemember { menu, .. } => {
                                            menu.selected_idx = idx
                                        }
                                    }
                                }
                                let taken = active_menu.take().unwrap();
                                match taken {
                                    ActiveMenu::AgentPicker {
                                        agents: picker_agents,
                                        ..
                                    } => {
                                        if let Some(agent) = picker_agents.get(idx) {
                                            let agent_id = agent.id.clone();
                                            let agent_binary = agent.agent_binary.clone();
                                            let working_dir = agent.working_dir.clone();
                                            let fm_cols = (last_content_area.width * 40 / 100)
                                                .saturating_sub(2)
                                                .max(1);
                                            let fm_rows =
                                                last_content_area.height.saturating_sub(3).max(1);
                                            let existing_terminals =
                                                overview_state.take_agent_terminals(&agent_id);
                                            focus_mode_state.open_agent(
                                                &agent_id,
                                                &agent_binary,
                                                fm_cols,
                                                fm_rows,
                                                &working_dir,
                                                agent.repo_path.as_deref(),
                                                agent.branch_name.as_deref(),
                                                agent.is_worktree,
                                                existing_terminals,
                                            );
                                            in_focus_mode = true;
                                        }
                                    }
                                    ActiveMenu::RepoActions { repo_path, .. } => {
                                        match idx {
                                            0 => {
                                                let items: Vec<ContextMenuItem> =
                                                    theme::REPO_COLOR_NAMES
                                                        .iter()
                                                        .map(|&name| ContextMenuItem {
                                                            label: name[0..1].to_uppercase()
                                                                + &name[1..],
                                                            color: Some(theme::repo_color(name)),
                                                        })
                                                        .collect();
                                                active_menu = Some(ActiveMenu::ColorPicker {
                                                    repo_path,
                                                    menu: ContextMenu::with_colors(
                                                        "Choose Color",
                                                        items,
                                                    ),
                                                });
                                            }
                                            1 => {
                                                open_in_file_system(&repo_path);
                                            }
                                            2 => {
                                                open_in_terminal(&repo_path);
                                            }
                                            3 => {
                                                // Collect worktree agents for this repo before stopping
                                                let repo_agents: Vec<_> = agents
                                                    .iter()
                                                    .filter(|a| {
                                                        a.repo_path.as_deref() == Some(&*repo_path)
                                                    })
                                                    .cloned()
                                                    .collect();
                                                stop_repo_agents_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                let cleanups =
                                                    crate::worktree::collect_worktree_cleanups(
                                                        &repo_agents,
                                                        &agents,
                                                    );
                                                if !cleanups.is_empty() {
                                                    pending_worktree_cleanups = cleanups;
                                                    active_menu = pop_worktree_cleanup_menu(
                                                        &mut pending_worktree_cleanups,
                                                    );
                                                }
                                            }
                                            4 => {
                                                unregister_repo_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            5 => {
                                                clean_stale_refs_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            6 => {
                                                // "Detach"
                                                let tx = status_tx.clone();
                                                let rp = repo_path.clone();
                                                tokio::spawn(async move {
                                                    let mut stream = match ipc::try_connect().await
                                                    {
                                                        Ok(s) => s,
                                                        Err(e) => {
                                                            let _ = tx.send(StatusMessage {
                                                                    text: format!("Detach failed: hub connect error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            return;
                                                        }
                                                    };
                                                    let msg =
                                                        CliMessage::DetachHead { repo_path: rp };
                                                    if let Err(e) =
                                                        clust_ipc::send_message(&mut stream, &msg)
                                                            .await
                                                    {
                                                        let _ = tx.send(StatusMessage {
                                                            text: format!("Detach failed: send error: {e}"),
                                                            level: StatusLevel::Error,
                                                            created: Instant::now(),
                                                        }).await;
                                                        return;
                                                    }
                                                    match clust_ipc::recv_message::<HubMessage>(
                                                        &mut stream,
                                                    )
                                                    .await
                                                    {
                                                        Ok(HubMessage::HeadDetached) => {
                                                            let _ = tx
                                                                .send(StatusMessage {
                                                                    text: "HEAD detached"
                                                                        .to_string(),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                })
                                                                .await;
                                                        }
                                                        Ok(HubMessage::Error { message }) => {
                                                            let _ = tx
                                                                .send(StatusMessage {
                                                                    text: format!(
                                                                        "Detach failed: {message}"
                                                                    ),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                })
                                                                .await;
                                                        }
                                                        Ok(_) => {
                                                            let _ = tx.send(StatusMessage {
                                                                text: "Detach failed: unexpected hub response".to_string(),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                        }
                                                        Err(e) => {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Detach failed: recv error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                        }
                                                    }
                                                });
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            7 => {
                                                active_menu = Some(ActiveMenu::ConfirmAction {
                                                    action: ConfirmedAction::PurgeRepo {
                                                        repo_path,
                                                    },
                                                    menu: ContextMenu::new(
                                                        "Purge Repository",
                                                        vec![
                                                            "Confirm".to_string(),
                                                            "Cancel".to_string(),
                                                        ],
                                                    )
                                                    .with_description(
                                                        "This will stop all agents, delete all\nworktrees, and delete all local branches.".to_string(),
                                                    ),
                                                });
                                            }
                                            _ => {}
                                        }
                                    }
                                    ActiveMenu::ColorPicker { repo_path, .. } => {
                                        if let Some(&color_name) = theme::REPO_COLOR_NAMES.get(idx)
                                        {
                                            set_repo_color_ipc(&repo_path, color_name);
                                            last_repo_fetch =
                                                Instant::now() - Duration::from_secs(10);
                                        }
                                    }
                                    ActiveMenu::BranchActions {
                                        repo_path,
                                        branch_name,
                                        is_head,
                                        agents: branch_agents,
                                        actions,
                                        ..
                                    } => {
                                        if let Some(&action) = actions.get(idx) {
                                            match action {
                                                BranchAction::StartAgent if is_head => {
                                                    active_menu = Some(ActiveMenu::ConfirmAction {
                                                        action: ConfirmedAction::StartAgentDetach {
                                                            repo_path,
                                                            branch_name,
                                                        },
                                                        menu: ContextMenu::new(
                                                            "Detach HEAD",
                                                            vec![
                                                                "Confirm".to_string(),
                                                                "Cancel".to_string(),
                                                            ],
                                                        )
                                                        .with_description(
                                                            "This will detach HEAD in your repo.\nThe branch will be moved to a worktree for the agent.".to_string(),
                                                        ),
                                                    });
                                                    continue;
                                                }
                                                BranchAction::StartAgent => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    let bp = bypass_permissions;
                                                    let (cols, rows) = crossterm::terminal::size()
                                                        .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: hub connect error: {e}")
                                                                    )).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::CreateWorktreeAgent {
                                                            repo_path: rp,
                                                            target_branch: Some(bn),
                                                            new_branch: None,
                                                            prompt: None,
                                                            agent_binary: None,
                                                            cols,
                                                            rows: rows.saturating_sub(2).max(1),
                                                            accept_edits: false,
                                                            plan_mode: bp,
                                                            allow_bypass: bp,
                                                            hub,
                                                            auto_exit: false,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(AgentStartResult::Failed(
                                                                format!("Agent create failed: send error: {e}")
                                                            )).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(
                                                                HubMessage::WorktreeAgentStarted {
                                                                    id,
                                                                    agent_binary,
                                                                    working_dir,
                                                                    repo_path,
                                                                    branch_name,
                                                                },
                                                            ) => {
                                                                let _ = tx
                                                                    .send(
                                                                        AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree: true,
                                                                        },
                                                                    )
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: {message}")
                                                                )).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    "Agent create failed: unexpected hub response".to_string()
                                                                )).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: recv error: {e}")
                                                                )).await;
                                                            }
                                                        }
                                                    });
                                                }
                                                BranchAction::StartAgentInPlace => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let rp = repo_path.clone();
                                                    let bp = bypass_permissions;
                                                    let (cols, rows) = crossterm::terminal::size()
                                                        .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: hub connect error: {e}")
                                                                    )).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::StartAgent {
                                                            prompt: None,
                                                            agent_binary: None,
                                                            working_dir: rp,
                                                            cols,
                                                            rows: rows.saturating_sub(2).max(1),
                                                            accept_edits: false,
                                                            plan_mode: bp,
                                                            allow_bypass: bp,
                                                            hub,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(AgentStartResult::Failed(
                                                                format!("Agent create failed: send error: {e}")
                                                            )).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::AgentStarted {
                                                                id,
                                                                agent_binary,
                                                                is_worktree,
                                                                repo_path,
                                                                branch_name,
                                                            }) => {
                                                                let working_dir = repo_path
                                                                    .clone()
                                                                    .unwrap_or_default();
                                                                let _ = tx
                                                                    .send(
                                                                        AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree,
                                                                        },
                                                                    )
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: {message}")
                                                                )).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    "Agent create failed: unexpected hub response".to_string()
                                                                )).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: recv error: {e}")
                                                                )).await;
                                                            }
                                                        }
                                                    });
                                                }
                                                BranchAction::Pull => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Pull failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::PullBranch {
                                                            repo_path: rp,
                                                            branch_name: bn.clone(),
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Pull failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::BranchPulled {
                                                                branch_name,
                                                                ..
                                                            }) => {
                                                                let _ = tx
                                                                    .send(StatusMessage {
                                                                        text: format!(
                                                                            "Pulled {branch_name}"
                                                                        ),
                                                                        level: StatusLevel::Success,
                                                                        created: Instant::now(),
                                                                    })
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Pull failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Pull failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Pull failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::StopAgents => {
                                                    let ids: Vec<String> = branch_agents
                                                        .iter()
                                                        .map(|a| a.id.clone())
                                                        .collect();
                                                    stop_agents_ipc(&ids);
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    // Queue worktree cleanup if applicable
                                                    let cleanups =
                                                        crate::worktree::collect_worktree_cleanups(
                                                            &branch_agents,
                                                            &agents,
                                                        );
                                                    if !cleanups.is_empty() {
                                                        pending_worktree_cleanups = cleanups;
                                                        active_menu = pop_worktree_cleanup_menu(
                                                            &mut pending_worktree_cleanups,
                                                        );
                                                    }
                                                }
                                                BranchAction::OpenAgent => {
                                                    if branch_agents.len() == 1 {
                                                        let agent = &branch_agents[0];
                                                        let fm_cols =
                                                            (last_content_area.width * 40 / 100)
                                                                .saturating_sub(2)
                                                                .max(1);
                                                        let fm_rows = last_content_area
                                                            .height
                                                            .saturating_sub(3)
                                                            .max(1);
                                                        let existing_terminals = overview_state
                                                            .take_agent_terminals(&agent.id);
                                                        focus_mode_state.open_agent(
                                                            &agent.id,
                                                            &agent.agent_binary,
                                                            fm_cols,
                                                            fm_rows,
                                                            &agent.working_dir,
                                                            agent.repo_path.as_deref(),
                                                            agent.branch_name.as_deref(),
                                                            agent.is_worktree,
                                                            existing_terminals,
                                                        );
                                                        in_focus_mode = true;
                                                    } else if branch_agents.len() > 1 {
                                                        let labels: Vec<String> = branch_agents
                                                            .iter()
                                                            .map(|a| {
                                                                format!(
                                                                    "{} ({})",
                                                                    a.agent_binary, a.id
                                                                )
                                                            })
                                                            .collect();
                                                        active_menu =
                                                            Some(ActiveMenu::AgentPicker {
                                                                menu: ContextMenu::new(
                                                                    "Open Agent",
                                                                    labels,
                                                                ),
                                                                agents: branch_agents,
                                                            });
                                                    }
                                                }
                                                BranchAction::OpenInEditor => {
                                                    if open_branch_in_editor(
                                                        &repo_path,
                                                        &branch_name,
                                                        &repos,
                                                        &mut active_menu,
                                                        &editors_cache,
                                                        &mut status_message,
                                                    ) {
                                                        last_repo_fetch = Instant::now()
                                                            - Duration::from_secs(10);
                                                    }
                                                }
                                                BranchAction::RemoveWorktree => {
                                                    remove_worktree_ipc(
                                                        &repo_path,
                                                        &branch_name,
                                                        false,
                                                        false,
                                                    );
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::DeleteBranch => {
                                                    delete_local_branch_ipc(
                                                        &repo_path,
                                                        &branch_name,
                                                    );
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::BaseWorktreeOff => {
                                                    if create_modal.is_some() {
                                                        // Modal already open — skip to avoid
                                                        // stacking. (Caller should close the
                                                        // existing modal first.)
                                                    } else if let Some(repo_info) = repos
                                                        .iter()
                                                        .find(|r| r.path == repo_path)
                                                        .cloned()
                                                    {
                                                        let modal =
                                                            CreateAgentModal::new_with_branch(
                                                                repos.clone(),
                                                                repo_info,
                                                                branch_name.clone(),
                                                            );
                                                        create_modal = Some(modal);
                                                    }
                                                }
                                                BranchAction::DetachHead => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Detach failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::DetachHead {
                                                            repo_path: rp,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Detach failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::HeadDetached) => {
                                                                let _ = tx
                                                                    .send(StatusMessage {
                                                                        text: "HEAD detached"
                                                                            .to_string(),
                                                                        level: StatusLevel::Success,
                                                                        created: Instant::now(),
                                                                    })
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Detach failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Detach failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Detach failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::CheckoutLocal => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Checkout failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::CheckoutLocalBranch {
                                                            repo_path: rp,
                                                            branch_name: bn.clone(),
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Checkout failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(
                                                                HubMessage::LocalBranchCheckedOut {
                                                                    branch_name,
                                                                },
                                                            ) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checked out {branch_name}"),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Checkout failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::RemoteStartAgent => {
                                                    if let Some(local) =
                                                        branch_name.split_once('/').map(|x| x.1)
                                                    {
                                                        let tx = agent_start_tx.clone();
                                                        let hub = hub_name.to_string();
                                                        let rp = repo_path.clone();
                                                        let remote_ref = branch_name.clone();
                                                        let local_name = local.to_string();
                                                        let bp = bypass_permissions;
                                                        let (cols, rows) =
                                                            crossterm::terminal::size()
                                                                .unwrap_or((80, 24));
                                                        tokio::spawn(async move {
                                                            let mut stream = match ipc::try_connect(
                                                            )
                                                            .await
                                                            {
                                                                Ok(s) => s,
                                                                Err(e) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                            format!("Agent create failed: hub connect error: {e}")
                                                                        )).await;
                                                                    return;
                                                                }
                                                            };
                                                            let msg =
                                                                CliMessage::CreateWorktreeAgent {
                                                                    repo_path: rp,
                                                                    target_branch: Some(remote_ref),
                                                                    new_branch: Some(local_name),
                                                                    prompt: None,
                                                                    agent_binary: None,
                                                                    cols,
                                                                    rows: rows
                                                                        .saturating_sub(2)
                                                                        .max(1),
                                                                    accept_edits: false,
                                                                    plan_mode: bp,
                                                                    allow_bypass: bp,
                                                                    hub,
                                                                    auto_exit: false,
                                                                };
                                                            if let Err(e) = clust_ipc::send_message(
                                                                &mut stream,
                                                                &msg,
                                                            )
                                                            .await
                                                            {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: send error: {e}")
                                                                )).await;
                                                                return;
                                                            }
                                                            match clust_ipc::recv_message::<HubMessage>(
                                                                &mut stream,
                                                            )
                                                            .await
                                                            {
                                                                Ok(HubMessage::WorktreeAgentStarted {
                                                                    id,
                                                                    agent_binary,
                                                                    working_dir,
                                                                    repo_path,
                                                                    branch_name,
                                                                }) => {
                                                                    let _ = tx
                                                                        .send(AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree: true,
                                                                        })
                                                                        .await;
                                                                }
                                                                Ok(HubMessage::Error { message }) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: {message}")
                                                                    )).await;
                                                                }
                                                                Ok(_) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                        "Agent create failed: unexpected hub response".to_string()
                                                                    )).await;
                                                                }
                                                                Err(e) => {
                                                                    let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: recv error: {e}")
                                                                    )).await;
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                                BranchAction::RemoteCreateWorktree => {
                                                    if let Some(local) =
                                                        branch_name.split_once('/').map(|x| x.1)
                                                    {
                                                        add_worktree_ipc(
                                                            &repo_path,
                                                            local,
                                                            &branch_name,
                                                        );
                                                        last_repo_fetch = Instant::now()
                                                            - Duration::from_secs(10);
                                                    }
                                                }
                                                BranchAction::DeleteRemoteBranch => {
                                                    delete_remote_branch_ipc(
                                                        &repo_path,
                                                        &branch_name,
                                                    );
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                BranchAction::CheckoutRemote => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                        text: format!("Checkout failed: hub connect error: {e}"),
                                                                        level: StatusLevel::Error,
                                                                        created: Instant::now(),
                                                                    }).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg =
                                                            CliMessage::CheckoutRemoteBranch {
                                                                working_dir: Some(rp),
                                                                repo_name: None,
                                                                remote_branch: bn.clone(),
                                                            };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(StatusMessage {
                                                                text: format!("Checkout failed: send error: {e}"),
                                                                level: StatusLevel::Error,
                                                                created: Instant::now(),
                                                            }).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(HubMessage::RemoteBranchCheckedOut { branch_name }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checked out {branch_name}"),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: {message}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: "Checkout failed: unexpected hub response".to_string(),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Checkout failed: recv error: {e}"),
                                                                    level: StatusLevel::Error,
                                                                    created: Instant::now(),
                                                                }).await;
                                                            }
                                                        }
                                                    });
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                            }
                                        }
                                    }
                                    ActiveMenu::ConfirmAction { action, .. } => {
                                        if idx == 0 {
                                            match action {
                                                ConfirmedAction::PurgeRepo { repo_path } => {
                                                    purge_progress =
                                                        Some(start_purge_async(&repo_path));
                                                }
                                                ConfirmedAction::RemoveRepository { repo_path } => {
                                                    unregister_repo_ipc(&repo_path);
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                ConfirmedAction::DeleteRepository { repo_path } => {
                                                    delete_repo_ipc(&repo_path, status_tx.clone());
                                                    last_repo_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                    last_agent_fetch =
                                                        Instant::now() - Duration::from_secs(10);
                                                }
                                                ConfirmedAction::DeleteScheduledTask { task_id } => {
                                                    let refresh_tx = scheduled_task_tx.clone();
                                                    tokio::spawn(async move {
                                                        let _ = ipc::send_one_shot(
                                                            CliMessage::DeleteScheduledTask {
                                                                id: task_id,
                                                            },
                                                        )
                                                        .await;
                                                        let tasks =
                                                            ipc::fetch_scheduled_tasks().await;
                                                        let _ = refresh_tx.send(tasks).await;
                                                    });
                                                }
                                                ConfirmedAction::ClearScheduledTasksByStatus {
                                                    status,
                                                } => {
                                                    let refresh_tx = scheduled_task_tx.clone();
                                                    tokio::spawn(async move {
                                                        let _ = ipc::send_one_shot(
                                                            CliMessage::DeleteScheduledTasksByStatus {
                                                                status,
                                                            },
                                                        )
                                                        .await;
                                                        let tasks =
                                                            ipc::fetch_scheduled_tasks().await;
                                                        let _ = refresh_tx.send(tasks).await;
                                                    });
                                                }
                                                ConfirmedAction::StartAgentDetach {
                                                    repo_path,
                                                    branch_name,
                                                } => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let bp = bypass_permissions;
                                                    let (cols, rows) = crossterm::terminal::size()
                                                        .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream = match ipc::try_connect()
                                                            .await
                                                        {
                                                            Ok(s) => s,
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                        format!("Agent create failed: hub connect error: {e}")
                                                                    )).await;
                                                                return;
                                                            }
                                                        };
                                                        let msg = CliMessage::CreateWorktreeAgent {
                                                            repo_path,
                                                            target_branch: Some(branch_name),
                                                            new_branch: None,
                                                            prompt: None,
                                                            agent_binary: None,
                                                            cols,
                                                            rows: rows.saturating_sub(2).max(1),
                                                            accept_edits: false,
                                                            plan_mode: bp,
                                                            allow_bypass: bp,
                                                            hub,
                                                            auto_exit: false,
                                                        };
                                                        if let Err(e) = clust_ipc::send_message(
                                                            &mut stream,
                                                            &msg,
                                                        )
                                                        .await
                                                        {
                                                            let _ = tx.send(AgentStartResult::Failed(
                                                                format!("Agent create failed: send error: {e}")
                                                            )).await;
                                                            return;
                                                        }
                                                        match clust_ipc::recv_message::<HubMessage>(
                                                            &mut stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(
                                                                HubMessage::WorktreeAgentStarted {
                                                                    id,
                                                                    agent_binary,
                                                                    working_dir,
                                                                    repo_path,
                                                                    branch_name,
                                                                },
                                                            ) => {
                                                                let _ = tx
                                                                    .send(
                                                                        AgentStartResult::Started {
                                                                            agent_id: id,
                                                                            agent_binary,
                                                                            working_dir,
                                                                            repo_path,
                                                                            branch_name,
                                                                            is_worktree: true,
                                                                        },
                                                                    )
                                                                    .await;
                                                            }
                                                            Ok(HubMessage::Error { message }) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: {message}")
                                                                )).await;
                                                            }
                                                            Ok(_) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    "Agent create failed: unexpected hub response".to_string()
                                                                )).await;
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.send(AgentStartResult::Failed(
                                                                    format!("Agent create failed: recv error: {e}")
                                                                )).await;
                                                            }
                                                        }
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    ActiveMenu::WorktreeCleanup {
                                        repo_path,
                                        branch_name,
                                        ..
                                    } => {
                                        match idx {
                                            1 => {
                                                remove_worktree_ipc(
                                                    &repo_path,
                                                    &branch_name,
                                                    false,
                                                    true,
                                                );
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            2 => {
                                                remove_worktree_ipc(
                                                    &repo_path,
                                                    &branch_name,
                                                    true,
                                                    true,
                                                );
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            _ => {}
                                        }
                                        if let Some(next) = pending_worktree_cleanups.pop() {
                                            let dirty = crate::worktree::is_worktree_dirty(
                                                &next.repo_path,
                                                &next.branch_name,
                                            );
                                            let title = if dirty {
                                                format!(
                                                    "Worktree '{}' (uncommitted changes)",
                                                    next.branch_name
                                                )
                                            } else {
                                                format!("Worktree '{}'", next.branch_name)
                                            };
                                            active_menu = Some(ActiveMenu::WorktreeCleanup {
                                                repo_path: next.repo_path,
                                                branch_name: next.branch_name,
                                                menu: ContextMenu::new(
                                                    &title,
                                                    vec![
                                                        "Keep".to_string(),
                                                        "Discard worktree".to_string(),
                                                        "Discard worktree + branch".to_string(),
                                                    ],
                                                ),
                                            });
                                        }
                                    }
                                    ActiveMenu::EditorPicker {
                                        target_path,
                                        repo_path,
                                        editors,
                                        ..
                                    } => {
                                        if let Some(editor) = editors.get(idx).cloned() {
                                            crate::editor::open_in_editor(&editor, &target_path);
                                            if repo_path.is_some() {
                                                active_menu = Some(ActiveMenu::EditorRemember {
                                                    repo_path,
                                                    editor,
                                                    menu: ContextMenu::new(
                                                        "Remember this editor?",
                                                        vec![
                                                            "Just this time".to_string(),
                                                            "For this repository".to_string(),
                                                            "For all repositories".to_string(),
                                                        ],
                                                    ),
                                                });
                                            }
                                        }
                                    }
                                    ActiveMenu::EditorRemember {
                                        repo_path, editor, ..
                                    } => match idx {
                                        1 => {
                                            if let Some(rp) = repo_path {
                                                set_repo_editor_ipc(&rp, &editor.binary);
                                                if let Some(repo) =
                                                    repos.iter_mut().find(|r| r.path == rp)
                                                {
                                                    repo.editor = Some(editor.binary);
                                                }
                                            }
                                        }
                                        2 => {
                                            set_default_editor_ipc(&editor.binary);
                                        }
                                        _ => {}
                                    },
                                }
                            }
                        } else if !click_map.menu_modal_rect.contains(pos) {
                            // Click outside modal → dismiss
                            active_menu = None;
                        }
                    } else if in_focus_mode {
                        // Focus mode click handling
                        if click_map.focus_back_button.contains(pos) {
                            if active_tab == ActiveTab::Overview
                                && overview_state.initialized
                            {
                                overview_state.force_resize_all();
                            }
                            if let Some((aid, cache)) = focus_mode_state.detach() {
                                overview_state.store_agent_terminals(aid, cache);
                            } else {
                                // detach() returned None: nothing new to cache.
                                // Any previously-stored cache is preserved.
                            }
                            in_focus_mode = false;
                        } else if focus_mode_state.repo_path.is_some() {
                            // Per-terminal label strip + content area: clicks
                            // here are checked before the generic left-area
                            // handler so they don't get swallowed by it. Only
                            // active when the Terminal tab is the visible one.
                            let in_terminal_tab =
                                focus_mode_state.left_tab == overview::LeftPanelTab::Terminal;
                            let label_hit = if in_terminal_tab {
                                click_map
                                    .focus_terminal_labels
                                    .iter()
                                    .find(|(r, _)| r.contains(pos))
                                    .map(|(_, idx)| *idx)
                            } else {
                                None
                            };
                            if in_terminal_tab && click_map.focus_terminal_new_button.contains(pos)
                            {
                                focus_mode_state.add_terminal();
                                focus_mode_state.focus_side = overview::FocusSide::Left;
                                focus_mode_state.terminal_input_focused = true;
                            } else if let Some(idx) = label_hit {
                                focus_mode_state.select_terminal(idx);
                                focus_mode_state.focus_side = overview::FocusSide::Left;
                            } else if in_terminal_tab
                                && click_map.focus_terminal_content_area.contains(pos)
                            {
                                // Click into the active terminal area = enter Type mode.
                                focus_mode_state.focus_side = overview::FocusSide::Left;
                                focus_mode_state.terminal_input_focused = true;
                            } else if let Some((_, tab)) = click_map
                                .focus_left_tabs
                                .iter()
                                .find(|(r, _)| r.contains(pos))
                            {
                                focus_mode_state.left_tab = *tab;
                                focus_mode_state.focus_side = overview::FocusSide::Left;
                                // Switching to a non-Terminal tab leaves Type mode.
                                if *tab != overview::LeftPanelTab::Terminal {
                                    focus_mode_state.terminal_input_focused = false;
                                }
                            } else if click_map.focus_left_area.contains(pos) {
                                focus_mode_state.focus_side = overview::FocusSide::Left;
                            } else if click_map.focus_right_area.contains(pos) {
                                focus_mode_state.focus_side = overview::FocusSide::Right;
                                focus_mode_state.terminal_input_focused = false;
                            }
                        } else if click_map.focus_right_area.contains(pos) {
                            focus_mode_state.focus_side = overview::FocusSide::Right;
                            focus_mode_state.terminal_input_focused = false;
                        }
                    } else if let Some((_, tab)) =
                        click_map.tabs.iter().find(|(r, _)| r.contains(pos))
                    {
                        // Tab bar clicks
                        active_tab = *tab;
                    } else {
                        match active_tab {
                            ActiveTab::Repositories => {
                                // Tree item clicks (left panel)
                                if click_map.tree_inner_area.contains(pos) {
                                    let line_idx = (row - click_map.tree_inner_area.y) as usize;
                                    if let Some(target) = click_map.tree_items.get(line_idx) {
                                        match target {
                                            TreeClickTarget::Repo(ri) => {
                                                if selection.level == TreeLevel::Repo
                                                    && selection.repo_idx == *ri
                                                {
                                                    selection.toggle_collapse();
                                                } else {
                                                    selection.repo_idx = *ri;
                                                    selection.level = TreeLevel::Repo;
                                                }
                                            }
                                            TreeClickTarget::Category(ri, ci) => {
                                                if selection.level == TreeLevel::Category
                                                    && selection.repo_idx == *ri
                                                    && selection.category_idx == *ci
                                                {
                                                    selection.toggle_collapse();
                                                } else {
                                                    selection.repo_idx = *ri;
                                                    selection.category_idx = *ci;
                                                    selection.level = TreeLevel::Category;
                                                }
                                            }
                                            TreeClickTarget::Branch(ri, ci, bi) => {
                                                selection.repo_idx = *ri;
                                                selection.category_idx = *ci;
                                                selection.branch_idx = *bi;
                                                selection.level = TreeLevel::Branch;
                                            }
                                        }
                                    }
                                }
                            }
                            ActiveTab::Overview => {
                                // Agent indicator clicks → focus that agent
                                if let Some((_, global_idx)) = click_map
                                    .overview_agent_indicators
                                    .iter()
                                    .find(|(r, _)| r.contains(pos))
                                {
                                    let idx = *global_idx;
                                    overview_state.focus = overview::OverviewFocus::Terminal(idx);
                                    overview_state.last_terminal_idx = idx;
                                    overview_state.ensure_visible_sorted(idx);
                                }
                                // Repo button clicks → toggle collapse
                                else if let Some((_, repo_path)) = click_map
                                    .overview_repo_buttons
                                    .iter()
                                    .find(|(r, _)| r.contains(pos))
                                {
                                    let rp = repo_path.clone();
                                    if overview_state.collapsed_repos.contains(&rp) {
                                        overview_state.collapsed_repos.remove(&rp);
                                    } else {
                                        overview_state.collapsed_repos.insert(rp);
                                    }
                                }
                                // Panel clicks
                                else if let Some((_, idx)) = click_map
                                    .overview_panels
                                    .iter()
                                    .find(|(r, _)| r.contains(pos))
                                {
                                    overview_state.focus = overview::OverviewFocus::Terminal(*idx);
                                }
                            }
                            ActiveTab::Schedule => {
                                // No mouse interactions on the Schedule tab
                                // yet — keys-only.
                            }
                        }
                    }
                }
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column,
                    row,
                    ..
                }) if mouse_captured => {
                    let pos = Position { x: column, y: row };
                    if let Some(ref mut menu_variant) = active_menu {
                        match menu_variant {
                            ActiveMenu::AgentPicker { menu, .. }
                            | ActiveMenu::RepoActions { menu, .. }
                            | ActiveMenu::ColorPicker { menu, .. }
                            | ActiveMenu::BranchActions { menu, .. }
                            | ActiveMenu::ConfirmAction { menu, .. }
                            | ActiveMenu::WorktreeCleanup { menu, .. }
                            | ActiveMenu::EditorPicker { menu, .. }
                            | ActiveMenu::EditorRemember { menu, .. } => {
                                menu.selected_idx = menu.selected_idx.saturating_sub(1);
                            }
                        }
                    } else if in_focus_mode && focus_mode_state.is_active() {
                        let on_terminal_label_strip = focus_mode_state.left_tab
                            == overview::LeftPanelTab::Terminal
                            && (click_map
                                .focus_terminal_labels
                                .iter()
                                .any(|(r, _)| r.contains(pos))
                                || click_map.focus_terminal_new_button.contains(pos));
                        if on_terminal_label_strip {
                            // Scroll wheel over the label strip cycles terminals.
                            focus_mode_state.prev_terminal();
                        } else if click_map.focus_right_area.contains(pos) {
                            if let Some(panel) = &mut focus_mode_state.panel {
                                let max = panel.vterm.scrollback_len();
                                panel.panel_scroll_offset =
                                    (panel.panel_scroll_offset + 3).min(max);
                            }
                        } else if click_map.focus_left_area.contains(pos) {
                            match focus_mode_state.left_tab {
                                overview::LeftPanelTab::Terminal => {
                                    if let Some(panel) = focus_mode_state.current_terminal_mut() {
                                        let max = panel.vterm.scrollback_len();
                                        panel.scroll_offset = (panel.scroll_offset + 3).min(max);
                                    }
                                }
                                overview::LeftPanelTab::Compare => {
                                    focus_mode_state.compare_scroll_up()
                                }
                                _ => focus_mode_state.diff_scroll_up(),
                            }
                        }
                    } else if active_tab == ActiveTab::Overview {
                        if let Some((_, idx)) = click_map
                            .overview_panels
                            .iter()
                            .find(|(r, _)| r.contains(pos))
                        {
                            if let Some(panel) = overview_state.panels.get_mut(*idx) {
                                let max = panel.vterm.scrollback_len();
                                panel.panel_scroll_offset =
                                    (panel.panel_scroll_offset + 3).min(max);
                            }
                        }
                    }
                }
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column,
                    row,
                    ..
                }) if mouse_captured => {
                    let pos = Position { x: column, y: row };
                    if let Some(ref mut menu_variant) = active_menu {
                        match menu_variant {
                            ActiveMenu::AgentPicker { menu, .. }
                            | ActiveMenu::RepoActions { menu, .. }
                            | ActiveMenu::ColorPicker { menu, .. }
                            | ActiveMenu::BranchActions { menu, .. }
                            | ActiveMenu::ConfirmAction { menu, .. }
                            | ActiveMenu::WorktreeCleanup { menu, .. }
                            | ActiveMenu::EditorPicker { menu, .. }
                            | ActiveMenu::EditorRemember { menu, .. } => {
                                if !menu.items.is_empty() {
                                    menu.selected_idx =
                                        (menu.selected_idx + 1).min(menu.items.len() - 1);
                                }
                            }
                        }
                    } else if in_focus_mode && focus_mode_state.is_active() {
                        let on_terminal_label_strip = focus_mode_state.left_tab
                            == overview::LeftPanelTab::Terminal
                            && (click_map
                                .focus_terminal_labels
                                .iter()
                                .any(|(r, _)| r.contains(pos))
                                || click_map.focus_terminal_new_button.contains(pos));
                        if on_terminal_label_strip {
                            // Scroll wheel over the label strip cycles terminals.
                            focus_mode_state.next_terminal();
                        } else if click_map.focus_right_area.contains(pos) {
                            if let Some(panel) = &mut focus_mode_state.panel {
                                panel.panel_scroll_offset =
                                    panel.panel_scroll_offset.saturating_sub(3);
                            }
                        } else if click_map.focus_left_area.contains(pos) {
                            match focus_mode_state.left_tab {
                                overview::LeftPanelTab::Terminal => {
                                    if let Some(panel) = focus_mode_state.current_terminal_mut() {
                                        panel.scroll_offset = panel.scroll_offset.saturating_sub(3);
                                    }
                                }
                                overview::LeftPanelTab::Compare => {
                                    focus_mode_state.compare_scroll_down()
                                }
                                _ => focus_mode_state.diff_scroll_down(),
                            }
                        }
                    } else if active_tab == ActiveTab::Overview {
                        if let Some((_, idx)) = click_map
                            .overview_panels
                            .iter()
                            .find(|(r, _)| r.contains(pos))
                        {
                            if let Some(panel) = overview_state.panels.get_mut(*idx) {
                                panel.panel_scroll_offset =
                                    panel.panel_scroll_offset.saturating_sub(3);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Clean up connections before exiting
    overview_state.shutdown();
    focus_mode_state.shutdown();

    if kbd_enhanced {
        let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    }
    io::stdout().execute(DisableFocusChange)?;
    io::stdout().execute(DisableBracketedPaste)?;
    io::stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    println!();

    if hub_stopped {
        let label = if hub_count > 1 { "hubs" } else { "hub" };
        println!(
            "\n  {}{label} stopped{}\n",
            theme::TEXT_SECONDARY,
            theme::RESET
        );
        crate::worktree::prompt_worktree_cleanup(&worktree_cleanups);
    }

    if let Some(ref msg) = *update_notice.lock().unwrap() {
        println!("  {}{msg}{}\n", theme::WARNING, theme::RESET);
    }

    Ok(())
}

/// Check whether a click position falls on a URL inside a terminal panel.
fn find_url_at_click(
    pos: Position,
    click_map: &ClickMap,
    overview_state: &mut OverviewState,
    focus_mode_state: &mut overview::FocusModeState,
    in_focus_mode: bool,
    active_tab: ActiveTab,
) -> Option<String> {
    if in_focus_mode {
        if click_map.focus_right_content_area.contains(pos) {
            let panel = focus_mode_state.panel.as_mut()?;
            let term_row = pos.y.checked_sub(click_map.focus_right_content_area.y)?;
            let term_col = pos.x.checked_sub(click_map.focus_right_content_area.x)?;
            return panel.vterm.url_at_position_scrolled(
                term_row,
                term_col,
                panel.panel_scroll_offset,
            );
        }
    } else if active_tab == ActiveTab::Overview {
        for &(area, idx) in &click_map.overview_content_areas {
            if area.contains(pos) {
                let panel = overview_state.panels.get_mut(idx)?;
                let term_row = pos.y.checked_sub(area.y)?;
                let term_col = pos.x.checked_sub(area.x)?;
                return panel.vterm.url_at_position_scrolled(
                    term_row,
                    term_col,
                    panel.panel_scroll_offset,
                );
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Rendering functions
// ---------------------------------------------------------------------------

fn render_tab_bar(frame: &mut Frame, area: Rect, active_tab: ActiveTab, click_map: &mut ClickMap) {
    let tabs = [
        ActiveTab::Repositories,
        ActiveTab::Overview,
        ActiveTab::Schedule,
    ];
    let mut spans = Vec::new();
    let mut cursor_x = area.x;

    spans.push(Span::styled(" ", Style::default().bg(theme::R_BG_RAISED)));
    cursor_x += 1;

    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                " │ ",
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_RAISED),
            ));
            cursor_x += 3;
        }

        let (fg, bg) = if *tab == active_tab {
            (theme::R_ACCENT_BRIGHT, theme::R_BG_OVERLAY)
        } else {
            (theme::R_TEXT_SECONDARY, theme::R_BG_RAISED)
        };

        let label = format!(" {} ", tab.label());
        let label_width = label.chars().count() as u16;
        click_map.tabs.push((
            Rect {
                x: cursor_x,
                y: area.y,
                width: label_width,
                height: 1,
            },
            *tab,
        ));
        cursor_x += label_width;

        spans.push(Span::styled(label, Style::default().fg(fg).bg(bg)));
    }

    // Tab switching hint
    spans.push(Span::styled(
        "  Tab/Shift+Tab",
        Style::default()
            .fg(theme::R_TEXT_TERTIARY)
            .bg(theme::R_BG_RAISED),
    ));

    // Fill remaining width with background
    let content_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(content_width);
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(theme::R_BG_RAISED),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_focus_back_bar(
    frame: &mut Frame,
    area: Rect,
    state: &overview::FocusModeState,
    origin_tab: ActiveTab,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
) {
    let bg = Style::default().bg(theme::R_BG_RAISED);
    let mut spans = Vec::new();
    let mut cursor_x = area.x;

    // Left: back indicator
    spans.push(Span::styled(
        " \u{2190} ",
        Style::default()
            .fg(theme::R_ACCENT_BRIGHT)
            .bg(theme::R_BG_RAISED),
    ));
    spans.push(Span::styled(
        "Shift+\u{2191}",
        Style::default()
            .fg(theme::R_TEXT_PRIMARY)
            .bg(theme::R_BG_RAISED)
            .add_modifier(Modifier::BOLD),
    ));
    let back_label = format!("  Back to {}", origin_tab.label());
    spans.push(Span::styled(
        &back_label,
        Style::default()
            .fg(theme::R_TEXT_SECONDARY)
            .bg(theme::R_BG_RAISED),
    ));

    // Record the entire back button region (arrow + Esc + label)
    let back_width: u16 = spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>() as u16;
    click_map.focus_back_button = Rect {
        x: cursor_x,
        y: area.y,
        width: back_width,
        height: 1,
    };
    cursor_x += back_width;
    let _ = cursor_x; // suppress unused warning

    // Center: agent info
    if let Some(panel) = &state.panel {
        spans.push(Span::styled("    ", bg));
        spans.push(Span::styled(
            panel.id.clone(),
            Style::default()
                .fg(theme::R_TEXT_PRIMARY)
                .bg(theme::R_BG_RAISED),
        ));
        spans.push(Span::styled(
            format!(" \u{00b7} {}", panel.agent_binary),
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ));

        // Repo name (colored) and branch
        if let Some(ref repo_path) = state.repo_path {
            let repo_display = std::path::Path::new(repo_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| repo_path.clone());
            let repo_clr = repo_colors
                .get(repo_path.as_str())
                .map(|c| theme::repo_color(c))
                .unwrap_or(theme::R_ACCENT);
            spans.push(Span::styled(
                " \u{00b7} ",
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_RAISED),
            ));
            spans.push(Span::styled(
                repo_display,
                Style::default().fg(repo_clr).bg(theme::R_BG_RAISED),
            ));
        }
        if let Some(ref branch) = panel.branch_name {
            spans.push(Span::styled(
                format!("/{branch}"),
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_RAISED),
            ));
        }
    }

    // Right: keyboard hints (only for repo agents)
    if state.repo_path.is_some() {
        let hints = "Shift+\u{2190}/\u{2192} panels  Shift+\u{2191}/\u{2193} jump file";
        let left_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let hints_width = hints.chars().count();
        let gap = (area.width as usize)
            .saturating_sub(left_width)
            .saturating_sub(hints_width)
            .saturating_sub(1); // trailing space
        if gap > 0 {
            spans.push(Span::styled(" ".repeat(gap), bg));
        }
        spans.push(Span::styled(
            hints,
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ));
    }

    // Fill remaining
    let total_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(total_width);
    if remaining > 0 {
        spans.push(Span::styled(" ".repeat(remaining), bg));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_divider(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::R_BG_RAISED)),
        area,
    );
}

fn render_left_panel(
    frame: &mut Frame,
    area: Rect,
    repos: &[RepoInfo],
    selection: &TreeSelection,
    focused: bool,
    click_map: &mut ClickMap,
) {
    let block = Block::default()
        .style(Style::default().bg(theme::R_BG_SURFACE))
        .padding(Padding::new(2, 2, 1, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner into title row, spacer, and tree area
    let [title_area, _spacer, tree_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    // Title + focus indicator on the same row
    let title = Paragraph::new(Span::styled(
        "Repositories",
        Style::default().fg(theme::R_TEXT_TERTIARY),
    ));
    frame.render_widget(title, title_area);

    let indicator_color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_TERTIARY
    };
    let indicator = Paragraph::new(Span::styled(
        "●",
        Style::default().fg(indicator_color).bg(theme::R_BG_SURFACE),
    ))
    .alignment(Alignment::Right);
    frame.render_widget(indicator, title_area);

    if repos.is_empty() {
        let text = Paragraph::new(Line::from(Span::styled(
            "No repositories found",
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )))
        .alignment(Alignment::Center);

        let [centered] = Layout::vertical([Constraint::Length(1)])
            .flex(Flex::Center)
            .areas(tree_area);

        frame.render_widget(text, centered);
    } else {
        let (lines, targets) = build_repo_tree_lines(repos, selection, tree_area.width);
        click_map.tree_inner_area = tree_area;
        click_map.tree_items = targets;
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, tree_area);
    }
}

fn build_repo_tree_lines(
    repos: &[RepoInfo],
    selection: &TreeSelection,
    width: u16,
) -> (Vec<Line<'static>>, Vec<TreeClickTarget>) {
    let mut lines = Vec::new();
    let mut targets = Vec::new();

    for (repo_idx, repo) in repos.iter().enumerate() {
        let is_this_repo = repo_idx == selection.repo_idx;
        let repo_selected = is_this_repo && selection.level == TreeLevel::Repo;
        let repo_collapsed = selection.is_repo_collapsed(repo_idx);

        // Empty line spacer between repos
        if repo_idx > 0 {
            lines.push(Line::from(""));
            targets.push(TreeClickTarget::Repo(repo_idx));
        }

        // "Add Repository" action entry — rendered as a distinct button-like row
        if repo.path == ADD_REPO_SENTINEL {
            let (bg, plus_fg, name_fg) = if repo_selected {
                (
                    Some(theme::R_BG_HOVER),
                    theme::R_ACCENT_BRIGHT,
                    theme::R_TEXT_PRIMARY,
                )
            } else {
                (None, theme::R_TEXT_TERTIARY, theme::R_TEXT_SECONDARY)
            };
            let mut spans = Vec::new();
            let mut plus_style = Style::default().fg(plus_fg);
            if let Some(bg_color) = bg {
                plus_style = plus_style.bg(bg_color);
            }
            spans.push(Span::styled(" + ", plus_style));
            let mut name_style = Style::default().fg(name_fg);
            if let Some(bg_color) = bg {
                name_style = name_style.bg(bg_color);
            }
            spans.push(Span::styled(repo.name.clone(), name_style));
            if repo_selected {
                spans.push(Span::styled(
                    "  Enter",
                    Style::default()
                        .fg(theme::R_TEXT_TERTIARY)
                        .bg(theme::R_BG_HOVER),
                ));
            }
            lines.push(pad_line(spans, width, bg));
            targets.push(TreeClickTarget::Repo(repo_idx));
            continue;
        }

        // Repo name header with collapse chevron
        let chevron = if repo_collapsed { "▸" } else { "▾" };
        let repo_clr = repo
            .color
            .as_deref()
            .map(theme::repo_color)
            .unwrap_or(theme::R_ACCENT);

        // Selected: hover bg with colored text; otherwise: reverse-video (repo color bg, dark text)
        let (line_bg, chev_fg, dot_fg, name_fg) = if repo_selected {
            (
                Some(theme::R_BG_HOVER),
                theme::R_TEXT_TERTIARY,
                repo_clr,
                repo_clr,
            )
        } else {
            (
                Some(repo_clr),
                theme::R_BG_BASE,
                theme::R_BG_BASE,
                theme::R_BG_BASE,
            )
        };

        let mut spans = Vec::new();
        let mut chev_style = Style::default().fg(chev_fg);
        if let Some(bg_color) = line_bg {
            chev_style = chev_style.bg(bg_color);
        }
        spans.push(Span::styled(format!(" {chevron} "), chev_style));
        let mut dot_style = Style::default().fg(dot_fg);
        if let Some(bg_color) = line_bg {
            dot_style = dot_style.bg(bg_color);
        }
        spans.push(Span::styled("● ", dot_style));
        let mut name_style = Style::default().fg(name_fg).add_modifier(Modifier::BOLD);
        if let Some(bg_color) = line_bg {
            name_style = name_style.bg(bg_color);
        }
        spans.push(Span::styled(repo.name.clone(), name_style));
        if repo_selected && !repo.path.is_empty() {
            spans.push(Span::styled(
                "  Enter",
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_HOVER),
            ));
            spans.push(Span::styled(
                format!("  {} open", editor_key_label()),
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_HOVER),
            ));
        }
        lines.push(pad_line(spans, width, line_bg));
        targets.push(TreeClickTarget::Repo(repo_idx));

        // Skip children if repo is collapsed
        if repo_collapsed {
            continue;
        }

        // Synthetic "No Repository" entry: show agents directly, no categories
        if repo.path.is_empty() {
            for (i, agent) in repo.local_branches.iter().enumerate() {
                let is_last = i == repo.local_branches.len() - 1;
                let connector = if is_last { "└──" } else { "├──" };
                let branch_selected = is_this_repo
                    && selection.level == TreeLevel::Branch
                    && i == selection.branch_idx;
                let bg = if branch_selected {
                    Some(theme::R_BG_HOVER)
                } else {
                    None
                };
                let indicator = if branch_selected { "▸ " } else { "  " };
                let mut prefix_style = Style::default().fg(theme::R_TEXT_TERTIARY);
                if let Some(bg_color) = bg {
                    prefix_style = prefix_style.bg(bg_color);
                }
                let mut spans = vec![Span::styled(
                    format!("   {connector} {indicator}"),
                    prefix_style,
                )];
                // Active indicator
                let mut dot_style = Style::default().fg(theme::R_SUCCESS);
                if let Some(bg_color) = bg {
                    dot_style = dot_style.bg(bg_color);
                }
                spans.push(Span::styled("● ", dot_style));
                // Agent name
                let name_color = if branch_selected {
                    theme::R_ACCENT_BRIGHT
                } else {
                    theme::R_TEXT_PRIMARY
                };
                let mut name_style = Style::default().fg(name_color).add_modifier(Modifier::BOLD);
                if let Some(bg_color) = bg {
                    name_style = name_style.bg(bg_color);
                }
                spans.push(Span::styled(agent.name.clone(), name_style));
                if branch_selected {
                    let mut hint_style = Style::default().fg(theme::R_TEXT_TERTIARY);
                    if let Some(bg_color) = bg {
                        hint_style = hint_style.bg(bg_color);
                    }
                    spans.push(Span::styled("  Enter", hint_style));
                    spans.push(Span::styled(
                        format!("  {} open", editor_key_label()),
                        hint_style,
                    ));
                }
                lines.push(pad_line(spans, width, bg));
                targets.push(TreeClickTarget::Branch(repo_idx, 0, i));
            }
            continue;
        }

        let has_local = !repo.local_branches.is_empty();
        let has_remote = !repo.remote_branches.is_empty();
        let local_cat_collapsed = selection.is_category_collapsed(repo_idx, 0);
        let remote_cat_collapsed = selection.is_category_collapsed(repo_idx, 1);

        // Local Branches section
        if has_local {
            let cat_selected = is_this_repo
                && selection.level == TreeLevel::Category
                && selection.category_idx == 0;
            let cat_open =
                is_this_repo && selection.level == TreeLevel::Branch && selection.category_idx == 0;

            let connector = if has_remote { "├──" } else { "└──" };
            let cat_chevron = if local_cat_collapsed { "▸" } else { "▾" };
            let (cat_fg, cat_bg) = if cat_selected {
                (theme::R_TEXT_PRIMARY, Some(theme::R_BG_HOVER))
            } else if cat_open {
                (theme::R_TEXT_TERTIARY, None)
            } else {
                (theme::R_TEXT_SECONDARY, None)
            };

            let cat_text = format!("   {connector} {cat_chevron} Local Branches");
            let mut cat_style = Style::default().fg(cat_fg);
            if let Some(bg_color) = cat_bg {
                cat_style = cat_style.bg(bg_color);
            }
            lines.push(pad_line(
                vec![Span::styled(cat_text, cat_style)],
                width,
                cat_bg,
            ));
            targets.push(TreeClickTarget::Category(repo_idx, 0));

            if !local_cat_collapsed {
                let continuation = if has_remote { "│" } else { " " };
                for (i, branch) in repo.local_branches.iter().enumerate() {
                    let is_last = i == repo.local_branches.len() - 1;
                    let branch_connector = if is_last { "└──" } else { "├──" };
                    let branch_selected = is_this_repo
                        && selection.level == TreeLevel::Branch
                        && selection.category_idx == 0
                        && i == selection.branch_idx;
                    lines.push(format_branch_line(
                        branch,
                        continuation,
                        branch_connector,
                        branch_selected,
                        width,
                        repo_clr,
                    ));
                    targets.push(TreeClickTarget::Branch(repo_idx, 0, i));
                }
            }
        }

        // Remote Branches section
        if has_remote {
            let cat_selected = is_this_repo
                && selection.level == TreeLevel::Category
                && selection.category_idx == 1;
            let cat_open =
                is_this_repo && selection.level == TreeLevel::Branch && selection.category_idx == 1;

            let cat_chevron = if remote_cat_collapsed { "▸" } else { "▾" };
            let (cat_fg, cat_bg) = if cat_selected {
                (theme::R_TEXT_PRIMARY, Some(theme::R_BG_HOVER))
            } else if cat_open {
                (theme::R_TEXT_TERTIARY, None)
            } else {
                (theme::R_TEXT_SECONDARY, None)
            };

            let cat_text = format!("   └── {cat_chevron} Remote Branches");
            let mut cat_style = Style::default().fg(cat_fg);
            if let Some(bg_color) = cat_bg {
                cat_style = cat_style.bg(bg_color);
            }
            lines.push(pad_line(
                vec![Span::styled(cat_text, cat_style)],
                width,
                cat_bg,
            ));
            targets.push(TreeClickTarget::Category(repo_idx, 1));

            if !remote_cat_collapsed {
                for (i, branch) in repo.remote_branches.iter().enumerate() {
                    let is_last = i == repo.remote_branches.len() - 1;
                    let branch_connector = if is_last { "└──" } else { "├──" };
                    let branch_selected = is_this_repo
                        && selection.level == TreeLevel::Branch
                        && selection.category_idx == 1
                        && i == selection.branch_idx;
                    lines.push(format_branch_line(
                        branch,
                        " ",
                        branch_connector,
                        branch_selected,
                        width,
                        repo_clr,
                    ));
                    targets.push(TreeClickTarget::Branch(repo_idx, 1, i));
                }
            }
        }
    }

    (lines, targets)
}

fn format_branch_line(
    branch: &clust_ipc::BranchInfo,
    continuation: &str,
    connector: &str,
    is_selected: bool,
    width: u16,
    repo_color: Color,
) -> Line<'static> {
    let mut spans = Vec::new();
    let bg = if is_selected {
        Some(theme::R_BG_HOVER)
    } else {
        None
    };

    let indicator = if is_selected { "▸ " } else { "  " };

    // Tree structure prefix
    let mut prefix_style = Style::default().fg(theme::R_TEXT_TERTIARY);
    if let Some(bg_color) = bg {
        prefix_style = prefix_style.bg(bg_color);
    }
    spans.push(Span::styled(
        format!("   {continuation}  {connector} {indicator}"),
        prefix_style,
    ));

    // Active agent indicator
    if branch.active_agent_count > 0 {
        let mut dot_style = Style::default().fg(theme::R_SUCCESS);
        if let Some(bg_color) = bg {
            dot_style = dot_style.bg(bg_color);
        }
        spans.push(Span::styled(
            format!("● {} ", branch.active_agent_count),
            dot_style,
        ));
    }

    let name_color = if is_selected || branch.is_head {
        repo_color
    } else {
        theme::R_TEXT_PRIMARY
    };
    let mut name_style = Style::default().fg(name_color).add_modifier(Modifier::BOLD);
    if let Some(bg_color) = bg {
        name_style = name_style.bg(bg_color);
    }
    spans.push(Span::styled(branch.name.clone(), name_style));

    // Worktree indicator
    if branch.is_worktree {
        let mut wt_style = Style::default().fg(theme::R_TEXT_SECONDARY);
        if let Some(bg_color) = bg {
            wt_style = wt_style.bg(bg_color);
        }
        spans.push(Span::styled(" ⎇".to_string(), wt_style));
    }

    if is_selected {
        let mut hint_style = Style::default().fg(theme::R_TEXT_TERTIARY);
        if let Some(bg_color) = bg {
            hint_style = hint_style.bg(bg_color);
        }
        spans.push(Span::styled("  Enter", hint_style));
        spans.push(Span::styled(
            format!("  {} open", editor_key_label()),
            hint_style,
        ));
    }

    pad_line(spans, width, bg)
}

/// Modifier key label for the "open in editor" shortcut, matching the host OS
/// convention (Opt on macOS, Alt elsewhere).
fn editor_key_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Opt+V"
    } else {
        "Alt+V"
    }
}

/// Context menu label for the "open in editor" branch action. HEAD branches
/// open the repo root, worktree branches open the worktree directly, and
/// non-worktree branches get a worktree created on-demand.
fn open_in_editor_label(branch: &clust_ipc::BranchInfo) -> String {
    if branch.is_head || branch.is_worktree {
        "Open in editor".to_string()
    } else {
        "Create worktree and open in editor".to_string()
    }
}

/// Pad a line's spans to fill `width`, applying background color to the padding.
fn pad_line(spans: Vec<Span<'static>>, width: u16, bg: Option<Color>) -> Line<'static> {
    let content_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (width as usize).saturating_sub(content_width);
    let mut all_spans = spans;
    if remaining > 0 {
        let mut pad_style = Style::default();
        if let Some(bg_color) = bg {
            pad_style = pad_style.bg(bg_color);
        }
        all_spans.push(Span::styled(" ".repeat(remaining), pad_style));
    }
    Line::from(all_spans)
}

pub(crate) fn render_logo(frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(""));

    // Logo lines with accent colors
    for (i, text) in LOGO_LINES.iter().enumerate() {
        let color = if i == 2 || i == 3 {
            theme::R_ACCENT_BRIGHT
        } else {
            theme::R_ACCENT
        };
        let padded = format!("  {:<44}", text);
        lines.push(Line::from(Span::styled(padded, Style::default().fg(color))));
    }

    lines.push(Line::from(""));

    // Gradient bar
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("░░", Style::default().fg(theme::R_TEXT_TERTIARY)),
        Span::styled("▒▒", Style::default().fg(theme::R_TEXT_SECONDARY)),
        Span::styled(
            "▓▓██████████████████████████████",
            Style::default().fg(theme::R_TEXT_PRIMARY),
        ),
        Span::styled("▓▓", Style::default().fg(theme::R_TEXT_SECONDARY)),
        Span::styled("▒▒░░", Style::default().fg(theme::R_TEXT_TERTIARY)),
        Span::raw("  "),
    ]));

    let block_height = lines.len() as u16;
    let block_width = 46u16;

    let [vert_area] = Layout::vertical([Constraint::Length(block_height)])
        .flex(Flex::Center)
        .areas(area);

    let [horz_area] = Layout::horizontal([Constraint::Length(block_width)])
        .flex(Flex::Center)
        .areas(vert_area);

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, horz_area);
}

fn state_to_agent_info(
    state: &overview::FocusModeState,
    repo_colors: &HashMap<String, String>,
) -> Option<(String, ratatui::style::Color, String)> {
    let rp = state.repo_path.as_ref()?;
    let repo_display = std::path::Path::new(rp)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| rp.clone());
    let repo_clr = repo_colors
        .get(rp.as_str())
        .map(|c| theme::repo_color(c))
        .unwrap_or(theme::R_ACCENT);
    let branch = state
        .panel
        .as_ref()
        .and_then(|p| p.branch_name.clone())
        .unwrap_or_default();
    Some((repo_display, repo_clr, branch))
}

/// Sub-state shown in the focus-mode status hint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FocusModeHint {
    /// User is somewhere other than the Terminal tab.
    Other,
    /// User is on the Terminal tab in Navigate sub-mode.
    TerminalNavigate,
    /// User is on the Terminal tab in Type sub-mode.
    TerminalType,
}

#[allow(clippy::too_many_arguments)]
fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    hub_running: bool,
    update_notice: &Option<String>,
    hub_name: &str,
    active_tab: ActiveTab,
    in_focus_mode: bool,
    overview_focus: OverviewFocus,
    focused_agent_info: Option<&(String, ratatui::style::Color, String)>,
    status_message: Option<&StatusMessage>,
    mouse_captured: bool,
    mouse_passthrough: bool,
    bypass_permissions: bool,
    focus_hint: FocusModeHint,
) {
    let bg = Style::default().bg(theme::R_BG_RAISED);

    // Build left spans
    let (dot_color, status_label) = if hub_running {
        (theme::R_SUCCESS, "connected")
    } else {
        (theme::R_TEXT_TERTIARY, "disconnected")
    };

    let mut left_spans = vec![
        Span::styled(" ●", Style::default().fg(dot_color).bg(theme::R_BG_RAISED)),
        Span::styled(
            format!(" {status_label}"),
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_RAISED),
        ),
    ];

    if hub_name != clust_ipc::DEFAULT_HUB {
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            hub_name.to_string(),
            Style::default().fg(theme::R_ACCENT).bg(theme::R_BG_RAISED),
        ));
    }

    // Focused agent: repo/branch
    if let Some((repo_name, repo_clr, branch)) = focused_agent_info {
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            repo_name.clone(),
            Style::default().fg(*repo_clr).bg(theme::R_BG_RAISED),
        ));
        if !branch.is_empty() {
            left_spans.push(Span::styled(
                format!("/{branch}"),
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_RAISED),
            ));
        }
    }

    if !mouse_captured {
        let label = if mouse_passthrough {
            "MOUSE OFF \u{00b7} \u{2325}M"
        } else {
            "MOUSE OFF \u{00b7} F2"
        };
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            label,
            Style::default()
                .fg(theme::R_WARNING)
                .bg(theme::R_BG_RAISED)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if bypass_permissions {
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            "BYPASS",
            Style::default()
                .fg(theme::R_WARNING)
                .bg(theme::R_BG_RAISED)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Status message overrides keybinding hints
    if let Some(msg) = status_message {
        let color = match msg.level {
            StatusLevel::Error => theme::R_ERROR,
            StatusLevel::Success => theme::R_SUCCESS,
        };
        left_spans.extend([
            Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)),
            Span::styled(
                msg.text.clone(),
                Style::default().fg(color).bg(theme::R_BG_RAISED),
            ),
        ]);
    } else {
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };
        let hint_text = if in_focus_mode {
            match focus_hint {
                FocusModeHint::TerminalType => {
                    "Ctrl+\\ stop typing  Shift+\u{2191} exit".to_string()
                }
                FocusModeHint::TerminalNavigate => {
                    format!("Ctrl+\\ or Enter type  ] / [ next/prev  n new  x close  {mod_key}+V open editor  Shift+\u{2191} exit")
                }
                FocusModeHint::Other => {
                    format!("Shift+\u{2190}/\u{2192} switch panel  {mod_key}+V open editor  {mod_key}+R new agent  Shift+\u{2191} exit")
                }
            }
        } else if active_tab == ActiveTab::Overview {
            match overview_focus {
                OverviewFocus::Terminal(_) => {
                    format!("Shift+\u{2191} options  Shift+\u{2193} focus  Shift+\u{2190}/\u{2192} switch agent  {mod_key}+V open editor  {mod_key}+R new agent")
                }
                OverviewFocus::OptionsBar => {
                    format!("Shift+\u{2193} enter terminal  Shift+\u{2190}/\u{2192} scroll  {mod_key}+R new agent  q quit  ? keys")
                }
            }
        } else if active_tab == ActiveTab::Schedule {
            format!("{mod_key}+S new task  Shift+\u{2190}/\u{2192} switch panel  d delete  Shift+C clear  ? keys")
        } else {
            format!("{mod_key}+N new repo  {mod_key}+R new agent  {mod_key}+V open editor  q quit  Q stop+quit  ? keys")
        };

        left_spans.extend([
            Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)),
            Span::styled(
                hint_text,
                Style::default()
                    .fg(theme::R_TEXT_TERTIARY)
                    .bg(theme::R_BG_RAISED),
            ),
        ]);
    }

    if let Some(ref msg) = *update_notice {
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            msg.clone(),
            Style::default().fg(theme::R_WARNING).bg(theme::R_BG_RAISED),
        ));
    }

    let left_line = Line::from(left_spans);

    // Right side: version
    let version_text = format!("v{} ", env!("CARGO_PKG_VERSION"));
    let version_width = version_text.len() as u16;
    let right_line = Line::from(Span::styled(
        version_text,
        Style::default()
            .fg(theme::R_TEXT_TERTIARY)
            .bg(theme::R_BG_RAISED),
    ));

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(version_width)]).areas(area);

    frame.render_widget(
        Paragraph::new(left_line).block(Block::default().style(bg)),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(right_line)
            .alignment(Alignment::Right)
            .block(Block::default().style(bg)),
        right_area,
    );
}

fn render_help_overlay(frame: &mut Frame, area: Rect, active_tab: ActiveTab, in_focus_mode: bool) {
    // Each section: optional header, then binding rows or sub-context labels.
    let mut lines: Vec<Line> = Vec::new();

    // Helper closures for consistent styling
    let binding_line = |key: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!(" {:<16}", key),
                Style::default().fg(theme::R_ACCENT),
            ),
            Span::styled(desc.to_string(), Style::default().fg(theme::R_TEXT_PRIMARY)),
        ])
    };
    let header_line = |title: &str| -> Line<'static> {
        Line::from(Span::styled(
            format!(" {title}"),
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let sub_label_line = |label: &str| -> Line<'static> {
        Line::from(Span::styled(
            format!("   {label}"),
            Style::default().fg(theme::R_TEXT_TERTIARY),
        ))
    };

    // -- Global --
    lines.push(binding_line("q / Esc\u{00d7}2", "Quit"));
    lines.push(binding_line("Q", "Quit and stop hub"));
    lines.push(binding_line("Ctrl+C", "Quit"));
    lines.push(binding_line("Tab", "Next tab"));
    lines.push(binding_line("Shift+Tab", "Previous tab"));
    lines.push(binding_line("?", "Toggle this help"));
    lines.push(binding_line("F2", "Toggle mouse capture"));
    lines.push(binding_line("Alt+M", "Mouse passthrough (5s)"));
    lines.push(binding_line("Alt+E", "Create agent"));
    lines.push(binding_line("Alt+D", "New directory agent"));
    lines.push(binding_line("Alt+F", "Search agents"));
    lines.push(binding_line("Alt+N", "Add repository"));
    lines.push(binding_line(
        editor_key_label(),
        "Open in editor (creates worktree if missing)",
    ));
    lines.push(binding_line("Alt+B", "Toggle bypass permissions"));
    lines.push(binding_line("Alt+P", "Toggle plan mode (in modals)"));

    // -- Repositories --
    if active_tab == ActiveTab::Repositories {
        lines.push(Line::from(""));
        lines.push(header_line("Repositories — left tree"));
        lines.push(binding_line("\u{2191} / \u{2193}", "Navigate items"));
        lines.push(binding_line("\u{2190} / \u{2192}", "Navigate tree"));
        lines.push(binding_line(
            "Shift+\u{2191}/\u{2193}",
            "Jump prev / next repo",
        ));
        lines.push(binding_line("Enter", "Open menu"));
        lines.push(binding_line("Space", "Collapse / expand"));
    }

    // -- Overview --
    if active_tab == ActiveTab::Overview {
        lines.push(Line::from(""));
        lines.push(header_line("Overview"));
        lines.push(binding_line("Shift+\u{2190}/\u{2192}", "Scroll panels"));
        lines.push(binding_line("Shift+\u{2193}", "Enter terminal"));
        lines.push(sub_label_line("In terminal:"));
        lines.push(binding_line("Shift+\u{2191}", "Back to options bar"));
        lines.push(binding_line("Shift+\u{2193}", "Enter focus mode"));
        lines.push(binding_line("Shift+\u{2190}/\u{2192}", "Switch agent"));
        lines.push(binding_line("PgUp / PgDn", "Scroll terminal"));
    }

    // -- Schedule --
    if active_tab == ActiveTab::Schedule {
        lines.push(Line::from(""));
        lines.push(header_line("Schedule"));
        lines.push(binding_line("Alt+S", "Schedule a new task"));
        lines.push(binding_line("Shift+\u{2190}/\u{2192}", "Switch focused task"));
        lines.push(binding_line("\u{2191} / \u{2193}", "Scroll prompt body"));
        lines.push(binding_line("d / Del", "Delete focused task"));
        lines.push(binding_line("Shift+C", "Clear by status menu"));
        lines.push(sub_label_line("Inactive task:"));
        lines.push(binding_line("e", "Edit prompt"));
        lines.push(binding_line("p", "Toggle plan mode"));
        lines.push(binding_line("x", "Toggle auto-exit"));
        lines.push(binding_line("s", "Start now"));
        lines.push(sub_label_line("Active task:"));
        lines.push(binding_line("Shift+\u{2193}", "Enter focus mode (live PTY)"));
        lines.push(sub_label_line("Aborted task:"));
        lines.push(binding_line("e", "Edit prompt"));
        lines.push(binding_line("p", "Toggle plan mode"));
        lines.push(binding_line("x", "Toggle auto-exit"));
        lines.push(binding_line("r", "Restart"));
        lines.push(binding_line("Shift+R", "Restart with clean worktree"));
    }

    // -- Focus Mode --
    if in_focus_mode {
        lines.push(Line::from(""));
        lines.push(header_line("Focus Mode"));
        lines.push(binding_line("Shift+\u{2191}", "Exit focus mode"));
        lines.push(binding_line("Shift+\u{2190}/\u{2192}", "Switch panel"));
        lines.push(binding_line("Shift+PgUp/PgDn", "Scroll terminal"));
        lines.push(sub_label_line("Left panel:"));
        lines.push(binding_line("Tab", "Cycle tabs"));
        lines.push(binding_line("\u{2191} / \u{2193}", "Scroll diff"));
        lines.push(sub_label_line("Terminal tab (Navigate):"));
        lines.push(binding_line("Ctrl+\\", "Toggle Type \u{2194} Navigate"));
        lines.push(binding_line("Enter", "Enter Type mode"));
        lines.push(binding_line("[ / ]", "Previous / next terminal"));
        lines.push(binding_line("n", "New terminal"));
        lines.push(binding_line("x", "Close current terminal"));
        lines.push(sub_label_line("Terminal tab (Type):"));
        lines.push(binding_line("Ctrl+\\", "Stop typing (back to Navigate)"));
        lines.push(binding_line("(any other)", "Forwarded to the active shell"));
    }

    let modal_width: u16 = 44;
    let modal_height: u16 = lines.len() as u16 + 2; // +2 for border

    let [horz_area] = Layout::horizontal([Constraint::Length(modal_width)])
        .flex(Flex::Center)
        .areas(area);

    let modal_rect = Rect {
        x: horz_area.x,
        y: area.y + area.height.saturating_sub(modal_height),
        width: modal_width,
        height: modal_height.min(area.height),
    };

    frame.render_widget(Clear, modal_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY))
        .style(Style::default().bg(theme::R_BG_OVERLAY));

    let inner = block.inner(modal_rect);
    frame.render_widget(block, modal_rect);

    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fetch_agents() -> Vec<AgentInfo> {
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return vec![];
        };
        if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { hub: None })
            .await
            .is_err()
        {
            return vec![];
        }
        match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
            Ok(HubMessage::AgentList { agents }) => agents,
            _ => vec![],
        }
    })
}

fn fetch_repos() -> Vec<RepoInfo> {
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return vec![];
        };
        if clust_ipc::send_message(&mut stream, &CliMessage::ListRepos)
            .await
            .is_err()
        {
            return vec![];
        }
        match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
            Ok(HubMessage::RepoList { repos }) => repos,
            _ => vec![],
        }
    })
}

fn fetch_bypass_permissions() -> bool {
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return false;
        };
        if clust_ipc::send_message(&mut stream, &CliMessage::GetBypassPermissions)
            .await
            .is_err()
        {
            return false;
        }
        match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
            Ok(HubMessage::BypassPermissions { enabled }) => enabled,
            _ => false,
        }
    })
}

fn set_bypass_permissions_ipc(enabled: bool) {
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(&mut stream, &CliMessage::SetBypassPermissions { enabled })
            .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn set_repo_color_ipc(path: &str, color: &str) {
    let path = path.to_string();
    let color = color.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ =
            clust_ipc::send_message(&mut stream, &CliMessage::SetRepoColor { path, color }).await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn stop_repo_agents_ipc(path: &str) {
    let path = path.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(&mut stream, &CliMessage::StopRepoAgents { path }).await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn unregister_repo_ipc(path: &str) {
    let path = path.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(&mut stream, &CliMessage::UnregisterRepo { path }).await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

/// Ask the hub to delete the repository folder from disk and unregister it.
/// Surfaces the hub's success or error message via the status bar so the
/// user sees why a deletion was refused (e.g. the safety check rejected it).
fn delete_repo_ipc(path: &str, status_tx: tokio::sync::mpsc::Sender<StatusMessage>) {
    let path = path.to_string();
    tokio::spawn(async move {
        let mut stream = match ipc::try_connect().await {
            Ok(s) => s,
            Err(e) => {
                let _ = status_tx
                    .send(StatusMessage {
                        text: format!("Delete failed: hub connect error: {e}"),
                        level: StatusLevel::Error,
                        created: Instant::now(),
                    })
                    .await;
                return;
            }
        };
        if let Err(e) = clust_ipc::send_message(&mut stream, &CliMessage::DeleteRepo { path }).await
        {
            let _ = status_tx
                .send(StatusMessage {
                    text: format!("Delete failed: send error: {e}"),
                    level: StatusLevel::Error,
                    created: Instant::now(),
                })
                .await;
            return;
        }
        match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
            Ok(HubMessage::RepoDeleted { name, .. }) => {
                let _ = status_tx
                    .send(StatusMessage {
                        text: format!("Repository deleted: {name}"),
                        level: StatusLevel::Success,
                        created: Instant::now(),
                    })
                    .await;
            }
            Ok(HubMessage::Error { message }) => {
                let _ = status_tx
                    .send(StatusMessage {
                        text: format!("Delete failed: {message}"),
                        level: StatusLevel::Error,
                        created: Instant::now(),
                    })
                    .await;
            }
            Ok(_) => {
                let _ = status_tx
                    .send(StatusMessage {
                        text: "Delete failed: unexpected hub response".to_string(),
                        level: StatusLevel::Error,
                        created: Instant::now(),
                    })
                    .await;
            }
            Err(e) => {
                let _ = status_tx
                    .send(StatusMessage {
                        text: format!("Delete failed: recv error: {e}"),
                        level: StatusLevel::Error,
                        created: Instant::now(),
                    })
                    .await;
            }
        }
    });
}

fn stop_agents_ipc(agent_ids: &[String]) {
    for id in agent_ids {
        let id = id.clone();
        block_on_async(async {
            let Ok(mut stream) = ipc::try_connect().await else {
                return;
            };
            let _ = clust_ipc::send_message(&mut stream, &CliMessage::StopAgent { id }).await;
            let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
        });
    }
}

fn remove_worktree_ipc(repo_path: &str, branch_name: &str, delete_branch: bool, force: bool) {
    let working_dir = repo_path.to_string();
    let branch_name = branch_name.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::RemoveWorktree {
                working_dir: Some(working_dir),
                repo_name: None,
                branch_name,
                delete_local_branch: delete_branch,
                force,
            },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

/// Pop the first pending worktree cleanup and return the corresponding `ActiveMenu`, if any.
fn pop_worktree_cleanup_menu(
    pending: &mut Vec<crate::worktree::WorktreeCleanup>,
) -> Option<ActiveMenu> {
    let next = pending.pop()?;
    let dirty = crate::worktree::is_worktree_dirty(&next.repo_path, &next.branch_name);
    let title = if dirty {
        format!("Worktree '{}' (uncommitted changes)", next.branch_name)
    } else {
        format!("Worktree '{}'", next.branch_name)
    };
    Some(ActiveMenu::WorktreeCleanup {
        repo_path: next.repo_path,
        branch_name: next.branch_name,
        menu: ContextMenu::new(
            &title,
            vec![
                "Keep".to_string(),
                "Discard worktree".to_string(),
                "Discard worktree + branch".to_string(),
            ],
        ),
    })
}

fn delete_local_branch_ipc(repo_path: &str, branch_name: &str) {
    let working_dir = repo_path.to_string();
    let branch_name = branch_name.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::DeleteLocalBranch {
                working_dir: Some(working_dir),
                repo_name: None,
                branch_name,
                force: true,
            },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn delete_remote_branch_ipc(repo_path: &str, branch_name: &str) {
    let working_dir = repo_path.to_string();
    let branch_name = branch_name.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::DeleteRemoteBranch {
                working_dir: Some(working_dir),
                repo_name: None,
                branch_name,
            },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn add_worktree_ipc(repo_path: &str, branch_name: &str, base_branch: &str) {
    let working_dir = repo_path.to_string();
    let branch_name = branch_name.to_string();
    let base_branch = base_branch.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::AddWorktree {
                working_dir: Some(working_dir),
                repo_name: None,
                branch_name,
                base_branch: Some(base_branch),
                checkout_existing: false,
            },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

/// Create a worktree for an existing local branch and return its path on success.
/// Used by the "Create worktree and open in editor" flow.
fn checkout_worktree_ipc(repo_path: &str, branch_name: &str) -> Option<String> {
    let working_dir = repo_path.to_string();
    let branch_name = branch_name.to_string();
    block_on_async(async {
        let mut stream = ipc::try_connect().await.ok()?;
        clust_ipc::send_message(
            &mut stream,
            &CliMessage::AddWorktree {
                working_dir: Some(working_dir),
                repo_name: None,
                branch_name,
                base_branch: None,
                checkout_existing: true,
            },
        )
        .await
        .ok()?;
        match clust_ipc::recv_message::<HubMessage>(&mut stream)
            .await
            .ok()?
        {
            HubMessage::WorktreeAdded { path, .. } => Some(path),
            _ => None,
        }
    })
}

fn start_purge_async(repo_path: &str) -> PurgeProgress {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let path = repo_path.to_string();
    let repo_name = std::path::Path::new(repo_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo_path.to_string());
    tokio::spawn(async move {
        let Ok(mut stream) = ipc::try_connect().await else {
            let _ = tx.send(PurgeEvent::Error("Failed to connect to hub".into()));
            return;
        };
        if clust_ipc::send_message(&mut stream, &CliMessage::PurgeRepo { path })
            .await
            .is_err()
        {
            let _ = tx.send(PurgeEvent::Error("Failed to send purge request".into()));
            return;
        }
        loop {
            match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
                Ok(HubMessage::PurgeProgress { step }) => {
                    let _ = tx.send(PurgeEvent::Step(step));
                }
                Ok(HubMessage::RepoPurged { .. }) => {
                    let _ = tx.send(PurgeEvent::Done);
                    return;
                }
                Ok(HubMessage::Error { message }) => {
                    let _ = tx.send(PurgeEvent::Error(message));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(PurgeEvent::Error(format!("Connection error: {e}")));
                    return;
                }
                _ => {}
            }
        }
    });
    PurgeProgress {
        repo_name,
        steps: Vec::new(),
        done: false,
        error: None,
        rx,
        started: Instant::now(),
    }
}

fn render_purge_progress(frame: &mut Frame, area: Rect, progress: &PurgeProgress) {
    let spinner_idx = (progress.started.elapsed().as_millis() / 120) as usize % SPINNER_CHARS.len();
    let spinner = SPINNER_CHARS[spinner_idx];

    let mut lines: Vec<Line> = Vec::new();

    for (i, step) in progress.steps.iter().enumerate() {
        let is_last = i == progress.steps.len() - 1;
        let (prefix, prefix_color) = if is_last && !progress.done {
            (format!(" {spinner} "), theme::R_ACCENT)
        } else {
            (" \u{2713} ".to_string(), theme::R_SUCCESS)
        };

        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(prefix_color).bg(theme::R_BG_OVERLAY),
            ),
            Span::styled(
                step.clone(),
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_OVERLAY),
            ),
        ]));
    }

    if progress.steps.is_empty() && !progress.done {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {spinner} "),
                Style::default().fg(theme::R_ACCENT).bg(theme::R_BG_OVERLAY),
            ),
            Span::styled(
                "Starting purge\u{2026}",
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_OVERLAY),
            ),
        ]));
    }

    if let Some(ref error) = progress.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" Error: {error}"),
            Style::default().fg(theme::R_ERROR).bg(theme::R_BG_OVERLAY),
        )));
    }

    if progress.done {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Press Esc to close",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_OVERLAY),
        )));
    }

    let title = format!("Purging {}", progress.repo_name);
    let content_max_width = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 4);
    let modal_width = (content_max_width + 4) as u16;
    let modal_height = (lines.len() + 3) as u16;

    let [horz_area] = Layout::horizontal([Constraint::Length(modal_width)])
        .flex(Flex::Center)
        .areas(area);

    let modal_rect = Rect {
        x: horz_area.x,
        y: area.y + area.height.saturating_sub(modal_height) / 2,
        width: modal_width.min(area.width),
        height: modal_height.min(area.height),
    };

    frame.render_widget(Clear, modal_rect);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                title,
                Style::default()
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY))
        .padding(Padding::new(1, 1, 0, 0))
        .style(Style::default().bg(theme::R_BG_OVERLAY));

    let inner = block.inner(modal_rect);
    frame.render_widget(block, modal_rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn start_clone_async(url: &str, parent_dir: &str, name: Option<&str>) -> CloneProgress {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let url_owned = url.to_string();
    let parent_dir_owned = parent_dir.to_string();
    let name_owned = name.map(|s| s.to_string());
    let display_url = url.to_string();
    tokio::spawn(async move {
        let Ok(mut stream) = ipc::try_connect().await else {
            let _ = tx.send(CloneEvent::Error("Failed to connect to hub".into()));
            return;
        };
        let msg = CliMessage::CloneRepo {
            url: url_owned,
            parent_dir: parent_dir_owned,
            name: name_owned,
        };
        if clust_ipc::send_message(&mut stream, &msg).await.is_err() {
            let _ = tx.send(CloneEvent::Error("Failed to send clone request".into()));
            return;
        }
        loop {
            match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
                Ok(HubMessage::CloneProgress { step }) => {
                    let _ = tx.send(CloneEvent::Step(step));
                }
                Ok(HubMessage::RepoCloned { .. }) => {
                    let _ = tx.send(CloneEvent::Done);
                    return;
                }
                Ok(HubMessage::Error { message }) => {
                    let _ = tx.send(CloneEvent::Error(message));
                    return;
                }
                Err(e) => {
                    let _ = tx.send(CloneEvent::Error(format!("Connection error: {e}")));
                    return;
                }
                _ => {}
            }
        }
    });
    CloneProgress {
        url: display_url,
        steps: Vec::new(),
        done: false,
        error: None,
        rx,
        started: Instant::now(),
    }
}

fn render_clone_progress(frame: &mut Frame, area: Rect, progress: &CloneProgress) {
    let spinner_idx = (progress.started.elapsed().as_millis() / 120) as usize % SPINNER_CHARS.len();
    let spinner = SPINNER_CHARS[spinner_idx];

    let mut lines: Vec<Line> = Vec::new();

    for (i, step) in progress.steps.iter().enumerate() {
        let is_last = i == progress.steps.len() - 1;
        let (prefix, prefix_color) = if is_last && !progress.done {
            (format!(" {spinner} "), theme::R_ACCENT)
        } else {
            (" \u{2713} ".to_string(), theme::R_SUCCESS)
        };

        lines.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(prefix_color).bg(theme::R_BG_OVERLAY),
            ),
            Span::styled(
                step.clone(),
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_OVERLAY),
            ),
        ]));
    }

    if progress.steps.is_empty() && !progress.done {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {spinner} "),
                Style::default().fg(theme::R_ACCENT).bg(theme::R_BG_OVERLAY),
            ),
            Span::styled(
                "Starting clone\u{2026}",
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_OVERLAY),
            ),
        ]));
    }

    if let Some(ref error) = progress.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" Error: {error}"),
            Style::default().fg(theme::R_ERROR).bg(theme::R_BG_OVERLAY),
        )));
    }

    if progress.done {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Press Esc to close",
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_OVERLAY),
        )));
    }

    // Truncate the URL for display
    let url_short = if progress.url.len() > 40 {
        format!("\u{2026}{}", &progress.url[progress.url.len() - 39..])
    } else {
        progress.url.clone()
    };
    let title = format!("Cloning {url_short}");
    let content_max_width = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 4);
    let modal_width = (content_max_width + 4) as u16;
    let modal_height = (lines.len() + 3) as u16;

    let [horz_area] = Layout::horizontal([Constraint::Length(modal_width)])
        .flex(Flex::Center)
        .areas(area);

    let modal_rect = Rect {
        x: horz_area.x,
        y: area.y + area.height.saturating_sub(modal_height) / 2,
        width: modal_width.min(area.width),
        height: modal_height.min(area.height),
    };

    frame.render_widget(Clear, modal_rect);

    let block = Block::default()
        .title(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(
                title,
                Style::default()
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::R_TEXT_TERTIARY))
        .padding(Padding::new(1, 1, 0, 0))
        .style(Style::default().bg(theme::R_BG_OVERLAY));

    let inner = block.inner(modal_rect);
    frame.render_widget(block, modal_rect);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn clean_stale_refs_ipc(path: &str) {
    let working_dir = path.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::CleanStaleRefs {
                working_dir: Some(working_dir),
                repo_name: None,
            },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn open_in_file_system(path: &str) {
    let path = path.to_string();
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
    }
}

fn open_in_terminal(path: &str) {
    let path = path.to_string();
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .args(["-a", "Terminal", &path])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let terminals: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["--working-directory"]),
            ("gnome-terminal", &["--working-directory"]),
            ("konsole", &["--workdir"]),
            ("xfce4-terminal", &["--working-directory"]),
        ];
        for &(bin, args) in terminals {
            let mut cmd = std::process::Command::new(bin);
            for arg in args {
                cmd.arg(arg);
            }
            cmd.arg(&path);
            if cmd.spawn().is_ok() {
                return;
            }
        }
    }
}

fn set_repo_editor_ipc(path: &str, editor: &str) {
    let path = path.to_string();
    let editor = editor.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ =
            clust_ipc::send_message(&mut stream, &CliMessage::SetRepoEditor { path, editor }).await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn set_default_editor_ipc(editor: &str) {
    let editor = editor.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ =
            clust_ipc::send_message(&mut stream, &CliMessage::SetDefaultEditor { editor }).await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

/// Determine the target path and repo path for "open in editor" based on current mode.
fn resolve_editor_target(
    in_focus_mode: bool,
    focus_mode_state: &overview::FocusModeState,
    active_tab: ActiveTab,
    overview_state: &OverviewState,
    display_repos: &[RepoInfo],
    selection: &TreeSelection,
    agents: &[AgentInfo],
) -> (Option<String>, Option<String>) {
    // Focus mode: use the agent's working directory
    if in_focus_mode && focus_mode_state.is_active() {
        let target = focus_mode_state.working_dir.clone();
        let rp = focus_mode_state.repo_path.clone();
        return (target, rp);
    }

    match active_tab {
        ActiveTab::Repositories => {
            if let Some(repo) = display_repos.get(selection.repo_idx) {
                if repo.path.is_empty() {
                    return (None, None);
                }
                match selection.level {
                    TreeLevel::Repo | TreeLevel::Category => {
                        (Some(repo.path.clone()), Some(repo.path.clone()))
                    }
                    TreeLevel::Branch => {
                        if selection.category_idx == 0 {
                            // Local branch
                            if let Some(branch) = repo.local_branches.get(selection.branch_idx) {
                                let target = if branch.is_head {
                                    repo.path.clone()
                                } else if branch.is_worktree {
                                    worktree_dir(&repo.path, &branch.name)
                                } else {
                                    repo.path.clone()
                                };
                                (Some(target), Some(repo.path.clone()))
                            } else {
                                (Some(repo.path.clone()), Some(repo.path.clone()))
                            }
                        } else {
                            // Remote branch — open repo root
                            (Some(repo.path.clone()), Some(repo.path.clone()))
                        }
                    }
                }
            } else {
                (None, None)
            }
        }
        ActiveTab::Overview => {
            if let overview::OverviewFocus::Terminal(idx) = overview_state.focus {
                if let Some(panel) = overview_state.panels.get(idx) {
                    // Look up agent working_dir from the agents list
                    let target = agents
                        .iter()
                        .find(|a| a.id == panel.id)
                        .map(|a| a.working_dir.clone());
                    let rp = panel.repo_path.clone();
                    return (target, rp);
                }
            }
            (None, None)
        }
        ActiveTab::Schedule => {
            // No editor target on the Schedule tab — Opt+V is a no-op here.
            (None, None)
        }
    }
}

/// Compute the worktree directory for a branch (branch name with / → __).
fn worktree_dir(repo_path: &str, branch_name: &str) -> String {
    let serialized = branch_name.replace('/', "__");
    format!("{repo_path}/.clust/worktrees/{serialized}")
}

/// Open the worktree (or repo root for HEAD) for `branch_name` in the user's
/// editor, creating the worktree first if it does not yet exist. Returns
/// `true` if a worktree was created (so the caller can refresh repo state).
fn open_branch_in_editor(
    repo_path: &str,
    branch_name: &str,
    repos: &[RepoInfo],
    active_menu: &mut Option<ActiveMenu>,
    editors_cache: &[crate::editor::DetectedEditor],
    status_message: &mut Option<StatusMessage>,
) -> bool {
    let branch = repos
        .iter()
        .find(|r| r.path == repo_path)
        .and_then(|r| r.local_branches.iter().find(|b| b.name == branch_name))
        .cloned();
    let mut created = false;
    let target = match branch {
        Some(b) if b.is_head => repo_path.to_string(),
        Some(b) if b.is_worktree => worktree_dir(repo_path, branch_name),
        Some(_) => match checkout_worktree_ipc(repo_path, branch_name) {
            Some(path) => {
                created = true;
                path
            }
            None => {
                *status_message = Some(StatusMessage {
                    text: format!("Failed to create worktree for {branch_name}"),
                    level: StatusLevel::Error,
                    created: Instant::now(),
                });
                return false;
            }
        },
        None => repo_path.to_string(),
    };
    trigger_open_in_editor(&target, Some(repo_path), repos, active_menu, editors_cache);
    created
}

/// Trigger the "open in editor" flow: either open directly if a preference is saved,
/// or show the editor picker modal.
fn trigger_open_in_editor(
    target_path: &str,
    repo_path: Option<&str>,
    repos: &[RepoInfo],
    active_menu: &mut Option<ActiveMenu>,
    editors_cache: &[crate::editor::DetectedEditor],
) {
    // Check if this repo has a saved editor preference
    if let Some(rp) = repo_path {
        if let Some(repo) = repos.iter().find(|r| r.path == rp) {
            if let Some(ref editor_binary) = repo.editor {
                if let Some(editor) =
                    crate::editor::find_editor_by_binary(editors_cache, editor_binary)
                {
                    crate::editor::open_in_editor(editor, target_path);
                    return;
                }
                // Saved editor no longer installed — fall through to picker
            }
        }
    }

    match editors_cache.len() {
        0 => {} // No editors found
        1 => {
            let editor = editors_cache[0].clone();
            crate::editor::open_in_editor(&editor, target_path);
            if repo_path.is_some() {
                *active_menu = Some(ActiveMenu::EditorRemember {
                    repo_path: repo_path.map(|s| s.to_string()),
                    editor,
                    menu: ContextMenu::new(
                        "Remember this editor?",
                        vec![
                            "Just this time".to_string(),
                            "For this repository".to_string(),
                            "For all repositories".to_string(),
                        ],
                    ),
                });
            }
        }
        _ => {
            let labels: Vec<String> = editors_cache.iter().map(|e| e.name.clone()).collect();
            *active_menu = Some(ActiveMenu::EditorPicker {
                target_path: target_path.to_string(),
                repo_path: repo_path.map(|s| s.to_string()),
                editors: editors_cache.to_vec(),
                menu: ContextMenu::new("Open in Editor", labels),
            });
        }
    }
}

/// Spawn batch task agents for each entry in `start_info.tasks_to_start`.
/// Prompts are built using the batch's prefix/suffix via `build_prompt`.
/// Run an async future from the synchronous UI loop.
/// Requires the multi-thread tokio scheduler (`#[tokio::main]`).
fn block_on_async<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Translate a `ScheduleAction` returned by the Schedule tab into concrete
/// state mutations + IPC sends. Kept out of the main loop's match arm so the
/// arm itself stays a single line.
fn dispatch_schedule_action(
    action: ScheduleAction,
    edit_prompt_modal: &mut Option<EditPromptModal>,
    active_menu: &mut Option<ActiveMenu>,
    status_tx: tokio::sync::mpsc::Sender<StatusMessage>,
    refresh_tx: tokio::sync::mpsc::Sender<Vec<clust_ipc::ScheduledTaskInfo>>,
) {
    use crate::context_menu::ContextMenu;

    let send_and_refresh = |msg: CliMessage| {
        let s_tx = status_tx.clone();
        let r_tx = refresh_tx.clone();
        tokio::spawn(async move {
            match ipc::send_one_shot(msg).await {
                Ok(HubMessage::Error { message }) => {
                    let _ = s_tx
                        .send(StatusMessage {
                            text: message,
                            level: StatusLevel::Error,
                            created: std::time::Instant::now(),
                        })
                        .await;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = s_tx
                        .send(StatusMessage {
                            text: format!("hub error: {e}"),
                            level: StatusLevel::Error,
                            created: std::time::Instant::now(),
                        })
                        .await;
                }
            }
            let tasks = ipc::fetch_scheduled_tasks().await;
            let _ = r_tx.send(tasks).await;
        });
    };

    match action {
        ScheduleAction::Noop => {}
        ScheduleAction::EditPrompt { task_id, current } => {
            *edit_prompt_modal = Some(EditPromptModal::new(
                task_id.clone(),
                task_id, // shown as "branch" in the title — close enough
                current,
            ));
        }
        ScheduleAction::TogglePlanMode { task_id, new_value } => {
            send_and_refresh(CliMessage::SetScheduledTaskPlanMode {
                id: task_id,
                plan_mode: new_value,
            });
        }
        ScheduleAction::ToggleAutoExit { task_id, new_value } => {
            send_and_refresh(CliMessage::SetScheduledTaskAutoExit {
                id: task_id,
                auto_exit: new_value,
            });
        }
        ScheduleAction::StartNow { task_id } => {
            send_and_refresh(CliMessage::StartScheduledTaskNow { id: task_id });
        }
        ScheduleAction::Restart { task_id, clean } => {
            send_and_refresh(CliMessage::RestartScheduledTask {
                id: task_id,
                clean,
            });
        }
        ScheduleAction::ConfirmDelete {
            task_id,
            branch_name,
        } => {
            *active_menu = Some(ActiveMenu::ConfirmAction {
                action: ConfirmedAction::DeleteScheduledTask { task_id },
                menu: ContextMenu::new(
                    &format!("Delete scheduled task on {branch_name}?"),
                    vec!["Delete".to_string(), "Cancel".to_string()],
                ),
            });
        }
        ScheduleAction::OpenClearMenu => {
            *active_menu = Some(ActiveMenu::ConfirmAction {
                action: ConfirmedAction::ClearScheduledTasksByStatus {
                    status: clust_ipc::ScheduledTaskStatus::Complete,
                },
                menu: ContextMenu::new(
                    "Clear all completed tasks?",
                    vec!["Clear Completed".to_string(), "Cancel".to_string()],
                ),
            });
        }
        ScheduleAction::EnterFocusMode { .. } => {
            // Handled inline at the Schedule key-routing site so it can
            // reach focus_mode_state / agents / last_content_area.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Repository tree rendering tests ──────────────────────────

    fn make_branch(
        name: &str,
        is_head: bool,
        agent_count: usize,
        is_worktree: bool,
    ) -> clust_ipc::BranchInfo {
        clust_ipc::BranchInfo {
            name: name.to_string(),
            is_head,
            active_agent_count: agent_count,
            is_worktree,
            is_remote: false,
        }
    }

    fn make_repo(
        name: &str,
        local: Vec<clust_ipc::BranchInfo>,
        remote: Vec<clust_ipc::BranchInfo>,
    ) -> clust_ipc::RepoInfo {
        clust_ipc::RepoInfo {
            path: format!("/repos/{name}"),
            name: name.to_string(),
            color: Some("blue".to_string()),
            editor: None,
            local_branches: local,
            remote_branches: remote,
        }
    }

    #[test]
    fn tree_empty_repos_produces_no_lines() {
        let sel = TreeSelection::default();
        let (lines, _targets) = build_repo_tree_lines(&[], &sel, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn tree_single_repo_with_local_branches() {
        let repo = make_repo(
            "myrepo",
            vec![
                make_branch("main", true, 0, false),
                make_branch("feature", false, 0, false),
            ],
            vec![],
        );
        let sel = TreeSelection::default();
        let (lines, _targets) = build_repo_tree_lines(&[repo], &sel, 80);

        // Should have: repo name + "Local Branches" header + 2 branch lines
        assert_eq!(lines.len(), 4);

        // First line is repo name
        let first = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(first.contains("myrepo"));

        // Second line is section header
        let second = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(second.contains("Local Branches"));
    }

    #[test]
    fn tree_repo_with_local_and_remote() {
        let repo = make_repo(
            "myrepo",
            vec![make_branch("main", true, 0, false)],
            vec![make_branch("origin/main", false, 0, false)],
        );
        let sel = TreeSelection::default();
        let (lines, _targets) = build_repo_tree_lines(&[repo], &sel, 80);

        // repo name + local header + 1 local branch + remote header (collapsed by default)
        assert_eq!(lines.len(), 4);

        let texts: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(texts[0].contains("myrepo"));
        assert!(texts[1].contains("Local Branches"));
        assert!(texts[2].contains("main"));
        assert!(texts[3].contains("Remote Branches"));
    }

    #[test]
    fn tree_multiple_repos_separated_by_blank_line() {
        let repos = vec![
            make_repo("alpha", vec![make_branch("main", true, 0, false)], vec![]),
            make_repo("beta", vec![make_branch("main", true, 0, false)], vec![]),
        ];
        let sel = TreeSelection::default();
        let (lines, _targets) = build_repo_tree_lines(&repos, &sel, 80);

        // alpha: name + header + branch = 3
        // spacer between repos = 1
        // beta: name + header + branch = 3
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn format_branch_line_shows_agent_indicator() {
        let branch = make_branch("main", false, 1, false);
        let line = format_branch_line(&branch, "│", "├─", false, 80, theme::R_ACCENT);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "should have active agent indicator");
        assert!(text.contains("main"));
    }

    #[test]
    fn format_branch_line_no_agent_indicator() {
        let branch = make_branch("main", false, 0, false);
        let line = format_branch_line(&branch, "│", "├─", false, 80, theme::R_ACCENT);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("●"), "should not have agent indicator");
    }

    #[test]
    fn format_branch_line_shows_worktree_indicator() {
        let branch = make_branch("feature", false, 0, true);
        let line = format_branch_line(&branch, " ", "└─", false, 80, theme::R_ACCENT);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⎇"), "should have worktree indicator");
    }

    #[test]
    fn format_branch_line_no_worktree_indicator() {
        let branch = make_branch("feature", false, 0, false);
        let line = format_branch_line(&branch, " ", "└─", false, 80, theme::R_ACCENT);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("⎇"), "should not have worktree indicator");
    }

    #[test]
    fn format_branch_line_head_and_agent_and_worktree() {
        let branch = make_branch("main", true, 1, true);
        let line = format_branch_line(&branch, "│", "├─", false, 80, theme::R_ACCENT);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("●"), "agent indicator");
        assert!(text.contains("main"), "branch name");
        assert!(text.contains("⎇"), "worktree indicator");
    }

    // ── Tree selection state tests ──────────────────────────────

    fn sample_repos() -> Vec<RepoInfo> {
        vec![
            make_repo(
                "alpha",
                vec![
                    make_branch("main", true, 0, false),
                    make_branch("dev", false, 0, false),
                ],
                vec![make_branch("origin/main", false, 0, false)],
            ),
            make_repo("beta", vec![make_branch("main", true, 0, false)], vec![]),
        ]
    }

    #[test]
    fn selection_default_is_repo_level() {
        let sel = TreeSelection::default();
        assert_eq!(sel.level, TreeLevel::Repo);
        assert_eq!(sel.repo_idx, 0);
    }

    #[test]
    fn selection_move_down_repos() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        // Flat nav: move_down from expanded repo descends into first category
        sel.move_down(&repos);
        assert_eq!(sel.level, TreeLevel::Category);
        assert_eq!(sel.repo_idx, 0);
        assert_eq!(sel.category_idx, 0);
    }

    #[test]
    fn selection_move_up_repos() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        // Already at 0, stays at 0
        sel.move_up(&repos);
        assert_eq!(sel.repo_idx, 0);
    }

    #[test]
    fn selection_descend_to_category() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos);
        assert_eq!(sel.level, TreeLevel::Category);
        assert_eq!(sel.category_idx, 0); // first valid = local
    }

    #[test]
    fn selection_descend_to_branch() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch
        assert_eq!(sel.level, TreeLevel::Branch);
        assert_eq!(sel.branch_idx, 0);
    }

    #[test]
    fn selection_ascend_round_trip() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch
        sel.ascend(&repos); // -> Category
        assert_eq!(sel.level, TreeLevel::Category);
        sel.ascend(&repos); // -> Repo (ascend always goes up, never collapses)
        assert_eq!(sel.level, TreeLevel::Repo);
        sel.ascend(&repos); // no-op, already at top
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn selection_category_up_down() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        // alpha has both local (0) and remote (1)
        sel.descend(&repos); // -> Category, idx 0
        assert_eq!(sel.category_idx, 0);
        // Flat nav: move_down from expanded category descends into first branch
        sel.move_down(&repos);
        assert_eq!(sel.level, TreeLevel::Branch);
        assert_eq!(sel.branch_idx, 0);
        // Go back up to category header
        sel.move_up(&repos);
        assert_eq!(sel.level, TreeLevel::Category);
        assert_eq!(sel.category_idx, 0);
        // Go up again to repo
        sel.move_up(&repos);
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn selection_branch_up_down() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category (local)
        sel.descend(&repos); // -> Branch (0)
        assert_eq!(sel.branch_idx, 0);
        sel.move_down(&repos); // alpha has 2 local branches
        assert_eq!(sel.branch_idx, 1);
        // Flat nav: last local branch -> crosses to remote category
        // (remote is collapsed by default, so lands on category header)
        sel.move_down(&repos);
        assert_eq!(sel.level, TreeLevel::Category);
        assert_eq!(sel.category_idx, 1);
        sel.move_up(&repos);
        assert_eq!(sel.level, TreeLevel::Branch);
        assert_eq!(sel.category_idx, 0);
        assert_eq!(sel.branch_idx, 1); // last local branch
    }

    #[test]
    fn selection_descend_noop_on_empty_repos() {
        let mut sel = TreeSelection::default();
        sel.descend(&[]);
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn selection_descend_skips_empty_category() {
        // beta has only local branches
        let repos = sample_repos();
        let mut sel = TreeSelection {
            repo_idx: 1,
            ..TreeSelection::default()
        };
        sel.descend(&repos); // -> Category
        assert_eq!(sel.category_idx, 0); // only local exists
                                         // Move down should be no-op (no remote)
        sel.move_down(&repos);
        assert_eq!(sel.category_idx, 0);
    }

    #[test]
    fn selection_clamp_empty_repos() {
        let mut sel = TreeSelection {
            level: TreeLevel::Branch,
            repo_idx: 5,
            category_idx: 1,
            branch_idx: 3,
            ..TreeSelection::default()
        };
        sel.clamp(&[]);
        assert_eq!(sel.level, TreeLevel::Repo);
        assert_eq!(sel.repo_idx, 0);
    }

    #[test]
    fn selection_clamp_shrinks_indices() {
        let repos = sample_repos(); // 2 repos
        let mut sel = TreeSelection {
            level: TreeLevel::Repo,
            repo_idx: 10,
            category_idx: 0,
            branch_idx: 0,
            ..TreeSelection::default()
        };
        sel.clamp(&repos);
        assert_eq!(sel.repo_idx, 1); // clamped to max valid
    }

    #[test]
    fn selection_clamp_invalid_category_resets() {
        let repos = sample_repos();
        // beta (idx 1) has no remote branches
        let mut sel = TreeSelection {
            level: TreeLevel::Category,
            repo_idx: 1,
            category_idx: 1, // remote doesn't exist for beta
            branch_idx: 0,
            ..TreeSelection::default()
        };
        sel.clamp(&repos);
        assert_eq!(sel.category_idx, 0); // falls back to local
    }

    #[test]
    fn tree_selected_repo_shows_expanded_chevron() {
        let repos = sample_repos();
        let sel = TreeSelection::default(); // repo 0 selected, expanded by default
        let (lines, _targets) = build_repo_tree_lines(&repos, &sel, 80);
        let first = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            first.contains("▾"),
            "expanded repo should have down chevron"
        );
    }

    #[test]
    fn tree_non_selected_repo_shows_expanded_chevron() {
        let repos = sample_repos();
        let sel = TreeSelection::default(); // repo 0 selected, not repo 1
        let (lines, _targets) = build_repo_tree_lines(&repos, &sel, 80);
        let beta_line = lines
            .iter()
            .find(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains("beta")
            })
            .expect("should find beta line");
        let text: String = beta_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("▾"),
            "non-selected expanded repo should have down chevron"
        );
    }

    #[test]
    fn tree_selected_branch_shows_indicator() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch 0 (main)
        let (lines, _targets) = build_repo_tree_lines(&repos, &sel, 80);
        // Branch line for "main" should have indicator
        let main_line = lines
            .iter()
            .find(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                text.contains("main") && !text.contains("origin") && !text.contains("Branches")
            })
            .expect("should find main branch line");
        let text: String = main_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("▸"),
            "selected branch should have arrow indicator"
        );
    }

    // ── Agent selection state tests ──────────────────────────────

    fn make_agent(id: &str, hub: &str) -> AgentInfo {
        AgentInfo {
            id: id.to_string(),
            agent_binary: "claude".to_string(),
            started_at: "2026-03-26T10:00:00Z".to_string(),
            attached_clients: 0,
            hub: hub.to_string(),
            working_dir: "/tmp".to_string(),
            repo_path: None,
            branch_name: None,
            is_worktree: false,
            auto_exit: false,
            plan_mode: false,
            prompt: None,
        }
    }

    #[test]
    fn scoped_agent_ids_filters_by_selected_repo() {
        let agents = vec![
            AgentInfo {
                repo_path: Some("/home/user/project-a".into()),
                ..make_agent("a1", "default")
            },
            AgentInfo {
                repo_path: Some("/home/user/project-b".into()),
                ..make_agent("b1", "default")
            },
            AgentInfo {
                repo_path: None,
                ..make_agent("c1", "default")
            },
        ];
        let display_repos = vec![
            RepoInfo {
                path: "/home/user/project-a".into(),
                name: "project-a".into(),
                color: None,
                editor: None,
                local_branches: vec![],
                remote_branches: vec![],
            },
            RepoInfo {
                path: "/home/user/project-b".into(),
                name: "project-b".into(),
                color: None,
                editor: None,
                local_branches: vec![],
                remote_branches: vec![],
            },
        ];
        let mut selection = TreeSelection {
            repo_idx: 0,
            ..Default::default()
        };
        let (ids, _) = scoped_agent_ids(&agents, &display_repos, &selection);
        assert_eq!(ids, vec!["a1".to_string()]);
        selection.repo_idx = 1;
        let (ids, _) = scoped_agent_ids(&agents, &display_repos, &selection);
        assert_eq!(ids, vec!["b1".to_string()]);
    }

    // ── Collapse state tests ──────────────────────────────────────

    #[test]
    fn toggle_collapse_repo() {
        let mut sel = TreeSelection::default();
        assert!(!sel.is_repo_collapsed(0));
        sel.toggle_collapse(); // at Repo level
        assert!(sel.is_repo_collapsed(0));
        sel.toggle_collapse(); // toggle back
        assert!(!sel.is_repo_collapsed(0));
    }

    #[test]
    fn toggle_collapse_category() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        assert!(!sel.is_category_collapsed(0, 0));
        sel.toggle_collapse();
        assert!(sel.is_category_collapsed(0, 0));
        sel.toggle_collapse();
        assert!(!sel.is_category_collapsed(0, 0));
    }

    #[test]
    fn toggle_collapse_branch_noop() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        sel.descend(&repos); // -> Branch
        sel.toggle_collapse(); // should not panic
        assert_eq!(sel.level, TreeLevel::Branch);
    }

    #[test]
    fn tree_collapsed_repo_hides_branches() {
        let repo = make_repo(
            "myrepo",
            vec![make_branch("main", true, 0, false)],
            vec![make_branch("origin/main", false, 0, false)],
        );
        let mut sel = TreeSelection::default();
        sel.toggle_collapse(); // collapse repo 0

        let (lines, _targets) = build_repo_tree_lines(&[repo], &sel, 80);
        // Only the repo header line should remain
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("myrepo"));
        assert!(
            text.contains("▸"),
            "collapsed repo should have right chevron"
        );
    }

    #[test]
    fn tree_collapsed_category_hides_branches() {
        let repo = make_repo(
            "myrepo",
            vec![
                make_branch("main", true, 0, false),
                make_branch("feature", false, 0, false),
            ],
            vec![],
        );
        let mut sel = TreeSelection::default();
        sel.descend(std::slice::from_ref(&repo)); // -> Category
        sel.toggle_collapse(); // collapse local category

        let (lines, _targets) = build_repo_tree_lines(&[repo], &sel, 80);
        // repo header + category header (collapsed, no branches)
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn descend_into_collapsed_is_noop() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.toggle_collapse(); // collapse repo 0
        assert!(sel.is_repo_collapsed(0));
        assert_eq!(sel.level, TreeLevel::Repo);

        sel.descend(&repos); // should be a no-op — Enter required to expand
        assert!(sel.is_repo_collapsed(0));
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    #[test]
    fn ascend_from_repo_is_noop() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        assert!(!sel.is_repo_collapsed(0));
        sel.ascend(&repos); // already at top — no-op, does NOT collapse
        assert!(!sel.is_repo_collapsed(0));
        assert_eq!(sel.level, TreeLevel::Repo);
    }

    // ── Tab navigation tests ──────────────────────────────────────

    #[test]
    fn active_tab_next_cycles() {
        let tab = ActiveTab::Repositories;
        assert_eq!(tab.next(), ActiveTab::Overview);
        assert_eq!(tab.next().next(), ActiveTab::Schedule);
        assert_eq!(tab.next().next().next(), ActiveTab::Repositories);
    }

    #[test]
    fn active_tab_prev_cycles() {
        let tab = ActiveTab::Repositories;
        assert_eq!(tab.prev(), ActiveTab::Schedule);
        assert_eq!(tab.prev().prev(), ActiveTab::Overview);
        assert_eq!(tab.prev().prev().prev(), ActiveTab::Repositories);
    }

    #[test]
    fn active_tab_labels() {
        assert_eq!(ActiveTab::Repositories.label(), "Repositories");
        assert_eq!(ActiveTab::Overview.label(), "Overview");
        assert_eq!(ActiveTab::Schedule.label(), "Schedule");
    }

    #[test]
    fn ascend_from_category_goes_to_repo() {
        let repos = sample_repos();
        let mut sel = TreeSelection::default();
        sel.descend(&repos); // -> Category
        assert_eq!(sel.level, TreeLevel::Category);
        sel.ascend(&repos); // always goes to Repo level
        assert_eq!(sel.level, TreeLevel::Repo);
    }
}
