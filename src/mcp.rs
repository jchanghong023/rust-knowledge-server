use std::sync::Arc;

use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    catalog::Catalog,
    model_http::{Embedding, Rerank},
    search::{self, FusionMode, SearchError, SearchRequest},
};

mod excerpt;
mod fetch;

use excerpt::{Excerpt, SearchOutput};
use fetch::{FetchError, SourceRef};

pub(crate) fn router(
    catalog: Arc<Catalog>,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<Arc<Rerank>>,
    fusion: FusionMode,
    mut allowed_origins: Vec<String>,
    cancellation_token: CancellationToken,
) -> Router {
    // rmcp 3.11 treats a portless allowlist entry as any port, unlike a browser origin.
    for origin in &mut allowed_origins {
        let Some((scheme, authority)) = origin.split_once("://") else {
            continue;
        };
        if port_suffix(authority).is_none() {
            match scheme {
                "http" => origin.push_str(":80"),
                "https" => origin.push_str(":443"),
                _ => {}
            }
        }
    }
    let config = StreamableHttpServerConfig::default()
        // The shared HTTP layer checks Host against the accepted TCP socket.
        .disable_allowed_hosts()
        .with_allowed_origins(allowed_origins)
        .enforce_origin_validation()
        .with_cancellation_token(cancellation_token);
    let server = KnowledgeServer {
        catalog,
        embedding,
        rerank,
        fusion,
    };
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn(reject_ambiguous_origin))
}

fn port_suffix(authority: &str) -> Option<&str> {
    let host_end = authority.rfind(']').map_or(0, |end| end + 1);
    authority[host_end..].split_once(':').map(|(_, port)| port)
}

async fn reject_ambiguous_origin(request: Request, next: Next) -> Response {
    // rmcp checks only the first Origin field.
    let headers = request.headers();
    let mut origins = headers.get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next()
        && (origins.next().is_some() || !origin.to_str().is_ok_and(is_serialized_origin))
    {
        return (StatusCode::FORBIDDEN, "Invalid or duplicate Origin header").into_response();
    }
    next.run(request).await
}

fn is_serialized_origin(origin: &str) -> bool {
    let origin = origin.trim();
    if origin == "null" {
        return true;
    }
    // The SDK extracts the URI's authority, ignoring e.g. a path or userinfo.
    // Pass only an origin serialization, and let the SDK match the tuple.
    origin.split_once("://").is_some_and(|(_, authority)| {
        !authority.is_empty()
            && !authority.contains(['/', '?', '#', '@', ','])
            && port_suffix(authority).is_none_or(|port| port.parse::<u16>().is_ok())
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    /// Non-blank keywords or natural language, treated as plain text rather than query syntax.
    query: String,
    /// Optional exact name of a knowledge library. Omit to search all published libraries.
    library: Option<String>,
    /// Optional slash-separated relative file or directory path, applied within each selected library.
    path: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    /// Opaque reference from a knowledge_search match. Copy it verbatim.
    #[serde(rename = "ref")]
    source_ref: String,
}

#[derive(Clone)]
struct KnowledgeServer {
    catalog: Arc<Catalog>,
    embedding: Option<Arc<Embedding>>,
    rerank: Option<Arc<Rerank>>,
    fusion: FusionMode,
}

#[tool_router]
impl KnowledgeServer {
    #[tool(
        description = "Search published enterprise knowledge and code wikis. Returns original excerpts and source locations. Pass a hit's ref to knowledge_fetch for more context. Related hits are not verified facts.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<SearchOutput<'static>>(),
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn knowledge_search(
        &self,
        Parameters(SearchArgs {
            query,
            library,
            path,
        }): Parameters<SearchArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let reply = tokio::select! {
            biased;
            _ = context.ct.cancelled() => {
                return Ok(CallToolResult::error(vec![ContentBlock::text("Search cancelled.")]));
            }
            reply = search::search(
                &self.catalog,
                SearchRequest { query, library, path },
                self.embedding.clone(),
                self.rerank.as_deref(),
                self.fusion,
            ) => reply,
        };
        match reply
            .and_then(|reply| SearchOutput::from_reply(&reply).map(|output| structured(output)))
        {
            Ok(result) => result,
            Err(SearchError::Store) => Ok(storage_error()),
            Err(SearchError::InvalidInput(message)) => Err(invalid_input(message)),
            Err(SearchError::LibraryNotFound) => Err(invalid_input("library does not exist")),
        }
    }

    #[tool(
        description = "Read the original lines surrounding a search match. Pass the match's ref verbatim.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<Excerpt<'static>>(),
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn knowledge_fetch(
        &self,
        Parameters(FetchArgs { source_ref }): Parameters<FetchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let locator = SourceRef::parse(&source_ref)
            .ok_or_else(|| invalid_input("invalid search reference"))?;
        let result = match locator.pin(&self.catalog) {
            Ok(version) => tokio::task::spawn_blocking(move || fetch::read(version, locator))
                .await
                .unwrap_or(Err(FetchError::Store)),
            Err(error) => Err(error),
        };
        match result {
            Ok(context) => structured(context.excerpt()),
            Err(FetchError::InvalidRef) => Err(invalid_input("invalid search reference")),
            Err(FetchError::StaleRef) => Ok(CallToolResult::structured_error(json!({
                "error": "stale_ref",
                "message": "The referenced version is no longer published; search again"
            }))),
            Err(FetchError::Store) => Ok(storage_error()),
        }
    }
}

#[tool_handler]
impl ServerHandler for KnowledgeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }
}

fn invalid_input(message: &'static str) -> ErrorData {
    ErrorData::invalid_params(message, Some(json!({"error": "invalid_input"})))
}

fn structured(value: impl Serialize) -> Result<CallToolResult, ErrorData> {
    let value = serde_json::to_value(value)
        .map_err(|_| ErrorData::internal_error("Tool result could not be encoded", None))?;
    Ok(CallToolResult::structured(value))
}

fn storage_error() -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": "storage",
        "message": "Published index or catalog is unavailable"
    }))
}

#[cfg(test)]
mod tests;
