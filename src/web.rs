//! Browser-only API: an in-process session establishes the self-reported audit actor.

use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque, hash_map::Entry},
    io::{self, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, FromRequestParts, Multipart, Path, Query, State,
        multipart::MultipartRejection,
        rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use tokio::{
    runtime::Handle,
    sync::{Semaphore, oneshot},
};
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::{
    audit::{Action, Audit, Operation, OperationStatus, Outcome, valid_employee},
    catalog::{Build, Catalog, CatalogError, INVALID_NAME, Receipt},
    import::{self, Batch, ImportError},
    index,
    model_http::{Embedding, Rerank},
    search::{self, FusionMode, SearchError, SearchRequest},
    settings::HttpAuthority,
};

const COOKIE: &str = "knowledge_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 4096;
const SESSION_BODY_LIMIT: usize = 1024;
const JSON_BODY_LIMIT: usize = 32 * 1024;

pub(crate) fn router(
    catalog: Arc<Catalog>,
    audit: Arc<Audit>,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<Arc<Rerank>>,
    fusion: FusionMode,
    jobs: TaskTracker,
) -> Router {
    Router::new()
        .route(
            "/api/session",
            get(get_session)
                .post(register)
                .layer(DefaultBodyLimit::max(SESSION_BODY_LIMIT)),
        )
        .route(
            "/api/libraries",
            get(list_libraries)
                .post(create_library)
                .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route("/api/library/paths", get(list_source_paths))
        .route(
            "/api/library",
            delete(delete_library).layer(DefaultBodyLimit::max(SESSION_BODY_LIMIT)),
        )
        .route(
            "/api/library/import",
            post(import_library).layer(DefaultBodyLimit::max(import::MAX_IMPORT_BYTES)),
        )
        .route(
            "/api/library/import-path",
            post(import_local_path).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route("/api/operations/{id}", get(operation_status))
        .route("/api/activity", get(recent_employees))
        .route("/api/activity/{employee}", get(employee_activity))
        .route(
            "/api/search",
            post(run_search).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .with_state(Arc::new(Web {
            catalog,
            audit,
            embedding,
            rerank,
            fusion,
            sessions: Mutex::new(Sessions::default()),
            jobs,
            blocking_slots: Arc::new(Semaphore::new(4)),
            import_slots: Arc::new(Semaphore::new(2)),
            imports: Mutex::new(HashMap::new()),
            search_slots: Arc::new(Semaphore::new(4)),
        }))
}

struct Web {
    catalog: Arc<Catalog>,
    audit: Arc<Audit>,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<Arc<Rerank>>,
    fusion: FusionMode,
    sessions: Mutex<Sessions>,
    jobs: TaskTracker,
    blocking_slots: Arc<Semaphore>,
    import_slots: Arc<Semaphore>,
    imports: Mutex<HashMap<Uuid, Arc<Mutex<ImportProgress>>>>,
    search_slots: Arc<Semaphore>,
}

impl Web {
    async fn blocking<T, F>(self: &Arc<Self>, work: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Self) -> Result<T, ApiError> + Send + 'static,
    {
        let permit = Arc::clone(&self.blocking_slots)
            .acquire_owned()
            .await
            .map_err(|_| ApiError::worker())?;
        let web = Arc::clone(self);
        self.jobs
            .spawn_blocking(move || {
                let _permit = permit;
                work(&web)
            })
            .await
            .map_err(|_| ApiError::worker())?
    }

    fn begin(
        &self,
        employee: Employee,
        action: Action,
        library: Option<&str>,
    ) -> Result<Operation, ApiError> {
        self.audit
            .begin(employee.as_str(), action, library)
            .map_err(|_| ApiError::audit())
    }

    fn finish_catalog(
        &self,
        op: &Operation,
        result: Result<Receipt, CatalogError>,
    ) -> Result<(), ApiError> {
        match result {
            Ok(_) | Err(CatalogError::OperationCommitted) => {
                // If the append fails, the durable catalog receipt is still the
                // authority for a committed mutation; do not ask clients to retry.
                if self.audit.finish(op, Outcome::Succeeded).is_ok() {
                    let _ = self.catalog.clear_receipt(op.id);
                }
                Ok(())
            }
            Err(error @ CatalogError::CommitUncertain(_)) => {
                self.audit.unresolved(op);
                Err(ApiError::from(error).for_operation(op.id))
            }
            Err(error) => Err(self.finish_failure(op, error.into())),
        }
    }

    fn finish_failure(&self, op: &Operation, error: ApiError) -> ApiError {
        if self.audit.finish(op, Outcome::Failed).is_ok() {
            error.for_operation(op.id)
        } else {
            ApiError::audit().for_operation(op.id)
        }
    }
}

#[derive(Clone, Copy)]
struct Employee([u8; 8]);

impl Employee {
    fn parse(value: &str) -> Result<Self, ApiError> {
        if !valid_employee(value) {
            return Err(ApiError::invalid(
                "employee must contain exactly eight ASCII digits",
            ));
        }
        Ok(Self(
            value
                .as_bytes()
                .try_into()
                .expect("validated employee length"),
        ))
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("validated ASCII employee")
    }

    fn response(self) -> Response {
        (
            [(header::CACHE_CONTROL, "no-store")],
            Json(EmployeeReply {
                employee: self.as_str(),
            }),
        )
            .into_response()
    }
}

#[derive(Default)]
struct Sessions {
    active: HashMap<Uuid, (Employee, Instant)>,
    expiry: VecDeque<Uuid>,
}

impl Sessions {
    fn insert(&mut self, employee: Employee) -> Uuid {
        let now = Instant::now();
        while let Some(&id) = self.expiry.front() {
            if self.active[&id].1 > now && self.active.len() < MAX_SESSIONS {
                break;
            }
            self.expiry.pop_front();
            self.active.remove(&id);
        }
        loop {
            let id = Uuid::new_v4();
            if let Entry::Vacant(entry) = self.active.entry(id) {
                entry.insert((employee, now + SESSION_TTL));
                self.expiry.push_back(id);
                return id;
            }
        }
    }

    fn get(&self, id: Uuid) -> Option<Employee> {
        let &(employee, expires) = self.active.get(&id)?;
        (Instant::now() < expires).then_some(employee)
    }
}

fn single_header(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn check_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let host = single_header(headers, header::HOST).and_then(HttpAuthority::parse);
    let referer;
    let source = if headers.contains_key(header::ORIGIN) {
        single_header(headers, header::ORIGIN)
            .and_then(|origin| origin.strip_prefix("http://"))
            .and_then(HttpAuthority::parse)
    } else {
        // Firefox ESR 68 can omit Origin on same-origin requests.
        referer = single_header(headers, header::REFERER)
            .filter(|value| !value.contains('#'))
            .and_then(|value| value.parse::<Uri>().ok());
        referer
            .as_ref()
            .filter(|uri| uri.scheme_str() == Some("http"))
            .and_then(Uri::authority)
            .and_then(|authority| HttpAuthority::parse(authority.as_str()))
    };
    if !matches!((host, source), (Some(host), Some(source)) if host.same_http_endpoint(&source)) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "a single matching HTTP Origin or, if absent, Referer header is required",
        ));
    }
    Ok(())
}

struct SameOrigin;

impl FromRequestParts<Arc<Web>> for SameOrigin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &Arc<Web>) -> Result<Self, Self::Rejection> {
        check_origin(&parts.headers)?;
        Ok(Self)
    }
}

