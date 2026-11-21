use std::sync::{
    Mutex,
    atomic::{AtomicU8, AtomicUsize, Ordering::Relaxed},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{FromRequest, Multipart, State},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, runtime::Handle, sync::Notify, task::JoinHandle};

use super::*;
use crate::{data_root::Root, import, index, model_http::ApiConfig};

const BOUNDARY: &str = "standalone-search-fixture";
const SEMANTIC_QUERY: &str = "heliosphere";
const EXACT_BODY: &str =
    "\u{feff}# 完整示例\r\n```cpp\r\n# not a heading\r\nC++ and std::vector\r\n```\r\n";
const NEAR_BODY: &str = "C++17 and std::vector2\r\n";

struct Fixture {
    catalog: Arc<Catalog>,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(Root::open(root.path()).unwrap()).unwrap();
        catalog.create("docs", None).unwrap();
        Self {
            catalog: Arc::new(catalog),
            _root: root,
        }
    }

    async fn publish(&self, name: &str, docs: &[(&str, &str)], embedding: Option<Arc<Embedding>>) {
        let mut body = Vec::new();
        for (path, text) in docs {
            write!(&mut body, "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n{path}\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.md\"\r\n\r\n{text}\r\n").unwrap();
        }
        write!(&mut body, "--{BOUNDARY}--\r\n").unwrap();
        let request = Request::post("/import")
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap();
        let fields = Multipart::from_request(request, &()).await.unwrap();
        let batch = import::stage(fields, self.catalog.stage_root())
            .await
            .unwrap();
        let build = self.catalog.start_build(name).unwrap();
        let runtime = Handle::current();
        let catalog = Arc::clone(&self.catalog);
        tokio::task::spawn_blocking(move || {
            let error = index::build(
                &build.index_path(),
                build.version_id(),
                &batch,
                embedding.as_deref(),
                &runtime,
                |_, _| {},
            )
            .unwrap();
            assert!(error.is_none());
            catalog.publish(build, None).unwrap();
        })
        .await
        .unwrap();
    }
}

fn request(query: &str, library: Option<&str>, path: Option<&str>) -> SearchRequest {
    SearchRequest {
        query: query.into(),
        library: library.map(str::to_owned),
        path: path.map(str::to_owned),
    }
}

fn paths(reply: &SearchReply) -> Vec<&str> {
    reply
        .matches
        .iter()
        .map(|item| item.path.as_str())
        .collect()
}

