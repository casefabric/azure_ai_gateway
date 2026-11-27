use actix_web::http::header::{self, HeaderMap};
use actix_web::http::{Method, StatusCode, Uri};
use actix_web::web::{self, Bytes};
use actix_web::{HttpRequest, HttpResponse};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::anthropic::{self, AnthropicClient, ChatStream};
use crate::foundry::{self, Deployment, DeploymentList};
use crate::ollama::{self, OllamaClient};
use crate::requestlog::{self, Fingerprint, PromptShape, RequestLog, RequestRecord, ResponseTap};
use crate::route::{self, Route};
use crate::savings::{self, LoopGuard, ToolsTidy};

/// Bodies longer than this are truncated in the debug log.
const MAX_LOGGED_BODY: usize = 4000;

/// Where chat requests and the deployment list come from.
pub enum Backend {
    Ollama(OllamaClient),
    Anthropic(AnthropicClient),
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Ollama(_) => "Ollama",
            Backend::Anthropic(_) => "Anthropic",
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            Backend::Ollama(ollama) => ollama.base_url(),
            Backend::Anthropic(anthropic) => anthropic.base_url(),
        }
    }
}

pub struct AppState {
    pub backend: Backend,
    /// When set, callers must send this key as `api-key` or `Authorization: Bearer`.
    pub api_key: Option<String>,
    /// Tools whose name matches are removed from chat requests.
    pub tools_deny: Option<regex::Regex>,
    pub loop_guard: LoopGuard,
    pub request_log: Arc<RequestLog>,
}

/// Single entry point: every request is logged, then dispatched on its path suffix.
pub async fn handle(
    state: web::Data<AppState>,
    req: HttpRequest,
    web::Query(query): web::Query<HashMap<String, String>>,
    body: Bytes,
) -> HttpResponse {
    let (method, uri, headers) = (req.method(), req.uri(), req.headers());
    log_request(method, uri, headers, &body);

    if !authorized(&state, headers) {
        log::warn!(
            "Rejected {method} {} with missing or wrong API key",
            uri.path()
        );
        return error(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "Missing or invalid API key",
        );
    }

    let route = route::parse(method, uri.path());
    if matches!(route, Route::ChatCompletions(_))
        && state.loop_guard.repeated_too_often(uri.path(), &body)
    {
        log::warn!(
            "Refused {} by the loop guard: the same request arrived more than {} times within a minute",
            uri.path(),
            state.loop_guard.max()
        );
        let mut record = state.request_log.start(state.backend.name(), uri.path());
        record.status = StatusCode::TOO_MANY_REQUESTS.as_u16();
        record.error_code = Some("LoopGuard".into());
        record.loop_guard = true;
        state.request_log.finish(record, Instant::now());
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "LoopGuard",
            "The same request was repeated too often; it is not sent to the backend again",
        );
    }

    match (route, &state.backend) {
        (Route::ListDeployments, _) => list_deployments(&state, &query).await,
        (Route::GetDeployment(name), _) => get_deployment(&state, &name).await,
        (Route::ChatCompletions(deployment), Backend::Ollama(ollama)) => {
            ollama_chat(&state, ollama, uri.path(), deployment.as_deref(), body).await
        }
        (Route::ChatCompletions(deployment), Backend::Anthropic(anthropic)) => {
            anthropic_chat(&state, anthropic, uri.path(), deployment.as_deref(), body).await
        }
        (Route::Embeddings(deployment), Backend::Ollama(ollama)) => {
            forward(
                &state,
                ollama,
                "/v1/embeddings",
                deployment.as_deref(),
                body,
            )
            .await
        }
        (Route::Embeddings(_), Backend::Anthropic(_)) => {
            log::warn!("Embeddings requested, but the Anthropic backend has no embeddings");
            error(
                StatusCode::BAD_REQUEST,
                "OperationNotSupported",
                "The Anthropic backend does not offer embeddings",
            )
        }
        (Route::Models, Backend::Ollama(ollama)) => {
            stream_response(&state, ollama.request(reqwest::Method::GET, "/v1/models")).await
        }
        (Route::Models, Backend::Anthropic(anthropic)) => match anthropic.models().await {
            Ok(models) => HttpResponse::Ok().json(anthropic::model_list(&models)),
            Err(e) => backend_unreachable(&state, e),
        },
        (Route::Unknown, _) => {
            log::warn!("No mapping for {method} {uri}");
            error(
                StatusCode::NOT_FOUND,
                "NotFound",
                &format!("No mapping for {method} {}", uri.path()),
            )
        }
    }
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.api_key else {
        return true;
    };
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let bearer = header(header::AUTHORIZATION.as_str()).and_then(|v| v.strip_prefix("Bearer "));
    header("api-key") == Some(expected) || bearer == Some(expected)
}

