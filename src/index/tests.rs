use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, State};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use qdrant_edge::{
    Condition, FieldCondition, Filter, JsonPath, Match, NamedQuery, PointId, QueryEnum,
    QueryRequestBuilder, ScoringQuery, ScrollRequestBuilder, WithPayloadInterface, WithVector,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, timeout};

use super::*;
use crate::model_http::ApiConfig;

#[derive(Deserialize)]
struct Passage {
    text: String,
    path: String,
    line_start: usize,
    line_end: usize,
    sha256: String,
    byte_start: usize,
    exact: Vec<String>,
}

async fn build_fixture(
    root: &Path,
    version: Uuid,
    documents: &[(&str, &str)],
    embedding: Option<Embedding>,
) -> Option<ModelError> {
    const BOUNDARY: &str = "index-test-boundary";
    let mut body = String::new();
    for &(path, text) in documents {
        write!(
            body,
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"path\"\r\n\r\n{path}\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ignored.md\"\r\n\r\n{text}\r\n",
        )
        .unwrap();
    }
    write!(body, "--{BOUNDARY}--\r\n").unwrap();
    let request = Request::post("/")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let multipart = Multipart::from_request(request, &()).await.unwrap();
    let batch = crate::import::stage(multipart, root).await.unwrap();
    let path = root.join("index");
    let runtime = Handle::current();
    tokio::task::spawn_blocking(move || {
        build(
            &path,
            version,
            &batch,
            embedding.as_ref(),
            &runtime,
            |_, _| {},
        )
    })
    .await
    .unwrap()
    .unwrap()
}

fn matching(field: &str, value: &str) -> Condition {
    Condition::Field(FieldCondition::new_match(
        field.parse().unwrap(),
        Match::from(value.to_owned()),
    ))
}

#[tokio::test]
async fn reopens_lossless_markdown_with_scoped_keyword_and_exact_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let first = "\u{feff}# 工艺\r\n```cpp\r\n# inside\r\nC++ std::vector\r\n```\r\n";
    let tail = "# Tail\r\nEnglish AND sentinel\r\n";
    let original = format!("{first}{tail}");
    let paths = ["docs/nested/Guide.MD", "docs2/Guide.MD"];
    assert_eq!(
        build_fixture(
            directory.path(),
            Uuid::new_v4(),
            &[(paths[0], &original), (paths[1], &original)],
            None,
        )
        .await,
        None,
    );
    let index = directory.path().join("index");
    assert!(!index.join(EMBEDDING_GENERATION).exists());
    let shard = EdgeShard::load(&index, None).unwrap();
    let schema = shard.info().unwrap().payload_schema;
    assert!(schema.contains_key(&SCOPE_FIELD.parse::<JsonPath>().unwrap()));
    assert!(schema.contains_key(&EXACT_FIELD.parse::<JsonPath>().unwrap()));
    let (points, _) = shard
        .scroll(
            ScrollRequestBuilder::new()
                .with_payload(WithPayloadInterface::Bool(true))
                .build(),
        )
        .unwrap();
    assert_eq!(points.len(), 4);
    let mut sha256 = String::with_capacity(64);
    for byte in Sha256::digest(original.as_bytes()) {
        write!(sha256, "{byte:02x}").unwrap();
    }
    for (i, point) in points.into_iter().enumerate() {
        assert_eq!(point.id, PointId::from(i as u64 + 1));
        let passage: Passage =
            serde_json::from_value(Value::Object(point.payload.unwrap().0)).unwrap();
        assert_eq!(passage.path, paths[i / 2]);
        assert_eq!(passage.sha256, sha256);
        if i % 2 == 0 {
            assert_eq!(passage.text, first);
            assert_eq!(
                (passage.byte_start, passage.line_start, passage.line_end),
                (0, 1, 5)
            );
            assert!(passage.exact.iter().any(|name| name == "std::vector"));
        } else {
            assert_eq!(passage.text, tail);
            assert_eq!(
                (passage.byte_start, passage.line_start, passage.line_end),
                (first.len(), 6, 7),
            );
        }
    }
    let (exact, _) = shard
        .scroll(
            ScrollRequestBuilder::new()
                .filter(Filter::new_must(matching(EXACT_FIELD, "c++")))
                .build(),
        )
        .unwrap();
    assert_eq!(exact.len(), 2);
    for query in ["工艺", "AND"] {
        let (scope, point) = if query == "工艺" {
            ("docs", 1_u64)
        } else {
            (paths[0], 2_u64)
        };
        let hits = shard
            .query(
                QueryRequestBuilder::new(4)
                    .query(ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery {
                        query: bm25().unwrap().embed_query(query).into(),
                        using: Some(KEYWORD_VECTOR.to_owned()),
                    })))
                    .filter(Filter::new_must(matching(SCOPE_FIELD, scope)))
                    .build(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, PointId::from(point));
    }
}

