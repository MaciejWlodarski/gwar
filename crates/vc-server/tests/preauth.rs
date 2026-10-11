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
