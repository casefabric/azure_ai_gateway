use actix_web::web::Bytes;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::files::File;

const API_VERSION: &str = "2023-06-01";
/// Beta header for `fallbacks: "default"`, which re-runs a request declined by a safety
/// classifier on the model Anthropic recommends for that refusal category.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Models that accept `fallbacks: "default"`; on other models the parameter is rejected or useless.
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];

/// A model from the Anthropic Models API (`GET /v1/models`).
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicModel {
    pub id: String,
    #[serde(default)]
    pub capabilities: Option<Value>,
}

impl AnthropicModel {
    /// Whether the model reports a capability as supported, e.g. `image_input`.
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|caps| caps[capability]["supported"] == true)
    }
}

#[derive(Deserialize)]
struct ModelsPage {
    data: Vec<AnthropicModel>,
    #[serde(default)]
    has_more: bool,
    last_id: Option<String>,
}

/// Proxy settings that shape every translated Messages request, see [`messages_request`].
#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    /// `max_tokens` used when the caller doesn't send `max_tokens` / `max_completion_tokens`.
    pub max_tokens: u64,
    /// Whether `fallbacks: "default"` is added for models that support it.
    pub fallbacks: bool,
    /// `output_config.effort` used when the caller doesn't send `reasoning_effort`.
    pub effort: Option<String>,
}

#[derive(Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub options: RequestOptions,
    /// Whether requests ask for cache diagnostics (`--anthropic-cache-diagnostics`).
    pub cache_diagnostics: bool,
}

impl AnthropicClient {
    pub fn new(
        base_url: &str,
        api_key: String,
        options: RequestOptions,
        timeout: Duration,
    ) -> reqwest::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            options,
            cache_diagnostics: false,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
    }

    /// Lists all models, following the Models API pagination.
    pub async fn models(&self) -> reqwest::Result<Vec<AnthropicModel>> {
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let path = match &after {
                Some(id) => format!("/v1/models?limit=1000&after_id={id}"),
                None => "/v1/models?limit=1000".to_string(),
            };
            let page: ModelsPage = self
                .request(reqwest::Method::GET, &path)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            models.extend(page.data);
            match page.last_id {
                Some(id) if page.has_more => after = Some(id),
                _ => return Ok(models),
            }
        }
    }

    /// Builds a `POST /v1/messages` request for a body made by [`messages_request`].
    pub fn messages(&self, body: &Value) -> reqwest::RequestBuilder {
        let request = self
            .request(reqwest::Method::POST, "/v1/messages")
            .json(body);
        match betas(body).as_slice() {
            [] => request,
            betas => request.header("anthropic-beta", betas.join(",")),
        }
    }
}

/// The `anthropic-beta` values a body made by [`messages_request`] needs.
pub fn betas(body: &Value) -> Vec<&'static str> {
    let mut betas = Vec::new();
    if body.get("fallbacks").is_some() {
        betas.push(FALLBACK_BETA);
    }
    betas
}