impl FromRequestParts<Arc<Web>> for Employee {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        web: &Arc<Web>,
    ) -> Result<Self, Self::Rejection> {
        if !matches!(parts.method, Method::GET | Method::HEAD) {
            check_origin(&parts.headers)?;
        }
        let id = cookie_token(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        web.sessions
            .lock()
            .map_err(|_| ApiError::worker())?
            .get(id)
            .ok_or_else(ApiError::unauthorized)
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<Uuid> {
    let mut id = None;
    for header in headers.get_all(header::COOKIE) {
        for pair in header.to_str().ok()?.split(';') {
            let pair = pair.trim();
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key.trim() != COOKIE {
                continue;
            }
            if key != COOKIE || id.is_some() {
                return None;
            }
            let parsed = Uuid::parse_str(value).ok()?;
            if parsed.hyphenated().encode_lower(&mut Uuid::encode_buffer()) != value {
                return None;
            }
            id = Some(parsed);
        }
    }
    id
}

#[derive(Serialize)]
struct EmployeeReply<'a> {
    employee: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    employee: String,
}

async fn register(
    State(web): State<Arc<Web>>,
    _: SameOrigin,
    request: Result<Json<Registration>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = request.map_err(ApiError::json)?;
    let employee = Employee::parse(&request.employee)?;
    web.blocking(move |web| {
        web.audit
            .register(employee.as_str())
            .map_err(|_| ApiError::audit())
    })
    .await?;
    let id = web
        .sessions
        .lock()
        .map_err(|_| ApiError::worker())?
        .insert(employee);
    let mut response = employee.response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            SESSION_TTL.as_secs()
        ))
        .expect("cookie contains only static attributes and a generated identifier"),
    );
    Ok(response)
}

