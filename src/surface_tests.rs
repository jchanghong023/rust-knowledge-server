use std::{
    collections::BTreeMap,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    Connection,
    audit::Audit,
    catalog::Catalog,
    data_root::Root,
    http_access,
    index::EMBEDDING_GENERATION,
    mcp,
    model_http::{ApiConfig, Embedding, Rerank},
    search::FusionMode,
    web,
};

const EMPLOYEE: &str = "00001234";
const LIBRARY: &str = "manual";
const SOURCE_PATH: &str = "guide/nested/Example.MD";
const SOURCE: &str =
    "\u{feff}# Example\r\n```cpp\r\n# not a heading\r\nstd::vector mystic;\r\n```\r\n";
const MCP_VERSION: &str = "2025-11-25";
const MCP_ACCEPT: &str = "application/json, text/event-stream";
const FOREIGN_ORIGIN: &str = "https://other.test";
const DEADLINE: Duration = Duration::from_secs(20);

struct Loopback {
    origin: String,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl Loopback {
    fn spawn(listener: TcpListener, app: Router, shutdown: CancellationToken) -> Self {
        let address: SocketAddr = listener.local_addr().unwrap();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<Connection>(),
            )
            .with_graceful_shutdown(task_shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        Self {
            origin: format!("http://{address}"),
            shutdown,
            task,
        }
    }

    async fn stop(mut self) {
        self.shutdown.cancel();
        time::timeout(DEADLINE, &mut self.task)
            .await
            .expect("loopback server did not stop")
            .expect("loopback server failed");
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

struct Service {
    server: Loopback,
    catalog: Arc<Catalog>,
    client: Client,
    imports: TaskTracker,
}

impl Service {
    async fn start(
        root: &Path,
        embedding: Option<Arc<Embedding>>,
        rerank: Option<Arc<Rerank>>,
        fusion: FusionMode,
    ) -> Self {
        let catalog = Arc::new(Catalog::open(Root::open(root).unwrap()).unwrap());
        let audit = Arc::new(Audit::open(catalog.root_path(), &catalog).unwrap());
        let imports = TaskTracker::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shutdown = CancellationToken::new();
        let app = web::router(
            catalog.clone(),
            audit,
            embedding.clone(),
            rerank.clone(),
            fusion,
            imports.clone(),
        )
        .merge(mcp::router(
            catalog.clone(),
            embedding,
            rerank,
            fusion,
            Vec::new(),
            shutdown.clone(),
        ));
        let server = Loopback::spawn(listener, http_access(app), shutdown);
        let client = Client::builder()
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(DEADLINE)
            .build()
            .unwrap();
        Self {
            server,
            catalog,
            client,
            imports,
        }
    }

    async fn stop(self) {
        self.server.stop().await;
        self.imports.close();
        time::timeout(DEADLINE, self.imports.wait())
            .await
            .expect("imports did not stop");
        // The SDK runs stateful MCP sessions in detached tasks. Let its
        // cancellation unwind the last catalog reference before disk faults.
        let catalog = Arc::downgrade(&self.catalog);
        drop(self.catalog);
        time::timeout(DEADLINE, async {
            while catalog.strong_count() != 0 {
                time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("shutdown did not release the catalog");
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.server.origin))
    }

    fn web(&self, method: Method, path: &str, cookie: Option<&str>) -> RequestBuilder {
        let mut request = self
            .request(method, path)
            .header(header::ORIGIN, &self.server.origin);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        request
    }

    async fn login(&self, employee: &str) -> String {
        let response = send(
            self.web(Method::POST, "/api/session", None)
                .json(&json!({"employee": employee})),
        )
        .await;
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(
            body(response, StatusCode::OK).await,
            json!({"employee": employee})
        );
        cookie
    }

    async fn create(&self, cookie: &str) {
        let response = send(
            self.web(Method::POST, "/api/libraries", Some(cookie))
                .json(&json!({"name": LIBRARY})),
        )
        .await;
        assert_status(&response, StatusCode::CREATED);
    }

    async fn libraries(&self, cookie: &str) -> Value {
        body(
            send(self.web(Method::GET, "/api/libraries", Some(cookie))).await,
            StatusCode::OK,
        )
        .await
    }

    async fn upload(&self, cookie: &str, files: &[(&str, &[u8])]) -> Response {
        // Keep bytes verbatim, including invalid UTF-8 in rejection cases.
        // The production request uses the same repeated path/file field pairs.
        const BOUNDARY: &str = "knowledge-surface-fixture-boundary";
        let mut payload = Vec::new();
        for (path, file) in files {
            payload.extend_from_slice(
                format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n{path}\r\n").as_bytes(),
            );
            payload.extend_from_slice(
                format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.md\"\r\nContent-Type: text/markdown\r\n\r\n").as_bytes(),
            );
            payload.extend_from_slice(file);
            payload.extend_from_slice(b"\r\n");
        }
        payload.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        send(
            self.web(Method::POST, "/api/library/import", Some(cookie))
                .query(&[("name", LIBRARY)])
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(payload),
        )
        .await
    }

    async fn publish_fixture(&self, cookie: &str) {
        self.publish(cookie, &[(SOURCE_PATH, SOURCE.as_bytes())])
            .await;
    }

    async fn publish(&self, cookie: &str, files: &[(&str, &[u8])]) {
        let accepted = body(self.upload(cookie, files).await, StatusCode::ACCEPTED).await;
        let id = accepted["id"].as_str().expect("import operation ID");
        time::timeout(DEADLINE, async {
            loop {
                let operation = self.operation(cookie, id).await;
                match operation["outcome"].as_str() {
                    Some("succeeded") => {
                        assert_eq!(operation["employee"], EMPLOYEE, "{operation}");
                        assert_eq!(operation["employee_self_reported"], true, "{operation}");
                        assert_eq!(operation["action"], "import", "{operation}");
                        assert_eq!(operation["library"], LIBRARY, "{operation}");
                        break;
                    }
                    Some("in_progress") => time::sleep(Duration::from_millis(20)).await,
                    _ => panic!("import {id} ended unexpectedly: {operation}"),
                }
            }
        })
        .await
        .expect("import did not reach a terminal outcome");
    }

    async fn operation(&self, cookie: &str, id: &str) -> Value {
        let path = format!("/api/operations/{id}");
        let operation = body(
            send(self.web(Method::GET, &path, Some(cookie))).await,
            StatusCode::OK,
        )
        .await;
        assert_eq!(operation["id"], id, "{operation}");
        operation
    }

    async fn web_search(&self, cookie: &str, query: &Value, status: StatusCode) -> Value {
        body(
            send(
                self.web(Method::POST, "/api/search", Some(cookie))
                    .json(query),
            )
            .await,
            status,
        )
        .await
    }

    fn index_path(&self) -> PathBuf {
        self.catalog.snapshot(Some(LIBRARY)).unwrap()[0]
            .1
            .index_path()
    }

    fn audit_records(&self) -> BTreeMap<String, Value> {
        fs::read_dir(self.catalog.root_path().join("audit"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                let data = fs::read(entry.path()).unwrap();
                (name, serde_json::from_slice(&data).unwrap())
            })
            .collect()
    }
}

async fn send(request: RequestBuilder) -> Response {
    request.send().await.unwrap()
}

fn assert_status(response: &Response, expected: StatusCode) {
    assert_eq!(response.status(), expected, "{}", response.url());
}

async fn body(response: Response, expected: StatusCode) -> Value {
    let status = response.status();
    let url = response.url().clone();
    let bytes = response.bytes().await.unwrap();
    assert_eq!(
        status,
        expected,
        "{url}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("{url}: {err}: {}", String::from_utf8_lossy(&bytes)))
}

async fn web_error(response: Response, status: StatusCode, code: &str) -> Value {
    let error = body(response, status).await;
    assert_eq!(error["error"], code, "{error}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{error}"
    );
    error
}

fn mcp_request(service: &Service) -> RequestBuilder {
    service
        .request(Method::POST, "/mcp")
        .header(header::ACCEPT, MCP_ACCEPT)
}

fn initialize_message() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": MCP_VERSION, "capabilities": {},
            "clientInfo": {"name": "surface-tests", "version": "1"}
        }
    })
}

