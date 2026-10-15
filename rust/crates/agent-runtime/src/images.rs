//! Native image-provider registry and the original OpenRouter chat-images API.
//! No provider request is made by catalog lookup or registry construction.
use agent_core::types::{timestamp, CancellationToken, Content, StopReason, Usage};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{header::HeaderMap, redirect::Policy, Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const IMAGE_SNAPSHOT: &str = include_str!("image-models.snapshot.json");
const BODY_LIMIT: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesModel {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    pub input: Vec<String>,
    pub output: Vec<String>,
    pub cost: Value,
    #[serde(default)]
    pub headers: BTreeMap<String, Option<String>>,
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

pub fn parse_image_catalog(source: &str) -> Result<Vec<ImagesModel>, String> {
    let value: Value = serde_json::from_str(source).map_err(|_| "Invalid image catalog JSON")?;
    if value["schemaVersion"] != 1 {
        return Err("Unsupported image catalog schema".into());
    }
    let providers = value["providers"]
        .as_object()
        .ok_or("Image providers missing")?;
    let mut models = Vec::new();
    for (provider, entries) in providers {
        for (id, raw) in entries.as_object().ok_or("Invalid image provider models")? {
            let model: ImagesModel =
                serde_json::from_value(raw.clone()).map_err(|_| "Invalid image model")?;
            if model.provider != *provider
                || model.id != *id
                || model.api.is_empty()
                || model.id.is_empty()
                || model.name.is_empty()
                || !model.output.iter().any(|v| v == "image")
            {
                return Err("Image catalog identity or modalities differ".into());
            }
            for name in ["input", "output", "cacheRead", "cacheWrite"] {
                // Routing models preserve the original negative "unknown price" sentinel.
                if !model.cost[name].as_f64().is_some_and(|v| v.is_finite()) {
                    return Err("Invalid image model cost".into());
                }
            }
            models.push(model);
        }
    }
    if models.is_empty() {
        return Err("Image catalog is empty".into());
    }
    Ok(models)
}

pub fn image_models() -> &'static [ImagesModel] {
    static MODELS: OnceLock<Vec<ImagesModel>> = OnceLock::new();
    MODELS.get_or_init(|| parse_image_catalog(IMAGE_SNAPSHOT).expect("validated image snapshot"))
}

pub fn image_model(provider: &str, id: &str) -> Option<&'static ImagesModel> {
    image_models()
        .iter()
        .find(|model| model.provider == provider && model.id == id)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImagesContext {
    pub input: Vec<Content>,
}

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImagesUsage {
    #[serde(flatten)]
    pub tokens: Usage,
    pub cost: ImagesCost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    pub api: String,
    pub provider: String,
    pub model: String,
    pub output: Vec<Content>,
    pub response_id: Option<String>,
    pub usage: Option<ImagesUsage>,
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
    pub timestamp: u64,
}

impl AssistantImages {
    fn empty(model: &ImagesModel) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: vec![],
            response_id: None,
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: timestamp(),
        }
    }
    fn failure(model: &ImagesModel, signal: &CancellationToken, error: String) -> Self {
        let mut result = Self::empty(model);
        result.stop_reason = if signal.is_cancelled() {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        result.error_message = Some(error);
        result
    }
}

pub struct ImagesOptions {
    pub api_key: String,
    pub headers: BTreeMap<String, Option<String>>,
    pub timeout_ms: u64,
    pub max_retries: u32,
    /// Zero disables the server-delay ceiling, not cancellation or request timeouts.
    pub max_retry_delay_ms: u64,
}
impl Default for ImagesOptions {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            headers: BTreeMap::new(),
            timeout_ms: 600_000,
            max_retries: 0,
            max_retry_delay_ms: 60_000,
        }
    }
}
impl std::fmt::Debug for ImagesOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagesOptions")
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