async fn get_session(employee: Employee) -> Response {
    employee.response()
}

#[derive(Serialize)]
struct LibraryReply {
    name: String,
    published: bool,
}

async fn list_libraries(
    State(web): State<Arc<Web>>,
    _: Employee,
) -> Result<impl IntoResponse, ApiError> {
    let libraries: Vec<_> = web
        .blocking(|web| {
            Ok(web
                .catalog
                .list()?
                .into_iter()
                .map(|library| LibraryReply {
                    name: library.name,
                    published: library.version_id.is_some(),
                })
                .collect())
        })
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(libraries)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LibraryRequest {
    name: String,
}

#[derive(Serialize)]
struct SourcePathsReply {
    paths: Vec<String>,
    complete: bool,
}

async fn list_source_paths(
    State(web): State<Arc<Web>>,
    _: Employee,
    request: Result<Query<LibraryRequest>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(LibraryRequest { name }) = request
        .map_err(|_| ApiError::invalid("a single library name query parameter is required"))?;
    let (paths, complete) = web
        .blocking(move |web| Ok(web.catalog.source_paths(&name)?))
        .await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(SourcePathsReply { paths, complete }),
    ))
}

async fn create_library(
    State(web): State<Arc<Web>>,
    employee: Employee,
    request: Result<Json<LibraryRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(LibraryRequest { name }) = request.map_err(ApiError::json)?;
    web.blocking(move |web| {
        let op = web.begin(employee, Action::Create, Some(&name))?;
        web.finish_catalog(&op, web.catalog.create(&name, Some(op.id)))?;
        Ok(operation_response(StatusCode::CREATED, op.id))
    })
    .await
}

async fn delete_library(
    State(web): State<Arc<Web>>,
    employee: Employee,
    request: Result<Query<LibraryRequest>, QueryRejection>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let Query(LibraryRequest { name }) = request
        .map_err(|_| ApiError::invalid("a single library name query parameter is required"))?;
    if !body
        .map_err(|error| ApiError::body(error.status(), "delete does not accept a request body"))?
        .is_empty()
    {
        return Err(ApiError::invalid("delete does not accept a request body"));
    }
    web.blocking(move |web| {
        let op = web.begin(employee, Action::Delete, Some(&name))?;
        web.finish_catalog(&op, web.catalog.delete(&name, Some(op.id)))?;
        Ok(operation_response(StatusCode::NO_CONTENT, op.id))
    })
    .await
}

#[derive(Serialize)]
struct AcceptedImport {
    id: Uuid,
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum ImportProgress {
    Queued,
    Indexing {
        files_done: usize,
        files_total: usize,
        passages: u64,
    },
    Publishing,
}

#[derive(Serialize)]
struct OperationReply {
    #[serde(flatten)]
    status: OperationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<ImportProgress>,
}

enum ImportSource {
    Upload(Multipart),
    LocalPath(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalImportRequest {
    path: String,
}

async fn import_library(
    State(web): State<Arc<Web>>,
    employee: Employee,
    request: Result<Query<LibraryRequest>, QueryRejection>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Response, ApiError> {
    let Query(LibraryRequest { name }) = request
        .map_err(|_| ApiError::invalid("a single library name query parameter is required"))?;
    let multipart = multipart.map_err(|error| {
        ApiError::body(
            error.status(),
            "expected a multipart upload with a valid boundary",
        )
    })?;
    submit_import(web, employee, name, ImportSource::Upload(multipart)).await
}

async fn import_local_path(
    State(web): State<Arc<Web>>,
    employee: Employee,
    request: Result<Query<LibraryRequest>, QueryRejection>,
    path: Result<Json<LocalImportRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Query(LibraryRequest { name }) = request
        .map_err(|_| ApiError::invalid("a single library name query parameter is required"))?;
    let Json(LocalImportRequest { path }) = path.map_err(ApiError::json)?;
    submit_import(web, employee, name, ImportSource::LocalPath(path)).await
}

async fn submit_import(
    web: Arc<Web>,
    employee: Employee,
    name: String,
    source: ImportSource,
) -> Result<Response, ApiError> {
    let Ok(permit) = Arc::clone(&web.import_slots).try_acquire_owned() else {
        drop(source);
        return web
            .blocking(move |web| {
                let op = web.begin(employee, Action::Import, Some(&name))?;
                Err(web.finish_failure(
                    &op,
                    ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "busy",
                        "too many imports are running",
                    ),
                ))
            })
            .await;
    };
    let (submitted, result) = oneshot::channel();
    let worker = Arc::clone(&web);
    let runtime = Handle::current();
    drop(web.jobs.spawn(async move {
        let _permit = permit;
        let (op, build, batch) = match prepare_import(&worker, employee, name, source).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = submitted.send(Err(error));
                return;
            }
        };
        let id = op.id;
        let progress = Arc::new(Mutex::new(ImportProgress::Queued));
        worker
            .imports
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, Arc::clone(&progress));
        let _ = submitted.send(Ok(op.id));
        // The tracked task owns all state after staging and completes even if
        // the browser disconnects. Shutdown waits for it before dropping Edge.
        let result = worker
            .blocking(move |web| {
                let files_total = batch.documents().len();
                let report_progress = |files_done, passages| {
                    *progress.lock().unwrap_or_else(|error| error.into_inner()) =
                        ImportProgress::Indexing {
                            files_done,
                            files_total,
                            passages,
                        };
                };
                report_progress(0, 0);
                let result = index::build(
                    &build.index_path(),
                    build.version_id(),
                    &batch,
                    web.embedding.as_deref(),
                    &runtime,
                    report_progress,
                );
                match result {
                    Ok(Some(error)) => {
                        // ModelError contains no URL, credentials, or input body.
                        let _ =
                            writeln!(io::stderr().lock(), "import embedding unavailable: {error}");
                    }
                    Ok(None) => {}
                    Err(_) => {
                        return Err(
                            web.finish_failure(&op, ApiError::storage("could not build the index"))
                        );
                    }
                }
                drop(batch);
                *progress.lock().unwrap_or_else(|error| error.into_inner()) =
                    ImportProgress::Publishing;
                web.finish_catalog(&op, web.catalog.publish(build, Some(op.id)))
            })
            .await;
        worker
            .imports
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
        if let Err(error) = result {
            let _ = writeln!(io::stderr().lock(), "import {id}: {}", error.code);
        }
    }));
    let id = result.await.map_err(|_| ApiError::worker())??;
    Ok((
        StatusCode::ACCEPTED,
        operation_headers(id),
        Json(AcceptedImport { id }),
    )
        .into_response())
}

