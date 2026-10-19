mod common;
use common::*;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Message},
};

#[tokio::test]
async fn unauthenticated_connections_are_limited_before_hello_or_password() {
    let server = TestServer::start_with(|c| c.server_password = Some("secret".into())).await;
    let mut connections = Vec::new();
    for _ in 0..16 {
        connections.push(open(&server).await);
    }
    let error = connect_async(server.url()).await.unwrap_err();
    let Error::Http(response) = error else {
        panic!("expected HTTP rate limit");
    };
    assert_eq!(response.status().as_u16(), 429);
}

#[tokio::test]
async fn oversized_hello_is_closed_before_json_parsing() {
    let server = TestServer::start().await;
    let mut connection = open(&server).await;
    connection.ws.send(Message::text(" ".repeat(96 * 1024 + 1))).await.unwrap();
    let reply = within("oversized frame rejection", connection.ws.next()).await;
    assert!(matches!(reply, None | Some(Err(_)) | Some(Ok(Message::Close(_)))));
}

#[tokio::test]
async fn oversized_declared_frames_are_rejected_without_their_body() {
    use tokio::io::AsyncWriteExt as _;
    for fragmented in [false, true] {
        let server = TestServer::start().await;
        let mut connection = open(&server).await;
        let tokio_tungstenite::MaybeTlsStream::Plain(tcp) = connection.ws.get_mut() else {
            panic!("local TCP");
        };
        if fragmented {
            // A 60 KiB non-final text frame, masked with zeroes.
            tcp.write_all(&[0x01, 0xfe, 0xf0, 0x00, 0, 0, 0, 0]).await.unwrap();
            tcp.write_all(&vec![b' '; 60 * 1024]).await.unwrap();
        }
        let length = if fragmented { 60 * 1024 } else { 96 * 1024 + 1 };
        let mut header = vec![if fragmented { 0x80 } else { 0x81 }, 0xff];
        header.extend_from_slice(&(length as u64).to_be_bytes());
        header.extend_from_slice(&[0; 4]);
        tcp.write_all(&header).await.unwrap();
        // Keep TCP open and send no payload: rejection must precede hello's 15 s timeout.
        let reply = within("declared frame length rejection", connection.ws.next()).await;
        assert!(matches!(reply, None | Some(Err(_)) | Some(Ok(Message::Close(_)))));
    }
}
