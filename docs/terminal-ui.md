# Terminal UI

## Attached Mode

When the CLI is attached to an agent, it takes over the terminal:

```
┌─────────────────────────────────────────────────────┐
│                                                     │
│              Agent PTY output                       │
│              (full terminal area minus bottom bar)   │
│                                                     │
│              This is the agent's real output,       │
│              rendered exactly as the agent writes   │
│              it to its PTY.                         │
│                                                     │
│                                                     │
│                                                     │
├─────────────────────────────────────────────────────┤
│ clust  a3f8c1 │ claude │ Ctrl+Q detach             │
└─────────────────────────────────────────────────────┘
```

### Bottom Status Bar

The bar is always 1 line tall, pinned to the bottom of the terminal.

**Contents (left to right):**

| Section | Example | Description |
|---------|---------|-------------|
| Brand | `clust` | Always shown |
| Agent ID | `a3f8c1` | The 6-char hex ID of the attached agent |
| Agent binary | `claude` | Which agent is running |
| Shortcuts | `Ctrl+Q detach` | Available keyboard shortcuts |

The bar uses a distinct background color (e.g., muted gray/blue) to visually separate it from agent output.

### Rendering Approach

- Put the terminal in **raw mode** (via `crossterm`)
- Reserve the bottom row for the status bar
- Agent PTY output is rendered in the remaining area
- On terminal resize: recalculate layout, redraw bar
- The attached mode status bar is rendered with raw ANSI escape sequences (not ratatui); agent output is passed through directly

### Input Handling

- All keyboard input is forwarded to the agent PTY, **except** recognized clust shortcuts
- Shortcut detection happens first; unmatched input passes through

### Attach Flow

1. CLI sends `AttachAgent { id }` to hub
2. Hub sends the agent's replay buffer contents as `AgentOutput` messages
3. Hub sends `AgentReplayComplete { id }` sentinel
4. Hub begins live-streaming new `AgentOutput` messages
5. CLI enters raw mode, draws status bar
6. During replay (before `AgentReplayComplete`), output is stored in scrollback but suppressed from stdout to prevent a flash of historical content
7. After `AgentReplayComplete`, normal stdout rendering resumes — the agent's own redraw provides a clean screen
8. CLI streams output to terminal, forwards input to hub

### Detach Flow

1. User presses `Ctrl+Q`
2. CLI sends `DetachAgent { id }` to hub
3. CLI exits raw mode, restores terminal
4. CLI exits cleanly (agent continues in hub)

### Background Mode (`-b`)

No terminal takeover. The CLI:

1. Sends `StartAgent` to hub
2. Receives `AgentStarted { id }`
3. Prints the ID to stdout: `Started agent a3f8c1`
4. Exits immediately

## `clust ui` Dashboard

A full terminal UI (TUI) built with `ratatui` + `crossterm`.

### Layout

The dashboard has a top tab bar, two content panels separated by a vertical divider, and a bottom status bar.

#### Tab Bar

A 1-row bar at the top of the terminal with two tabs:

| Tab | Description |
|-----|-------------|
| `Repositories` | Two-panel view with repo tree and agent cards (default) |
| `Overview` | Multi-agent terminal overview with horizontal panels |

The active tab is highlighted with the accent color. A `Tab/Shift+Tab` hint is shown to the right of the tabs. Focus mode is not a tab -- it is an overlay state entered explicitly from either tab (see Focus Mode section below). When focus mode is active, the tab bar is replaced by a back-bar header.

#### Content Panels (Repositories tab)

- **Left panel (40%):** Repository tracker with `(2,2,1,0)` padding. Shows a "Repositories" title on the first row with the focus indicator (`●`) right-aligned on the same line, followed by a 1-row spacer, then the tree content below. Shows a tree view of registered git repositories with their local and remote branches. Repository header lines use reverse-video styling (repo color background, dark text, bold) for visual prominence; when selected, the header uses the standard hover background with colored text instead. An empty line separates each repository group in the tree for visual clarity. Repository names are preceded by a colored `●` dot matching the repo's assigned color (from the 10-color repo palette: red, orange, yellow, lime, green, teal, blue, purple, pink, coral). Tree connectors use `├──` / `└──` for clear hierarchy. Branch names are rendered Bold. Remote branches are collapsed by default. Branches with active agents display a green `●` indicator with count; branches checked out in worktrees display a `⎇` indicator. The current HEAD branch is highlighted using the repo's assigned color. Displays "No repositories found" when no repos are registered. Uses background colors for visual separation (no borders). The focused panel shows a bright accent dot; the unfocused panel shows a dim dot. Agents not associated with any git repository are grouped under a synthetic "No Repository" entry at the bottom of the tree. This entry has no local/remote category level -- agents are listed directly under the repo node with their binary name and working directory. Navigation skips the category level for this group. An "Add Repository" entry with a `+` icon is always appended at the bottom of the tree. Selecting it and pressing Enter (or clicking it) opens the add-repository modal (`RepoModal`).
- **Vertical divider (1 col):** A single-column divider between the two panels.
- **Right panel (60%):** Shows agent cards grouped by repository (default) or by hub name, with section headers and `(2,2,1,0)` padding. 1-line gaps separate agent cards within a group and a 1-line spacer follows each group header. The mode label line shows the current grouping (e.g., "by repo") with a "v to switch" hint. When only a single default group exists (only "default_hub" in by-hub mode, or only "No repository" in by-repo mode), the group header is hidden for a cleaner look. In by-repo mode, agents without a linked repository display their working directory on the agent card. Displays the CLUST logo when no agents are running.

Agent cards show: ID, binary name, status, start time, and attached terminal count. In by-repo mode, agents without a repository also show their working directory.

Repositories are registered via `clust repo -a` or auto-registered when an agent is launched inside a git repo. Branch data is fetched from the local git state every 2 seconds (no network calls or authentication required). Each repo is assigned a color from the repo palette on registration; colors cycle through `red`, `orange`, `yellow`, `lime`, `green`, `teal`, `blue`, `purple`, `pink`, `coral`. In the left panel, repository names use reverse-video styling (repo color background with dark text, bold) for visual prominence. Branches checked out as HEAD use the repo's assigned color instead of the default accent blue.

#### Overview Tab