async fn deployments(state: &AppState) -> reqwest::Result<Vec<Deployment>> {
    match &state.backend {
        Backend::Ollama(ollama) => ollama
            .models()
            .await
            .map(|m| m.iter().map(Deployment::from_ollama).collect()),
        Backend::Anthropic(anthropic) => anthropic
            .models()
            .await
            .map(|m| m.iter().map(Deployment::from_anthropic).collect()),
    }
}

async fn list_deployments(state: &AppState, query: &HashMap<String, String>) -> HttpResponse {
    match deployments(state).await {
        Ok(deployments) => {
            let value: Vec<Deployment> = deployments
                .into_iter()
                .filter(|d| d.matches(query))
                .collect();
            log::info!("Listing {} deployment(s)", value.len());
            HttpResponse::Ok().json(DeploymentList {
                value,
                next_link: None,
            })
        }
        Err(e) => backend_unreachable(state, e),
    }
}

async fn get_deployment(state: &AppState, name: &str) -> HttpResponse {
    match deployments(state).await {
        Ok(deployments) => match deployments.into_iter().find(|d| d.name == name) {
            Some(deployment) => HttpResponse::Ok().json(deployment),
            None => error(
                StatusCode::NOT_FOUND,
                "DeploymentNotFound",
                &format!("Deployment '{name}' not found"),
            ),
        },
        Err(e) => backend_unreachable(state, e),
    }
}

/// Forwards an OpenAI-style POST to Ollama. When the deployment comes from the path
/// (Azure OpenAI style), it is written into the body as `model`.
async fn forward(
    state: &AppState,
    ollama: &OllamaClient,
    path: &str,
    deployment: Option<&str>,
    body: Bytes,
) -> HttpResponse {
    let body = match deployment {
        None => body,
        Some(_) => match with_model(&body, deployment) {
            Some(chat) => Bytes::from(serde_json::to_vec(&chat).expect("JSON object serializes")),
            None => return not_an_object(),
        },
    };

    let request = ollama
        .request(reqwest::Method::POST, path)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    stream_response(state, request).await
}

/// Forwards a chat completion to Ollama, first turning `file` parts (which Ollama rejects) into
/// text or image parts.
async fn ollama_chat(
    state: &AppState,
    ollama: &OllamaClient,
    path: &str,
    deployment: Option<&str>,
    body: Bytes,
) -> HttpResponse {
    let started = Instant::now();
    let Some(mut chat) = with_model(&body, deployment) else {
        return not_an_object();
    };
    let original = full_log(state).then(|| chat.clone());
    // A stable tool order lets Ollama reuse the prompt it has already processed (KV cache).
    let tidy = savings::tidy_tools(&mut chat, state.tools_deny.as_ref());
    let mut record = chat_record(state, path, deployment, &chat, &tidy, original);
    // Extracting PDF text is CPU-bound, so it runs off the async workers.
    let inlined =
        web::block(move || ollama::inline_files(&mut chat).map(|changed| (changed, chat)));
    let body = match inlined.await {
        Ok(Ok((false, _))) if deployment.is_none() && !tidy.changed() => body,
        Ok(Ok((_, chat))) => {
            Bytes::from(serde_json::to_vec(&chat).expect("JSON object serializes"))
        }
        Ok(Err(message)) => {
            fail(
                state,
                record,
                started,
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
            );
            return error(StatusCode::BAD_REQUEST, "InvalidRequest", &message);
        }
        Err(e) => {
            log::error!("Could not convert file parts: {e}");
            fail(
                state,
                record,
                started,
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
            );
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "Could not convert file parts",
            );
        }
    };
    if full_log(state) {
        record.sent = serde_json::from_slice(&body).ok().map(redacted);
    }

    let request = ollama
        .request(reqwest::Method::POST, "/v1/chat/completions")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            fail(
                state,
                record,
                started,
                StatusCode::BAD_GATEWAY,
                "BadGateway",
            );
            return backend_unreachable(state, e);
        }
    };
    let mut builder = response_builder(&response);
    record.status = response.status().as_u16();
    let sse = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"));
    // The tap reads usage from the bytes going to the caller and writes the record when done.
    let mut tap = ResponseTap::new(state.request_log.clone(), record, started, sse);
    builder.streaming(response.bytes_stream().map(move |chunk| {
        if let Ok(bytes) = &chunk {
            tap.observe(bytes);
        }
        chunk
    }))
}

