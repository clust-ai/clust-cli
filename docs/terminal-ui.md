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
- Use `ratatui` for the status bar rendering, agent output is passed through directly

### Input Handling

- All keyboard input is forwarded to the agent PTY, **except** recognized clust shortcuts
- Shortcut detection happens first; unmatched input passes through

### Attach Flow

1. CLI sends `AttachAgent { id }` to pool
2. Pool starts streaming `AgentOutput` messages
3. CLI enters raw mode, draws status bar
4. CLI streams output to terminal, forwards input to pool

### Detach Flow

1. User presses `Ctrl+Q`
2. CLI sends `DetachAgent { id }` to pool
3. CLI exits raw mode, restores terminal
4. CLI exits cleanly (agent continues in pool)

### Background Mode (`-b`)

No terminal takeover. The CLI:

1. Sends `StartAgent` to pool
2. Receives `AgentStarted { id }`
3. Prints the ID to stdout: `Started agent a3f8c1`
4. Exits immediately

## Future: `clust ui`

A full terminal UI (TUI) built with `ratatui` that will provide:

- Agent list view
- Live output from selected agent
- Agent management (stop, attach, detach)
- Split views for multiple agents

This is not in scope for v0.1 but the choice of `ratatui` + `crossterm` for the status bar ensures we are building on the same foundation.
