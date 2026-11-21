use std::{
    cmp::Ordering,
    collections::HashMap,
    error::Error,
    fmt,
    io::{self, Write},
    ops::Range,
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
    index::{EXACT_FIELD, KEYWORD_VECTOR, SCOPE_FIELD, SEMANTIC_VECTOR, bm25, usable_embedding},
    model_http::{Embedding, ModelError, Rerank},
};

mod literal;
mod page;

pub(crate) use page::{Page, page};

use literal::LiteralQuery;

const MAX_QUERY_BYTES: usize = 4096;
const BRANCH_LIMIT: usize = 32;
const PASSAGES_PER_FILE_PER_BRANCH: usize = 2;
const RESULT_LIMIT: usize = 16;
const RERANK_LIMIT: usize = 8;
const RRF_K: usize = 60;
const LITERAL_BATCH: usize = 32;

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
    #[serde(default)]
    pub(crate) strict: bool,
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
    Literal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Category {
    Exact,
    Related,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum MatchTier {
    Related,
    Identifier,
    Topic,
    TargetTopic,
    DirectTopic,
    ExactNavigation,
    Exact,
    ExactDetail,
}

impl MatchTier {
    fn is_exact(self) -> bool {
        self >= Self::ExactNavigation
    }
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
    #[serde(skip)]
    pub(crate) anchor: Option<Range<usize>>,
    #[serde(skip)]
    tier: MatchTier,
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
    execute(catalog, request, embedding, rerank, fusion, None)
        .await
        .map(|page| page.reply)
}

async fn execute(
    catalog: &Catalog,
    request: SearchRequest,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<&Rerank>,
    fusion: FusionMode,
    paging: Option<page::Paging>,
) -> Result<Page, SearchError> {
    let start = Instant::now();
    let mut timing = SearchTiming::default();
    let result = run_search(
        catalog,
        request,
        embedding,
        rerank,
        fusion,
        paging,
        &mut timing,
    )
    .await;
    let total = start.elapsed();
    let (status, hits) = match &result {
        Ok(page) if page.reply.status == Status::NoPublishedIndex => ("no_published_index", 0),
        Ok(page) => ("ok", page.reply.matches.len()),
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
    mut paging: Option<page::Paging>,
    timing: &mut SearchTiming,
) -> Result<Page, SearchError> {
    let SearchRequest {
        query,
        library,
        path,
        strict,
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
        CatalogError::NotFound if paging.as_ref().is_some_and(page::Paging::is_continuation) => {
            page::STALE_CURSOR
        }
        CatalogError::NotFound => SearchError::LibraryNotFound,
        _ => SearchError::Store,
    })?;
    timing.shards = snapshot.len();
    let scope = Scope { library, path };
    if let Some(paging) = &mut paging {
        paging.bind(&query, &scope, strict, fusion, &snapshot)?;
    }
    if snapshot.is_empty() {
        return Ok(Page::unpaged(SearchReply {
            scope,
            status: Status::NoPublishedIndex,
            matches: Vec::new(),
            retrieval: if strict {
                Retrieval::Literal
            } else {
                Retrieval::Keyword
            },
            reranked: false,
        }));
    }
    let filter = scope
        .path
        .as_deref()
        .map(|path| match_condition(SCOPE_FIELD, path).map(Filter::new_must))
        .transpose()?;

    let replay_retrieval = paging
        .as_ref()
        .and_then(page::Paging::replay)
        .map(|ranking| ranking.retrieval);
    let embedding = embedding.filter(|_| !strict && replay_retrieval.is_none());
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

    let retrieval = if let Some(retrieval) = replay_retrieval {
        retrieval
    } else if strict {
        Retrieval::Literal
    } else if semantic.is_some() {
        Retrieval::Hybrid
    } else {
        Retrieval::Keyword
    };
    // The runtime bounds its blocking pool. Keep each batch of Edge work together
    // rather than scheduling a worker per library or per retrieval branch.
    let start = Instant::now();
    let retrieved = tokio::task::spawn_blocking(move || {
        let mut stages = RetrievalTiming::default();
        let matches = retrieve(
            &snapshot,
            &query,
            strict,
            filter,
            semantic,
            fusion,
            paging,
            &mut stages,
        );
        (query, matches, stages, snapshot)
    })
    .await;
    timing.retrieval = start.elapsed();
    let (query, matches, stages, snapshot) = retrieved.map_err(|_| SearchError::Store)?;
    timing.stages = stages;
    let mut ranked = matches?;
    let matches = &mut ranked.head;

    let mut reranked = false;
    if let Some(rerank) =
        rerank.filter(|_| !strict && replay_retrieval.is_none() && !matches.is_empty())
    {
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
                let exact_count =
                    window.partition_point(|evidence| evidence.category == Category::Exact);
                rerank_window(&mut window[exact_count..], &mut scores[exact_count..]);
                reranked = true;
            }
            Err(error) => log_model_failure("rerank", error),
        }
    }
    ranked.finish(scope, retrieval, reranked, snapshot)
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