/// Translates an OpenAI chat-completions request into an Anthropic Messages request.
///
/// Sampling parameters (`temperature`, `top_p`, …) are dropped, because current Claude models
/// reject non-default values. Errors are messages for a 400 response.
///
/// The request is marked for prompt caching (see [`mark_cache_breakpoints`]); tools are expected
/// in a stable order already (`savings::tidy_tools`).
///
/// Object keys come out sorted (`serde_json` without `preserve_order`), so the same request always
/// gives the same bytes, which prompt caching needs.
pub fn messages_request(chat: &Value, options: &RequestOptions) -> Result<Value, String> {
    let chat = chat
        .as_object()
        .ok_or("Request body must be a JSON object")?;
    let model = chat
        .get("model")
        .and_then(Value::as_str)
        .ok_or("Request body has no model")?;
    let input = chat
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("Request body has no messages")?;

    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for message in input {
        match message["role"].as_str().unwrap_or_default() {
            "system" | "developer" => system.extend(content_blocks(&message["content"])?),
            "user" => push(&mut messages, "user", content_blocks(&message["content"])?),
            "assistant" => {
                let mut blocks = content_blocks(&message["content"])?;
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    let arguments = call["function"]["arguments"].as_str().unwrap_or("{}");
                    let input: Value =
                        serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"],
                        "name": call["function"]["name"],
                        "input": input,
                    }));
                }
                push(&mut messages, "assistant", blocks);
            }
            "tool" => {
                let result = json!({
                    "type": "tool_result",
                    "tool_use_id": message["tool_call_id"],
                    "content": text_of(&message["content"]),
                });
                push(&mut messages, "user", vec![result]);
            }
            role => return Err(format!("Unsupported message role '{role}'")),
        }
    }

    let max_tokens = chat
        .get("max_completion_tokens")
        .or_else(|| chat.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(options.max_tokens);

    let mut tools: Vec<Value> = chat
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(tool)
        .collect();
    mark_cache_breakpoints(&mut tools, &mut system, &mut messages);

    let mut request = Map::new();
    request.insert("model".into(), model.into());
    request.insert("max_tokens".into(), max_tokens.into());
    if !system.is_empty() {
        request.insert("system".into(), system.into());
    }
    request.insert("messages".into(), messages.into());
    if chat.get("stream") == Some(&Value::Bool(true)) {
        request.insert("stream".into(), true.into());
    }
    match chat.get("stop") {
        Some(Value::String(stop)) => {
            request.insert("stop_sequences".into(), json!([stop]));
        }
        Some(stop @ Value::Array(_)) => {
            request.insert("stop_sequences".into(), stop.clone());
        }
        _ => {}
    }

    if !tools.is_empty() {
        request.insert("tools".into(), tools.into());
        let mut choice = match chat.get("tool_choice") {
            Some(Value::String(choice)) if choice == "none" => json!({"type": "none"}),
            Some(Value::String(choice)) if choice == "required" => json!({"type": "any"}),
            Some(Value::Object(choice)) => {
                json!({"type": "tool", "name": choice["function"]["name"]})
            }
            _ => json!({"type": "auto"}),
        };
        if chat.get("parallel_tool_calls") == Some(&Value::Bool(false)) && choice["type"] != "none"
        {
            choice["disable_parallel_tool_use"] = true.into();
        }
        request.insert("tool_choice".into(), choice);
    }

    let mut output_config = Map::new();
    let effort = chat.get("reasoning_effort").and_then(Value::as_str);
    if let Some(effort) = effort.or(options.effort.as_deref()) {
        let effort = if effort == "minimal" { "low" } else { effort };
        output_config.insert("effort".into(), effort.into());
    }
    let format = chat.get("response_format").unwrap_or(&Value::Null);
    if format["type"] == "json_schema" {
        output_config.insert(
            "format".into(),
            json!({"type": "json_schema", "schema": format["json_schema"]["schema"]}),
        );
    }
    if !output_config.is_empty() {
        request.insert("output_config".into(), output_config.into());
    }

    if options.fallbacks && FALLBACK_MODELS.contains(&model) {
        request.insert("fallbacks".into(), "default".into());
    }
    Ok(Value::Object(request))
}

/// Marks up to four prompt-cache breakpoints: the last tool, the last system block, the last block
/// of the previous user turn and the last block of the conversation. Callers resend tools, system
/// prompt and the whole conversation on every step of a tool loop; with the breakpoints the
/// repeated prefix is read from the cache (about 0.1× the input price) instead of paid in full.
/// The previous user turn is a read point that survives a last turn adding many blocks.
fn mark_cache_breakpoints(tools: &mut [Value], system: &mut [Value], messages: &mut [Value]) {
    fn mark(block: Option<&mut Value>) {
        if let Some(block) = block {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    mark(tools.last_mut());
    mark(system.last_mut());
    let users: Vec<usize> = (0..messages.len())
        .filter(|&i| messages[i]["role"] == "user")
        .collect();
    let previous_user = users.len().checked_sub(2).map(|i| users[i]);
    let last = messages.len().checked_sub(1);
    for i in previous_user.into_iter().chain(last) {
        mark(
            messages[i]["content"]
                .as_array_mut()
                .and_then(|c| c.last_mut()),
        );
    }
}

/// Appends content to the conversation, merging consecutive messages of the same role
/// (Anthropic expects alternating roles; OpenAI sends one `tool` message per tool result).
fn push(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    match messages.last_mut() {
        Some(last) if last["role"] == role => last["content"]
            .as_array_mut()
            .expect("content is an array")
            .extend(blocks),
        _ => messages.push(json!({ "role": role, "content": blocks })),
    }
}

/// OpenAI message content (a string or an array of parts) as Anthropic content blocks.
fn content_blocks(content: &Value) -> Result<Vec<Value>, String> {
    match content {
        Value::Null => Ok(Vec::new()),
        Value::String(text) if text.is_empty() => Ok(Vec::new()),
        Value::String(text) => Ok(vec![json!({"type": "text", "text": text})]),
        Value::Array(parts) => parts
            .iter()
            // Anthropic rejects empty text blocks (and a cache breakpoint on one).
            .filter(|part| !(part["type"] == "text" && part["text"] == ""))
            .map(|part| match part["type"].as_str() {
                Some("text") => Ok(json!({"type": "text", "text": part["text"]})),
                Some("image_url") => {
                    let url = part["image_url"]["url"]
                        .as_str()
                        .or(part["image_url"].as_str())
                        .unwrap_or_default();
                    Ok(image(url))
                }
                Some("file") => document(&File::from_part(&part["file"])?),
                Some("input_audio") => Err("Claude does not accept audio input".into()),
                other => Err(format!("Unsupported content part type {other:?}")),
            })
            .collect(),
        _ => Err("Message content must be a string or an array".into()),
    }
}

fn image(url: &str) -> Value {
    let base64 = url
        .strip_prefix("data:")
        .and_then(|data| data.split_once(";base64,"));
    match base64 {
        Some((media_type, data)) => {
            json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}})
        }
        None => json!({"type": "image", "source": {"type": "url", "url": url}}),
    }
}

