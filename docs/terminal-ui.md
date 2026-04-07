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

A 1-row bar at the top of the terminal with three tabs:

| Tab | Description |
|-----|-------------|
| `Repositories` | Two-panel view with repo tree and agent cards (default) |
| `Overview` | Multi-agent terminal overview with horizontal panels |
| `Jobs` | Batch creation and task management with horizontal batch cards |

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
│ ● connected  Shift+↓ enter terminal  ...    v0.0.16 │
└─────────────────────────────────────────────────────┘
```

**Layout:**

- **Filter bar (1 row):** A single-line bar at the top that groups agents by repository. The left section shows repo chips -- each repo has a colored `●` dot (using the repo's assigned color) followed by the repo name. The cursor position is indicated with `R_BG_ACTIVE` background on the chip. A `│` separator divides the left and right sections. The right section shows all agent branch indicators colored by their repo's assigned color. Visible agents use inverse video styling (repo color background, primary text foreground); off-screen agents use the repo color as foreground; collapsed repo agents use `R_TEXT_DISABLED`. Agents without a repository are grouped under a synthetic "Other" chip using the accent color. Clicking a repo chip toggles collapse/expand -- collapsed repos have dimmed dots and `R_TEXT_DISABLED` text, and their agent panels are filtered out of the viewport. Clicking an individual branch indicator focuses that agent's terminal panel. When no repos exist, the bar is rendered as an empty 1-row area. Background changes based on focus (`R_BG_OVERLAY` when focused, `R_BG_RAISED` when unfocused). Panels are ordered by repo group (matching repo registration order), then by batch group (non-batch agents first, then batch agents grouped by batch_id and ordered by task_index), then by branch name, then by agent ID within each group via `compute_sorted_indices()`. Batch agent indicators in the right section are preceded by a bold batch name label (e.g., "Batch 1:") in `R_INFO` color, inserted when entering a new batch group.
- **Agent panels (horizontal):** Dynamically sized columns distributed evenly across the available width. The number of visible panels is determined by how many fit at the minimum width of 60 columns. Panels use ratio-based constraints so they fill the screen evenly (1 panel = half screen, 2 panels = half each, 3 panels = one-third each, etc.). A single panel never exceeds half the screen width. When more agents exist than fit on screen, horizontal scrolling is enabled with `◀ N` / `N ▶` indicators.
- Each panel has **box-drawing borders** (top, bottom, left, right). When a panel's agent is associated with a repository, the border color uses the repo's assigned color (bright when focused, dimmed to 60% brightness when unfocused via `dim_color()`). Panels without a repo fall back to accent blue when focused and subtle gray when unfocused. Batch agent panels display a **batch title in the top border** showing the batch name in bold `R_INFO` color followed by the task position (e.g., "Batch 1 2/5") in `R_TEXT_TERTIARY` color.
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

#### Jobs Tab

A batch management tab that displays created batch definitions as horizontal cards. Batches have a launch mode (Auto or Manual). Auto-mode batches can be toggled between Idle and Active status, and when activated, automatically start agents for their tasks via `CreateWorktreeAgent` IPC. Manual-mode batches display "Manual" status and allow starting individual tasks one at a time via `Opt+S` / `Alt+S`. Auto-mode Idle batches can also be queued for scheduled execution via the `t` keybinding, which sends a `QueueBatch` IPC message to the hub daemon. Queued batches display a live countdown until their scheduled start time and can be cancelled with `Space`.

**Layout:**

- **Options bar (1 row):** A single-line bar at the top showing the batch count (e.g., "3 batches") and hints `Opt+T create batch`, `Space toggle status`, `M mode`, `B bypass`, and `d clear done`. The bar uses `R_BG_RAISED` background.
- **Batch cards (horizontal):** Dynamically sized columns distributed evenly across the available width. The minimum card width is 40 columns. Cards use ratio-based constraints so they fill the screen evenly. A single card never exceeds half the screen width (minimum 2 slots). When more batches exist than fit on screen, horizontal scrolling is enabled via `Shift+Left`/`Shift+Right`.

Each batch card has:
- **Box-drawing borders** using the repo's assigned color (bright when focused, dimmed when unfocused via `dim_color()`). Cards without a repo fall back to accent blue when focused and tertiary text color when unfocused.
- **Title** displayed in the border using the repo's assigned color (bright when focused, dimmed when unfocused via `dim_color()`). Cards without a repo fall back to accent bright when focused and accent when unfocused.
- **Card body** showing: Repo name (in repo color), Branch name, Workers (concurrency limit or infinity symbol, for Auto-mode batches) or Mode ("Manual" in info color, for Manual-mode batches), Tasks (count of tasks added to the batch), Prefix (prompt prefix or "(none)"), Suffix (prompt suffix or "(none)"), Mode ("Plan" in warning/bold when plan_mode is enabled, "Normal" in disabled text otherwise), Bypass ("Allowed" in warning/bold when allow_bypass is enabled, "Off" in disabled text otherwise), Status (Idle in disabled/gray, Active in green/bold for Auto-mode, "Queued HH:MM (Xh Ym)" in info color with live countdown for queued batches; "Manual" in info color for Manual-mode), and a task box list below the metadata.
- **Task boxes:** Each task is rendered as a full-width box within the batch card, separated by horizontal lines colored by status (green for Active, gray for Idle, tertiary for Done; accent bright when the task is focused). Each task box displays: focus indicator (`>` when focused), task number, status indicator, branch name (truncated with ellipsis if too long), and truncated first line of the prompt. Tasks are sorted with Active tasks above Idle and Done tasks.
- **Terminal preview:** Active task boxes optionally show a small terminal output preview (last 4 lines of the agent's terminal output). The preview is gated behind the `SHOW_TERMINAL_PREVIEW` constant in `tasks/mod.rs` for easy toggling. Preview data is extracted from the corresponding `AgentPanel` via the task's `agent_id` field.
- Focused cards use `R_BG_SURFACE` background; unfocused cards use `R_BG_BASE`.

**Empty state:** When no batches exist, a centered message is displayed: "No batches defined -- press Opt+T to create one".

**Focus modes:**

| Focus | Description |
|-------|-------------|
| BatchList | Default. No card is focused. |
| BatchCard(N) | A specific batch card is focused and can be deleted. |

**Keyboard shortcuts (Jobs tab):**

| Shortcut | Action |
|----------|--------|
| `Left` / `Right` | Navigate between batch cards |
| `Shift+Left` / `Shift+Right` | Scroll the batch viewport left/right |
| `Down` | Focus the first visible batch card, or navigate to the next task within a focused batch card |
| `Up` | Navigate to the previous task within a focused batch card, or return to batch list focus when no task is focused |
| `Delete` / `Backspace` | Remove the focused batch card |
| `Enter` | Open the Add Task modal for the focused batch card |
| `Space` | Toggle focused batch status between Idle and Active (Auto-mode batches only; no-op for Manual-mode). If the batch is Queued, cancels the queue via `CancelQueuedBatch` IPC and reverts to Idle. |
| `t` | Open the Timer modal to set a scheduled start time for the focused batch (Auto-mode Idle batches only). Sends `QueueBatch` IPC to the hub daemon. |
| `Opt+S` (macOS) / `Alt+S` | Start the focused task in a Manual-mode batch (only when a task is focused and the task is Idle) |
| `m` | Toggle plan mode on the focused batch card |
| `b` | Toggle allow bypass on the focused batch card |
| `p` | Open the Edit Field modal to edit the prompt prefix of the focused batch |
| `s` | Open the Edit Field modal to edit the prompt suffix of the focused batch |
| `d` | Remove all tasks with Done status from the focused batch card |

**State management:**

- `TasksState` manages the batch list, focus state, task-level focus, scroll offset, and ID generation.
- `TasksFocus` enum tracks whether the batch list or a specific card is focused.
- `focused_task: Option<usize>` tracks which task within a focused batch card has task-level focus. `None` means no task is focused; `Some(i)` means task at index `i` is focused. Reset when navigating between batch cards.
- `BatchInfo` stores: id, title, repo_path, repo_name, branch_name, max_concurrent, launch_mode (`LaunchMode`), prompt_prefix, prompt_suffix, tasks (list of `TaskEntry`), status (`BatchStatus`: Idle or Active), plan_mode (bool), allow_bypass (bool), and created_at timestamp.
- `LaunchMode` enum: `Auto` (default, batches use concurrency-based toggling) and `Manual` (tasks are started individually).
- `TaskEntry` stores: branch_name, prompt, status (`TaskStatus`: Idle, Active, or Done), and an optional `agent_id` (set when the task's agent is started, linking it to its `AgentPanel` in `OverviewState`) for a single task within a batch.
- `BatchAgentInfo` stores: batch_title, batch_id, task_index, and task_count for an agent that belongs to a batch. Built by `TasksState::batch_agent_map()` which returns a `HashMap<String, BatchAgentInfo>` mapping agent IDs to their batch membership info. Used by the overview tab for sorting, filter bar labels, and panel border titles.
- `TerminalPreviewMap` (type alias for `HashMap<String, Vec<Line>>`) maps agent IDs to their last N terminal output lines, built by `build_task_terminal_previews()` in `ui.rs` each render frame.
- `SHOW_TERMINAL_PREVIEW` constant (default `true`) gates whether terminal output previews are shown in active task boxes.
- `TASK_TERMINAL_PREVIEW_LINES` constant (default `4`) controls how many terminal output lines are shown in the preview.
- `BatchStatus` enum: `Idle` (default, gray/disabled text), `Active` (green bold text), and `Queued { scheduled_at: String, batch_id: String }` (info color with live countdown). Queued batches display a formatted countdown (e.g., "Queued 16:00 (1h 30m)") that updates each render frame via `timer_modal::format_countdown()`.
- `TaskStatus` enum: `Idle` (gray/disabled text), `Active` (green bold text), and `Done` (amber/warning text).
- `add_task()` adds a `TaskEntry` (with `Idle` status) to a specific batch by index.
- `remove_done_tasks()` removes all tasks with `Done` status from a batch by index (retains only non-Done tasks).
- `toggle_plan_mode()` toggles plan mode on a batch by index.
- `toggle_allow_bypass()` toggles allow bypass on a batch by index.
- `set_prompt_prefix()` / `set_prompt_suffix()` update the prompt prefix/suffix for a batch (empty string clears to `None`).
- `BatchInfo::build_prompt(task_prompt)` combines the batch prefix, task prompt, and batch suffix into a single string (joined with double newlines). Used by the agent spawner when starting batch tasks.
- `toggle_batch_status()` toggles a batch between Idle and Active. Returns `None` for Manual-mode batches. When transitioning an Auto-mode batch to Active, returns a `BatchStartInfo` containing the tasks to start (up to `max_concurrent` minus already-active tasks). Only Idle tasks are started. The batch's `plan_mode` and `allow_bypass` settings are passed through to the `CreateWorktreeAgent` IPC message for each spawned agent.
- `start_single_task()` starts a single task by index within a Manual-mode batch. Returns `None` if the batch is not Manual-mode or the task is not Idle. Returns a `BatchStartInfo` with exactly one task to start.
- `focus_task_down()` / `focus_task_up()` navigate task-level focus within a focused batch card. Down enters task focus from None to first task; Up from the first task exits task focus back to None.
- `batch_by_id_mut()` finds a batch by its unique id for updating task statuses after agent start results.
- `mark_agent_done(agent_id)` finds the task associated with the given agent_id, marks it as Done, and if the batch is still Active with Idle tasks remaining, returns a `BatchStartInfo` describing the next task(s) to start (respecting `max_concurrent`).
- `BatchStartInfo` struct carries batch_id, repo_path, target_branch, and a list of (task_index, branch_name, prompt) tuples for tasks that need agents started.
- `spawn_batch_tasks()` is a shared helper function that spawns `CreateWorktreeAgent` IPC requests for each task in a `BatchStartInfo`, used by both Auto-mode toggling and Manual-mode single-task starting.
- Auto-naming: when no title is provided, batches are named sequentially ("Batch 1", "Batch 2", etc.).
- Click support: clicking a batch card focuses it via the `tasks_batch_cards` click map.
- Agent exit detection: on each agent list refresh, the main UI loop checks if any Active batch task agents have disappeared from the hub's agent list. For each exited agent, `mark_agent_done()` is called which marks the task as Done and returns the next Idle tasks to start (if any). This provides automatic task progression within a batch.
- `spawn_batch_tasks()` is a helper function that spawns worktree agents for each entry in a `BatchStartInfo`. It builds full prompts using the batch's prefix/suffix via `build_prompt()`, then spawns a tokio task per entry that sends `CreateWorktreeAgent` IPC to the hub. Results are sent back via the `agent_start_tx` channel. This function is used both when toggling a batch to Active and when auto-starting next tasks after agent exit.

### Auto-connect

On startup, `clust ui` automatically connects to the hub daemon, starting it if not already running. The bottom status bar shows connection status.

### Bottom Status Bar

```
● connected  q to quit  Q to quit and stop hub  ↑↓←→ navigate  Shift+←→ panels  v toggle agents          v0.0.16
```

| Section | Description |
|---------|-------------|
| Status dot | Green `●` when connected, dim when disconnected |
| Status label | `connected` or `disconnected` |
| BYPASS indicator | When bypass-permissions is enabled globally, shows `BYPASS` in a distinct color. Hidden when disabled. |
| Focused agent | When an agent has keyboard focus (in Overview terminal focus or focus mode), shows the repo name in the repo's assigned color followed by `/branch` in secondary text color |
| Status message / Shortcuts | Either a temporary status message or context-aware keybinding hints (see below) |
| Version | Right-aligned, e.g. `v0.0.16` |

**Status messages:** Temporary status messages override the keybinding hints area. Messages are displayed for 5 seconds before auto-dismissing, after which the keybinding hints reappear. Two severity levels exist: `Error` (displayed in `R_ERROR` color) and `Success` (displayed in `R_SUCCESS` color). Status messages are used to surface feedback from async operations such as agent creation, branch pulls, and remote branch checkout -- both success confirmations (e.g., "Agent started on feature-branch", "Pulled main: Already up to date.", "Checked out feature-branch") and error details (e.g., "Agent create failed: hub connect error: ...", "Pull failed: ...", "Checkout failed: ..."). The `StatusMessage` struct tracks the message text, level, and creation `Instant` for auto-dismissal timing. Status messages are delivered from background tokio tasks to the main event loop via a dedicated `mpsc` channel (`status_tx` / `status_rx`), separate from the `AgentStartResult` channel used for agent creation results.

**Keybinding hints (when no status message is active):** Context-aware hints: on Repositories tab shows `q quit`, `Q stop+quit`, navigation hints; on Overview tab shows focus-dependent hints (e.g., `Shift+↓ enter terminal` or `Shift+↑ options`); on Jobs tab shows `Opt+T new batch`, `Left/Right navigate`, `Up/Down tasks`, `Space toggle`, `Opt+S start task`, `M mode`, `B bypass`, `Enter add task`, `p prefix`, `s suffix`, `d clear done`, `Del remove`, `q quit`, `? keys`; in focus mode shows `Shift+←/→ switch panel`, `Shift+↑ exit`.

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
| `Opt+T` (macOS) / `Alt+T` | Open the create-batch modal (only when repos are registered) |
| `Opt+V` (macOS) / `Alt+V` | Open in editor (see Editor Integration below) |
| `Cmd+1` | Switch to Repositories tab (dismisses context menus, exits focus mode) |
| `Cmd+2` | Switch to Overview tab (dismisses context menus, exits focus mode) |
| `Cmd+3` | Switch to Jobs tab (dismisses context menus, exits focus mode) |

**Repositories tab:**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move selection in visual order (flat navigation across repos, categories, and branches) |
| `Shift+↑` / `Shift+↓` | Jump to previous/next repository header (skips categories and branches) |
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
│ ● connected  Shift+←/→ switch panel  ...     v0.0.16│
└─────────────────────────────────────────────────────┘
```