struct Mcp<'a> {
    service: &'a Service,
    protocol: String,
    session: header::HeaderValue,
    next_id: u64,
}

impl<'a> Mcp<'a> {
    async fn connect(service: &'a Service) -> Self {
        let response = send(mcp_request(service).json(&initialize_message())).await;
        assert_status(&response, StatusCode::OK);
        let session = response.headers()["mcp-session-id"].clone();
        assert!(!session.as_bytes().is_empty());
        let reply = mcp_reply(response, 1).await;
        assert_eq!(reply["result"]["protocolVersion"], MCP_VERSION, "{reply}");
        assert!(
            reply["result"]["capabilities"]["tools"].is_object(),
            "{reply}"
        );
        let mcp = Self {
            service,
            protocol: reply["result"]["protocolVersion"]
                .as_str()
                .unwrap()
                .to_owned(),
            session,
            next_id: 2,
        };
        let response = send(mcp.request().json(&json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })))
        .await;
        assert_status(&response, StatusCode::ACCEPTED);
        assert!(response.bytes().await.unwrap().is_empty());
        mcp
    }

    fn request(&self) -> RequestBuilder {
        mcp_request(self.service)
            .header("mcp-protocol-version", &self.protocol)
            .header("mcp-session-id", self.session.clone())
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let response = send(self.request().json(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })))
        .await;
        mcp_reply(response, id).await
    }

    async fn tool(&mut self, name: &str, arguments: &Value) -> Value {
        self.rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    async fn search(&mut self, arguments: &Value) -> Value {
        successful_tool(self.tool("knowledge_search", arguments).await)
    }

    async fn fetch(&mut self, reference: &Value) -> Value {
        self.tool("knowledge_fetch", &json!({"ref": reference}))
            .await
    }
}

fn successful_tool(reply: Value) -> Value {
    assert!(reply.get("error").is_none(), "{reply}");
    let result = &reply["result"];
    assert_ne!(result["isError"], true, "{reply}");
    let structured = &result["structuredContent"];
    assert!(structured.is_object(), "{reply}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        *structured,
        "{reply}"
    );
    structured.clone()
}

async fn mcp_reply(mut response: Response, id: u64) -> Value {
    assert_status(&response, StatusCode::OK);
    let media_type = response.headers()[header::CONTENT_TYPE].to_str().unwrap();
    let reply: Value = match media_type.split(';').next().unwrap().trim() {
        "application/json" => response.json().await.unwrap(),
        "text/event-stream" => {
            // An SDK stream can begin with an empty priming event and need not
            // close after the response. Consume just the requested RPC reply.
            let mut pending = Vec::new();
            let mut data = Vec::new();
            'events: loop {
                let chunk = response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("SSE ended before JSON-RPC reply");
                pending.extend_from_slice(&chunk);
                let mut consumed = 0;
                while let Some(offset) = pending[consumed..].iter().position(|&byte| byte == b'\n')
                {
                    let end = consumed + offset;
                    let line = &pending[consumed..end];
                    consumed = end + 1;
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    if line.is_empty() {
                        if data.last() == Some(&b'\n') {
                            data.pop();
                        }
                        if !data.is_empty() {
                            let event: Value = serde_json::from_slice(&data).unwrap();
                            if event["id"] == id {
                                break 'events event;
                            }
                            data.clear();
                        }
                    } else if let Some(value) = line.strip_prefix(b"data:") {
                        data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
                        data.push(b'\n');
                    }
                }
                pending.drain(..consumed);
            }
        }
        other => panic!("unexpected MCP response media type: {other}"),
    };
    assert_eq!(reply["jsonrpc"], "2.0", "{reply}");
    assert_eq!(reply["id"], id, "{reply}");
    reply
}

fn fixture_query() -> Value {
    json!({"query": "std::vector", "library": LIBRARY, "path": "guide/nested"})
}

fn assert_fixture_evidence(reply: &Value) {
    let digest = Sha256::digest(SOURCE.as_bytes());
    let sha256: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        reply["scope"],
        json!({"library": LIBRARY, "path": "guide/nested"})
    );
    assert_eq!(reply["status"], "ok");
    assert_eq!(reply["retrieval"], "keyword");
    assert_eq!(
        reply["matches"],
        json!([{
            "text": SOURCE, "library": LIBRARY, "path": SOURCE_PATH,
            "line_start": 1, "line_end": 5, "sha256": sha256, "category": "exact"
        }])
    );
}

fn assert_mcp_fixture(reply: &Value) {
    let hit = &reply["matches"][0];
    assert!(hit["ref"].as_str().is_some_and(|value| !value.is_empty()));
    assert_eq!(
        reply,
        &json!({
            "status": "ok",
            "matches": [{
                "text": SOURCE, "library": LIBRARY, "path": SOURCE_PATH,
                "line_start": 1, "line_end": 5, "category": "exact",
                "ref": hit["ref"], "truncated": false, "section_title": "Example",
                "match_ranges": [{"line_start": 4, "line_end": 4}]
            }]
        })
    );
}

#[tokio::test]
async fn collab2_room_number_cannot_be_replaced_and_expires_on_normal_close() {
    async fn open_host(client: &Client, origin: &str, room_id: &str) -> reqwest::Upgraded {
        let response = send(
            client
                .get(format!("{origin}/r/{room_id}?role=host"))
                .header(header::CONNECTION, "upgrade")
                .header(header::UPGRADE, "websocket")
                .header(header::SEC_WEBSOCKET_VERSION, "13")
                .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ=="),
        )
        .await;
        assert_status(&response, StatusCode::SWITCHING_PROTOCOLS);
        let mut socket = response.upgrade().await.unwrap();
        let mut ready = [0; 20];
        time::timeout(DEADLINE, socket.read_exact(&mut ready))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&ready, b"\x81\x12{\"t\":\"host-ready\"}");
        socket
    }

    let jobs = TaskTracker::new();
    let shutdown = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = crate::collab::router(jobs.clone(), shutdown.clone());
    let server = Loopback::spawn(listener, http_access(app), shutdown);
    let client = Client::builder()
        .http1_only()
        .no_proxy()
        .timeout(DEADLINE)
        .build()
        .unwrap();
    let room_number = "001Review";
    let room_id = "FirstHost01";
    let other_room_id = "SecondHost02";
    let mut host = open_host(&client, &server.origin, room_id).await;
    let other_host = open_host(&client, &server.origin, other_room_id).await;
    let register = |room_id, secret| {
        client
            .post(format!("{}/api/collab2/rooms", server.origin))
            .json(&json!({"roomNumber": room_number, "roomId": room_id, "secret": secret}))
    };
    let lookup = || {
        client
            .post(format!("{}/api/collab2/lookup", server.origin))
            .json(&json!({"roomNumber": room_number}))
    };
    let secret = "A".repeat(64);
    let response = send(register(room_id, secret.clone())).await;
    assert_status(&response, StatusCode::CREATED);
    let response = send(register(other_room_id, "B".repeat(64))).await;
    assert_status(&response, StatusCode::CONFLICT);
    assert_eq!(
        body(send(lookup()).await, StatusCode::OK).await,
        json!({"roomId": room_id, "secret": secret})
    );

    // Client Close(1000), masked with [1, 2, 3, 4]; server reply is unmasked.
    host.write_all(b"\x88\x82\x01\x02\x03\x04\x02\xea")
        .await
        .unwrap();
    let mut reply = [0; 4];
    time::timeout(DEADLINE, host.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, b"\x88\x02\x03\xe8");
    assert_status(&send(lookup()).await, StatusCode::NOT_FOUND);

    drop((host, other_host));
    server.stop().await;
    jobs.close();
    time::timeout(DEADLINE, jobs.wait()).await.unwrap();
}

