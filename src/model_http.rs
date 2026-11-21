use std::{fmt, time::Duration};

use reqwest::{Client, StatusCode, Url, header::HeaderValue};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};

#[derive(Deserialize)]
pub(crate) struct ApiConfig {
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) api_key: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ModelError {
    InvalidEndpoint,
    InvalidModel,
    InvalidKey,
    InvalidDimensions,
    Setup,
    Request,
    Timeout,
    Status(StatusCode),
    InvalidReply,
    ModelMismatch,
    InvalidResultCount,
    InvalidIndex,
    InvalidVector,
    InvalidScore,
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEndpoint => "invalid model API base URL",
            Self::InvalidModel => "missing model API model name",
            Self::InvalidKey => "invalid model API key",
            Self::InvalidDimensions => "embedding dimensions must be positive",
            Self::Setup => "model HTTP client setup failed",
            Self::Request => "model API request failed",
            Self::Timeout => "model API request timed out",
            Self::Status(status) => {
                return write!(f, "model API returned HTTP {}", status.as_u16());
            }
            Self::InvalidReply => "invalid model API reply",
            Self::ModelMismatch => "model API reply model does not match the requested model",
            Self::InvalidResultCount => "model API reply has an unexpected number of results",
            Self::InvalidIndex => "model API reply has a duplicate or out-of-range result index",
            Self::InvalidVector => "model API reply has an invalid embedding vector",
            Self::InvalidScore => "model API reply has a non-finite rerank score",
        })
    }
}

impl std::error::Error for ModelError {}

struct ModelApi {
    client: Client,
    endpoint: Url,
    model: String,
    authorization: Option<HeaderValue>,
}

impl ModelApi {
    fn new(config: ApiConfig, route: &str) -> Result<Self, ModelError> {
        let endpoint = model_endpoint(&config.base_url, route)?;
        if config.model.trim().is_empty() {
            return Err(ModelError::InvalidModel);
        }
        let authorization = config
            .api_key
            .map(|key| {
                if key.trim().is_empty() {
                    return Err(ModelError::InvalidKey);
                }
                let mut header = HeaderValue::from_str(&format!("Bearer {key}"))
                    .map_err(|_| ModelError::InvalidKey)?;
                header.set_sensitive(true);
                Ok(header)
            })
            .transpose()?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| ModelError::Setup)?;
        Ok(Self {
            client,
            endpoint,
            model: config.model,
            authorization,
        })
    }

    async fn post<T: Serialize, R: DeserializeOwned>(&self, body: T) -> Result<R, ModelError> {
        let mut request = self.client.post(self.endpoint.clone()).json(&body);
        if let Some(authorization) = &self.authorization {
            request = request.header(reqwest::header::AUTHORIZATION, authorization.clone());
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                ModelError::Timeout
            } else {
                ModelError::Request
            }
        })?;
        if !response.status().is_success() {
            return Err(ModelError::Status(response.status()));
        }
        response.json().await.map_err(|error| {
            if error.is_timeout() {
                ModelError::Timeout
            } else {
                ModelError::InvalidReply
            }
        })
    }

    fn validate_reply(
        &self,
        model: Option<&str>,
        expected: usize,
        actual: usize,
    ) -> Result<(), ModelError> {
        if model.is_some_and(|model| model != self.model) {
            return Err(ModelError::ModelMismatch);
        }
        if expected != actual {
            return Err(ModelError::InvalidResultCount);
        }
        Ok(())
    }
}

fn model_endpoint(base_url: &str, route: &str) -> Result<Url, ModelError> {
    let (_, address) = base_url
        .split_once("://")
        .ok_or(ModelError::InvalidEndpoint)?;
    let authority = address
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or_default();
    // URL normalization may discard empty userinfo before giving us a parsed URL.
    if authority.is_empty() || authority.contains('@') || address.contains('\\') {
        return Err(ModelError::InvalidEndpoint);
    }
    let mut url = Url::parse(base_url).map_err(|_| ModelError::InvalidEndpoint)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ModelError::InvalidEndpoint);
    }
    let path = url.path().trim_end_matches('/');
    let version = if path.ends_with("/v1") { "" } else { "/v1" };
    url.set_path(&format!("{path}{version}/{route}"));
    Ok(url)
}