async fn prepare_import(
    web: &Arc<Web>,
    employee: Employee,
    name: String,
    source: ImportSource,
) -> Result<(Operation, Build, Batch), ApiError> {
    let (op, build) = web
        .blocking(move |web| {
            let op = web.begin(employee, Action::Import, Some(&name))?;
            let build = web
                .catalog
                .start_build(&name)
                .map_err(|error| web.finish_failure(&op, error.into()))?;
            Ok((op, build))
        })
        .await?;
    let batch = match source {
        ImportSource::Upload(multipart) => import::stage(multipart, web.catalog.stage_root())
            .await
            .map_err(ApiError::from),
        ImportSource::LocalPath(path) => {
            web.blocking(move |web| {
                import::stage_local(&path, web.catalog.stage_root()).map_err(ApiError::from)
            })
            .await
        }
    };
    match batch {
        Ok(batch) => Ok((op, build, batch)),
        Err(error) => {
            web.blocking(move |web| {
                let failure = web.finish_failure(&op, error);
                drop(build);
                Err(failure)
            })
            .await
        }
    }
}

async fn operation_status(
    State(web): State<Arc<Web>>,
    _: Employee,
    id: Result<Path<Uuid>, PathRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Path(id) = id.map_err(|_| ApiError::invalid("operation id must be a UUID"))?;
    let status = web
        .blocking(move |web| {
            web.audit
                .status(id, &web.catalog)
                .map_err(|_| ApiError::audit())?
                .ok_or_else(|| ApiError::not_found("operation does not exist"))
        })
        .await?;
    let progress = if status.outcome == Outcome::InProgress {
        let progress = web
            .imports
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&id)
            .cloned();
        progress.map(|progress| *progress.lock().unwrap_or_else(|error| error.into_inner()))
    } else {
        None
    };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(OperationReply { status, progress }),
    ))
}

async fn recent_employees(State(web): State<Arc<Web>>) -> Result<impl IntoResponse, ApiError> {
    let employees = web
        .blocking(|web| web.audit.recent_employees().map_err(|_| ApiError::audit()))
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(employees)))
}

async fn employee_activity(
    State(web): State<Arc<Web>>,
    employee: Result<Path<String>, PathRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Path(employee) = employee.map_err(|_| ApiError::invalid("invalid employee number"))?;
    let employee = Employee::parse(&employee)?;
    let activity = web
        .blocking(move |web| {
            web.audit
                .employee_activity(employee.as_str(), &web.catalog)
                .map_err(|_| ApiError::audit())
        })
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(activity)))
}

