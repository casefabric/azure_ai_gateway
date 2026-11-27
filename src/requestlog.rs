//! One JSON line per chat request (`logs/requests.jsonl`, see `log-config.yaml`), so caching, token
//! use and cost can be analysed afterwards. `summary` records hold counts, timings and hashes only;
//! `full` records also hold the request, the translated request and the answer (media replaced).

use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::anthropic::Usage;
use crate::savings::ToolsTidy;

/// log4rs target of the records; `log-config.yaml` sends it to its own file.
pub const TARGET: &str = "request_log";

/// How long a conversation is remembered for gaps and hit-rate drops.
const CONVERSATION_TTL: Duration = Duration::from_secs(3600);

/// Non-streamed bodies larger than this aren't parsed for usage.
const MAX_BUFFERED_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Off,
    Summary,
    Full,
}

/// One chat request as written to the request log.
#[derive(Debug, Default, Serialize)]
pub struct RequestRecord {
    pub ts_ms: u64,
    pub request_id: String,
    pub backend: &'static str,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
    /// Model the caller asked for (after the deployment from the path is applied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model that answered; differs from `model` after a refusal fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_answered: Option<String>,
    pub stream: bool,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// The response stream ended before the backend finished (client disconnect, broken upstream).
    #[serde(skip_serializing_if = "is_false")]
    pub incomplete: bool,
    pub duration_ms: u64,
    /// Time until the first response bytes arrived from the backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageRecord>,
    /// Estimate from [`PRICES`]; absent for unknown models and for Ollama.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,
    /// Time since the previous request of the same conversation started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_since_previous_ms: Option<u64>,
    /// First message that differs from the previous request of the conversation; absent when the
    /// conversation was only appended to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_message: Option<usize>,
    /// Refused by the loop guard without calling the backend.
    #[serde(skip_serializing_if = "is_false")]
    pub loop_guard: bool,
    /// The turn had neither text nor tool calls and was answered with `Done.`.
    #[serde(skip_serializing_if = "is_false")]
    pub empty_turn: bool,
    /// Id of the Anthropic message that answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Anthropic cache diagnostics (`--anthropic-cache-diagnostics`): `null` when nothing changed
    /// since the previous request, otherwise `{"cache_miss_reason": …}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Value>,
    /// OpenAI prompt-caching fields the caller sent (`prompt_cache_key`, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cache_hints: Vec<String>,
    /// `full` only: the request as received (after the deployment is applied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    /// `full` only: the request sent to the backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<Value>,
    /// `full` only: the assistant message returned to the caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Token counts. Anthropic reports uncached input and cache reads/writes separately; Ollama reports
/// `prompt` and, in recent versions, the tokens it reused from its KV cache as `cache_read`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageRecord {
    /// All input tokens.
    pub prompt: u64,
    pub output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u64>,
    /// `cache_read / prompt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_ratio: Option<f64>,
}

impl UsageRecord {
    pub fn from_anthropic(usage: &Usage) -> Self {
        let prompt = usage.input + usage.cache_read + usage.cache_write;
        Self {
            prompt,
            output: usage.output,
            input: Some(usage.input),
            cache_read: Some(usage.cache_read),
            cache_write: Some(usage.cache_write),
            cache_write_5m: Some(usage.cache_write_5m),
            cache_write_1h: Some(usage.cache_write_1h),
            hit_ratio: ratio(usage.cache_read, prompt),
        }
    }

    /// From an OpenAI `usage` object (Ollama's `/v1` API).
    pub fn from_openai(usage: &Value) -> Option<Self> {
        let prompt = usage["prompt_tokens"].as_u64()?;
        let cache_read = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
        Some(Self {
            prompt,
            output: usage["completion_tokens"].as_u64().unwrap_or_default(),
            cache_read,
            hit_ratio: cache_read.and_then(|read| ratio(read, prompt)),
            ..Self::default()
        })
    }
}

fn ratio(part: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (part as f64 / total as f64 * 1000.0).round() / 1000.0)
}

