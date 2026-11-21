use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::OnceLock;

use qdrant_edge::bm25_embed::{EdgeBm25, EdgeBm25Config, EdgeBm25Error};
use qdrant_edge::{
    CreateIndex, DisabledStemmerParams, Distance, EdgeConfigBuilder, EdgeShard,
    EdgeSparseVectorParamsBuilder, EdgeVectorParamsBuilder, FieldIndexOperations, Memory, Modifier,
    NoStemmer, OperationError, Payload, PayloadFieldSchema, PayloadSchemaType,
    PointInsertOperations, PointOperations, PointStructPersisted, ScrollRequest, StemmingAlgorithm,
    StopwordsInterface, StopwordsSet, TokenizerType, UpdateOperation, VectorPersisted,
    VectorStorageDatatype, VectorStructPersisted, WithPayloadInterface,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use uuid::Uuid;

use crate::durable::SOURCE_PATHS;
use crate::import::{Batch, Document, ImportError};
use crate::markdown::{self, Section};
use crate::model_http::{Embedding, ModelError};
use crate::technical;

pub(crate) const KEYWORD_VECTOR: &str = "bm25";
pub(crate) const SEMANTIC_VECTOR: &str = "embedding";
pub(crate) const SCOPE_FIELD: &str = "scope";
pub(crate) const EXACT_FIELD: &str = "exact";
const PATH_FIELD: &str = "path";

// Markdown already limits each section's length. This limits both the Edge update
// and the corresponding HTTP request independently of the source document size.
const POINT_BATCH: usize = 16;
pub(crate) const EMBEDDING_GENERATION: &str = "embeddings.json";
const EMBEDDING_ENCODING: &str = "float32-cosine";

pub(crate) fn bm25() -> io::Result<&'static EdgeBm25> {
    static MODEL: OnceLock<Result<EdgeBm25, EdgeBm25Error>> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            EdgeBm25::new(EdgeBm25Config {
                k: 1.2.try_into().expect("finite BM25 k"),
                b: 0.75.try_into().expect("finite BM25 b"),
                // Never take length statistics from outside the query's scope.
                avg_len: 256.0.try_into().expect("finite BM25 average length"),
                tokenizer: TokenizerType::Multilingual,
                lowercase: Some(true),
                stopwords: Some(StopwordsInterface::Set(StopwordsSet::default())),
                stemmer: Some(StemmingAlgorithm::Disabled(DisabledStemmerParams {
                    r#type: NoStemmer::None,
                })),
                ..EdgeBm25Config::default()
            })
        })
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Passage {
    pub(crate) text: String,
    pub(crate) path: String,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) sha256: String,
    pub(crate) byte_start: usize,
    pub(crate) exact: Vec<String>,
}

impl Passage {
    pub(crate) fn from_payload(payload: Payload) -> serde_json::Result<Self> {
        serde_json::from_value(Value::Object(payload.0))
    }
}

