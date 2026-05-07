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
3. CLI **aborts** the output streaming task (so it does not keep writing to stdout while we tear down the alternate screen)
4. CLI exits raw mode, restores terminal
5. CLI exits cleanly (agent continues in hub)

The attached session uses an `AltScreenGuard` RAII wrapper around the alternate-screen + raw-mode setup. If raw mode fails to engage after the alternate screen is entered, the guard restores the main screen on drop so the user's shell never inherits a broken terminal state. The guard also fires on panic, abnormal exit, and detach.

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

A 1-row bar at the top of the terminal with three tabs:

| Tab | Description |
|-----|-------------|
| `Repositories` | Two-panel view with repo tree and agent cards (default) |
| `Overview` | Multi-agent terminal overview with horizontal panels |
| `Schedule` | Per-task scheduler with horizontal columns for inactive/active/complete/aborted tasks |

The active tab is highlighted with the accent color. A `Tab/Shift+Tab` hint is shown to the right of the tabs. Tabs render in the order shown above (Repositories, Overview, Schedule) and `Tab`/`Shift+Tab` cycles through them in that order. Focus mode is not a tab -- it is an overlay state entered explicitly from either tab (see Focus Mode section below). When focus mode is active, the tab bar is replaced by a back-bar header.

#### Content Panels (Repositories tab)

- **Left panel (25%):** Repository tracker with `(2,2,1,0)` padding. Shows a "Repositories" title on the first row with an accent indicator (`●`) right-aligned on the same line, followed by a 1-row spacer, then the tree content below. Shows a tree view of registered git repositories with their local and remote branches. Repository header lines use reverse-video styling (repo color background, dark text, bold) for visual prominence; when selected, the header uses the standard hover background with colored text instead. An empty line separates each repository group in the tree for visual clarity. Repository names are preceded by a colored `●` dot matching the repo's assigned color (from the 10-color repo palette: red, orange, yellow, lime, green, teal, blue, purple, pink, coral). Tree connectors use `├──` / `└──` for clear hierarchy. Branch names are rendered Bold. Remote branches are collapsed by default. Branches with active agents display a green `●` indicator with count; branches checked out in worktrees display a `⎇` indicator. The current HEAD branch is highlighted using the repo's assigned color. Displays "No repositories found" when no repos are registered. Uses background colors for visual separation (no borders). Agents not associated with any git repository are grouped under a synthetic "No Repository" entry at the bottom of the tree. This entry has no local/remote category level -- agents are listed directly under the repo node with their binary name and working directory. Navigation skips the category level for this group. An "Add Repository" entry with a `+` icon is always appended at the bottom of the tree. Selecting it and pressing Enter (or clicking it) opens the add-repository modal (`RepoModal`). The selector lives only in this panel — there is no way to focus the right panel.
- **Vertical divider (1 col):** A single-column divider between the two panels.
- **Right panel (75%):** Window view -- a recursive 2x2 grid of live agent terminals scoped to the repository currently selected in the left panel. The panel is view-only and reflects the left selection; to interact with an agent (type into it), switch to the Overview tab via `Tab`. The grid is filtered by repo only (regardless of whether the cursor is on the repo, a category, or a branch). Cells are filled in row-major order (top-left, top-right, bottom-left, bottom-right) and each quadrant is recursively subdivided once it holds more than one agent (so 5 agents become a 2-cell sub-grid in TL plus three quarter cells; 8 agents tile as four 2-cell sub-grids; etc.). Agents are sorted by `started_at` then `id` so newly-started agents append at the end. Each cell renders the same `AgentPanel` widget used by the Overview tab via `render_agent_panel()`, sharing `overview_state.panels` so each agent keeps a single IPC connection across both Repositories and Overview tabs. Per-cell vterm resize happens each frame (sends a `PanelCommand::Resize` SIGWINCH then resizes the local vterm if the channel send succeeds). When no agents are running for the selected repo, a centered "No agents running for <repo-name>" message is shown (or "No detached agents" for the synthetic "No Repository" entry). When the "Add Repository" sentinel is selected on the left panel, the CLUST logo is rendered instead.

Window-view layout lives in `crates/clust-cli/src/window_view.rs`: `window_layout(rect, n)` returns the cell rects in row-major order and `render()` draws the cells using `render_agent_panel()`. The panel has no selection state and no keyboard focus.

Repositories are registered via `clust repo -a` or auto-registered when an agent is launched inside a git repo. Branch data is fetched from the local git state every 2 seconds (no network calls or authentication required). Each repo is assigned a color from the repo palette on registration; colors cycle through `red`, `orange`, `yellow`, `lime`, `green`, `teal`, `blue`, `purple`, `pink`, `coral`. In the left panel, repository names use reverse-video styling (repo color background with dark text, bold) for visual prominence. Branches checked out as HEAD use the repo's assigned color instead of the default accent blue.

#### Overview Tab

A multi-agent terminal overview that displays all active agents side-by-side with live terminal output. Each agent gets its own panel with a full terminal emulator backed by the `vt100` crate.

```
┌─────────────────────────────────────────────────────┐
│ ● myrepo  ● Other │  main  feat-x  wip             │
├──────────────────────┬──────────────────┬───────────┤
│┌──────────────────────┐│┌────────────────┐│┌─────────┐│
││a3f8c1·claude·repo/main●│││ b7e2d9·claude● │││ c4a1e0 ·││
││                    │││                │││         ││
││ Agent PTY output   │││ Agent PTY out  │││ (partial││
││ (VTE emulated)     │││ (VTE emulated) │││  view)  ││
││                    │││                │││         ││
│└──── Shift+↓ focus──┘│└────────────────┘│└─────────┘│
├──────────────────────┴──────────────────┴───────────┤
│ ● connected  Shift+↓ enter terminal  ...    v0.0.23 │
└─────────────────────────────────────────────────────┘
```

**Layout:**