/// An inline file as a `document` block: PDFs as base64, text files as plain text.
fn document(file: &File) -> Result<Value, String> {
    let source = if file.is_pdf() {
        json!({"type": "base64", "media_type": "application/pdf", "data": file.data})
    } else if file.is_text() {
        json!({"type": "text", "media_type": "text/plain", "data": file.text()?})
    } else if file.media_type.starts_with("image/") {
        return Ok(
            json!({"type": "image", "source": {"type": "base64", "media_type": file.media_type, "data": file.data}}),
        );
    } else {
        return Err(format!(
            "{}: unsupported file type {}",
            file.name(),
            file.media_type
        ));
    };
    let mut block = json!({"type": "document", "source": source});
    if let Some(filename) = file.filename {
        block["title"] = filename.into();
    }
    Ok(block)
}

/// The text of OpenAI message content, with array parts concatenated.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

fn tool(tool: &Value) -> Value {
    let function = &tool["function"];
    let mut result = json!({
        "name": function["name"],
        "input_schema": match &function["parameters"] {
            Value::Null => json!({"type": "object", "properties": {}}),
            schema => schema.clone(),
        },
    });
    if let Some(description) = function["description"].as_str() {
        result["description"] = description.into();
    }
    if function["strict"] == true {
        result["strict"] = true.into();
    }
    result
}

/// Translates an Anthropic Messages response into an OpenAI chat completion, returning the token
/// usage too so the caller can log it.
pub fn chat_completion(message: &Value) -> (Value, Usage) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in message["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => tool_calls.push(json!({
                "id": block["id"],
                "type": "function",
                "function": { "name": block["name"], "arguments": block["input"].to_string() },
            })),
            // Thinking, fallback markers and server-tool blocks have no chat-completions equivalent.
            _ => {}
        }
    }

    if is_empty_turn(message) {
        text = EMPTY_TURN_TEXT.into();
    }
    let mut reply = json!({ "role": "assistant", "content": text });
    if !tool_calls.is_empty() {
        if text.is_empty() {
            reply["content"] = Value::Null;
        }
        reply["tool_calls"] = tool_calls.into();
    }

    let mut usage = Usage::default();
    usage.update(&message["usage"]);
    let completion = json!({
        "id": message["id"],
        "object": "chat.completion",
        "created": now(),
        "model": message["model"],
        "choices": [{
            "index": 0,
            "message": reply,
            "finish_reason": finish_reason(message["stop_reason"].as_str()),
        }],
        "usage": usage.openai(),
    });
    (completion, usage)
}

/// Whether a Messages response ends the turn without text or tool calls, see [`EMPTY_TURN_TEXT`].
pub fn is_empty_turn(message: &Value) -> bool {
    let answered = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|block| match block["type"].as_str() {
            Some("text") => block["text"].as_str().is_some_and(|t| !t.is_empty()),
            Some("tool_use") => true,
            _ => false,
        });
    !answered && message["stop_reason"] == "end_turn"
}