#[derive(Deserialize)]
struct PassageText {
    text: String,
    path: String,
    line_start: usize,
    line_end: usize,
    sha256: String,
    byte_start: usize,
}

struct Candidate {
    library: usize,
    id: u64,
    passage: PassageText,
    anchor: Option<Range<usize>>,
    tier: MatchTier,
    rank_score: f64,
}

impl Candidate {
    fn category(&self) -> Category {
        if self.tier.is_exact() {
            Category::Exact
        } else {
            Category::Related
        }
    }

    fn into_evidence(self, snapshot: &Snapshot) -> Evidence {
        let (library, version) = &snapshot[self.library];
        let (library_id, version_id) = version.identity();
        Evidence {
            category: self.category(),
            text: self.passage.text,
            library: library.clone(),
            path: self.passage.path,
            line_start: self.passage.line_start,
            line_end: self.passage.line_end,
            sha256: self.passage.sha256,
            source: Source {
                library: library_id,
                version: version_id,
                point: self.id,
                byte_start: self.passage.byte_start,
            },
            anchor: self.anchor,
            tier: self.tier,
        }
    }
}

fn passage(payload: Option<Payload>) -> Result<PassageText, SearchError> {
    let payload = payload.ok_or(SearchError::Store)?;
    let passage: PassageText =
        serde_json::from_value(Value::Object(payload.0)).map_err(|_| SearchError::Store)?;
    if passage.text.is_empty() || passage.byte_start.checked_add(passage.text.len()).is_none() {
        return Err(SearchError::Store);
    }
    Ok(passage)
}

