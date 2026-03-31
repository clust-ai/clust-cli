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
| `Focus` | Single-agent focus view with a 60%-width left panel (tabbed, with diff viewer) and 40%-width terminal panel on the right |

The active tab is highlighted with the accent color. A `Tab/Shift+Tab` hint is shown to the right of the tabs.

#### Content Panels (Repositories tab)

- **Left panel (40%):** Repository tracker with `(2,2,1,0)` padding. Shows a tree view of registered git repositories with their local and remote branches. Repository names are rendered in Bold using their original case. Tree items have no blank spacer lines between them for a compact layout. Tree connectors use `├──` / `└──` for clear hierarchy. Branch names are rendered Bold. Remote branches are collapsed by default. Branches with active agents display a green `●` indicator with count; branches checked out in worktrees display a `⎇` indicator. The current HEAD branch is highlighted. Displays "No repositories found" when no repos are registered. Uses background colors for visual separation (no borders). The focused panel shows a bright accent dot; the unfocused panel shows a dim dot. Agents not associated with any git repository are grouped under a synthetic "No Repository" entry at the bottom of the tree. This entry has no local/remote category level -- agents are listed directly under the repo node with their binary name and working directory. Navigation skips the category level for this group.
- **Vertical divider (1 col):** A single-column divider between the two panels.
- **Right panel (60%):** Shows agent cards grouped by repository (default) or by hub name, with section headers and `(2,2,1,0)` padding. 1-line gaps separate agent cards within a group and a 1-line spacer follows each group header. The mode label line shows the current grouping (e.g., "by repo") with a "v to switch" hint. When only a single default group exists (only "default_hub" in by-hub mode, or only "No repository" in by-repo mode), the group header is hidden for a cleaner look. In by-repo mode, agents without a linked repository display their working directory on the agent card. Displays the CLUST logo when no agents are running.

Agent cards show: ID, binary name, status, start time, and attached terminal count. In by-repo mode, agents without a repository also show their working directory.

Repositories are registered via `clust repo -R` or auto-registered when an agent is launched inside a git repo. Branch data is fetched from the local git state every 2 seconds (no network calls or authentication required).

#### Overview Tab

A multi-agent terminal overview that displays all active agents side-by-side with live terminal output. Each agent gets its own panel with a full VTE terminal emulator.

```
┌─────────────────────────────────────────────────────┐
│ [options bar]                                       │
├──────────────────────┬──────────────────┬───────────┤
│┌────────────────────┐│┌────────────────┐│┌─────────┐│
││ a3f8c1 · claude ●  │││ b7e2d9 · claude│││ c4a1e0 ·││
││                    │││                │││         ││
││ Agent PTY output   │││ Agent PTY out  │││ (partial││
││ (VTE emulated)     │││ (VTE emulated) │││  view)  ││
││                    │││                │││         ││
│└────────────────────┘│└────────────────┘│└─────────┘│
├──────────────────────┴──────────────────┴───────────┤
│ ● connected  Shift+↓ enter terminal  ...    v0.0.8 │
└─────────────────────────────────────────────────────┘
```

**Layout:**

- **Options bar (1 row):** Top row, reserved for future filter buttons. Background changes based on focus.
- **Agent panels (horizontal):** Fixed-width columns sized so exactly 2.5 panels fit across the screen (showing 2 full panels + half of a third), with a minimum width of 40 columns. When more agents exist than fit on screen, horizontal scrolling is enabled with `◀ N` / `N ▶` indicators.
- Each panel has **box-drawing borders** (top, bottom, left, right). The border color is accent blue when the panel is focused, and subtle gray when unfocused.
- Inside the border, a **header row** shows agent ID (accent-colored), separator, agent binary name, and status indicator (`●` green for running, `[exited]` red for exited).
- The **terminal area** below the header renders the agent's PTY output using a VTE-based virtual terminal emulator with full ANSI support (cursor movement, SGR colors/styles, erase operations, scroll regions, line wrapping). The VTE terminal gets the inner width (total panel width minus 2 border columns).

**Focus modes:**

| Focus | Description |
|-------|-------------|
| Options Bar | Default. Navigation keys scroll viewport or enter terminal. |
| Terminal(N) | All keyboard input is forwarded directly to the focused agent, except Shift+arrow keys. Focused panel has accent-blue borders; unfocused panels have subtle gray borders. |

**Keyboard shortcuts (Overview tab):**

| Context | Shortcut | Action |
|---------|----------|--------|
| Options Bar | `Shift+↓` | Enter terminal focus (returns to last focused panel) |
| Options Bar | `Shift+←` / `Shift+→` | Scroll viewport left/right |
| Terminal | `Shift+↑` | Return to options bar |
| Terminal | `Shift+←` / `Shift+→` | Switch focus to previous/next agent panel (wraps around) |
| Terminal | `PageUp` / `PageDown` | Scroll focused panel through scrollback history |
| Terminal | Any other key | Forwarded to the focused agent's PTY |

