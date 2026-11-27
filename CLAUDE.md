# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build            # Build
cargo run              # Run (reads log-config.yaml from the working directory)
cargo test             # Run tests
cargo test route::     # Run one module's tests (or a single test by name, e.g. `cargo test query_filters`)
cargo clippy           # Lint
cargo fmt              # Format
```

Tests are inline `#[cfg(test)]` unit tests in `route.rs`, `foundry.rs`, `ollama.rs`, `anthropic.rs`, `savings.rs` and `requestlog.rs`; none need a running
Ollama or an Anthropic API key.

## Purpose

`azure_ai_gateway` makes a local Ollama server look like an Azure AI Foundry project endpoint, so the
Mendix Agent Editor (which only supports Mendix Cloud GenAI and Azure AI Foundry) can "List deployments"
and run chat completions against Ollama models. With `--backend anthropic` it fronts the Anthropic Messages API
instead, so the Agent Editor can use Claude models.

### Background: what Mendix is assumed to call

Mendix doesn't document which calls the Agent Editor makes. The shapes below come from the Foundry
project Deployments API as the official Azure AI Projects SDK builds it:

- `GET {projectEndpoint}/deployments?api-version=v1`, and `…/deployments/{name}` for a single deployment.
  `{projectEndpoint}` looks like `https://<resource>.services.ai.azure.com/api/projects/<project>`.
  Optional filters are `modelPublisher`, `modelName` and `deploymentType`.
- Response: `{"value": [ModelDeployment…], "nextLink": null}`, each deployment having `type`, `name`,
  `modelName`, `modelVersion`, `modelPublisher`, `capabilities` (string→string map), `sku`, `connectionName`.
  **The Agent Editor only offers deployments with `"chat_completion": "true"`.**
- Auth: the SDK sends an Entra `Authorization: Bearer`, but Mendix's API-key field probably sends
  `api-key`. That's why the proxy accepts both.
- The runtime chat path is a guess: either `…/openai/deployments/{name}/chat/completions?api-version=…`
  or the `/models/chat/completions` inference route. That's why both are routed and unknown paths are
  logged; check the log for what Mendix actually sends before adding routes.

If Studio Pro runs in a Windows VM on a Mac, `localhost` inside the VM is the VM itself. The proxy binds
`0.0.0.0`, so point the Agent Editor at `http://<mac-ip>:8135/api/projects/<any>`. Ollama can stay on
localhost as long as the proxy runs on the same Mac.

## Architecture

- [src/main.rs](src/main.rs) — CLI/env configuration (`clap`), log4rs setup, actix-web server whose `default_service` is the single handler
  (`--max-body-mb` is applied via `web::PayloadConfig`). reqwest runs on actix's Tokio runtime.
  reqwest and actix-web use different `http` crate versions, so status codes and headers are converted by value.
- [src/route.rs](src/route.rs) — maps method + path **suffix** to a `Route`, so any project prefix
  (`/api/projects/<project>/…`) works.
- [src/proxy.rs](src/proxy.rs) — request handling: logs every request (headers/body at debug, keys redacted),
  optional API-key check, deployment listing, and streaming forwarding of chat/embeddings to Ollama's `/v1` API.
