use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, EnableMouseCapture, DisableMouseCapture, EnableFocusChange, DisableFocusChange, PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags},
    terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Alignment, Constraint, Flex, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame, Terminal,
};

use clust_ipc::{AgentInfo, CliMessage, HubMessage, RepoInfo, DEFAULT_HUB};

use crate::{
    context_menu::{ContextMenu, ContextMenuItem, MenuResult},
    create_agent_modal::{CreateAgentModal, ModalResult},
    format::{format_attached, format_started},
    ipc,
    overview::{self, OverviewFocus, OverviewState},
    search_modal::{SearchModal, SearchResult},
    terminal_emulator,
    theme, version,
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

enum AgentStartResult {
    Started {
        agent_id: String,
        agent_binary: String,
        working_dir: String,
        repo_path: Option<String>,
        branch_name: Option<String>,
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

const SPINNER_CHARS: &[char] = &['\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}',
                                  '\u{2826}', '\u{2827}', '\u{2807}', '\u{280f}'];

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

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FocusPanel {
    Left,
    Right,
}

/// Active tab in the top-level tab bar.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ActiveTab {
    Repositories,
    Overview,
}

impl ActiveTab {
    fn next(self) -> Self {
        match self {
            Self::Repositories => Self::Overview,
            Self::Overview => Self::Repositories,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Repositories => Self::Overview,
            Self::Overview => Self::Repositories,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Repositories => "Repositories",
            Self::Overview => "Overview",
        }
    }
}

/// Label for agents that have no linked repository.
const NO_REPOSITORY: &str = "No repository";

// ---------------------------------------------------------------------------
// Agent view mode
// ---------------------------------------------------------------------------

/// Controls how agents are grouped in the right panel.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AgentViewMode {
    /// Group agents by their hub name.
    ByHub,
    /// Group agents by their git repository path (default).
    ByRepo,
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
    left_panel_area: Rect,
    right_panel_area: Rect,
    tree_items: Vec<TreeClickTarget>,
    tree_inner_area: Rect,
    agent_cards: Vec<(Rect, usize, usize)>, // (area, group_idx, agent_idx)
    mode_label_area: Rect,

    // Overview tab
    pub(crate) overview_panels: Vec<(Rect, usize)>, // (area, global_panel_idx)

    // Focus mode
    pub(crate) focus_left_area: Rect,
    pub(crate) focus_right_area: Rect,
    pub(crate) focus_left_tabs: Vec<(Rect, overview::LeftPanelTab)>,
    focus_back_button: Rect,

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
    RemoveWorktree,
    DeleteBranch,
    RemoteStartAgent,
    RemoteCreateWorktree,
    DeleteRemoteBranch,
    BaseWorktreeOff,
}

/// Action to execute after user confirms in a confirmation dialog.
enum ConfirmedAction {
    PurgeRepo { repo_path: String },
    StartAgentDetach { repo_path: String, branch_name: String },
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
}

// ---------------------------------------------------------------------------
// Agent selection state (right panel)
// ---------------------------------------------------------------------------

/// Returns sorted, deduplicated group names from an agent list based on view mode.
fn group_names(agents: &[AgentInfo], mode: AgentViewMode) -> Vec<String> {
    let mut names: Vec<String> = agents
        .iter()
        .map(|a| match mode {
            AgentViewMode::ByHub => a.hub.clone(),
            AgentViewMode::ByRepo => a
                .repo_path
                .clone()
                .unwrap_or_else(|| NO_REPOSITORY.to_string()),
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Returns the group key for an agent based on view mode.
fn agent_group_key(agent: &AgentInfo, mode: AgentViewMode) -> String {
    match mode {
        AgentViewMode::ByHub => agent.hub.clone(),
        AgentViewMode::ByRepo => agent
            .repo_path
            .clone()
            .unwrap_or_else(|| NO_REPOSITORY.to_string()),
    }
}

/// Tracks the user's cursor position within the agent panel (group + agent).
#[derive(Default)]
struct AgentSelection {
    group_idx: usize,
    agent_idx: usize,
}

impl AgentSelection {
    /// Returns the number of agents in the currently selected group.
    fn agent_count(&self, agents: &[AgentInfo], mode: AgentViewMode) -> usize {
        let names = group_names(agents, mode);
        names
            .get(self.group_idx)
            .map(|group| {
                agents
                    .iter()
                    .filter(|a| agent_group_key(a, mode) == *group)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Adjust indices to stay within bounds after data refresh.
    fn clamp(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        let names = group_names(agents, mode);
        if names.is_empty() {
            self.group_idx = 0;
            self.agent_idx = 0;
            return;
        }
        self.group_idx = self.group_idx.min(names.len() - 1);
        let ac = self.agent_count(agents, mode);
        if ac > 0 {
            self.agent_idx = self.agent_idx.min(ac - 1);
        } else {
            self.agent_idx = 0;
        }
    }

    fn move_up(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        if group_names(agents, mode).is_empty() {
            return;
        }
        self.agent_idx = self.agent_idx.saturating_sub(1);
    }

    fn move_down(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        let ac = self.agent_count(agents, mode);
        if ac > 0 {
            self.agent_idx = (self.agent_idx + 1).min(ac - 1);
        }
    }

    fn prev_group(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        if group_names(agents, mode).is_empty() {
            return;
        }
        if self.group_idx > 0 {
            self.group_idx -= 1;
            self.agent_idx = 0;
        }
    }

    fn next_group(&mut self, agents: &[AgentInfo], mode: AgentViewMode) {
        let names = group_names(agents, mode);
        if names.is_empty() {
            return;
        }
        if self.group_idx + 1 < names.len() {
            self.group_idx += 1;
            self.agent_idx = 0;
        }
    }
}

/// Resolve the currently selected agent from the selection state.
///
/// Replicates the sorting/grouping logic used by the render functions to map
/// `(group_idx, agent_idx)` back to an actual `AgentInfo`.
fn resolve_selected_agent<'a>(
    agents: &'a [AgentInfo],
    sel: &AgentSelection,
    mode: AgentViewMode,
) -> Option<&'a AgentInfo> {
    let names = group_names(agents, mode);
    let group_name = names.get(sel.group_idx)?;

    match mode {
        AgentViewMode::ByHub => {
            let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
            sorted.sort_by(|a, b| a.hub.cmp(&b.hub).then(a.started_at.cmp(&b.started_at)));
            sorted
                .into_iter()
                .filter(|a| a.hub == *group_name)
                .nth(sel.agent_idx)
        }
        AgentViewMode::ByRepo => {
            let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
            sorted.sort_by(|a, b| {
                let ak = agent_group_key(a, AgentViewMode::ByRepo);
                let bk = agent_group_key(b, AgentViewMode::ByRepo);
                ak.cmp(&bk)
                    .then(a.branch_name.cmp(&b.branch_name))
                    .then(a.started_at.cmp(&b.started_at))
            });
            sorted
                .into_iter()
                .filter(|a| agent_group_key(a, AgentViewMode::ByRepo) == *group_name)
                .nth(sel.agent_idx)
        }
    }
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
        repos
            .get(self.repo_idx)
            .is_some_and(|r| r.path.is_empty())
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
            let bc = repos.get(self.repo_idx).map_or(0, |r| r.local_branches.len());
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
                    let bc = repos.get(self.repo_idx).map_or(0, |r| r.local_branches.len());
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
    let mut focus = FocusPanel::Left;
    let mut agent_selection = AgentSelection::default();
    let mut agent_view_mode = AgentViewMode::ByRepo;
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
    // Purge progress modal state
    let mut purge_progress: Option<PurgeProgress> = None;
    let (agent_start_tx, mut agent_start_rx) =
        tokio::sync::mpsc::channel::<AgentStartResult>(4);
    let (status_tx, mut status_rx) =
        tokio::sync::mpsc::channel::<StatusMessage>(4);

    loop {
        // Drain output events (non-blocking, runs regardless of tab)
        overview_state.drain_output_events();
        focus_mode_state.drain_output_events();
        focus_mode_state.drain_diff_events();

        // Immediate worktree cleanup prompt when agent exits in focus mode
        if in_focus_mode && active_menu.is_none() {
            if let Some(panel) = focus_mode_state.panel.as_mut() {
                if panel.exited && panel.is_worktree && !panel.worktree_cleanup_shown {
                    panel.worktree_cleanup_shown = true;
                    if let (Some(rp), Some(bn)) = (&panel.repo_path, &panel.branch_name) {
                        pending_worktree_cleanups = vec![crate::worktree::WorktreeCleanup {
                            repo_path: rp.clone(),
                            branch_name: bn.clone(),
                        }];
                        active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
                    }
                }
            }
        }

        // Check for completed agent start requests
        if let Ok(result) = agent_start_rx.try_recv() {
            match result {
                AgentStartResult::Started {
                    agent_id,
                    agent_binary,
                    working_dir,
                    repo_path,
                    branch_name,
                } => {
                    if active_tab == ActiveTab::Overview {
                        // Stay in overview mode; select the agent after next sync
                        pending_overview_select = Some(agent_id.clone());
                    } else {
                        let fm_cols = (last_content_area.width * 40 / 100)
                            .saturating_sub(2)
                            .max(1);
                        let fm_rows = last_content_area.height.saturating_sub(3).max(1);
                        focus_mode_state.open_agent(
                            &agent_id,
                            &agent_binary,
                            fm_cols,
                            fm_rows,
                            &working_dir,
                            repo_path.as_deref(),
                            branch_name.as_deref(),
                            true, // WorktreeAgentStarted → always a worktree
                        );
                        in_focus_mode = true;
                    }
                    let branch_label = branch_name.as_deref().unwrap_or("unknown");
                    status_message = Some(StatusMessage {
                        text: format!("Agent started on {branch_label}"),
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

        // Periodically fetch agent list and repo state from hub
        let mut agents_refreshed = false;
        if hub_running && last_agent_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            agents = fetch_agents();
            agent_selection.clamp(&agents, agent_view_mode);
            last_agent_fetch = Instant::now();
            agents_refreshed = true;
        }
        if hub_running && last_repo_fetch.elapsed() >= AGENT_FETCH_INTERVAL {
            repos = fetch_repos();
            last_repo_fetch = Instant::now();
        }

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
                            }
                        })
                        .collect(),
                    remote_branches: vec![],
                });
            }
            dr
        };
        selection.clamp(&display_repos);

        // Sync overview agent connections when agents are refreshed
        if agents_refreshed && active_tab == ActiveTab::Overview {
            overview_state.sync_agents(&agents, last_content_area);
            if let Some(id) = pending_overview_select.take() {
                overview_state.select_agent_by_id(&id);
            }
        }

        let hub_status = hub_running;
        let notice = update_notice.lock().unwrap().clone();
        let cur_focus = focus;
        let cur_tab = active_tab;
        let cur_focus_mode = in_focus_mode;
        let overview_focus = overview_state.focus;
        let status_msg_ref = status_message.as_ref();
        let show_help_now = show_help;
        let mouse_captured_now = mouse_captured;
        let menu_ref = &active_menu;
        let repo_colors: HashMap<String, String> = repos
            .iter()
            .filter_map(|r| r.color.as_ref().map(|c| (r.path.clone(), c.clone())))
            .collect();

        let mut click_map = ClickMap::default();
        let show_modal = create_modal.is_some();
        let show_search = search_modal.is_some();
        let purge_ref = &purge_progress;

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
                overview::render_focus_mode(frame, content_area, &mut focus_mode_state, &mut click_map, &repo_colors);
            } else {
                render_tab_bar(frame, header_area, cur_tab, &mut click_map);

                match cur_tab {
                    ActiveTab::Repositories => {
                        // Content: left (40%) + divider (1 col) + right (60%)
                        let [left_area, divider_area, right_area] =
                            Layout::horizontal([
                                Constraint::Percentage(40),
                                Constraint::Length(1),
                                Constraint::Percentage(60),
                            ])
                            .areas(content_area);

                        click_map.left_panel_area = left_area;
                        click_map.right_panel_area = right_area;

                        render_left_panel(
                            frame,
                            left_area,
                            &display_repos,
                            &selection,
                            cur_focus == FocusPanel::Left,
                            &mut click_map,
                        );
                        render_divider(frame, divider_area);
                        render_right_panel(
                            frame,
                            right_area,
                            &agents,
                            &agent_selection,
                            cur_focus == FocusPanel::Right,
                            agent_view_mode,
                            &mut click_map,
                            &repo_colors,
                        );
                    }
                    ActiveTab::Overview => {
                        overview::render_overview(frame, content_area, &mut overview_state, &mut click_map, &repo_colors);
                    }
                }
            }

            // Resolve focused agent info for status bar
            let focused_agent_info: Option<(String, ratatui::style::Color, String)> = if cur_focus_mode {
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
            );

            if let Some(ref menu_state) = menu_ref {
                let menu = match menu_state {
                    ActiveMenu::AgentPicker { ref menu, .. } => menu,
                    ActiveMenu::RepoActions { ref menu, .. } => menu,
                    ActiveMenu::ColorPicker { ref menu, .. } => menu,
                    ActiveMenu::BranchActions { ref menu, .. } => menu,
                    ActiveMenu::ConfirmAction { ref menu, .. } => menu,
                    ActiveMenu::WorktreeCleanup { ref menu, .. } => menu,
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
            }

            if show_search {
                if let Some(ref modal) = search_modal {
                    modal.render(frame, content_area);
                }
            }

            if let Some(ref pp) = *purge_ref {
                render_purge_progress(frame, content_area, pp);
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // F2: toggle mouse capture (global, never forwarded)
                    if key.code == KeyCode::F(2) {
                        mouse_captured = !mouse_captured;
                        if mouse_captured {
                            io::stdout().execute(EnableMouseCapture)?;
                        } else {
                            io::stdout().execute(DisableMouseCapture)?;
                        }
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
                    // Context menu overlay: intercept all keys when active
                    } else if let Some(ref mut menu_state) = active_menu {
                        let result = match menu_state {
                            ActiveMenu::AgentPicker { ref mut menu, .. } => menu.handle_key(key.code),
                            ActiveMenu::RepoActions { ref mut menu, .. } => menu.handle_key(key.code),
                            ActiveMenu::ColorPicker { ref mut menu, .. } => menu.handle_key(key.code),
                            ActiveMenu::BranchActions { ref mut menu, .. } => menu.handle_key(key.code),
                            ActiveMenu::ConfirmAction { ref mut menu, .. } => menu.handle_key(key.code),
                            ActiveMenu::WorktreeCleanup { ref mut menu, .. } => menu.handle_key(key.code),
                        };
                        match result {
                            MenuResult::Selected(idx) => {
                                // Take ownership of the menu state to process the action
                                let taken = active_menu.take().unwrap();
                                match taken {
                                    ActiveMenu::AgentPicker { agents: picker_agents, .. } => {
                                        if let Some(agent) = picker_agents.get(idx) {
                                            let agent_id = agent.id.clone();
                                            let agent_binary = agent.agent_binary.clone();
                                            let working_dir = agent.working_dir.clone();
                                            let fm_cols = (last_content_area.width * 40 / 100)
                                                .saturating_sub(2)
                                                .max(1);
                                            let fm_rows =
                                                last_content_area.height.saturating_sub(3).max(1);
                                            focus_mode_state.open_agent(
                                                &agent_id,
                                                &agent_binary,
                                                fm_cols,
                                                fm_rows,
                                                &working_dir,
                                                agent.repo_path.as_deref(),
                                                agent.branch_name.as_deref(),
                                                agent.is_worktree,
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
                                                    .filter(|a| a.repo_path.as_deref() == Some(&*repo_path))
                                                    .cloned()
                                                    .collect();
                                                stop_repo_agents_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                let cleanups = crate::worktree::collect_worktree_cleanups(
                                                    &repo_agents, &agents,
                                                );
                                                if !cleanups.is_empty() {
                                                    pending_worktree_cleanups = cleanups;
                                                    active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
                                                }
                                            }
                                            4 => {
                                                // "Unregister"
                                                unregister_repo_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                            }
                                            5 => {
                                                // "Clean Stale Refs"
                                                clean_stale_refs_ipc(&repo_path);
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
                                            _ => {}
                                        }
                                    }
                                    ActiveMenu::ColorPicker { repo_path, .. } => {
                                        if let Some(&color_name) =
                                            theme::REPO_COLOR_NAMES.get(idx)
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
                                                    let (cols, rows) =
                                                        crossterm::terminal::size()
                                                            .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                                target_branch: Some(bn),
                                                                new_branch: None,
                                                                prompt: None,
                                                                agent_binary: None,
                                                                cols,
                                                                rows: rows
                                                                    .saturating_sub(2)
                                                                    .max(1),
                                                                accept_edits: false,
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
                                                BranchAction::StartAgentInPlace => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let rp = repo_path.clone();
                                                    let (cols, rows) =
                                                        crossterm::terminal::size()
                                                            .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                            rows: rows
                                                                .saturating_sub(2)
                                                                .max(1),
                                                            accept_edits: false,
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
                                                                repo_path,
                                                                branch_name,
                                                                ..
                                                            }) => {
                                                                let working_dir = repo_path
                                                                    .clone()
                                                                    .unwrap_or_default();
                                                                let _ = tx
                                                                    .send(AgentStartResult::Started {
                                                                        agent_id: id,
                                                                        agent_binary,
                                                                        working_dir,
                                                                        repo_path,
                                                                        branch_name,
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
                                                BranchAction::Pull => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                            Ok(HubMessage::BranchPulled { branch_name, .. }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Pulled {branch_name}"),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                }).await;
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
                                                    let cleanups = crate::worktree::collect_worktree_cleanups(
                                                        &branch_agents, &agents,
                                                    );
                                                    if !cleanups.is_empty() {
                                                        pending_worktree_cleanups = cleanups;
                                                        active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
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
                                                        focus_mode_state.open_agent(
                                                            &agent.id,
                                                            &agent.agent_binary,
                                                            fm_cols,
                                                            fm_rows,
                                                            &agent.working_dir,
                                                            agent.repo_path.as_deref(),
                                                            agent.branch_name.as_deref(),
                                                            agent.is_worktree,
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
                                                    if let Some(repo_info) = repos.iter().find(|r| r.path == repo_path).cloned() {
                                                        create_modal = Some(CreateAgentModal::new_with_branch(
                                                            repos.clone(),
                                                            repo_info,
                                                            branch_name.clone(),
                                                        ));
                                                    }
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
                                                        let (cols, rows) =
                                                            crossterm::terminal::size()
                                                                .unwrap_or((80, 24));
                                                        tokio::spawn(async move {
                                                            let mut stream =
                                                                match ipc::try_connect().await
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
                                                                CliMessage::CreateWorktreeAgent
                                                                {
                                                                    repo_path: rp,
                                                                    target_branch: Some(
                                                                        remote_ref,
                                                                    ),
                                                                    new_branch: Some(
                                                                        local_name,
                                                                    ),
                                                                    prompt: None,
                                                                    agent_binary: None,
                                                                    cols,
                                                                    rows: rows
                                                                        .saturating_sub(2)
                                                                        .max(1),
                                                                    accept_edits: false,
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
                                            }
                                        }
                                    }
                                    ActiveMenu::ConfirmAction { action, .. } => {
                                        if idx == 0 {
                                            match action {
                                                ConfirmedAction::PurgeRepo { repo_path } => {
                                                    purge_progress = Some(start_purge_async(&repo_path));
                                                }
                                                ConfirmedAction::StartAgentDetach { repo_path, branch_name } => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let (cols, rows) =
                                                        crossterm::terminal::size()
                                                            .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                                repo_path,
                                                                target_branch: Some(branch_name),
                                                                new_branch: None,
                                                                prompt: None,
                                                                agent_binary: None,
                                                                cols,
                                                                rows: rows
                                                                    .saturating_sub(2)
                                                                    .max(1),
                                                                accept_edits: false,
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
                                        }
                                    }
                                    ActiveMenu::WorktreeCleanup { repo_path, branch_name, .. } => {
                                        match idx {
                                            1 => {
                                                // Discard worktree
                                                remove_worktree_ipc(&repo_path, &branch_name, false, true);
                                                last_repo_fetch = Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch = Instant::now() - Duration::from_secs(10);
                                            }
                                            2 => {
                                                // Discard worktree + branch
                                                remove_worktree_ipc(&repo_path, &branch_name, true, true);
                                                last_repo_fetch = Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch = Instant::now() - Duration::from_secs(10);
                                            }
                                            _ => {} // Keep
                                        }
                                        // Show next pending cleanup if any
                                        if let Some(m) = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups) {
                                            active_menu = Some(m);
                                        }
                                    }
                                }
                            }
                            MenuResult::Dismissed => {
                                active_menu = None;
                                // If dismissed during worktree cleanup, show next if any
                                if let Some(m) = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups) {
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
                                let (cols, rows) =
                                    crossterm::terminal::size().unwrap_or((80, 24));
                                tokio::spawn(async move {
                                    let mut stream = match ipc::try_connect().await {
                                        Ok(s) => s,
                                        Err(e) => {
                                            let _ = tx.send(AgentStartResult::Failed(
                                                format!("Agent create failed: hub connect error: {e}")
                                            )).await;
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
                                        hub,
                                    };
                                    if let Err(e) =
                                        clust_ipc::send_message(&mut stream, &msg).await
                                    {
                                        let _ = tx.send(AgentStartResult::Failed(
                                            format!("Agent create failed: send error: {e}")
                                        )).await;
                                        return;
                                    }
                                    match clust_ipc::recv_message::<HubMessage>(&mut stream)
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
                            ModalResult::Pending => {}
                        }
                    // Search-agent modal takes priority over all other input
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
                                let fm_rows =
                                    last_content_area.height.saturating_sub(3).max(1);
                                focus_mode_state.open_agent(
                                    &agent.id,
                                    &agent.agent_binary,
                                    fm_cols,
                                    fm_rows,
                                    &agent.working_dir,
                                    agent.repo_path.as_deref(),
                                    agent.branch_name.as_deref(),
                                    agent.is_worktree,
                                );
                                in_focus_mode = true;
                                active_tab = ActiveTab::Overview;
                            }
                            SearchResult::Pending => {}
                        }
                    } else if key.code == KeyCode::Char('r')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+R opens create-agent modal
                        if !repos.is_empty() {
                            create_modal = Some(CreateAgentModal::new(repos.clone()));
                            show_help = false;
                        }
                    } else if key.code == KeyCode::Char('f')
                        && key.modifiers.contains(KeyModifiers::ALT)
                    {
                        // Global shortcut: Alt+F opens search-agent modal
                        if !agents.is_empty() {
                            search_modal = Some(SearchModal::new(agents.clone()));
                            show_help = false;
                        }
                    } else
                    // Focus mode: behavior depends on which side has focus
                    if in_focus_mode
                        && focus_mode_state.is_active()
                    {
                        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                        if focus_mode_state.focus_side
                            == overview::FocusSide::Left
                        {
                            // Left panel focused: arrow keys scroll diff
                            if shift {
                                match key.code {
                                    KeyCode::Up => {
                                        // Exit focus mode
                                        focus_mode_state.shutdown();
                                        in_focus_mode = false;
                                        if active_tab == ActiveTab::Overview
                                            && overview_state.initialized
                                        {
                                            overview_state.force_resize_all();
                                        }
                                    }
                                    KeyCode::Right => {
                                        focus_mode_state.focus_side =
                                            overview::FocusSide::Right;
                                    }
                                    _ => {}
                                }
                            } else {
                                match key.code {
                                    KeyCode::Up => {
                                        focus_mode_state.diff_scroll_up()
                                    }
                                    KeyCode::Down => {
                                        focus_mode_state.diff_scroll_down()
                                    }
                                    KeyCode::Tab => {
                                        focus_mode_state.left_tab =
                                            focus_mode_state.left_tab.next();
                                    }
                                    _ => {}
                                }
                            }
                        } else {
                            // Right panel focused
                            if shift {
                                match key.code {
                                    KeyCode::Up => {
                                        // Exit focus mode
                                        focus_mode_state.shutdown();
                                        in_focus_mode = false;
                                        if active_tab == ActiveTab::Overview
                                            && overview_state.initialized
                                        {
                                            overview_state.force_resize_all();
                                        }
                                    }
                                    KeyCode::Left => {
                                        focus_mode_state.focus_side =
                                            overview::FocusSide::Left;
                                    }
                                    KeyCode::PageUp => {
                                        if let Some(panel) =
                                            &mut focus_mode_state.panel
                                        {
                                            let page = panel.vterm.rows();
                                            let max =
                                                panel.vterm.scrollback_len();
                                            panel.panel_scroll_offset =
                                                (panel.panel_scroll_offset + page)
                                                    .min(max);
                                        }
                                    }
                                    KeyCode::PageDown => {
                                        if let Some(panel) =
                                            &mut focus_mode_state.panel
                                        {
                                            let page = panel.vterm.rows();
                                            panel.panel_scroll_offset = panel
                                                .panel_scroll_offset
                                                .saturating_sub(page);
                                        }
                                    }
                                    _ => {
                                        if let Some(bytes) =
                                            overview::input::key_event_to_bytes(
                                                &key,
                                            )
                                        {
                                            focus_mode_state.send_input(bytes);
                                        }
                                    }
                                }
                            } else if key.code == KeyCode::Esc {
                                // Forward Esc to agent process
                                focus_mode_state.send_input(vec![0x1b]);
                            } else if let Some(bytes) =
                                overview::input::key_event_to_bytes(&key)
                            {
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
                                    if let OverviewFocus::Terminal(idx) =
                                        overview_state.focus
                                    {
                                        if let Some(panel) =
                                            overview_state.panels.get(idx)
                                        {
                                            let agent_id = panel.id.clone();
                                            let agent_binary =
                                                panel.agent_binary.clone();
                                            let found = agents
                                                .iter()
                                                .find(|a| a.id == agent_id);
                                            let working_dir = found
                                                .map(|a| a.working_dir.clone())
                                                .unwrap_or_default();
                                            let repo_path = found
                                                .and_then(|a| a.repo_path.clone());
                                            let branch_name = found
                                                .and_then(|a| a.branch_name.clone());
                                            let is_wt = found
                                                .map(|a| a.is_worktree)
                                                .unwrap_or(false);
                                            let fm_cols = (last_content_area.width
                                                * 40
                                                / 100)
                                                .saturating_sub(2)
                                                .max(1);
                                            let fm_rows = last_content_area
                                                .height
                                                .saturating_sub(3)
                                                .max(1);
                                            focus_mode_state.open_agent(
                                                &agent_id,
                                                &agent_binary,
                                                fm_cols,
                                                fm_rows,
                                                &working_dir,
                                                repo_path.as_deref(),
                                                branch_name.as_deref(),
                                                is_wt,
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
                                    if let Some(bytes) =
                                        overview::input::key_event_to_bytes(&key)
                                    {
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
                                            if let Some(panel) = overview_state.panels.get_mut(idx) {
                                                if panel.exited && panel.is_worktree && !panel.worktree_cleanup_shown {
                                                    panel.worktree_cleanup_shown = true;
                                                    if let (Some(rp), Some(bn)) = (&panel.repo_path, &panel.branch_name) {
                                                        pending_worktree_cleanups = vec![crate::worktree::WorktreeCleanup {
                                                            repo_path: rp.clone(),
                                                            branch_name: bn.clone(),
                                                        }];
                                                    }
                                                }
                                            }
                                        }
                                        overview_state.exit_terminal();
                                        if let Some(m) = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups) {
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
                                    if let Some(bytes) =
                                        overview::input::key_event_to_bytes(&key)
                                    {
                                        overview_state.send_input(bytes);
                                    }
                                }
                            }
                        }
                    } else {
                        // Normal key handling (options bar, other tabs)
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Esc => {
                                if is_double_esc(&mut last_esc_press) {
                                    break;
                                }
                            }
                            KeyCode::Char('c')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                break
                            }
                            KeyCode::Char('Q') => {
                                let mut names: Vec<&str> =
                                    agents.iter().map(|a| a.hub.as_str()).collect();
                                names.sort();
                                names.dedup();
                                hub_count = names.len().max(1);
                                // Collect worktree info before stopping
                                worktree_cleanups = crate::worktree::collect_worktree_cleanups(
                                    &agents, &agents,
                                );
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
                                        overview_state
                                            .sync_agents(&agents, last_content_area);
                                    } else {
                                        overview_state.force_resize_all();
                                    }
                                }
                            }
                            KeyCode::BackTab => {
                                active_tab = active_tab.prev();
                                if active_tab == ActiveTab::Overview {
                                    if !overview_state.initialized {
                                        overview_state
                                            .sync_agents(&agents, last_content_area);
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
                                let shift =
                                    key.modifiers.contains(KeyModifiers::SHIFT);
                                match key.code {
                                    KeyCode::Down if shift => {
                                        overview_state.enter_terminal();
                                        overview_state.force_resize_focused();
                                    }
                                    KeyCode::Left if shift => {
                                        overview_state
                                            .scroll_left();
                                    }
                                    KeyCode::Right if shift => {
                                        overview_state
                                            .scroll_right(last_content_area.width);
                                    }
                                    _ => {}
                                }
                            }
                            // Repositories tab navigation
                            _ if active_tab == ActiveTab::Repositories => {
                                match key.code {
                                    KeyCode::Left
                                        if key
                                            .modifiers
                                            .contains(KeyModifiers::SHIFT) =>
                                    {
                                        focus = FocusPanel::Left;
                                    }
                                    KeyCode::Right
                                        if key
                                            .modifiers
                                            .contains(KeyModifiers::SHIFT) =>
                                    {
                                        focus = FocusPanel::Right;
                                    }
                                    KeyCode::Enter => {
                                        if focus == FocusPanel::Left {
                                            match selection.level {
                                                TreeLevel::Repo => {
                                                    // Open context menu for real repos
                                                    if let Some(repo) = display_repos.get(selection.repo_idx) {
                                                        if !repo.path.is_empty() {
                                                            active_menu = Some(ActiveMenu::RepoActions {
                                                                repo_path: repo.path.clone(),
                                                                menu: ContextMenu::new(
                                                                    &repo.name,
                                                                    vec![
                                                                        "Change Color".to_string(),
                                                                        "Open in File System".to_string(),
                                                                        "Open in Terminal".to_string(),
                                                                        "Stop All Agents".to_string(),
                                                                        "Unregister".to_string(),
                                                                        "Clean Stale Refs".to_string(),
                                                                        "Purge".to_string(),
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
                                                    if let Some(repo) = display_repos.get(selection.repo_idx) {
                                                        if repo.path.is_empty() {
                                                            // Skip "No Repository"
                                                        } else if selection.category_idx == 0 {
                                                            // Local branch context menu
                                                            if let Some(branch) = repo.local_branches.get(selection.branch_idx) {
                                                                let matching: Vec<AgentInfo> = agents
                                                                    .iter()
                                                                    .filter(|a| {
                                                                        a.repo_path.as_deref() == Some(&*repo.path)
                                                                            && a.branch_name.as_deref() == Some(&*branch.name)
                                                                    })
                                                                    .cloned()
                                                                    .collect();
                                                                let mut labels = Vec::new();
                                                                let mut actions = Vec::new();
                                                                labels.push("Start Agent (worktree)".to_string());
                                                                actions.push(BranchAction::StartAgent);
                                                                if branch.is_head {
                                                                    labels.push("Start Agent (in place)".to_string());
                                                                    actions.push(BranchAction::StartAgentInPlace);
                                                                }
                                                                labels.push("Base Worktree Off".to_string());
                                                                actions.push(BranchAction::BaseWorktreeOff);
                                                                labels.push("Pull".to_string());
                                                                actions.push(BranchAction::Pull);
                                                                if branch.active_agent_count > 0 {
                                                                    labels.push("Stop Agents".to_string());
                                                                    actions.push(BranchAction::StopAgents);
                                                                    labels.push("Open Agent".to_string());
                                                                    actions.push(BranchAction::OpenAgent);
                                                                }
                                                                if branch.is_worktree {
                                                                    labels.push("Remove Worktree".to_string());
                                                                    actions.push(BranchAction::RemoveWorktree);
                                                                }
                                                                labels.push("Delete Branch".to_string());
                                                                actions.push(BranchAction::DeleteBranch);
                                                                active_menu = Some(ActiveMenu::BranchActions {
                                                                    repo_path: repo.path.clone(),
                                                                    branch_name: branch.name.clone(),
                                                                    is_head: branch.is_head,
                                                                    agents: matching,
                                                                    actions,
                                                                    menu: ContextMenu::new(&branch.name, labels),
                                                                });
                                                            }
                                                        } else if selection.category_idx == 1 {
                                                            // Remote branch context menu
                                                            if let Some(branch) = repo.remote_branches.get(selection.branch_idx) {
                                                                let mut labels = Vec::new();
                                                                let mut actions = Vec::new();
                                                                labels.push("Start Agent (checkout)".to_string());
                                                                actions.push(BranchAction::RemoteStartAgent);
                                                                labels.push("Create Worktree".to_string());
                                                                actions.push(BranchAction::RemoteCreateWorktree);
                                                                labels.push("Delete Remote Branch".to_string());
                                                                actions.push(BranchAction::DeleteRemoteBranch);
                                                                active_menu = Some(ActiveMenu::BranchActions {
                                                                    repo_path: repo.path.clone(),
                                                                    branch_name: branch.name.clone(),
                                                                    is_head: false,
                                                                    agents: vec![],
                                                                    actions,
                                                                    menu: ContextMenu::new(&branch.name, labels),
                                                                });
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        } else if focus == FocusPanel::Right {
                                            if let Some(agent) = resolve_selected_agent(
                                                &agents,
                                                &agent_selection,
                                                agent_view_mode,
                                            ) {
                                                let agent_id = agent.id.clone();
                                                let agent_binary =
                                                    agent.agent_binary.clone();
                                                let working_dir =
                                                    agent.working_dir.clone();
                                                let agent_repo_path = agent.repo_path.clone();
                                                let agent_branch = agent.branch_name.clone();
                                                let agent_is_wt = agent.is_worktree;
                                                let fm_cols = (last_content_area.width
                                                    * 40
                                                    / 100)
                                                    .saturating_sub(2)
                                                    .max(1);
                                                let fm_rows = last_content_area
                                                    .height
                                                    .saturating_sub(3)
                                                    .max(1);
                                                focus_mode_state.open_agent(
                                                    &agent_id,
                                                    &agent_binary,
                                                    fm_cols,
                                                    fm_rows,
                                                    &working_dir,
                                                    agent_repo_path.as_deref(),
                                                    agent_branch.as_deref(),
                                                    agent_is_wt,
                                                );
                                                in_focus_mode = true;
                                            }
                                        }
                                    }
                                    KeyCode::Char('v')
                                        if focus == FocusPanel::Right =>
                                    {
                                        agent_view_mode = match agent_view_mode {
                                            AgentViewMode::ByHub => {
                                                AgentViewMode::ByRepo
                                            }
                                            AgentViewMode::ByRepo => {
                                                AgentViewMode::ByHub
                                            }
                                        };
                                        agent_selection = AgentSelection::default();
                                    }
                                    KeyCode::Up => match focus {
                                        FocusPanel::Left => {
                                            selection.move_up(&display_repos)
                                        }
                                        FocusPanel::Right => {
                                            agent_selection
                                                .move_up(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Down => match focus {
                                        FocusPanel::Left => {
                                            selection.move_down(&display_repos)
                                        }
                                        FocusPanel::Right => {
                                            agent_selection
                                                .move_down(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Right => match focus {
                                        FocusPanel::Left => {
                                            selection.descend(&display_repos)
                                        }
                                        FocusPanel::Right => {
                                            agent_selection
                                                .next_group(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Left => match focus {
                                        FocusPanel::Left => selection.ascend(&display_repos),
                                        FocusPanel::Right => {
                                            agent_selection
                                                .prev_group(&agents, agent_view_mode)
                                        }
                                    },
                                    KeyCode::Char(' ') if focus == FocusPanel::Left => {
                                        selection.toggle_collapse();
                                    }
                                    _ => {}
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
                        overview_state
                            .handle_resize(agents.len(), new_content_area);
                    }
                    if in_focus_mode {
                        let fm_cols = (new_content_area.width * 40 / 100)
                            .saturating_sub(2)
                            .max(1);
                        let fm_rows =
                            new_content_area.height.saturating_sub(3).max(1);
                        focus_mode_state.handle_resize(fm_cols, fm_rows);
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
                            overview_state
                                .handle_resize(agents.len(), new_content_area);
                            overview_state.force_resize_all();
                        }
                        if in_focus_mode && focus_mode_state.is_active() {
                            let fm_cols = (new_content_area.width * 40 / 100)
                                .saturating_sub(2)
                                .max(1);
                            let fm_rows =
                                new_content_area.height.saturating_sub(3).max(1);
                            focus_mode_state.handle_resize(fm_cols, fm_rows);
                            focus_mode_state.force_resize();
                        }
                    }
                }
                Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers }) if mouse_captured => {
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
                            let item_count = match active_menu.as_ref().unwrap() {
                                ActiveMenu::AgentPicker { menu, .. } => menu.items.len(),
                                ActiveMenu::RepoActions { menu, .. } => menu.items.len(),
                                ActiveMenu::ColorPicker { menu, .. } => menu.items.len(),
                                ActiveMenu::BranchActions { menu, .. } => menu.items.len(),
                                ActiveMenu::ConfirmAction { menu, .. } => menu.items.len(),
                                ActiveMenu::WorktreeCleanup { menu, .. } => menu.items.len(),
                            };
                            if idx < item_count {
                                // Highlight the clicked item then select it
                                match active_menu.as_mut().unwrap() {
                                    ActiveMenu::AgentPicker { menu, .. } => menu.selected_idx = idx,
                                    ActiveMenu::RepoActions { menu, .. } => menu.selected_idx = idx,
                                    ActiveMenu::ColorPicker { menu, .. } => menu.selected_idx = idx,
                                    ActiveMenu::BranchActions { menu, .. } => menu.selected_idx = idx,
                                    ActiveMenu::ConfirmAction { menu, .. } => menu.selected_idx = idx,
                                    ActiveMenu::WorktreeCleanup { menu, .. } => menu.selected_idx = idx,
                                }
                                let taken = active_menu.take().unwrap();
                                match taken {
                                    ActiveMenu::AgentPicker { agents: picker_agents, .. } => {
                                        if let Some(agent) = picker_agents.get(idx) {
                                            let agent_id = agent.id.clone();
                                            let agent_binary = agent.agent_binary.clone();
                                            let working_dir = agent.working_dir.clone();
                                            let fm_cols = (last_content_area.width * 40 / 100)
                                                .saturating_sub(2)
                                                .max(1);
                                            let fm_rows =
                                                last_content_area.height.saturating_sub(3).max(1);
                                            focus_mode_state.open_agent(
                                                &agent_id,
                                                &agent_binary,
                                                fm_cols,
                                                fm_rows,
                                                &working_dir,
                                                agent.repo_path.as_deref(),
                                                agent.branch_name.as_deref(),
                                                agent.is_worktree,
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
                                                    .filter(|a| a.repo_path.as_deref() == Some(&*repo_path))
                                                    .cloned()
                                                    .collect();
                                                stop_repo_agents_ipc(&repo_path);
                                                last_repo_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch =
                                                    Instant::now() - Duration::from_secs(10);
                                                let cleanups = crate::worktree::collect_worktree_cleanups(
                                                    &repo_agents, &agents,
                                                );
                                                if !cleanups.is_empty() {
                                                    pending_worktree_cleanups = cleanups;
                                                    active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
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
                                        if let Some(&color_name) =
                                            theme::REPO_COLOR_NAMES.get(idx)
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
                                                    let (cols, rows) =
                                                        crossterm::terminal::size()
                                                            .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                                target_branch: Some(bn),
                                                                new_branch: None,
                                                                prompt: None,
                                                                agent_binary: None,
                                                                cols,
                                                                rows: rows
                                                                    .saturating_sub(2)
                                                                    .max(1),
                                                                accept_edits: false,
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
                                                BranchAction::StartAgentInPlace => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let rp = repo_path.clone();
                                                    let (cols, rows) =
                                                        crossterm::terminal::size()
                                                            .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                            rows: rows
                                                                .saturating_sub(2)
                                                                .max(1),
                                                            accept_edits: false,
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
                                                                repo_path,
                                                                branch_name,
                                                                ..
                                                            }) => {
                                                                let working_dir = repo_path
                                                                    .clone()
                                                                    .unwrap_or_default();
                                                                let _ = tx
                                                                    .send(AgentStartResult::Started {
                                                                        agent_id: id,
                                                                        agent_binary,
                                                                        working_dir,
                                                                        repo_path,
                                                                        branch_name,
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
                                                BranchAction::Pull => {
                                                    let tx = status_tx.clone();
                                                    let rp = repo_path.clone();
                                                    let bn = branch_name.clone();
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                            Ok(HubMessage::BranchPulled { branch_name, .. }) => {
                                                                let _ = tx.send(StatusMessage {
                                                                    text: format!("Pulled {branch_name}"),
                                                                    level: StatusLevel::Success,
                                                                    created: Instant::now(),
                                                                }).await;
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
                                                    let cleanups = crate::worktree::collect_worktree_cleanups(
                                                        &branch_agents, &agents,
                                                    );
                                                    if !cleanups.is_empty() {
                                                        pending_worktree_cleanups = cleanups;
                                                        active_menu = pop_worktree_cleanup_menu(&mut pending_worktree_cleanups);
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
                                                        focus_mode_state.open_agent(
                                                            &agent.id,
                                                            &agent.agent_binary,
                                                            fm_cols,
                                                            fm_rows,
                                                            &agent.working_dir,
                                                            agent.repo_path.as_deref(),
                                                            agent.branch_name.as_deref(),
                                                            agent.is_worktree,
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
                                                    if let Some(repo_info) = repos.iter().find(|r| r.path == repo_path).cloned() {
                                                        create_modal = Some(CreateAgentModal::new_with_branch(
                                                            repos.clone(),
                                                            repo_info,
                                                            branch_name.clone(),
                                                        ));
                                                    }
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
                                                        let (cols, rows) =
                                                            crossterm::terminal::size()
                                                                .unwrap_or((80, 24));
                                                        tokio::spawn(async move {
                                                            let mut stream =
                                                                match ipc::try_connect().await
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
                                                                CliMessage::CreateWorktreeAgent
                                                                {
                                                                    repo_path: rp,
                                                                    target_branch: Some(
                                                                        remote_ref,
                                                                    ),
                                                                    new_branch: Some(
                                                                        local_name,
                                                                    ),
                                                                    prompt: None,
                                                                    agent_binary: None,
                                                                    cols,
                                                                    rows: rows
                                                                        .saturating_sub(2)
                                                                        .max(1),
                                                                    accept_edits: false,
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
                                            }
                                        }
                                    }
                                    ActiveMenu::ConfirmAction { action, .. } => {
                                        if idx == 0 {
                                            match action {
                                                ConfirmedAction::PurgeRepo { repo_path } => {
                                                    purge_progress = Some(start_purge_async(&repo_path));
                                                }
                                                ConfirmedAction::StartAgentDetach { repo_path, branch_name } => {
                                                    let tx = agent_start_tx.clone();
                                                    let hub = hub_name.to_string();
                                                    let (cols, rows) =
                                                        crossterm::terminal::size()
                                                            .unwrap_or((80, 24));
                                                    tokio::spawn(async move {
                                                        let mut stream =
                                                            match ipc::try_connect().await {
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
                                                                repo_path,
                                                                target_branch: Some(branch_name),
                                                                new_branch: None,
                                                                prompt: None,
                                                                agent_binary: None,
                                                                cols,
                                                                rows: rows
                                                                    .saturating_sub(2)
                                                                    .max(1),
                                                                accept_edits: false,
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
                                        }
                                    }
                                    ActiveMenu::WorktreeCleanup { repo_path, branch_name, .. } => {
                                        match idx {
                                            1 => {
                                                remove_worktree_ipc(&repo_path, &branch_name, false, true);
                                                last_repo_fetch = Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch = Instant::now() - Duration::from_secs(10);
                                            }
                                            2 => {
                                                remove_worktree_ipc(&repo_path, &branch_name, true, true);
                                                last_repo_fetch = Instant::now() - Duration::from_secs(10);
                                                last_agent_fetch = Instant::now() - Duration::from_secs(10);
                                            }
                                            _ => {}
                                        }
                                        if let Some(next) = pending_worktree_cleanups.pop() {
                                            let dirty = crate::worktree::is_worktree_dirty(&next.repo_path, &next.branch_name);
                                            let title = if dirty {
                                                format!("Worktree '{}' (uncommitted changes)", next.branch_name)
                                            } else {
                                                format!("Worktree '{}'", next.branch_name)
                                            };
                                            active_menu = Some(ActiveMenu::WorktreeCleanup {
                                                repo_path: next.repo_path,
                                                branch_name: next.branch_name,
                                                menu: ContextMenu::new(&title, vec![
                                                    "Keep".to_string(),
                                                    "Discard worktree".to_string(),
                                                    "Discard worktree + branch".to_string(),
                                                ]),
                                            });
                                        }
                                    }
                                }
                            }
                        } else if !click_map.menu_modal_rect.contains(pos) {
                            // Click outside modal → dismiss
                            active_menu = None;
                        }
                    } else if in_focus_mode {
                        // Focus mode click handling
                        if click_map.focus_back_button.contains(pos) {
                            focus_mode_state.shutdown();
                            in_focus_mode = false;
                            if active_tab == ActiveTab::Overview
                                && overview_state.initialized
                            {
                                overview_state.force_resize_all();
                            }
                        } else if let Some((_, tab)) = click_map.focus_left_tabs.iter().find(|(r, _)| r.contains(pos)) {
                            focus_mode_state.left_tab = *tab;
                            focus_mode_state.focus_side = overview::FocusSide::Left;
                        } else if click_map.focus_left_area.contains(pos) {
                            focus_mode_state.focus_side = overview::FocusSide::Left;
                        } else if click_map.focus_right_area.contains(pos) {
                            focus_mode_state.focus_side = overview::FocusSide::Right;
                        }
                    } else if let Some((_, tab)) = click_map.tabs.iter().find(|(r, _)| r.contains(pos)) {
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
                                                if selection.level == TreeLevel::Repo && selection.repo_idx == *ri {
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
                                    focus = FocusPanel::Left;
                                }
                                // Mode label click (right panel) → toggle view mode
                                else if click_map.mode_label_area.contains(pos) {
                                    agent_view_mode = match agent_view_mode {
                                        AgentViewMode::ByHub => AgentViewMode::ByRepo,
                                        AgentViewMode::ByRepo => AgentViewMode::ByHub,
                                    };
                                    agent_selection = AgentSelection::default();
                                    focus = FocusPanel::Right;
                                }
                                // Agent card clicks (right panel)
                                else if let Some((_, gidx, aidx)) = click_map.agent_cards.iter().find(|(r, _, _)| r.contains(pos)) {
                                    agent_selection.group_idx = *gidx;
                                    agent_selection.agent_idx = *aidx;
                                    focus = FocusPanel::Right;
                                }
                                // Panel focus switching (click anywhere in a panel)
                                else if click_map.left_panel_area.contains(pos) {
                                    focus = FocusPanel::Left;
                                } else if click_map.right_panel_area.contains(pos) {
                                    focus = FocusPanel::Right;
                                }
                            }
                            ActiveTab::Overview => {
                                if let Some((_, idx)) = click_map.overview_panels.iter().find(|(r, _)| r.contains(pos)) {
                                    overview_state.focus = overview::OverviewFocus::Terminal(*idx);
                                }
                            }
                        }
                    }
                }
                Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollUp, column, row, .. }) if mouse_captured => {
                    let pos = Position { x: column, y: row };
                    if let Some(ref mut menu_variant) = active_menu {
                        match menu_variant {
                            ActiveMenu::AgentPicker { menu, .. }
                            | ActiveMenu::RepoActions { menu, .. }
                            | ActiveMenu::ColorPicker { menu, .. }
                            | ActiveMenu::BranchActions { menu, .. }
                            | ActiveMenu::ConfirmAction { menu, .. }
                            | ActiveMenu::WorktreeCleanup { menu, .. } => {
                                menu.selected_idx = menu.selected_idx.saturating_sub(1);
                            }
                        }
                    } else if in_focus_mode && focus_mode_state.is_active() {
                        if click_map.focus_right_area.contains(pos) {
                            if let Some(panel) = &mut focus_mode_state.panel {
                                let max = panel.vterm.scrollback_len();
                                panel.panel_scroll_offset =
                                    (panel.panel_scroll_offset + 3).min(max);
                            }
                        } else if click_map.focus_left_area.contains(pos) {
                            focus_mode_state.diff_scroll_up();
                        }
                    } else if active_tab == ActiveTab::Overview {
                        if let Some((_, idx)) = click_map.overview_panels.iter().find(|(r, _)| r.contains(pos)) {
                            if let Some(panel) = overview_state.panels.get_mut(*idx) {
                                let max = panel.vterm.scrollback_len();
                                panel.panel_scroll_offset = (panel.panel_scroll_offset + 3).min(max);
                            }
                        }
                    }
                }
                Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column, row, .. }) if mouse_captured => {
                    let pos = Position { x: column, y: row };
                    if let Some(ref mut menu_variant) = active_menu {
                        match menu_variant {
                            ActiveMenu::AgentPicker { menu, .. }
                            | ActiveMenu::RepoActions { menu, .. }
                            | ActiveMenu::ColorPicker { menu, .. }
                            | ActiveMenu::BranchActions { menu, .. }
                            | ActiveMenu::ConfirmAction { menu, .. }
                            | ActiveMenu::WorktreeCleanup { menu, .. } => {
                                if !menu.items.is_empty() {
                                    menu.selected_idx = (menu.selected_idx + 1).min(menu.items.len() - 1);
                                }
                            }
                        }
                    } else if in_focus_mode && focus_mode_state.is_active() {
                        if click_map.focus_right_area.contains(pos) {
                            if let Some(panel) = &mut focus_mode_state.panel {
                                panel.panel_scroll_offset =
                                    panel.panel_scroll_offset.saturating_sub(3);
                            }
                        } else if click_map.focus_left_area.contains(pos) {
                            focus_mode_state.diff_scroll_down();
                        }
                    } else if active_tab == ActiveTab::Overview {
                        if let Some((_, idx)) = click_map.overview_panels.iter().find(|(r, _)| r.contains(pos)) {
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
    let tabs = [ActiveTab::Repositories, ActiveTab::Overview];
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
            Rect { x: cursor_x, y: area.y, width: label_width, height: 1 },
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
    let back_width: u16 = spans.iter().map(|s| s.content.chars().count()).sum::<usize>() as u16;
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

    // Right: keyboard hints
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

        // Repo name header with collapse chevron
        let chevron = if repo_collapsed { "▸" } else { "▾" };
        let repo_clr = repo
            .color
            .as_deref()
            .map(theme::repo_color)
            .unwrap_or(theme::R_ACCENT);

        // Selected: hover bg with colored text; otherwise: reverse-video (repo color bg, dark text)
        let (line_bg, chev_fg, dot_fg, name_fg) = if repo_selected {
            (Some(theme::R_BG_HOVER), theme::R_TEXT_TERTIARY, repo_clr, repo_clr)
        } else {
            (Some(repo_clr), theme::R_BG_BASE, theme::R_BG_BASE, theme::R_BG_BASE)
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
                let mut name_style =
                    Style::default().fg(name_color).add_modifier(Modifier::BOLD);
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
    }

    pad_line(spans, width, bg)
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

#[allow(clippy::too_many_arguments)]
fn render_right_panel(
    frame: &mut Frame,
    area: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode: AgentViewMode,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );

    if agents.is_empty() {
        render_logo(frame, area);
        // Focus indicator even when empty
        let indicator_color = if focused {
            theme::R_ACCENT_BRIGHT
        } else {
            theme::R_TEXT_TERTIARY
        };
        let indicator = Paragraph::new(Span::styled(
            "●",
            Style::default().fg(indicator_color).bg(theme::R_BG_BASE),
        ))
        .alignment(Alignment::Right);
        let indicator_area = Rect {
            x: area.x + 1,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        };
        frame.render_widget(indicator, indicator_area);
    } else {
        render_agent_list(frame, area, agents, agent_sel, focused, mode, click_map, repo_colors);
    }
}

fn render_logo(frame: &mut Frame, area: Rect) {
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

#[allow(clippy::too_many_arguments)]
fn render_agent_list(
    frame: &mut Frame,
    area: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode: AgentViewMode,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
) {
    let block = Block::default().padding(Padding::new(2, 2, 1, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Mode label + hint
    let mode_label = match mode {
        AgentViewMode::ByHub => "by hub",
        AgentViewMode::ByRepo => "by repo",
    };
    let mode_line = Paragraph::new(Line::from(vec![
        Span::styled(mode_label, Style::default().fg(theme::R_TEXT_TERTIARY)),
        Span::styled("  v to switch", Style::default().fg(theme::R_TEXT_TERTIARY)),
    ]));

    // Focus indicator in top-right corner (overlaid on mode label line)
    let indicator_color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_TERTIARY
    };
    let indicator = Paragraph::new(Span::styled("●", Style::default().fg(indicator_color)))
        .alignment(Alignment::Right);

    match mode {
        AgentViewMode::ByHub => render_agent_list_by_hub(
            frame, inner, agents, agent_sel, focused, &mode_line, &indicator, click_map, repo_colors,
        ),
        AgentViewMode::ByRepo => render_agent_list_by_repo(
            frame, inner, agents, agent_sel, focused, &mode_line, &indicator, click_map, repo_colors,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_agent_list_by_hub(
    frame: &mut Frame,
    inner: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode_line: &Paragraph<'_>,
    indicator: &Paragraph<'_>,
    click_map: &mut ClickMap,
    _repo_colors: &HashMap<String, String>,
) {
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by(|a, b| a.hub.cmp(&b.hub).then(a.started_at.cmp(&b.started_at)));

    let mut pnames: Vec<&str> = sorted.iter().map(|a| a.hub.as_str()).collect();
    pnames.dedup();

    let hide_headers = pnames.len() == 1 && pnames[0] == DEFAULT_HUB;

    // Build layout: mode label + spacer + hub headers + agent cards + gaps
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // mode label
        Constraint::Length(1), // spacer
    ];
    for (pidx, hub_name) in pnames.iter().enumerate() {
        if !hide_headers {
            if pidx > 0 {
                constraints.push(Constraint::Length(1)); // gap before hub header
            }
            constraints.push(Constraint::Length(1)); // hub header
            constraints.push(Constraint::Length(1)); // spacer after header
        }
        let count = sorted.iter().filter(|a| a.hub == *hub_name).count();
        for i in 0..count {
            constraints.push(Constraint::Length(4)); // agent card
            if i < count - 1 {
                constraints.push(Constraint::Length(1)); // gap between cards
            }
        }
    }
    constraints.push(Constraint::Min(0));

    let areas = Layout::vertical(constraints).split(inner);

    frame.render_widget(mode_line.clone(), areas[0]);
    click_map.mode_label_area = areas[0];
    frame.render_widget(
        indicator.clone(),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    let mut area_idx = 2;
    for (pidx, hub_name) in pnames.iter().enumerate() {
        if !hide_headers {
            if pidx > 0 {
                area_idx += 1; // skip gap before hub header
            }
            let hub_header = Paragraph::new(Line::from(vec![Span::styled(
                format!(" {hub_name}"),
                Style::default().fg(theme::R_ACCENT),
            )]));
            frame.render_widget(hub_header, areas[area_idx]);
            area_idx += 1;
            area_idx += 1; // skip spacer after header
        }

        let agents_in_hub: Vec<(usize, &&AgentInfo)> = sorted
            .iter()
            .filter(|a| a.hub == *hub_name)
            .enumerate()
            .collect();
        let agent_count = agents_in_hub.len();
        for (aidx, agent) in agents_in_hub {
            let is_selected = focused && pidx == agent_sel.group_idx && aidx == agent_sel.agent_idx;
            click_map.agent_cards.push((areas[area_idx], pidx, aidx));
            render_agent_card(frame, areas[area_idx], agent, is_selected, false);
            area_idx += 1;
            if aidx < agent_count - 1 {
                area_idx += 1; // skip gap between cards
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_agent_list_by_repo(
    frame: &mut Frame,
    inner: Rect,
    agents: &[AgentInfo],
    agent_sel: &AgentSelection,
    focused: bool,
    mode_line: &Paragraph<'_>,
    indicator: &Paragraph<'_>,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
) {
    let mut sorted: Vec<&AgentInfo> = agents.iter().collect();
    sorted.sort_by(|a, b| {
        let ak = agent_group_key(a, AgentViewMode::ByRepo);
        let bk = agent_group_key(b, AgentViewMode::ByRepo);
        ak.cmp(&bk)
            .then(a.branch_name.cmp(&b.branch_name))
            .then(a.started_at.cmp(&b.started_at))
    });

    let gnames = group_names(agents, AgentViewMode::ByRepo);

    let hide_headers = gnames.len() == 1 && gnames[0] == NO_REPOSITORY;

    // Build layout: mode label + spacer + repo/branch headers + agent cards + gaps
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // mode label
        Constraint::Length(1), // spacer
    ];
    for (ridx, repo) in gnames.iter().enumerate() {
        let is_no_repo = repo == NO_REPOSITORY;
        if !hide_headers {
            if ridx > 0 {
                constraints.push(Constraint::Length(1)); // empty line gap between repos
            }
            constraints.push(Constraint::Length(1)); // repo header
        }
        let mut branches: Vec<&str> = sorted
            .iter()
            .filter(|a| agent_group_key(a, AgentViewMode::ByRepo) == *repo)
            .map(|a| a.branch_name.as_deref().unwrap_or("no branch"))
            .collect();
        branches.dedup();
        for branch in &branches {
            if !is_no_repo {
                constraints.push(Constraint::Length(1)); // branch sub-header
            }
            let count = sorted
                .iter()
                .filter(|a| {
                    agent_group_key(a, AgentViewMode::ByRepo) == *repo
                        && a.branch_name.as_deref().unwrap_or("no branch") == *branch
                })
                .count();
            for i in 0..count {
                constraints.push(Constraint::Length(4)); // agent card
                if i < count - 1 {
                    constraints.push(Constraint::Length(1)); // gap between cards
                }
            }
        }
    }
    constraints.push(Constraint::Min(0));

    let areas = Layout::vertical(constraints).split(inner);

    frame.render_widget(mode_line.clone(), areas[0]);
    click_map.mode_label_area = areas[0];
    frame.render_widget(
        indicator.clone(),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    let mut area_idx = 2;
    let mut flat_agent_idx = 0;
    for (gidx, repo) in gnames.iter().enumerate() {
        let is_no_repo = repo == NO_REPOSITORY;
        if !hide_headers {
            if gidx > 0 {
                area_idx += 1; // skip empty line gap between repos
            }
            // Repo header — reverse video: repo color background, dark text
            let repo_display = std::path::Path::new(repo)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| repo.clone());
            let header_color = repo_colors
                .get(repo.as_str())
                .map(|c| theme::repo_color(c))
                .unwrap_or(theme::R_ACCENT);
            let repo_header = Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {repo_display} "),
                    Style::default()
                        .fg(header_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            frame.render_widget(repo_header, areas[area_idx]);
            area_idx += 1;
        }

        // Branch sub-groups
        let repo_agents: Vec<&AgentInfo> = sorted
            .iter()
            .filter(|a| agent_group_key(a, AgentViewMode::ByRepo) == *repo)
            .copied()
            .collect();
        let mut branches: Vec<&str> = repo_agents
            .iter()
            .map(|a| a.branch_name.as_deref().unwrap_or("no branch"))
            .collect();
        branches.dedup();

        for branch in &branches {
            if !is_no_repo {
                // Branch sub-header
                let branch_header = Paragraph::new(Line::from(vec![Span::styled(
                    format!("   {branch}"),
                    Style::default().fg(theme::R_TEXT_SECONDARY),
                )]));
                frame.render_widget(branch_header, areas[area_idx]);
                area_idx += 1;
            }

            let branch_agents: Vec<&&AgentInfo> = repo_agents
                .iter()
                .filter(|a| a.branch_name.as_deref().unwrap_or("no branch") == *branch)
                .collect();
            let branch_agent_count = branch_agents.len();
            for (bidx, agent) in branch_agents.into_iter().enumerate() {
                let is_selected =
                    focused && gidx == agent_sel.group_idx && flat_agent_idx == agent_sel.agent_idx;
                click_map.agent_cards.push((areas[area_idx], gidx, flat_agent_idx));
                render_agent_card(
                    frame,
                    areas[area_idx],
                    agent,
                    is_selected,
                    agent.repo_path.is_none(),
                );
                area_idx += 1;
                flat_agent_idx += 1;
                if bidx < branch_agent_count - 1 {
                    area_idx += 1; // skip gap between cards
                }
            }
        }
        flat_agent_idx = 0; // reset for next repo group
    }
}

fn render_agent_card(
    frame: &mut Frame,
    area: Rect,
    agent: &AgentInfo,
    is_selected: bool,
    show_working_dir: bool,
) {
    let bg = if is_selected {
        theme::R_BG_HOVER
    } else {
        theme::R_BG_SURFACE
    };
    let block = Block::default()
        .style(Style::default().bg(bg))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let started = format_started(&agent.started_at);
    let attached = format_attached(agent.attached_clients);

    let mut lines = vec![
        Line::from(if is_selected {
            vec![
                Span::styled(&agent.id, Style::default().fg(theme::R_ACCENT)),
                Span::styled("  Enter", Style::default().fg(theme::R_TEXT_TERTIARY)),
            ]
        } else {
            vec![Span::styled(&agent.id, Style::default().fg(theme::R_ACCENT))]
        }),
        Line::from({
            let mut spans = vec![
                Span::styled(
                    agent.agent_binary.clone(),
                    Style::default().fg(theme::R_TEXT_PRIMARY),
                ),
                Span::raw("  "),
                Span::styled("● running", Style::default().fg(theme::R_SUCCESS)),
            ];
            if let Some(ref branch) = agent.branch_name {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("\u{e0a0} {branch}"),
                    Style::default().fg(theme::R_TEXT_SECONDARY),
                ));
            }
            spans
        }),
        Line::from(vec![
            Span::styled(
                format!("started {started}"),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            ),
            Span::raw("    "),
            Span::styled(
                format!("attached: {attached}"),
                Style::default().fg(theme::R_TEXT_SECONDARY),
            ),
        ]),
    ];

    if show_working_dir {
        lines.push(Line::from(Span::styled(
            agent.working_dir.clone(),
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
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
        left_spans.push(Span::styled("  ", Style::default().bg(theme::R_BG_RAISED)));
        left_spans.push(Span::styled(
            "MOUSE OFF \u{00b7} F2",
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
        let mod_key = if cfg!(target_os = "macos") { "Opt" } else { "Alt" };
        let hint_text = if in_focus_mode {
            format!("Shift+\u{2190}/\u{2192} switch panel  Shift+\u{2191} exit  {mod_key}+R new agent")
        } else if active_tab == ActiveTab::Overview {
            match overview_focus {
                OverviewFocus::Terminal(_) => {
                    format!("Shift+\u{2191} options  Shift+\u{2193} focus  Shift+\u{2190}/\u{2192} switch agent  {mod_key}+R new agent")
                }
                OverviewFocus::OptionsBar => {
                    format!("Shift+\u{2193} enter terminal  Shift+\u{2190}/\u{2192} scroll  {mod_key}+R new agent  q quit  ? keys")
                }
            }
        } else {
            format!("{mod_key}+R new agent  q quit  Q stop+quit  ? keys")
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
            Span::styled(
                desc.to_string(),
                Style::default().fg(theme::R_TEXT_PRIMARY),
            ),
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
    lines.push(binding_line("Alt+E", "Create agent"));
    lines.push(binding_line("Alt+F", "Search agents"));

    // -- Repositories --
    if active_tab == ActiveTab::Repositories {
        lines.push(Line::from(""));
        lines.push(header_line("Repositories"));
        lines.push(binding_line("\u{2191} / \u{2193}", "Navigate items"));
        lines.push(binding_line("\u{2190} / \u{2192}", "Navigate tree"));
        lines.push(binding_line("Shift+\u{2190}/\u{2192}", "Switch panel"));
        lines.push(binding_line("Enter", "Open menu / focus agent"));
        lines.push(binding_line("Space", "Collapse / expand"));
        lines.push(binding_line("v", "Toggle agent grouping"));
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

fn set_repo_color_ipc(path: &str, color: &str) {
    let path = path.to_string();
    let color = color.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::SetRepoColor { path, color },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn stop_repo_agents_ipc(path: &str) {
    let path = path.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::StopRepoAgents { path },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn unregister_repo_ipc(path: &str) {
    let path = path.to_string();
    block_on_async(async {
        let Ok(mut stream) = ipc::try_connect().await else {
            return;
        };
        let _ = clust_ipc::send_message(
            &mut stream,
            &CliMessage::UnregisterRepo { path },
        )
        .await;
        let _ = clust_ipc::recv_message::<HubMessage>(&mut stream).await;
    });
}

fn stop_agents_ipc(agent_ids: &[String]) {
    for id in agent_ids {
        let id = id.clone();
        block_on_async(async {
            let Ok(mut stream) = ipc::try_connect().await else {
                return;
            };
            let _ = clust_ipc::send_message(
                &mut stream,
                &CliMessage::StopAgent { id },
            )
            .await;
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
        menu: ContextMenu::new(&title, vec![
            "Keep".to_string(),
            "Discard worktree".to_string(),
            "Discard worktree + branch".to_string(),
        ]),
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
    let spinner_idx =
        (progress.started.elapsed().as_millis() / 120) as usize % SPINNER_CHARS.len();
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

/// Run an async future from the synchronous UI loop.
/// Requires the multi-thread tokio scheduler (`#[tokio::main]`).
fn block_on_async<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
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
        }
    }

    fn sample_agents() -> Vec<AgentInfo> {
        vec![
            make_agent("aaa111", "alpha"),
            make_agent("aaa222", "alpha"),
            make_agent("bbb111", "beta"),
        ]
    }

    #[test]
    fn agent_selection_default_is_first() {
        let sel = AgentSelection::default();
        assert_eq!(sel.group_idx, 0);
        assert_eq!(sel.agent_idx, 0);
    }

    #[test]
    fn agent_selection_clamp_empty() {
        let mut sel = AgentSelection {
            group_idx: 5,
            agent_idx: 3,
        };
        sel.clamp(&[], AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 0);
        assert_eq!(sel.agent_idx, 0);
    }

    #[test]
    fn agent_selection_clamp_shrinks() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 10,
            agent_idx: 10,
        };
        sel.clamp(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 1); // 2 hubs: alpha, beta
        assert_eq!(sel.agent_idx, 0); // beta has 1 agent
    }

    #[test]
    fn agent_selection_move_down_within_hub() {
        let agents = sample_agents();
        let mut sel = AgentSelection::default(); // hub 0 (alpha), agent 0
        sel.move_down(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.agent_idx, 1); // alpha has 2 agents
        sel.move_down(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.agent_idx, 1); // saturates
    }

    #[test]
    fn agent_selection_move_up_within_hub() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 0,
            agent_idx: 1,
        };
        sel.move_up(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.agent_idx, 0);
        sel.move_up(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.agent_idx, 0); // saturates
    }

    #[test]
    fn agent_selection_next_group() {
        let agents = sample_agents();
        let mut sel = AgentSelection::default();
        sel.next_group(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 1);
        assert_eq!(sel.agent_idx, 0); // reset on group switch
        sel.next_group(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 1); // saturates
    }

    #[test]
    fn agent_selection_prev_group() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 1,
            agent_idx: 0,
        };
        sel.prev_group(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 0);
        assert_eq!(sel.agent_idx, 0);
        sel.prev_group(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 0); // saturates
    }

    #[test]
    fn agent_selection_next_group_resets_agent_idx() {
        let agents = sample_agents();
        let mut sel = AgentSelection {
            group_idx: 0,
            agent_idx: 1,
        };
        sel.next_group(&agents, AgentViewMode::ByHub);
        assert_eq!(sel.group_idx, 1);
        assert_eq!(sel.agent_idx, 0);
    }

    #[test]
    fn group_names_by_hub_sorted_deduped() {
        let agents = sample_agents();
        let names = group_names(&agents, AgentViewMode::ByHub);
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn group_names_by_repo() {
        let agents = vec![
            AgentInfo {
                repo_path: Some("/home/user/project-a".into()),
                branch_name: Some("main".into()),
                ..make_agent("a1", "default")
            },
            AgentInfo {
                repo_path: Some("/home/user/project-b".into()),
                branch_name: Some("dev".into()),
                ..make_agent("b1", "default")
            },
            AgentInfo {
                repo_path: None,
                branch_name: None,
                ..make_agent("c1", "default")
            },
        ];
        let names = group_names(&agents, AgentViewMode::ByRepo);
        assert_eq!(
            names,
            vec![
                "/home/user/project-a",
                "/home/user/project-b",
                NO_REPOSITORY
            ]
        );
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
        assert_eq!(tab.next().next(), ActiveTab::Repositories);
    }

    #[test]
    fn active_tab_prev_cycles() {
        let tab = ActiveTab::Repositories;
        assert_eq!(tab.prev(), ActiveTab::Overview);
        assert_eq!(tab.prev().prev(), ActiveTab::Repositories);
    }

    #[test]
    fn active_tab_labels() {
        assert_eq!(ActiveTab::Repositories.label(), "Repositories");
        assert_eq!(ActiveTab::Overview.label(), "Overview");
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