#[tokio::test]
async fn native_mcp_and_web_agree_on_evidence_empty_states_and_missing_segments() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let before_mcp = service.audit_records();
    let rejected = send(
        mcp_request(&service)
            .header(header::ORIGIN, FOREIGN_ORIGIN)
            .json(&initialize_message()),
    )
    .await;
    assert_status(&rejected, StatusCode::FORBIDDEN);
    let mut mcp = Mcp::connect(&service).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2, "{listed}");
    let find = |name| tools.iter().find(|tool| tool["name"] == name).unwrap();
    let search = find("knowledge_search");
    let fetch = find("knowledge_fetch");
    for tool in [search, fetch] {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
    }
    assert_eq!(search["inputSchema"]["required"], json!(["query"]));
    let input = &search["inputSchema"]["properties"];
    assert_eq!(input["limit"]["minimum"], 1);
    assert_eq!(input["limit"]["maximum"], 16);
    assert!(
        input["cursor"]["description"]
            .as_str()
            .unwrap()
            .contains("next_cursor")
    );
    assert!(search["description"].as_str().unwrap().contains("limit"));
    assert_eq!(
        search["outputSchema"]["properties"]["matches"]["maxItems"],
        16
    );
    let output = &search["outputSchema"];
    let hit_ref = output["properties"]["matches"]["items"]["$ref"]
        .as_str()
        .unwrap();
    let hit = output.pointer(hit_ref.strip_prefix('#').unwrap()).unwrap();
    let required = hit["required"].as_array().unwrap();
    assert!(required.contains(&json!("match_ranges")), "{hit}");
    assert_eq!(hit["properties"]["match_ranges"]["type"], "array");
    assert_eq!(fetch["inputSchema"]["required"], json!(["ref"]));
    assert_eq!(fetch["inputSchema"]["additionalProperties"], false);
    let required = fetch["outputSchema"]["required"].as_array().unwrap();
    for field in [
        "text",
        "library",
        "path",
        "line_start",
        "line_end",
        "truncated",
    ] {
        assert!(required.contains(&json!(field)));
    }
    let unknown = mcp
        .tool("knowledge_search", &json!({"query": "vector", "offset": 3}))
        .await;
    assert_eq!(unknown["result"]["isError"], true, "{unknown}");
    assert!(
        unknown["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown field `offset`")
    );
    let write = mcp
        .tool("knowledge_create", &json!({"name": "from-mcp"}))
        .await;
    assert_eq!(write["error"]["code"], -32602, "{write}");
    let anonymous = mcp.search(&json!({"query": "std::vector"})).await;
    assert_eq!(anonymous["status"], "no_published_index", "{anonymous}");
    assert_eq!(
        service.audit_records(),
        before_mcp,
        "MCP must not register or audit as a Web employee"
    );

    let cookie = service.login(EMPLOYEE).await;
    assert_eq!(service.libraries(&cookie).await, json!([]));
    service.create(&cookie).await;
    assert_eq!(
        service.libraries(&cookie).await,
        json!([{"name": LIBRARY, "published": false}])
    );
    let query = fixture_query();
    let no_index = service.web_search(&cookie, &query, StatusCode::OK).await;
    assert_eq!(no_index["status"], "no_published_index");
    assert_eq!(no_index["matches"], json!([]));
    assert_eq!(
        mcp.search(&query).await,
        json!({"status": "no_published_index", "matches": []})
    );

    for (invalid, status, code, reason) in [
        (
            json!({"query": " \t", "library": LIBRARY}),
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "query must not be blank",
        ),
        (
            json!({"query": "std::vector", "path": "../guide"}),
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "invalid relative path scope",
        ),
        (
            json!({"query": "std::vector", "library": "missing"}),
            StatusCode::NOT_FOUND,
            "not_found",
            "library does not exist",
        ),
    ] {
        let web = service.web_search(&cookie, &invalid, status).await;
        assert_eq!(web["error"], code, "{web}");
        assert_eq!(web["message"], reason, "{web}");
        let rpc = mcp.tool("knowledge_search", &invalid).await;
        assert_eq!(rpc["error"]["code"], -32602, "{rpc}");
        assert_eq!(rpc["error"]["data"]["error"], "invalid_input", "{rpc}");
        assert!(rpc["error"].to_string().contains(reason), "{rpc}");
        assert!(rpc.get("result").is_none(), "{rpc}");
    }

    service.publish_fixture(&cookie).await;
    assert_eq!(
        service.libraries(&cookie).await,
        json!([{"name": LIBRARY, "published": true}])
    );
    let web = service.web_search(&cookie, &query, StatusCode::OK).await;
    assert_fixture_evidence(&web);
    assert_eq!(web["reranked"], false);
    let audit = service.audit_records();
    let found = mcp.search(&query).await;
    assert_mcp_fixture(&found);
    let reference = &found["matches"][0]["ref"];
    let fetch = successful_tool(mcp.fetch(reference).await);
    assert_eq!(
        fetch,
        json!({
            "text": SOURCE, "library": LIBRARY, "path": SOURCE_PATH,
            "line_start": 1, "line_end": 5, "truncated": false,
            "previous_ref": null, "next_ref": null
        })
    );
    let invalid = mcp.fetch(&json!("../not-a-reference")).await;
    assert_eq!(
        invalid["error"]["data"]["error"], "invalid_input",
        "{invalid}"
    );
    assert_mcp_fixture(&mcp.search(&json!({"query": "std::vector"})).await);
    assert_eq!(
        service.audit_records(),
        audit,
        "MCP must not audit as the registered Web employee"
    );

    let unmatched = json!({"query": "std::vector", "library": LIBRARY, "path": "guide/absent.md"});
    let zero = service
        .web_search(&cookie, &unmatched, StatusCode::OK)
        .await;
    assert_eq!(zero["status"], "ok");
    assert_eq!(zero["matches"], json!([]));
    assert_eq!(
        mcp.search(&unmatched).await,
        json!({"status": "ok", "matches": []})
    );

    let index = service.index_path();
    let persisted_audit = service.audit_records();
    service.stop().await;
    let segments = index.join("segments");
    let entries: Vec<_> = fs::read_dir(&segments)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!entries.is_empty(), "expected persisted Edge segments");
    for entry in entries {
        if entry.file_type().unwrap().is_dir() {
            fs::remove_dir_all(entry.path()).unwrap();
        } else {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let config = index.join("edge_config.json");
    let original_config = fs::read(&config).expect("the index configuration remains on disk");

    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    assert_eq!(service.audit_records(), persisted_audit);
    let mut mcp = Mcp::connect(&service).await;
    let cookie = service.login(EMPLOYEE).await;
    let web = service
        .web_search(&cookie, &query, StatusCode::SERVICE_UNAVAILABLE)
        .await;
    assert_eq!(web["error"], "storage", "{web}");
    assert!(web.get("matches").is_none(), "{web}");
    let rpc = mcp.tool("knowledge_search", &query).await;
    let result = &rpc["result"];
    assert_eq!(result["isError"], true, "{rpc}");
    assert_eq!(result["structuredContent"]["error"], "storage", "{rpc}");
    assert!(
        result["structuredContent"].get("matches").is_none(),
        "{rpc}"
    );
    let rpc = mcp.fetch(reference).await;
    assert_eq!(rpc["result"]["isError"], true, "{rpc}");
    assert_eq!(
        rpc["result"]["structuredContent"]["error"], "storage",
        "{rpc}"
    );
    assert!(fs::read_dir(segments).unwrap().next().is_none());
    assert_eq!(fs::read(config).unwrap(), original_config);
    service.stop().await;
}

#[tokio::test]
async fn mcp_search_keeps_distinct_sources_and_complete_names_within_budget() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    // The name is in both overlapping windows of an oversized physical line.
    let wide = format!(
        "opening\r\n{} std::vector {}\r\n",
        "界".repeat(17_750),
        "字".repeat(3_000)
    );
    let standard = format!(
        "# Title\r\nbefore\r\n{}\r\nstd::vector value;\r\nafter\r\n",
        "项".repeat(1500)
    );
    let paths = ["wide.md", "a.md", "b.md", "c.md", "d.md", "e.md", "f.md"];
    let files = paths.map(|path| {
        (
            path,
            if path == "wide.md" {
                wide.as_bytes()
            } else {
                standard.as_bytes()
            },
        )
    });
    service.publish(&cookie, &files).await;
    let mut mcp = Mcp::connect(&service).await;
    let scoped = json!({"query": "std::vector", "path": "wide.md"});
    let web = service.web_search(&cookie, &scoped, StatusCode::OK).await;
    assert_eq!(web["matches"].as_array().unwrap().len(), 2);
    let response = mcp.search(&scoped).await;
    let hits = response["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{response}");
    let hit = &hits[0];
    assert_eq!(hit["category"], "exact");
    assert_eq!(hit["truncated"], true);
    assert_eq!(hit["line_start"], 2);
    assert_eq!(hit["line_end"], 2);
    let excerpt = hit["text"].as_str().unwrap();
    assert!(excerpt.contains("std::vector") && wide.contains(excerpt));

    let query = json!({"query": "std::vector", "library": LIBRARY});
    let web = service.web_search(&cookie, &query, StatusCode::OK).await;
    let response = mcp.search(&query).await;
    let hits = response["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{response}");
    assert!(
        hits.iter()
            .map(|hit| hit["text"].as_str().unwrap().len())
            .sum::<usize>()
            <= 4096
    );
    let mut ranked = web["matches"].as_array().unwrap().iter();
    let mut distinct = std::collections::BTreeSet::new();
    let mut copies = std::collections::BTreeSet::new();
    for hit in hits {
        let path = hit["path"].as_str().unwrap();
        assert!(distinct.insert(path));
        let text = hit["text"].as_str().unwrap();
        assert!(text.contains("std::vector"));
        assert!(
            ranked.any(|web_hit| {
                web_hit["path"] == path && web_hit["text"].as_str().unwrap().contains(text)
            }),
            "{path} not in shared rank order"
        );
        if path != "wide.md" {
            copies.insert(path.to_owned());
            let duplicates = hit["duplicates"].as_array().unwrap();
            for duplicate in duplicates {
                let copy_path = duplicate["path"].as_str().unwrap();
                assert!(copies.insert(copy_path.to_owned()), "{duplicate}");
                assert_eq!(duplicate["line_start"], hit["line_start"]);
                assert_eq!(duplicate["line_end"], hit["line_end"]);
                let fetched = successful_tool(mcp.fetch(&duplicate["ref"]).await);
                assert_eq!(fetched["path"], copy_path);
                assert!(fetched["text"].as_str().unwrap().contains(text));
            }
            let offset = standard.find(text).unwrap();
            let line_start = standard[..offset]
                .bytes()
                .filter(|&byte| byte == b'\n')
                .count()
                + 1;
            let line_end = line_start
                + text
                    .trim_end_matches('\n')
                    .bytes()
                    .filter(|&byte| byte == b'\n')
                    .count();
            assert_eq!(hit["line_start"], line_start);
            assert_eq!(hit["line_end"], line_end);
        }
    }
    assert_eq!(
        copies,
        paths[1..].iter().map(|path| (*path).to_owned()).collect()
    );
    service.stop().await;
}

