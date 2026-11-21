use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
};

use sha2::{Digest, Sha256};

use super::{
    Candidate, Catalog, Category, Embedding, Evidence, FusionMode, RESULT_LIMIT, Rerank, Retrieval,
    Scope, SearchError, SearchReply, SearchRequest, Snapshot, Source, Status, Uuid,
    candidate_order, diverse_slot, execute,
};
use crate::catalog::Version;

// Keep room for aliases plus one raw lookahead. Requests that consume the
// whole usable window can continue even if some entries were combined.
const PAGE_WINDOW: usize = 2 * RESULT_LIMIT + 1;
const REPLAY_CACHE_SIZE: usize = 32;
const INVALID_CURSOR: SearchError =
    SearchError::InvalidInput("invalid search cursor; restart without a cursor");
pub(super) const STALE_CURSOR: SearchError =
    SearchError::InvalidInput("stale search cursor; restart without a cursor");

pub(crate) async fn page(
    catalog: &Catalog,
    request: SearchRequest,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<&Rerank>,
    fusion: FusionMode,
    cursor: Option<&str>,
) -> Result<Page, SearchError> {
    let paging = Paging {
        resume: cursor.map(Cursor::decode).transpose()?,
        binding: [0; 32],
        replay: None,
    };
    execute(catalog, request, embedding, rerank, fusion, Some(paging)).await
}

pub(crate) struct Page {
    pub(crate) reply: SearchReply,
    snapshot: Snapshot,
    continuation: Option<Continuation>,
}

impl Page {
    pub(super) fn unpaged(reply: SearchReply) -> Self {
        Self {
            reply,
            snapshot: Vec::new(),
            continuation: None,
        }
    }

    pub(crate) fn version(&self, source: &Source) -> Option<&Version> {
        self.snapshot.iter().find_map(|(_, version)| {
            (version.identity() == (source.library, source.version)).then_some(version.as_ref())
        })
    }

    pub(crate) fn next_cursor(&self, consumed: usize) -> Result<Option<String>, SearchError> {
        let Some(next) = &self.continuation else {
            return Ok(None);
        };
        if consumed > self.reply.matches.len() {
            return Err(SearchError::Store);
        }
        if consumed == self.reply.matches.len() && !next.more {
            return Ok(None);
        }
        let previous = consumed.checked_sub(1).ok_or(SearchError::Store)?;
        let source = &self.reply.matches[previous].source;
        remember(&next.replay);
        Ok(Some(
            Cursor {
                stage: next.stages[previous],
                version: source.version,
                point: source.point,
                binding: next.binding,
                replay: next.replay.id,
            }
            .encode(),
        ))
    }
}

struct Continuation {
    stages: [Stage; PAGE_WINDOW],
    more: bool,
    binding: Fingerprint,
    replay: Arc<Ranking>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Stage {
    ExactHead = 1,
    ExactTail,
    RelatedHead,
    RelatedTail,
}

impl Stage {
    fn category(self) -> Category {
        match self {
            Self::ExactHead | Self::ExactTail => Category::Exact,
            Self::RelatedHead | Self::RelatedTail => Category::Related,
        }
    }
}

type Fingerprint = [u8; 32];
const CURSOR_BODY: usize = 1 + 16 + 8 + 32 + 16;
const CURSOR_BYTES: usize = CURSOR_BODY + 32;

struct Cursor {
    stage: Stage,
    version: Uuid,
    point: u64,
    binding: Fingerprint,
    replay: Uuid,
}

fn keyed_digest(mut digest: Sha256) -> Fingerprint {
    // Tokens and fingerprints are process-local, with distinct domain tags.
    // A secret suffix prevents forging a continuation or guessing its scope.
    static KEY: OnceLock<([u8; 16], [u8; 16])> = OnceLock::new();
    let (first, second) =
        KEY.get_or_init(|| (*Uuid::new_v4().as_bytes(), *Uuid::new_v4().as_bytes()));
    digest.update(first);
    digest.update(second);
    digest.finalize().into()
}

fn cursor_tag(bytes: &[u8]) -> Fingerprint {
    let mut digest = Sha256::new();
    digest.update(b"knowledge-search-cursor-v1");
    digest.update(bytes);
    keyed_digest(digest)
}

impl Cursor {
    fn encode(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0; CURSOR_BYTES];
        bytes[0] = self.stage as u8;
        bytes[1..17].copy_from_slice(self.version.as_bytes());
        bytes[17..25].copy_from_slice(&self.point.to_le_bytes());
        bytes[25..57].copy_from_slice(&self.binding);
        bytes[57..73].copy_from_slice(self.replay.as_bytes());
        let tag = cursor_tag(&bytes[..CURSOR_BODY]);
        bytes[CURSOR_BODY..].copy_from_slice(&tag);
        let mut encoded = String::with_capacity(CURSOR_BYTES * 2);
        for byte in bytes {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
        encoded
    }