#[derive(Debug)]
pub(crate) enum IndexError {
    Document(ImportError),
    Storage(&'static str, io::Error),
    Edge(&'static str, OperationError),
    PointCount,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Document(_) => f.write_str("could not read staged Markdown document"),
            Self::Storage(operation, _) | Self::Edge(operation, _) => f.write_str(operation),
            Self::PointCount => f.write_str("too many passages in the index"),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Document(error) => Some(error),
            Self::Storage(_, error) => Some(error),
            Self::Edge(_, error) => Some(error),
            Self::PointCount => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct EmbeddingGeneration<T> {
    version: Uuid,
    model: T,
    dimensions: usize,
    encoding: T,
}

impl EmbeddingGeneration<String> {
    pub(crate) fn matches(&self, version: Uuid, provider: &Embedding) -> bool {
        self.version == version
            && self.model == provider.model()
            && self.dimensions == provider.dimensions()
            && self.encoding == EMBEDDING_ENCODING
    }
}

/// Edge uses a float32 sum of squares and skips cosine normalization below
/// epsilon. Reject vectors that would become zero or remain unnormalized.
pub(crate) fn usable_embedding(vector: &[f32]) -> bool {
    let square_norm = vector.iter().map(|&value| value * value).sum::<f32>();
    square_norm.is_finite() && square_norm >= f32::EPSILON
}

/// Build a fresh generation on a blocking worker. The catalog owns publication
/// and durability sealing, and receives no live build-time Edge handle.
pub(crate) fn build(
    index_path: &Path,
    version: Uuid,
    batch: &Batch,
    embedding: Option<&Embedding>,
    runtime: &Handle,
    mut report_progress: impl FnMut(usize, u64),
) -> Result<Option<ModelError>, IndexError> {
    let model = bm25().map_err(|error| IndexError::Storage("configure BM25", error))?;
    // EdgeShard::new by itself permits reusing an existing directory without
    // segments; never reuse that directory or its optional generation record.
    fs::create_dir(index_path)
        .map_err(|error| IndexError::Storage("create fresh index directory", error))?;
    let mut config = EdgeConfigBuilder::new()
        .payload_memory(Memory::Cold)
        .max_search_threads(2)
        .sparse_vector(
            KEYWORD_VECTOR,
            EdgeSparseVectorParamsBuilder::new()
                .memory(Memory::Pinned)
                .modifier(Modifier::Idf)
                .build(),
        )
        // Intermediate states are private, never resumed, and sealed by the caller.
        .wal_enabled(false);
    if let Some(provider) = embedding {
        config = config.vector(
            SEMANTIC_VECTOR,
            EdgeVectorParamsBuilder::new(provider.dimensions(), Distance::Cosine)
                .memory(Memory::Cold)
                .datatype(VectorStorageDatatype::Float32)
                .build(),
        );
    }
    let shard = EdgeShard::new(index_path, config.build())
        .map_err(|error| IndexError::Edge("create Edge shard", error))?;
    for field in [SCOPE_FIELD, EXACT_FIELD] {
        shard
            .update(UpdateOperation::FieldIndexOperation(
                FieldIndexOperations::CreateIndex(CreateIndex {
                    field_name: field.parse().expect("constant Edge field name"),
                    field_schema: Some(PayloadFieldSchema::FieldType(PayloadSchemaType::Keyword)),
                }),
            ))
            .map_err(|error| IndexError::Edge("create Edge keyword payload index", error))?;
    }

    let mut embedding_error = None;
    let mut last_id = 0_u64;
    for (completed, document) in batch.documents().iter().enumerate() {
        let source = document.read_body().map_err(IndexError::Document)?;
        let scope: Vec<_> = document
            .relative_path
            .match_indices('/')
            .map(|(end, _)| &document.relative_path[..end])
            .chain(std::iter::once(document.relative_path.as_str()))
            .collect();
        let sections = markdown::sections(&source);
        let batches = sections.chunks(POINT_BATCH);
        let batch_count = batches.len();
        for (number, sections) in batches.enumerate() {
            let mut vectors = Vec::new();
            if let Some(provider) = embedding.filter(|_| embedding_error.is_none()) {
                let mut inputs = [""; POINT_BATCH];
                for (input, section) in inputs.iter_mut().zip(sections) {
                    *input = section.text;
                }
                match runtime.block_on(provider.embed(&inputs[..sections.len()])) {
                    Ok(batch) if batch.iter().all(|vector| usable_embedding(vector)) => {
                        vectors = batch;
                    }
                    Ok(_) => embedding_error = Some(ModelError::InvalidVector),
                    Err(error) => embedding_error = Some(error),
                }
            }
            let mut vectors = vectors.into_iter();
            let mut points = Vec::with_capacity(sections.len());
            for section in sections {
                last_id = last_id.checked_add(1).ok_or(IndexError::PointCount)?;
                let mut named = HashMap::with_capacity(if vectors.len() == 0 { 1 } else { 2 });
                named.insert(
                    KEYWORD_VECTOR.to_owned(),
                    VectorPersisted::Sparse(model.embed_document(section.text)),
                );
                if let Some(vector) = vectors.next() {
                    named.insert(SEMANTIC_VECTOR.to_owned(), VectorPersisted::Dense(vector));
                }
                points.push(PointStructPersisted {
                    id: last_id.into(),
                    vector: VectorStructPersisted::Named(named),
                    payload: Some(passage_payload(document, &source, section, &scope)),
                });
            }
            shard
                .update(UpdateOperation::PointOperation(
                    PointOperations::UpsertPoints(PointInsertOperations::PointsList(points)),
                ))
                .map_err(|error| IndexError::Edge("write Edge passage batch", error))?;
            report_progress(completed + usize::from(number + 1 == batch_count), last_id);
        }
        if batch_count == 0 {
            report_progress(completed + 1, last_id);
        }
    }
    shard
        .flush()
        .map_err(|error| IndexError::Edge("flush Edge shard", error))?;
    drop(shard);

    // Record every accepted file, including zero-byte files that have no passages.
    let mut paths: Vec<_> = batch
        .documents()
        .iter()
        .map(|document| document.relative_path.as_str())
        .collect();
    paths.sort_unstable();
    let mut file = BufWriter::new(
        fs::File::create_new(index_path.join(SOURCE_PATHS))
            .map_err(|error| IndexError::Storage("create source path list", error))?,
    );
    serde_json::to_writer(&mut file, &paths)
        .map_err(|error| IndexError::Storage("write source path list", error.into()))?;
    file.flush()
        .map_err(|error| IndexError::Storage("flush source path list", error))?;

    if let Some(provider) = embedding.filter(|_| last_id != 0 && embedding_error.is_none()) {
        let generation = EmbeddingGeneration {
            version,
            model: provider.model(),
            dimensions: provider.dimensions(),
            encoding: EMBEDDING_ENCODING,
        };
        let path = index_path.join(EMBEDDING_GENERATION);
        let bytes = serde_json::to_vec(&generation)
            .map_err(|error| IndexError::Storage("encode embedding generation", error.into()))?;
        fs::write(path, bytes)
            .map_err(|error| IndexError::Storage("write embedding generation", error))?;
    }
    Ok(embedding_error)
}

/// Older versions only recorded paths on passage payloads.
pub(crate) fn indexed_paths(shard: &EdgeShard) -> io::Result<Vec<String>> {
    let mut request = ScrollRequest::new();
    request.limit = Some(256);
    request.with_payload = Some(WithPayloadInterface::Fields(vec![
        PATH_FIELD.parse().expect("constant Edge field name"),
    ]));
    let mut paths = BTreeSet::new();
    loop {
        let (points, next) = shard.scroll(request.clone()).map_err(io::Error::other)?;
        for point in points {
            let Some(Value::String(path)) = point
                .payload
                .and_then(|mut payload| payload.0.remove(PATH_FIELD))
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "indexed passage has no source path",
                ));
            };
            paths.insert(path);
        }
        let Some(next) = next else {
            return Ok(paths.into_iter().collect());
        };
        request.offset = Some(next);
    }
}

fn passage_payload(
    document: &Document,
    source: &str,
    section: &Section<'_>,
    scope: &[&str],
) -> Payload {
    let span = section.byte_start..section.byte_start + section.text.len();
    let exact = technical::section_names(source, span);
    Payload(Map::from_iter(
        [
            ("text", Value::from(section.text)),
            (PATH_FIELD, Value::from(document.relative_path.as_str())),
            ("line_start", Value::from(section.line_start)),
            ("line_end", Value::from(section.line_end)),
            ("sha256", Value::from(document.sha256.as_str())),
            ("byte_start", Value::from(section.byte_start)),
            (EXACT_FIELD, Value::from(exact)),
            (SCOPE_FIELD, Value::from(scope)),
        ]
        .map(|(key, value)| (key.to_owned(), value)),
    ))
}

#[cfg(test)]
mod tests;
