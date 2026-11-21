use std::{
    cmp::Ordering,
    collections::{HashMap, hash_map::Entry},
    error::Error,
    fmt,
    io::{self, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use qdrant_edge::{
    Condition, EdgeShardRead, FieldCondition, Filter, Fusion, GroupRequest, IdfCorpusParams,
    IdfParams, JsonPath, Match, NamedQuery, Payload, PointId, Prefetch, QueryEnum, QueryRequest,
    RetrieveRequest, ScoringQuery, ScrollRequest, SearchParams, ValueVariants, VectorInternal,
    WithPayloadInterface,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    catalog::{Catalog, CatalogError, Snapshot},
    import::validate_path,
    index::{
        EXACT_FIELD, KEYWORD_VECTOR, Passage, SCOPE_FIELD, SEMANTIC_VECTOR, bm25, usable_embedding,
    },
    model_http::{Embedding, ModelError, Rerank},
    technical::query_name,
};

const MAX_QUERY_BYTES: usize = 4096;
const BRANCH_LIMIT: usize = 32;
const PASSAGES_PER_FILE_PER_BRANCH: usize = 2;
const RESULT_LIMIT: usize = 16;
const RERANK_LIMIT: usize = 8;
const RRF_K: usize = 60;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FusionMode {
    #[default]
    Grouped,
    Edge,
}

impl FusionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Grouped => "grouped",
            Self::Edge => "edge",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) library: Option<String>,
    pub(crate) path: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SearchError {
    InvalidInput(&'static str),
    LibraryNotFound,
    Store,
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInput(message) => message,
            Self::LibraryNotFound => "library does not exist",
            Self::Store => "published index or catalog is unavailable",
        })
    }
}

impl Error for SearchError {}

