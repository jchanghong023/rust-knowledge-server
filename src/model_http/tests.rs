use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, Uri, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

use super::*;

#[derive(Debug, PartialEq)]
struct Observed {
    method: Method,
    target: String,
    authorization: Option<String>,
    body: Value,
}

struct Reply {
    status: StatusCode,
    body: &'static str,
    location: Option<&'static str>,
}

impl Reply {
    fn json(body: &'static str) -> Self {
        Self {
            status: StatusCode::OK,
            body,
            location: None,
        }
    }
}

#[derive(Default)]
struct MockState {
    requests: Vec<Observed>,
    replies: VecDeque<Reply>,
}

struct Loopback {
    base_url: String,
    state: Arc<Mutex<MockState>>,
    task: JoinHandle<()>,
}

impl Loopback {
    async fn start(replies: impl IntoIterator<Item = Reply>) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            replies: replies.into_iter().collect(),
            ..MockState::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .fallback(respond)
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url,
            state,
            task,
        }
    }

    fn config(&self, path: &str, model: &str, api_key: Option<&str>) -> ApiConfig {
        ApiConfig {
            base_url: format!("{}{path}", self.base_url),
            model: model.to_owned(),
            api_key: api_key.map(str::to_owned),
        }
    }

    fn requests(&self) -> Vec<Observed> {
        std::mem::take(&mut self.state.lock().unwrap().requests)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond(
    State(state): State<Arc<Mutex<MockState>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut state = state.lock().unwrap();
    state.requests.push(Observed {
        method,
        target: uri.to_string(),
        authorization: headers
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap().to_owned()),
        body,
    });
    let Some(reply) = state.replies.pop_front() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut response = (
        reply.status,
        [(header::CONTENT_TYPE, "application/json")],
        reply.body,
    )
        .into_response();
    if let Some(location) = reply.location {
        response
            .headers_mut()
            .insert(header::LOCATION, header::HeaderValue::from_static(location));
    }
    response
}