/// Answer for a turn that ends without text or tool calls (the model had nothing to add after a
/// tool result). The Mendix chat resent the same request endlessly on an empty answer.
const EMPTY_TURN_TEXT: &str = "Done.";

/// Token usage of one Messages response.
#[derive(Clone, Debug, Default)]
pub struct Usage {
    pub(crate) input: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
    /// Split of `cache_write` by cache lifetime (`usage.cache_creation`).
    pub(crate) cache_write_5m: u64,
    pub(crate) cache_write_1h: u64,
    pub(crate) output: u64,
}

impl Usage {
    /// Takes the counts present in an Anthropic `usage` object (stream events carry some of them).
    fn update(&mut self, usage: &Value) {
        let creation = &usage["cache_creation"];
        for (count, value) in [
            (&mut self.input, &usage["input_tokens"]),
            (&mut self.cache_read, &usage["cache_read_input_tokens"]),
            (&mut self.cache_write, &usage["cache_creation_input_tokens"]),
            (
                &mut self.cache_write_5m,
                &creation["ephemeral_5m_input_tokens"],
            ),
            (
                &mut self.cache_write_1h,
                &creation["ephemeral_1h_input_tokens"],
            ),
            (&mut self.output, &usage["output_tokens"]),
        ] {
            if let Some(n) = value.as_u64() {
                *count = n;
            }
        }
    }

    /// All input tokens: uncached, read from the cache and written to it.
    fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Share of the input tokens read from the cache, in percent; `None` without input.
    fn hit_percent(&self) -> Option<u64> {
        let prompt = self.prompt();
        (prompt > 0).then(|| (self.cache_read * 100 + prompt / 2) / prompt)
    }

    /// OpenAI usage; prompt tokens include cache reads and writes, as OpenAI counts them.
    fn openai(&self) -> Value {
        let prompt = self.prompt();
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": self.output,
            "total_tokens": prompt + self.output,
            "prompt_tokens_details": {
                "cached_tokens": self.cache_read,
                "cache_write_tokens": self.cache_write,
            },
        })
    }

    /// Logged so the effect of prompt caching can be checked.
    pub fn log(&self, model: &str) {
        let hit = self
            .hit_percent()
            .map_or_else(|| "-".to_string(), |percent| format!("{percent}%"));
        log::info!(
            "Anthropic usage: model={model} input={} cache_read={} cache_write={} (5m={} 1h={}) output={} hit={hit}",
            self.input,
            self.cache_read,
            self.cache_write,
            self.cache_write_5m,
            self.cache_write_1h,
            self.output
        );
        if self.input > 0 && self.cache_read == 0 && self.cache_write == 0 {
            log::debug!(
                "Prompt not cached: shorter than the model's minimum cacheable length, or no cache breakpoint"
            );
        }
    }
}

fn finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The `(code, message)` of an Anthropic error body, for an Azure-style error response.
pub fn error_details(body: &[u8]) -> (String, String) {
    let error: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let code = error["error"]["type"]
        .as_str()
        .unwrap_or("UpstreamError")
        .to_string();
    let message = match error["error"]["message"].as_str() {
        Some(message) => message.to_string(),
        None => String::from_utf8_lossy(body).into_owned(),
    };
    (code, message)
}

/// The models as an OpenAI `/v1/models` list.
pub fn model_list(models: &[AnthropicModel]) -> Value {
    let data: Vec<Value> = models
        .iter()
        .map(|model| json!({ "id": model.id, "object": "model", "created": 0, "owned_by": "anthropic" }))
        .collect();
    json!({ "object": "list", "data": data })
}

/// Turns an Anthropic Messages event stream into OpenAI `chat.completion.chunk` events.
pub struct ChatStream {
    buffer: Vec<u8>,
    include_usage: bool,
    id: Value,
    model: Value,
    created: u64,
    /// Anthropic content block index → OpenAI tool call index.
    tool_calls: HashMap<u64, usize>,
    /// Whether any text or tool call was sent, see [`EMPTY_TURN_TEXT`].
    answered: bool,
    /// Whether the turn was answered with [`EMPTY_TURN_TEXT`].
    empty_turn: bool,
    /// Cache diagnostics from `message_start`.
    diagnostics: Value,
    usage: Usage,
}