#[tokio::test]
async fn real_index_preserves_technical_evidence_and_distinguishes_empty_states() {
    let fixture = Fixture::new();
    let catalog = &fixture.catalog;
    let empty = search(
        catalog,
        request("C++", Some("docs"), None),
        None,
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(empty.status, Status::NoPublishedIndex);
    assert_eq!(
        search(
            catalog,
            request("C++", Some("missing"), None),
            None,
            None,
            FusionMode::Grouped
        )
        .await
        .unwrap_err(),
        SearchError::LibraryNotFound
    );
    for invalid in [
        request(" \u{feff}", None, None),
        request("C++", None, Some("../guide")),
    ] {
        assert!(matches!(
            search(catalog, invalid, None, None, FusionMode::Grouped).await,
            Err(SearchError::InvalidInput(_))
        ));
    }

    // Make a misleading partial suffix at a long-window boundary.
    let oversized = format!("{}xC++ {}", " ".repeat(17_499), "y".repeat(1000));
    fixture
        .publish(
            "docs",
            &[
                ("guide/old/two.md", NEAR_BODY),
                ("guide/v1/ref.md", EXACT_BODY),
                ("guide/v2/ref.md", EXACT_BODY),
                ("guide/long.md", &oversized),
                ("guides/ref.md", EXACT_BODY),
                (
                    "guide/operators.md",
                    "quartz separates OR literals and mica follows",
                ),
            ],
            None,
        )
        .await;
    let expected_sha = Sha256::digest(EXACT_BODY.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    for query in ["`c++`", "std::VECTOR"] {
        let reply = search(
            catalog,
            request(query, None, Some("guide")),
            None,
            None,
            FusionMode::Grouped,
        )
        .await
        .unwrap();
        assert_eq!(reply.status, Status::Ok);
        assert_eq!(reply.retrieval, Retrieval::Keyword);
        assert!(!reply.reranked);
        let exact = reply
            .matches
            .iter()
            .filter(|item| item.category == Category::Exact)
            .collect::<Vec<_>>();
        assert_eq!(
            exact
                .iter()
                .map(|item| item.path.as_str())
                .collect::<Vec<_>>(),
            ["guide/v1/ref.md", "guide/v2/ref.md"]
        );
        assert_eq!(reply.matches[0].category, Category::Exact);
        assert!(
            reply
                .matches
                .iter()
                .all(|item| item.path.starts_with("guide/"))
        );
        for item in exact {
            assert_eq!(item.library, "docs");
            assert_eq!(item.text, EXACT_BODY);
            assert_eq!((item.line_start, item.line_end), (1, 5));
            assert_eq!(item.sha256, expected_sha);
        }
    }
    for (query, path, expected) in [
        ("完整示例", "guide/v2/ref.md", "guide/v2/ref.md"),
        ("\"quartz mica\" OR *", "guide", "guide/operators.md"),
    ] {
        let reply = search(
            catalog,
            request(query, Some("docs"), Some(path)),
            None,
            None,
            FusionMode::Grouped,
        )
        .await
        .unwrap();
        assert!(paths(&reply).contains(&expected));
    }
    let missing_path = search(
        catalog,
        request("C++", None, Some("guide/v3/ref.md")),
        None,
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(missing_path.status, Status::Ok);
    assert!(missing_path.matches.is_empty());
}

#[tokio::test]
async fn grouped_recall_keeps_answer_evidence_from_other_files() {
    let fixture = Fixture::new();
    let repeated = (0..BRANCH_LIMIT + 12)
        .map(|n| format!("## Entry {n}\n\nVXR7310 quartz calibration.\n\n"))
        .collect::<String>();
    let answer = "VXR7310 quartz calibration: connect the frequency counter, wait for temperature stabilization, then adjust the oscillator to the reference frequency.";
    fixture
        .publish(
            "docs",
            &[("01-reference.md", &repeated), ("02-procedure.md", answer)],
            None,
        )
        .await;

    for (query, category) in [
        ("VXR7310", Category::Exact),
        ("quartz calibration", Category::Related),
    ] {
        let reply = search(
            &fixture.catalog,
            request(query, Some("docs"), None),
            None,
            None,
            FusionMode::Grouped,
        )
        .await
        .unwrap();
        assert!(
            reply.matches.iter().take(5).any(|hit| {
                hit.path == "02-procedure.md" && hit.text == answer && hit.category == category
            }),
            "{query}: repeated entries buried the procedure: {:?}",
            paths(&reply)
        );
    }
}

#[tokio::test]
async fn scoped_bm25_rank_is_unaffected_by_outside_document_frequency_and_budget() {
    let fixture = Fixture::new();
    let in_scope = [
        ("inside/z.md", "amber amber talc cinder"),
        ("inside/a.md", "zircon marble granite chalk"),
    ];
    fixture.publish("docs", &in_scope, None).await;
    let query = || request("amber zircon", Some("docs"), Some("inside"));
    let before = search(&fixture.catalog, query(), None, None, FusionMode::Grouped)
        .await
        .unwrap();
    assert_eq!(paths(&before), ["inside/z.md", "inside/a.md"]);

    let outside = (0..BRANCH_LIMIT + 4)
        .map(|i| format!("insider/{i}.md"))
        .collect::<Vec<_>>();
    let extended = outside
        .iter()
        .map(|path| (path.as_str(), "amber amber amber amber amber amber"))
        .chain(in_scope)
        .collect::<Vec<_>>();
    fixture.publish("docs", &extended, None).await;
    let after = search(&fixture.catalog, query(), None, None, FusionMode::Grouped)
        .await
        .unwrap();
    // Reimport assigns new internal point and version IDs, not new evidence.
    assert_eq!(
        serde_json::to_value(&after.matches).unwrap(),
        serde_json::to_value(&before.matches).unwrap()
    );
}

#[derive(Default)]
struct ModelState {
    embed_queries: AtomicUsize,
    embed_mode: AtomicU8,
    rerank_mode: AtomicU8,
    rerank_documents: Mutex<Vec<Vec<String>>>,
    submitted: Notify,
    release: Notify,
}

struct Loopback {
    addr: std::net::SocketAddr,
    state: Arc<ModelState>,
    task: JoinHandle<()>,
}

impl Loopback {
    async fn start() -> Self {
        let state = Arc::new(ModelState::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/embeddings", post(mock_embed))
            .route("/v1/rerank", post(mock_rerank))
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state, task }
    }

    fn config(&self, model: &str) -> ApiConfig {
        ApiConfig {
            base_url: format!("http://{}/v1", self.addr),
            model: model.into(),
            api_key: None,
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_embed(State(state): State<Arc<ModelState>>, Json(body): Json<Value>) -> Response {
    let inputs = body["input"].as_array().unwrap();
    let query = inputs.len() == 1 && matches!(inputs[0].as_str(), Some(SEMANTIC_QUERY | "C++"));
    let invalid = if query {
        state.embed_queries.fetch_add(1, Relaxed);
        match state.embed_mode.load(Relaxed) {
            1 => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            2 => true,
            3 => {
                state.submitted.notify_one();
                state.release.notified().await;
                false
            }
            _ => false,
        }
    } else {
        false
    };
    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let vector = match text.as_str().unwrap() {
                _ if invalid => [0.0_f32, 0.0],
                SEMANTIC_QUERY | "C++" | "outside neighbor" => [1.0, 0.0],
                _ => [1.0, 1.0],
            };
            json!({"index": index, "embedding": vector})
        })
        .collect::<Vec<_>>();
    Json(json!({"data": data})).into_response()
}

async fn mock_rerank(State(state): State<Arc<ModelState>>, Json(body): Json<Value>) -> Response {
    let documents = body["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let mode = state.rerank_mode.load(Relaxed);
    let count = documents.len();
    state.rerank_documents.lock().unwrap().push(documents);
    if mode == 1 {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let results = (0..count)
        .rev()
        .map(|index| {
            json!({
                "index": if mode == 2 { index + count } else { index },
                "relevance_score": if mode == 3 { 1 } else { index },
            })
        })
        .collect::<Vec<_>>();
    Json(json!({"results": results})).into_response()
}

#[tokio::test]
async fn hybrid_uses_eligible_pinned_versions_and_scoped_dense_recall() {
    let fixture = Fixture::new();
    let server = Loopback::start().await;
    let provider = Arc::new(Embedding::new(server.config("vectors"), 2).unwrap());
    let outside = (0..BRANCH_LIMIT + 4)
        .map(|i| format!("outside/{i}.md"))
        .collect::<Vec<_>>();
    let docs = outside
        .iter()
        .map(|path| (path.as_str(), "outside neighbor"))
        .chain([
            ("inside/a.md", "solar wind plasma"),
            ("inside/b.md", "solar wind plasma"),
            ("inside/keyword.md", SEMANTIC_QUERY),
        ])
        .collect::<Vec<_>>();
    fixture
        .publish("docs", &docs, Some(Arc::clone(&provider)))
        .await;
    fixture.catalog.create("plain", None).unwrap();
    fixture
        .publish("plain", &[("inside/plain.md", SEMANTIC_QUERY)], None)
        .await;

    let query = || request(SEMANTIC_QUERY, None, Some("inside"));
    let keyword = search(&fixture.catalog, query(), None, None, FusionMode::Grouped)
        .await
        .unwrap();
    assert_eq!(
        server.state.embed_queries.load(Relaxed),
        1,
        "only the literal document used this input during import"
    );
    assert!(!paths(&keyword).contains(&"inside/a.md"));
    let hybrid = search(
        &fixture.catalog,
        query(),
        Some(Arc::clone(&provider)),
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(hybrid.retrieval, Retrieval::Hybrid);
    let mut found = paths(&hybrid);
    found.sort_unstable();
    assert_eq!(
        found,
        [
            "inside/a.md",
            "inside/b.md",
            "inside/keyword.md",
            "inside/plain.md"
        ]
    );
    let native = search(
        &fixture.catalog,
        query(),
        Some(Arc::clone(&provider)),
        None,
        FusionMode::Edge,
    )
    .await
    .unwrap();
    assert_eq!(native.retrieval, Retrieval::Hybrid);
    let mut native_paths = paths(&native);
    native_paths.sort_unstable();
    assert_eq!(native_paths, found);

    let mismatched = Arc::new(Embedding::new(server.config("different model"), 2).unwrap());
    let before = server.state.embed_queries.load(Relaxed);
    let unqualified = search(
        &fixture.catalog,
        query(),
        Some(mismatched),
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(unqualified.retrieval, Retrieval::Keyword);
    assert_eq!(unqualified.matches, keyword.matches);
    assert_eq!(server.state.embed_queries.load(Relaxed), before);
    let no_scope = search(
        &fixture.catalog,
        request(SEMANTIC_QUERY, None, Some("unmatched")),
        Some(Arc::clone(&provider)),
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(no_scope.retrieval, Retrieval::Keyword);
    assert_eq!(server.state.embed_queries.load(Relaxed), before);

    server.state.embed_mode.store(3, Relaxed);
    let catalog = Arc::clone(&fixture.catalog);
    let search_provider = Arc::clone(&provider);
    let mut running = tokio::spawn(async move {
        search(
            &catalog,
            query(),
            Some(search_provider),
            None,
            FusionMode::Grouped,
        )
        .await
        .unwrap()
    });
    tokio::select! {
        () = server.state.submitted.notified() => {},
        result = &mut running => panic!("search finished before the embedding request: {result:?}"),
    }
    fixture
        .publish("docs", &[("inside/new.md", SEMANTIC_QUERY)], None)
        .await;
    server.state.release.notify_one();
    let pinned = running.await.unwrap();
    assert_eq!(pinned.matches, hybrid.matches);
    let calls = server.state.embed_queries.load(Relaxed);
    let next = search(
        &fixture.catalog,
        query(),
        Some(provider),
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(next.retrieval, Retrieval::Keyword);
    assert!(paths(&next).contains(&"inside/new.md"));
    assert!(!paths(&next).contains(&"inside/a.md"));
    assert_eq!(server.state.embed_queries.load(Relaxed), calls);
}

#[tokio::test]
async fn rerank_reorders_only_its_window_and_survives_embedding_degradation() {
    let fixture = Fixture::new();
    let server = Loopback::start().await;
    let provider = Arc::new(Embedding::new(server.config("vectors"), 2).unwrap());
    let rerank = Rerank::new(server.config("rerank")).unwrap();
    let names = (0..RERANK_LIMIT + 2)
        .map(|i| format!("bulk/{i:02}.md"))
        .collect::<Vec<_>>();
    let mut docs = names
        .iter()
        .map(|name| (name.as_str(), "C++ zircon amber"))
        .collect::<Vec<_>>();
    docs.extend([
        ("mix/ref.md", EXACT_BODY),
        ("mix/near.md", NEAR_BODY),
        ("mix/semantic.md", "model-only related passage"),
    ]);
    fixture
        .publish("docs", &docs, Some(Arc::clone(&provider)))
        .await;
    let query = || request("C++", Some("docs"), None);

    let keyword = search(&fixture.catalog, query(), None, None, FusionMode::Grouped)
        .await
        .unwrap();
    let keyword_ranked = search(
        &fixture.catalog,
        query(),
        None,
        Some(&rerank),
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert!(keyword_ranked.reranked);
    assert_eq!(keyword_ranked.retrieval, Retrieval::Keyword);
    assert_eq!(keyword.matches[0].category, Category::Exact);
    let mut expected = paths(&keyword);
    let len = expected.len().min(RERANK_LIMIT);
    let exact = keyword
        .matches
        .iter()
        .take(len)
        .take_while(|item| item.category == Category::Exact)
        .count();
    expected[..exact].reverse();
    expected[exact..len].reverse();
    assert_eq!(paths(&keyword_ranked), expected);
    assert_ne!(keyword_ranked.matches, keyword.matches);
    assert_eq!(
        keyword_ranked.matches[RERANK_LIMIT..],
        keyword.matches[RERANK_LIMIT..]
    );
    for item in &keyword_ranked.matches {
        assert!(keyword.matches.contains(item));
    }

    let scoped_query = || request("C++", Some("docs"), Some("mix"));
    let hybrid = search(
        &fixture.catalog,
        scoped_query(),
        Some(Arc::clone(&provider)),
        None,
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    let reranked_hybrid = search(
        &fixture.catalog,
        scoped_query(),
        Some(Arc::clone(&provider)),
        Some(&rerank),
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert_eq!(reranked_hybrid.retrieval, Retrieval::Hybrid);
    assert!(reranked_hybrid.reranked);
    assert_eq!(reranked_hybrid.matches[0].category, Category::Exact);
    assert_eq!(
        hybrid
            .matches
            .iter()
            .find(|item| item.path == "mix/near.md")
            .unwrap()
            .category,
        Category::Related
    );
    let hybrid_paths = paths(&hybrid);
    assert_eq!(
        paths(&reranked_hybrid),
        [hybrid_paths[0], hybrid_paths[2], hybrid_paths[1]]
    );
    assert_eq!(
        server
            .state
            .rerank_documents
            .lock()
            .unwrap()
            .last()
            .unwrap(),
        &hybrid
            .matches
            .iter()
            .take(RERANK_LIMIT)
            .map(|item| item.text.clone())
            .collect::<Vec<_>>()
    );

    for mode in [1, 2, 3] {
        server.state.rerank_mode.store(mode, Relaxed);
        let reply = search(
            &fixture.catalog,
            query(),
            None,
            Some(&rerank),
            FusionMode::Grouped,
        )
        .await
        .unwrap();
        assert_eq!(reply.reranked, mode == 3);
        assert_eq!(reply.matches, keyword.matches);
    }
    server.state.rerank_mode.store(0, Relaxed);
    for mode in [1, 2] {
        server.state.embed_mode.store(mode, Relaxed);
        let degraded = search(
            &fixture.catalog,
            query(),
            Some(Arc::clone(&provider)),
            Some(&rerank),
            FusionMode::Grouped,
        )
        .await
        .unwrap();
        assert_eq!(degraded.retrieval, Retrieval::Keyword);
        assert!(degraded.reranked);
        assert_eq!(degraded.matches, keyword_ranked.matches);
    }
    let calls = server.state.rerank_documents.lock().unwrap().len();
    let no_hits = search(
        &fixture.catalog,
        request("absent", Some("docs"), Some("missing")),
        None,
        Some(&rerank),
        FusionMode::Grouped,
    )
    .await
    .unwrap();
    assert!(!no_hits.reranked);
    assert_eq!(server.state.rerank_documents.lock().unwrap().len(), calls);
}
