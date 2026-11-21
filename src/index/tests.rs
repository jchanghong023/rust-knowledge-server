use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{FromRequest, Multipart, State};
use axum::http::{Request, header};
use axum::{Json, Router, routing::post};
use qdrant_edge::{
    Condition, FieldCondition, Filter, JsonPath, Match, NamedQuery, PointId, QueryEnum,
    QueryRequestBuilder, ScoringQuery, ScrollRequestBuilder, WithPayloadInterface, WithVector,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

use super::*;
use crate::model_http::ApiConfig;

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
    tokio::task::spawn_blocking(move || build(&path, version, &batch, embedding.as_ref(), &runtime))
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
        let passage = Passage::from_payload(point.payload.unwrap()).unwrap();
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
                    let mut requests = requests.lock().unwrap();
                    requests.push(body);
                    let embedding = if requests.len() == 2 {
                        [1.0e30, 1.0]
                    } else {
                        [1.0, 0.0]
                    };
                    Json(
                        json!({"model": "fixture", "data": [{"index": 0, "embedding": embedding}]}),
                    )
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
    for (i, record) in records.iter().enumerate() {
        assert!(record.get_vector_by_name(KEYWORD_VECTOR).is_some());
        assert_eq!(record.get_vector_by_name(SEMANTIC_VECTOR).is_some(), i == 0);
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
        ["one", "two", "four"].map(|input| json!({"model": "fixture", "input": [input]})),
    );
}