#[tokio::test]
async fn mcp_combines_repeated_examples_without_aliasing_different_originals() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let steps = (1..=18)
        .map(|step| format!("Step {step}: inspect the timing report for this lane.\r\n"))
        .collect::<String>();
    let original = format!("Lane Select example: use -value=1.\r\n{steps}Layout notes.");
    let copy = format!(
        "LANE SELECT   EXAMPLE: use -value=1.\n\n{}Constraints notes.",
        steps.to_ascii_uppercase().replace("\r\n", "\n")
    );
    let opposite = format!("Lane Select example: use -value=0.\r\n{steps}Other mode.");
    let files = [
        ("original.md", original.as_str()),
        ("reprint.md", copy.as_str()),
        ("different.md", opposite.as_str()),
    ];
    let upload = files.map(|(path, text)| (path, text.as_bytes()));
    service.publish(&cookie, &upload).await;
    let mut mcp = Mcp::connect(&service).await;
    let response = mcp.search(&json!({"query": "Lane Select"})).await;
    let hits = response["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "{response}");
    let other = hits
        .iter()
        .find(|hit| hit["path"] == "different.md")
        .unwrap();
    assert!(other["text"].as_str().unwrap().contains("-value=0"));
    assert!(other["duplicates"].is_null());
    let primary = hits
        .iter()
        .find(|hit| hit["path"] != "different.md")
        .unwrap();
    let alternatives = primary["duplicates"].as_array().unwrap();
    assert_eq!(alternatives.len(), 1, "{primary}");
    let alternate = &alternatives[0];
    assert_eq!(alternate["match"], "shared_prefix");
    assert_ne!(primary["path"], alternate["path"]);
    let expected = files
        .iter()
        .find(|(path, _)| alternate["path"] == *path)
        .unwrap()
        .1;
    let context = successful_tool(mcp.fetch(&alternate["ref"]).await);
    let text = context["text"].as_str().unwrap();
    assert_eq!(context["path"], alternate["path"]);
    assert!(expected.contains(text) && text.contains("-value=1"));
    assert!(context["line_start"].as_u64() <= alternate["line_start"].as_u64());
    assert!(context["line_end"].as_u64() >= alternate["line_end"].as_u64());
    service.stop().await;
}