**Implementation:**

- Each agent panel runs a **background tokio task** that maintains its own IPC streaming connection to the hub (attach, receive output, forward input).
- Output events are sent to the UI thread via an `mpsc` channel and drained each frame.
- `VirtualTerminal` wraps a `vte` parser (`vte = 0.13`) and a `Screen` grid that implements `vte::Perform` for full ANSI escape sequence handling. The `Screen` maintains a scrollback ring buffer (default 2,000 lines, configurable via `with_scrollback_capacity()`) of rows that have scrolled off the top of the grid. Scrollback is populated both by natural line scrolling and by `erase_in_display()`, which saves non-blank screen content to scrollback before clearing (for ED modes 2/3, and mode 0 when the cursor is at the home position). This ensures that TUI-style agents that clear the screen with `\x1b[2J` or `\x1b[H\x1b[J` before each redraw accumulate scrollback history. The VirtualTerminal is also used as a shadow terminal in the attached session for scrollback (with 5,000-line capacity), where `resize_keep_scrollback()` preserves scrollback on window resize and `to_ansi_lines_scrolled()` renders scrollback content as ANSI-escaped strings for direct stdout output.
- `key_event_to_bytes()` converts `crossterm::KeyEvent` to raw terminal byte sequences for agent input forwarding.
- Lazy initialization: overview connections are only established on first switch to the Overview tab.
- On connect, each panel's background task consumes the hub's replay buffer before entering the main output loop, so panels show recent history immediately.
- On terminal resize, all panels are resized via `resize_keep_scrollback()` (which preserves accumulated scrollback history) and the hub is notified via `ResizeAgent`. The resize command is sent before clearing the local VTE grid so that a failed send does not leave the grid empty. The VTE grid is cleared on resize (the agent sends a full redraw after receiving SIGWINCH); same-size resizes are skipped as a no-op to preserve content. Resize also resets `current_style` and `wrap_pending` to prevent style bleed. The viewport is scrolled automatically to keep the focused panel visible.
- Each panel has a `panel_scroll_offset` for scrolling through the combined scrollback + live grid. When scrolled, a `↑N` indicator appears in the panel header.
- On exit, all connections are detached and background tasks are aborted.

### Auto-connect

On startup, `clust ui` automatically connects to the hub daemon, starting it if not already running. The bottom status bar shows connection status.

### Bottom Status Bar

```
● connected  q to quit  Q to quit and stop hub  ↑↓←→ navigate  Shift+←→ panels  v toggle agents          v0.0.8
```

| Section | Description |
|---------|-------------|
| Status dot | Green `●` when connected, dim when disconnected |
| Status label | `connected` or `disconnected` |
| Shortcuts | Context-aware hints: on Repositories tab shows `q quit`, `Q stop+quit`, navigation hints; on Overview tab shows focus-dependent hints (e.g., `Shift+↓ enter terminal` or `Shift+↑ options`); on Focus tab shows `Shift+←/→ switch panel`, `Shift+↑/↓ jump file`, `Esc exit` |
| Version | Right-aligned, e.g. `v0.0.8` |

### Keyboard Shortcuts

**Global (all tabs, unless overridden):**

| Shortcut | Action |
|----------|--------|
| `q` / `Esc` | Quit the UI (hub keeps running) |
| `Q` | Quit the UI and stop the hub |
| `Tab` | Switch to next tab |
| `Shift+Tab` | Switch to previous tab |
| `?` | Toggle keyboard shortcut overlay |

**Repositories tab:**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move selection within current level |
| `→` | Descend into selected item, or expand if collapsed |
| `←` | Collapse current item, or ascend to parent level |
| `Enter` | Toggle collapse/expand (left panel); enter focus mode for selected agent (right panel) |
| `Shift+←` / `Shift+→` | Switch focus between left and right panels |
| `v` | Toggle agent grouping between by-hub and by-repo (right panel) |

