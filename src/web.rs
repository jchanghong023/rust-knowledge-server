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
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
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
};

const COOKIE: &str = "knowledge_session";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_SESSIONS: usize = 4096;
const SESSION_BODY_LIMIT: usize = 1024;
const JSON_BODY_LIMIT: usize = 32 * 1024;
// Cap the entire multipart body (metadata, boundaries, and files) at 1 GiB.
const IMPORT_BODY_LIMIT: usize = 1024 * 1024 * 1024;

pub(crate) fn router(
    catalog: Arc<Catalog>,
    audit: Arc<Audit>,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<Arc<Rerank>>,
    fusion: FusionMode,
    origin: String,
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
        .route(
            "/api/library",
            delete(delete_library).layer(DefaultBodyLimit::max(SESSION_BODY_LIMIT)),
        )
        .route(
            "/api/library/import",
            post(import_library).layer(DefaultBodyLimit::max(IMPORT_BODY_LIMIT)),
        )
        .route("/api/operations/{id}", get(operation_status))
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
            secure_cookie: origin.starts_with("https://"),
            origin,
            sessions: Mutex::new(Sessions::default()),
            jobs,
            blocking_slots: Arc::new(Semaphore::new(4)),
            import_slots: Arc::new(Semaphore::new(2)),
            search_slots: Arc::new(Semaphore::new(4)),
        }))
}

struct Web {
    catalog: Arc<Catalog>,
    audit: Arc<Audit>,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<Arc<Rerank>>,
    fusion: FusionMode,
    origin: String,
    secure_cookie: bool,
    sessions: Mutex<Sessions>,
    jobs: TaskTracker,
    blocking_slots: Arc<Semaphore>,
    import_slots: Arc<Semaphore>,
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

    fn same_origin(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let matching = origins
            .next()
            .is_some_and(|origin| origin.as_bytes() == self.origin.as_bytes());
        if matching && origins.next().is_none() {
            Ok(())
        } else {
            Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "a single matching Origin header is required",
            ))
        }
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

struct SameOrigin;

impl FromRequestParts<Arc<Web>> for SameOrigin {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        web: &Arc<Web>,
    ) -> Result<Self, Self::Rejection> {
        web.same_origin(&parts.headers)?;
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
            web.same_origin(&parts.headers)?;
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
    let secure = if web.secure_cookie { "; Secure" } else { "" };
    let mut response = employee.response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}={id}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
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
    let Ok(permit) = Arc::clone(&web.import_slots).try_acquire_owned() else {
        drop(multipart);
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
        let (op, build, batch) = match prepare_import(&worker, employee, name, multipart).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = submitted.send(Err(error));
                return;
            }
        };
        let _ = submitted.send(Ok(op.id));
        // The tracked task owns all state after staging and completes even if
        // the browser disconnects. Shutdown waits for it before dropping Edge.
        let id = op.id;
        if let Err(error) = worker
            .blocking(move |web| {
                let result = index::build(
                    &build.index_path(),
                    build.version_id(),
                    &batch,
                    web.embedding.as_deref(),
                    &runtime,
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
                web.finish_catalog(&op, web.catalog.publish(build, Some(op.id)))
            })
            .await
        {
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
    multipart: Multipart,
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
    match import::stage(multipart, web.catalog.stage_root()).await {
        Ok(batch) => Ok((op, build, batch)),
        Err(error) => {
            web.blocking(move |web| {
                let failure = web.finish_failure(&op, error.into());
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
    let status: OperationStatus = web
        .blocking(move |web| {
            web.audit
                .status(id, &web.catalog)
                .map_err(|_| ApiError::audit())?
                .ok_or_else(|| ApiError::not_found("operation does not exist"))
        })
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(status)))
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
            ImportError::Storage { .. } => Self::storage("upload staging storage is unavailable"),
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