**Left panel:**

The left panel has a tab bar at the top with three tabs: `Changes`, `Compare`, `Terminal`. The `Changes` tab shows a unified inline diff viewer showing uncommitted changes (`git diff HEAD`). The `Compare` tab shows a branch comparison diff viewer where users can select any local branch and view the diff between it and the agent's current branch. The `Terminal` tab provides an interactive shell session running inside the agent's worktree directory, allowing users to run shell commands alongside the agent. When the agent has no `repo_path` (non-repository agent), the left panel renders a simplified state: the tab bar and diff viewer are replaced by a centered "Agent not running inside repository" message in tertiary text color on the base background. The diff refresh background task is not spawned for non-repository agents.

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

**Terminal tab:**

- Provides an interactive shell session running inside the agent's worktree directory
- On entering focus mode (`open_agent()`), a terminal session is automatically started via the hub's `StartTerminal` IPC message
- The shell is spawned by the hub as a PTY process (using `$SHELL` or `/bin/zsh` as fallback) with the agent's `working_dir` as the working directory
- Terminal output is rendered using a `TerminalEmulator` (same `vt100`-backed emulator used for agent panels), supporting full ANSI escape sequences, colors, cursor movement, and alternate screen buffer
- The terminal connection runs as a background tokio task (`terminal_connection_task`) with its own IPC streaming connection to the hub, independent of the agent panel connection
- All keyboard input is forwarded directly to the terminal shell when the Terminal tab is focused, including `Tab` (which is why `Shift+Tab` / `BackTab` is used as "previous tab" to navigate away from the Terminal tab)
- `Esc` is forwarded to the shell process (not intercepted by the UI)
- Paste events (bracketed paste) are forwarded to the terminal shell
- Scrollback is supported via `Shift+PageUp` / `Shift+PageDown` with the same scrollback mechanism as agent panels
- Mouse scroll within the left panel area scrolls the terminal scrollback when the Terminal tab is active
- On terminal resize, the terminal PTY is resized via `ResizeTerminal` IPC message. The terminal dimensions are calculated as 60% of the content area width by the content area height minus 2 rows (for the tab bar)
- `TerminalPanel` struct manages the terminal state: ID, `TerminalEmulator`, command channel, exited flag, and scroll offset
- `TerminalOutputEvent` enum carries output, exited, and connection-lost events from the background task to the UI thread via an `mpsc` channel
- `drain_terminal_events()` is called each frame in the main event loop alongside `drain_output_events()` and `drain_diff_events()`
- When the terminal session exits, the tab displays "Terminal session ended" in tertiary text
- When the terminal is starting (before the first output), the tab displays "Starting terminal..."
- On focus mode exit (`close_panel()`), the terminal session is cleaned up: a `DetachTerminal` message is sent and the background task is aborted