/// What the request looked like, without its content.
#[derive(Debug, Default, Serialize)]
pub struct PromptShape {
    pub messages: usize,
    pub content_parts: usize,
    pub images: usize,
    pub files: usize,
    pub tools_before: usize,
    pub tools_after: usize,
    pub tools_reordered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// `anthropic-beta` headers sent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub betas: Vec<String>,
    /// Where the cache breakpoints went, e.g. `tools[3]`, `messages[4][0]`, with `:1h` for a 1-hour TTL.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breakpoints: Vec<String>,
}

impl PromptShape {
    /// The shape of an OpenAI chat request, after `tidy_tools`.
    pub fn of_chat(chat: &Value, tools: &ToolsTidy) -> Self {
        let mut shape = Self {
            tools_before: tools.before,
            tools_after: tools.after,
            tools_reordered: tools.reordered,
            tool_choice: chat.get("tool_choice").cloned(),
            effort: chat["reasoning_effort"].as_str().map(String::from),
            max_tokens: chat["max_completion_tokens"]
                .as_u64()
                .or(chat["max_tokens"].as_u64()),
            ..Self::default()
        };
        for message in chat["messages"].as_array().into_iter().flatten() {
            shape.messages += 1;
            for part in message["content"].as_array().into_iter().flatten() {
                shape.content_parts += 1;
                match part["type"].as_str() {
                    Some("image_url") => shape.images += 1,
                    Some("file") => shape.files += 1,
                    _ => {}
                }
            }
        }
        shape
    }

    /// Adds what the translated Anthropic request actually uses.
    pub fn add_anthropic(&mut self, request: &Value, betas: &[&str]) {
        self.tool_choice = request.get("tool_choice").cloned();
        self.effort = request["output_config"]["effort"]
            .as_str()
            .map(String::from);
        self.max_tokens = request["max_tokens"].as_u64();
        self.betas = betas.iter().map(|beta| beta.to_string()).collect();
        self.breakpoints = breakpoints(request);
    }
}

fn breakpoints(request: &Value) -> Vec<String> {
    let label = |block: &Value, at: String| {
        let control = &block["cache_control"];
        if control.is_null() {
            return None;
        }
        Some(match control["ttl"].as_str() {
            Some(ttl) => format!("{at}:{ttl}"),
            None => at,
        })
    };
    let mut found = Vec::new();
    for section in ["tools", "system"] {
        for (i, block) in request[section]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            found.extend(label(block, format!("{section}[{i}]")));
        }
    }
    for (i, message) in request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (j, block) in message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            found.extend(label(block, format!("messages[{i}][{j}]")));
        }
    }
    found
}

/// Hashes of the parts of a chat request, so a cache miss can be traced to the first changed part
/// without storing content. Taken from the OpenAI request the caller sent (after `tidy_tools`), so
/// it is the same for both backends and not affected by moving cache breakpoints.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Fingerprint {
    /// Same for every request of one conversation: the model and the first non-system message.
    /// Tools and system prompt are left out, so a changed system prompt or tool list stays in the
    /// conversation and shows up as `first_changed_message` and in the cache diagnostics.
    pub conversation_key: String,
    pub tools: String,
    pub system: String,
    pub messages: Vec<String>,
}

impl Fingerprint {
    pub fn of_chat(chat: &Value) -> Self {
        let messages: Vec<&Value> = chat["messages"].as_array().into_iter().flatten().collect();
        let is_system = |m: &Value| matches!(m["role"].as_str(), Some("system" | "developer"));
        let system: String = messages
            .iter()
            .take_while(|m| is_system(m))
            .map(|m| m.to_string())
            .collect();
        let first = messages
            .iter()
            .find(|m| !is_system(m))
            .map(|m| m.to_string())
            .unwrap_or_default();
        let tools = hash(chat["tools"].to_string().as_bytes());
        let system = hash(system.as_bytes());
        let key = format!("{}|{first}", chat["model"]);
        Self {
            conversation_key: hash(key.as_bytes()),
            tools,
            system,
            messages: messages
                .iter()
                .map(|m| hash(m.to_string().as_bytes()))
                .collect(),
        }
    }
}