    fn decode(encoded: &str) -> Result<Self, SearchError> {
        if encoded.len() != CURSOR_BYTES * 2 {
            return Err(INVALID_CURSOR);
        }
        let mut bytes = [0; CURSOR_BYTES];
        let digit = |byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(INVALID_CURSOR),
        };
        for (output, pair) in bytes.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
            *output = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        let tag = cursor_tag(&bytes[..CURSOR_BODY]);
        let tag_mismatch = tag
            .iter()
            .zip(&bytes[CURSOR_BODY..])
            .fold(0, |acc, (a, b)| acc | (a ^ b));
        if tag_mismatch != 0 {
            return Err(INVALID_CURSOR);
        }
        let stage = match bytes[0] {
            1 => Stage::ExactHead,
            2 => Stage::ExactTail,
            3 => Stage::RelatedHead,
            4 => Stage::RelatedTail,
            _ => return Err(INVALID_CURSOR),
        };
        Ok(Self {
            stage,
            version: Uuid::from_bytes(bytes[1..17].try_into().expect("cursor version width")),
            point: u64::from_le_bytes(bytes[17..25].try_into().expect("cursor point width")),
            binding: bytes[25..57].try_into().expect("cursor binding width"),
            replay: Uuid::from_bytes(bytes[57..73].try_into().expect("ranking ID width")),
        })
    }
}

pub(super) struct Paging {
    resume: Option<Cursor>,
    binding: Fingerprint,
    replay: Option<Arc<Ranking>>,
}

impl Paging {
    pub(super) fn is_continuation(&self) -> bool {
        self.resume.is_some()
    }

    pub(super) fn bind(
        &mut self,
        query: &str,
        scope: &Scope,
        strict: bool,
        fusion: FusionMode,
        snapshot: &Snapshot,
    ) -> Result<(), SearchError> {
        let mut digest = Sha256::new();
        digest.update(b"knowledge-search-binding-v1");
        digest.update([u8::from(strict), u8::from(fusion == FusionMode::Edge)]);
        for field in [Some(query), scope.library.as_deref(), scope.path.as_deref()] {
            if let Some(field) = field {
                digest.update([1]);
                digest.update((field.len() as u64).to_le_bytes());
                digest.update(field.as_bytes());
            } else {
                digest.update([0]);
            }
        }
        for (_, version) in snapshot {
            let (library, version) = version.identity();
            digest.update(library.as_bytes());
            digest.update(version.as_bytes());
        }
        self.binding = keyed_digest(digest);
        if let Some(resume) = &self.resume {
            if resume.binding != self.binding {
                return Err(STALE_CURSOR);
            }
            self.replay = Some(recalled(resume.replay).ok_or(STALE_CURSOR)?);
        }
        Ok(())
    }

    pub(super) fn replay(&self) -> Option<&Arc<Ranking>> {
        self.replay.as_ref()
    }
}

// The grouped Edge query can choose different points at a tied cutoff.
// Cache only bounded branch ranks and the head's order, never query or evidence text.
pub(super) struct Ranking {
    id: Uuid,
    ranks: Vec<Box<[(u64, f64)]>>,
    head: Vec<(Uuid, u64)>,
    pub(super) retrieval: Retrieval,
    reranked: bool,
}

impl Ranking {
    pub(super) fn ranks(&self, library: usize, into: &mut HashMap<u64, f64>) {
        into.extend(self.ranks[library].iter().copied());
    }
}