/// Starts the request record of a chat request (after `tidy_tools`) and compares it with the
/// previous request of the same conversation.
fn chat_record(
    state: &AppState,
    path: &str,
    deployment: Option<&str>,
    chat: &Value,
    tidy: &ToolsTidy,
    original: Option<Value>,
) -> RequestRecord {
    let mut record = state.request_log.start(state.backend.name(), path);
    record.deployment = deployment.map(String::from);
    record.model = chat["model"].as_str().map(String::from);
    record.stream = chat["stream"] == true;
    record.prompt = Some(PromptShape::of_chat(chat, tidy));
    record.fingerprint = Some(Fingerprint::of_chat(chat));
    record.cache_hints = requestlog::cache_hints(chat);
    record.request = original.map(redacted);
    state.request_log.observe(&mut record);
    record
}

/// Writes the record of a chat request that failed before or at the backend.
fn fail(
    state: &AppState,
    mut record: RequestRecord,
    started: Instant,
    status: StatusCode,
    code: &str,
) {
    record.status = status.as_u16();
    record.error_code = Some(code.to_string());
    state.request_log.finish(record, started);
}

fn full_log(state: &AppState) -> bool {
    state.request_log.mode() == requestlog::Mode::Full
}

fn redacted(mut value: Value) -> Value {
    requestlog::redact_media(&mut value);
    value
}

/// Parses a JSON object body, setting `model` to the deployment from the path when there is one.
/// `None` when the body isn't a JSON object.
fn with_model(body: &Bytes, deployment: Option<&str>) -> Option<serde_json::Value> {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(mut map)) => {
            if let Some(deployment) = deployment {
                map.insert("model".into(), deployment.into());
            }
            Some(serde_json::Value::Object(map))
        }
        _ => None,
    }
}

fn not_an_object() -> HttpResponse {
    error(
        StatusCode::BAD_REQUEST,
        "InvalidRequest",
        "Request body must be a JSON object",
    )
}