/// FNV-1a: stable across Rust versions and runs, so hashes can be compared between log files.
fn hash(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// OpenAI prompt-caching fields in a chat request.
pub fn cache_hints(chat: &Value) -> Vec<String> {
    let mut hints: Vec<String> = [
        "prompt_cache_key",
        "prompt_cache_retention",
        "prompt_cache_options",
    ]
    .into_iter()
    .filter(|field| chat.get(*field).is_some())
    .map(String::from)
    .collect();
    let markers = chat["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|part| part.get("prompt_cache_breakpoint").is_some())
        .count();
    if markers > 0 {
        hints.push(format!("prompt_cache_breakpoint×{markers}"));
    }
    hints
}

/// Replaces base64 media (data URLs, `{"type":"base64","data":…}` sources) by their type, size and
/// hash, so `full` records stay small and don't contain file contents.
pub fn redact_media(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Some((media_type, data)) = text
                .strip_prefix("data:")
                .and_then(|rest| rest.split_once(";base64,"))
            {
                *value = media_summary(media_type, data);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_media),
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("base64")
                && let Some(Value::String(data)) = map.get("data")
            {
                let media_type = map
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let summary = media_summary(media_type, data);
                map.insert("data".into(), summary);
            } else {
                map.values_mut().for_each(redact_media);
            }
        }
        _ => {}
    }
}

fn media_summary(media_type: &str, data: &str) -> Value {
    json!({"media_type": media_type, "bytes": data.len() * 3 / 4, "hash": hash(data.as_bytes())})
}

/// Anthropic prices in USD per million tokens (input, output) and the cache-read multiplier.
/// Cache writes cost 1.25× (5 minutes) or 2× (1 hour) the input price on every model.
/// Source: Anthropic's model table and prompt-caching page, 2026-10-24. Update when prices change.
pub const PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-fable-5-1", 10.0, 50.0, 0.025),
    ("claude-fable-5", 10.0, 50.0, 0.1),
    ("claude-opus-5-5", 4.0, 20.0, 0.05),
    ("claude-opus-5", 5.0, 25.0, 0.1),
    ("claude-sonnet-5-5", 2.0, 10.0, 0.05),
    ("claude-sonnet-5", 2.0, 10.0, 0.1),
    // Up to 100K prompt tokens; longer prompts cost $0.50 / $2.50, see `cost_usd`.
    ("claude-haiku-5-5", 0.10, 0.50, 0.1),
];

/// Estimated cost of an Anthropic request; `None` for unknown models or without cache counts.
pub fn cost_usd(model: &str, usage: &UsageRecord) -> Option<f64> {
    let &(_, mut input, mut output, read) = PRICES.iter().find(|(id, ..)| *id == model)?;
    if model == "claude-haiku-5-5" && usage.prompt > 100_000 {
        (input, output) = (0.50, 2.50);
    }
    let write = usage.cache_write?;
    let write_1h = usage.cache_write_1h.unwrap_or_default();
    // Without the split, writes are counted as 5-minute writes.
    let write_5m = write.saturating_sub(write_1h);
    let tokens = usage.input? as f64 * input
        + usage.cache_read? as f64 * input * read
        + write_5m as f64 * input * 1.25
        + write_1h as f64 * input * 2.0
        + usage.output as f64 * output;
    Some((tokens / 1_000_000.0 * 1e6).round() / 1e6)
}

struct Conversation {
    at: Instant,
    messages: Vec<String>,
    had_cache_read: bool,
    /// Id of the last Anthropic answer, for `diagnostics.previous_message_id`.
    last_message_id: Option<String>,
}

/// Writes request records and remembers recent conversations (by `conversation_key`) to report
/// gaps between requests, changed history and drops in the cache hit rate.
pub struct RequestLog {
    mode: Mode,
    boot: u64,
    next_id: AtomicU64,
    conversations: Mutex<HashMap<String, Conversation>>,
}