fn retrieve(
    snapshot: &Snapshot,
    query: &str,
    strict: bool,
    filter: Option<Filter>,
    semantic: Option<(Vec<usize>, Vec<f32>)>,
    fusion: FusionMode,
    paging: Option<page::Paging>,
    timing: &mut RetrievalTiming,
) -> Result<page::Ranked, SearchError> {
    let mut literal = LiteralQuery::new(query, strict);
    let exact_filter = literal
        .posting
        .as_deref()
        .map(|name| {
            let mut filter = filter.clone().unwrap_or_default();
            filter
                .must
                .get_or_insert_with(Vec::new)
                .push(match_condition(EXACT_FIELD, name)?);
            Ok::<_, SearchError>(filter)
        })
        .transpose()?;
    let replay = paging.as_ref().and_then(page::Paging::replay).cloned();
    let keyword = if strict || replay.is_some() {
        None
    } else {
        let sparse = bm25().map_err(|_| SearchError::Store)?.embed_query(query);
        (!sparse.indices.is_empty()).then(|| {
            ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery::new(
                sparse.into(),
                KEYWORD_VECTOR,
            )))
        })
    };
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
    let text_fields = WithPayloadInterface::Fields(
        [
            "text",
            "path",
            "line_start",
            "line_end",
            "sha256",
            "byte_start",
        ]
        .into_iter()
        .map(|field| field.parse().expect("constant Edge payload field"))
        .collect(),
    );
    let path: JsonPath = "path".parse().expect("constant Edge payload path");
    let grouped = |request: QueryRequest| {
        GroupRequest::new(
            QueryRequest {
                with_payload: false.into(),
                ..request
            },
            path.clone(),
            BRANCH_LIMIT / PASSAGES_PER_FILE_PER_BRANCH,
            PASSAGES_PER_FILE_PER_BRANCH,
        )
    };
    let mut selection = page::Selection::new(paging);
    let mut points = Vec::new();
    let mut ranks = |library: usize,
                     shard: &qdrant_edge::EdgeShard,
                     local: &mut HashMap<u64, f64>,
                     timing: &mut RetrievalTiming|
     -> Result<(), SearchError> {
        if let Some(replay) = &replay {
            replay.ranks(library, local);
            return Ok(());
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
                local.insert(id, f64::from(point.score));
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
                    *local.entry(id).or_default() += 1.0 / (RRF_K + rank) as f64;
                }
            }
        }
        Ok(())
    };
    // An overflow cursor stores only point identity, not its path or score.
    // Recover its key inside the pinned index, reusing the bounded rank branch
    // for that shard when the main scan reaches it.
    let mut boundary_ranks = None;
    if let Some((version_id, id, category)) = selection.seek() {
        let library = snapshot
            .iter()
            .position(|(_, version)| version.identity().1 == version_id)
            .ok_or(page::STALE_CURSOR)?;
        let shard = measure(&mut timing.shard_open, || snapshot[library].1.open())
            .map_err(|_| SearchError::Store)?;
        let mut rank_score = 0.0;
        if category == Category::Related {
            let mut local = HashMap::new();
            ranks(library, shard, &mut local, timing)?;
            rank_score = local.get(&id).copied().unwrap_or_default();
            boundary_ranks = Some((library, local));
        }
        let mut request = RetrieveRequest::new(vec![PointId::NumId(id)]);
        request.with_payload = Some(text_fields.clone());
        let mut records = shard.retrieve(request).map_err(|_| SearchError::Store)?;
        let record = records.pop().ok_or(page::STALE_CURSOR)?;
        let passage = passage(record.payload)?;
        let (anchor, mut tier) = literal.verify(shard, id, &passage)?;
        if category == Category::Related && tier.is_exact() {
            tier = MatchTier::Related;
        }
        selection.set_boundary(Candidate {
            library,
            id,
            passage,
            anchor,
            tier,
            rank_score,
        })?;
    }
    let mut local = HashMap::new();
    for (library, (_, version)) in snapshot.iter().enumerate() {
        let shard =
            measure(&mut timing.shard_open, || version.open()).map_err(|_| SearchError::Store)?;
        if boundary_ranks
            .as_ref()
            .is_some_and(|(ordinal, _)| *ordinal == library)
        {
            let (_, mut ranked) = boundary_ranks.take().expect("matching boundary ranks");
            std::mem::swap(&mut local, &mut ranked);
        } else {
            ranks(library, shard, &mut local, timing)?;
        }
        selection.record_ranks(&local);
        // Both modes derive exact membership only from the same scan. If there
        // is no safe posting, verify the whole scope in bounded batches.
        measure(&mut timing.exact, || {
            let mut request = ScrollRequest {
                filter: exact_filter.clone().or_else(|| filter.clone()),
                limit: Some(LITERAL_BATCH),
                with_payload: Some(text_fields.clone()),
                ..ScrollRequest::new()
            };
            loop {
                let (records, next) = shard
                    .scroll(request.clone())
                    .map_err(|_| SearchError::Store)?;
                for record in records {
                    let PointId::NumId(id) = record.id else {
                        return Err(SearchError::Store);
                    };
                    let passage = passage(record.payload)?;
                    let (anchor, tier) = literal.verify(shard, id, &passage)?;
                    let rank_score = local.remove(&id);
                    if tier.is_exact() || (!strict && (anchor.is_some() || rank_score.is_some())) {
                        selection.consider(
                            Candidate {
                                library,
                                id,
                                passage,
                                anchor,
                                tier,
                                rank_score: rank_score.unwrap_or_default(),
                            },
                            snapshot,
                        );
                    }
                }
                let Some(next) = next else {
                    return Ok(());
                };
                request.offset = Some(next);
            }
        })?;
        if local.is_empty() {
            continue;
        }
        measure(&mut timing.evidence, || {
            let mut request =
                RetrieveRequest::new(local.keys().copied().map(PointId::NumId).collect());
            request.with_payload = Some(text_fields.clone());
            let records = shard.retrieve(request).map_err(|_| SearchError::Store)?;
            for record in records {
                let PointId::NumId(id) = record.id else {
                    return Err(SearchError::Store);
                };
                let rank_score = local.remove(&id).ok_or(SearchError::Store)?;
                let passage = passage(record.payload)?;
                // Branch-only hits may still have a verified lexical anchor,
                // but ranks cannot extend the shared scan's exact membership.
                let (anchor, tier) = literal.verify(shard, id, &passage)?;
                let tier = if tier.is_exact() {
                    MatchTier::Related
                } else {
                    tier
                };
                selection.consider(
                    Candidate {
                        library,
                        id,
                        passage,
                        anchor,
                        tier,
                        rank_score,
                    },
                    snapshot,
                );
            }
            if local.is_empty() {
                Ok(())
            } else {
                Err(SearchError::Store)
            }
        })?;
    }
    Ok(selection.finish(snapshot))
}