/// Translates a chat completion to the Anthropic Messages API and the response back,
/// including `stream: true` server-sent events.
async fn anthropic_chat(
    state: &AppState,
    client: &AnthropicClient,
    path: &str,
    deployment: Option<&str>,
    body: Bytes,
) -> HttpResponse {
    let started = Instant::now();
    let Some(mut chat) = with_model(&body, deployment) else {
        return not_an_object();
    };
    let original = full_log(state).then(|| chat.clone());
    let tidy = savings::tidy_tools(&mut chat, state.tools_deny.as_ref());
    let mut record = chat_record(state, path, deployment, &chat, &tidy, original);
    let mut request = match anthropic::messages_request(&chat, &client.options) {
        Ok(request) => request,
        Err(message) => {
            fail(
                state,
                record,
                started,
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
            );
            return error(StatusCode::BAD_REQUEST, "InvalidRequest", &message);
        }
    };
    if client.cache_diagnostics {
        // The API compares this request with the previous one and reports where they diverge.
        let previous = state.request_log.previous_message_id(&record);
        request["diagnostics"] = serde_json::json!({ "previous_message_id": previous });
    }
    if let Some(prompt) = &mut record.prompt {
        prompt.add_anthropic(&request, &anthropic::betas(&request));
    }
    if full_log(state) {
        record.sent = Some(redacted(request.clone()));
    }

    let response = match client.messages(&request).send().await {
        Ok(response) => response,
        Err(e) => {
            fail(
                state,
                record,
                started,
                StatusCode::BAD_GATEWAY,
                "BadGateway",
            );
            return backend_unreachable(state, e);
        }
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.bytes().await.unwrap_or_default();
        let (code, message) = anthropic::error_details(&body);
        log::warn!("Anthropic responded {status}: {message}");
        let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        fail(state, record, started, status, &code);
        return error(status, &code, &message);
    }
    log::info!("Anthropic responded {status}");
    record.status = status.as_u16();

    if request["stream"] == true {
        let mut stream = ChatStream::new(chat["stream_options"]["include_usage"] == true);
        // The tap sees the translated chunks and writes the record when the stream ends.
        let mut tap = ResponseTap::new(state.request_log.clone(), record, started, true);
        let diagnostics = client.cache_diagnostics;
        return HttpResponse::Ok()
            .content_type("text/event-stream")
            .streaming(response.bytes_stream().map(move |chunk| {
                chunk.map(|bytes| {
                    let out = stream.feed(&bytes);
                    tap.set_anthropic(stream.usage(), stream.empty_turn());
                    if let Some(id) = stream.message_id() {
                        tap.set_message(Some(id), diagnostics.then(|| stream.diagnostics()));
                    }
                    tap.observe(&out);
                    out
                })
            }));
    }
    match response.json::<Value>().await {
        Ok(message) => {
            let (completion, usage) = anthropic::chat_completion(&message);
            usage.log(message["model"].as_str().unwrap_or_default());
            record.model_answered = message["model"].as_str().map(String::from);
            record.finish_reason = completion["choices"][0]["finish_reason"]
                .as_str()
                .map(String::from);
            record.usage = Some(requestlog::UsageRecord::from_anthropic(&usage));
            record.empty_turn = anthropic::is_empty_turn(&message);
            record.message_id = message["id"].as_str().map(String::from);
            if client.cache_diagnostics {
                record.diagnostics = Some(message["diagnostics"].clone());
            }
            if full_log(state) {
                record.response = Some(completion["choices"][0]["message"].clone());
            }
            state.request_log.finish(record, started);
            HttpResponse::Ok().json(completion)
        }
        Err(e) => {
            fail(
                state,
                record,
                started,
                StatusCode::BAD_GATEWAY,
                "BadGateway",
            );
            backend_unreachable(state, e)
        }
    }
}

/// Sends the request and streams Ollama's response back unchanged, so `stream: true`
/// server-sent events reach the caller as they are produced.
async fn stream_response(state: &AppState, request: reqwest::RequestBuilder) -> HttpResponse {
    match request.send().await {
        Ok(response) => response_builder(&response).streaming(response.bytes_stream()),
        Err(e) => backend_unreachable(state, e),
    }
}

/// Logs Ollama's status and starts a response with the same status and `Content-Type`.
fn response_builder(response: &reqwest::Response) -> actix_web::HttpResponseBuilder {
    let status = response.status();
    if status.is_success() {
        log::info!("Ollama responded {status}");
    } else {
        log::warn!("Ollama responded {status}");
    }

    // reqwest and actix-web use different `http` crate versions, so status and headers are converted by value.
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = HttpResponse::build(status);
    if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE) {
        builder.insert_header((header::CONTENT_TYPE, content_type.as_bytes()));
    }
    builder
}

fn backend_unreachable(state: &AppState, e: reqwest::Error) -> HttpResponse {
    let backend = state.backend.name();
    log::error!(
        "{backend} request to {} failed: {e}",
        state.backend.base_url()
    );
    error(
        StatusCode::BAD_GATEWAY,
        "BadGateway",
        &format!("{backend} request failed: {e}"),
    )
}

fn error(status: StatusCode, code: &str, message: &str) -> HttpResponse {
    HttpResponse::build(status).json(foundry::error_body(code, message))
}

/// Logs every incoming call, so unknown paths Mendix uses show up in the log.
fn log_request(method: &Method, uri: &Uri, headers: &HeaderMap, body: &Bytes) {
    log::info!("{method} {uri}");
    if log::log_enabled!(log::Level::Debug) {
        for (name, value) in headers {
            let value = if matches!(name.as_str(), "api-key" | "authorization") {
                "<redacted>"
            } else {
                value.to_str().unwrap_or("<binary>")
            };
            log::debug!("  {name}: {value}");
        }
        if !body.is_empty() {
            let text = String::from_utf8_lossy(body);
            match text.char_indices().nth(MAX_LOGGED_BODY) {
                Some((cut, _)) => log::debug!("  body ({} bytes): {}…", body.len(), &text[..cut]),
                None => log::debug!("  body: {text}"),
            }
        }
    }
}