impl RequestLog {
    pub fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            boot: now_ms() / 1000,
            next_id: AtomicU64::new(1),
            conversations: Mutex::new(HashMap::new()),
        })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// A new record with timestamp and id.
    pub fn start(&self, backend: &'static str, path: &str) -> RequestRecord {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        RequestRecord {
            ts_ms: now_ms(),
            request_id: format!("{:x}-{id}", self.boot),
            backend,
            path: path.to_string(),
            ..RequestRecord::default()
        }
    }

    /// Compares the record's fingerprint with the previous request of the same conversation.
    pub fn observe(&self, record: &mut RequestRecord) {
        let Some(fingerprint) = &record.fingerprint else {
            return;
        };
        let now = Instant::now();
        let mut conversations = self.conversations.lock().expect("request log lock");
        conversations.retain(|_, c| now.duration_since(c.at) < CONVERSATION_TTL);
        let key = fingerprint.conversation_key.clone();
        if let Some(previous) = conversations.get_mut(&key) {
            record.gap_since_previous_ms = Some(millis(now.duration_since(previous.at)));
            record.first_changed_message = previous
                .messages
                .iter()
                .zip(&fingerprint.messages)
                .position(|(before, now)| before != now)
                .or_else(|| {
                    // A shorter history means messages were removed at the end.
                    (fingerprint.messages.len() < previous.messages.len())
                        .then_some(fingerprint.messages.len())
                });
            previous.at = now;
            previous.messages = fingerprint.messages.clone();
        } else {
            conversations.insert(
                key,
                Conversation {
                    at: now,
                    messages: fingerprint.messages.clone(),
                    had_cache_read: false,
                    last_message_id: None,
                },
            );
        }
    }

    /// The id of the previous answer in the conversation of `record` (after [`Self::observe`]).
    pub fn previous_message_id(&self, record: &RequestRecord) -> Option<String> {
        let key = &record.fingerprint.as_ref()?.conversation_key;
        let conversations = self.conversations.lock().expect("request log lock");
        conversations.get(key)?.last_message_id.clone()
    }

    /// Completes the record (duration, cost), warns when a conversation that was read from the cache
    /// no longer is, logs cache diagnostics, and writes it.
    pub fn finish(&self, mut record: RequestRecord, started: Instant) {
        record.duration_ms = millis(started.elapsed());
        if let Some(usage) = &record.usage {
            let model = record.model_answered.as_ref().or(record.model.as_ref());
            record.cost_usd = model.and_then(|model| cost_usd(model, usage));
            if let (Some(read), Some(fingerprint)) = (usage.cache_read, &record.fingerprint) {
                self.track_cache_read(&record, &fingerprint.conversation_key, read);
            }
        }
        if let (Some(id), Some(fingerprint)) = (&record.message_id, &record.fingerprint) {
            let mut conversations = self.conversations.lock().expect("request log lock");
            if let Some(conversation) = conversations.get_mut(&fingerprint.conversation_key) {
                conversation.last_message_id = Some(id.clone());
            }
        }
        if let Some(reason) = record
            .diagnostics
            .as_ref()
            .map(|d| &d["cache_miss_reason"])
            .filter(|reason| reason.is_object())
        {
            log::info!(
                "Cache diagnostics ({}): {} (about {} input tokens missed), first changed message: {}",
                record.request_id,
                reason["type"].as_str().unwrap_or("?"),
                reason["cache_missed_input_tokens"],
                record
                    .first_changed_message
                    .map_or_else(|| "none".to_string(), |i| i.to_string())
            );
        }
        if self.mode == Mode::Off {
            return;
        }
        match serde_json::to_string(&record) {
            Ok(line) => log::info!(target: TARGET, "{line}"),
            Err(e) => log::warn!("Could not serialize request record: {e}"),
        }
    }

    fn track_cache_read(&self, record: &RequestRecord, key: &str, read: u64) {
        let mut conversations = self.conversations.lock().expect("request log lock");
        let Some(conversation) = conversations.get_mut(key) else {
            return;
        };
        if read == 0 && conversation.had_cache_read && !record.incomplete && record.status < 400 {
            log::warn!(
                "Cache miss in a cached conversation ({}): {} ms since the previous request, first changed message: {}",
                record.request_id,
                record.gap_since_previous_ms.unwrap_or_default(),
                record
                    .first_changed_message
                    .map_or_else(|| "none".to_string(), |i| i.to_string())
            );
        }
        conversation.had_cache_read = read > 0;
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis)
        .unwrap_or_default()
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Watches an OpenAI-format response on its way to the caller (SSE chunks or one JSON body) and
/// writes the request record when dropped: at the end of the response, or when the caller
/// disconnects (then marked `incomplete`). The bytes themselves are not changed.
pub struct ResponseTap {
    log: Arc<RequestLog>,
    record: Option<RequestRecord>,
    started: Instant,
    sse: bool,
    buffer: Vec<u8>,
    done: bool,
    capture: bool,
    text: String,
    tool_calls: Vec<Value>,
}

