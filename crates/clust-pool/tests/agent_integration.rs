use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::Mutex;

// Integration tests for the agent module.
// These tests spawn real PTY processes to verify the full lifecycle.

/// Helper to create shared pool state for tests.
fn new_shared_state() -> clust_pool::agent::SharedPoolState {
    Arc::new(Mutex::new(clust_pool::agent::PoolState::new()))
}

#[tokio::test]
async fn spawn_agent_echo_produces_output() {
    let state = new_shared_state();
    let (id, binary) = {
        let mut pool = state.lock().await;
        clust_pool::agent::spawn_agent(
            &mut pool,
            None,
            Some("echo".into()),
            "/tmp".into(),
            80,
            24,
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    assert_eq!(id.len(), 6);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(binary, "echo");

    // Subscribe to output and wait for data
    let mut rx = {
        let pool = state.lock().await;
        let entry = pool.agents.get(&id).expect("agent should be in state");
        entry.output_tx.subscribe()
    };

    // echo with no args just outputs a newline then exits
    let mut got_output = false;
    let mut got_exit = false;

    let timeout = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(clust_pool::agent::AgentEvent::Output(data)) => {
                    assert!(!data.is_empty());
                    got_output = true;
                }
                Ok(clust_pool::agent::AgentEvent::Exited(code)) => {
                    assert_eq!(code, 0);
                    got_exit = true;
                    break;
                }
                Ok(clust_pool::agent::AgentEvent::PoolShutdown) => break,
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
    let pool = state.lock().await;
    assert!(
        !pool.agents.contains_key(&id),
        "agent should be removed from state after exit"
    );
}

#[tokio::test]
async fn spawn_agent_cat_receives_input_and_echoes() {
    let state = new_shared_state();
    let (id, _) = {
        let mut pool = state.lock().await;
        clust_pool::agent::spawn_agent(
            &mut pool,
            None,
            Some("cat".into()),
            "/tmp".into(),
            80,
            24,
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Subscribe to output
    let mut rx = {
        let pool = state.lock().await;
        let entry = pool.agents.get(&id).unwrap();
        entry.output_tx.subscribe()
    };

    // Write input to the agent
    {
        let mut pool = state.lock().await;
        let entry = pool.agents.get_mut(&id).unwrap();
        use std::io::Write;
        entry.pty_writer.write_all(b"hello\n").unwrap();
    }

    // Read output — cat should echo back "hello\n"
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut collected = Vec::new();
        loop {
            match rx.recv().await {
                Ok(clust_pool::agent::AgentEvent::Output(data)) => {
                    collected.extend_from_slice(&data);
                    let s = String::from_utf8_lossy(&collected);
                    if s.contains("hello") {
                        return collected;
                    }
                }
                Ok(clust_pool::agent::AgentEvent::Exited(_)) => {
                    return collected;
                }
                Ok(clust_pool::agent::AgentEvent::PoolShutdown) => return collected,
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
        let mut pool = state.lock().await;
        if let Some(entry) = pool.agents.get_mut(&id) {
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
        let mut pool = state.lock().await;
        clust_pool::agent::spawn_agent(
            &mut pool,
            Some("hello from test".into()),
            Some("echo".into()),
            "/tmp".into(),
            80,
            24,
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Create two subscribers
    let (mut rx1, mut rx2) = {
        let pool = state.lock().await;
        let entry = pool.agents.get(&id).unwrap();
        (entry.output_tx.subscribe(), entry.output_tx.subscribe())
    };

    // Both should receive the same output
    async fn collect(
        rx: &mut tokio::sync::broadcast::Receiver<clust_pool::agent::AgentEvent>,
    ) -> Vec<u8> {
        let mut collected = Vec::new();
        loop {
            match rx.recv().await {
                Ok(clust_pool::agent::AgentEvent::Output(data)) => {
                    collected.extend_from_slice(&data);
                }
                Ok(clust_pool::agent::AgentEvent::Exited(_)) => break,
                Ok(clust_pool::agent::AgentEvent::PoolShutdown) => break,
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
        let mut pool = state.lock().await;
        clust_pool::agent::spawn_agent(
            &mut pool,
            None,
            Some("cat".into()),
            "/tmp".into(),
            80,
            24,
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    {
        let pool = state.lock().await;
        let entry = pool.agents.get(&id).unwrap();
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
        let mut pool = state.lock().await;
        if let Some(entry) = pool.agents.get_mut(&id) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}

#[tokio::test]
async fn resize_agent_pty() {
    let state = new_shared_state();
    let (id, _) = {
        let mut pool = state.lock().await;
        clust_pool::agent::spawn_agent(
            &mut pool,
            None,
            Some("cat".into()),
            "/tmp".into(),
            80,
            24,
            state.clone(),
        )
        .expect("spawn_agent should succeed")
    };

    // Resize should not error
    {
        let pool = state.lock().await;
        let entry = pool.agents.get(&id).unwrap();
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
        let mut pool = state.lock().await;
        if let Some(entry) = pool.agents.get_mut(&id) {
            use std::io::Write;
            entry.pty_writer.write_all(&[0x04]).unwrap();
        }
    }
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
}