/// Native equivalents of onPayload/onResponse. Failures never include callback secrets.
pub trait ImagesHooks: Send {
    fn on_payload(&mut self, _payload: &mut Value, _model: &ImagesModel) -> Result<(), String> {
        Ok(())
    }
    fn on_response(
        &mut self,
        _status: u16,
        _headers: &BTreeMap<String, String>,
        _model: &ImagesModel,
    ) -> Result<(), String> {
        Ok(())
    }
}
pub struct NoImagesHooks;
impl ImagesHooks for NoImagesHooks {}

pub trait ImagesApi: Send + Sync {
    fn generate_images(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: &ImagesOptions,
        signal: &CancellationToken,
        hooks: &mut dyn ImagesHooks,
    ) -> AssistantImages;
}

pub struct ImagesRegistry(BTreeMap<String, Arc<dyn ImagesApi>>);
impl Default for ImagesRegistry {
    fn default() -> Self {
        Self(BTreeMap::from([(
            "openrouter-images".into(),
            Arc::new(OpenRouterImages) as Arc<dyn ImagesApi>,
        )]))
    }
}
impl ImagesRegistry {
    /// Explicit native registration replaces an API's previous implementation, as the old registry did.
    pub fn register(
        &mut self,
        api: &str,
        implementation: Arc<dyn ImagesApi>,
    ) -> Result<(), String> {
        if api.trim().is_empty() || api.len() > 256 || api.bytes().any(|b| b.is_ascii_control()) {
            return Err("Invalid image API ID".into());
        }
        self.0.insert(api.to_owned(), implementation);
        Ok(())
    }
    pub fn generate_images(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: &ImagesOptions,
        signal: &CancellationToken,
        hooks: &mut dyn ImagesHooks,
    ) -> AssistantImages {
        match self.0.get(&model.api) {
            Some(api) => api.generate_images(model, context, options, signal, hooks),
            None => AssistantImages::failure(model, signal, "Image API is not registered".into()),
        }
    }
}

pub fn generate_images(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
    signal: &CancellationToken,
) -> AssistantImages {
    ImagesRegistry::default().generate_images(model, context, options, signal, &mut NoImagesHooks)
}

pub fn images_payload(model: &ImagesModel, context: &ImagesContext) -> Result<Value, String> {
    let mut content = vec![];
    for item in &context.input {
        match item {
            Content::Text { text } => content.push(json!({"type":"text", "text":text})),
            Content::Image { data, mime_type } => {
                if !mime_type.starts_with("image/")
                    || mime_type.contains([';', '\r', '\n'])
                    || data.len() > 16 * 1024 * 1024
                    || STANDARD.decode(data).is_err()
                {
                    return Err("Invalid image generation input".into());
                }
                content.push(json!({"type":"image_url","image_url":{"url":format!("data:{mime_type};base64,{data}")}}));
            }
            _ => return Err("Image input requires text or image content".into()),
        }
    }
    let modalities = if model.output.iter().any(|output| output == "text") {
        vec!["image", "text"]
    } else {
        vec!["image"]
    };
    Ok(
        json!({"model":model.id,"messages":[{"role":"user","content":content}],"stream":false,"modalities":modalities}),
    )
}