impl ResponseTap {
    pub fn new(log: Arc<RequestLog>, record: RequestRecord, started: Instant, sse: bool) -> Self {
        Self {
            capture: log.mode == Mode::Full,
            log,
            record: Some(record),
            started,
            sse,
            buffer: Vec::new(),
            done: false,
            text: String::new(),
            tool_calls: Vec::new(),
        }
    }

    fn record(&mut self) -> &mut RequestRecord {
        self.record.as_mut().expect("record is taken only on drop")
    }

    /// Sees the next chunk of the response.
    pub fn observe(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let started = self.started;
        let record = self.record();
        if record.ttft_ms.is_none() {
            record.ttft_ms = Some(millis(started.elapsed()));
        }
        if !self.sse {
            if self.buffer.len() + bytes.len() <= MAX_BUFFERED_BODY {
                self.buffer.extend_from_slice(bytes);
            }
            return;
        }
        self.buffer.extend_from_slice(bytes);
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.buffer.drain(..end + 2).collect();
            for line in String::from_utf8_lossy(&event).lines() {
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data == "[DONE]" {
                    self.done = true;
                } else if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                    self.chunk(&chunk);
                }
            }
        }
    }

    /// The Anthropic message id and cache diagnostics (from `message_start` when streaming).
    pub fn set_message(&mut self, id: Option<&str>, diagnostics: Option<&Value>) {
        let record = self.record();
        if record.message_id.is_none() {
            record.message_id = id.map(String::from);
        }
        if record.diagnostics.is_none() {
            record.diagnostics = diagnostics.cloned();
        }
    }

    /// Anthropic counts (with the cache split), which replace the OpenAI usage in the stream.
    pub fn set_anthropic(&mut self, usage: &Usage, empty_turn: bool) {
        let record = self.record();
        record.usage = Some(UsageRecord::from_anthropic(usage));
        record.empty_turn = empty_turn;
    }

    fn chunk(&mut self, chunk: &Value) {
        let capture = self.capture;
        let record = self.record();
        if let Some(model) = chunk["model"].as_str() {
            record.model_answered = Some(model.to_string());
        }
        if let Some(reason) = chunk["choices"][0]["finish_reason"].as_str() {
            record.finish_reason = Some(reason.to_string());
        }
        if let Some(code) = chunk["error"]["code"].as_str() {
            record.error_code = Some(code.to_string());
        }
        if record.backend != "Anthropic"
            && let Some(usage) = UsageRecord::from_openai(&chunk["usage"])
        {
            record.usage = Some(usage);
        }
        if !capture {
            return;
        }
        let delta = &chunk["choices"][0]["delta"];
        if let Some(text) = delta["content"].as_str() {
            self.text.push_str(text);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or_default() as usize;
            if self.tool_calls.len() <= index {
                self.tool_calls
                    .resize(index + 1, json!({"function": {"arguments": ""}}));
            }
            let entry = &mut self.tool_calls[index];
            for key in ["id", "type"] {
                if !call[key].is_null() {
                    entry[key] = call[key].clone();
                }
            }
            if let Some(name) = call["function"]["name"].as_str() {
                entry["function"]["name"] = name.into();
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                let joined = format!(
                    "{}{arguments}",
                    entry["function"]["arguments"].as_str().unwrap_or_default()
                );
                entry["function"]["arguments"] = joined.into();
            }
        }
    }

    /// For a non-streamed body: model, finish reason, usage and the answer.
    fn body(&mut self) {
        let Ok(body) = serde_json::from_slice::<Value>(&self.buffer) else {
            return;
        };
        self.done = true;
        let capture = self.capture;
        let record = self.record();
        record.model_answered = body["model"].as_str().map(String::from);
        record.finish_reason = body["choices"][0]["finish_reason"]
            .as_str()
            .map(String::from);
        record.error_code = body["error"]["code"].as_str().map(String::from);
        record.usage = UsageRecord::from_openai(&body["usage"]);
        if capture {
            record.response = Some(body["choices"][0]["message"].clone());
        }
    }
}

