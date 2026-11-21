use std::{net::SocketAddr, sync::Arc};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

use super::router;
use crate::{Connection, catalog::Catalog, data_root::Root, http_access, search::FusionMode};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;

async fn initialize_status(addr: SocketAddr, path: &str, host: &str, extra_headers: &str) -> u16 {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\n{extra_headers}\
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
        vec!["https://client.internal".into()],
        cancellation.clone(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = cancellation.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            http_access(app).into_make_service_with_connect_info::<Connection>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .unwrap();
    });

    let host = addr.to_string();
    let alias = format!("localhost:{}", addr.port());
    let allowed = "Origin: https://client.internal\r\n";
    assert_eq!(initialize_status(addr, "/mcp", &host, allowed).await, 200);
    assert_eq!(initialize_status(addr, "/mcp", &alias, "").await, 200);
    assert_eq!(initialize_status(addr, "/mcp/", &host, allowed).await, 404);
    assert_eq!(
        initialize_status(addr, "/mcp/extra", &host, allowed).await,
        404
    );
    for origin in [
        "Origin: https://client.internal:8443\r\n",
        "Origin: https://client.internal/path\r\n",
        "Origin: https://client.internal\r\nOrigin: https://other.internal\r\n",
    ] {
        assert_eq!(initialize_status(addr, "/mcp", &host, origin).await, 403);
    }
    let wrong_ip = format!("127.0.0.2:{}", addr.port());
    let wrong_port = format!("127.0.0.1:{}", addr.port().wrapping_add(1));
    for wrong_host in [&wrong_ip, &wrong_port] {
        assert_eq!(initialize_status(addr, "/mcp", wrong_host, "").await, 403);
    }
    assert_eq!(
        initialize_status(addr, "/mcp", &host, "Host: other.internal\r\n").await,
        400,
    );

    cancellation.cancel();
    server.await.unwrap();
}
