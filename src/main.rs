mod audit;
mod catalog;
mod collab;
#[cfg(target_os = "linux")]
mod daemon;
mod data_root;
mod durable;
mod edge_probe;
mod import;
mod index;
mod markdown;
mod mcp;
#[cfg(target_os = "linux")]
mod model_defaults;
mod model_http;
mod search;
mod settings;
mod shard;
#[cfg(test)]
mod surface_tests;
mod technical;
mod web;

use std::{
    env,
    error::Error,
    future::Future,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    process::ExitCode,
    sync::Arc,
};

use axum::{
    Router,
    extract::{ConnectInfo, Request, connect_info::Connected},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    serve::IncomingStream,
};
use tokio::net::TcpListener;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use settings::{Command, HttpAuthority, Settings};

#[derive(Clone, Copy)]
enum RunMode {
    Foreground,
    #[cfg(target_os = "linux")]
    Daemon,
}

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
    match settings::parse(env::args_os().skip(1)) {
        Ok(Command::Help) => {
            print!("{}", settings::HELP);
            #[cfg(target_os = "linux")]
            print!("{}", settings::DAEMON_HELP);
            Ok(())
        }
        Ok(Command::Probe { mode, directory }) => edge_probe::run(mode, &directory),
        Ok(Command::Serve(settings)) => serve_http(settings, RunMode::Foreground),
        #[cfg(target_os = "linux")]
        Ok(Command::Start) => Ok(daemon::start()?),
        #[cfg(target_os = "linux")]
        Ok(Command::Status) => Ok(daemon::status()?),
        #[cfg(target_os = "linux")]
        Ok(Command::Stop) => Ok(daemon::stop()?),
        #[cfg(target_os = "linux")]
        Ok(Command::Daemon(settings)) => {
            let result = serve_http(settings, RunMode::Daemon);
            if let Err(error) = &result {
                daemon::report_start_error(error.as_ref());
            }
            result
        }
        Err(error) => Err(format!("{error}\nRun rust-knowledge-server --help for usage.").into()),
    }
}

fn serve_http(settings: Settings, mode: RunMode) -> Result<(), Box<dyn Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .max_blocking_threads(8)
        .enable_all()
        .build()?
        .block_on(serve(settings, mode))
}

async fn serve(settings: Settings, mode: RunMode) -> Result<(), Box<dyn Error>> {
    let Settings {
        listen: address,
        mcp_origins,
        embedding,
        rerank,
        fusion,
    } = settings;
    let embedding = embedding
        .map(|value| model_http::Embedding::new(value.api, value.dimensions).map(Arc::new))
        .transpose()?;
    let rerank = rerank
        .map(|value| model_http::Rerank::new(value).map(Arc::new))
        .transpose()?;
    let root = data_root::Root::open_default()?;
    #[cfg(target_os = "linux")]
    if matches!(mode, RunMode::Daemon) {
        daemon::redirect_log(root.path())?;
    }
    #[cfg(target_os = "linux")]
    let (embedding, rerank) = model_defaults::resolve(embedding, rerank);
    let catalog = Arc::new(catalog::Catalog::open(root)?);
    let audit = Arc::new(audit::Audit::open(catalog.root_path(), &catalog)?);
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
    let bound = listener.local_addr()?;
    let jobs = TaskTracker::new();
    let cancellation = CancellationToken::new();
    let app = Router::new()
        .merge(collab::router(jobs.clone(), cancellation.clone()))
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .merge(web::router(
            Arc::clone(&catalog),
            audit,
            embedding.clone(),
            rerank.clone(),
            fusion,
            jobs.clone(),
        ))
        .merge(mcp::router(
            Arc::clone(&catalog),
            embedding,
            rerank,
            fusion,
            mcp_origins,
            cancellation.clone(),
        ));
    #[cfg(target_os = "linux")]
    let mut control = None;
    match mode {
        RunMode::Foreground => {
            println!("Data root: {}", catalog.root_path().display());
            println!("Listening on http://{bound}");
            let web_address = match bound {
                SocketAddr::V4(address) if address.ip().is_unspecified() => {
                    SocketAddr::from((Ipv4Addr::LOCALHOST, address.port()))
                }
                SocketAddr::V6(address) if address.ip().is_unspecified() => {
                    SocketAddr::from((Ipv6Addr::LOCALHOST, address.port()))
                }
                _ => bound,
            };
            println!("Web: http://{web_address}/");
            println!("Fusion mode: {}", fusion.as_str());
        }
        #[cfg(target_os = "linux")]
        RunMode::Daemon => {
            let socket = daemon::Control::bind(catalog.root_path())?;
            eprintln!("Listening on http://{bound}");
            daemon::report_ready(bound)?;
            control = Some(tokio::spawn(socket.wait(bound, cancellation.clone())));
        }
    }
    let shutdown_token = cancellation.clone();
    let stopped = axum::serve(
        listener,
        http_access(app).into_make_service_with_connect_info::<Connection>(),
    )
    .with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown => {}
            _ = shutdown_token.cancelled() => {}
        }
        shutdown_token.cancel();
    })
    .await;
    cancellation.cancel();
    jobs.close();
    jobs.wait().await;
    #[cfg(target_os = "linux")]
    let reply = if let Some(control) = control {
        let (socket, reply) = control.await??;
        drop(socket);
        reply
    } else {
        None
    };
    drop(catalog);
    stopped?;
    #[cfg(target_os = "linux")]
    if let Some(reply) = reply {
        daemon::confirm_stop(reply).await;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Connection {
    local: Option<SocketAddr>,
    peer_is_loopback: bool,
}

impl Connected<IncomingStream<'_, TcpListener>> for Connection {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        Self {
            local: stream.io().local_addr().ok(),
            peer_is_loopback: stream.remote_addr().ip().to_canonical().is_loopback(),
        }
    }
}

fn http_access(app: Router) -> Router {
    app.layer(middleware::from_fn(require_http_access))
}

async fn require_http_access(
    ConnectInfo(connection): ConnectInfo<Connection>,
    request: Request,
    next: Next,
) -> Response {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next().and_then(|value| value.to_str().ok());
    if hosts.next().is_some() {
        return (StatusCode::BAD_REQUEST, "Duplicate Host header.").into_response();
    }
    if !host.and_then(HttpAuthority::parse).is_some_and(|host| {
        connection
            .local
            .is_some_and(|local| host.matches_socket(local, connection.peer_is_loopback))
            && request
                .uri()
                .scheme_str()
                .is_none_or(|scheme| scheme == "http")
            && request.uri().authority().is_none_or(|target| {
                HttpAuthority::parse(target.as_str())
                    .is_some_and(|target| host.same_http_endpoint(&target))
            })
    }) {
        return (
            StatusCode::FORBIDDEN,
            "Host does not match the connection endpoint.",
        )
            .into_response();
    }
    next.run(request).await
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
