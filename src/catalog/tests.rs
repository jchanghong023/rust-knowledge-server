use qdrant_edge::external::serde_json::json;
use qdrant_edge::{
    EdgeConfigBuilder, EdgeSparseVectorParamsBuilder, Memory, NamedQuery, PointInsertOperations,
    PointOperations, PointStruct, QueryEnum, QueryRequestBuilder, ScoringQuery, SparseVector,
    UpdateOperation, Vectors,
};

use super::*;

const TEXT: &str = "text";
type Evidence<'a> = (u32, &'a str, &'a str);

fn reopen(path: &Path) -> Catalog {
    Catalog::open(Root::open(path).unwrap()).unwrap()
}

fn write_index(build: &Build, evidence: &[Evidence<'_>]) {
    let config = EdgeConfigBuilder::new()
        .sparse_vector(
            TEXT,
            EdgeSparseVectorParamsBuilder::new()
                .memory(Memory::Pinned)
                .build(),
        )
        .wal_enabled(false)
        .build();
    let shard = EdgeShard::new(&build.index_path(), config).unwrap();
    let points = evidence
        .iter()
        .enumerate()
        .map(|(offset, &(term, path, body))| {
            let vector = SparseVector::new(vec![term], vec![1.0]).unwrap();
            PointStruct::new(
                (offset + 1) as u64,
                Vectors::new_named([(TEXT, vector)]),
                json!({"path": path, "body": body}),
            )
            .into()
        })
        .collect();
    shard
        .update(UpdateOperation::PointOperation(
            PointOperations::UpsertPoints(PointInsertOperations::PointsList(points)),
        ))
        .unwrap();
    shard.flush().unwrap();
}

fn query(snapshot: &Snapshot, library: &str, term: u32) -> Vec<(String, String)> {
    let (_, version) = snapshot.iter().find(|(name, _)| name == library).unwrap();
    let nearest = NamedQuery {
        query: SparseVector::new(vec![term], vec![1.0]).unwrap().into(),
        using: Some(TEXT.to_owned()),
    };
    let request = QueryRequestBuilder::new(5)
        .query(ScoringQuery::Vector(QueryEnum::Nearest(nearest)))
        .with_payload(true.into())
        .build();
    version
        .open()
        .unwrap()
        .query(request)
        .unwrap()
        .into_iter()
        .map(|point| {
            let payload = point.payload.unwrap();
            (
                payload.0["path"].as_str().unwrap().to_owned(),
                payload.0["body"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn expect_evidence(snapshot: &Snapshot, name: &str, (term, path, body): Evidence<'_>) {
    assert_eq!(
        query(snapshot, name, term),
        [(path.to_owned(), body.to_owned())]
    );
}

fn published_path(catalog: &Catalog, name: &str) -> PathBuf {
    let version_id = catalog
        .list()
        .unwrap()
        .into_iter()
        .find(|library| library.name == name)
        .unwrap()
        .version_id
        .unwrap();
    id_path(&catalog.versions_root, version_id)
}

#[test]
fn complete_replacement_and_deletion_preserve_pinned_snapshots_and_receipts() {
    const LIBRARY: &str = " 中文 库 ";
    const OTHER: &str = "CON";
    const A_OLD: Evidence<'_> = (1, "nested/a.md", "old evidence");
    const A_NEW: Evidence<'_> = (1, "nested/a.md", "new evidence");
    const OMITTED: Evidence<'_> = (2, "b.md", "omitted from the reimport");
    const OTHER_DOC: Evidence<'_> = (3, "a.md", "untouched evidence");

    let disk = tempfile::tempdir().unwrap();
    let source = disk.path().join("b.md");
    fs::write(&source, OMITTED.2).unwrap();
    let root = disk.path().join("service");
    let catalog = reopen(&root);
    assert!(catalog.snapshot(None).unwrap().is_empty());
    assert!(matches!(
        catalog.create("\t", None),
        Err(CatalogError::InvalidName)
    ));
    let create_id = Uuid::new_v4();
    let create = catalog.create(LIBRARY, Some(create_id)).unwrap();
    assert!(matches!(
        catalog.create(LIBRARY, None),
        Err(CatalogError::AlreadyExists)
    ));
    assert!(matches!(
        catalog.create(LIBRARY, Some(create_id)),
        Err(CatalogError::OperationCommitted)
    ));
    assert_eq!(catalog.receipt(create_id).unwrap(), Some(create));
    assert!(catalog.snapshot(Some(LIBRARY)).unwrap().is_empty());
    catalog.create(OTHER, None).unwrap();

    for (name, docs) in [(LIBRARY, &[A_OLD, OMITTED][..]), (OTHER, &[OTHER_DOC][..])] {
        let build = catalog.start_build(name).unwrap();
        write_index(&build, docs);
        catalog.publish(build, None).unwrap();
    }
    let old_path = published_path(&catalog, LIBRARY);
    let pinned = catalog.snapshot(None).unwrap();
    expect_evidence(&pinned, LIBRARY, A_OLD);

    let replacement = catalog.start_build(LIBRARY).unwrap();
    let lost_race = catalog.start_build(LIBRARY).unwrap();
    write_index(&replacement, &[A_NEW]);
    let publish_id = Uuid::new_v4();
    let publish = catalog.publish(replacement, Some(publish_id)).unwrap();
    assert!(matches!(
        catalog.publish(lost_race, None),
        Err(CatalogError::StaleBuild)
    ));

    let current = catalog.snapshot(None).unwrap();
    expect_evidence(&current, LIBRARY, A_NEW);
    assert!(query(&current, LIBRARY, OMITTED.0).is_empty());
    expect_evidence(&pinned, LIBRARY, OMITTED);
    expect_evidence(&pinned, OTHER, OTHER_DOC);
    assert!(old_path.is_dir());
    drop(pinned);
    assert!(!old_path.exists());

    let stale = catalog.start_build(LIBRARY).unwrap();
    let delete_id = Uuid::new_v4();
    let delete = catalog.delete(LIBRARY, Some(delete_id)).unwrap();
    assert!(matches!(
        catalog.snapshot(Some(LIBRARY)),
        Err(CatalogError::NotFound)
    ));
    let recreate = catalog.create(LIBRARY, None).unwrap();
    assert_ne!(recreate, create);
    assert!(matches!(
        catalog.publish(stale, None),
        Err(CatalogError::StaleBuild)
    ));
    assert!(catalog.snapshot(Some(LIBRARY)).unwrap().is_empty());

    // With neither incarnation published, the library ID alone must reject
    // a lease held across delete and recreate.
    let empty_lease = catalog.start_build(LIBRARY).unwrap();
    catalog.delete(LIBRARY, None).unwrap();
    catalog.create(LIBRARY, None).unwrap();
    assert!(matches!(
        catalog.publish(empty_lease, None),
        Err(CatalogError::StaleBuild)
    ));
    expect_evidence(&current, LIBRARY, A_NEW);
    drop(catalog);
    assert_eq!(
        Root::open(&root).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(current);

    let catalog = reopen(&root);
    expect_evidence(&catalog.snapshot(None).unwrap(), OTHER, OTHER_DOC);
    assert!(catalog.snapshot(Some(LIBRARY)).unwrap().is_empty());
    assert_eq!(fs::read_to_string(source).unwrap(), OMITTED.2);
    for (id, receipt) in [
        (create_id, create),
        (publish_id, publish),
        (delete_id, delete),
    ] {
        assert_eq!(catalog.receipt(id).unwrap(), Some(receipt));
    }
    catalog.clear_receipt(publish_id).unwrap();
    drop(catalog);
    let catalog = reopen(&root);
    assert_eq!(catalog.receipt(publish_id).unwrap(), None);
    assert_eq!(catalog.receipt(delete_id).unwrap(), Some(delete));
}

#[test]
fn restart_reclaims_only_owned_orphans_and_keeps_previous_evidence() {
    let disk = tempfile::tempdir().unwrap();
    let catalog = reopen(disk.path());
    catalog.create("docs", None).unwrap();
    let old = (1, "a.md", "published");
    let initial = catalog.start_build("docs").unwrap();
    write_index(&initial, &[old]);
    catalog.publish(initial, None).unwrap();
    let mut interrupted = catalog.start_build("docs").unwrap();
    write_index(&interrupted, &[(1, "a.md", "half finished")]);
    // Suppress this process's Build::drop to model the abandoned build on
    // disk left by an interrupted process whose root lock has been released.
    interrupted.cleanup = false;
    let abandoned = interrupted.path.clone();
    let orphan = id_path(&catalog.versions_root, Uuid::new_v4());
    let unknown_version = catalog.versions_root.join(".build-not-a-uuid");
    let unknown_upload = catalog.stage_root().join("import-operator-notes");
    let abandoned_upload = tempfile::Builder::new()
        .prefix("import-")
        .tempdir_in(catalog.stage_root())
        .unwrap()
        .keep();
    let temp_table = catalog
        .root_path()
        .join(format!(".catalog.json-{}.tmp", Uuid::new_v4()));
    fs::write(&temp_table, "abandoned replacement").unwrap();
    for dir in [&orphan, &unknown_version, &unknown_upload] {
        fs::create_dir(dir).unwrap();
    }
    drop(catalog);
    assert_eq!(
        Root::open(disk.path()).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(interrupted);

    let catalog = reopen(disk.path());
    expect_evidence(&catalog.snapshot(None).unwrap(), "docs", old);
    for path in [abandoned, orphan, abandoned_upload, temp_table] {
        assert!(!path.exists(), "left managed orphan {}", path.display());
    }
    assert!(unknown_version.is_dir());
    assert!(unknown_upload.is_dir());
}

#[test]
fn a_missing_segment_is_a_lazy_storage_error_and_remains_missing() {
    let disk = tempfile::tempdir().unwrap();
    let catalog = reopen(disk.path());
    catalog.create("docs", None).unwrap();
    let build = catalog.start_build("docs").unwrap();
    write_index(&build, &[(1, "a.md", "published")]);
    catalog.publish(build, None).unwrap();
    let index = published_path(&catalog, "docs").join("index");
    let segments = index.join("segments");
    drop(catalog);
    let segment = fs::read_dir(&segments)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::remove_dir_all(&segment).unwrap();
    assert!(index.join("edge_config.json").is_file());

    let catalog = reopen(disk.path());
    let snapshot = catalog.snapshot(Some("docs")).unwrap();
    let version = &snapshot[0].1;
    let Err(first) = version.open() else {
        panic!("accepted a missing segment")
    };
    let Err(second) = version.open() else {
        panic!("retried a corrupt published version")
    };
    assert!(std::ptr::eq(first, second));
    assert!(index.join("edge_config.json").is_file());
    assert!(!segment.exists());
    assert!(fs::read_dir(segments).unwrap().next().is_none());
}

#[test]
fn failed_table_replace_poisons_catalog_and_preserves_candidates_until_restart() {
    let disk = tempfile::tempdir().unwrap();
    let catalog = reopen(disk.path());
    catalog.create("docs", None).unwrap();
    let old = (1, "a.md", "old evidence");
    let initial = catalog.start_build("docs").unwrap();
    write_index(&initial, &[old]);
    catalog.publish(initial, None).unwrap();
    let previous = published_path(&catalog, "docs");
    let replacement = catalog.start_build("docs").unwrap();
    write_index(&replacement, &[(1, "a.md", "new evidence")]);
    let candidate = id_path(&catalog.versions_root, replacement.version_id);
    let pinned = catalog.snapshot(None).unwrap();

    // Replacing a directory with a file fails on either platform. Preserve
    // the last durable file to model the pre-commit outcome upon restart.
    let table = catalog.table_path.clone();
    let saved_table = catalog.root_path().join("saved-catalog");
    fs::rename(&table, &saved_table).unwrap();
    fs::create_dir(&table).unwrap();
    let publish_id = Uuid::new_v4();
    assert!(matches!(
        catalog.publish(replacement, Some(publish_id)),
        Err(CatalogError::CommitUncertain(_))
    ));
    assert!(matches!(catalog.list(), Err(CatalogError::Unavailable)));
    assert!(matches!(
        catalog.snapshot(None),
        Err(CatalogError::Unavailable)
    ));
    assert!(matches!(
        catalog.receipt(publish_id),
        Err(CatalogError::Unavailable)
    ));
    assert!(matches!(
        catalog.create("other", None),
        Err(CatalogError::Unavailable)
    ));
    expect_evidence(&pinned, "docs", old);
    drop(pinned);
    drop(catalog);
    assert!(previous.is_dir());
    assert!(candidate.is_dir());

    fs::remove_dir(&table).unwrap();
    fs::rename(saved_table, table).unwrap();
    let catalog = reopen(disk.path());
    expect_evidence(&catalog.snapshot(None).unwrap(), "docs", old);
    assert_eq!(catalog.receipt(publish_id).unwrap(), None);
    assert!(!candidate.exists());
}

#[test]
fn missing_catalog_preserves_published_version_for_recovery() {
    let disk = tempfile::tempdir().unwrap();
    let catalog = reopen(disk.path());
    catalog.create("docs", None).unwrap();
    let old = (1, "a.md", "published");
    let build = catalog.start_build("docs").unwrap();
    write_index(&build, &[old]);
    catalog.publish(build, None).unwrap();
    let published = published_path(&catalog, "docs");
    let table = catalog.table_path.clone();
    let saved = fs::read(&table).unwrap();
    drop(catalog);

    fs::remove_file(&table).unwrap();
    let Err(CatalogError::Storage(error)) = Catalog::open(Root::open(disk.path()).unwrap()) else {
        panic!("startup must refuse to replace a missing catalog with published data");
    };
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(published.join("integrity.json").is_file());
    assert!(!table.exists());

    fs::write(table, saved).unwrap();
    let recovered = reopen(disk.path());
    expect_evidence(&recovered.snapshot(None).unwrap(), "docs", old);
}

#[test]
fn failed_seal_and_precommit_write_leave_the_catalog_queryable() {
    let disk = tempfile::tempdir().unwrap();
    let mut catalog = reopen(disk.path());
    catalog.create("docs", None).unwrap();
    let old = (1, "a.md", "published");
    let first = catalog.start_build("docs").unwrap();
    write_index(&first, &[old]);
    catalog.publish(first, None).unwrap();

    let unsealed = catalog.start_build("docs").unwrap();
    assert!(matches!(
        catalog.publish(unsealed, None),
        Err(CatalogError::Storage(_))
    ));
    expect_evidence(&catalog.snapshot(None).unwrap(), "docs", old);

    let replacement = catalog.start_build("docs").unwrap();
    write_index(&replacement, &[(1, "a.md", "replacement")]);
    let candidate = id_path(&catalog.versions_root, replacement.version_id);
    let op = Uuid::new_v4();
    catalog.table_path = disk.path().join("missing").join(CATALOG);
    assert!(matches!(
        catalog.publish(replacement, Some(op)),
        Err(CatalogError::Storage(_))
    ));
    assert_eq!(catalog.receipt(op).unwrap(), None);
    expect_evidence(&catalog.snapshot(None).unwrap(), "docs", old);
    assert!(!candidate.exists());
}