#[tokio::test]
async fn posts_only_the_configured_wire_requests_and_maps_results_by_index() {
    let server = Loopback::start([
        Reply::json(r#"{"model":"embed-test","data":[{"index":1,"embedding":[0,2]},{"index":0,"embedding":[3,0]}]}"#),
        Reply::json(r#"{"results":[{"index":1,"relevance_score":2.75},{"index":0,"relevance_score":-0.25}]}"#),
    ])
    .await;
    let embedding =
        Embedding::new(server.config("/v1/", "embed-test", Some("test-key")), 2).unwrap();
    let rerank = Rerank::new(server.config("", "rerank-test", None)).unwrap();
    assert_eq!(embedding.model(), "embed-test");
    assert_eq!(embedding.dimensions(), 2);
    assert!(embedding.embed(&[]).await.unwrap().is_empty());
    assert!(rerank.scores("ignored", &[]).await.unwrap().is_empty());
    assert!(server.requests().is_empty());

    assert_eq!(
        embedding.embed(&["doc one", "doc two"]).await.unwrap(),
        [vec![3.0, 0.0], vec![0.0, 2.0]]
    );
    assert_eq!(
        rerank
            .scores("test query", &["doc two", "doc one"])
            .await
            .unwrap(),
        [-0.25, 2.75]
    );
    assert_eq!(
        server.requests(),
        [
            Observed {
                method: Method::POST,
                target: "/v1/embeddings".into(),
                authorization: Some("Bearer test-key".into()),
                body: json!({"model":"embed-test","input":["doc one","doc two"]}),
            },
            Observed {
                method: Method::POST,
                target: "/v1/rerank".into(),
                authorization: None,
                body: json!({"model":"rerank-test","query":"test query","documents":["doc two","doc one"]}),
            },
        ]
    );
}

#[tokio::test]
async fn rejects_incomplete_mismatched_or_invalid_embedding_replies() {
    let cases = [
        (
            r#"{"model":"wrong","data":[{"index":0,"embedding":[1,0]}]}"#,
            ModelError::ModelMismatch,
        ),
        (
            r#"{"model":null,"data":[{"index":0,"embedding":[1,0]}]}"#,
            ModelError::InvalidReply,
        ),
        (r#"{"data":[]}"#, ModelError::InvalidResultCount),
        (
            r#"{"data":[{"index":1,"embedding":[1,0]}]}"#,
            ModelError::InvalidIndex,
        ),
        (
            r#"{"data":[{"index":0,"embedding":[]}]}"#,
            ModelError::InvalidVector,
        ),
        (
            r#"{"data":[{"index":0,"embedding":[1]}]}"#,
            ModelError::InvalidVector,
        ),
        (
            r#"{"data":[{"index":0,"embedding":[0,0]}]}"#,
            ModelError::InvalidVector,
        ),
        (
            r#"{"data":[{"index":0,"embedding":[1e999,1]}]}"#,
            ModelError::InvalidReply,
        ),
        (
            r#"{"data":[{"index":0,"embedding":[1e39,1]}]}"#,
            ModelError::InvalidReply,
        ),
        (
            r#"{"data":[{"index":0,"embedding":[1e-99,1]}]}"#,
            ModelError::InvalidReply,
        ),
    ];
    let server = Loopback::start(cases.iter().map(|(body, _)| Reply::json(body)).chain([
        Reply::json(r#"{"data":[{"index":0,"embedding":[1,0]},{"index":0,"embedding":[0,1]}]}"#),
        Reply::json(r#"{"data":[{"index":0,"embedding":[1,0]}]}"#),
    ]))
    .await;
    let client = Embedding::new(server.config("", "embed-test", None), 2).unwrap();
    for (_, expected) in cases {
        assert_eq!(client.embed(&["doc"]).await, Err(expected));
    }
    assert_eq!(
        client.embed(&["first", "second"]).await,
        Err(ModelError::InvalidIndex)
    );
    assert_eq!(client.embed(&["doc"]).await.unwrap(), [vec![1.0, 0.0]]);
}

#[tokio::test]
async fn rejects_incomplete_mismatched_or_invalid_rerank_replies() {
    let cases = [
        (
            r#"{"model":"wrong","results":[{"index":0,"relevance_score":8},{"index":1,"relevance_score":6}]}"#,
            ModelError::ModelMismatch,
        ),
        (
            r#"{"results":[{"index":1,"relevance_score":6}]}"#,
            ModelError::InvalidResultCount,
        ),
        (
            r#"{"results":[{"index":0,"relevance_score":8},{"index":0,"relevance_score":6}]}"#,
            ModelError::InvalidIndex,
        ),
        (
            r#"{"results":[{"index":0,"relevance_score":8},{"index":2,"relevance_score":6}]}"#,
            ModelError::InvalidIndex,
        ),
        (
            r#"{"results":[{"index":0,"relevance_score":NaN},{"index":1,"relevance_score":6}]}"#,
            ModelError::InvalidReply,
        ),
        (
            r#"{"results":[{"index":0,"relevance_score":1e999},{"index":1,"relevance_score":6}]}"#,
            ModelError::InvalidReply,
        ),
    ];
    let server = Loopback::start(cases.iter().map(|(body, _)| Reply::json(body))).await;
    let client = Rerank::new(server.config("/v1", "rerank-test", None)).unwrap();
    for (_, expected) in cases {
        assert_eq!(
            client.scores("query", &["first", "second"]).await,
            Err(expected)
        );
    }
}

#[tokio::test]
async fn does_not_follow_redirects_or_expose_reply_contents() {
    let server = Loopback::start([
        Reply {
            status: StatusCode::TEMPORARY_REDIRECT,
            body: "not for diagnostics",
            location: Some("/unexpected"),
        },
        Reply {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: "private query and key",
            location: None,
        },
    ])
    .await;
    let client = Rerank::new(server.config("", "rerank-test", Some("secret-token"))).unwrap();
    assert_eq!(
        client.scores("sensitive query", &["doc"]).await,
        Err(ModelError::Status(StatusCode::TEMPORARY_REDIRECT))
    );
    let error = client
        .scores("sensitive query", &["doc"])
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "model API returned HTTP 503");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.target == "/v1/rerank")
    );
}

#[test]
fn accepts_explicit_http_addresses_but_rejects_ambiguous_or_credentialed_urls() {
    let endpoint = model_endpoint("https://[::1]:9000/models/v1", "rerank").unwrap();
    assert_eq!(endpoint.as_str(), "https://[::1]:9000/models/v1/rerank");
    for url in [
        "https://user:password@localhost/v1",
        "https://@localhost/v1",
        "https://localhost/v1?key=secret",
        "https://localhost/v1#fragment",
        "file:///tmp/models",
        "http:localhost/v1",
        "http:///localhost/v1",
        "localhost:9000/v1",
    ] {
        assert_eq!(
            model_endpoint(url, "rerank"),
            Err(ModelError::InvalidEndpoint)
        );
    }
}