#[tokio::test]
async fn mcp_literal_queries_keep_punctuation_order_and_complete_identifiers() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    service
        .publish(
            &cookie,
            &[
                ("a-other.md", b"March C+ tmerge. IEEE 1500 TRST optional."),
                ("b-march.md", b"Run March C- before verification."),
                ("c-flag.md", b"Simulate with `-xprop=tmerge`."),
                (
                    "d-standard.md",
                    b"Under IEEE 1149.1, TRST is an optional pin.",
                ),
                ("e-error.md", b"Error N17: scan chain is not connected."),
            ],
        )
        .await;
    let mut mcp = Mcp::connect(&service).await;
    for (query, path) in [
        ("March C-", "b-march.md"),
        ("March C+", "a-other.md"),
        ("-xprop=tmerge", "c-flag.md"),
        ("IEEE 1149.1", "d-standard.md"),
        ("Error N17: scan chain is not connected.", "e-error.md"),
    ] {
        let ranked = mcp.search(&json!({"query": query})).await;
        assert_eq!(ranked["matches"][0]["path"], path, "{ranked}");
        assert_eq!(ranked["matches"][0]["category"], "exact", "{ranked}");
        let strict = json!({"query": query, "strict": true});
        let ranked = mcp.search(&strict).await;
        let hits = ranked["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{ranked}");
        assert_eq!(hits[0]["path"], path, "{ranked}");
        assert!(hits[0]["text"].as_str().unwrap().contains(query));
    }
    let standard = mcp
        .search(&json!({"query": "IEEE 1149.1 TRST optional"}))
        .await;
    assert_eq!(
        standard["matches"][0]["path"], "d-standard.md",
        "{standard}"
    );
    let missing = json!({"query": "N17999", "strict": true});
    assert_eq!(mcp.search(&missing).await["matches"], json!([]));
    let missing = json!({"query": "N17 scan chain", "strict": true});
    assert_eq!(mcp.search(&missing).await["matches"], json!([]));
    let strict = json!({"query": "-xprop=tmerge", "strict": true});
    let web = service.web_search(&cookie, &strict, StatusCode::OK).await;
    assert_eq!(web["retrieval"], "literal");
    assert_eq!(web["matches"][0]["path"], "c-flag.md");
    service.stop().await;
}

#[tokio::test]
async fn mcp_exact_rule_search_keeps_late_body_and_ranks_it_above_contents() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let manual = "\
# Contents\r\n\
Check Rule N16 117\r\n\
Check Rule N17 ................ 118\r\n\
- [Check Rule N18](#check-rule-n18)\r\n\
# Fixture netlist\r\n\
wire N16, N17, N18;\r\n\
# Upgrade notes\r\n\
See N17 for a previously documented check.\r\n\
# Check Rule N16\r\n\
Warning: the synthetic amber input has no reference.\r\n\
Connect the reference before running the fixture.\r\n\
# Check Rule N17\r\n\
Error N17: the synthetic violet input is unconnected.\r\n\
Connect the violet input to the dummy loop.\r\n\
# Check Rule N18\r\n\
Error N18: the synthetic magenta output is reserved.\r\n\
Use the spare output in the dummy loop.\r\n";
    service
        .publish(
            &cookie,
            &[
                ("manual.md", manual.as_bytes()),
                ("other.md", b"Check Rule N170 is a different check."),
            ],
        )
        .await;
    let mut mcp = Mcp::connect(&service).await;
    let body = "Error N17: the synthetic violet input is unconnected.";
    let known = mcp
        .search(&json!({"query": body, "strict": true, "path": "manual.md"}))
        .await;
    assert!(known["matches"][0]["text"].as_str().unwrap().contains(body));
    for (code, line, title, expected) in [
        (
            "N16",
            9,
            "Check Rule N16",
            "the synthetic amber input has no reference.",
        ),
        ("N17", 12, "Check Rule N17", body),
        (
            "N18",
            15,
            "Check Rule N18",
            "the synthetic magenta output is reserved.",
        ),
    ] {
        for query in [code, title] {
            let mut exact = Vec::new();
            for strict in [false, true] {
                let result = mcp
                    .search(&json!({"query": query, "strict": strict, "path": "manual.md"}))
                    .await;
                let hits = result["matches"].as_array().unwrap();
                let first = &hits[0];
                assert_eq!(
                    first["category"], "exact",
                    "{query} strict={strict}: {first}"
                );
                assert_eq!(first["section_title"], title);
                assert_eq!(
                    first["match_ranges"][0],
                    json!({"line_start": line, "line_end": line})
                );
                let shown = first["text"].as_str().unwrap();
                assert!(shown.contains(expected), "{query} strict={strict}: {first}");
                let excerpt = successful_tool(mcp.fetch(&first["ref"]).await);
                assert!(excerpt["text"].as_str().unwrap().contains(shown));
                exact.push(
                    hits.iter()
                        .filter(|hit| hit["category"] == "exact")
                        .map(|hit| (hit["path"].clone(), hit["match_ranges"].clone()))
                        .collect::<Vec<_>>(),
                );
            }
            assert_eq!(exact[0], exact[1], "{query}");
        }
    }
    service.stop().await;
}

#[tokio::test]
async fn exact_mentions_outrank_contents_links_and_index_rows() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    service
        .publish(
            &cookie,
            &[
                ("a.md", b"# Contents\n\n- [QX-7](z.md)\n"),
                (
                    "b.md",
                    b"# Index\n\n| Rule | File |\n| --- | --- |\n| QX-7 | z-rule |\n",
                ),
                (
                    "c.md",
                    b"# Reminders\n\nSee QX-7 before opening the shutter.\n",
                ),
                ("z.md", b"## QX-7\n\nClose the shutter and lock the pin.\n"),
            ],
        )
        .await;
    let mut mcp = Mcp::connect(&service).await;
    for strict in [false, true] {
        let query = json!({"query": "QX-7", "strict": strict});
        for result in [
            service.web_search(&cookie, &query, StatusCode::OK).await,
            mcp.search(&query).await,
        ] {
            let exact: Vec<_> = result["matches"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|hit| hit["category"] == "exact")
                .map(|hit| hit["path"].as_str().unwrap())
                .collect();
            assert_eq!(exact, ["z.md", "c.md", "a.md", "b.md"], "{result}");
        }
    }
    service.stop().await;
}

