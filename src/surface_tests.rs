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
use tokio::{net::TcpListener, task::JoinHandle, time};
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
                "ref": hit["ref"], "truncated": false
            }]
        })
    );
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
    assert_eq!(
        search["outputSchema"]["properties"]["matches"]["maxItems"],
        5
    );
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
        .tool("knowledge_search", &json!({"query": "vector", "limit": 3}))
        .await;
    assert_eq!(unknown["result"]["isError"], true, "{unknown}");
    assert!(
        unknown["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown field `limit`")
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
            "line_start": 1, "line_end": 5, "truncated": false
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
    assert_eq!(hits.len(), 5, "{response}");
    assert!(
        hits.iter()
            .map(|hit| hit["text"].as_str().unwrap().len())
            .sum::<usize>()
            <= 4096
    );
    let mut ranked = web["matches"].as_array().unwrap().iter();
    let mut distinct = std::collections::BTreeSet::new();
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
    assert!(text.contains("left_marker") && text.contains("right_marker") && wide.contains(text));
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
    assert_status(
        &send(
            service
                .web(Method::POST, "/api/libraries", None)
                .json(&json!({"name": "without-session"})),
        )
        .await,
        StatusCode::UNAUTHORIZED,
    );
    let cross_origin = send(
        service
            .request(Method::POST, "/api/libraries")
            .header(header::ORIGIN, FOREIGN_ORIGIN)
            .header(header::COOKIE, &cookie)
            .json(&json!({"name": "cross-origin"})),
    )
    .await;
    assert_status(&cross_origin, StatusCode::FORBIDDEN);
    let injection = send(
        service
            .web(Method::POST, "/api/libraries", Some(&cookie))
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

    service.create(&cookie).await;
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