#[derive(Debug, Serialize)]
pub(crate) struct Scope {
    pub(crate) library: Option<String>,
    pub(crate) path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Ok,
    NoPublishedIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Retrieval {
    Keyword,
    Hybrid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Category {
    Exact,
    Related,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Evidence {
    pub(crate) text: String,
    pub(crate) library: String,
    pub(crate) path: String,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) sha256: String,
    pub(crate) category: Category,
    #[serde(skip)]
    pub(crate) source: Source,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) library: Uuid,
    pub(crate) version: Uuid,
    pub(crate) point: u64,
    pub(crate) byte_start: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct SearchReply {
    pub(crate) scope: Scope,
    pub(crate) status: Status,
    pub(crate) matches: Vec<Evidence>,
    pub(crate) retrieval: Retrieval,
    pub(crate) reranked: bool,
    #[serde(skip)]
    pub(crate) name: Option<String>,
}

#[derive(Default)]
struct SearchTiming {
    snapshot: Duration,
    shards: usize,
    embedding_check: Duration,
    embedding: Duration,
    retrieval: Duration,
    stages: RetrievalTiming,
    rerank: Duration,
}

#[derive(Default)]
struct RetrievalTiming {
    shard_open: Duration,
    exact: Duration,
    bm25: Duration,
    dense: Duration,
    edge_fusion: Duration,
    evidence: Duration,
}

fn measure<T>(elapsed: &mut Duration, work: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = work();
    *elapsed += start.elapsed();
    result
}

pub(crate) async fn search(
    catalog: &Catalog,
    request: SearchRequest,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<&Rerank>,
    fusion: FusionMode,
) -> Result<SearchReply, SearchError> {
    let start = Instant::now();
    let mut timing = SearchTiming::default();
    let result = run_search(catalog, request, embedding, rerank, fusion, &mut timing).await;
    let total = start.elapsed();
    let (status, hits) = match &result {
        Ok(reply) if reply.status == Status::NoPublishedIndex => ("no_published_index", 0),
        Ok(reply) => ("ok", reply.matches.len()),
        Err(SearchError::InvalidInput(_)) => ("invalid_input", 0),
        Err(SearchError::LibraryNotFound) => ("library_not_found", 0),
        Err(SearchError::Store) => ("store_error", 0),
    };
    let stages = timing.stages;
    let _ = writeln!(
        io::stderr().lock(),
        "search_timing status={status} hits={hits} shards={} total_us={} snapshot_us={} \
         embedding_check_us={} embedding_us={} retrieval_us={} shard_open_us={} exact_us={} \
         bm25_us={} dense_us={} edge_fusion_us={} evidence_us={} rerank_us={}",
        timing.shards,
        total.as_micros(),
        timing.snapshot.as_micros(),
        timing.embedding_check.as_micros(),
        timing.embedding.as_micros(),
        timing.retrieval.as_micros(),
        stages.shard_open.as_micros(),
        stages.exact.as_micros(),
        stages.bm25.as_micros(),
        stages.dense.as_micros(),
        stages.edge_fusion.as_micros(),
        stages.evidence.as_micros(),
        timing.rerank.as_micros(),
    );
    result
}

async fn run_search(
    catalog: &Catalog,
    request: SearchRequest,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<&Rerank>,
    fusion: FusionMode,
    timing: &mut SearchTiming,
) -> Result<SearchReply, SearchError> {
    let SearchRequest {
        query,
        library,
        path,
    } = request;
    if query
        .trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
        .is_empty()
    {
        return Err(SearchError::InvalidInput("query must not be blank"));
    }
    if query.len() > MAX_QUERY_BYTES {
        return Err(SearchError::InvalidInput("query exceeds 4096 UTF-8 bytes"));
    }
    if library
        .as_deref()
        .is_some_and(|name| name.trim().is_empty() || name.chars().any(char::is_control))
    {
        return Err(SearchError::InvalidInput("invalid library name"));
    }
    if path
        .as_deref()
        .is_some_and(|path| validate_path(path).is_err())
    {
        return Err(SearchError::InvalidInput("invalid relative path scope"));
    }

    // Pin the whole scope before opening any shard or awaiting any model.
    let snapshot = measure(&mut timing.snapshot, || {
        catalog.snapshot(library.as_deref())
    })
    .map_err(|error| match error {
        CatalogError::NotFound => SearchError::LibraryNotFound,
        _ => SearchError::Store,
    })?;
    timing.shards = snapshot.len();
    let scope = Scope { library, path };
    if snapshot.is_empty() {
        return Ok(SearchReply {
            scope,
            status: Status::NoPublishedIndex,
            matches: Vec::new(),
            retrieval: Retrieval::Keyword,
            reranked: false,
            name: None,
        });
    }
    let filter = scope
        .path
        .as_deref()
        .map(|path| match_condition(SCOPE_FIELD, path).map(Filter::new_must))
        .transpose()?;

    let (snapshot, filter, semantic) = if let Some(embedding) = embedding {
        let start = Instant::now();
        let checked = tokio::task::spawn_blocking(move || {
            let eligible = embedding_versions(&snapshot, filter.as_ref(), &embedding)?;
            Ok::<_, SearchError>((snapshot, filter, eligible, embedding))
        })
        .await;
        timing.embedding_check = start.elapsed();
        let (snapshot, filter, eligible, embedding) = checked.map_err(|_| SearchError::Store)??;
        let semantic = if eligible.is_empty() {
            None
        } else {
            let start = Instant::now();
            let embedded = embedding.embed(&[&query]).await;
            timing.embedding = start.elapsed();
            match embedded {
                Ok(vectors) => match <[Vec<f32>; 1]>::try_from(vectors) {
                    Ok([vector]) if usable_embedding(&vector) => Some((eligible, vector)),
                    _ => {
                        log_model_failure("embedding", ModelError::InvalidVector);
                        None
                    }
                },
                Err(error) => {
                    log_model_failure("embedding", error);
                    None
                }
            }
        };
        (snapshot, filter, semantic)
    } else {
        (snapshot, filter, None)
    };

    let retrieval = if semantic.is_some() {
        Retrieval::Hybrid
    } else {
        Retrieval::Keyword
    };
    // The runtime bounds its blocking pool. Keep each batch of Edge work together
    // rather than scheduling a worker per library or per retrieval branch.
    let start = Instant::now();
    let retrieved = tokio::task::spawn_blocking(move || {
        let name = query_name(&query);
        let mut stages = RetrievalTiming::default();
        let matches = retrieve(
            &snapshot,
            &query,
            name.as_deref(),
            filter,
            semantic,
            fusion,
            &mut stages,
        );
        (query, name, matches, stages)
    })
    .await;
    timing.retrieval = start.elapsed();
    let (query, name, matches, stages) = retrieved.map_err(|_| SearchError::Store)?;
    timing.stages = stages;
    let mut matches = matches?;

    let mut reranked = false;
    if let Some(rerank) = rerank.filter(|_| !matches.is_empty()) {
        let window_len = matches.len().min(RERANK_LIMIT);
        let window = &mut matches[..window_len];
        let mut documents = [""; RERANK_LIMIT];
        for (document, evidence) in documents.iter_mut().zip(window.iter()) {
            *document = &evidence.text;
        }
        let start = Instant::now();
        let scores = rerank.scores(&query, &documents[..window_len]).await;
        timing.rerank = start.elapsed();
        match scores {
            Ok(mut scores) => {
                rerank_window(window, &mut scores);
                reranked = true;
            }
            Err(error) => log_model_failure("rerank", error),
        }
    }
    Ok(SearchReply {
        scope,
        status: Status::Ok,
        matches,
        retrieval,
        reranked,
        name,
    })
}

fn log_model_failure(operation: &str, kind: ModelError) {
    // ModelError contains no URL, credentials, query, or wire body.
    let _ = writeln!(io::stderr().lock(), "{operation} unavailable: {kind}");
}

fn match_condition(field: &str, value: &str) -> Result<Condition, SearchError> {
    let key = field.parse().map_err(|_| SearchError::Store)?;
    Ok(Condition::Field(FieldCondition::new_match(
        key,
        Match::new_value(ValueVariants::String(value.to_owned())),
    )))
}

fn embedding_versions(
    snapshot: &Snapshot,
    filter: Option<&Filter>,
    embedding: &Embedding,
) -> Result<Vec<usize>, SearchError> {
    let mut eligible = Vec::new();
    for (ordinal, (_, version)) in snapshot.iter().enumerate() {
        let Some(shard) = version
            .for_embedding(embedding)
            .map_err(|_| SearchError::Store)?
        else {
            continue;
        };
        if let Some(filter) = filter {
            let (points, _) = shard
                .scroll(ScrollRequest {
                    filter: Some(filter.clone()),
                    limit: Some(1),
                    with_payload: Some(false.into()),
                    ..ScrollRequest::new()
                })
                .map_err(|_| SearchError::Store)?;
            if points.is_empty() {
                continue;
            }
        }
        eligible.push(ordinal);
    }
    Ok(eligible)
}

#[derive(Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct Location {
    path: String,
    line_start: usize,
    byte_start: usize,
}

struct Candidate {
    library: usize,
    id: u64,
    location: Location,
    category: Category,
    rank_score: f64,
}

impl Candidate {
    fn from_payload(
        library: usize,
        id: u64,
        payload: Option<Payload>,
        name: Option<&str>,
    ) -> Result<Self, SearchError> {
        let payload = payload.ok_or(SearchError::Store)?;
        let category = if let Some(name) = name {
            let names = payload
                .0
                .get(EXACT_FIELD)
                .and_then(Value::as_array)
                .ok_or(SearchError::Store)?;
            category(names.iter().filter_map(Value::as_str), Some(name))
        } else {
            Category::Related
        };
        let location =
            serde_json::from_value(Value::Object(payload.0)).map_err(|_| SearchError::Store)?;
        Ok(Self {
            library,
            id,
            location,
            category,
            rank_score: 0.0,
        })
    }
}

fn category<'a>(mut names: impl Iterator<Item = &'a str>, query: Option<&str>) -> Category {
    if query.is_some_and(|name| names.any(|candidate| candidate == name)) {
        Category::Exact
    } else {
        Category::Related
    }
}

