use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::Mutex;

// Integration tests for the agent module.
// These tests spawn real PTY processes to verify the full lifecycle.

/// Helper to create shared hub state for tests.
fn new_shared_state() -> clust_hub::agent::SharedHubState {
    Arc::new(Mutex::new(clust_hub::agent::HubState::new()))
}

#[tokio::test]
async fn spawn_agent_echo_produces_output() {
    let state = new_shared_state();
    let (id, binary) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("echo".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    assert_eq!(id.len(), 6);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(binary, "echo");

    // Subscribe to output and wait for data
    let mut rx = {
        let hub = state.lock().await;
        let entry = hub.agents.get(&id).expect("agent should be in state");
        entry.output_tx.subscribe()
    };

    // echo with no args just outputs a newline then exits
    let mut got_output = false;
    let mut got_exit = false;

    let timeout = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(clust_hub::agent::AgentEvent::Output(data)) => {
                    assert!(!data.is_empty());
                    got_output = true;
                }
                Ok(clust_hub::agent::AgentEvent::Exited(code)) => {
                    assert_eq!(code, 0);
                    got_exit = true;
                    break;
                }
                Ok(clust_hub::agent::AgentEvent::HubShutdown) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .await;

    assert!(timeout.is_ok(), "timed out waiting for agent events");
    assert!(got_output, "should have received output");
    assert!(got_exit, "should have received exit event");

    // Agent should be cleaned up from state after exit
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let hub = state.lock().await;
    assert!(
        !hub.agents.contains_key(&id),
        "agent should be removed from state after exit"
    );
}

#[tokio::test]
async fn spawn_agent_cat_receives_input_and_echoes() {
    let state = new_shared_state();
    let (id, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Subscribe to output
    let mut rx = {
        let hub = state.lock().await;
        let entry = hub.agents.get(&id).unwrap();
        entry.output_tx.subscribe()
    };

    // Write input to the agent
    {
        let mut hub = state.lock().await;
        let entry = hub.agents.get_mut(&id).unwrap();
        use std::io::Write;
        entry.pty_writer.write_all(b"hello\n").unwrap();
    }

    // Read output — cat should echo back "hello\n"
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            match rx.recv().await {
                Ok(clust_hub::agent::AgentEvent::Output(data)) => {
                    collected.extend_from_slice(&data);
                    let s = String::from_utf8_lossy(&collected);
                    if s.contains("hello") {
                        return collected;
                    }
                }
                Ok(clust_hub::agent::AgentEvent::Exited(_)) => {
                    return collected;
                }
                Ok(clust_hub::agent::AgentEvent::HubShutdown) => return collected,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return collected,
            }
        }
    })
    .await;

    assert!(timeout.is_ok(), "timed out waiting for echo");
    let data = timeout.unwrap();
    let output = String::from_utf8_lossy(&data);
    assert!(output.contains("hello"), "output should contain 'hello', got: {output}");

    // Send EOF to cat (Ctrl+D) to make it exit
    {
        let mut hub = state.lock().await;
        if let Some(entry) = hub.agents.get_mut(&id) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }

    // Wait for exit
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn multiple_subscribers_receive_same_output() {
    let state = new_shared_state();
    let (id, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Subscribe BEFORE writing input to avoid race with fast-exiting commands
    let (mut rx1, mut rx2) = {
        let hub = state.lock().await;
        let entry = hub.agents.get(&id).unwrap();
        (entry.output_tx.subscribe(), entry.output_tx.subscribe())
    };

    // Now send input; cat will echo it back to both subscribers
    {
        let mut hub = state.lock().await;
        if let Some(entry) = hub.agents.get_mut(&id) {
            use std::io::Write;
            entry
                .pty_writer
                .write_all(b"hello from test\n")
                .unwrap();
            // Send EOF so cat exits
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }

    // Both should receive the same output
    async fn collect(
        rx: &mut tokio::sync::broadcast::Receiver<clust_hub::agent::AgentEvent>,
    ) -> Vec<u8> {
        let mut collected = Vec::new();
        loop {
            match rx.recv().await {
                Ok(clust_hub::agent::AgentEvent::Output(data)) => {
                    collected.extend_from_slice(&data);
                }
                Ok(clust_hub::agent::AgentEvent::Exited(_)) => break,
                Ok(clust_hub::agent::AgentEvent::HubShutdown) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        collected
    }

    let timeout = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (out1, out2) = tokio::join!(collect(&mut rx1), collect(&mut rx2));
        (out1, out2)
    })
    .await;

    assert!(timeout.is_ok(), "timed out");
    let (out1, out2) = timeout.unwrap();

    let s1 = String::from_utf8_lossy(&out1);
    let s2 = String::from_utf8_lossy(&out2);
    assert!(
        s1.contains("hello from test"),
        "subscriber 1 should see output, got: {s1}"
    );
    assert!(
        s2.contains("hello from test"),
        "subscriber 2 should see output, got: {s2}"
    );
}

#[tokio::test]
async fn attached_count_tracks_subscribers() {
    let state = new_shared_state();
    let (id, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    {
        let hub = state.lock().await;
        let entry = hub.agents.get(&id).unwrap();
        assert_eq!(entry.attached_count.load(Ordering::Relaxed), 0);

        // Simulate attaching
        entry.attached_count.fetch_add(1, Ordering::Relaxed);
        entry.attached_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(entry.attached_count.load(Ordering::Relaxed), 2);

        // Simulate detaching
        entry.attached_count.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(entry.attached_count.load(Ordering::Relaxed), 1);
    }

    // Clean up: send EOF to cat
    {
        let mut hub = state.lock().await;
        if let Some(entry) = hub.agents.get_mut(&id) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn stop_agent_terminates_running_process() {
    let state = new_shared_state();
    let (id, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("sleep".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Verify agent exists
    {
        let hub = state.lock().await;
        assert!(hub.agents.contains_key(&id));
    }

    // Stop the agent
    let result = clust_hub::agent::stop_agent(&state, &id).await;
    assert!(result.is_ok(), "stop_agent should succeed");

    // The PTY reader task should clean up the agent from state after process exits
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let hub = state.lock().await;
    assert!(
        !hub.agents.contains_key(&id),
        "agent should be removed from state after stop"
    );
}

#[tokio::test]
async fn set_and_get_default_agent_via_hub_state() {
    let mut state = clust_hub::agent::HubState::new();

    // Fresh state has no default
    assert_eq!(state.default_agent, None);

    // Initialize with in-memory DB (inline schema to avoid needing private run_migrations)
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO schema_version (version) VALUES (1);",
    )
    .unwrap();
    state.db = Some(conn);

    // Set default via DB
    clust_hub::db::set_default_agent(state.db.as_ref().unwrap(), "claude").unwrap();
    state.default_agent = clust_hub::db::get_default_agent(state.db.as_ref().unwrap());
    assert_eq!(state.default_agent, Some("claude".to_string()));

    // Overwrite
    clust_hub::db::set_default_agent(state.db.as_ref().unwrap(), "aider").unwrap();
    state.default_agent = clust_hub::db::get_default_agent(state.db.as_ref().unwrap());
    assert_eq!(state.default_agent, Some("aider".to_string()));
}

#[tokio::test]
async fn resize_agent_pty() {
    let state = new_shared_state();
    let (id, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Resize should not error
    {
        let hub = state.lock().await;
        let entry = hub.agents.get(&id).unwrap();
        let result = entry.pty_master.resize(portable_pty::PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        });
        assert!(result.is_ok(), "resize should succeed");
    }

    // Clean up
    {
        let mut hub = state.lock().await;
        if let Some(entry) = hub.agents.get_mut(&id) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn spawn_agent_stores_custom_hub_name() {
    let state = new_shared_state();
    let (id, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: "my_feature".into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Verify the hub name is stored on the entry
    {
        let hub = state.lock().await;
        let entry = hub.agents.get(&id).expect("agent should exist");
        assert_eq!(entry.hub, "my_feature");
    }

    // Clean up
    {
        let mut hub = state.lock().await;
        if let Some(entry) = hub.agents.get_mut(&id) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn agents_in_different_hubs_are_separated() {
    let state = new_shared_state();

    // Spawn two agents in different hubs
    let (id_a, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: clust_ipc::DEFAULT_HUB.into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn default_hub agent")
    };

    let (id_b, _) = {
        let mut hub = state.lock().await;
        clust_hub::agent::spawn_agent(
            &mut hub,
            clust_hub::agent::SpawnAgentParams {
                prompt: None,
                agent_binary: Some("cat".into()),
                working_dir: "/tmp".into(),
                cols: 80,
                rows: 24,
                accept_edits: false,
                hub: "my_feature".into(),
                repo_path: None,
                branch_name: None,
                is_worktree: false,
            },
            state.clone(),
        )
        .expect("spawn my_feature agent")
    };

    // Verify both agents exist with correct hubs
    {
        let hub = state.lock().await;
        assert_eq!(hub.agents.len(), 2);
        assert_eq!(hub.agents.get(&id_a).unwrap().hub, clust_ipc::DEFAULT_HUB);
        assert_eq!(hub.agents.get(&id_b).unwrap().hub, "my_feature");

        // Simulate ListAgents filter: no filter returns all
        let all: Vec<_> = hub.agents.values().collect();
        assert_eq!(all.len(), 2);

        // Filter by default_hub returns only id_a
        let default_only: Vec<_> = hub
            .agents
            .values()
            .filter(|e| e.hub == clust_ipc::DEFAULT_HUB)
            .collect();
        assert_eq!(default_only.len(), 1);
        assert_eq!(default_only[0].id, id_a);

        // Filter by my_feature returns only id_b
        let feature_only: Vec<_> = hub
            .agents
            .values()
            .filter(|e| e.hub == "my_feature")
            .collect();
        assert_eq!(feature_only.len(), 1);
        assert_eq!(feature_only[0].id, id_b);

        // Filter by nonexistent hub returns empty
        let none: Vec<_> = hub
            .agents
            .values()
            .filter(|e| e.hub == "nonexistent")
            .collect();
        assert!(none.is_empty());
    }

    // Clean up both agents
    for id in [&id_a, &id_b] {
        let mut hub = state.lock().await;
        if let Some(entry) = hub.agents.get_mut(id as &str) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}