**Panel focus:**

The focus view has a concept of which side (left or right) has keyboard focus. The focused side is indicated by visual cues (tab bar highlight, panel border accent). `Shift+←` and `Shift+→` switch focus between the left and right panels. `Shift+↑` exits focus mode from either panel. When the right panel is focused, `Esc` is forwarded to the agent process. When the agent has no `repo_path` (non-repository agent), `Shift+←` from the right panel is blocked (the left panel cannot receive focus), and mouse clicks on the left panel area do not switch focus to the left panel. Clicking the right panel area still works normally.

**Entry points:**

- **From Overview tab:** While in terminal focus, press `Shift+↓` to open the focused agent in focus mode. The `in_focus_mode` flag is set to `true`.
- **From Repositories tab:** While the right panel is focused, press `Enter` on a selected agent to open it in focus mode. The `in_focus_mode` flag is set to `true`.

The agent's `working_dir`, `repo_path`, and `branch_name` are passed to `open_agent()` to determine the git repository for the diff viewer and to display repo/branch identity in the back-bar and status bar.

**Exit:** Press `Shift+↑` from either panel to exit focus mode and return to the originating tab. The `in_focus_mode` flag is set back to `false`. When the right panel is focused, `Esc` is forwarded to the agent process (e.g., to dismiss an agent's own UI element). If the focused agent exits while in focus mode and was running in a worktree, the cleanup dialog is shown immediately (without waiting for the user to exit focus mode). A `worktree_cleanup_shown` flag prevents the dialog from appearing again when focus mode is later exited.

**Implementation:**

- `FocusModeState` manages a single `AgentPanel` with its own IPC background task, output channel, and `TerminalEmulator`. It also manages an optional `TerminalPanel` for the Terminal tab, with its own IPC background task, output channel, and `TerminalEmulator`. It also tracks `branch_name` (in addition to `working_dir` and `repo_path`) to support worktree cleanup dialogs when exiting focus mode.
- The panel dimensions are calculated as 40% of the content area width (minus borders) by the content area height (minus header).
- `FocusSide` enum tracks which panel has keyboard focus (`Left` or `Right`).
- `LeftPanelTab` enum tracks the active tab in the left panel (`Changes`, `Compare`, `Terminal`) with `next()` and `prev()` for cycling in both directions.
- Diff state is managed via `ParsedDiff` (lines, file start indices, file names), `diff_scroll` (current scroll position), and `diff_error` (error message if `git diff` failed).
- A background diff refresh task (`spawn_diff_task`) runs every 2 seconds and sends `DiffEvent::Updated` or `DiffEvent::Error` via an `mpsc` channel. A `watch` channel signals the task to stop. The diff task is only spawned when `repo_path` is `Some` (i.e., the agent is running inside a git repository).
- `drain_diff_events()` is called each frame in the main event loop alongside `drain_output_events()`.
- `parse_unified_diff()` parses raw `git diff HEAD` output into structured `DiffLine` entries with kind (FileHeader, HunkHeader, Context, Add, Delete, FileMetadata, Separator), content, line numbers, and file index. Separator lines are automatically inserted between files during parsing.
- On terminal resize, the focus mode panel is resized via `TerminalEmulator::resize()` (preserving scrollback history) and the hub is notified via `ResizeAgent`. On `FocusGained` events, dimensions are also re-sent unconditionally to account for PTY resizes by other clients while the window was unfocused.
- Focus mode is orthogonal to tab cycling -- `Tab` / `Shift+Tab` cycles between `Repositories`, `Overview`, and `Jobs` (3 tabs). Focus mode is only entered explicitly via the entry points above.
- State is tracked by an `in_focus_mode: bool` flag rather than a `previous_tab` option. The `ActiveTab` enum no longer has a `FocusMode` variant.
- On exit (via `close_panel()`), the diff task is stopped via the watch channel and aborted, diff state is cleared, the terminal session is closed (detach message sent, background task aborted), and the panel's connection is detached.

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

**Keyboard shortcuts (focus mode, left panel focused, Terminal tab):**

| Shortcut | Action |
|----------|--------|
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+→` | Switch focus to right panel |
| `Shift+Tab` (`BackTab`) | Cycle to previous left panel tab |
| `Shift+PageUp` | Scroll terminal scrollback up by one page |
| `Shift+PageDown` | Scroll terminal scrollback down by one page |
| `Esc` | Forwarded to the terminal shell |
| `Tab` | Forwarded to the terminal shell |
| All other keys | Forwarded to the terminal shell's PTY |

### Help Overlay (`?`)

The `?` key toggles a keyboard shortcut overlay rendered as a centered modal (44 columns wide) anchored to the bottom of the content area. The modal is organized into sections with bold secondary-colored headers and context-aware visibility:

- **Global section (always shown):** `q / Esc×2`, `Q`, `Ctrl+C`, `Tab`, `Shift+Tab`, `?`, `F2`, `Alt+M`, `Alt+E`, `Alt+D`, `Alt+F`, `Alt+N`, `Alt+V`, `Alt+B`, `Alt+T`, `Cmd+1`, `Cmd+2`.
- **Repositories section (shown when Repositories tab is active):** `↑/↓` navigate, `Shift+↑/↓` jump repos, `←/→` navigate tree, `Shift+←/→` switch panel, `Enter` open menu/focus agent, `Space` collapse/expand, `v` toggle grouping.
- **Overview section (shown when Overview tab is active):** `Shift+←/→` scroll panels, `Shift+↓` enter terminal, plus an "In terminal:" sub-context label followed by `Shift+↑` back to options bar, `Shift+↓` enter focus mode, `Shift+←/→` switch agent, `PgUp/PgDn` scroll terminal.
- **Jobs section (shown when Jobs tab is active):** `←/→` navigate batches, `↑/↓` navigate tasks within a batch, `Shift+←/→` scroll batches, `Space` toggle batch status (or cancel queued), `t` set timer (queue batch), `Alt+S` start selected task (manual), `Enter` add task to batch, `p` edit prompt prefix, `s` edit prompt suffix, `d` clear done tasks, `Del/Backspace` remove batch.
- **Focus Mode section (shown when in focus mode):** `Shift+↑` exit, `Shift+←/→` switch panel, `Shift+PgUp/PgDn` scroll terminal, plus a "Left panel:" sub-context label followed by `Tab` cycle tabs, `Shift+Tab` prev tab (used in Terminal tab since Tab is forwarded to the shell), `↑/↓` scroll diff.

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

**Completion:** On completing step 4, the modal sends a `CreateWorktreeAgent` IPC message to the hub. The hub creates the worktree (via the existing `add_worktree()` logic), spawns an agent in it, and returns `WorktreeAgentStarted`. The behavior depends on the active tab: when on the **Overview tab**, the TUI stays in overview mode and selects the newly created agent's panel after the next agent sync (via `pending_overview_select` and `OverviewState::select_agent_by_id()`); when on the **Repositories tab**, the TUI opens the new agent in focus mode as before. On success, a status bar message confirms the agent started (e.g., "Agent started on feature-branch"). On failure (hub connection error, send error, unexpected response, or hub-reported error), the error is surfaced as a status bar error message instead of being lost to stderr. The `AgentStartResult` enum has `Started`, `Failed(String)`, `BatchTaskStarted`, and `BatchTaskFailed` variants to communicate the outcome from background tokio tasks to the main event loop. Batch variants include `batch_id` and `task_index` for updating task statuses. The agent start channel uses a buffer size of 16 (to handle concurrent batch task starts) and is drained in a `while let Ok(result)` loop instead of processing a single result per tick.

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

### Create Batch Modal

A multi-step modal for creating prompt batch definitions, opened globally with `Opt+T` (macOS) / `Alt+T`. The modal is only available when at least one repository is registered. The modal guides the user through up to 5 sequential steps (the concurrency step is skipped when Manual launch mode is selected):

| Step | Title | Description |
|------|-------|-------------|
| 1/5 | Select repository | Choose from registered repos. Fuzzy search filters by name and path. |
| 2/5 | Select branch | Choose a local branch from the selected repo. Fuzzy search filters by name. Shows HEAD, worktree, and active agent indicators. Skipped if the repo has no local branches. |
| 3/5 | Batch name | Enter a name for the batch. Optional -- press Enter for auto-name (e.g., "Batch 1", "Batch 2"). |
| 4/5 | Select launch mode | Choose between Auto (default) and Manual launch modes. Auto proceeds to the concurrency step; Manual skips it and completes immediately. |
| 5/5 | Max concurrent agents | Set the maximum number of concurrent agents. Use Up/Down arrows or type a digit to set. Down from 1 or 0 sets unlimited (shown as infinity symbol). Only shown for Auto launch mode. |

**Navigation (steps 1-3):**
- `Up` / `Down` -- move selection in list steps
- `Enter` -- confirm selection / advance to next step
- `Esc` -- go back to previous step, or cancel from step 1
- Type to filter -- fuzzy matching via `fuzzy-matcher` (SkimV2 algorithm)
- `Left` / `Right` -- move cursor within the input field
- `Backspace` -- delete character before cursor

**Navigation (step 4 -- launch mode):**
- `Up` / `Down` -- toggle between Auto and Manual
- `Enter` -- confirm selection. Auto advances to the concurrency step; Manual completes the modal immediately.
- `Esc` -- go back to the title step (restoring previously entered title)

**Navigation (step 5 -- concurrency):**
- `Up` / `Right` -- increase concurrency by 1
- `Down` / `Left` -- decrease concurrency by 1 (to minimum of unlimited)
- Type a digit -- set concurrency value directly (appends digit)
- `Backspace` -- remove last digit
- `Enter` -- confirm and complete the modal
- `Esc` -- go back to the launch mode step

**Completion:** On completing the final step, the modal outputs a `BatchModalOutput` containing the selected repo path, repo name, branch name, optional title, optional max concurrent value, and launch mode. The batch is added to `TasksState` and the active tab switches to the Jobs tab.

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 60% of terminal height) with a titled border, input field with visible cursor, and a scrollable list with fuzzy-matched results. The selected item is indicated with a `>` prefix and bold text. In the launch mode step, two options ("Auto" and "Manual") are displayed with descriptions, using a `>` prefix for the selected option. In the concurrency step, the input shows the current value (or infinity symbol for unlimited) with arrow hint text below.

### Add Task Modal

A 2-step modal for adding a task to an existing batch, opened by pressing `Enter` when a batch card is focused on the Jobs tab.

| Step | Title | Description |
|------|-------|-------------|
| 1/2 | Branch name | Enter the branch name for the task. Required. |
| 2/2 | Task prompt | Enter the prompt for the agent. Required. |

**Navigation:**
- `Enter` -- confirm input and advance to next step (step 1) or complete the modal (step 2)
- `Esc` -- cancel the modal from step 1, or go back to step 1 from step 2 (restoring previously entered branch name)
- `Left` / `Right` -- move cursor within the input field
- `Backspace` -- delete character before cursor

**Completion:** On completing step 2, the modal outputs an `AddTaskOutput` containing the batch index, branch name, and prompt. A `TaskEntry` is added to the corresponding batch via `TasksState::add_task()`.

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 60% of terminal height) with a titled border showing the step number and batch title. The title for step 1 shows "Step 1/2 -- Branch name (batch title)" and step 2 shows "Step 2/2 -- Task prompt (batch title)". A hint line above the input provides navigation guidance. In step 2, the previously entered branch name is shown below the input as context. The prompt input uses word-wrap with scrolling support.

### Edit Field Modal

A reusable single-field text editor modal for editing a text value. Currently used for editing batch prompt prefix (`p` key) and prompt suffix (`s` key) when a batch card is focused on the Jobs tab.

**Navigation:**
- `Enter` -- save the current value and close the modal (empty input clears the field)
- `Esc` -- cancel and close the modal without saving
- `Left` / `Right` -- move cursor within the input field
- `Backspace` -- delete character before cursor

**Completion:** On pressing Enter, the modal outputs the trimmed input string. The caller applies it via `TasksState::set_prompt_prefix()` or `TasksState::set_prompt_suffix()`. An empty string clears the field to `None`.

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 60% of terminal height) with a titled border showing the field context (e.g., "Edit Prefix -- My Batch"). A hint line above the input provides navigation guidance. The input field uses multi-line word-wrap with scrolling support. The modal is pre-filled with the current value when opened.

### Timer Modal

A single-field modal for setting a scheduled start time on a batch, opened by pressing `t` when a batch card is focused on the Jobs tab. Only available for Auto-mode Idle batches.

**Input formats:**
- **Duration**: `2h`, `30m`, `1h30m`, `45s`, or a bare number treated as minutes (e.g., `90` = 90 minutes)
- **Absolute time**: `16:00`, `9:30` (24-hour format; if the time has passed today, schedules for tomorrow)

**Navigation:**
- `Enter` -- set the timer and close the modal. Sends a `QueueBatch` IPC message to the hub with the batch definition and all its tasks. The batch status changes to `Queued` with a live countdown.
- `Esc` -- cancel and close the modal without setting a timer
- `Left` / `Right` -- move cursor within the input field
- `Backspace` -- delete character before cursor

**Live preview:** As the user types, the modal parses the input and shows a green preview line (e.g., "Starts at 16:00 (in 1h 30m)") or a hint message if the input is not yet valid.

**Rendering:** The modal is rendered as a centered overlay (60 columns wide, 8 rows tall) with a titled border showing "Set Timer -- {batch title}". A hint line explains the accepted formats. The input field shows a `>` prompt with a visible cursor. Below the input, a preview line shows the parsed result in green (`R_SUCCESS`) or an error hint in tertiary text.

**Implementation:** `timer_modal.rs` contains the `TimerModal` struct, `TimerResult` enum (`Pending`, `Cancelled`, `Completed(rfc3339)`), duration/time parsing helpers, and the `format_countdown()` function used by the batch card renderer.

### Clone Progress Modal

A centered overlay that shows real-time progress during a git clone operation. Displayed automatically when a clone is initiated from the add-repository modal.

- Each progress line from the git clone stderr output is shown as a line item with an animated braille spinner while in progress, replaced by a checkmark when complete.
- On successful completion, the repository is auto-registered and the modal shows "Press Esc to close".
- On error, the error message is displayed in `R_ERROR` color.
- All keyboard and mouse input is blocked while the clone is running, except `Esc` which is accepted after completion or error.
- The clone runs asynchronously via a background tokio task that streams `CloneProgress` IPC messages from the hub via an unbounded channel, keeping the TUI responsive throughout.
- The URL is displayed in the modal title, truncated to fit the modal width.

### Text Input and Paste Handling

All modal text inputs (Create Agent, Create Batch, Add Task, Edit Field, Timer, Search Agent, Detached Agent, Add Repository, and the Branch Picker in focus mode) track cursor position as a **byte offset** into the UTF-8 `String`, not a character index. This ensures correct behavior when the input contains multi-byte UTF-8 characters (e.g., em-dash, en-dash, accented characters):

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
| Repositories tab (branch selected) | Worktree directory (if worktree), otherwise repo root |
| Overview tab (terminal focused) | Agent's working directory |
| Jobs tab | No target (editor shortcut is a no-op) |

**Flow:**

1. If the repository has a saved editor preference (per-repo `editor` column or global `default_editor`), the editor opens immediately without showing any modal.
2. If multiple editors are detected, an **editor picker modal** is shown listing all detected editors by name. The user selects one.
3. If only one editor is detected, it opens immediately.
4. After opening (when the target is inside a repository), an **editor remember modal** asks "Remember this editor?" with three options:
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
- `left_panel_area` / `right_panel_area` -- full panel areas for Repositories tab focus switching
- `tree_items` / `tree_inner_area` -- repo tree line targets mapped via `TreeClickTarget` enum (Repo, Category, Branch)
- `agent_cards` -- right panel agent card regions mapped to (group_idx, agent_idx) pairs
- `mode_label_area` -- right panel mode label region (the "by repo / by hub" line) for click-to-toggle view mode
- `overview_panels` -- Overview tab panel regions mapped to global panel indices
- `overview_repo_buttons` -- Overview tab repo group block regions mapped to repo path strings (click to toggle collapse/expand)
- `overview_agent_indicators` -- Overview tab agent indicator regions within repo group blocks mapped to global panel indices (click to focus agent)
- `focus_left_area` / `focus_right_area` -- Focus mode panel areas for focus switching
- `focus_left_tabs` -- Focus mode left panel tab regions mapped to `LeftPanelTab` values
- `overview_content_areas` -- Overview tab terminal content areas (inner area excluding borders/header) mapped to global panel indices, used for Cmd+click URL detection
- `focus_right_content_area` -- Focus mode right panel terminal content area (inner area excluding borders/header), used for Cmd+click URL detection
- `tasks_batch_cards` -- Jobs tab batch card regions mapped to batch indices (click to focus batch card)

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
| Repo group block | Toggle collapse/expand for that repo (collapsed repos hide their agent indicators and filter their panels out of the viewport) |
| Agent indicator (within repo block) | Focus that agent's terminal panel (`OverviewFocus::Terminal(idx)`) |
| Agent panel | Focus that terminal panel (`OverviewFocus::Terminal(idx)`) |

**Jobs tab:**

| Click Target | Action |
|--------------|--------|
| Batch card | Focus that batch card (`TasksFocus::BatchCard(idx)`) |

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

**Focus mode:**

| Cursor Position | Scroll Action |
|-----------------|---------------|
| Over the right panel | Scroll the agent terminal scrollback up/down |
| Over the left panel | Scroll the diff viewer up/down |

The Repositories tab does not have scroll wheel handling.