impl Drop for ResponseTap {
    fn drop(&mut self) {
        if !self.sse {
            self.body();
        } else if self.capture {
            let mut message = json!({"role": "assistant", "content": self.text});
            if !self.tool_calls.is_empty() {
                message["tool_calls"] = std::mem::take(&mut self.tool_calls).into();
            }
            self.record().response = Some(message);
        }
        if let Some(mut record) = self.record.take() {
            record.incomplete = !self.done;
            self.log.finish(record, self.started);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "tools": [{"type": "function", "function": {"name": "a"}}],
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "text", "text": "What is this?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAAAAAA"}}
                ]}
            ]
        })
    }

    #[test]
    fn conversation_key_survives_appended_messages() {
        let first = Fingerprint::of_chat(&chat());
        let mut longer = chat();
        let messages = longer["messages"].as_array_mut().unwrap();
        messages.push(json!({"role": "assistant", "content": "A square."}));
        messages.push(json!({"role": "user", "content": "Sure?"}));
        let second = Fingerprint::of_chat(&longer);
        assert_eq!(first.conversation_key, second.conversation_key);
        assert_eq!(second.messages[..2], first.messages[..]);

        let mut other = chat();
        other["model"] = "claude-sonnet-5-5".into();
        assert_ne!(
            Fingerprint::of_chat(&other).conversation_key,
            first.conversation_key
        );
        // A changed system prompt stays in the conversation, with its own hash.
        let mut edited = chat();
        edited["messages"][0]["content"] = "Be very brief.".into();
        let edited = Fingerprint::of_chat(&edited);
        assert_eq!(edited.conversation_key, first.conversation_key);
        assert_ne!(edited.system, first.system);
    }

    #[test]
    fn observes_gaps_and_changed_history() {
        let log = RequestLog::new(Mode::Off);
        let mut first = log.start("Anthropic", "/chat/completions");
        first.fingerprint = Some(Fingerprint::of_chat(&chat()));
        log.observe(&mut first);
        assert_eq!(first.gap_since_previous_ms, None);

        let mut longer = chat();
        longer["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "A square."}));
        let mut second = log.start("Anthropic", "/chat/completions");
        second.fingerprint = Some(Fingerprint::of_chat(&longer));
        log.observe(&mut second);
        assert!(second.gap_since_previous_ms.is_some());
        assert_eq!(second.first_changed_message, None);
        assert_ne!(first.request_id, second.request_id);

        // The client rewrote the assistant message.
        longer["messages"][2]["content"] = "A circle.".into();
        let mut third = log.start("Anthropic", "/chat/completions");
        third.fingerprint = Some(Fingerprint::of_chat(&longer));
        log.observe(&mut third);
        assert_eq!(third.first_changed_message, Some(2));

        // The answer's id is offered as `previous_message_id` to the next request.
        assert_eq!(log.previous_message_id(&third), None);
        third.message_id = Some("msg_3".into());
        log.finish(third, Instant::now());
        let mut fourth = log.start("Anthropic", "/chat/completions");
        fourth.fingerprint = Some(Fingerprint::of_chat(&longer));
        log.observe(&mut fourth);
        assert_eq!(log.previous_message_id(&fourth).as_deref(), Some("msg_3"));
    }

    #[test]
    fn describes_prompt_and_breakpoints() {
        let tidy = ToolsTidy {
            before: 2,
            after: 1,
            reordered: false,
        };
        let mut shape = PromptShape::of_chat(&chat(), &tidy);
        assert_eq!(
            (shape.messages, shape.content_parts, shape.images),
            (2, 2, 1)
        );
        let request = json!({
            "max_tokens": 100,
            "output_config": {"effort": "low"},
            "tools": [{"name": "a", "cache_control": {"type": "ephemeral", "ttl": "1h"}}],
            "system": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}
            ]}]
        });
        shape.add_anthropic(&request, &["beta-1"]);
        assert_eq!(
            shape.breakpoints,
            ["tools[0]:1h", "system[0]", "messages[0][1]"]
        );
        assert_eq!(shape.effort.as_deref(), Some("low"));
        assert_eq!(shape.betas, ["beta-1"]);
    }

    #[test]
    fn redacts_media() {
        let mut value = chat();
        value["sent"] =
            json!({"type": "base64", "media_type": "application/pdf", "data": "JVBERi0x"});
        redact_media(&mut value);
        let url = &value["messages"][1]["content"][1]["image_url"]["url"];
        assert_eq!(url["media_type"], "image/png");
        assert_eq!(url["bytes"], 6);
        assert_eq!(value["sent"]["data"]["media_type"], "application/pdf");
        assert!(!value.to_string().contains("JVBERi0x"));
    }

    #[test]
    fn finds_cache_hints() {
        let mut value = chat();
        value["prompt_cache_key"] = "user-1".into();
        value["messages"][1]["content"][0]["prompt_cache_breakpoint"] = json!({});
        assert_eq!(
            cache_hints(&value),
            ["prompt_cache_key", "prompt_cache_breakpoint×1"]
        );
    }

    #[test]
    fn estimates_cost() {
        let usage = UsageRecord {
            prompt: 2_000_000,
            output: 1_000_000,
            input: Some(1_000_000),
            cache_read: Some(500_000),
            cache_write: Some(500_000),
            cache_write_5m: Some(400_000),
            cache_write_1h: Some(100_000),
            hit_ratio: None,
        };
        // Opus 5.5: 4 + 0.5×0.2 + 0.4×5 + 0.1×8 + 20
        assert_eq!(cost_usd("claude-opus-5-5", &usage), Some(26.9));
        assert_eq!(cost_usd("llama3.1:8b", &usage), None);
        let ollama =
            UsageRecord::from_openai(&json!({"prompt_tokens": 10, "completion_tokens": 2}));
        assert_eq!(cost_usd("claude-opus-5-5", &ollama.unwrap()), None);
    }

    #[test]
    fn reads_usage_from_an_sse_tail() {
        let log = RequestLog::new(Mode::Full);
        let mut record = log.start("Ollama", "/chat/completions");
        record.stream = true;
        let mut tap = ResponseTap::new(log.clone(), record, Instant::now(), true);
        tap.observe(br#"data: {"model":"llama3.1:8b","choices":[{"delta":{"content":"Hi"}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"f","arguments":"{\"a\""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3}}

data: [DONE]

"#);
        let record = tap.record();
        assert_eq!(record.model_answered.as_deref(), Some("llama3.1:8b"));
        assert_eq!(record.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(record.usage.as_ref().map(|u| u.prompt), Some(12));
        assert_eq!(tap.text, "Hi");
        assert_eq!(tap.tool_calls[0]["function"]["arguments"], "{\"a\":1}");
        assert!(tap.done);
    }

    #[test]
    fn summary_records_hold_no_content() {
        let mut record = RequestRecord {
            prompt: Some(PromptShape::of_chat(&chat(), &ToolsTidy::default())),
            fingerprint: Some(Fingerprint::of_chat(&chat())),
            ..RequestRecord::default()
        };
        record.usage = UsageRecord::from_openai(&json!({"prompt_tokens": 1}));
        let line = serde_json::to_string(&record).unwrap();
        assert!(!line.contains("Be brief"));
        assert!(!line.contains("What is this"));
        assert!(!line.contains("\"request\""));
    }
}
