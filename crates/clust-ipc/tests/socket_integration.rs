use tempfile::tempdir;
use tokio::net::{UnixListener, UnixStream};

use clust_ipc::{recv_message, send_message, AgentInfo, CliMessage, PoolMessage};

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
            send_message(&mut stream, &CliMessage::ListAgents)
                .await
                .unwrap();
            let resp: PoolMessage = recv_message(&mut stream).await.unwrap();
            resp
        }
    });

    // Server receives the message and responds
    let (mut server_stream, _) = listener.accept().await.unwrap();
    let msg: CliMessage = recv_message(&mut server_stream).await.unwrap();
    assert_eq!(msg, CliMessage::ListAgents);

    let response = PoolMessage::AgentList {
        agents: vec![AgentInfo {
            id: "abc123".into(),
            agent_binary: "claude".into(),
            started_at: "2026-03-25T10:00:00Z".into(),
            attached_clients: 1,
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
            assert_eq!(msg, CliMessage::StopPool);
            send_message(&mut stream, &PoolMessage::Ok).await.unwrap();
        }
    });

    for _ in 0..3 {
        let mut stream = UnixStream::connect(&sock_path).await.unwrap();
        send_message(&mut stream, &CliMessage::StopPool)
            .await
            .unwrap();
        let resp: PoolMessage = recv_message(&mut stream).await.unwrap();
        assert_eq!(resp, PoolMessage::Ok);
    }

    server.await.unwrap();
}