#[tokio::test]
async fn mcp_search_pages_past_old_result_and_file_limits() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let manual = (1..=21)
        .map(|id| format!("## Fixture {id}\n\nP713 controls the test output on lane {id}.\n\n"))
        .collect::<String>();
    service
        .publish(&cookie, &[("manual.md", manual.as_bytes())])
        .await;
    let mut mcp = Mcp::connect(&service).await;
    let mut args = json!({"query": "P713", "strict": true, "limit": 3, "path": "manual.md"});
    let mut first_cursor = None;
    let mut seen = std::collections::BTreeSet::new();
    for page in 0..=7 {
        let result = mcp.search(&args).await;
        let hits = result["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 3, "{result}");
        for hit in hits {
            assert_eq!(hit["category"], "exact");
            let matched = hit["match_ranges"][0]["line_start"].as_u64().unwrap();
            assert!(seen.insert(matched), "repeated hit from line {matched}");
            assert_eq!(hit["match_ranges"][0]["line_end"], matched);
            assert_eq!(
                hit["section_title"],
                format!("Fixture {}", (matched + 1) / 4)
            );
        }
        if page == 0 {
            first_cursor = result["next_cursor"].as_str().map(str::to_owned);
        }
        if let Some(cursor) = result["next_cursor"].as_str() {
            args["cursor"] = json!(cursor);
        } else {
            break;
        }
        assert!(page < 6, "cursor did not terminate after all 21 hits");
    }
    assert_eq!(seen, (1..=21).map(|n| 4 * n - 1).collect());
    let first_cursor = first_cursor.expect("first page offers continuation");
    let mismatch = mcp
        .tool(
            "knowledge_search",
            &json!({"query": "P714", "strict": true, "limit": 3, "path": "manual.md", "cursor": first_cursor}),
        )
        .await;
    assert_eq!(mismatch["error"]["code"], -32602, "{mismatch}");
    let malformed = mcp
        .tool(
            "knowledge_search",
            &json!({"query": "P713", "cursor": "bad"}),
        )
        .await;
    assert_eq!(malformed["error"]["code"], -32602, "{malformed}");
    assert!(
        malformed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("without a cursor"),
        "{malformed}"
    );
    let invalid_limit = mcp
        .tool("knowledge_search", &json!({"query": "P713", "limit": 17}))
        .await;
    assert!(
        invalid_limit["error"].is_object() || invalid_limit["result"]["isError"] == true,
        "{invalid_limit}"
    );
    service
        .publish(&cookie, &[("manual.md", manual.as_bytes())])
        .await;
    let stale = mcp
        .tool(
            "knowledge_search",
            &json!({"query": "P713", "strict": true, "limit": 3, "path": "manual.md", "cursor": first_cursor}),
        )
        .await;
    assert_eq!(stale["error"]["code"], -32602, "{stale}");
    service.stop().await;
}

#[tokio::test]
async fn mcp_search_resumes_a_hit_deferred_by_the_byte_budget() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let query = "calibrate the dummy channel ".repeat(110) + "now";
    let first = format!("# Amber procedure\n{query}\n");
    let second = format!("# Violet procedure\n{query}\n");
    service
        .publish(
            &cookie,
            &[("a.md", first.as_bytes()), ("b.md", second.as_bytes())],
        )
        .await;
    let mut mcp = Mcp::connect(&service).await;
    let args = json!({"query": query, "strict": true, "limit": 2});
    let first = mcp.search(&args).await;
    let cursor = first["next_cursor"].as_str().expect("more evidence");
    let mut resumed = args;
    resumed["cursor"] = json!(cursor);
    let second = mcp.search(&resumed).await;
    for (page, path) in [(&first, "a.md"), (&second, "b.md")] {
        let hits = page["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{path}");
        assert_eq!(hits[0]["path"], path);
        assert!(hits[0]["text"].as_str().unwrap().contains(&query));
    }
    assert!(second["next_cursor"].is_null());
    service.stop().await;
}

#[tokio::test]
async fn mcp_match_lines_inherit_the_real_heading_across_a_fenced_split() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let query = "Fictional check\r\nreports a missing part";
    let source = format!(
        "# Verified section\r\n```text\r\n# Not a section\r\n{}\r\n{query}\r\n```\r\n",
        "x".repeat(18_010)
    );
    service
        .publish(&cookie, &[("fixture.md", source.as_bytes())])
        .await;
    let mut mcp = Mcp::connect(&service).await;
    let result = mcp.search(&json!({"query": query, "strict": true})).await;
    let hit = &result["matches"][0];
    assert_eq!(hit["section_title"], "Verified section", "{result}");
    assert_eq!(
        hit["match_ranges"],
        json!([{"line_start": 5, "line_end": 6}])
    );
    assert!(hit["text"].as_str().unwrap().contains(query));
    service.stop().await;
}

#[tokio::test]
async fn mcp_finds_a_late_heading_without_treating_fenced_text_as_a_heading() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let prelude = "Synthetic filler for a merged reference.\r\n".repeat(8_000);
    let source = format!(
        "# Prelude\r\n{prelude}\r\n\
         ## Local context\r\nA short synthetic introduction.\r\n\r\n\
         ### Rule Z47\r\n\r\nThe synthetic monitor watches the spare lane.\r\n\
         ```text\r\n{}\r\n### FencedLookalike47\r\n```\r\n",
        "fence contents for a synthetic source\r\n".repeat(600)
    );
    let last_section = format!(
        "{}\n# Last section\nLocal proof for synthetic EOF marker T9R.\n",
        ("x".repeat(99) + "\n").repeat(2_701)
    );
    service
        .publish(
            &cookie,
            &[
                ("merged.md", source.as_bytes()),
                ("tail.md", last_section.as_bytes()),
                ("z-next-file.md", b"# A different source\nUnrelated text.\n"),
            ],
        )
        .await;
    let mut mcp = Mcp::connect(&service).await;
    for query in ["Rule Z47", "FencedLookalike47"] {
        let result = mcp.search(&json!({"query": query, "strict": true})).await;
        let hit = &result["matches"][0];
        assert_eq!(hit["section_title"], "Rule Z47", "{result}");
        assert!(hit["match_ranges"][0]["line_start"].as_u64().unwrap() > 8_000);
    }
    let result = mcp
        .search(&json!({"query": "T9R", "strict": true, "path": "tail.md"}))
        .await;
    let hit = &result["matches"][0];
    assert_eq!(hit["section_title"], "Last section", "{result}");
    assert_eq!(
        hit["match_ranges"],
        json!([{"line_start": 2704, "line_end": 2704}])
    );
    service.stop().await;
}

#[tokio::test]
async fn mcp_related_fetch_preserves_shown_bytes_and_pages_both_directions() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let mut source = String::from("# Prelude\r\n");
    for line in 1..=32 {
        source.push_str(&format!("Prelude {line} 中文\r\n"));
    }
    source.push_str("# Evidence\r\nalpha beta\r\n");
    for line in 1..=80 {
        source.push_str(&format!("Evidence {line} 中文\r\n"));
    }
    service
        .publish(&cookie, &[("source.md", source.as_bytes())])
        .await;
    let mut mcp = Mcp::connect(&service).await;
    let ranked = mcp.search(&json!({"query": "beta alpha"})).await;
    let hit = &ranked["matches"][0];
    assert_eq!(hit["category"], "related", "{ranked}");
    assert_eq!(hit["match_ranges"], json!([]), "{ranked}");
    let context = successful_tool(mcp.fetch(&hit["ref"]).await);
    let middle = context["text"].as_str().unwrap();
    assert!(middle.contains(hit["text"].as_str().unwrap()));
    assert!(context["line_end"].as_u64().unwrap() - context["line_start"].as_u64().unwrap() < 60);
    let previous = successful_tool(mcp.fetch(&context["previous_ref"]).await);
    let next = successful_tool(mcp.fetch(&context["next_ref"]).await);
    let before = previous["text"].as_str().unwrap();
    let after = next["text"].as_str().unwrap();
    assert_eq!(format!("{before}{middle}{after}"), source);
    assert!(previous["previous_ref"].is_null() && next["next_ref"].is_null());
    service.stop().await;
}