pub fn parse_images_response(
    model: &ImagesModel,
    value: &Value,
) -> Result<AssistantImages, String> {
    if !value.is_object() || value.get("error").is_some() {
        return Err("Image provider returned an invalid response".into());
    }
    let mut output = AssistantImages::empty(model);
    output.response_id = value["id"].as_str().map(str::to_owned);
    let choice = value["choices"]
        .as_array()
        .ok_or("Image response choices missing")?
        .first();
    if let Some(choice) = choice {
        let message = &choice["message"];
        if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
            output.output.push(Content::text(text));
        }
        if let Some(images) = message["images"].as_array() {
            if images.len() > 64 {
                return Err("Image response count exceeds limit".into());
            }
            for image in images {
                let url = image["image_url"]
                    .as_str()
                    .or_else(|| image["image_url"]["url"].as_str());
                if let Some((mime_type, data)) = url
                    .and_then(|url| url.strip_prefix("data:"))
                    .and_then(|url| url.split_once(";base64,"))
                {
                    if !mime_type.is_empty() && !data.is_empty() && !mime_type.contains(';') {
                        // Preserve original data URLs; do not fetch remote images or reinterpret provider bytes.
                        output.output.push(Content::Image {
                            data: data.into(),
                            mime_type: mime_type.into(),
                        });
                    }
                }
            }
        }
    }
    if value["usage"].is_object() {
        let raw = &value["usage"];
        let prompt = raw["prompt_tokens"].as_u64().unwrap_or(0);
        let reported_cached = raw["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let cache_write = raw["prompt_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .unwrap_or(0);
        let cache_read = reported_cached.saturating_sub(cache_write);
        let input = prompt
            .saturating_sub(cache_read)
            .saturating_sub(cache_write);
        let tokens = raw["completion_tokens"].as_u64().unwrap_or(0);
        let usage = Usage {
            input,
            output: tokens,
            cache_read,
            cache_write,
            total_tokens: input
                .saturating_add(tokens)
                .saturating_add(cache_read)
                .saturating_add(cache_write),
        };
        let price = |name: &str, tokens: u64| {
            (model.cost[name].as_f64().unwrap_or(0.0) / 1_000_000.0) * tokens as f64
        };
        let mut cost = ImagesCost {
            input: price("input", input),
            output: price("output", tokens),
            cache_read: price("cacheRead", cache_read),
            cache_write: price("cacheWrite", cache_write),
            total: 0.0,
        };
        cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
        output.usage = Some(ImagesUsage {
            tokens: usage,
            cost,
        });
    }
    Ok(output)
}

struct OpenRouterImages;
impl ImagesApi for OpenRouterImages {
    fn generate_images(
        &self,
        model: &ImagesModel,
        context: &ImagesContext,
        options: &ImagesOptions,
        signal: &CancellationToken,
        hooks: &mut dyn ImagesHooks,
    ) -> AssistantImages {
        let run = || -> Result<AssistantImages, String> {
            if signal.is_cancelled() {
                return Err("Operation aborted".into());
            }
            if options.api_key.trim().is_empty() || options.api_key.contains(['\r', '\n']) {
                return Err("Image API key missing or invalid".into());
            }
            if options.timeout_ms == 0 || options.timeout_ms > 3_600_000 || options.max_retries > 32
            {
                return Err("Invalid image request limits".into());
            }
            let mut url = Url::parse(&model.base_url).map_err(|_| "Invalid image provider URL")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("Invalid image provider URL".into());
            }
            url.set_path(&format!(
                "{}/chat/completions",
                url.path().trim_end_matches('/')
            ));
            let mut payload = images_payload(model, context)?;
            hooks
                .on_payload(&mut payload, model)
                .map_err(|_| "Image payload callback failed")?;
            if !payload.is_object() {
                return Err("Image payload must be an object".into());
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "Image request runtime unavailable")?;
            runtime.block_on(async {
                let client = Client::builder().redirect(Policy::none()).connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_millis(options.timeout_ms)).build().map_err(|_| "Image HTTP client unavailable")?;
                let mut headers = BTreeMap::new();
                for (name, value) in model.headers.iter().chain(options.headers.iter()) {
                    if let Some(value) = value { headers.insert(name.to_ascii_lowercase(), value.clone()); }
                    else { headers.remove(&name.to_ascii_lowercase()); }
                }
                for attempt in 0..=options.max_retries {
                    if signal.is_cancelled() { return Err("Operation aborted".into()); }
                    let mut request = client.post(url.clone()).bearer_auth(&options.api_key).json(&payload);
                    for (name, value) in &headers { request = request.header(name, value); }
                    let sending = request.send();
                    let response = tokio::select! {
                        response = sending => response,
                        _ = cancelled(signal) => return Err("Operation aborted".into()),
                    };
                    let mut response = match response {
                        Ok(response) => response,
                        Err(_) if attempt < options.max_retries => {
                            wait_retry(500u64.saturating_mul(1 << attempt.min(4)), signal).await?;
                            continue;
                        }
                        Err(_) => return Err("Image HTTP request failed".into()),
                    };
                    if !response.status().is_success() {
                        let status = response.status().as_u16();
                        let retry = match response.headers().get("x-should-retry").and_then(|v| v.to_str().ok()) {
                            Some("true") => true, Some("false") => false,
                            _ => matches!(status, 408 | 409 | 429) || status >= 500,
                        };
                        if retry && attempt < options.max_retries {
                            let delay = retry_delay(response.headers(), attempt, options.max_retry_delay_ms)?;
                            drop(response);
                            wait_retry(delay, signal).await?;
                            continue;
                        }
                        return Err(format!("Image provider returned HTTP {status}"));
                    }
                    let response_headers = response.headers().iter().filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().into(), value.into()))).collect();
                    hooks.on_response(response.status().as_u16(), &response_headers, model).map_err(|_| "Image response callback failed")?;
                    let mut bytes = Vec::new();
                    loop {
                        let chunk = tokio::select! {
                            chunk = response.chunk() => chunk.map_err(|_| "Image response body failed")?,
                            _ = cancelled(signal) => return Err("Operation aborted".into()),
                        };
                        let Some(chunk) = chunk else { break; };
                        if bytes.len().saturating_add(chunk.len()) > BODY_LIMIT { return Err("Image response exceeds limit".into()); }
                        bytes.extend_from_slice(&chunk);
                    }
                    if signal.is_cancelled() { return Err("Operation aborted".into()); }
                    let value = serde_json::from_slice(&bytes).map_err(|_| "Invalid image response JSON")?;
                    return parse_images_response(model, &value);
                }
                Err("Image retry limit exceeded".into())
            })
        };
        // A scoped worker also supports synchronous callers inside an existing Tokio runtime.
        match std::thread::scope(|scope| scope.spawn(run).join()) {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => AssistantImages::failure(model, signal, error),
            Err(_) => AssistantImages::failure(model, signal, "Image request worker failed".into()),
        }
    }
}