- [src/foundry.rs](src/foundry.rs) — Foundry `ModelDeployment` response shape and Azure-style error bodies.
- [src/ollama.rs](src/ollama.rs) — Ollama client: local `/api/tags`, the ollama.com cloud catalog, and `/api/show` (capabilities).
- [src/files.rs](src/files.rs) — `file` content parts (`{"type":"file","file":{"filename","file_data":"data:…;base64,…"}}`,
  how the Agent Editor's AI SDK client sends PDFs): data-URL parsing, base64 decoding, PDF text extraction (`pdf-extract`).
- [src/anthropic.rs](src/anthropic.rs) — Anthropic client (raw HTTP: Rust has no official Anthropic SDK) and the translation
  between OpenAI chat completions and the Messages API, both ways, including SSE streams (`ChatStream`).
  `proxy::Backend` picks Ollama or Anthropic; `AppState` holds one of them.
- [src/savings.rs](src/savings.rs) — token savings for both backends: `tidy_tools` (`--tools-deny` filter, tools sorted by name)
  and `LoopGuard` (refuses identical chat requests repeated too often).
- [src/requestlog.rs](src/requestlog.rs) — the request log (`--request-log`): one `RequestRecord` per chat request, conversation
  tracking, the price table (`PRICES`) and `ResponseTap`, which reads usage from the response on its way to the caller.

| Incoming | Forwarded to Ollama |
|---|---|
| `GET …/deployments` | `/api/tags` + `https://ollama.com/api/tags` + `/api/show` → Foundry deployment list |
| `GET …/deployments/{name}` | same, single deployment |
| `POST …/deployments/{name}/chat/completions` | `/v1/chat/completions` with `model = {name}` |
| `POST …/chat/completions` | `/v1/chat/completions` (model from body) |
| `POST …/deployments/{name}/embeddings`, `POST …/embeddings` | `/v1/embeddings` |
| `GET …/models` | `/v1/models` |
| anything else | logged, 404 with Azure-style error |

With `--backend anthropic`:

| Incoming | Sent to Anthropic |
|---|---|
| `GET …/deployments`, `GET …/deployments/{name}` | `/v1/models` (paginated) → one deployment per Claude model |
| `POST …/chat/completions` (both forms) | `/v1/messages`, request and response translated |
| `GET …/models` | `/v1/models`, returned as an OpenAI model list |
| embeddings | 400 `OperationNotSupported` (Anthropic has no embeddings) |

Behaviors worth knowing before changing things:

- **Adding a route** means a new `Route` variant, a suffix rule in `route::parse`, and an arm in `proxy::handle`.
  POST rules are checked before GET rules, and matching is on percent-decoded path segments.
- **Deployment name = full Ollama model name including the tag** (`llama3.1:8b`); `modelName`/`modelVersion`
  are the parts split on `:` (`latest` when there's no tag). Ollama capabilities map to Foundry ones
  (`completion`→`chat_completion`, `embedding`→`embeddings`, `tools`→`tool_calling`, `vision`); if Ollama
  doesn't report capabilities, the model is assumed to be chat-capable.
- **Cloud models are listed too**, the same way the Ollama desktop app does it: the app merges local
  `/api/tags` with the ollama.com catalog (`https://ollama.com/api/tags`, public), appending `:cloud`
  to each name (`glm-5.3:cloud`, `gpt-oss:120b:cloud`) and dropping ones that are also available locally.
  The local Ollama runs `…:cloud` models remotely, so they need no special forwarding, but chats only work
  when that Ollama is signed in to ollama.com. If the catalog can't be fetched, a warning is logged and only
  local models are listed.
- **Deployment listing is not cached**: every list/get call hits `/api/tags`, the cloud catalog, and one `/api/show` per model
  (in parallel). A failed `/api/show` is logged and treated as "no capabilities reported".
- **Forwarding is a byte-for-byte stream**: the response body and `Content-Type` are passed through unchanged
  (so SSE for `stream: true` works); only the `model` field is rewritten when the deployment comes from the path.
  Ollama connection failures become `502` with an Azure-style error.
- **File parts**: neither backend accepts OpenAI `file` parts as-is. For Ollama, `ollama::inline_files` turns PDFs
  and text files into text parts (`<file name="…">…</file>`, PDF text extracted on a blocking thread) and images into
  `image_url`; the request body is only re-serialized when something changed or the deployment came from the path.
  For Anthropic, PDFs become base64 `document` blocks and text files text `document` blocks. `file_id` references,
  other media types and `input_audio` (Anthropic) are rejected with a 400.
- **Anthropic translation** (`anthropic::messages_request` / `chat_completion` / `ChatStream`): `system`/`developer`
  messages become `system`; assistant `tool_calls` become `tool_use` blocks; `tool` messages become `tool_result`
  blocks, and consecutive same-role messages are merged (Anthropic needs alternating roles). Images (`image_url`,
  data URLs or http URLs), `tools`, `tool_choice` (`required`→`any`), `parallel_tool_calls`, `stop`,
  `reasoning_effort`→`output_config.effort` and `response_format` `json_schema`→`output_config.format` are mapped.
  `temperature`/`top_p` are dropped because current Claude models reject them. `max_tokens` defaults to
  `--anthropic-max-tokens`. Thinking, `fallback` and other non-text blocks are left out of the reply.
  Deployments are named by the model ID (`claude-opus-5-5`), version `latest`, publisher `Anthropic`, with
  `chat_completion` + `tool_calling`, and `vision` when the Models API reports `image_input`.
- **Refusal fallbacks**: for `claude-fable-5-1`, `claude-opus-5-5`, `claude-opus-5` and `claude-sonnet-5-5` the
  proxy adds `fallbacks: "default"` (beta `server-side-fallback-2026-07-01`), so a safety-classifier decline is
  re-run on another model server-side. A remaining refusal comes back as `finish_reason: "content_filter"`.
  `--anthropic-no-fallbacks` turns this off.
- **Token savings** (logic ported from `mx-demo-runbook/tools/claude-proxy.mjs`): clients resend all tools, the system prompt
  and the whole conversation on every step of a tool loop, so the prefix must stay byte-identical to be cached.
  Tools are sorted by name on every chat request (the Mendix GenAI connector reorders them per call), which lets
  Ollama reuse its KV cache and Anthropic its prompt cache; `--tools-deny` drops tools by name. For Anthropic,
  `cache_control` breakpoints go on the last tool, the last system block, the previous user turn and the last block
  (the maximum of 4). The log line `Anthropic usage: model=… input=… cache_read=… cache_write=… (5m=… 1h=…) output=… hit=…%`
  shows whether caching works (a debug line follows when nothing was cached), and `prompt_tokens_details.cached_tokens`
  and `cache_write_tokens` are reported back. `PROMPT_CACHING_PLAN.md` holds the planned caching improvements.
  A turn ending with neither text nor tool calls is answered with `Done.`, because the Mendix chat resends a
  request after an empty answer; the loop guard (`--loop-max`) answers the 4th identical chat body within a minute
  with a 429 without calling the backend.
- Anthropic errors keep their status and become Azure-style errors with the Anthropic error type as `code`.
- The unknown-route 404 plus request logging exists so that new paths the Mendix client calls show up in
  the log. Headers/bodies are logged only at debug level, which `log-config.yaml` enables for this crate;
  bodies are truncated at 4000 chars. Logs go to stdout and `logs/azure_ai_gateway.log` (rolling).
  A missing log config only prints a warning, and the proxy then runs without logging.
- **Request log** (`--request-log`, default `summary`): one JSON line per chat request in `logs/requests.jsonl`, through the
  log4rs target `request_log` (configured in `log-config.yaml`; without that logger the lines end up in the main log).
  A record holds the model asked for and the model that answered, status, `finish_reason`, `duration_ms`/`ttft_ms`, usage
  (with cache reads/writes for Anthropic, and the KV-cache reuse Ollama reports as `cached_tokens`), an estimated `cost_usd` (from `requestlog::PRICES`, Anthropic only), the prompt
  shape (counts, tools before/after `--tools-deny`, effort, cache breakpoints, betas) and FNV-1a hashes of tools, system
  prompt and each message. The hashes are taken from the OpenAI request after `tidy_tools`, so they are the same for both
  backends. `conversation_key` (model and first non-system message, so a changed system prompt or tool list stays in the
  conversation) links the requests of one conversation; the log remembers conversations for an hour to fill
  `gap_since_previous_ms` and `first_changed_message`, and warns in the main log when a conversation that was read from the
  cache gets `cache_read=0`. With `--anthropic-cache-diagnostics`, each Anthropic request sends
  `diagnostics.previous_message_id` (the last answer of its conversation, or `null`), and the API's verdict goes into the
  record's `diagnostics` (`null` = nothing changed) and, for a `cache_miss_reason`, into the main log
  (`Cache diagnostics (…): system_changed (about N input tokens missed), first changed message: 0`). No beta header needed. Streams are recorded when they end, or as
  `incomplete` when the proxy notices that the caller went away. Loop-guard refusals get a short record. `full` adds the
  request, the request sent and the answer, with base64 media replaced by type, size and hash. Analysis examples:

  ```bash
  # Hit rate and cost per conversation
  jq -s -c 'map(select(.usage)) | group_by(.fingerprint.conversation_key) | map({conversation: .[0].fingerprint.conversation_key, requests: length, hit_rate: ((map(.usage.cache_read // 0) | add) / (map(.usage.prompt) | add)), cost_usd: (map(.cost_usd // 0) | add)})' logs/requests.jsonl
  # Requests and spend per day
  jq -s -c 'group_by(.ts_ms / 86400000 | floor) | map({day: (.[0].ts_ms / 1000 | strftime("%Y-%m-%d")), requests: length, cost_usd: (map(.cost_usd // 0) | add)})' logs/requests.jsonl
  # Gaps between requests of a conversation, compared with the 5-minute and 1-hour cache lifetimes
  jq -s -c 'map(.gap_since_previous_ms | select(. != null)) | {under_5m: map(select(. < 300000)) | length, from_5m_to_1h: map(select(. >= 300000 and . < 3600000)) | length, over_1h: map(select(. >= 3600000)) | length}' logs/requests.jsonl
  # Write payback: tokens written to and read from the cache per system prompt
  jq -s -c 'map(select(.usage.cache_write != null)) | group_by(.fingerprint.system) | map({system: .[0].fingerprint.system, written: (map(.usage.cache_write) | add), read: (map(.usage.cache_read) | add)})' logs/requests.jsonl
  # Requests with the largest cache writes, with the first changed message
  jq -r 'select(.usage.cache_write != null) | [.usage.cache_write, .request_id, (.first_changed_message // "-")] | @tsv' logs/requests.jsonl | sort -rn | head
  ```

## Configuration

| Flag | Env | Default |
|---|---|---|
| `--port` | `AZURE_AI_GATEWAY_PORT` | `8135` |
| `--backend` | `AZURE_AI_GATEWAY_BACKEND` | `ollama` (or `anthropic`) |
| `--ollama-url` | `OLLAMA_URL` | `http://localhost:11434` |
| `--ollama-api-key` | `OLLAMA_API_KEY` | none (sent as Bearer when set) |
| `--ollama-cloud-url` | `OLLAMA_CLOUD_URL` | `https://ollama.com` (empty = local models only) |
| `--anthropic-url` | `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` |
| `--anthropic-api-key` | `ANTHROPIC_API_KEY` | none (required for `--backend anthropic`) |
| `--anthropic-max-tokens` | `ANTHROPIC_MAX_TOKENS` | `16000` (when the caller sends no `max_tokens`) |
| `--anthropic-no-fallbacks` | `ANTHROPIC_NO_FALLBACKS` | off |
| `--anthropic-effort` | `ANTHROPIC_EFFORT` | none (`low`…`max`, when the caller sends no `reasoning_effort`) |
| `--tools-deny` | `AZURE_AI_GATEWAY_TOOLS_DENY` | none (regex of tool names to drop, either backend) |
| `--loop-max` | `AZURE_AI_GATEWAY_LOOP_MAX` | `3` (identical chat requests per minute; `0` = off) |
| `--request-log` | `AZURE_AI_GATEWAY_REQUEST_LOG` | `summary` (`off`, `summary`, `full`: adds prompts and answers) |
| `--anthropic-cache-diagnostics` | `ANTHROPIC_CACHE_DIAGNOSTICS` | off (asks Anthropic where consecutive prompts diverge) |
| `--api-key` | `AZURE_AI_GATEWAY_API_KEY` | none (any caller accepted) |
| `--timeout` | `OLLAMA_TIMEOUT` | `600` seconds (applies to either backend) |
| `--max-body-mb` | `AZURE_AI_GATEWAY_MAX_BODY_MB` | `32` |
| `--log-config` | `AZURE_AI_GATEWAY_LOG_CONFIG` | `log-config.yaml` |