#[tokio::test]
async fn mcp_fetch_reopens_existing_fragments_and_rejects_retired_refs() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    let wide = format!(
        "# Context\r\n{} left_marker std::vector {} right_marker {}\r\n# End\r\n",
        "界".repeat(17_460),
        "字".repeat(590),
        "文".repeat(1_500)
    );
    let lines = (1..=83)
        .map(|line| {
            if line == 40 {
                "# std::vector\r\n".to_owned()
            } else {
                format!("# line {line}\r\n")
            }
        })
        .collect::<String>();
    let files = [
        ("overlap.md", wide.as_bytes()),
        ("lines.md", lines.as_bytes()),
        ("other.md", b"not this file"),
    ];
    service.publish(&cookie, &files).await;
    let mut mcp = Mcp::connect(&service).await;
    let query = json!({"query": "std::vector", "path": "overlap.md"});
    let hit = mcp.search(&query).await;
    let wide_shown = hit["matches"][0]["text"].as_str().unwrap();
    let wide_ref = &hit["matches"][0]["ref"];
    let hit = mcp
        .search(&json!({"query": "std::vector", "path": "lines.md"}))
        .await;
    let lines_ref = &hit["matches"][0]["ref"];
    service.stop().await;

    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let mut mcp = Mcp::connect(&service).await;
    let context = successful_tool(mcp.fetch(wide_ref).await);
    let text = context["text"].as_str().unwrap();
    assert!((4093..=4096).contains(&text.len()));
    assert!(text.contains(wide_shown) && wide.contains(text));
    let previous = successful_tool(mcp.fetch(&context["previous_ref"]).await);
    let next = successful_tool(mcp.fetch(&context["next_ref"]).await);
    let adjacent = format!(
        "{}{text}{}",
        previous["text"].as_str().unwrap(),
        next["text"].as_str().unwrap()
    );
    assert!(wide.contains(&adjacent));
    assert!(adjacent.contains("left_marker") && adjacent.contains("right_marker"));
    assert_eq!(context["path"], "overlap.md");
    assert_eq!(context["line_start"], 2);
    assert_eq!(context["line_end"], 2);
    assert_eq!(context["truncated"], true);
    let context = successful_tool(mcp.fetch(lines_ref).await);
    let start = context["line_start"].as_u64().unwrap() as usize;
    let end = context["line_end"].as_u64().unwrap() as usize;
    assert_eq!(end - start + 1, 60);
    assert!((start..=end).contains(&40));
    assert_eq!(
        context["text"],
        lines
            .split_inclusive('\n')
            .skip(start - 1)
            .take(60)
            .collect::<String>()
    );
    assert_eq!(context["path"], "lines.md");
    assert_eq!(context["truncated"], true);

    let cookie = service.login(EMPLOYEE).await;
    service.publish(&cookie, &files).await; // Same content, different published generation.
    let stale = mcp.fetch(wide_ref).await;
    assert_eq!(stale["result"]["isError"], true);
    assert_eq!(
        stale["result"]["structuredContent"]["error"], "stale_ref",
        "{stale}"
    );
    let found = mcp.search(&query).await;
    let active_ref = &found["matches"][0]["ref"];
    assert_ne!(active_ref, wide_ref);
    let deleted = send(
        service
            .web(Method::DELETE, "/api/library", Some(&cookie))
            .query(&[("name", LIBRARY)]),
    )
    .await;
    assert_status(&deleted, StatusCode::NO_CONTENT);
    let stale = mcp.fetch(active_ref).await;
    assert_eq!(stale["result"]["isError"], true);
    assert_eq!(
        stale["result"]["structuredContent"]["error"], "stale_ref",
        "{stale}"
    );
    service.stop().await;
}

