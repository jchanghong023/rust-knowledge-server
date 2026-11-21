use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Barrier;
use std::thread;

use qdrant_edge::external::serde_json::json;
use qdrant_edge::{
    Condition, EdgeConfigBuilder, EdgeShard, EdgeSparseVectorParamsBuilder, FieldCondition, Filter,
    IdfCorpusParams, IdfParams, IdfScope, Match, Memory, Modifier, NamedQuery, PointId,
    PointInsertOperations, PointOperations, PointStruct, QueryEnum, QueryRequestBuilder,
    ScoredPoint, ScoringQuery, SearchParams, SparseVector, UpdateOperation, Vectors,
};

use crate::shard::Shard;

const VECTOR_NAME: &str = "text";
const SCOPE_KEY: &str = "in_scope";

type ProbeError = Box<dyn Error + Send + Sync>;

pub(crate) fn run(mode: &str, path: &Path) -> Result<(), Box<dyn Error>> {
    let result = match mode {
        "write" => write_fixture(path),
        "read" => read_fixture(path),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid Edge probe mode {mode:?}; expected write or read"),
        )
        .into()),
    };
    result.map_err(|err| -> Box<dyn Error> { err })
}

fn write_fixture(path: &Path) -> Result<(), ProbeError> {
    // Unlike EdgeShard::new, create_dir also rejects an existing empty directory.
    fs::create_dir(path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("creating Edge probe shard {}: {err}", path.display()),
        )
    })?;
    let config = EdgeConfigBuilder::new()
        .sparse_vector(
            VECTOR_NAME,
            EdgeSparseVectorParamsBuilder::new()
                .memory(Memory::Pinned)
                .modifier(Modifier::Idf)
                .build(),
        )
        // The read probe must see flushed segments, not recovered WAL entries.
        .wal_enabled(false)
        .build();
    let shard = EdgeShard::new(path, config)?;

    // Terms A and B occur once each inside the corpus; point 3 adds only A.
    let fixture = [
        (1_u64, 1, 1.5, true),
        (2, 2, 1.0, true),
        (3, 1, 100.0, false),
    ];
    let mut points = Vec::with_capacity(fixture.len());
    for (id, term, weight, in_scope) in fixture {
        let vector = SparseVector::new(vec![term], vec![weight])?;
        points.push(
            PointStruct::new(
                id,
                Vectors::new_named([(VECTOR_NAME, vector)]),
                json!({ (SCOPE_KEY): in_scope }),
            )
            .into(),
        );
    }
    shard.update(UpdateOperation::PointOperation(
        PointOperations::UpsertPoints(PointInsertOperations::PointsList(points)),
    ))?;
    shard.flush()?;
    Ok(())
}

fn read_fixture(path: &Path) -> Result<(), ProbeError> {
    let shard = Shard::new(path.to_path_buf());
    let barrier = Barrier::new(2);
    let read = || {
        barrier.wait();
        let edge = shard
            .open()
            .map_err(|err| io::Error::new(err.kind(), err.to_string()))?;
        Ok::<_, ProbeError>((edge, query_fixture(edge)?))
    };
    let (first, second) = thread::scope(|scope| {
        let worker = scope.spawn(&read);
        let first = read();
        (first, worker.join())
    });
    let (
        first_edge,
        [
            scoped_first,
            scoped_second,
            global_first,
            global_second,
            outside,
        ],
    ) = first?;
    let (second_edge, _) = second.map_err(|_| io::Error::other("Edge probe reader panicked"))??;
    if !std::ptr::eq(first_edge, second_edge) {
        return Err(io::Error::other("Edge probe readers opened different shard instances").into());
    }

    writeln!(
        io::stdout().lock(),
        "edge-probe: scoped=[1:{scoped_first:.4},2:{scoped_second:.4}] global=[2:{global_first:.4},1:{global_second:.4}] unfiltered_top=[3:{outside:.4}]",
    )?;
    Ok(())
}

