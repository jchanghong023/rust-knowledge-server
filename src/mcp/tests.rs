use std::{net::SocketAddr, sync::Arc};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

use super::router;
use crate::{catalog::Catalog, data_root::Root, search::FusionMode};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;

async fn initialize_status(addr: SocketAddr, path: &str, extra_headers: &str) -> u16 {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: knowledge.internal\r\n{extra_headers}\
         Accept: application/json, text/event-stream\r\nContent-Type: application/json\r\n\
         Connection: close\r\nContent-Length: {}\r\n\r\n{INITIALIZE}",
        INITIALIZE.len(),
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn exact_mcp_route_and_unambiguous_origin_and_host() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(Catalog::open(Root::open(dir.path()).unwrap()).unwrap());
    let cancellation = CancellationToken::new();
    let app = router(
        catalog,
        None,
        None,
        FusionMode::Grouped,
        vec!["knowledge.internal".into()],
        vec!["https://client.internal".into()],
        cancellation.clone(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = cancellation.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
    });

    let allowed = "Origin: https://client.internal\r\n";
    assert_eq!(initialize_status(addr, "/mcp", allowed).await, 200);
    assert_eq!(initialize_status(addr, "/mcp/", allowed).await, 404);
    assert_eq!(initialize_status(addr, "/mcp/extra", allowed).await, 404);
    assert_eq!(
        initialize_status(addr, "/mcp", "Origin: https://client.internal:8443\r\n").await,
        403,
    );
    assert_eq!(
        initialize_status(addr, "/mcp", "Origin: https://client.internal/path\r\n").await,
        403,
    );
    assert_eq!(
        initialize_status(
            addr,
            "/mcp",
            &format!("{allowed}Origin: https://other.internal\r\n")
        )
        .await,
        403,
    );
    assert_eq!(
        initialize_status(addr, "/mcp", "Host: other.internal\r\n").await,
        400,
    );

    cancellation.cancel();
    server.await.unwrap();
}