**Overview tab (Options Bar focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↓` | Enter terminal focus |
| `Shift+←` / `Shift+→` | Scroll viewport left/right |

**Overview tab (Terminal focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↑` | Exit terminal, return to options bar |
| `Shift+↓` | Enter focus mode for the focused agent |
| `Shift+←` / `Shift+→` | Switch to previous/next agent panel |
| `PageUp` / `PageDown` | Scroll focused panel through scrollback history |
| `Shift+PageUp` / `Shift+PageDown` | Scroll focused panel through scrollback history (same as above) |
| All other keys | Forwarded to the focused agent's PTY |

#### Focus Tab

A single-agent focus view with a two-panel split: a 60%-width left panel with tabbed content (including a git diff viewer) and a 40%-width right panel displaying the agent's terminal.

```
┌─────────────────────────────────────────────────────┐
│ Changes │ Panel 2 │ Panel 3 │┌────────────────────┐│
│                               ││ a3f8c1 · claude ●  ││
│      1      1│fn main() {     ││                    ││
│      2       │-  old_code();  ││ Agent PTY output   ││
│         2│+  new_code();  ││ (VTE emulated)     ││
│      3      3│  let x = 1;   ││                    ││
│                               │└────────────────────┘│
├─────────────────────────────────────────────────────┤
│ ● connected  Shift+←/→ switch panel  ...     v0.0.8│
└─────────────────────────────────────────────────────┘
```

**Left panel:**

The left panel has a tab bar at the top with three tabs: `Changes`, `Panel 2`, `Panel 3`. The `Changes` tab shows a unified inline diff viewer. `Panel 2` and `Panel 3` are placeholders for future content.

**Diff viewer (Changes tab):**

- Displays the output of `git diff HEAD` for the agent's working directory
- Unified inline format with dual-column line numbers (old and new)
- Line-by-line color coding: additions use a green-tinted background (`R_DIFF_ADD_BG`), deletions use a red-tinted background (`R_DIFF_DEL_BG`), file headers use `R_BG_RAISED`, hunk headers use the accent color, context lines use the base background
- A gutter column (9 chars wide) shows old/new line numbers separated by a `│` divider; file headers and hunk headers suppress line numbers
- The diff is refreshed every 2 seconds via a background tokio task that runs `git diff HEAD` in a `spawn_blocking` call
- Scrolling is supported with `↑` / `↓` keys when the left panel is focused
- File jumping with `Shift+↑` / `Shift+↓` navigates directly to the previous/next file header
- Empty state shows "No uncommitted changes"; loading state shows "Loading diff..."

**Panel focus:**

The focus view has a concept of which side (left or right) has keyboard focus. The focused side is indicated by visual cues (tab bar highlight, panel border accent). `Shift+←` and `Shift+→` switch focus between the left and right panels. `Esc` from the left panel returns focus to the right panel; `Esc` from the right panel exits focus mode.

**Entry points:**

- **From Overview tab:** While in terminal focus, press `Shift+↓` to open the focused agent in focus mode.
- **From Repositories tab:** While the right panel is focused, press `Enter` on a selected agent to open it in focus mode.

The agent's `working_dir` is passed to `open_agent()` to determine the git repository for the diff viewer.

**Exit:** Press `Esc` (when right panel is focused) or `Shift+↑` (when right panel is focused) to return to the previous tab.

**Implementation:**

- `FocusModeState` manages a single `AgentPanel` with its own IPC background task, output channel, and VTE terminal emulator.
- The panel dimensions are calculated as 40% of the content area width (minus borders) by the content area height (minus header).
- `FocusSide` enum tracks which panel has keyboard focus (`Left` or `Right`).
- `LeftPanelTab` enum tracks the active tab in the left panel (`Changes`, `Panel2`, `Panel3`) with `next()` for cycling.
- Diff state is managed via `ParsedDiff` (lines, file start indices, file names), `diff_scroll` (current scroll position), and `diff_error` (error message if `git diff` failed).
- A background diff refresh task (`spawn_diff_task`) runs every 2 seconds and sends `DiffEvent::Updated` or `DiffEvent::Error` via an `mpsc` channel. A `watch` channel signals the task to stop.
- `drain_diff_events()` is called each frame in the main event loop alongside `drain_output_events()`.
- `parse_unified_diff()` parses raw `git diff HEAD` output into structured `DiffLine` entries with kind, content, line numbers, and file index.
- On terminal resize, the focus mode panel is resized via `resize_keep_scrollback()` (preserving scrollback history) and the hub is notified via `ResizeAgent`.
- Tab cycling (`Tab` / `Shift+Tab`) skips the Focus tab; it is only entered explicitly via the entry points above.
- On exit (via `close_panel()`), the diff task is stopped via the watch channel and aborted, diff state is cleared, and the panel's connection is detached.

**Keyboard shortcuts (Focus tab, right panel focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` | Exit focus mode, return to previous tab |
| `Shift+↑` | Exit focus mode, return to previous tab |
| `Shift+←` | Switch focus to left panel |
| `Shift+PageUp` | Scroll up through scrollback history |
| `Shift+PageDown` | Scroll down through scrollback history |
| All other keys | Forwarded to the focused agent's PTY |

**Keyboard shortcuts (Focus tab, left panel focused):**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Scroll diff up/down |
| `Shift+↑` | Jump to previous file header |
| `Shift+↓` | Jump to next file header |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab |
| `Esc` | Switch focus to right panel |