#[tokio::test]
async fn embedding_generation_requires_a_complete_usable_reply_for_every_passage() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/v1/embeddings",
            post(
                async |State(requests): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>| {
                    let input = body["input"].as_array().unwrap();
                    let mut requests = requests.lock().unwrap();
                    let seen = requests.contains(&body);
                    requests.push(body.clone());
                    if input[0] == "four" && !seen {
                        return StatusCode::TOO_MANY_REQUESTS.into_response();
                    }
                    let rows: Vec<_> = input
                        .iter()
                        .enumerate()
                        .map(|(index, text)| {
                            let embedding = if text == "two" {
                                [1.0e30, 1.0]
                            } else {
                                [1.0, 0.0]
                            };
                            json!({"index": index, "embedding": embedding})
                        })
                        .collect();
                    Json(json!({"data": rows})).into_response()
                },
            ),
        )
        .with_state(Arc::clone(&requests));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = |model: &str, dimensions| {
        Embedding::new(
            ApiConfig {
                base_url: url.clone(),
                model: model.to_owned(),
                api_key: None,
            },
            dimensions,
        )
        .unwrap()
    };

    let partial = tempfile::tempdir().unwrap();
    let version = Uuid::new_v4();
    assert_eq!(
        build_fixture(
            partial.path(),
            version,
            &[("one.md", "one"), ("two.md", "two"), ("three.md", "three")],
            Some(provider("fixture", 2)),
        )
        .await,
        Some(ModelError::InvalidVector),
    );
    let index = partial.path().join("index");
    assert!(!index.join(EMBEDDING_GENERATION).exists());
    let shard = EdgeShard::load(&index, None).unwrap();
    let (records, _) = shard
        .scroll(
            ScrollRequestBuilder::new()
                .with_vector(WithVector::Bool(true))
                .build(),
        )
        .unwrap();
    assert_eq!(records.len(), 3);
    for record in &records {
        assert!(record.get_vector_by_name(KEYWORD_VECTOR).is_some());
    }
    drop(shard);

    let complete = tempfile::tempdir().unwrap();
    let version = Uuid::new_v4();
    assert_eq!(
        build_fixture(
            complete.path(),
            version,
            &[("four.md", "four")],
            Some(provider("fixture", 2))
        )
        .await,
        None,
    );
    let index = complete.path().join("index");
    let generation: EmbeddingGeneration<String> =
        serde_json::from_slice(&fs::read(index.join(EMBEDDING_GENERATION)).unwrap()).unwrap();
    assert!(generation.matches(version, &provider("fixture", 2)));
    assert!(!generation.matches(Uuid::new_v4(), &provider("fixture", 2)));
    assert!(!generation.matches(version, &provider("other", 2)));
    assert!(!generation.matches(version, &provider("fixture", 3)));
    server.abort();
    assert_eq!(
        *requests.lock().unwrap(),
        [
            json!({"model": "fixture", "input": ["one", "two", "three"]}),
            json!({"model": "fixture", "input": ["four"]}),
            json!({"model": "fixture", "input": ["four"]}),
        ],
    );
}

#[tokio::test]
async fn embedding_import_bounds_concurrency_and_maps_out_of_order_replies() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = Embedding::new(
        ApiConfig {
            base_url: format!("http://{}", listener.local_addr().unwrap()),
            model: "fixture".to_owned(),
            api_key: None,
        },
        2,
    )
    .unwrap();
    let (arrivals, mut requests) = mpsc::unbounded_channel();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move |Json(body): Json<Value>| {
            let arrivals = arrivals.clone();
            async move {
                let inputs = body["input"].as_array().unwrap();
                let (release, wait) = oneshot::channel();
                let long_a = inputs[0].as_str().unwrap().starts_with("# Long A\n");
                let units: usize = inputs
                    .iter()
                    .map(|input| input.as_str().unwrap().encode_utf16().count())
                    .sum();
                arrivals
                    .send((inputs.len(), units, long_a, release))
                    .unwrap();
                wait.await.unwrap();
                let data: Vec<_> = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, _)| {
                        json!({"index": index, "embedding": if long_a { [1, 0] } else { [0, 1] }})
                    })
                    .collect();
                Json(json!({"data": data}))
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let long = "芯".repeat(16_500);
    let first = format!("# Long A\n{long}\n# Long B\n{long}\n");
    let mut short = String::new();
    for i in 0..144 {
        writeln!(short, "# Section {i}").unwrap();
    }
    let documents = [("a.md", first.as_str()), ("b.md", short.as_str())];
    let build = build_fixture(directory.path(), Uuid::new_v4(), &documents, Some(provider));
    let respond = async {
        let mut lengths = Vec::new();
        let mut pending = Vec::new();
        for _ in 0..8 {
            let (count, units, long_a, release) = timeout(Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(units <= 32_768);
            lengths.push(count);
            pending.push((long_a, release));
        }
        assert!(
            timeout(Duration::from_millis(100), requests.recv())
                .await
                .is_err()
        );
        let mut first_response = None;
        for (long_a, release) in pending {
            if long_a {
                first_response = Some(release);
            } else {
                release.send(()).unwrap();
            }
        }
        // A slot must be reused even though the first request is still waiting.
        for _ in 0..3 {
            let (count, units, _, release) = timeout(Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(units <= 32_768);
            lengths.push(count);
            release.send(()).unwrap();
            if lengths.len() == 9 {
                first_response.take().unwrap().send(()).unwrap();
            }
        }
        lengths.sort_unstable();
        assert_eq!(lengths, [1, 2, 15, 16, 16, 16, 16, 16, 16, 16, 16]);
    };
    let (result, ()) = tokio::join!(build, respond);
    assert_eq!(result, None);
    server.abort();

    let index = directory.path().join("index");
    assert!(index.join(EMBEDDING_GENERATION).is_file());
    let shard = EdgeShard::load(&index, None).unwrap();
    let hits = shard
        .query(
            QueryRequestBuilder::new(1)
                .query(ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery {
                    query: vec![1.0, 0.0].into(),
                    using: Some(SEMANTIC_VECTOR.to_owned()),
                })))
                .build(),
        )
        .unwrap();
    assert_eq!(hits[0].id, PointId::from(1_u64));
    assert_eq!(hits[0].score, 1.0);
}