fn cache() -> &'static Mutex<VecDeque<Arc<Ranking>>> {
    static CACHE: OnceLock<Mutex<VecDeque<Arc<Ranking>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(VecDeque::with_capacity(REPLAY_CACHE_SIZE)))
}

fn remember(ranking: &Arc<Ranking>) {
    let mut cache = cache().lock().expect("search cursor cache");
    if let Some(at) = cache.iter().position(|entry| entry.id == ranking.id) {
        let existing = cache.remove(at).expect("existing ranking");
        cache.push_back(existing);
    } else {
        if cache.len() == REPLAY_CACHE_SIZE {
            cache.pop_front();
        }
        cache.push_back(Arc::clone(ranking));
    }
}

fn recalled(id: Uuid) -> Option<Arc<Ranking>> {
    cache()
        .lock()
        .expect("search cursor cache")
        .iter()
        .find(|ranking| ranking.id == id)
        .cloned()
}

pub(super) struct Selection {
    head: Vec<Candidate>,
    tail: Option<Tail>,
}

struct Tail {
    paging: Paging,
    exact: Vec<Candidate>,
    related: Vec<Candidate>,
    boundary: Option<Candidate>,
    ranks: Vec<Box<[(u64, f64)]>>,
}

impl Selection {
    pub(super) fn new(paging: Option<Paging>) -> Self {
        Self {
            head: Vec::with_capacity(RESULT_LIMIT),
            tail: paging.map(|paging| Tail {
                paging,
                exact: Vec::new(),
                related: Vec::new(),
                boundary: None,
                ranks: Vec::new(),
            }),
        }
    }

    pub(super) fn seek(&self) -> Option<(Uuid, u64, Category)> {
        let cursor = self.tail.as_ref()?.paging.resume.as_ref()?;
        matches!(cursor.stage, Stage::ExactTail | Stage::RelatedTail).then_some((
            cursor.version,
            cursor.point,
            cursor.stage.category(),
        ))
    }

    pub(super) fn set_boundary(&mut self, candidate: Candidate) -> Result<(), SearchError> {
        let (_, _, category) = self.seek().ok_or(SearchError::Store)?;
        if candidate.category() != category {
            return Err(STALE_CURSOR);
        }
        self.tail.as_mut().expect("paged selection").boundary = Some(candidate);
        Ok(())
    }

    pub(super) fn record_ranks(&mut self, local: &HashMap<u64, f64>) {
        if let Some(tail) = &mut self.tail
            && tail.paging.replay.is_none()
        {
            tail.ranks
                .push(local.iter().map(|(&id, &score)| (id, score)).collect());
        }
    }

    pub(super) fn consider(&mut self, candidate: Candidate, snapshot: &Snapshot) {
        let displaced = match diverse_slot(&self.head, &candidate, snapshot) {
            Some((position, remove)) => {
                let displaced = remove.map(|index| self.head.remove(index));
                self.head.insert(position, candidate);
                displaced
            }
            None => Some(candidate),
        };
        if let (Some(tail), Some(candidate)) = (&mut self.tail, displaced) {
            tail.consider(candidate, snapshot);
        }
    }

    pub(super) fn finish(self, snapshot: &Snapshot) -> Ranked {
        Ranked {
            head: self
                .head
                .into_iter()
                .map(|candidate| candidate.into_evidence(snapshot))
                .collect(),
            tail: self.tail,
        }
    }
}

impl Tail {
    fn consider(&mut self, candidate: Candidate, snapshot: &Snapshot) {
        let category = candidate.category();
        let resume = self.paging.resume.as_ref().map(|cursor| cursor.stage);
        if category == Category::Exact
            && matches!(resume, Some(Stage::RelatedHead | Stage::RelatedTail))
        {
            return;
        }
        if self.boundary.as_ref().is_some_and(|boundary| {
            boundary.category() == category
                && !candidate_order(boundary, &candidate, snapshot).is_lt()
        }) {
            return;
        }
        let best = if category == Category::Exact {
            &mut self.exact
        } else {
            &mut self.related
        };
        let slot =
            best.partition_point(|other| candidate_order(other, &candidate, snapshot).is_lt());
        if slot < PAGE_WINDOW {
            if best.len() == PAGE_WINDOW {
                best.pop();
            }
            best.insert(slot, candidate);
        }
    }
}

