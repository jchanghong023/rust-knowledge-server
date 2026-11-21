use std::{
    borrow::Cow,
    env,
    ffi::OsStr,
    fmt::Display,
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use serde::Deserialize;

use crate::model_http::{ApiConfig, Embedding, ModelError, Rerank};

#[derive(Deserialize)]
struct ClaudeSettings<'a> {
    #[serde(borrow)]
    env: Gateway<'a>,
}

#[derive(Deserialize)]
struct Gateway<'a> {
    #[serde(borrow, rename = "ANTHROPIC_BASE_URL")]
    base_url: Cow<'a, str>,
    #[serde(borrow, rename = "ANTHROPIC_AUTH_TOKEN")]
    token: Cow<'a, str>,
}

pub(crate) fn resolve(
    mut embedding: Option<Arc<Embedding>>,
    mut rerank: Option<Arc<Rerank>>,
) -> (Option<Arc<Embedding>>, Option<Arc<Rerank>>) {
    if (embedding.is_none() || rerank.is_none())
        && env::var_os("OMP_OFFLINE").as_deref() == Some(OsStr::new("1"))
        && let Err(reason) = fill_defaults(&mut embedding, &mut rerank)
    {
        warn(reason);
    }
    (embedding, rerank)
}

fn fill_defaults(
    embedding: &mut Option<Arc<Embedding>>,
    rerank: &mut Option<Arc<Rerank>>,
) -> Result<(), &'static str> {
    let dir = env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".claude")))
        .ok_or("cannot locate Claude Code settings.json")?;
    let bytes = fs::read(dir.join("settings.json"))
        .map_err(|_| "cannot read Claude Code settings.json")?;
    let ClaudeSettings { env: gateway } = serde_json::from_slice(&bytes)
        .map_err(|_| "invalid Claude Code settings.json or missing gateway URL/Token")?;
    if gateway.token.contains(['\r', '\n']) {
        return Err("invalid gateway Token");
    }
    let api = |model: &str| ApiConfig {
        base_url: gateway.base_url.trim().to_owned(),
        model: model.to_owned(),
        api_key: Some(gateway.token.trim().to_owned()),
    };
    if embedding.is_none() {
        *embedding = optional_client("Embedding", Embedding::new(api("Qwen3-Embedding-8B"), 4096));
    }
    if rerank.is_none() {
        *rerank = optional_client("Rerank", Rerank::new(api("Qwen3-Reranker-8B")));
    }
    Ok(())
}

fn optional_client<T>(name: &str, client: Result<T, ModelError>) -> Option<Arc<T>> {
    match client {
        Ok(client) => Some(Arc::new(client)),
        Err(error) => {
            warn(format_args!("{name}: {error}"));
            None
        }
    }
}

fn warn(reason: impl Display) {
    let _ = writeln!(
        io::stderr().lock(),
        "model defaults unavailable: {reason}"
    );
}