#[tokio::test]
async fn web_denies_untrusted_writes_and_bad_batches_preserve_the_published_version() {
    let root = tempfile::tempdir().unwrap();
    let service = Service::start(root.path(), None, None, FusionMode::Grouped).await;
    let empty_audit = service.audit_records();
    for employee in ["1234567", "0000123８", " 0001234"] {
        let response = send(
            service
                .web(Method::POST, "/api/session", None)
                .json(&json!({"employee": employee})),
        )
        .await;
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        web_error(response, StatusCode::BAD_REQUEST, "invalid_input").await;
    }
    assert_eq!(service.audit_records(), empty_audit);
    let cookie = service.login(EMPLOYEE).await;
    assert_eq!(
        body(
            send(service.web(Method::GET, "/api/session", Some(&cookie))).await,
            StatusCode::OK
        )
        .await,
        json!({"employee": EMPLOYEE}),
    );
    let registration = service.audit_records();
    assert_eq!(registration.len(), 1);
    let registered = registration.values().next().unwrap();
    assert_eq!(registered["employee"], EMPLOYEE);
    assert_eq!(registered["employee_self_reported"], true);
    assert_eq!(registered["action"], "register");
    assert_eq!(registered["outcome"], "succeeded");

    assert_status(
        &send(service.web(Method::GET, "/api/session", None)).await,
        StatusCode::UNAUTHORIZED,
    );
    assert_status(
        &send(
            service
                .web(Method::POST, "/api/search", None)
                .json(&fixture_query()),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    let referer = format!("{}/?from=workbench", service.server.origin);
    assert_status(
        &send(
            service
                .request(Method::POST, "/api/libraries")
                .header(header::REFERER, &referer)
                .json(&json!({"name": "without-session"})),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    for headers in [
        &[][..],
        &[("referer", "/")],
        &[("referer", FOREIGN_ORIGIN)],
        &[("referer", &referer), ("referer", &referer)],
        &[("origin", "null"), ("referer", &referer)],
        &[("origin", FOREIGN_ORIGIN), ("referer", &referer)],
        &[
            ("origin", service.server.origin.as_str()),
            ("origin", service.server.origin.as_str()),
            ("referer", &referer),
        ],
    ] {
        let mut request = service
            .request(Method::POST, "/api/libraries")
            .header(header::COOKIE, &cookie)
            .json(&json!({"name": "untrusted-source"}));
        for &(name, value) in headers {
            request = request.header(name, value);
        }
        assert_status(&send(request).await, StatusCode::FORBIDDEN);
    }
    let injection = send(
        service
            .web(Method::POST, "/api/libraries", Some(&cookie))
            .header(header::REFERER, FOREIGN_ORIGIN)
            .json(&json!({"name": "injected", "employee": "99999999"})),
    )
    .await;
    web_error(injection, StatusCode::BAD_REQUEST, "invalid_input").await;
    assert_eq!(service.libraries(&cookie).await, json!([]));
    assert_eq!(service.audit_records(), registration);

    // Make audit persistence fail even when running as an administrator: a
    // non-directory at the managed audit path cannot contain a start record.
    let audit = service.catalog.root_path().join("audit");
    let saved_audit = audit.with_file_name("previous-audit");
    fs::rename(&audit, &saved_audit).unwrap();
    fs::write(&audit, "not a directory").unwrap();
    let denied_registration = send(
        service
            .web(Method::POST, "/api/session", None)
            .json(&json!({"employee": "12345678"})),
    )
    .await;
    assert!(
        !denied_registration
            .headers()
            .contains_key(header::SET_COOKIE)
    );
    web_error(
        denied_registration,
        StatusCode::SERVICE_UNAVAILABLE,
        "storage",
    )
    .await;
    let denied_create = send(
        service
            .web(Method::POST, "/api/libraries", Some(&cookie))
            .json(&json!({"name": "no-audit"})),
    )
    .await;
    assert!(!denied_create.headers().contains_key("x-operation-id"));
    web_error(denied_create, StatusCode::SERVICE_UNAVAILABLE, "storage").await;
    fs::remove_file(&audit).unwrap();
    fs::rename(saved_audit, audit).unwrap();
    assert_eq!(service.libraries(&cookie).await, json!([]));
    assert_eq!(service.audit_records(), registration);

    let created = send(
        service
            .request(Method::POST, "/api/libraries")
            .header(header::REFERER, &referer)
            .header(header::COOKIE, &cookie)
            .json(&json!({"name": LIBRARY})),
    )
    .await;
    assert_status(&created, StatusCode::CREATED);
    service.publish_fixture(&cookie).await;
    let query = fixture_query();
    let published = service.web_search(&cookie, &query, StatusCode::OK).await;
    assert_fixture_evidence(&published);
    let index = service.index_path();
    for (bad_path, bad_file, reason) in [
        ("../escape.md", &b"legitimate syntax"[..], "segments"),
        (SOURCE_PATH, &b"duplicate content"[..], "duplicate path"),
        ("bad.md", &b"invalid UTF-8 \xff"[..], "file must be UTF-8"),
    ] {
        let response = service
            .upload(
                &cookie,
                &[
                    (SOURCE_PATH, b"std::vector replacementmarker"),
                    (bad_path, bad_file),
                ],
            )
            .await;
        let id = response.headers()["x-operation-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let error = web_error(response, StatusCode::BAD_REQUEST, "invalid_input").await;
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(reason), "{error}");
        assert!(message.contains("document 2"), "{error}");
        let rejected = service.operation(&cookie, &id).await;
        assert_eq!(rejected["outcome"], "failed", "{rejected}");
        assert_eq!(rejected["employee"], EMPLOYEE, "{rejected}");
        assert_eq!(
            service.index_path(),
            index,
            "invalid upload replaced the published version"
        );
        assert_eq!(
            service.web_search(&cookie, &query, StatusCode::OK).await,
            published
        );
    }
    assert!(
        service
            .catalog
            .stage_root()
            .read_dir()
            .unwrap()
            .next()
            .is_none()
    );
    let records = service.audit_records();
    for (name, record) in &records {
        if name.ends_with(".start.json") || name.ends_with(".registration.json") {
            assert_eq!(record["employee"], EMPLOYEE, "{record}");
            assert_eq!(record["employee_self_reported"], true, "{record}");
        }
    }
    for action in ["create", "import", "search"] {
        assert!(
            records
                .values()
                .any(|record| record["action"] == action && record["library"] == LIBRARY),
            "no audit record for {action}: {records:?}"
        );
    }
    let serialized = serde_json::to_string(&records).unwrap();
    for private in [
        "99999999",
        "std::vector",
        "replacementmarker",
        "invalid UTF-8",
        cookie.as_str(),
        cookie.split_once('=').unwrap().1,
    ] {
        assert!(
            !serialized.contains(private),
            "audit retained private input: {private}"
        );
    }
    service.stop().await;
}

#[tokio::test]
async fn equivalent_requests_report_their_own_rerank_success_or_degradation() {
    let root = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let model =
        Loopback::spawn(
            TcpListener::bind("127.0.0.1:0").await.unwrap(),
            Router::new()
                .route(
                    "/v1/rerank",
                    post(
                        |State(requests): State<Arc<Mutex<Vec<Value>>>>,
                         Json(request): Json<Value>| async move {
                            let mut requests = requests.lock().unwrap();
                            requests.push(request);
                            if requests.len() == 1 {
                                return StatusCode::SERVICE_UNAVAILABLE.into_response();
                            }
                            Json(json!({"results": [{"index": 0, "relevance_score": 0.8}]}))
                                .into_response()
                        },
                    ),
                )
                .with_state(requests.clone()),
            CancellationToken::new(),
        );
    let rerank = Arc::new(
        Rerank::new(ApiConfig {
            base_url: model.origin.clone(),
            model: "fixture-rerank".to_owned(),
            api_key: None,
        })
        .unwrap(),
    );
    let service = Service::start(root.path(), None, Some(rerank), FusionMode::Grouped).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    service.publish_fixture(&cookie).await;
    let mut mcp = Mcp::connect(&service).await;
    let query = fixture_query();
    let fallback = service.web_search(&cookie, &query, StatusCode::OK).await;
    let found = mcp.search(&query).await;
    assert_mcp_fixture(&found);
    assert_eq!(
        successful_tool(mcp.fetch(&found["matches"][0]["ref"]).await)["text"],
        SOURCE
    );
    assert_fixture_evidence(&fallback);
    assert_eq!(fallback["reranked"], false);
    let reranked = service.web_search(&cookie, &query, StatusCode::OK).await;
    assert_fixture_evidence(&reranked);
    assert_eq!(reranked["reranked"], true);
    let expected =
        json!({"model": "fixture-rerank", "query": "std::vector", "documents": [SOURCE]});
    assert_eq!(
        *requests.lock().unwrap(),
        [expected.clone(), expected.clone(), expected]
    );
    service.stop().await;
    model.stop().await;
}

#[tokio::test]
async fn web_and_mcp_report_embedding_fallback_and_reopen_without_optional_metadata() {
    let root = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let model = Loopback::spawn(
        TcpListener::bind("127.0.0.1:0").await.unwrap(),
        Router::new()
            .route(
                "/v1/embeddings",
                post(
                    |State(requests): State<Arc<Mutex<Vec<Value>>>>,
                     Json(request): Json<Value>| async move {
                        let mut requests = requests.lock().unwrap();
                        requests.push(request);
                        if requests.len() == 2 {
                            return StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        Json(json!({"model": "fixture", "data": [{"index": 0, "embedding": [1.0, 0.0]}]}))
                            .into_response()
                    },
                ),
            )
            .with_state(requests.clone()),
        CancellationToken::new(),
    );
    let embedding = Arc::new(
        Embedding::new(
            ApiConfig {
                base_url: model.origin.clone(),
                model: "fixture".to_owned(),
                api_key: None,
            },
            2,
        )
        .unwrap(),
    );
    let service =
        Service::start(root.path(), Some(embedding.clone()), None, FusionMode::Edge).await;
    let cookie = service.login(EMPLOYEE).await;
    service.create(&cookie).await;
    service.publish_fixture(&cookie).await;
    let metadata = service.index_path().join(EMBEDDING_GENERATION);
    let mut mcp = Mcp::connect(&service).await;
    let query = fixture_query();
    let fallback = service.web_search(&cookie, &query, StatusCode::OK).await;
    assert_fixture_evidence(&fallback);
    let mut hybrid = fallback.clone();
    hybrid["retrieval"] = json!("hybrid");
    let found = mcp.search(&query).await;
    assert_mcp_fixture(&found);
    assert_eq!(
        successful_tool(mcp.fetch(&found["matches"][0]["ref"]).await)["text"],
        SOURCE
    );
    assert_eq!(
        service.web_search(&cookie, &query, StatusCode::OK).await,
        hybrid
    );
    service.stop().await;

    // First damage the sealed metadata; then remove it entirely. Both
    // reopen paths must retain the same published keyword evidence.
    fs::write(&metadata, b"not JSON").unwrap();
    for missing in [false, true] {
        if missing {
            fs::remove_file(&metadata).unwrap();
        }
        let service =
            Service::start(root.path(), Some(embedding.clone()), None, FusionMode::Edge).await;
        let cookie = service.login(EMPLOYEE).await;
        let mut mcp = Mcp::connect(&service).await;
        assert_eq!(
            service.web_search(&cookie, &query, StatusCode::OK).await,
            fallback
        );
        assert_mcp_fixture(&mcp.search(&query).await);
        service.stop().await;
    }
    assert_eq!(
        *requests.lock().unwrap(),
        [SOURCE, "std::vector", "std::vector", "std::vector"]
            .map(|input| json!({"model": "fixture", "input": [input]}))
    );
    model.stop().await;
}