- **Filter bar (1 row):** A single-line bar at the top that groups agents by repository. The left section shows repo chips -- each repo has a colored `●` dot (using the repo's assigned color) followed by the repo name. The cursor position is indicated with `R_BG_ACTIVE` background on the chip. A `│` separator divides the left and right sections. The right section shows all agent branch indicators colored by their repo's assigned color. Visible agents use inverse video styling (repo color background, primary text foreground); off-screen agents use the repo color as foreground; collapsed repo agents use `R_TEXT_DISABLED`. Agents without a repository are grouped under a synthetic "Other" chip using the accent color. Clicking a repo chip toggles collapse/expand -- collapsed repos have dimmed dots and `R_TEXT_DISABLED` text, and their agent panels are filtered out of the viewport. Clicking an individual branch indicator focuses that agent's terminal panel. When no repos exist, the bar is rendered as an empty 1-row area. Background changes based on focus (`R_BG_OVERLAY` when focused, `R_BG_RAISED` when unfocused). Panels are ordered by repo group (matching repo registration order), then by creation time (`started_at`) so newly-spawned agents are appended at the end of their group via `compute_sorted_indices()`.
- **Agent panels (horizontal):** Dynamically sized columns distributed evenly across the available width. The number of visible panels is determined by how many fit at the minimum width of 60 columns. Panels use ratio-based constraints so they fill the screen evenly (1 panel = half screen, 2 panels = half each, 3 panels = one-third each, etc.). A single panel never exceeds half the screen width. When more agents exist than fit on screen, horizontal scrolling is enabled with `◀ N` / `N ▶` indicators.
- Each panel has **box-drawing borders** (top, bottom, left, right). When a panel's agent is associated with a repository, the border color uses the repo's assigned color (bright when focused, dimmed to 60% brightness when unfocused via `dim_color()`). Panels without a repo fall back to accent blue when focused and subtle gray when unfocused.
- The **focused panel** displays a centered `Shift+↓ focus` hint in its bottom border (rendered via `Block::title_bottom()`). The shortcut text uses the bright accent color and the label uses secondary text color. This hint only appears when a terminal panel is focused in overview mode (not in focus mode).
- Inside the border, a **header row** shows agent ID (accent-colored), separator, agent binary name, optional repo/branch info, and status indicator (`●` green for running, `[exited]` red for exited). When the agent has a `repo_path`, the repo name (extracted from the path's last component) is displayed in the repo's assigned color, followed by `/branch_name` in tertiary text color (e.g., `myrepo/main`). When the agent has no `repo_path` but has a `branch_name`, the branch is shown alone in tertiary text color. Both are preceded by a `·` separator. The branch name is sourced from `AgentInfo.branch_name` and updates on each sync cycle (every 2 seconds).
- The **terminal area** below the header renders the agent's PTY output using a `vt100`-backed terminal emulator (`TerminalEmulator`) with full ANSI support (cursor movement, SGR colors/styles, erase operations, scroll regions, line wrapping, alternate screen buffer). The terminal emulator gets the inner width (total panel width minus 2 border columns).

**Focus modes:**

| Focus | Description |
|-------|-------------|
| Options Bar | Default. Left/Right navigate repo groups, Enter/Space toggle repo collapse/expand, Shift+arrows scroll viewport or enter terminal. |
| Terminal(N) | All keyboard input is forwarded directly to the focused agent, except Shift+arrow keys. Focused panel has accent-blue borders; unfocused panels have subtle gray borders. |

**Keyboard shortcuts (Overview tab):**

| Context | Shortcut | Action |
|---------|----------|--------|
| Options Bar | `Shift+↓` | Enter terminal focus (returns to last focused panel) |
| Options Bar | `Shift+←` / `Shift+→` | Scroll viewport left/right |
| Options Bar | `←` / `→` | Navigate repo groups |
| Options Bar | `Enter` / `Space` | Toggle collapse/expand of selected repo group |
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
- **Viewport scroll buffering:** When 3 or more panels are visible, the viewport maintains a 1-panel buffer from the edges -- scrolling begins when the selection reaches the first or last visible position rather than moving off-screen. This keeps the selected panel away from the viewport edges for a more centered feel. When fewer than 3 panels are visible, the viewport falls back to the original behavior where it scrolls only when the selection moves outside the visible area.
- **Force-resize triggers:** Panel dimensions are re-sent to the hub unconditionally (bypassing the same-size skip) in several scenarios where the hub's PTY may have been resized by another client: (1) switching to the Overview tab via `Tab`/`Shift+Tab` or `Cmd+2` when already initialized, (2) exiting focus mode back to Overview (when `in_focus_mode` is set to `false`), (3) navigating between panels with `Shift+←`/`Shift+→` (focused panel only), (4) entering terminal focus with `Shift+↓` (focused panel only), and (5) when the terminal window regains focus (`FocusGained` event). The `EnableFocusChange`/`DisableFocusChange` crossterm sequences are used to detect window focus changes.
- Each panel has a `panel_scroll_offset` for scrolling through the combined scrollback + live grid. When scrolled, a `↑N` indicator appears in the panel header.
- On exit, all connections are detached and background tasks are aborted.

### Auto-connect

On startup, `clust ui` automatically connects to the hub daemon, starting it if not already running. The bottom status bar shows connection status.

### Bottom Status Bar

```
● connected  q to quit  Q to quit and stop hub  ↑↓←→ navigate  Shift+←→ panels                           v0.0.23
```

| Section | Description |
|---------|-------------|
| Status dot | Green `●` when connected, dim when disconnected |
| Status label | `connected` or `disconnected` |
| BYPASS indicator | When bypass-permissions is enabled globally, shows `BYPASS` in a distinct color. Hidden when disabled. |
| Focused agent | When an agent has keyboard focus (in Overview terminal focus or focus mode), shows the repo name in the repo's assigned color followed by `/branch` in secondary text color |
| Status message / Shortcuts | Either a temporary status message or context-aware keybinding hints (see below) |
| Version | Right-aligned, e.g. `v0.0.23` |

**Status messages:** Temporary status messages override the keybinding hints area. Messages are displayed for 5 seconds before auto-dismissing, after which the keybinding hints reappear. Two severity levels exist: `Error` (displayed in `R_ERROR` color) and `Success` (displayed in `R_SUCCESS` color). Status messages are used to surface feedback from async operations such as agent creation, branch pulls, and remote branch checkout -- both success confirmations (e.g., "Agent started on feature-branch", "Pulled main: Already up to date.", "Checked out feature-branch") and error details (e.g., "Agent create failed: hub connect error: ...", "Pull failed: ...", "Checkout failed: ..."). The `StatusMessage` struct tracks the message text, level, and creation `Instant` for auto-dismissal timing. Status messages are delivered from background tokio tasks to the main event loop via a dedicated `mpsc` channel (`status_tx` / `status_rx`), separate from the `AgentStartResult` channel used for agent creation results.

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
| `Opt+M` (macOS) / `Alt+M` | Temporarily disable mouse capture for 5 seconds (mouse passthrough) |
| `Opt+E` (macOS) / `Alt+E` | Open the create-agent modal |
| `Opt+D` (macOS) / `Alt+D` | Open the detached agent modal (any directory) |
| `Opt+F` (macOS) / `Alt+F` | Open the search-agent modal (only when agents are running) |
| `Opt+B` (macOS) / `Alt+B` | Toggle bypass permissions (global, persisted in SQLite) |
| `Opt+N` (macOS) / `Alt+N` | Open the add-repository modal |
| `Opt+V` (macOS) / `Alt+V` | Open in editor (see Editor Integration below) |
| `Cmd+1` | Switch to Repositories tab (dismisses context menus, exits focus mode) |
| `Cmd+2` | Switch to Overview tab (dismisses context menus, exits focus mode) |
| `Cmd+3` | Switch to Schedule tab (dismisses context menus, exits focus mode) |

**Repositories tab:**

The selector lives only in the left panel. The right window-view panel is view-only — it has no keyboard focus and no per-cell selection. To interact with an agent (type into it), switch to the Overview tab via `Tab`.

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move selection in visual order (flat navigation across repos, categories, and branches). |
| `Shift+↑` / `Shift+↓` | Jump to previous/next repository header (skips categories and branches). |
| `→` | Descend into selected item. |
| `←` | Ascend to parent level. |
| `Enter` | On repo opens repo context menu; on local branch opens local branch context menu; on remote branch opens remote branch context menu. |
| `Space` | Toggle collapse/expand on repo or category level. |
| `Esc` | Dismiss context menu (when open) |
| `1`-`9`, `0` | Select context menu item by number (when context menu is open) |

**Inline key hints:** When an item is selected in the tree, a dim `Enter` hint is displayed inline next to the item name to indicate that pressing Enter will perform an action. This appears on: selected repository lines (for repos with a path), selected branch lines, and selected "No Repository" agent entries. The hint uses `R_TEXT_TERTIARY` color for subtlety.

**Context Menus:**

Context menus appear as centered modal overlays. They support arrow key navigation, Enter to confirm, Esc to dismiss, and number keys 1-9/0 for direct item selection. Context menus may include an optional description field -- body text rendered between the title and the numbered items (used for confirmation dialogs). Mouse clicks on menu items are supported; clicking outside the modal dismisses it.

- **Repo context menu:** Appears on Enter when a repo is selected. Contains: "Change Color" (opens color picker), "Open in File System", "Open in Terminal", "Stop All Agents", "Clean Stale Refs" (prunes stale remote tracking refs), "Detach" (detaches HEAD from the currently checked-out branch via `DetachHead` IPC), "Purge" (opens confirmation dialog), "Remove Repository" (opens confirmation dialog; stops tracking the repo in clust but leaves the folder on disk), and "Delete Repository" (opens confirmation dialog; stops tracking AND removes the folder from disk via `DeleteRepo` IPC, with a hub-side safety check that refuses the home directory and the clust state directory).
- **Remove Repository confirmation dialog:** A `ConfirmAction` menu with the description "Stop tracking this repository in clust. The folder on disk is left untouched." Options are "Confirm" and "Cancel". On confirm, sends `UnregisterRepo` to the hub.
- **Delete Repository confirmation dialog:** A `ConfirmAction` menu with the description "Stop tracking this repository AND permanently delete the folder from disk. This cannot be undone." Options are "Confirm" and "Cancel". On confirm, sends `DeleteRepo` to the hub. The hub stops all agents for the repo, recursively deletes the folder, and unregisters the repo from the database. Result (success or refusal) is surfaced to the status bar.
- **Purge confirmation dialog:** A `ConfirmAction` menu with a description explaining the destructive operation ("This will stop all agents, delete all worktrees, and delete all local branches."). Options are "Confirm" and "Cancel". On confirm, launches an asynchronous purge operation and displays the purge progress modal.
- **Purge progress modal:** A centered overlay that shows real-time progress during the purge operation. Each phase (stopping agents, removing worktrees, deleting branches, cleaning stale refs) is displayed as a line item with an animated braille spinner while in progress, replaced by a checkmark when complete. All keyboard and mouse input is blocked while the purge is running. On completion, the modal shows "Press Esc to close" and only then accepts Esc to dismiss. If an error occurs, it is displayed in the modal. The purge runs asynchronously via a background task that streams `PurgeProgress` IPC messages from the hub, keeping the TUI responsive throughout.
- **Local branch context menu:** Appears on Enter when a local branch is selected. Contains: "Open Agent" (shown first when the branch has active agents), "Start Agent (worktree)" (always shown; creates a worktree and starts an agent), "Start Agent (in place)" (shown only for the HEAD branch; starts an agent directly in the repo root without creating a worktree, using the existing `StartAgent` IPC message), "Detach" (shown only for the HEAD branch; detaches HEAD from the branch via `DetachHead` IPC), "Checkout" (shown only for non-HEAD branches that are not worktrees; checks out the branch via `CheckoutLocalBranch` IPC), "Base Worktree Off" (always shown; opens the create-agent modal pre-populated with the selected repo and branch -- user only enters a new branch name and prompt), "Pull" (always shown; pulls or fetches the branch -- see Pull Branch below), "Stop Agents" (shown when the branch has active agents), "Remove Worktree" (shown when the branch is a worktree), and "Delete Branch" (force-deletes the local branch via `DeleteLocalBranch` IPC). When "Stop Agents" is selected and the stopped agents were in worktrees, a worktree cleanup dialog is shown after stopping.
- **Detach HEAD confirmation dialog:** When "Start Agent (worktree)" is selected on the HEAD branch, a `ConfirmAction` confirmation dialog is shown before proceeding: "This will detach HEAD in your repo. The branch will be moved to a worktree for the agent." with "Confirm" and "Cancel" options. On confirm, the hub auto-detaches HEAD in the main worktree so the branch can be moved to a linked worktree, then creates the worktree and starts the agent via `CreateWorktreeAgent`. This dialog is shown on both keyboard and mouse paths.
- **Remote branch context menu:** Appears on Enter when a remote branch is selected. Contains: "Checkout & Track Locally" (shown first; checks out the remote branch as a local tracking branch via `CheckoutRemoteBranch` IPC using `git checkout --track`), "Start Agent (checkout)" (creates a worktree from the remote branch and starts an agent), "Create Worktree" (checks out the remote branch as a worktree), and "Delete Remote Branch" (deletes the remote branch via `DeleteRemoteBranch` IPC).
- **Color picker:** Shows the 10 available repo colors (red, orange, yellow, lime, green, teal, blue, purple, pink, coral) with colored `●` indicators. Selecting a color sends a `SetRepoColor` IPC message to the hub.
- **Worktree cleanup dialog:** Appears after stopping agents that were running in worktrees. Shows the worktree branch name (with a dirty indicator if the worktree has uncommitted changes) and offers three options: "Keep" (leave the worktree as-is), "Discard worktree" (remove the worktree via `RemoveWorktree` IPC with force), "Discard worktree + branch" (remove both worktree and local branch). When multiple worktrees need cleanup, dialogs are shown sequentially. Dismissing a cleanup dialog (Esc) advances to the next pending cleanup. This dialog is triggered from four contexts: (1) "Stop All Agents" in the repo context menu, (2) "Stop Agents" in the local branch context menu, (3) immediately when an agent exits in focus mode (if the agent was running in a worktree), (4) immediately when an agent exits in overview mode (if the agent was running in a worktree). In overview mode, the top-of-frame check iterates all panels, collecting pending cleanups for any exited worktree agents, and pops the first cleanup modal; subsequent ones chain via existing `pop_worktree_cleanup_menu` calls. A `worktree_cleanup_shown` flag on each `AgentPanel` prevents the dialog from being shown more than once per agent.
- **Agent picker:** Appears on Enter when a branch has multiple active agents. Lists agent IDs for selection; selecting one opens focus mode.

**Overview tab (Options Bar focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↓` | Enter terminal focus |
| `Shift+←` / `Shift+→` | Scroll viewport left/right |
| `←` / `→` | Navigate repo groups (move cursor left/right) |
| `Enter` / `Space` | Toggle collapse/expand of the selected repo group |

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
│ Changes │ Compare │ Terminal │┌────────────────────┐│
│                               ││ a3f8c1 · claude ●  ││
│      1      1│fn main() {     ││                    ││
│      2       │-  old_code();  ││ Agent PTY output   ││
│         2│+  new_code();  ││ (VTE emulated)     ││
│      3      3│  let x = 1;   ││                    ││
│                               │└────────────────────┘│
├─────────────────────────────────────────────────────┤
│ ● connected  Shift+←/→ switch panel  ...     v0.0.23│
└─────────────────────────────────────────────────────┘
```

**Left panel:**

The left panel has a tab bar at the top with three tabs: `Changes`, `Compare`, `Terminal`. The `Changes` tab shows a unified inline diff viewer showing uncommitted changes (`git diff HEAD`). The `Compare` tab shows a branch comparison diff viewer where users can select any local branch and view the diff between it and the agent's current branch. The `Terminal` tab provides an interactive shell session running inside the agent's worktree directory, allowing users to run shell commands alongside the agent. When the agent has no `repo_path` (non-repository agent), the left panel renders a simplified state: the tab bar and diff viewer are replaced by a centered "Agent not running inside repository" message in tertiary text color on the base background. The diff refresh background task is not spawned for non-repository agents.

When a GitHub PR is detected for the agent's branch, a `PR #N` indicator is appended to the Compare tab label in the tab bar (rendered in `R_INFO` color, bold when the tab is active). The indicator is part of the Compare tab's click region.

**Diff viewer (Changes tab):**

- Displays the output of `git diff HEAD` for the agent's working directory
- Unified inline format with dual-column line numbers (old and new)
- Line-by-line color coding: additions use a green-tinted background (`R_DIFF_ADD_BG`), deletions use a red-tinted background (`R_DIFF_DEL_BG`), file headers use reverse-video styling (repo color background, `R_BG_BASE` foreground, bold) for visual prominence, hunk headers use the repo color as foreground, context lines use the base background. The repo's assigned color is used for file and hunk headers, falling back to `R_ACCENT` when no repo color is available.
- Per-token syntax highlighting is applied to code lines (Add, Delete, Context) via the `syntax` module using `syntect`. The file extension from the diff's file name is used to look up the appropriate TextMate grammar (`syntax_for_file()`). Each token is colored according to a custom Graphite-themed palette mapping 20+ TextMate scopes (keywords, strings, comments, numeric literals, type names, function names, decorators, punctuation, etc.) to Graphite theme colors. Token foreground colors are layered over the diff line's background color (add/delete/context). Lines with unrecognized file types fall back to plain monochrome styling. The `SyntaxSet` and `Theme` are lazy-loaded once via `LazyLock` to avoid repeated initialization cost.
- Blank separator lines are inserted between different files for visual spacing
- File headers display clean file paths (e.g., `src/main.rs`) instead of raw `diff --git a/... b/...` lines
- A gutter column (9 chars wide) shows old/new line numbers separated by a `│` divider; file headers and hunk headers suppress line numbers
- **Line wrapping:** Long lines that exceed the content width (panel width minus gutter) are wrapped to multiple visual rows via `wrap_spans()`. This helper splits syntax-highlighted spans at character boundaries while preserving styles across wrap points. The first visual row of a wrapped line shows the real gutter (line numbers); continuation rows display a blank gutter to keep content aligned. Cursor and selection highlighting covers all visual rows of a wrapped diff line. The rendering loop uses a `while` loop that tracks consumed visual rows rather than a fixed diff-line range, ensuring wrapped lines correctly fill the viewport
- The diff is refreshed every 2 seconds via a background tokio task that runs `git diff HEAD` in a `spawn_blocking` call
- **Cursor and selection:** When the left panel is focused, a cursor line is shown with `R_BG_ACTIVE` background. `↑` / `↓` move the cursor and auto-scroll the viewport. Pressing `v` toggles a selection anchor; while active, all lines between the anchor and cursor are highlighted with `R_SELECTION_BG`. `Enter` sends the selected lines (or the single cursor line if no selection) to the agent's terminal as a text payload prefixed with `# file:` headers for each file span. `Esc` cancels the selection. The cursor and selection are reset when the diff updates, when focus switches away from the left panel, or when switching tabs.
- **Hint bar:** When the cursor is active (left panel focused), a 1-row hint bar is rendered below the diff body. It shows `v select  Enter send  Esc cancel` when no selection is active, or `arrows extend  Enter send  Esc cancel` when a selection is active. Uses `R_TEXT_TERTIARY` foreground on `R_BG_SURFACE` background
- Error state shows the error message in `R_ERROR` color with word wrapping enabled (`.wrap(Wrap { trim: false })`) so long error messages do not overflow the terminal width
- Empty state shows "No uncommitted changes"; loading state shows "Loading diff..."

**Branch Compare (Compare tab):**

- Allows comparing the agent's current branch against any other local or remote branch in the same repository
- **Automatic PR detection:** When entering focus mode, a one-shot background task runs `gh pr view --json number,baseRefName,url` to detect an open GitHub PR for the agent's branch. If a PR is found, the Compare tab auto-selects the PR's base branch and starts the diff, and the left panel auto-switches to the Compare tab (unless the user has already navigated away from the default Changes tab). The `PrInfo` struct (defined in `gitdiff.rs`) holds the PR number, base branch name, and URL. The detection task is only spawned when the agent has both a `repo_path` and a `branch_name`. The base branch name is resolved via `resolve_branch_for_compare()`, which checks for a local branch first, then `origin/<name>`, and falls back to the raw name
- Has two modes controlled by `BranchPickerMode`: `Searching` and `Selected`
- **Searching mode:** Shows a text input field with fuzzy search filtering and a scrollable branch list below it. The agent's own branch is excluded from the list. Uses `SkimMatcherV2` for fuzzy matching, with results sorted by match score descending. Keyboard controls: `↑` / `↓` navigate the list, `Enter` selects a branch and switches to Selected mode, `Esc` cancels and returns to Selected mode, typing filters the list, `Backspace` deletes characters, `←` / `→` move the cursor within the input
- **Selected mode:** Shows a label bar displaying the selected branch name (or "No branch selected" if none), followed by a diff viewer showing the output of `git diff <selected-branch> <agent-branch>`. `↑` / `↓` move the cursor. `v` toggles selection, `Enter` sends the selection to the agent (or re-opens the search picker when no selection is active). `Tab` cycles to the next left panel tab
- The diff is refreshed every 2 seconds via a background tokio task (`spawn_branch_diff_task`) that runs `git diff <base> <head>` in a `spawn_blocking` call, mirroring the Changes tab refresh mechanism
- `BranchPicker` struct manages the picker state: input text, cursor position, selected index, selected branch name, branch list, and a `SkimMatcherV2` fuzzy matcher
- Branch list is updated via `update_compare_branches()` which is called during the repo refresh path, pulling both local and remote branches from the matching `RepoInfo`. Remote branches are displayed with a `[remote]` badge in tertiary text color
- When a branch is selected, `start_compare_diff()` stops any existing compare diff task and spawns a new one
- `drain_compare_diff_events()` is called each frame in the main event loop to process background diff results
- Scroll state, diff data, cursor/selection state, and error state are managed independently from the Changes tab (`compare_diff`, `compare_diff_scroll`, `compare_cursor`, `compare_sel_anchor`, `compare_diff_error`)
- The diff viewer rendering is shared with the Changes tab via a parameterized `render_diff_viewer()` function that accepts optional `cursor` and `sel_anchor` parameters for cursor/selection highlighting
- Mouse scroll within the left panel area is tab-aware, routing to `compare_scroll_up/down` when the Compare tab is active

**Terminal tab:**

- Provides one or more interactive shell sessions per agent, all running inside the agent's worktree directory. Each agent may have many terminals; they are stacked into a label strip (e.g. `[1] [2*] [3]    [+]`) at the top of the tab content, with the active terminal's vterm rendered below
- On entering focus mode (`open_agent()`), a terminal session is automatically started via the hub's `StartTerminal` IPC message
- The shell is spawned by the hub as a PTY process (using `$SHELL` or `/bin/zsh` as fallback) with the agent's `working_dir` as the working directory
- Terminal output is rendered using a `TerminalEmulator` (same `vt100`-backed emulator used for agent panels), supporting full ANSI escape sequences, colors, cursor movement, and alternate screen buffer
- Each terminal's connection runs as its own background tokio task (`terminal_connection_task`) with its own IPC streaming connection to the hub, independent of the agent panel connection. All shells stay live whether or not the user is currently looking at them, so e.g. `npm run dev` keeps running in terminal 1 while the user types in terminal 2
- The Terminal tab has two sub-modes: **Navigate** (default) and **Type**. In Navigate, keystrokes are TUI commands; in Type, they are forwarded to the active shell. The current sub-mode is shown as `Terminal · type` / `Terminal · nav` in the tab bar, and the hardware cursor is only shown in Type mode so the user has a clear visual cue about whether typing reaches the shell. The footer hint mirrors the active sub-mode
- **Sub-mode toggle:** `Ctrl+\` toggles Navigate ↔ Type in both directions. From Navigate, `Enter` is also accepted as a shortcut into Type (since pressing Enter on an empty terminal feels natural); `Enter` is *not* used to leave Type (it must reach the shell). `Ctrl+\` was chosen because it is unbound in bash readline, zsh ZLE, tmux, and vim insert mode, so it never collides with shell input
- **Navigate-mode keys:** `Tab` cycles left-panel tabs; `]` / `[` switch to the next / previous terminal; `n` opens a new terminal (and immediately enters Type mode on it); `x` closes the current terminal (kills the PTY and removes it from the list)
- **Type-mode keys:** keystrokes are forwarded to the active shell. `Ctrl+\` is intercepted to leave Type mode. `Tab` is intercepted to drive TUI-level completion (see *Tab completion* below); the byte still falls through to the shell when no candidate exists
- **Tab completion:** Each `TerminalPanel` keeps a best-effort local mirror (`InputBuffer`) of what the user has typed since the last command boundary (Enter / Ctrl+C / Ctrl+U / Ctrl+G), updated from printable chars and Backspace. When the user presses `Tab`, `compute_completions()` (in `overview/term_complete.rs`) returns either command candidates (PATH executables, when the prefix is the first word and doesn't look path-like) or filesystem candidates (anchored at `working_dir`, `/`, or `~/` depending on the prefix). One candidate is inserted inline (suffix bytes + `/` for directories, ` ` otherwise); multiple candidates open a popup whose state lives in `FocusModeState::completion`. The popup is keyboard-only: `↑`/`↓` to navigate, `Enter` or `Tab` to accept, `Esc` to dismiss; any other key dismisses the popup and falls through. Path completion uses the terminal's *initial* working directory — `cd` is not tracked (a known v1 limitation). The PATH executable list is cached for the lifetime of the process. Buffer-tracking is intentionally simple: arrow keys, `Ctrl+W`, paste, etc. are not modelled; if the buffer drifts, pressing Enter or Ctrl+U resets it
- Paste events (bracketed paste) are forwarded to the active terminal's shell
- Scrollback is supported via `Shift+PageUp` / `Shift+PageDown` with the same scrollback mechanism as agent panels; it always targets the active terminal
- Mouse:
  - Click a label `[N]` in the strip to switch to that terminal (sub-mode is preserved)
  - Click `[+]` to spawn a new terminal and enter Type mode on it
  - Click anywhere in the active terminal's content area to enter Type mode
  - Clicking on a different left-panel tab or on the right panel resets to Navigate mode (so re-entering the Terminal tab starts in Navigate)
  - Scroll wheel over the terminal area scrolls the active terminal's scrollback; scroll wheel over the label strip cycles between terminals
- On terminal resize, every terminal's PTY is resized via `ResizeTerminal` IPC message so the user sees consistent geometry whichever terminal they switch to
- `TerminalPanel` struct manages a single terminal's state: ID, `TerminalEmulator`, command channel, exited flag, scroll offset, per-panel event receiver, and the background task handle. The active focus-mode terminals live in `FocusModeState::terminal_panels: Vec<TerminalPanel>` with `current_terminal_idx: usize` tracking the active one, and a `terminal_input_focused: bool` for the sub-mode
- `TerminalOutputEvent` enum carries output, exited, and connection-lost events from the background task to the UI thread via an `mpsc` channel; each `TerminalPanel` owns its own receiver so output keeps flowing into its `vterm` whether or not focus mode is currently displaying it (and whether or not the user is currently looking at *that* terminal in the tab)
- `TerminalPanel::drain_events()` consumes any pending events on the panel's own receiver and feeds them into its `vterm` / exited flag
- `FocusModeState::drain_terminal_events()` is called each frame in the main event loop alongside `drain_output_events()` and `drain_diff_events()`, and iterates every panel in `terminal_panels`. `OverviewState::drain_cached_terminal_events()` iterates every panel in every cached `AgentTerminalCache` so backgrounded shells accumulate scrollback while focus mode is closed
- When the terminal session exits, the tab displays "Terminal session ended — press x to close, n to start a new one" in tertiary text. When no terminals are open, it displays "No terminals — press n (or click [+]) to start one"
- On focus mode exit, the terminal panels are detached from `FocusModeState` (via `FocusModeState::detach()`, which closes the agent panel and auxiliary tasks but leaves the terminals intact) and stashed on `OverviewState::agent_terminals: HashMap<String, AgentTerminalCache>` keyed by `agent_id`. The cache also remembers `current_idx` so the previously-active terminal is restored on re-entry. No `DetachTerminal` is sent and the shell processes keep running. On the next focus-mode entry for the same agent, `OverviewState::take_agent_terminals` removes the cache entry and `FocusModeState::open_agent` is called with the cache, which routes through `install_existing_terminals` to re-attach every cached panel and resize each to the current focus dimensions instead of spawning new shells. App-level shutdown still uses `FocusModeState::shutdown()` (which calls `close_panel()` → `close_all_terminals()`) to fully tear down all terminals
- Cached terminal panels are pruned by `OverviewState::sync_agents`: any cache entry whose `agent_id` is no longer present in the agent list has every panel's background task aborted and the entry is dropped (covers both the in-focus-when-agent-dies path and the close-focus-then-agent-dies path)
- Each focus-mode terminal is linked to the agent by passing `Some(agent_id)` in its `StartTerminal` message; the hub records this on the `TerminalEntry` so that when the agent exits (explicit stop, worktree removal, or natural exit) every terminal owned by that agent is killed, preventing orphaned child processes such as dev servers
- A hardware cursor (caret) is displayed via `frame.set_cursor_position()` only when the active terminal is the input target *and* the Terminal tab is in Type sub-mode (with the panel left-focused, not scrolled back, and the application has not hidden the cursor via DECTCEM). The cursor position is read from the `TerminalEmulator`'s `cursor_position()` method and clamped to the terminal area bounds

**Panel focus:**

The focus view has a concept of which side (left or right) has keyboard focus. The focused side is indicated by visual cues (tab bar highlight, panel border accent). `Shift+←` and `Shift+→` switch focus between the left and right panels. `Shift+↑` exits focus mode from either panel. When the right panel is focused, `Esc` is forwarded to the agent process. When the agent has no `repo_path` (non-repository agent), `Shift+←` from the right panel is blocked (the left panel cannot receive focus), and mouse clicks on the left panel area do not switch focus to the left panel. Clicking the right panel area still works normally. When switching focus to the left panel (via `Shift+←` or `←` from the right panel), the diff cursor and compare cursor are positioned at the top of the current visible viewport.

**Entry points:**

- **From Overview tab:** While in terminal focus, press `Shift+↓` to open the focused agent in focus mode. The `in_focus_mode` flag is set to `true`.

The agent's `working_dir`, `repo_path`, and `branch_name` are passed to `open_agent()` to determine the git repository for the diff viewer and to display repo/branch identity in the back-bar and status bar.

**Exit:** Press `Shift+↑` from either panel to exit focus mode and return to the originating tab. The `in_focus_mode` flag is set back to `false`. When the right panel is focused, `Esc` is forwarded to the agent process (e.g., to dismiss an agent's own UI element). If the focused agent exits while in focus mode and was running in a worktree, the cleanup dialog is shown immediately (without waiting for the user to exit focus mode). A `worktree_cleanup_shown` flag prevents the dialog from appearing again when focus mode is later exited.

**Implementation:**

- `FocusModeState` manages a single `AgentPanel` with its own IPC background task, output channel, and `TerminalEmulator`. It also manages an optional `TerminalPanel` for the Terminal tab, with its own IPC background task, output channel, and `TerminalEmulator`. It also tracks `branch_name` (in addition to `working_dir` and `repo_path`) to support worktree cleanup dialogs when exiting focus mode. PR detection state is managed via `pr_info: Option<PrInfo>`, a one-shot `mpsc` channel (`pr_detection_tx`/`pr_detection_rx`), and an optional `JoinHandle` for the detection task. The `pr_info` field is cleared on `open_agent()` (when switching agents) and `close_panel()` (on exit).
- The panel dimensions are calculated as 40% of the content area width (minus borders) by the content area height (minus header).
- `FocusSide` enum tracks which panel has keyboard focus (`Left` or `Right`).
- `LeftPanelTab` enum tracks the active tab in the left panel (`Changes`, `Compare`, `Terminal`) with `next()` and `prev()` for cycling in both directions.
- Diff state is managed via `ParsedDiff` (lines, file start indices, file names), `diff_scroll` (current scroll position), `diff_cursor` (current cursor line index), `diff_sel_anchor` (optional selection anchor line index), and `diff_error` (error message if `git diff` failed). The Compare tab has corresponding `compare_cursor` and `compare_sel_anchor` fields. Cursor/selection state is reset on diff updates, tab switches, and focus changes.
- A background diff refresh task (`spawn_diff_task`) runs every 2 seconds and sends `DiffEvent::Updated` or `DiffEvent::Error` via an `mpsc` channel. A `watch` channel signals the task to stop. The diff task is only spawned when `repo_path` is `Some` (i.e., the agent is running inside a git repository).
- `drain_diff_events()` is called each frame in the main event loop alongside `drain_output_events()`.
- `drain_pr_events()` is called each frame in the main event loop to process the one-shot PR detection result. When a PR is detected, it stores the `PrInfo`, resolves the base branch, auto-configures the compare picker, and optionally switches to the Compare tab.
- `parse_unified_diff()` parses raw `git diff HEAD` output into structured `DiffLine` entries with kind (FileHeader, HunkHeader, Context, Add, Delete, FileMetadata, Separator), content, line numbers, and file index. Separator lines are automatically inserted between files during parsing.
- On terminal resize, the focus mode panel is resized via `TerminalEmulator::resize()` (preserving scrollback history) and the hub is notified via `ResizeAgent`. On `FocusGained` events, dimensions are also re-sent unconditionally to account for PTY resizes by other clients while the window was unfocused.
- Focus mode is orthogonal to tab cycling -- `Tab` / `Shift+Tab` cycles between `Repositories` and `Overview`. Focus mode is only entered explicitly via the entry points above.
- State is tracked by an `in_focus_mode: bool` flag rather than a `previous_tab` option. The `ActiveTab` enum no longer has a `FocusMode` variant.
- On user-initiated exit (returning to a tab via `Shift+↑` or back-bar click), `FocusModeState::detach()` is called: the agent panel connection is detached, the diff task is stopped via the watch channel and aborted, diff state is cleared, the PR detection task is aborted and `pr_info` is cleared, and the terminal panel is taken (kept alive) so the caller can stash it on `OverviewState::agent_terminals`. On full app shutdown (or when the agent has been pruned), `close_panel()` is used instead, which performs the same teardown plus closes the terminal session (sends `DetachTerminal` and aborts the terminal background task).

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
| `↑` / `↓` | Move cursor up/down (auto-scrolls viewport) |
| `v` | Toggle selection anchor at cursor position |
| `Enter` | Send selected lines (or current cursor line) to the agent terminal, then cancel selection |
| `Esc` | Cancel selection (clear anchor) |
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab (cancels selection) |

**Keyboard shortcuts (focus mode, left panel focused, Compare tab -- Selected mode):**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move cursor up/down (auto-scrolls viewport) |
| `v` | Toggle selection anchor at cursor position |
| `Enter` | If selection is active: send selected lines to the agent terminal and cancel selection. Otherwise: open branch search picker |
| `Esc` | Cancel selection (clear anchor) |
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab (cancels selection) |

**Keyboard shortcuts (focus mode, left panel focused, Compare tab -- Searching mode):**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Navigate branch list up/down |
| `Enter` | Select highlighted branch, start diff |
| `Esc` | Cancel search, return to Selected mode |
| Typing | Filter branch list with fuzzy search |
| `Backspace` | Delete character before cursor |
| `←` / `→` | Move cursor within search input |

**Keyboard shortcuts (focus mode, left panel focused, Terminal tab):**

| Shortcut | Action |
|----------|--------|
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+→` | Switch focus to right panel |
| `Shift+Tab` (`BackTab`) | Cycle to previous left panel tab |
| `Tab` | Cycle to next left panel tab |
| `Shift+PageUp` | Scroll terminal scrollback up by one page |
| `Shift+PageDown` | Scroll terminal scrollback down by one page |
| `Esc` | Forwarded to the terminal shell |
| All other keys | Forwarded to the terminal shell's PTY |

### Help Overlay (`?`)

The `?` key toggles a keyboard shortcut overlay rendered as a centered modal (44 columns wide) anchored to the bottom of the content area. The modal is organized into sections with bold secondary-colored headers and context-aware visibility:

- **Global section (always shown):** `q / Esc×2`, `Q`, `Ctrl+C`, `Tab`, `Shift+Tab`, `?`, `F2`, `Alt+M`, `Alt+E`, `Alt+D`, `Alt+F`, `Alt+N`, `Alt+V`, `Alt+B`, `Alt+P`, `Alt+T`, `Alt+I`, `Cmd+1`, `Cmd+2`.
- **Repositories section (shown when Repositories tab is active):** `↑/↓` navigate items, `←/→` navigate tree, `Shift+↑/↓` jump prev/next repo, `Enter` open menu, `Space` collapse/expand.
- **Overview section (shown when Overview tab is active):** `Shift+←/→` scroll panels, `Shift+↓` enter terminal, plus an "In terminal:" sub-context label followed by `Shift+↑` back to options bar, `Shift+↓` enter focus mode, `Shift+←/→` switch agent, `PgUp/PgDn` scroll terminal.
- **Schedule section (shown when Schedule tab is active):** `Alt+S` schedule new task, `Shift+←/→` switch focused task, `↑/↓` scroll prompt, `d/Del` delete, `Shift+C` clear by status. Three sub-context labels follow: "Inactive task:" (`e` edit prompt, `p` toggle plan, `x` toggle auto-exit, `s` start now, `Shift+S` reschedule), "Active task:" (typing forwards to the agent — `Shift+↓` enter focus mode, `PgUp/PgDn` scroll), "Aborted task:" (`e` edit, `p` plan, `x` auto-exit, `r` restart, `Shift+R` clean restart, `Shift+S` reschedule).
- **Focus Mode section (shown when in focus mode):** `Shift+↑` exit, `Shift+←/→` switch panel, `Shift+PgUp/PgDn` scroll terminal, plus a "Left panel:" sub-context label followed by `Tab` cycle tabs, `Shift+Tab` prev tab (used in Terminal tab since Tab is forwarded to the shell), `↑/↓` move cursor, `v` toggle selection, `Enter` send selection, `Esc` cancel selection.

Key names are displayed in accent color (left-aligned, 16 chars wide); descriptions use primary text color. Section headers use secondary text color with bold modifier. Sub-context labels use tertiary text color and are indented.

### Schedule Tab

A third top-level tab next to Repositories and Overview, dedicated to **persistent scheduled tasks**. Each task is one row in the SQLite `scheduled_tasks` table (see `storage.md`) and survives hub restarts. Tasks are identified by their git branch name; only one non-completed task can target any given branch at once.

**Layout.** A single-row **top bar** (mirroring the Overview options bar) above a horizontal grid of vertical task columns, with a fixed two-row **keybind hint footer** below so every applicable shortcut is visible at a glance. `Shift+Left` / `Shift+Right` focus the previous / next column; `Shift+Down` enters focus mode (Active tasks only).

**Typing into Active panels.** When the focused task is `Active`, regular keys (and pasted text wrapped in bracketed-paste markers) are forwarded directly to that agent's PTY — no need to drop into focus mode for quick interaction. Reserved keys: `Shift+←/→` (switch panel), `Shift+↓` (focus mode), `PgUp`/`PgDn` (scroll the panel's scrollback). Everything else, including `Esc`, `Ctrl+C`, `q`, `Tab`, `Shift+C`, etc., is forwarded to the agent. To use `q`/`Tab`/`Shift+C` as TUI commands, navigate to a non-Active task first with `Shift+←/→`. Mirrors Overview's terminal-focus key flow.

**Top bar.** Shows the same kind of summary as the Overview options bar. On the left, one chip per repo that currently has a scheduled task: a coloured `●` plus the repo name in primary text. A vertical-bar separator follows, then one branch indicator per task. Branch indicators are coloured by repo: tasks visible on screen render in inverse video (repo-coloured background, primary-text foreground); tasks scrolled off render in repo-coloured text on the bar background. Each branch chip is **clickable** — clicking one focuses that task and scrolls the panel grid until it is visible (handy when many tasks have been scheduled at once and only some fit on screen).

**Repo grouping.** Tasks are sorted by repo (using the same order as the Overview repo chips), then by `created_at`, then by id. Each task's column border tints with its repo colour — full colour when focused, dimmed when unfocused — so the same per-repo palette used in Overview applies here. When the focused task survives a re-sync that reshuffles indices, focus follows the task by id rather than staying glued to the numeric slot.

**Keybind hint footer (bottom 2 rows of the tab area):**
- Row 1 (always the same): `Shift+← prev · Shift+→ next · Opt+S new task · d/Del delete · Shift+C clear by status · ? help`.
- Row 2 (status-aware): a colored status pill (`INACTIVE` / `ACTIVE` / `ABORTED` / `COMPLETE`) followed by only the bindings that apply to the focused task — e.g. for Inactive: `e edit prompt · p toggle plan · x toggle auto-exit · s start now · Shift+S reschedule · ↑/↓ scroll prompt`; for Active: `type send to agent · Shift+↓ focus mode · PgUp/PgDn scroll`. When no task is focused (empty list), row 2 invites the user to press `Opt+S` to begin. Keys are rendered in `R_ACCENT_BRIGHT` bold; descriptions in `R_TEXT_SECONDARY`; separators in `R_TEXT_DISABLED`. **The footer is also a clickable button strip** — clicking any hint with a defined key fires the same action (e.g. clicking `e edit prompt` opens the edit-prompt modal). The `↑/↓ scroll prompt` hint is the only non-clickable hint, since its action is the wheel itself.

**Per-column rendering depends on `status`:**

| Status | Column body |
|--------|-------------|
| `Inactive` | Status pill, `PLAN` and `AUTO-EXIT` indicator pills, a schedule info line that highlights inline keybinds in accent color (e.g. "Unscheduled — press **s** to start now" / "Starts in 1h 23m" / "Waiting on N task(s)"), and the full prompt text wrapping on the X axis and scrollable on Y. If any upstream dep has Auto-Exit OFF, a warning line appears: `⚠ depends on tasks without AUTO-EXIT`. |
| `Active` | A live `TerminalEmulator` rendering the agent's PTY output, filling the entire inner area of the column (no inner header row, status pills, or hint line — those would only steal rows from the live output). The keybind footer at the bottom of the tab continues to advertise `Shift+↓ focus mode`. |
| `Complete` | Centred branch name + small green `✓`, with a final `press d to remove` hint. |
| `Aborted` | Status pill (red), the original prompt, and an inline hint "Aborted — press **r** to restart, **Shift+R** for clean restart" with the keys highlighted in accent color. |

**Opening a task (Opt+S modal).** Mirrors the create-agent modal but adds a `Select schedule kind` step (Schedule / Depend / Unscheduled) and either a final time-entry step or a multi-select dependency step. Time strings accept `Ns`, `Nm`, `Nh`, `Nd` durations or wall-clock `HH:MM`. The prompt step rejects empty input. `Alt+P` toggles plan mode and `Alt+X` toggles `Auto-Exit` from anywhere in the modal — Auto-Exit defaults to OFF and only takes effect for agents that advertise the Stop hook (Claude today). If the chosen branch is already used by a non-completed scheduled task, the hub rejects the create message and the modal surfaces "branch '<x>' is already scheduled" on the status bar.

**Dep picker contents.** The `Depend` step lists every existing scheduled task **and** every currently-running Opt+E worktree agent (with a known repo + branch and no shadow task linked yet). Task rows display `repo / branch [STATUS]`; agent rows display `repo / branch [AGENT AUTO-EXIT]` or `[AGENT no-auto-exit]` so you can see at a glance whether the upstream agent will reach `Complete` on its own. Selecting an agent as a dep triggers the hub to insert a shadow `scheduled_tasks` row (`status=active`, `agent_id=<the running agent>`, `schedule_kind=unscheduled`) before persisting the new task — that's the only path by which an Opt+E agent shows up on the Schedule tab. When the agent exits, the shadow row flips to `Complete` via the existing PTY-reader hook, unblocking downstream tasks.

**Editing a task.** Press `e` on an Inactive or Aborted column for the inline edit-prompt modal (multi-line input, Enter to save a non-empty value, Esc to cancel). `p` toggles plan mode and `x` toggles Auto-Exit on the focused task without re-opening the create flow. `s` starts an Inactive task immediately. `r` and `R` restart Aborted tasks (in place / with worktree reset).

**Rescheduling a task.** Press `Shift+S` on an Inactive or Aborted column to open the schedule modal in *reschedule* mode. The repo, branch, prompt and the plan/auto-exit flags are carried over from the existing row — the modal jumps straight into the "pick when to start" step and only the trigger (Schedule / Depend / Unscheduled) and its associated start time or dep set can be overwritten. Aborted tasks flip back to Inactive on submit so the new schedule takes effect on the next scheduler tick. The hub message is `RescheduleScheduledTask { id, schedule, extra_agent_deps }`; the dep edges in `scheduled_task_deps` are rewritten in a single transaction.

**Deleting tasks.** `d` or `Delete` opens a confirmation; if the task is Active, the hub stops the agent before removing the row. `Shift+C` opens the bulk-clear-completed confirmation, which sends `DeleteScheduledTasksByStatus { status: Complete }`.

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

**Plan mode toggle:** `Alt+P` toggles plan mode on/off within the modal (available in all steps). When bypass permissions is globally enabled, plan mode starts ON automatically.

**Auto-Exit toggle:** `Alt+X` (`Opt+X` on macOS) toggles `Auto-Exit` from anywhere in the modal — defaults to OFF and only takes effect for agents that advertise the Stop hook (Claude today). The same flag the schedule modal exposes; surfaced here so a manually spawned Opt+E agent can later be promoted into a scheduled-task dependency chain (Opt+S dep picker — see "Schedule Tab"). Without auto-exit, a manually spawned dep would never reach `Complete` and downstream scheduled tasks would stall.

**Status bar:** A bottom row of the modal shows two pills — "PLAN" / "Plan" and "AUTO-EXIT" / "Auto-Exit" — followed by `Opt+P plan · Opt+X auto-exit` hints. Each pill renders bold/coloured when the flag is on and dimmed when off, mirroring the Opt+S modal exactly.

**Completion:** On completing step 4, the modal sends a `CreateWorktreeAgent` IPC message to the hub with `plan_mode` and `auto_exit` set according to the toggle state. The hub creates the worktree (via the existing `add_worktree()` logic), spawns an agent in it, and returns `WorktreeAgentStarted`. The behavior depends on the active tab: when on the **Overview tab**, the TUI stays in overview mode and selects the newly created agent's panel after the next agent sync (via `pending_overview_select` and `OverviewState::select_agent_by_id()`); when on the **Repositories tab**, the TUI opens the new agent in focus mode as before. On success, a status bar message confirms the agent started (e.g., "Agent started on feature-branch"). On failure (hub connection error, send error, unexpected response, or hub-reported error), the error is surfaced as a status bar error message instead of being lost to stderr. The `AgentStartResult` enum has `Started` and `Failed(String)` variants to communicate the outcome from background tokio tasks to the main event loop.

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

**Plan mode toggle:** `Alt+P` toggles plan mode on/off within the modal (available in all steps). When bypass permissions is globally enabled, plan mode starts ON automatically. A status bar at the bottom of the modal shows the current plan mode state ("PLAN" in warning/bold when enabled, "Normal" in disabled text) with an `Alt+P toggle plan mode` hint.

**Completion:** On completing step 2, the modal sends a `CliMessage::StartAgent` IPC message to the hub with `plan_mode` set according to the toggle state (reusing the existing agent start path). The hub auto-detects git repository information if the selected directory is inside a git repo. The `AgentStartResult::Started` variant includes an `is_worktree: bool` field to properly propagate whether the agent is running in a worktree. On success, the TUI opens the new agent in focus mode with a status message (e.g., "Agent started in /path/to/dir"). On failure, the error is surfaced as a status bar error message.

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

### Text Input and Paste Handling

All modal text inputs (Create Agent, Search Agent, Detached Agent, Add Repository, and the Branch Picker in focus mode) track cursor position as a **byte offset** into the UTF-8 `String`, not a character index. This ensures correct behavior when the input contains multi-byte UTF-8 characters (e.g., em-dash, en-dash, accented characters):

- **Insert:** advances `cursor_pos` by `c.len_utf8()`
- **Backspace / Left arrow:** retreats `cursor_pos` to the previous character boundary via `char_indices().next_back()`
- **Right arrow:** advances `cursor_pos` by the byte length of the character at the current position
- **Render:** the cursor character is extracted by slicing a full character from the byte offset, not a single byte

Bracketed paste mode is enabled via `crossterm::EnableBracketedPaste` on TUI startup and disabled on exit. This causes pasted text to arrive as a single `Event::Paste(String)` rather than as individual `KeyCode::Char` events. Without bracketed paste, pasted newlines would trigger `Enter` (submitting forms prematurely) and escape characters would cancel modals. Each modal exposes a `handle_paste()` method that inserts the pasted text character-by-character (stripping newlines and carriage returns) while maintaining the byte-offset cursor position. When no modal is active, paste events are forwarded to agent terminals: in focus mode (when the right panel is focused) and in overview mode (when a terminal panel is focused), the pasted text is wrapped in bracketed paste sequences (`\x1b[200~`...`\x1b[201~`) and sent to the agent's PTY input.

### Editor Integration

The `Opt+V` (macOS) / `Alt+V` shortcut opens the current context in an external editor. Available globally across all modes (focus, overview, repository).

**Editor detection:** On startup, the TUI scans PATH for known editor binaries using the `which` crate. Detected editors are cached for the session. Editors are sorted by category:

| Category | Editors |
|----------|---------|
| Generic | VS Code (`code`), Cursor (`cursor`), Zed (`zed`), Sublime Text (`subl`) |
| JetBrains | IntelliJ IDEA (`idea`), WebStorm (`webstorm`), PyCharm (`pycharm`), GoLand (`goland`), RustRover (`rustrover`), CLion (`clion`), PHPStorm (`phpstorm`), Rider (`rider`), Fleet (`fleet`) |
| Terminal | Neovim (`nvim`), Vim (`vim`), Emacs (`emacs`), Helix (`hx`) |

GUI editors (Generic, JetBrains) are opened directly via their binary. Terminal editors are opened in a new terminal window (via `osascript` on macOS, or by trying `x-terminal-emulator`, `gnome-terminal`, `konsole`, `xfce4-terminal` on Linux).

**Target resolution:** The target path depends on the current context:

| Context | Target |
|---------|--------|
| Focus mode | Agent's working directory |
| Repositories tab (repo selected) | Repository root path |
| Repositories tab (HEAD branch selected) | Repository root path |
| Repositories tab (worktree branch selected) | Worktree directory |
| Repositories tab (non-worktree local branch selected) | Worktree is created on-demand for the branch, then opened |
| Overview tab (terminal focused) | Agent's working directory |

**Discoverability:** Every surface where the shortcut is meaningful exposes it visibly:

- The bottom status bar lists `Opt+V open editor` (Repositories, Overview/terminal-focused, Focus mode).
- A selected repo/branch row in the Repositories tree shows an inline `Opt+V open` pill next to `Enter`.
- The branch context menu (Enter on a local branch) includes either **Open in editor** (HEAD or worktree branch) or **Create worktree and open in editor** (non-worktree branch). Picking that entry has the same effect as the keybind.

**Flow:**

1. If the selected branch has no worktree (and is not HEAD), a worktree is created on-demand via `AddWorktree { checkout_existing: true }` before opening.
2. If the repository has a saved editor preference (per-repo `editor` column or global `default_editor`), the editor opens immediately without showing any modal.
3. If multiple editors are detected, an **editor picker modal** is shown listing all detected editors by name. The user selects one.
4. If only one editor is detected, it opens immediately.
5. After opening (when the target is inside a repository), an **editor remember modal** asks "Remember this editor?" with three options:
   - "Just this time" -- no preference saved
   - "For this repository" -- saves the editor in the `repos.editor` column via `SetRepoEditor` IPC
   - "For all repositories" -- saves the editor as the global default via `SetDefaultEditor` IPC

Both modals use the standard `ContextMenu` rendering (centered modal overlay with arrow key navigation, Enter to confirm, Esc to dismiss, number keys for direct selection, mouse click support).

### Mouse Support

Mouse capture is enabled via `crossterm::EnableMouseCapture` on TUI startup and disabled on exit. The Kitty keyboard protocol (`PushKeyboardEnhancementFlags` with `DISAMBIGUATE_ESCAPE_CODES`) is also enabled when the terminal supports it, allowing detection of the SUPER (Cmd) modifier on mouse events. Terminals that do not support the Kitty protocol gracefully degrade (the modifier is simply not reported). All mouse interactions use `MouseEventKind::Down(MouseButton::Left)` for clicks and `MouseEventKind::ScrollUp`/`ScrollDown` for scroll wheel.

#### F2 Mouse Capture Toggle

Pressing `F2` toggles mouse capture on/off. When mouse capture is disabled, the terminal emulator regains control of mouse events, allowing native text selection, copy/paste, and link clicking. When mouse capture is off, all mouse events (clicks, scrolls) are ignored by the TUI. The status bar displays a `MOUSE OFF . F2` indicator in the warning color when mouse capture is disabled. Pressing `F2` again re-enables mouse capture and restores normal TUI mouse handling. The `mouse_captured` boolean state is tracked in the main event loop and passed to `render_status_bar()` for display.

#### Alt+M Mouse Passthrough (5 seconds)

Pressing `Alt+M` (or `Opt+M` on macOS) temporarily disables mouse capture for 5 seconds, then automatically re-enables it. This is useful for quickly clicking a terminal link without needing to manually toggle mouse capture back on. The status bar displays `MOUSE OFF . ⌥M` during the passthrough period (distinct from the persistent `MOUSE OFF . F2` indicator). The passthrough is tracked via a `mouse_passthrough_until: Option<Instant>` state variable. At the top of each event loop iteration, the timer is checked; when the deadline is reached, mouse capture is re-enabled automatically. Pressing `F2` while a passthrough is active clears the passthrough timer and applies the persistent toggle instead.

#### Click Map Architecture

A `ClickMap` struct is populated during each render pass and consumed during mouse event handling. During rendering, each clickable element records its bounding `Rect` and associated action target into the click map. When a mouse click arrives, the handler checks each region in the click map to determine what was clicked. The click map is rebuilt from scratch every frame.

`ClickMap` fields:
- `tabs` -- tab bar regions mapped to `ActiveTab` values
- `tree_items` / `tree_inner_area` -- repo tree line targets mapped via `TreeClickTarget` enum (Repo, Category, Branch)
- `overview_panels` -- Overview tab panel regions mapped to global panel indices
- `overview_repo_buttons` -- Overview tab repo group block regions mapped to repo path strings (click to toggle collapse/expand)
- `overview_agent_indicators` -- Overview tab agent indicator regions within repo group blocks mapped to global panel indices (click to focus agent)
- `schedule_panels` -- Schedule tab task panel regions mapped to task indices (click to focus the task)
- `schedule_branch_indicators` -- Schedule tab top-bar branch chip regions mapped to task indices (click to focus + scroll into view)
- `schedule_hint_keys` -- Schedule tab footer keybind hints mapped to `ScheduleHintKey` values (click fires the same action the key would)
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

The right panel is view-only — clicks in it have no effect. To interact with an agent, switch to the Overview tab via `Tab`.

**Overview tab:**

| Click Target | Action |
|--------------|--------|
| Repo group block | Toggle collapse/expand for that repo (collapsed repos hide their agent indicators and filter their panels out of the viewport) |
| Agent indicator (within repo block) | Focus that agent's terminal panel (`OverviewFocus::Terminal(idx)`) |
| Agent panel | Focus that terminal panel (`OverviewFocus::Terminal(idx)`) |

**Schedule tab:**

| Click Target | Action |
|--------------|--------|
| Branch chip in top bar | Focus that task and scroll the panel grid until it is visible |
| Task panel | Focus that task |
| Footer keybind hint (e.g. `e edit prompt`, `s start now`, `r restart`) | Trigger the same action the key would (the click is dispatched through `ScheduleState::handle_key`, so all status/focus rules apply identically) |
| Footer keybind hint (`Opt+S new task`, `?` help) | Open the schedule modal / show the help overlay (mirrors the global keyboard shortcut) |

The hint footer is two rows: the top row holds globally-applicable hints (prev/next panel, new task, delete, clear, help) and the bottom row holds focus-specific hints whose contents change with the focused task's status (Inactive / Active / Aborted / Complete). All hints with a defined keystroke are clickable; the `↑/↓ scroll prompt` hint on the Inactive row is not (scrolling is handled via the wheel instead). Each hit zone covers the full `key + space + description` width so the user can click sloppily and still hit the right action.

**Focus mode:**

| Click Target | Action |
|--------------|--------|
| Left panel tab (Changes/Compare/Terminal) | Switch to that tab and focus the left panel (only when agent has a repo) |
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

**Schedule tab:**

| Cursor Position | Scroll Action |
|-----------------|---------------|
| Over a task panel | Scroll that task's prompt up/down by one line per wheel tick (1 line). For Inactive/Aborted tasks this scrolls the prompt body; for Active tasks the wheel scroll has no visible effect because the panel embeds a live agent vterm whose scrollback is reachable from focus mode. Focus is **not** changed by the wheel — the user can scroll a panel's prompt without losing their current task selection. |

**Focus mode:**

| Cursor Position | Scroll Action |
|-----------------|---------------|
| Over the right panel | Scroll the agent terminal scrollback up/down |
| Over the left panel | Scroll the diff viewer up/down |

The Repositories tab does not have scroll wheel handling.