async fn run_search(
    State(web): State<Arc<Web>>,
    employee: Employee,
    request: Result<Json<SearchRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) = request.map_err(ApiError::json)?;
    let permit = Arc::clone(&web.search_slots)
        .acquire_owned()
        .await
        .map_err(|_| ApiError::worker())?;
    let worker = Arc::clone(&web);
    web.jobs
        .spawn(async move {
            let _permit = permit;
            let (op, request) = worker
                .blocking(move |web| {
                    let op = web.begin(employee, Action::Search, request.library.as_deref())?;
                    Ok((op, request))
                })
                .await?;
            let result = search::search(
                &worker.catalog,
                request,
                worker.embedding.clone(),
                worker.rerank.as_deref(),
                worker.fusion,
            )
            .await;
            let id = op.id;
            // Persist the terminal outcome before returning any evidence.
            let reply = worker
                .blocking(move |web| match result {
                    Ok(reply) => {
                        web.audit
                            .finish(&op, Outcome::Succeeded)
                            .map_err(|_| ApiError::audit().for_operation(id))?;
                        Ok(reply)
                    }
                    Err(error) => Err(web.finish_failure(&op, error.into())),
                })
                .await?;
            Ok((StatusCode::OK, operation_headers(id), Json(reply)).into_response())
        })
        .await
        .map_err(|_| ApiError::worker())?
}

fn operation_response(status: StatusCode, id: Uuid) -> Response {
    (status, operation_headers(id)).into_response()
}

fn operation_headers(id: Uuid) -> [(&'static str, HeaderValue); 2] {
    let header_id = HeaderValue::from_str(id.hyphenated().encode_lower(&mut Uuid::encode_buffer()))
        .expect("UUID is a valid header value");
    [
        ("x-operation-id", header_id),
        ("cache-control", HeaderValue::from_static("no-store")),
    ]
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: Cow<'static, str>,
    operation: Option<Uuid>,
}

#[derive(Serialize)]
struct ErrorReply<'a> {
    error: &'static str,
    message: &'a str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            operation: None,
        }
    }

    fn for_operation(mut self, id: Uuid) -> Self {
        self.operation = Some(id);
        self
    }

    fn invalid(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_input", message)
    }

    fn not_found(message: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "register an employee number first",
        )
    }

    fn storage(message: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "storage", message)
    }

    fn audit() -> Self {
        Self::storage("operation audit storage is unavailable")
    }

    fn worker() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage",
            "operation worker is unavailable",
        )
    }

    fn body(status: StatusCode, message: &'static str) -> Self {
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            Self::new(
                status,
                "invalid_input",
                "request body exceeds the allowed size",
            )
        } else {
            Self::invalid(message)
        }
    }

    fn json(error: JsonRejection) -> Self {
        Self::body(
            error.status(),
            "expected a valid JSON object with only the supported fields",
        )
    }
}

impl From<CatalogError> for ApiError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::InvalidName => Self::invalid(INVALID_NAME),
            CatalogError::AlreadyExists => {
                Self::new(StatusCode::CONFLICT, "conflict", "library already exists")
            }
            CatalogError::NotFound => Self::not_found("library does not exist"),
            CatalogError::StaleBuild => Self::new(
                StatusCode::CONFLICT,
                "conflict",
                "library changed while building the import",
            ),
            CatalogError::CommitUncertain(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unknown",
                "catalog commit outcome is uncertain; check the operation status before retrying",
            ),
            CatalogError::Storage(_)
            | CatalogError::Unavailable
            | CatalogError::OperationCommitted => Self::storage("catalog storage is unavailable"),
        }
    }
}

impl From<ImportError> for ApiError {
    fn from(error: ImportError) -> Self {
        match error {
            ImportError::InvalidInput { .. } => Self::invalid(error.to_string()),
            ImportError::PayloadTooLarge => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_input",
                error.to_string(),
            ),
            ImportError::Storage { .. } => Self::storage("import staging storage is unavailable"),
        }
    }
}

impl From<SearchError> for ApiError {
    fn from(error: SearchError) -> Self {
        match error {
            SearchError::InvalidInput(message) => Self::invalid(message),
            SearchError::LibraryNotFound => Self::not_found("library does not exist"),
            SearchError::Store => Self::storage("published index or catalog is unavailable"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorReply {
            error: self.code,
            message: &self.message,
        });
        if let Some(id) = self.operation {
            (self.status, operation_headers(id), body).into_response()
        } else {
            (self.status, [(header::CACHE_CONTROL, "no-store")], body).into_response()
        }
    }
}

#[cfg(test)]
mod tests;
