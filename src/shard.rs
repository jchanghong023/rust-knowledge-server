use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use qdrant_edge::EdgeShard;
use uuid::Uuid;

use crate::{durable, index::EmbeddingGeneration, model_http::Embedding};

enum Location {
    Probe(PathBuf),
    Published(PathBuf),
}

/// A published shard version. The catalog owns one handle per version and
/// verifies the version's integrity manifest before loading it.
pub(crate) struct Shard {
    location: Location,
    opened: OnceLock<io::Result<Opened>>,
}

struct Opened {
    edge: EdgeShard,
    generation: Option<EmbeddingGeneration<String>>,
}

impl Shard {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            location: Location::Probe(path),
            opened: OnceLock::new(),
        }
    }

    pub(crate) fn published(version_path: PathBuf) -> Self {
        Self {
            location: Location::Published(version_path),
            opened: OnceLock::new(),
        }
    }

    /// The constructor path: a version root for published shards, an index for probes.
    pub(crate) fn path(&self) -> &Path {
        match &self.location {
            Location::Probe(path) | Location::Published(path) => path,
        }
    }

    /// Close Edge and release its file handles before returning the path for retirement.
    pub(crate) fn into_path(self) -> PathBuf {
        let Self { location, opened } = self;
        drop(opened);
        match location {
            Location::Probe(path) | Location::Published(path) => path,
        }
    }

    /// Opens once, sharing success or failure for this handle's lifetime.
    /// Service callers must invoke this in the bounded blocking runner.
    pub(crate) fn open(&self) -> Result<&EdgeShard, &io::Error> {
        self.load().map(|opened| &opened.edge)
    }

    pub(crate) fn for_embedding(
        &self,
        id: Uuid,
        provider: &Embedding,
    ) -> Result<Option<&EdgeShard>, &io::Error> {
        let opened = self.load()?;
        Ok(opened
            .generation
            .as_ref()
            .filter(|generation| generation.matches(id, provider))
            .map(|_| &opened.edge))
    }

    fn load(&self) -> Result<&Opened, &io::Error> {
        self.opened
            .get_or_init(|| match &self.location {
                Location::Probe(path) => Ok(Opened {
                    edge: open_existing(path)?,
                    generation: None,
                }),
                Location::Published(version) => Ok(Opened {
                    generation: durable::verify_version(version)?,
                    edge: open_existing(&version.join("index"))?,
                }),
            })
            .as_ref()
    }
}

fn open_existing(path: &Path) -> io::Result<EdgeShard> {
    // Edge can derive a missing config or initialize an empty store. Require
    // both the config and a persisted segment before invoking its normal load.
    let config = path.join("edge_config.json");
    let metadata = config.metadata().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot read {}: {err}", config.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Edge config is not a file: {}", config.display()),
        ));
    }

    let segments = path.join("segments");
    let entries = fs::read_dir(&segments).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot read {}: {err}", segments.display()),
        )
    })?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Edge ignores hidden/deleted entries and segments without version.info.
        if !name.starts_with('.')
            && !name.ends_with(".deleted")
            && entry.file_type()?.is_dir()
            && entry.path().join("version.info").is_file()
        {
            return EdgeShard::load(path, None).map_err(|err| {
                io::Error::other(format!(
                    "cannot open published Edge shard {}: {err}",
                    path.display()
                ))
            });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("no persisted Edge segment in {}", segments.display()),
    ))
}