A multi-agent terminal overview that displays all active agents side-by-side with live terminal output. Each agent gets its own panel with a full terminal emulator backed by the `vt100` crate.

```
┌─────────────────────────────────────────────────────┐
│ [options bar]                                       │
├──────────────────────┬──────────────────┬───────────┤
│┌──────────────────────┐│┌────────────────┐│┌─────────┐│
││a3f8c1·claude·repo/main●│││ b7e2d9·claude● │││ c4a1e0 ·││
││                    │││                │││         ││
││ Agent PTY output   │││ Agent PTY out  │││ (partial││
││ (VTE emulated)     │││ (VTE emulated) │││  view)  ││
││                    │││                │││         ││
│└──── Shift+↓ focus──┘│└────────────────┘│└─────────┘│
├──────────────────────┴──────────────────┴───────────┤
│ ● connected  Shift+↓ enter terminal  ...    v0.0.13 │
└─────────────────────────────────────────────────────┘
```

**Layout:**

- **Options bar (1 row):** Top row containing repository filter chips. Each chip displays a colored `●` dot (using the repo's assigned color) followed by the repo name. Clicking a chip or pressing Enter/Space when the chip is selected toggles that repo's visibility -- hidden repos are dimmed (dot uses `dim_color()`, text uses `R_TEXT_DISABLED`) and their agent panels are filtered out of the viewport. The active chip (cursor position) is highlighted with `R_BG_ACTIVE` background when the options bar is focused. When no repos exist, the bar is rendered as an empty row. Background changes based on focus (`R_BG_OVERLAY` when focused, `R_BG_RAISED` when unfocused).
- **Agent panels (horizontal):** Dynamically sized columns distributed evenly across the available width. The number of visible panels is determined by how many fit at the minimum width of 60 columns. Panels use ratio-based constraints so they fill the screen evenly (1 panel = half screen, 2 panels = half each, 3 panels = one-third each, etc.). A single panel never exceeds half the screen width. When more agents exist than fit on screen, horizontal scrolling is enabled with `◀ N` / `N ▶` indicators.
- Each panel has **box-drawing borders** (top, bottom, left, right). When a panel's agent is associated with a repository, the border color uses the repo's assigned color (bright when focused, dimmed to 60% brightness when unfocused via `dim_color()`). Panels without a repo fall back to accent blue when focused and subtle gray when unfocused.
- The **focused panel** displays a centered `Shift+↓ focus` hint in its bottom border (rendered via `Block::title_bottom()`). The shortcut text uses the bright accent color and the label uses secondary text color. This hint only appears when a terminal panel is focused in overview mode (not in focus mode).
- Inside the border, a **header row** shows agent ID (accent-colored), separator, agent binary name, optional repo/branch info, and status indicator (`●` green for running, `[exited]` red for exited). When the agent has a `repo_path`, the repo name (extracted from the path's last component) is displayed in the repo's assigned color, followed by `/branch_name` in tertiary text color (e.g., `myrepo/main`). When the agent has no `repo_path` but has a `branch_name`, the branch is shown alone in tertiary text color. Both are preceded by a `·` separator. The branch name is sourced from `AgentInfo.branch_name` and updates on each sync cycle (every 2 seconds).
- The **terminal area** below the header renders the agent's PTY output using a `vt100`-backed terminal emulator (`TerminalEmulator`) with full ANSI support (cursor movement, SGR colors/styles, erase operations, scroll regions, line wrapping, alternate screen buffer). The terminal emulator gets the inner width (total panel width minus 2 border columns).

**Focus modes:**

| Focus | Description |
|-------|-------------|
| Options Bar | Default. Left/Right navigate filter chips, Enter/Space toggle repo visibility, Shift+arrows scroll viewport or enter terminal. |
| Terminal(N) | All keyboard input is forwarded directly to the focused agent, except Shift+arrow keys. Focused panel has accent-blue borders; unfocused panels have subtle gray borders. |

**Keyboard shortcuts (Overview tab):**

| Context | Shortcut | Action |
|---------|----------|--------|
| Options Bar | `Shift+↓` | Enter terminal focus (returns to last focused panel) |
| Options Bar | `Shift+←` / `Shift+→` | Scroll viewport left/right |
| Options Bar | `←` / `→` | Navigate filter chips |
| Options Bar | `Enter` / `Space` | Toggle visibility of selected repo |
| Terminal | `Esc` (single) | Forward Esc to agent process |
| Terminal | `Esc×2` (double-tap) | Deselect terminal, return to options bar |
| Terminal | `Shift+↑` | Return to options bar |
| Terminal | `Shift+←` / `Shift+→` | Switch focus to previous/next agent panel (wraps around) |
| Terminal | `PageUp` / `PageDown` | Scroll focused panel through scrollback history |
| Terminal | Any other key | Forwarded to the focused agent's PTY |

**Implementation:**

- Each `AgentPanel` stores `is_worktree: bool` indicating whether the agent is running in a git worktree. This is used to determine whether to show worktree cleanup dialogs when the agent is stopped or exits. Each panel also stores `worktree_cleanup_shown: bool` to ensure the cleanup dialog is only shown once per agent, preventing duplicate prompts across mode transitions.
- Each agent panel runs a **background tokio task** that maintains its own IPC streaming connection to the hub (attach, receive output, forward input).
- Output events are sent to the UI thread via an `mpsc` channel and drained each frame.
- `TerminalEmulator` wraps a `vt100::Parser` (`vt100 = 0.15`) for full ANSI escape sequence handling, including alternate screen buffer support (private mode sequences like `?1049h`/`?1049l`), cursor visibility, scroll regions, and all standard SGR attributes. The `vt100` crate maintains scrollback internally (default 2,000 lines, configurable via `with_scrollback_capacity()`). The `TerminalEmulator` provides conversion to ratatui `Line`/`Span` types for TUI rendering (`to_ratatui_lines()`, `to_ratatui_lines_scrolled()`), to ANSI-escaped strings for direct stdout output (`to_ansi_lines_scrolled()`), and URL detection at screen coordinates (`url_at_position()`, `url_at_position_scrolled()`). It is also used as a shadow terminal in the attached session for scrollback (with 5,000-line capacity).
- `key_event_to_bytes()` converts `crossterm::KeyEvent` to raw terminal byte sequences for agent input forwarding.
- Lazy initialization: overview connections are only established on first switch to the Overview tab.
- On connect, each panel's background task consumes the hub's replay buffer before entering the main output loop, so panels show recent history immediately.
- On terminal resize, all panels are resized via `TerminalEmulator::resize()` (which preserves accumulated scrollback history) and the hub is notified via `ResizeAgent`. Same-size resizes are skipped as a no-op to preserve content. The viewport is scrolled automatically to keep the focused panel visible.
- **Force-resize triggers:** Panel dimensions are re-sent to the hub unconditionally (bypassing the same-size skip) in several scenarios where the hub's PTY may have been resized by another client: (1) switching to the Overview tab via `Tab`/`Shift+Tab` or `Cmd+2` when already initialized, (2) exiting focus mode back to Overview (when `in_focus_mode` is set to `false`), (3) navigating between panels with `Shift+←`/`Shift+→` (focused panel only), (4) entering terminal focus with `Shift+↓` (focused panel only), and (5) when the terminal window regains focus (`FocusGained` event). The `EnableFocusChange`/`DisableFocusChange` crossterm sequences are used to detect window focus changes.
- Each panel has a `panel_scroll_offset` for scrolling through the combined scrollback + live grid. When scrolled, a `↑N` indicator appears in the panel header.
- On exit, all connections are detached and background tasks are aborted.

### Auto-connect

On startup, `clust ui` automatically connects to the hub daemon, starting it if not already running. The bottom status bar shows connection status.

### Bottom Status Bar

```
● connected  q to quit  Q to quit and stop hub  ↑↓←→ navigate  Shift+←→ panels  v toggle agents          v0.0.13
```

| Section | Description |
|---------|-------------|
| Status dot | Green `●` when connected, dim when disconnected |
| Status label | `connected` or `disconnected` |
| Focused agent | When an agent has keyboard focus (in Overview terminal focus or focus mode), shows the repo name in the repo's assigned color followed by `/branch` in secondary text color |
| Status message / Shortcuts | Either a temporary status message or context-aware keybinding hints (see below) |
| Version | Right-aligned, e.g. `v0.0.13` |

**Status messages:** Temporary status messages override the keybinding hints area. Messages are displayed for 5 seconds before auto-dismissing, after which the keybinding hints reappear. Two severity levels exist: `Error` (displayed in `R_ERROR` color) and `Success` (displayed in `R_SUCCESS` color). Status messages are used to surface feedback from async operations such as agent creation and branch pulls -- both success confirmations (e.g., "Agent started on feature-branch", "Pulled main: Already up to date.") and error details (e.g., "Agent create failed: hub connect error: ...", "Pull failed: ..."). The `StatusMessage` struct tracks the message text, level, and creation `Instant` for auto-dismissal timing. Status messages are delivered from background tokio tasks to the main event loop via a dedicated `mpsc` channel (`status_tx` / `status_rx`), separate from the `AgentStartResult` channel used for agent creation results.

**Keybinding hints (when no status message is active):** Context-aware hints: on Repositories tab shows `q quit`, `Q stop+quit`, navigation hints; on Overview tab shows focus-dependent hints (e.g., `Shift+↓ enter terminal` or `Shift+↑ options`); in focus mode shows `Shift+←/→ switch panel`, `Shift+↑ exit`.

### Keyboard Shortcuts

**Global (all tabs, unless overridden):**

| Shortcut | Action |
|----------|--------|
| `q` | Quit the UI (hub keeps running) |
| `Esc×2` (double-tap) | Quit the UI (hub keeps running) |
| `Q` | Quit the UI and stop the hub |
| `Tab` | Switch to next tab |
| `Shift+Tab` | Switch to previous tab |
| `?` | Toggle keyboard shortcut overlay |
| `F2` | Toggle mouse capture (allows text selection and link clicking when off) |
| `Opt+E` (macOS) / `Alt+E` | Open the create-agent modal |
| `Opt+D` (macOS) / `Alt+D` | Open the detached agent modal (any directory) |
| `Opt+F` (macOS) / `Alt+F` | Open the search-agent modal (only when agents are running) |
| `Opt+N` (macOS) / `Alt+N` | Open the add-repository modal |
| `Cmd+1` | Switch to Repositories tab (dismisses context menus, exits focus mode) |
| `Cmd+2` | Switch to Overview tab (dismisses context menus, exits focus mode) |

**Repositories tab:**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move selection in visual order (flat navigation across repos, categories, and branches) |
| `→` | Descend into selected item (navigate tree) |
| `←` | Ascend to parent level (navigate tree) |
| `Enter` | Left panel: on repo opens repo context menu; on local branch opens local branch context menu; on remote branch opens remote branch context menu. Right panel: enter focus mode for selected agent. |
| `Space` | Left panel: toggle collapse/expand on repo or category level |
| `Shift+←` / `Shift+→` | Switch focus between left and right panels |
| `v` | Toggle agent grouping between by-hub and by-repo (right panel) |
| `Esc` | Dismiss context menu (when open) |
| `1`-`9`, `0` | Select context menu item by number (when context menu is open) |

**Inline key hints:** When an item is selected in the tree or right panel, a dim `Enter` hint is displayed inline next to the item name to indicate that pressing Enter will perform an action. This appears on: selected repository lines (for repos with a path), selected branch lines, selected "No Repository" agent entries, and selected agent cards in the right panel. The hint uses `R_TEXT_TERTIARY` color for subtlety.

**Context Menus:**

Context menus appear as centered modal overlays. They support arrow key navigation, Enter to confirm, Esc to dismiss, and number keys 1-9/0 for direct item selection. Context menus may include an optional description field -- body text rendered between the title and the numbered items (used for confirmation dialogs). Mouse clicks on menu items are supported; clicking outside the modal dismisses it.

- **Repo context menu:** Appears on Enter when a repo is selected. Contains: "Change Color" (opens color picker), "Open in File System", "Open in Terminal", "Stop All Agents", "Unregister", "Clean Stale Refs" (prunes stale remote tracking refs), and "Purge" (opens confirmation dialog).
- **Purge confirmation dialog:** A `ConfirmAction` menu with a description explaining the destructive operation ("This will stop all agents, delete all worktrees, and delete all local branches."). Options are "Confirm" and "Cancel". On confirm, launches an asynchronous purge operation and displays the purge progress modal.
- **Purge progress modal:** A centered overlay that shows real-time progress during the purge operation. Each phase (stopping agents, removing worktrees, deleting branches, cleaning stale refs) is displayed as a line item with an animated braille spinner while in progress, replaced by a checkmark when complete. All keyboard and mouse input is blocked while the purge is running. On completion, the modal shows "Press Esc to close" and only then accepts Esc to dismiss. If an error occurs, it is displayed in the modal. The purge runs asynchronously via a background task that streams `PurgeProgress` IPC messages from the hub, keeping the TUI responsive throughout.
- **Local branch context menu:** Appears on Enter when a local branch is selected. Contains: "Open Agent" (shown first when the branch has active agents), "Start Agent (worktree)" (always shown; creates a worktree and starts an agent), "Start Agent (in place)" (shown only for the HEAD branch; starts an agent directly in the repo root without creating a worktree, using the existing `StartAgent` IPC message), "Base Worktree Off" (always shown; opens the create-agent modal pre-populated with the selected repo and branch -- user only enters a new branch name and prompt), "Pull" (always shown; pulls or fetches the branch -- see Pull Branch below), "Stop Agents" (shown when the branch has active agents), "Remove Worktree" (shown when the branch is a worktree), and "Delete Branch" (force-deletes the local branch via `DeleteLocalBranch` IPC). When "Stop Agents" is selected and the stopped agents were in worktrees, a worktree cleanup dialog is shown after stopping.
- **Detach HEAD confirmation dialog:** When "Start Agent (worktree)" is selected on the HEAD branch, a `ConfirmAction` confirmation dialog is shown before proceeding: "This will detach HEAD in your repo. The branch will be moved to a worktree for the agent." with "Confirm" and "Cancel" options. On confirm, the hub auto-detaches HEAD in the main worktree so the branch can be moved to a linked worktree, then creates the worktree and starts the agent via `CreateWorktreeAgent`. This dialog is shown on both keyboard and mouse paths.
- **Remote branch context menu:** Appears on Enter when a remote branch is selected. Contains: "Checkout & Track Locally" (shown first; checks out the remote branch as a local tracking branch via `CheckoutRemoteBranch` IPC using `git checkout --track`), "Start Agent (checkout)" (creates a worktree from the remote branch and starts an agent), "Create Worktree" (checks out the remote branch as a worktree), and "Delete Remote Branch" (deletes the remote branch via `DeleteRemoteBranch` IPC).
- **Color picker:** Shows the 10 available repo colors (red, orange, yellow, lime, green, teal, blue, purple, pink, coral) with colored `●` indicators. Selecting a color sends a `SetRepoColor` IPC message to the hub.
- **Worktree cleanup dialog:** Appears after stopping agents that were running in worktrees. Shows the worktree branch name (with a dirty indicator if the worktree has uncommitted changes) and offers three options: "Keep" (leave the worktree as-is), "Discard worktree" (remove the worktree via `RemoveWorktree` IPC with force), "Discard worktree + branch" (remove both worktree and local branch). When multiple worktrees need cleanup, dialogs are shown sequentially. Dismissing a cleanup dialog (Esc) advances to the next pending cleanup. This dialog is triggered from four contexts: (1) "Stop All Agents" in the repo context menu, (2) "Stop Agents" in the local branch context menu, (3) immediately when an agent exits in focus mode (if the agent was running in a worktree), (4) exiting a terminal in overview mode (double-Esc back to options bar) when the agent has exited and was running in a worktree. A `worktree_cleanup_shown` flag on each `AgentPanel` prevents the dialog from being shown more than once per agent.
- **Agent picker:** Appears on Enter when a branch has multiple active agents. Lists agent IDs for selection; selecting one opens focus mode.

**Overview tab (Options Bar focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↓` | Enter terminal focus |
| `Shift+←` / `Shift+→` | Scroll viewport left/right |
| `←` / `→` | Navigate filter chips (move cursor left/right) |
| `Enter` / `Space` | Toggle visibility of the selected filter chip's repo |

**Overview tab (Terminal focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` (single) | Forward Esc to agent process |
| `Esc×2` (double-tap) | Deselect terminal, return to options bar |
| `Shift+↑` | Exit terminal, return to options bar |
| `Shift+↓` | Enter focus mode for the focused agent |
| `Shift+←` / `Shift+→` | Switch to previous/next agent panel |
| `PageUp` / `PageDown` | Scroll focused panel through scrollback history |
| `Shift+PageUp` / `Shift+PageDown` | Scroll focused panel through scrollback history (same as above) |
| All other keys | Forwarded to the focused agent's PTY |

#### Focus Mode

Focus mode is an overlay state (tracked by an `in_focus_mode` boolean), not a tab. When active, the tab bar is replaced by a back-bar header and the content area shows a single-agent focus view with a two-panel split: a 60%-width left panel with tabbed content (including a git diff viewer) and a 40%-width right panel displaying the agent's terminal.

**Back-bar header:**

When focus mode is active, the 1-row tab bar is replaced by a back-bar that shows: `<- Shift+↑  Back to [tab name]  agent-id . binary . repo/branch  Shift+←/→ panels`. The left side shows the exit hint and origin tab name; the center shows the agent identity followed by the repo name (in the repo's assigned color) and branch name; the right side shows keyboard hints. When the agent has no `repo_path` (non-repository agent), the keyboard hints on the right side of the back-bar are hidden.

```
┌─────────────────────────────────────────────────────┐
│ ← Shift+↑  Back to Overview  a3f8c1 · claude · myrepo/main        Shift+←/→ panels │
│ Changes │ Compare │ Panel 3 │┌────────────────────┐│
│                               ││ a3f8c1 · claude ●  ││
│      1      1│fn main() {     ││                    ││
│      2       │-  old_code();  ││ Agent PTY output   ││
│         2│+  new_code();  ││ (VTE emulated)     ││
│      3      3│  let x = 1;   ││                    ││
│                               │└────────────────────┘│
├─────────────────────────────────────────────────────┤
│ ● connected  Shift+←/→ switch panel  ...     v0.0.13│
└─────────────────────────────────────────────────────┘
```

**Left panel:**

The left panel has a tab bar at the top with three tabs: `Changes`, `Compare`, `Panel 3`. The `Changes` tab shows a unified inline diff viewer showing uncommitted changes (`git diff HEAD`). The `Compare` tab shows a branch comparison diff viewer where users can select any local branch and view the diff between it and the agent's current branch. `Panel 3` is a placeholder for future content. When the agent has no `repo_path` (non-repository agent), the left panel renders a simplified state: the tab bar and diff viewer are replaced by a centered "Agent not running inside repository" message in tertiary text color on the base background. The diff refresh background task is not spawned for non-repository agents.

**Diff viewer (Changes tab):**

- Displays the output of `git diff HEAD` for the agent's working directory
- Unified inline format with dual-column line numbers (old and new)
- Line-by-line color coding: additions use a green-tinted background (`R_DIFF_ADD_BG`), deletions use a red-tinted background (`R_DIFF_DEL_BG`), file headers use reverse-video styling (repo color background, `R_BG_BASE` foreground, bold) for visual prominence, hunk headers use the repo color as foreground, context lines use the base background. The repo's assigned color is used for file and hunk headers, falling back to `R_ACCENT` when no repo color is available.
- Per-token syntax highlighting is applied to code lines (Add, Delete, Context) via the `syntax` module using `syntect`. The file extension from the diff's file name is used to look up the appropriate TextMate grammar (`syntax_for_file()`). Each token is colored according to a custom Graphite-themed palette mapping 20+ TextMate scopes (keywords, strings, comments, numeric literals, type names, function names, decorators, punctuation, etc.) to Graphite theme colors. Token foreground colors are layered over the diff line's background color (add/delete/context). Lines with unrecognized file types fall back to plain monochrome styling. The `SyntaxSet` and `Theme` are lazy-loaded once via `LazyLock` to avoid repeated initialization cost.
- Blank separator lines are inserted between different files for visual spacing
- File headers display clean file paths (e.g., `src/main.rs`) instead of raw `diff --git a/... b/...` lines
- A gutter column (9 chars wide) shows old/new line numbers separated by a `│` divider; file headers and hunk headers suppress line numbers
- The diff is refreshed every 2 seconds via a background tokio task that runs `git diff HEAD` in a `spawn_blocking` call
- Scrolling is supported with `↑` / `↓` keys when the left panel is focused
- Error state shows the error message in `R_ERROR` color with word wrapping enabled (`.wrap(Wrap { trim: false })`) so long error messages do not overflow the terminal width
- Empty state shows "No uncommitted changes"; loading state shows "Loading diff..."

**Branch Compare (Compare tab):**

- Allows comparing the agent's current branch against any other local branch in the same repository
- Has two modes controlled by `BranchPickerMode`: `Searching` and `Selected`
- **Searching mode:** Shows a text input field with fuzzy search filtering and a scrollable branch list below it. The agent's own branch is excluded from the list. Uses `SkimMatcherV2` for fuzzy matching, with results sorted by match score descending. Keyboard controls: `↑` / `↓` navigate the list, `Enter` selects a branch and switches to Selected mode, `Esc` cancels and returns to Selected mode, typing filters the list, `Backspace` deletes characters, `←` / `→` move the cursor within the input
- **Selected mode:** Shows a label bar displaying the selected branch name (or "No branch selected" if none), followed by a diff viewer showing the output of `git diff <selected-branch> <agent-branch>`. Pressing `Enter` re-opens the search picker. `↑` / `↓` scroll the diff. `Tab` cycles to the next left panel tab
- The diff is refreshed every 2 seconds via a background tokio task (`spawn_branch_diff_task`) that runs `git diff <base> <head>` in a `spawn_blocking` call, mirroring the Changes tab refresh mechanism
- `BranchPicker` struct manages the picker state: input text, cursor position, selected index, selected branch name, branch list, and a `SkimMatcherV2` fuzzy matcher
- Branch list is updated via `update_compare_branches()` which is called during the repo refresh path, pulling local branches from the matching `RepoInfo`
- When a branch is selected, `start_compare_diff()` stops any existing compare diff task and spawns a new one
- `drain_compare_diff_events()` is called each frame in the main event loop to process background diff results
- Scroll state, diff data, and error state are managed independently from the Changes tab (`compare_diff`, `compare_diff_scroll`, `compare_diff_error`)
- The diff viewer rendering is shared with the Changes tab via a parameterized `render_diff_viewer()` function
- Mouse scroll within the left panel area is tab-aware, routing to `compare_scroll_up/down` when the Compare tab is active

**Panel focus:**

The focus view has a concept of which side (left or right) has keyboard focus. The focused side is indicated by visual cues (tab bar highlight, panel border accent). `Shift+←` and `Shift+→` switch focus between the left and right panels. `Shift+↑` exits focus mode from either panel. When the right panel is focused, `Esc` is forwarded to the agent process. When the agent has no `repo_path` (non-repository agent), `Shift+←` from the right panel is blocked (the left panel cannot receive focus), and mouse clicks on the left panel area do not switch focus to the left panel. Clicking the right panel area still works normally.

**Entry points:**

- **From Overview tab:** While in terminal focus, press `Shift+↓` to open the focused agent in focus mode. The `in_focus_mode` flag is set to `true`.
- **From Repositories tab:** While the right panel is focused, press `Enter` on a selected agent to open it in focus mode. The `in_focus_mode` flag is set to `true`.

The agent's `working_dir`, `repo_path`, and `branch_name` are passed to `open_agent()` to determine the git repository for the diff viewer and to display repo/branch identity in the back-bar and status bar.

**Exit:** Press `Shift+↑` from either panel to exit focus mode and return to the originating tab. The `in_focus_mode` flag is set back to `false`. When the right panel is focused, `Esc` is forwarded to the agent process (e.g., to dismiss an agent's own UI element). If the focused agent exits while in focus mode and was running in a worktree, the cleanup dialog is shown immediately (without waiting for the user to exit focus mode). A `worktree_cleanup_shown` flag prevents the dialog from appearing again when focus mode is later exited.

**Implementation:**

- `FocusModeState` manages a single `AgentPanel` with its own IPC background task, output channel, and `TerminalEmulator`. It also tracks `branch_name` (in addition to `working_dir` and `repo_path`) to support worktree cleanup dialogs when exiting focus mode.
- The panel dimensions are calculated as 40% of the content area width (minus borders) by the content area height (minus header).
- `FocusSide` enum tracks which panel has keyboard focus (`Left` or `Right`).
- `LeftPanelTab` enum tracks the active tab in the left panel (`Changes`, `Compare`, `Panel3`) with `next()` for cycling.
- Diff state is managed via `ParsedDiff` (lines, file start indices, file names), `diff_scroll` (current scroll position), and `diff_error` (error message if `git diff` failed).
- A background diff refresh task (`spawn_diff_task`) runs every 2 seconds and sends `DiffEvent::Updated` or `DiffEvent::Error` via an `mpsc` channel. A `watch` channel signals the task to stop. The diff task is only spawned when `repo_path` is `Some` (i.e., the agent is running inside a git repository).
- `drain_diff_events()` is called each frame in the main event loop alongside `drain_output_events()`.
- `parse_unified_diff()` parses raw `git diff HEAD` output into structured `DiffLine` entries with kind (FileHeader, HunkHeader, Context, Add, Delete, FileMetadata, Separator), content, line numbers, and file index. Separator lines are automatically inserted between files during parsing.
- On terminal resize, the focus mode panel is resized via `TerminalEmulator::resize()` (preserving scrollback history) and the hub is notified via `ResizeAgent`. On `FocusGained` events, dimensions are also re-sent unconditionally to account for PTY resizes by other clients while the window was unfocused.
- Focus mode is orthogonal to tab cycling -- `Tab` / `Shift+Tab` simply toggles between `Repositories` and `Overview` (2 tabs). Focus mode is only entered explicitly via the entry points above.
- State is tracked by an `in_focus_mode: bool` flag rather than a `previous_tab` option. The `ActiveTab` enum no longer has a `FocusMode` variant.
- On exit (via `close_panel()`), the diff task is stopped via the watch channel and aborted, diff state is cleared, and the panel's connection is detached.

**Keyboard shortcuts (focus mode, right panel focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` | Forward Esc to agent process |
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+←` | Switch focus to left panel |
| `Shift+PageUp` | Scroll up through scrollback history |
| `Shift+PageDown` | Scroll down through scrollback history |
| All other keys | Forwarded to the focused agent's PTY |

**Keyboard shortcuts (focus mode, left panel focused, Changes tab):**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Scroll diff up/down |
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab |

**Keyboard shortcuts (focus mode, left panel focused, Compare tab -- Selected mode):**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Scroll compare diff up/down |
| `Enter` | Open branch search picker |
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab |

**Keyboard shortcuts (focus mode, left panel focused, Compare tab -- Searching mode):**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Navigate branch list up/down |
| `Enter` | Select highlighted branch, start diff |
| `Esc` | Cancel search, return to Selected mode |
| Typing | Filter branch list with fuzzy search |
| `Backspace` | Delete character before cursor |
| `←` / `→` | Move cursor within search input |

### Help Overlay (`?`)

The `?` key toggles a keyboard shortcut overlay rendered as a centered modal (44 columns wide) anchored to the bottom of the content area. The modal is organized into sections with bold secondary-colored headers and context-aware visibility:

- **Global section (always shown):** `q / Esc×2`, `Q`, `Ctrl+C`, `Tab`, `Shift+Tab`, `?`, `F2`, `Alt+E`, `Alt+D`, `Alt+F`, `Alt+N`, `Cmd+1`, `Cmd+2`.
- **Repositories section (shown when Repositories tab is active):** `↑/↓` navigate, `←/→` navigate tree, `Shift+←/→` switch panel, `Enter` open menu/focus agent, `Space` collapse/expand, `v` toggle grouping.
- **Overview section (shown when Overview tab is active):** `Shift+←/→` scroll panels, `Shift+↓` enter terminal, plus an "In terminal:" sub-context label followed by `Shift+↑` back to options bar, `Shift+↓` enter focus mode, `Shift+←/→` switch agent, `PgUp/PgDn` scroll terminal.
- **Focus Mode section (shown when in focus mode):** `Shift+↑` exit, `Shift+←/→` switch panel, `Shift+PgUp/PgDn` scroll terminal, plus a "Left panel:" sub-context label followed by `Tab` cycle tabs, `↑/↓` scroll diff.

Key names are displayed in accent color (left-aligned, 16 chars wide); descriptions use primary text color. Section headers use secondary text color with bold modifier. Sub-context labels use tertiary text color and are indented.

### Create Agent Modal

A multi-step modal for creating new agents on git worktrees, opened globally with `Opt+E` (macOS) / `Alt+E`. The modal guides the user through 4 sequential steps:

| Step | Title | Description |
|------|-------|-------------|
| 1/4 | Select repository | Choose from registered repos. Fuzzy search filters by name and path. |
| 2/4 | Select target branch | Choose a local branch from the selected repo. Fuzzy search filters by name. Shows HEAD, worktree, and active agent indicators. Skipped if the repo has no local branches. |
| 3/4 | New branch | Enter a branch name for the new worktree. Required if no branches exist; optional otherwise (press Enter to use the target branch directly). |
| 4/4 | Enter prompt | Type an initial prompt for the agent. Optional -- press Enter to start with no prompt. |

**Pre-selected mode (Base Worktree Off):** When opened via the "Base Worktree Off" context menu on a local branch, the modal is pre-populated with the selected repo and branch. Steps 1 and 2 are skipped, and the user starts at step 1/2 (New branch) followed by step 2/2 (Enter prompt). In this mode, `Esc` on the first step cancels the modal instead of navigating back, and the hint reads "Esc to cancel" instead of "Esc to go back". The `new_with_branch()` constructor creates this pre-selected state.

**Navigation:**
- `Up` / `Down` -- move selection in list steps
- `Enter` -- confirm selection / advance to next step
- `Esc` -- go back to previous step, or cancel from step 1 (or cancel from any step in pre-selected mode)
- Type to filter -- fuzzy matching via `fuzzy-matcher` (SkimV2 algorithm)

**Branch name sanitization:** In step 3 (New branch), user input is sanitized via `clust_ipc::branch::sanitize_branch_name()` before being sent to the hub. This converts spaces to hyphens, slashes to double underscores, strips git-invalid characters, collapses sequences, and handles edge cases. The sanitized name is what gets used as the actual git branch name.

**Completion:** On completing step 4, the modal sends a `CreateWorktreeAgent` IPC message to the hub. The hub creates the worktree (via the existing `add_worktree()` logic), spawns an agent in it, and returns `WorktreeAgentStarted`. The behavior depends on the active tab: when on the **Overview tab**, the TUI stays in overview mode and selects the newly created agent's panel after the next agent sync (via `pending_overview_select` and `OverviewState::select_agent_by_id()`); when on the **Repositories tab**, the TUI opens the new agent in focus mode as before. On success, a status bar message confirms the agent started (e.g., "Agent started on feature-branch"). On failure (hub connection error, send error, unexpected response, or hub-reported error), the error is surfaced as a status bar error message instead of being lost to stderr. The `AgentStartResult` enum has `Started` and `Failed(String)` variants to communicate the outcome from the background tokio task to the main event loop.

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 60% of terminal height) with a titled border, input field with visible cursor, and a scrollable list with fuzzy-matched results. The selected item is indicated with a `>` prefix and bold text. In the prompt step (4/4), the input area expands to fill remaining modal space and text wraps within the field (`.wrap(Wrap { trim: false })`) with automatic scrolling to keep the cursor visible.

The `Opt+E` / `Alt+E` and `Opt+N` / `Alt+N` hints are shown in the status bar (platform-aware).

### Search Agent Modal

A fuzzy search modal for quickly finding and opening running agents, opened globally with `Opt+F` (macOS) / `Alt+F`. The modal is only available when at least one agent is running.

**Search fields:** The fuzzy matcher (SkimV2 algorithm via `fuzzy-matcher`) searches across all agent fields: ID, agent binary name, branch name, repo path, repo name (last path component), working directory, and hub name. Results are ranked by best match score.

**Navigation:**
- Type to filter -- fuzzy matching narrows the agent list in real time
- `Up` / `Down` -- move selection through filtered results
- `Left` / `Right` -- move cursor within the input field
- `Backspace` -- delete character before cursor
- `Enter` -- open the selected agent in focus mode
- `Esc` -- cancel and close the modal

**Rendering:** The modal is rendered as a centered overlay (70 columns wide, 60% of terminal height) with a titled border ("Search Agents"), a hint line, an input field with a visible block cursor, and a scrollable list of matching agents. The selected item is indicated with a `>` prefix and bold text. Each agent line shows: binary name, branch name (if present, in info color when selected), repo name (last path component), and a short ID suffix. The list scrolls automatically to keep the selection visible.

**Completion:** On selecting an agent with Enter, the modal closes and the agent is opened in focus mode (same behavior as entering focus mode from the Repositories or Overview tabs).

### Detached Agent Modal

A two-step modal for spawning agents in any directory (not limited to git repositories), opened globally with `Opt+D` (macOS) / `Alt+D`. Unlike the Create Agent Modal which operates on git worktrees, this modal allows starting agents in arbitrary filesystem directories.

| Step | Title | Description |
|------|-------|-------------|
| 1/2 | Select directory | Browse and select a working directory. Starts at the user's home directory. Fuzzy search filters subdirectories by name. Hidden directories (starting with `.`) are excluded. |
| 2/2 | Enter prompt | Type an initial prompt for the agent. Optional -- press Enter to start with no prompt. |

**Navigation (directory step):**
- `Up` / `Down` -- move selection in directory list
- `Tab` -- autocomplete: enter the selected directory and show its subdirectories
- `Enter` -- if filter text matches a directory, enter it first, then confirm the current path as the working directory and advance to the prompt step
- `Backspace` (when input is empty) -- navigate up one directory level
- `Esc` -- cancel the modal
- Type to filter -- fuzzy matching via `fuzzy-matcher` (SkimV2 algorithm)

**Navigation (prompt step):**
- `Enter` -- start the agent
- `Esc` -- go back to the directory selection step

**Completion:** On completing step 2, the modal sends a `CliMessage::StartAgent` IPC message to the hub (reusing the existing agent start path). The hub auto-detects git repository information if the selected directory is inside a git repo. The `AgentStartResult::Started` variant includes an `is_worktree: bool` field to properly propagate whether the agent is running in a worktree. On success, the TUI opens the new agent in focus mode with a status message (e.g., "Agent started in /path/to/dir"). On failure, the error is surfaced as a status bar error message.

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 60% of terminal height) with the same visual style as the Create Agent Modal. The directory step shows the current base path above the input field, with a scrollable list of filtered subdirectories below. The selected item is indicated with a `>` prefix and bold text. In the prompt step, the input area expands to fill remaining modal space with text wrapping.

### Add Repository Modal

A multi-step wizard modal for creating new repositories or cloning existing ones, opened globally with `Opt+N` (macOS) / `Alt+N` or by selecting the "Add Repository" entry in the repository tree. The modal follows the same visual patterns as the `DetachedAgentModal`.

| Step | Title | Description |
|------|-------|-------------|
| 1 | Choose action | Select between "Create new repository" and "Clone existing repository". |
| 2 | Select directory | Browse and select a parent directory for the new/cloned repository. Fuzzy search filters subdirectories by name. Hidden directories (starting with `.`) are excluded. |
| 3 (Clone only) | Enter URL | Type the git clone URL (HTTPS or SSH format). |
| 4 | Enter name | Type a name for the repository directory. For clone, this is optional -- press Enter to use the name extracted from the URL. For create, this is required. |

**Navigation:**
- `Up` / `Down` -- move selection in list steps
- `Tab` -- autocomplete: enter the selected directory and show its subdirectories (directory step)
- `Enter` -- confirm selection / advance to next step
- `Backspace` (when input is empty) -- navigate up one directory level (directory step)
- `Esc` -- go back to previous step, or cancel from step 1

**Completion:**
- **Create:** Sends a `CliMessage::CreateRepo` IPC message. The hub runs `git init`, registers the repository, and returns `RepoCreated`. A success status message is displayed.
- **Clone:** Initiates an asynchronous clone operation via `start_clone_async()`, which sends a `CliMessage::CloneRepo` IPC message. The clone progress modal is shown during the operation (see below).

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 60% of terminal height) with the same visual style as the Detached Agent Modal. The action selection step shows numbered options. The directory step shows the current base path above the input field. The URL and name steps show text input fields.

### Clone Progress Modal

A centered overlay that shows real-time progress during a git clone operation. Displayed automatically when a clone is initiated from the add-repository modal.

- Each progress line from the git clone stderr output is shown as a line item with an animated braille spinner while in progress, replaced by a checkmark when complete.
- On successful completion, the repository is auto-registered and the modal shows "Press Esc to close".
- On error, the error message is displayed in `R_ERROR` color.
- All keyboard and mouse input is blocked while the clone is running, except `Esc` which is accepted after completion or error.
- The clone runs asynchronously via a background tokio task that streams `CloneProgress` IPC messages from the hub via an unbounded channel, keeping the TUI responsive throughout.
- The URL is displayed in the modal title, truncated to fit the modal width.

### Mouse Support

Mouse capture is enabled via `crossterm::EnableMouseCapture` on TUI startup and disabled on exit. The Kitty keyboard protocol (`PushKeyboardEnhancementFlags` with `DISAMBIGUATE_ESCAPE_CODES`) is also enabled when the terminal supports it, allowing detection of the SUPER (Cmd) modifier on mouse events. Terminals that do not support the Kitty protocol gracefully degrade (the modifier is simply not reported). All mouse interactions use `MouseEventKind::Down(MouseButton::Left)` for clicks and `MouseEventKind::ScrollUp`/`ScrollDown` for scroll wheel.

#### F2 Mouse Capture Toggle

Pressing `F2` toggles mouse capture on/off. When mouse capture is disabled, the terminal emulator regains control of mouse events, allowing native text selection, copy/paste, and link clicking. When mouse capture is off, all mouse events (clicks, scrolls) are ignored by the TUI. The status bar displays a `MOUSE OFF . F2` indicator in the warning color when mouse capture is disabled. Pressing `F2` again re-enables mouse capture and restores normal TUI mouse handling. The `mouse_captured` boolean state is tracked in the main event loop and passed to `render_status_bar()` for display.

#### Click Map Architecture

A `ClickMap` struct is populated during each render pass and consumed during mouse event handling. During rendering, each clickable element records its bounding `Rect` and associated action target into the click map. When a mouse click arrives, the handler checks each region in the click map to determine what was clicked. The click map is rebuilt from scratch every frame.

`ClickMap` fields:
- `tabs` -- tab bar regions mapped to `ActiveTab` values
- `left_panel_area` / `right_panel_area` -- full panel areas for Repositories tab focus switching
- `tree_items` / `tree_inner_area` -- repo tree line targets mapped via `TreeClickTarget` enum (Repo, Category, Branch)
- `agent_cards` -- right panel agent card regions mapped to (group_idx, agent_idx) pairs
- `mode_label_area` -- right panel mode label region (the "by repo / by hub" line) for click-to-toggle view mode
- `overview_panels` -- Overview tab panel regions mapped to global panel indices
- `overview_filter_chips` -- Overview tab filter chip regions mapped to repo path strings
- `focus_left_area` / `focus_right_area` -- Focus mode panel areas for focus switching
- `focus_left_tabs` -- Focus mode left panel tab regions mapped to `LeftPanelTab` values
- `overview_content_areas` -- Overview tab terminal content areas (inner area excluding borders/header) mapped to global panel indices, used for Cmd+click URL detection
- `focus_right_content_area` -- Focus mode right panel terminal content area (inner area excluding borders/header), used for Cmd+click URL detection

#### Mouse Click Behavior

**Tab bar (when not in focus mode):**

| Click Target | Action |
|--------------|--------|
| Tab label | Switch to that tab (Repositories or Overview) |

**Repositories tab:**

| Click Target | Action |
|--------------|--------|
| Tree item (repo) | Select the repo; click again when already selected to toggle collapse |
| Tree item (category) | Select the category; click again when already selected to toggle collapse |
| Tree item (branch) | Select the branch |
| Mode label ("by repo/hub  v to switch") | Toggle agent grouping between by-hub and by-repo (same as pressing `v`) |
| Agent card | Select the agent and focus the right panel |
| Left panel (anywhere) | Switch keyboard focus to left panel |
| Right panel (anywhere) | Switch keyboard focus to right panel |

Clicking anywhere in the tree area (including empty space) sets keyboard focus to the left panel. Clicking an agent card sets focus to the right panel.

**Overview tab:**

| Click Target | Action |
|--------------|--------|
| Filter chip | Toggle that repo's visibility (hidden repos are dimmed and their panels are filtered out) |
| Agent panel | Focus that terminal panel (`OverviewFocus::Terminal(idx)`) |

**Focus mode:**

| Click Target | Action |
|--------------|--------|
| Left panel tab (Changes/Compare/Panel 3) | Switch to that tab and focus the left panel (only when agent has a repo) |
| Left panel area | Switch keyboard focus to left panel (only when agent has a repo) |
| Right panel area | Switch keyboard focus to right panel |

#### Cmd+Click URL Opening

Holding Cmd (SUPER modifier) while clicking on a URL in a terminal panel opens the URL in the system's default browser. This works in both overview mode and focus mode.

| Context | Click Target | Action |
|---------|--------------|--------|
| Overview tab | URL in terminal content area | Open URL in default browser |
| Focus mode | URL in right panel terminal content area | Open URL in default browser |

URL detection is handled by `TerminalEmulator::url_at_position_scrolled()`, which extracts the plain text for the clicked row, maps the click column to a byte offset, and searches for `https://` or `http://` URLs containing that offset. Trailing punctuation (`.`, `,`, `;`, `:`, `!`, `?`, `"`, `'`, `>`, `]`) is stripped from URL endings. Trailing `)` is only stripped when the URL does not contain a matching `(` (preserving Wikipedia-style URLs like `https://en.wikipedia.org/wiki/Rust_(language)`).

The `open_url()` helper uses `open` on macOS and `xdg-open` on Linux (fire-and-forget).

#### Cursor-Aware Scroll Wheel

Scroll wheel events scroll the element under the mouse cursor rather than the keyboard-focused element. The scroll step is 3 lines per event.

**Overview tab:**

| Cursor Position | Scroll Action |
|-----------------|---------------|
| Over an agent panel | Scroll that panel's scrollback up/down (regardless of which panel has keyboard focus) |

**Focus mode:**

| Cursor Position | Scroll Action |
|-----------------|---------------|
| Over the right panel | Scroll the agent terminal scrollback up/down |
| Over the left panel | Scroll the diff viewer up/down |

The Repositories tab does not have scroll wheel handling.