fn retrieve(
    snapshot: &Snapshot,
    query: &str,
    name: Option<&str>,
    filter: Option<Filter>,
    semantic: Option<(Vec<usize>, Vec<f32>)>,
    fusion: FusionMode,
    timing: &mut RetrievalTiming,
) -> Result<Vec<Evidence>, SearchError> {
    let exact_filter = name
        .map(|name| {
            let mut filter = filter.clone().unwrap_or_default();
            filter
                .must
                .get_or_insert_with(Vec::new)
                .push(match_condition(EXACT_FIELD, name)?);
            Ok::<_, SearchError>(filter)
        })
        .transpose()?;
    let sparse = bm25().map_err(|_| SearchError::Store)?.embed_query(query);
    let keyword = (!sparse.indices.is_empty()).then(|| {
        ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery::new(
            sparse.into(),
            KEYWORD_VECTOR,
        )))
    });
    let idf = filter.as_ref().map(|scope| SearchParams {
        idf: Some(IdfParams::Corpus(IdfCorpusParams {
            corpus: scope.clone(),
        })),
        ..SearchParams::default()
    });
    let (eligible, dense) = match semantic {
        Some((eligible, vector)) => (
            eligible,
            Some(ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery::new(
                VectorInternal::from(vector),
                SEMANTIC_VECTOR,
            )))),
        ),
        None => (Vec::new(), None),
    };
    let mut payload_paths = vec!["path", "line_start", "byte_start"];
    if name.is_some() {
        payload_paths.push(EXACT_FIELD);
    }
    let metadata = WithPayloadInterface::Fields(
        payload_paths
            .into_iter()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| SearchError::Store)?,
    );
    let path: JsonPath = "path".parse().expect("constant Edge payload path");
    let grouped = |request: QueryRequest| {
        GroupRequest::new(
            QueryRequest {
                with_payload: metadata.clone(),
                ..request
            },
            path.clone(),
            BRANCH_LIMIT / PASSAGES_PER_FILE_PER_BRANCH,
            PASSAGES_PER_FILE_PER_BRANCH,
        )
    };
    let mut best = Vec::with_capacity(RESULT_LIMIT + BRANCH_LIMIT * 3);
    let mut local = HashMap::new();
    let mut points = Vec::with_capacity(BRANCH_LIMIT);
    for (library, (_, version)) in snapshot.iter().enumerate() {
        let shard =
            measure(&mut timing.shard_open, || version.open()).map_err(|_| SearchError::Store)?;
        if let Some(exact) = &exact_filter {
            let groups = measure(&mut timing.exact, || {
                shard.query_groups(grouped(QueryRequest {
                    filter: Some(exact.clone()),
                    ..QueryRequest::new(BRANCH_LIMIT)
                }))
            })
            .map_err(|_| SearchError::Store)?;
            for point in groups.into_iter().flat_map(|group| group.hits) {
                let PointId::NumId(id) = point.id else {
                    return Err(SearchError::Store);
                };
                let candidate = Candidate::from_payload(library, id, point.payload, name)?;
                if candidate.category != Category::Exact {
                    return Err(SearchError::Store);
                }
                local.insert(candidate.id, candidate);
            }
        }
        let dense = dense
            .as_ref()
            .filter(|_| eligible.binary_search(&library).is_ok());
        if let (FusionMode::Edge, Some(keyword), Some(dense)) = (fusion, keyword.as_ref(), dense) {
            let groups = measure(&mut timing.edge_fusion, || {
                shard.query_groups(grouped(QueryRequest {
                    prefetches: vec![
                        Prefetch {
                            query: Some(keyword.clone()),
                            params: idf.clone(),
                            ..Prefetch::new(BRANCH_LIMIT)
                        },
                        Prefetch {
                            query: Some(dense.clone()),
                            ..Prefetch::new(BRANCH_LIMIT)
                        },
                    ],
                    query: Some(ScoringQuery::Fusion(Fusion::Rrf {
                        // Edge uses zero-based positions; our ranks start at one.
                        k: RRF_K + 1,
                        weights: None,
                    })),
                    filter: filter.clone(),
                    ..QueryRequest::new(BRANCH_LIMIT)
                }))
            })
            .map_err(|_| SearchError::Store)?;
            for point in groups.into_iter().flat_map(|group| group.hits) {
                let PointId::NumId(id) = point.id else {
                    return Err(SearchError::Store);
                };
                if !point.score.is_finite() {
                    return Err(SearchError::Store);
                }
                let mut candidate = Candidate::from_payload(library, id, point.payload, name)?;
                candidate.rank_score = f64::from(point.score);
                local.insert(id, candidate);
            }
        } else {
            for (scoring, params, elapsed) in [
                (keyword.as_ref(), idf.as_ref(), &mut timing.bm25),
                (dense, None, &mut timing.dense),
            ] {
                let Some(scoring) = scoring else { continue };
                let groups = measure(elapsed, || {
                    shard.query_groups(grouped(QueryRequest {
                        query: Some(scoring.clone()),
                        filter: filter.clone(),
                        params: params.cloned(),
                        ..QueryRequest::new(BRANCH_LIMIT)
                    }))
                })
                .map_err(|_| SearchError::Store)?;
                points.extend(groups.into_iter().flat_map(|group| group.hits));
                // Edge sorts groups by their best hit, not all hits globally.
                // Restore branch-wide score order before converting scores to ranks.
                points.sort_unstable_by(|left, right| right.score.total_cmp(&left.score));
                let mut rank = 0;
                let mut last = None;
                for (position, point) in points.drain(..).enumerate() {
                    if !point.score.is_finite() {
                        return Err(SearchError::Store);
                    }
                    if last != Some(point.score) {
                        rank = position + 1;
                        last = Some(point.score);
                    }
                    let PointId::NumId(id) = point.id else {
                        return Err(SearchError::Store);
                    };
                    let candidate = match local.entry(id) {
                        Entry::Occupied(entry) => entry.into_mut(),
                        Entry::Vacant(entry) => {
                            entry.insert(Candidate::from_payload(library, id, point.payload, name)?)
                        }
                    };
                    candidate.rank_score += 1.0 / (RRF_K + rank) as f64;
                }
            }
        }
        best.extend(local.drain().map(|(_, candidate)| candidate));
        best.sort_unstable_by(|left, right| {
            left.category
                .cmp(&right.category)
                .then_with(|| right.rank_score.total_cmp(&left.rank_score))
                .then_with(|| snapshot[left.library].0.cmp(&snapshot[right.library].0))
                .then_with(|| location_order(left, right))
        });
        // Later shards cannot add a vote to points from this shard.
        best.truncate(RESULT_LIMIT);
    }
    measure(&mut timing.evidence, || {
        load_evidence(snapshot, &best, name)
    })
}