impl ChatStream {
    pub fn new(include_usage: bool) -> Self {
        Self {
            buffer: Vec::new(),
            include_usage,
            id: Value::Null,
            model: Value::Null,
            created: now(),
            tool_calls: HashMap::new(),
            answered: false,
            empty_turn: false,
            diagnostics: Value::Null,
            usage: Usage::default(),
        }
    }

    /// The message id, once `message_start` has arrived.
    pub fn message_id(&self) -> Option<&str> {
        self.id.as_str()
    }

    /// Cache diagnostics from `message_start` (`--anthropic-cache-diagnostics`); `null` when
    /// nothing changed since the previous request, or when none were asked for.
    pub fn diagnostics(&self) -> &Value {
        &self.diagnostics
    }

    /// Token usage so far (complete after `message_stop`).
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Whether the turn was answered with [`EMPTY_TURN_TEXT`].
    pub fn empty_turn(&self) -> bool {
        self.empty_turn
    }

    /// Consumes a chunk of the upstream stream and returns the translated SSE bytes,
    /// keeping an incomplete trailing event for the next chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Bytes {
        self.buffer.extend_from_slice(chunk);
        let mut out = String::new();
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let data: String = String::from_utf8_lossy(&event)
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect();
            let Ok(data) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            for chunk in self.event(&data) {
                match chunk {
                    Value::Null => out.push_str("data: [DONE]\n\n"),
                    chunk => out.push_str(&format!("data: {chunk}\n\n")),
                }
            }
        }
        Bytes::from(out)
    }

    /// Translates one Anthropic event; `Value::Null` stands for the final `[DONE]`. The usage is
    /// logged here, at `message_stop`, because only the stream knows when it has ended.
    fn event(&mut self, event: &Value) -> Vec<Value> {
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let message = &event["message"];
                self.id = message["id"].clone();
                self.diagnostics = message["diagnostics"].clone();
                self.model = message["model"].clone();
                self.usage.update(&message["usage"]);
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            "content_block_start" => {
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        self.answered = true;
                        let index = self.tool_calls.len();
                        self.tool_calls
                            .insert(event["index"].as_u64().unwrap_or_default(), index);
                        let call = json!({
                            "index": index,
                            "id": block["id"],
                            "type": "function",
                            "function": { "name": block["name"], "arguments": "" },
                        });
                        vec![self.chunk(json!({ "tool_calls": [call] }), None)]
                    }
                    Some("text") if block["text"].as_str().is_some_and(|t| !t.is_empty()) => {
                        self.answered = true;
                        vec![self.chunk(json!({ "content": block["text"] }), None)]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        self.answered |= delta["text"].as_str().is_some_and(|t| !t.is_empty());
                        vec![self.chunk(json!({ "content": delta["text"] }), None)]
                    }
                    Some("input_json_delta") => {
                        let block = event["index"].as_u64().unwrap_or_default();
                        match self.tool_calls.get(&block) {
                            Some(&index) => {
                                let call = json!({ "index": index, "function": { "arguments": delta["partial_json"] } });
                                vec![self.chunk(json!({ "tool_calls": [call] }), None)]
                            }
                            None => Vec::new(),
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                self.usage.update(&event["usage"]);
                let stop_reason = event["delta"]["stop_reason"].as_str();
                let mut chunks = Vec::new();
                if !self.answered && stop_reason == Some("end_turn") {
                    self.empty_turn = true;
                    chunks.push(self.chunk(json!({ "content": EMPTY_TURN_TEXT }), None));
                }
                chunks.push(self.chunk(json!({}), Some(finish_reason(stop_reason))));
                chunks
            }
            "message_stop" => {
                self.usage.log(self.model.as_str().unwrap_or_default());
                let mut chunks = Vec::new();
                if self.include_usage {
                    let mut chunk = self.chunk(json!({}), None);
                    chunk["choices"] = json!([]);
                    chunk["usage"] = self.usage.openai();
                    chunks.push(chunk);
                }
                chunks.push(Value::Null);
                chunks
            }
            "error" => {
                let error = &event["error"];
                log::warn!("Anthropic stream error: {error}");
                vec![
                    json!({ "error": { "code": error["type"], "message": error["message"] } }),
                    Value::Null,
                ]
            }
            _ => Vec::new(),
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(fallbacks: bool, effort: Option<&str>) -> RequestOptions {
        RequestOptions {
            max_tokens: 16000,
            fallbacks,
            effort: effort.map(String::from),
        }
    }

    #[test]
    fn translates_conversation_with_tools() {
        let chat = json!({
            "model": "claude-opus-5-5",
            "temperature": 0.2,
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": "Weather in Paris and Rome?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Rome\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "Sunny"},
                {"role": "tool", "tool_call_id": "call_2", "content": "Rain"}
            ],
            "tools": [{"type": "function", "function": {"name": "weather", "parameters": {"type": "object"}}}],
            "tool_choice": "required"
        });
        let request = messages_request(&chat, &options(false, None)).unwrap();
        assert_eq!(request["max_tokens"], 16000);
        assert_eq!(
            request["system"],
            json!([{"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}])
        );
        assert!(request.get("temperature").is_none());
        assert!(request.get("fallbacks").is_none());
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][1]["input"], json!({"city": "Rome"}));
        // Both tool results go into one user message.
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(
            request["tools"][0]["input_schema"],
            json!({"type": "object"})
        );
        assert_eq!(request["tool_choice"], json!({"type": "any"}));
    }

    #[test]
    fn maps_options() {
        let chat = json!({
            "model": "claude-opus-5-5",
            "max_completion_tokens": 500,
            "stream": true,
            "stop": "END",
            "reasoning_effort": "high",
            "response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}}},
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        });
        let request = messages_request(&chat, &options(true, None)).unwrap();
        assert_eq!(request["max_tokens"], 500);
        assert_eq!(request["stream"], true);
        assert_eq!(request["stop_sequences"], json!(["END"]));
        assert_eq!(request["output_config"]["effort"], "high");
        assert_eq!(
            request["output_config"]["format"]["schema"],
            json!({"type": "object"})
        );
        assert_eq!(
            request["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(request["fallbacks"], "default");
    }

    #[test]
    fn maps_files_to_documents() {
        let chat = json!({"model": "claude-haiku-5-5", "messages": [{"role": "user", "content": [
            {"type": "file", "file": {"filename": "a.pdf", "file_data": "data:application/pdf;base64,JVBE"}},
            {"type": "file", "file": {"filename": "a.txt", "file_data": "data:text/plain;base64,aGVsbG8="}},
        ]}]});
        let request = messages_request(&chat, &options(true, None)).unwrap();
        let content = &request["messages"][0]["content"];
        assert_eq!(
            content[0],
            json!({"type": "document", "title": "a.pdf",
                "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBE"}})
        );
        assert_eq!(content[1]["source"]["type"], "text");
        assert_eq!(content[1]["source"]["data"], "hello");

        let chat = json!({"model": "claude-haiku-5-5", "messages": [{"role": "user", "content": [
            {"type": "file", "file": {"file_id": "file-1"}}
        ]}]});
        assert!(messages_request(&chat, &options(true, None)).is_err());
    }

    #[test]
    fn marks_cache_breakpoints() {
        let chat = json!({
            "model": "claude-opus-5-5",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [{"type": "text", "text": "Hi"}, {"type": "text", "text": ""}]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
            ],
            "tools": [
                {"type": "function", "function": {"name": "a"}},
                {"type": "function", "function": {"name": "b"}}
            ]
        });
        let request = messages_request(&chat, &options(false, Some("low"))).unwrap();
        let cached = json!({"type": "ephemeral"});
        assert!(request["tools"][0].get("cache_control").is_none());
        assert_eq!(request["tools"][1]["cache_control"], cached);
        assert_eq!(request["system"][0]["cache_control"], cached);
        // The empty text part is dropped, the previous user turn and the last block are marked.
        let messages = &request["messages"];
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(messages[0]["content"][0]["cache_control"], cached);
        assert!(messages[1]["content"][0].get("cache_control").is_none());
        assert_eq!(messages[2]["content"][0]["cache_control"], cached);
        assert_eq!(request.to_string().matches("cache_control").count(), 4);
        assert_eq!(request["output_config"]["effort"], "low");
    }

    #[test]
    fn serializes_deterministically() {
        // Same request, keys in a different order: a streamed tool call reaches the client in the
        // model's key order, a non-streamed one sorted. The prompt cache needs identical bytes.
        let chat = |schema: &str, arguments: &str| {
            json!({
                "model": "claude-opus-5-5",
                "messages": [
                    {"role": "user", "content": "Hi"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "a", "arguments": arguments}}
                    ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "a",
                    "parameters": serde_json::from_str::<Value>(schema).unwrap()
                }}]
            })
        };
        let first = chat(
            r#"{"type":"object","properties":{"b":{"type":"string"},"a":{"type":"number"}}}"#,
            r#"{"b":"x","a":1}"#,
        );
        let second = chat(
            r#"{"properties":{"a":{"type":"number"},"b":{"type":"string"}},"type":"object"}"#,
            r#"{"a":1,"b":"x"}"#,
        );
        let bytes = |chat: &Value| {
            messages_request(chat, &options(false, None))
                .unwrap()
                .to_string()
        };
        assert_eq!(bytes(&first), bytes(&second));
    }

    #[test]
    fn answers_empty_turn() {
        let message = json!({"id": "msg_1", "content": [], "stop_reason": "end_turn", "usage": {}});
        let (completion, _) = chat_completion(&message);
        assert_eq!(
            completion["choices"][0]["message"]["content"],
            EMPTY_TURN_TEXT
        );

        let mut stream = ChatStream::new(false);
        let out = stream.feed(
            br#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}

"#,
        );
        assert!(String::from_utf8_lossy(&out).contains(EMPTY_TURN_TEXT));
    }

    #[test]
    fn no_fallbacks_for_unsupported_models() {
        let chat =
            json!({"model": "claude-haiku-5-5", "messages": [{"role": "user", "content": "Hi"}]});
        assert!(
            messages_request(&chat, &options(true, None))
                .unwrap()
                .get("fallbacks")
                .is_none()
        );
    }

    #[test]
    fn translates_response() {
        let message = json!({
            "id": "msg_1",
            "model": "claude-opus-5-5",
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "toolu_1", "name": "weather", "input": {"city": "Paris"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 5,
                "cache_creation": {"ephemeral_5m_input_tokens": 2, "ephemeral_1h_input_tokens": 3},
                "output_tokens": 7}
        });
        let (completion, usage) = chat_completion(&message);
        assert_eq!((usage.input, usage.cache_read, usage.output), (10, 5, 7));
        assert_eq!(
            (
                usage.cache_write,
                usage.cache_write_5m,
                usage.cache_write_1h
            ),
            (5, 2, 3)
        );
        assert_eq!(usage.hit_percent(), Some(25));
        let choice = &completion["choices"][0];
        assert_eq!(choice["message"]["content"], "Checking.");
        assert_eq!(
            choice["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Paris\"}"
        );
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(completion["usage"]["prompt_tokens"], 20);
        assert_eq!(completion["usage"]["total_tokens"], 27);
        assert_eq!(
            completion["usage"]["prompt_tokens_details"],
            json!({"cached_tokens": 5, "cache_write_tokens": 5})
        );
    }

    #[test]
    fn translates_stream_split_across_chunks() {
        let events = [
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5-5","usage":{"input_tokens":3,"cache_read_input_tokens":6,"cache_creation_input_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hé"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"weather","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\""}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let sse: String = events
            .iter()
            .map(|e| format!("event: x\ndata: {e}\n\n"))
            .collect();
        let mut stream = ChatStream::new(true);
        // Split in the middle of a multi-byte character to check buffering.
        let split = sse.find('é').unwrap() + 1;
        let mut out = String::from_utf8(stream.feed(&sse.as_bytes()[..split]).to_vec()).unwrap();
        out.push_str(std::str::from_utf8(&stream.feed(&sse.as_bytes()[split..])).unwrap());

        let chunks: Vec<&str> = out.split("\n\n").filter(|c| !c.is_empty()).collect();
        assert_eq!(chunks.last(), Some(&"data: [DONE]"));
        let parsed: Vec<Value> = chunks[..chunks.len() - 1]
            .iter()
            .map(|c| serde_json::from_str(c.strip_prefix("data: ").unwrap()).unwrap())
            .collect();
        assert_eq!(parsed[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(parsed[1]["choices"][0]["delta"]["content"], "Hé");
        assert_eq!(
            parsed[2]["choices"][0]["delta"]["tool_calls"][0]["id"],
            "toolu_1"
        );
        assert_eq!(
            parsed[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\""
        );
        assert_eq!(parsed[4]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(parsed[5]["usage"]["total_tokens"], 13);
        assert_eq!(
            parsed[5]["usage"]["prompt_tokens_details"]["cached_tokens"],
            6
        );
    }

    #[test]
    fn reads_error_body() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
        assert_eq!(
            error_details(body),
            ("invalid_request_error".into(), "bad".into())
        );
    }
}