async fn cancelled(signal: &CancellationToken) {
    while !signal.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
async fn wait_retry(delay: u64, signal: &CancellationToken) -> Result<(), String> {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(delay)) => Ok(()),
        _ = cancelled(signal) => Err("Operation aborted".into()),
    }
}

fn retry_delay(headers: &HeaderMap, attempt: u32, max: u64) -> Result<u64, String> {
    let number = |name, factor| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| (v * factor) as u64)
    };
    let delay = number("retry-after-ms", 1.0)
        .or_else(|| number("retry-after", 1000.0))
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(http_date)
                .map(|when| when.saturating_sub(timestamp()))
        })
        .unwrap_or_else(|| 500u64.saturating_mul(1 << attempt.min(4)).min(8000));
    if max > 0 && delay > max {
        return Err("Image server retry delay exceeds configured limit".into());
    }
    Ok(delay)
}

// RFC 7231 IMF-fixdate, independent of locale/timezone and external date libraries.
fn http_date(value: &str) -> Option<u64> {
    let p: Vec<_> = value.split_ascii_whitespace().collect();
    if p.len() != 6 || p[5] != "GMT" {
        return None;
    }
    let day = p[1].parse::<u64>().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == p[2])?;
    let year = p[3].parse::<u64>().ok()?;
    if !(1970..=9999).contains(&year) {
        return None;
    }
    let time = p[4]
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if time.len() != 3 || time[0] >= 24 || time[1] >= 60 || time[2] >= 60 {
        return None;
    }
    let leap = |y: u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let days = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > days[month] {
        return None;
    }
    let count = (1970..year)
        .map(|y| if leap(y) { 366 } else { 365 })
        .sum::<u64>()
        + days[..month].iter().sum::<u64>()
        + day
        - 1;
    Some((count * 86400 + time[0] * 3600 + time[1] * 60 + time[2]) * 1000)
}