fn location_order(left: &Candidate, right: &Candidate) -> Ordering {
    left.location
        .cmp(&right.location)
        .then(left.id.cmp(&right.id))
}

fn load_evidence(
    snapshot: &Snapshot,
    best: &[Candidate],
    name: Option<&str>,
) -> Result<Vec<Evidence>, SearchError> {
    // Fetch raw text only for the final unique IDs, batched by pinned shard.
    let mut by_library = (0..best.len()).collect::<Vec<_>>();
    by_library.sort_unstable_by_key(|&rank| best[rank].library);
    let mut evidence = (0..best.len()).map(|_| None).collect::<Vec<_>>();
    for ranks in by_library.chunk_by(|&left, &right| best[left].library == best[right].library) {
        let (library, version) = &snapshot[best[ranks[0]].library];
        let shard = version.open().map_err(|_| SearchError::Store)?;
        let ids = ranks
            .iter()
            .map(|&rank| PointId::NumId(best[rank].id))
            .collect();
        let records = shard
            .retrieve(RetrieveRequest::new(ids))
            .map_err(|_| SearchError::Store)?;
        if records.len() != ranks.len() {
            return Err(SearchError::Store);
        }
        for (&rank, record) in ranks.iter().zip(records) {
            let candidate = &best[rank];
            let passage = Passage::from_payload(record.payload.ok_or(SearchError::Store)?)
                .map_err(|_| SearchError::Store)?;
            if record.id != PointId::NumId(candidate.id)
                || candidate.category != category(passage.exact.iter().map(String::as_str), name)
            {
                return Err(SearchError::Store);
            }
            let (library_id, version_id) = version.identity();
            evidence[rank] = Some(Evidence {
                text: passage.text,
                library: library.clone(),
                path: passage.path,
                line_start: passage.line_start,
                line_end: passage.line_end,
                sha256: passage.sha256,
                category: candidate.category,
                source: Source {
                    library: library_id,
                    version: version_id,
                    point: candidate.id,
                    byte_start: passage.byte_start,
                },
            });
        }
    }
    evidence
        .into_iter()
        .map(|item| item.ok_or(SearchError::Store))
        .collect()
}

fn rerank_window(window: &mut [Evidence], scores: &mut [f64]) {
    // Insertion sort is stable, in-place and bounded by RERANK_LIMIT. A point
    // cannot cross its category boundary, and points outside the slice do not move.
    for next in 1..window.len() {
        let mut position = next;
        while position > 0
            && window[position].category == window[position - 1].category
            && scores[position] > scores[position - 1]
        {
            window.swap(position, position - 1);
            scores.swap(position, position - 1);
            position -= 1;
        }
    }
}

#[cfg(test)]
mod tests;
