mod audit;
mod catalog;
mod data_root;
mod durable;
mod edge_probe;
mod import;
mod index;
mod markdown;
mod mcp;
mod model_http;
mod search;
mod settings;
mod shard;
#[cfg(test)]
mod surface_tests;
mod technical;
mod web;

use std::{
    env, error::Error, ffi::OsStr, future::Future, io, net::SocketAddr, path::Path,
    process::ExitCode, sync::Arc,
};

use axum::{Router, http::header, response::Html, routing::get};
use tokio::net::TcpListener;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const USAGE: &str = "Usage: rust-knowledge-server [--listen <SocketAddr>]\n       rust-knowledge-server --edge-probe <write|read> <directory>";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref() {
        None => serve_http(None),
        Some(flag) if flag == "--listen" => {
            let address = args
                .next()
                .as_deref()
                .and_then(OsStr::to_str)
                .and_then(|address| address.parse::<SocketAddr>().ok())
                .ok_or("--listen requires an IP address and port, e.g. 127.0.0.1:7336")
                .map_err(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error("unexpected argument after --listen <address>").into());
            }
            serve_http(Some(address))
        }
        Some(flag) if flag == "--edge-probe" => {
            let mode = args.next();
            let mode = match mode.as_deref().and_then(OsStr::to_str) {
                Some(mode @ ("write" | "read")) => mode,
                _ => return Err(usage_error("--edge-probe requires mode write or read").into()),
            };
            let directory = args
                .next()
                .filter(|directory| !directory.is_empty())
                .ok_or("--edge-probe requires a directory path")
                .map_err(usage_error)?;
            if args.next().is_some() {
                return Err(usage_error(
                    "unexpected argument after --edge-probe <mode> <directory>",
                )
                .into());
            }
            edge_probe::run(mode, Path::new(&directory))
        }
        Some(flag) => Err(usage_error(&format!("unknown argument {flag:?}")).into()),
    }
}

fn usage_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{message}\n{USAGE}"))
}

fn serve_http(override_address: Option<SocketAddr>) -> Result<(), Box<dyn Error>> {
    let root = data_root::Root::open_from_env()?;
    let settings = settings::read(root.path())?;
    let address = override_address.unwrap_or(settings.listen);
    settings.validate_listen(address)?;
    let catalog = Arc::new(catalog::Catalog::open(root)?);
    let audit = Arc::new(audit::Audit::open(catalog.root_path(), &catalog)?);
    tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(8)
        .enable_all()
        .build()?
        .block_on(serve(address, settings, catalog, audit))
}

async fn serve(
    address: SocketAddr,
    settings: settings::Settings,
    catalog: Arc<catalog::Catalog>,
    audit: Arc<audit::Audit>,
) -> Result<(), Box<dyn Error>> {
    let embedding = settings
        .embedding
        .map(|value| model_http::Embedding::new(value.api, value.dimensions).map(Arc::new))
        .transpose()?;
    let rerank = settings
        .rerank
        .map(|value| model_http::Rerank::new(value).map(Arc::new))
        .transpose()?;
    let shutdown = shutdown_signal().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not register shutdown signals: {error}"),
        )
    })?;
    let listener = TcpListener::bind(address).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not bind HTTP listener to {address}: {error}"),
        )
    })?;
    let local_url = format!("http://{}", listener.local_addr()?);
    let origin = match settings.web_origin {
        Some(origin) => origin,
        None => settings::canonical_origin(&local_url, "web_origin")?,
    };
    let jobs = TaskTracker::new();
    let cancellation = CancellationToken::new();
    let app = Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .merge(web::router(
            Arc::clone(&catalog),
            audit,
            embedding.clone(),
            rerank.clone(),
            origin,
            jobs.clone(),
        ))
        .merge(mcp::router(
            Arc::clone(&catalog),
            embedding,
            rerank,
            settings.mcp_hosts,
            settings.mcp_origins,
            cancellation.clone(),
        ));
    println!("Data root: {}", catalog.root_path().display());
    println!("Listening on {local_url}");
    let stopped = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            cancellation.cancel();
        })
        .await;
    jobs.close();
    jobs.wait().await;
    stopped?;
    Ok(())
}

#[cfg(windows)]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    let mut interrupt = ctrl_c()?;
    let mut ctrl_break = ctrl_break()?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = ctrl_break.recv() => {},
        }
    })
}

#[cfg(target_os = "linux")]
fn shutdown_signal() -> io::Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn stylesheet() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/app.css"),
    )
}

async fn script() -> ([(header::HeaderName, &'static str); 1], &'static str) {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}