fn query_fixture(shard: &EdgeShard) -> Result<[f32; 5], ProbeError> {
    let scope = Filter::new_must(Condition::Field(FieldCondition::new_match(
        SCOPE_KEY
            .parse()
            .map_err(|()| io::Error::other("invalid Edge probe payload key"))?,
        Match::from(true),
    )));
    let query = ScoringQuery::Vector(QueryEnum::Nearest(NamedQuery {
        query: SparseVector::new(vec![1, 2], vec![1.0, 1.0])?.into(),
        using: Some(VECTOR_NAME.to_owned()),
    }));
    let global_params = SearchParams {
        idf: Some(IdfParams::Scope(IdfScope::Global)),
        ..SearchParams::default()
    };
    let scoped_params = SearchParams {
        idf: Some(IdfParams::Corpus(IdfCorpusParams {
            corpus: scope.clone(),
        })),
        ..SearchParams::default()
    };
    let candidates = QueryRequestBuilder::new(2)
        .query(query.clone())
        .filter(scope);
    let scoped = shard.query(candidates.clone().params(scoped_params).build())?;
    let global = shard.query(candidates.params(global_params.clone()).build())?;
    let unfiltered = shard.query(
        QueryRequestBuilder::new(1)
            .query(query)
            .params(global_params)
            .build(),
    )?;

    // Edge uses IDF = ln((N + 1) / (df + 0.5)). Scoped scores must match
    // the two-document corpus alone, despite the persisted outside vector.
    let scoped_idf = std::f32::consts::LN_2;
    let global_a = 1.6_f32.ln();
    let global_b = (8.0_f32 / 3.0).ln();
    let [scoped_first, scoped_second] = expect_hits(
        "scoped IDF",
        &scoped,
        [(1, 1.5 * scoped_idf), (2, scoped_idf)],
    )?;
    let [global_first, global_second] = expect_hits(
        "global IDF with scoped candidates",
        &global,
        [(2, global_b), (1, 1.5 * global_a)],
    )?;
    let [outside] = expect_hits(
        "unfiltered global IDF",
        &unfiltered,
        [(3, 100.0 * global_a)],
    )?;

    Ok([
        scoped_first,
        scoped_second,
        global_first,
        global_second,
        outside,
    ])
}

fn expect_hits<const N: usize>(
    label: &str,
    hits: &[ScoredPoint],
    expected: [(u64, f32); N],
) -> io::Result<[f32; N]> {
    if hits.len() != N {
        return Err(io::Error::other(format!(
            "{label}: expected {N} hits, got {}",
            hits.len()
        )));
    }
    let mut scores = [0.0; N];
    for ((hit, (id, score)), actual_score) in hits.iter().zip(expected).zip(&mut scores) {
        if hit.id != PointId::from(id)
            || !hit.score.is_finite()
            || hit.score <= 0.0
            || (hit.score - score).abs() > 0.0001
        {
            return Err(io::Error::other(format!(
                "{label}: expected id {id} with score {score:.6}, got id {:?} with score {:.6}",
                hit.id, hit.score,
            )));
        }
        *actual_score = hit.score;
    }
    Ok(scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_published_segments_are_not_recreated_or_retried() -> Result<(), ProbeError> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("shard");
        let shard = Shard::new(path.clone());
        write_fixture(&path)?;
        let segments = path.join("segments");
        let saved = root.path().join("saved-segments");
        fs::rename(&segments, &saved)?;

        let Err(missing) = shard.open() else {
            panic!("opened a published shard with no segments directory");
        };
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(!segments.try_exists()?);

        fs::create_dir(&segments)?;
        let empty_shard = Shard::new(path.clone());
        let Err(empty) = empty_shard.open() else {
            panic!("opened a published shard with an empty segments directory");
        };
        assert_eq!(empty.kind(), io::ErrorKind::InvalidData);
        assert!(fs::read_dir(&segments)?.next().is_none());
        fs::remove_dir(&segments)?;
        fs::rename(&saved, &segments)?;

        let Err(cached) = shard.open() else {
            panic!("retried a failed shard open after its segments were restored");
        };
        assert!(std::ptr::eq(missing, cached));
        let fresh = Shard::new(path);
        let edge = fresh
            .open()
            .map_err(|err| io::Error::new(err.kind(), err.to_string()))?;
        query_fixture(edge)?;
        Ok(())
    }
}
