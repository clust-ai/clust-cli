use tempfile::tempdir;
use tokio::net::{UnixListener, UnixStream};

use clust_ipc::{
    recv_message, recv_message_read, send_message, send_message_write, AgentInfo, CliMessage,
    HubMessage,
};

#[tokio::test]
async fn request_response_over_real_socket() {
    let dir = tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");

    let listener = UnixListener::bind(&sock_path).unwrap();

    // Client sends ListAgents, expects AgentList response
    let client = tokio::spawn({
        let sock_path = sock_path.clone();
        async move {
            let mut stream = UnixStream::connect(&sock_path).await.unwrap();
            send_message(&mut stream, &CliMessage::ListAgents { hub: None })
                .await
                .unwrap();
            let resp: HubMessage = recv_message(&mut stream).await.unwrap();
            resp
        }
    });

    // Server receives the message and responds
    let (mut server_stream, _) = listener.accept().await.unwrap();
    let msg: CliMessage = recv_message(&mut server_stream).await.unwrap();
    assert_eq!(msg, CliMessage::ListAgents { hub: None });

    let response = HubMessage::AgentList {
        agents: vec![AgentInfo {
            id: "abc123".into(),
            agent_binary: "claude".into(),
            started_at: "2026-03-25T10:00:00Z".into(),
            attached_clients: 1,
            hub: clust_ipc::DEFAULT_HUB.into(),
            working_dir: "/tmp".into(),
            repo_path: None,
            branch_name: None,
        }],
    };
    send_message(&mut server_stream, &response).await.unwrap();

    let client_resp = client.await.unwrap();
    assert_eq!(client_resp, response);
}

#[tokio::test]
async fn multiple_clients_sequential() {
    let dir = tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");

    let listener = UnixListener::bind(&sock_path).unwrap();

    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let msg: CliMessage = recv_message(&mut stream).await.unwrap();
            assert_eq!(msg, CliMessage::StopHub);
            send_message(&mut stream, &HubMessage::Ok).await.unwrap();
        }
    });

    for _ in 0..3 {
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        send_message(&mut stream, &CliMessage::StopHub)
            .await
            .unwrap();
        let resp: HubMessage = recv_message(&mut stream).await.unwrap();
        assert_eq!(resp, HubMessage::Ok);
    }

    server.await.unwrap();
}

#[tokio::test]
async fn bidirectional_streaming_over_split_socket() {
    let dir = tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");

    let listener = UnixListener::bind(&sock_path).unwrap();

    // Server: accept connection, split, read/write concurrently
    let server = tokio::spawn({
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();

            // Respond to StartAgent with AgentStarted
            let msg: CliMessage = recv_message_read(&mut reader).await.unwrap();
            assert!(matches!(msg, CliMessage::StartAgent { .. }));
            send_message_write(
                &mut writer,
                &HubMessage::AgentStarted {
                    id: "abc123".into(),
                    agent_binary: "claude".into(),
                },
            )
            .await
            .unwrap();

            // Send some output
            send_message_write(
                &mut writer,
                &HubMessage::AgentOutput {
                    id: "abc123".into(),
                    data: b"hello world".to_vec(),
                },
            )
            .await
            .unwrap();

            // Read input from client
            let input: CliMessage = recv_message_read(&mut reader).await.unwrap();
            assert!(matches!(input, CliMessage::AgentInput { .. }));

            // Read resize from client
            let resize: CliMessage = recv_message_read(&mut reader).await.unwrap();
            assert!(matches!(resize, CliMessage::ResizeAgent { .. }));

            // Read detach
            let detach: CliMessage = recv_message_read(&mut reader).await.unwrap();
            assert!(matches!(detach, CliMessage::DetachAgent { .. }));
        }
    });

    // Client: connect, split, send StartAgent, then stream messages
    let mut stream = UnixStream::connect(&sock_path).await.unwrap();
    send_message(
        &mut stream,
        &CliMessage::StartAgent {
            prompt: None,
            agent_binary: None,
            working_dir: "/tmp".into(),
            cols: 80,
            rows: 24,
            accept_edits: false,
            hub: clust_ipc::DEFAULT_HUB.into(),
        },
    )
    .await
    .unwrap();

    let response: HubMessage = recv_message(&mut stream).await.unwrap();
    assert_eq!(
        response,
        HubMessage::AgentStarted {
            id: "abc123".into(),
            agent_binary: "claude".into(),
        }
    );

    // Split for bidirectional streaming
    let (mut reader, mut writer) = stream.into_split();

    // Read output from server
    let output: HubMessage = recv_message_read(&mut reader).await.unwrap();
    assert_eq!(
        output,
        HubMessage::AgentOutput {
            id: "abc123".into(),
            data: b"hello world".to_vec(),
        }
    );

    // Send input
    send_message_write(
        &mut writer,
        &CliMessage::AgentInput {
            id: "abc123".into(),
            data: vec![0x41],
        },
    )
    .await
    .unwrap();

    // Send resize
    send_message_write(
        &mut writer,
        &CliMessage::ResizeAgent {
            id: "abc123".into(),
            cols: 120,
            rows: 40,
        },
    )
    .await
    .unwrap();

    // Detach
    send_message_write(
        &mut writer,
        &CliMessage::DetachAgent {
            id: "abc123".into(),
        },
    )
    .await
    .unwrap();

    server.await.unwrap();
}