fn diverse_slot(
    best: &[Candidate],
    candidate: &Candidate,
    snapshot: &Snapshot,
) -> Option<(usize, Option<usize>)> {
    let position =
        best.partition_point(|other| candidate_order(other, candidate, snapshot).is_lt());
    if position == RESULT_LIMIT {
        return None;
    }
    if candidate.anchor.is_some() {
        // A large repetitive reference file must not exhaust its literal tier.
        let same_file = |(_, other): &(usize, &Candidate)| {
            candidate.library == other.library
                && candidate.passage.path == other.passage.path
                && other.anchor.is_some()
                && (candidate.tier == other.tier
                    || (candidate.tier.is_exact() && other.tier.is_exact()))
        };
        if let Some((last_index, _)) = best
            .iter()
            .enumerate()
            .filter(same_file)
            .nth(PASSAGES_PER_FILE_PER_BRANCH - 1)
        {
            if last_index < position {
                return None;
            }
            return Some((position, Some(last_index)));
        }
    }
    Some((
        position,
        (best.len() == RESULT_LIMIT).then_some(RESULT_LIMIT - 1),
    ))
}

fn candidate_order(left: &Candidate, right: &Candidate, snapshot: &Snapshot) -> Ordering {
    right
        .tier
        .cmp(&left.tier)
        .then_with(|| {
            // The exact evidence tiers and source tie-break apply in both modes.
            // Branch scores only order related results.
            if left.tier.is_exact() {
                Ordering::Equal
            } else {
                right.rank_score.total_cmp(&left.rank_score)
            }
        })
        .then_with(|| snapshot[left.library].0.cmp(&snapshot[right.library].0))
        .then_with(|| left.passage.path.cmp(&right.passage.path))
        .then_with(|| left.passage.line_start.cmp(&right.passage.line_start))
        .then_with(|| left.passage.byte_start.cmp(&right.passage.byte_start))
        .then_with(|| left.id.cmp(&right.id))
}

fn rerank_window(window: &mut [Evidence], scores: &mut [f64]) {
    // Insertion sort is stable, in-place and bounded by RERANK_LIMIT. A point
    // cannot cross the verified related-evidence tiers.
    for next in 1..window.len() {
        let mut position = next;
        while position > 0
            && window[position].tier == window[position - 1].tier
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