pub(super) struct Ranked {
    pub(super) head: Vec<Evidence>,
    tail: Option<Tail>,
}

impl Ranked {
    pub(super) fn finish(
        self,
        scope: Scope,
        retrieval: Retrieval,
        reranked: bool,
        snapshot: Snapshot,
    ) -> Result<Page, SearchError> {
        let mut reply = SearchReply {
            scope,
            status: Status::Ok,
            matches: self.head,
            retrieval,
            reranked,
        };
        let Some(tail) = self.tail else {
            return Ok(Page::unpaged(reply));
        };
        let resume = tail.paging.resume.as_ref();
        let replay = if let Some(replay) = tail.paging.replay {
            if replay.head.len() != reply.matches.len() {
                return Err(STALE_CURSOR);
            }
            for (index, &wanted) in replay.head.iter().enumerate() {
                let position = reply.matches[index..]
                    .iter()
                    .position(|hit| (hit.source.version, hit.source.point) == wanted)
                    .ok_or(STALE_CURSOR)?;
                reply.matches.swap(index, index + position);
            }
            reply.reranked = replay.reranked;
            replay
        } else {
            Arc::new(Ranking {
                id: Uuid::new_v4(),
                ranks: tail.ranks,
                head: reply
                    .matches
                    .iter()
                    .map(|hit| (hit.source.version, hit.source.point))
                    .collect(),
                retrieval,
                reranked,
            })
        };

        let exact_count = reply
            .matches
            .partition_point(|item| item.category == Category::Exact);
        let (exact_head, related_head) = reply.matches.split_at(exact_count);
        let skip_exact = skip_head(resume, Stage::ExactHead, exact_head)?;
        let skip_related = skip_head(resume, Stage::RelatedHead, related_head)?;
        let mut head = std::mem::take(&mut reply.matches).into_iter();
        let mut items = Vec::with_capacity(PAGE_WINDOW);
        let mut stages = [Stage::ExactHead; PAGE_WINDOW];
        append(
            &mut items,
            &mut stages,
            Stage::ExactHead,
            head.by_ref().take(exact_count).skip(skip_exact),
        );
        append(
            &mut items,
            &mut stages,
            Stage::ExactTail,
            tail.exact
                .into_iter()
                .map(|candidate| candidate.into_evidence(&snapshot)),
        );
        append(
            &mut items,
            &mut stages,
            Stage::RelatedHead,
            head.skip(skip_related),
        );
        append(
            &mut items,
            &mut stages,
            Stage::RelatedTail,
            tail.related
                .into_iter()
                .map(|candidate| candidate.into_evidence(&snapshot)),
        );
        // Reserve the last item as lookahead. Each tail keeps its own full
        // window, so truncation cannot hide a successor without lookahead.
        let more = items.len() == PAGE_WINDOW;
        if more {
            items.pop();
        }
        reply.matches = items;
        Ok(Page {
            reply,
            snapshot,
            continuation: Some(Continuation {
                stages,
                more,
                binding: tail.paging.binding,
                replay,
            }),
        })
    }
}

fn append(
    items: &mut Vec<Evidence>,
    stages: &mut [Stage; PAGE_WINDOW],
    stage: Stage,
    entries: impl Iterator<Item = Evidence>,
) {
    for evidence in entries.take(PAGE_WINDOW - items.len()) {
        stages[items.len()] = stage;
        items.push(evidence);
    }
}

fn skip_head(
    cursor: Option<&Cursor>,
    stage: Stage,
    items: &[Evidence],
) -> Result<usize, SearchError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    if cursor.stage as u8 > stage as u8 {
        return Ok(items.len());
    }
    if cursor.stage != stage {
        return Ok(0);
    }
    items
        .iter()
        .position(|item| item.source.version == cursor.version && item.source.point == cursor.point)
        .map(|position| position + 1)
        .ok_or(STALE_CURSOR)
}