// Absence is compatible; an explicitly returned model must be a matching string.
fn present_model<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

pub(crate) struct Embedding {
    api: ModelApi,
    dimensions: usize,
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbeddingReply {
    #[serde(default, deserialize_with = "present_model")]
    model: Option<String>,
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    #[serde(deserialize_with = "embedding_vector")]
    embedding: Vec<f32>,
}

fn embedding_vector<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<f32>, D::Error> {
    use serde::de::{Error, SeqAccess, Visitor};

    struct VectorVisitor;
    impl<'de> Visitor<'de> for VectorVisitor {
        type Value = Vec<f32>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an array of finite float32 values")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(wire) = sequence.next_element::<f64>()? {
                let value = wire as f32;
                if !value.is_finite() || (value == 0.0 && wire != 0.0) {
                    return Err(A::Error::custom("invalid float32 embedding value"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(VectorVisitor)
}

impl Embedding {
    pub(crate) fn new(config: ApiConfig, dimensions: usize) -> Result<Self, ModelError> {
        if dimensions == 0 {
            return Err(ModelError::InvalidDimensions);
        }
        Ok(Self {
            api: ModelApi::new(config, "embeddings")?,
            dimensions,
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.api.model
    }

    pub(crate) fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub(crate) async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, ModelError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let reply: EmbeddingReply = self
            .api
            .post(EmbeddingRequest {
                model: self.model(),
                input: inputs,
            })
            .await?;
        self.api
            .validate_reply(reply.model.as_deref(), inputs.len(), reply.data.len())?;
        let mut vectors = vec![Vec::new(); inputs.len()];
        for item in reply.data {
            let slot = vectors
                .get_mut(item.index)
                .ok_or(ModelError::InvalidIndex)?;
            if !slot.is_empty() {
                return Err(ModelError::InvalidIndex);
            }
            if item.embedding.len() != self.dimensions || !item.embedding.iter().any(|&v| v != 0.0)
            {
                return Err(ModelError::InvalidVector);
            }
            *slot = item.embedding;
        }
        Ok(vectors)
    }
}

pub(crate) struct Rerank {
    api: ModelApi,
}

#[derive(Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [&'a str],
}

#[derive(Deserialize)]
struct RerankReply {
    #[serde(default, deserialize_with = "present_model")]
    model: Option<String>,
    results: Vec<RerankItem>,
}

#[derive(Deserialize)]
struct RerankItem {
    index: usize,
    relevance_score: f64,
}

impl Rerank {
    pub(crate) fn new(config: ApiConfig) -> Result<Self, ModelError> {
        Ok(Self {
            api: ModelApi::new(config, "rerank")?,
        })
    }

    pub(crate) async fn scores(
        &self,
        query: &str,
        candidates: &[&str],
    ) -> Result<Vec<f64>, ModelError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let reply: RerankReply = self
            .api
            .post(RerankRequest {
                model: &self.api.model,
                query,
                documents: candidates,
            })
            .await?;
        self.api.validate_reply(
            reply.model.as_deref(),
            candidates.len(),
            reply.results.len(),
        )?;
        let mut scores = vec![f64::NAN; candidates.len()];
        for item in reply.results {
            let slot = scores.get_mut(item.index).ok_or(ModelError::InvalidIndex)?;
            if !slot.is_nan() {
                return Err(ModelError::InvalidIndex);
            }
            if !item.relevance_score.is_finite() {
                return Err(ModelError::InvalidScore);
            }
            *slot = item.relevance_score;
        }
        Ok(scores)
    }
}

#[cfg(test)]
mod tests;
