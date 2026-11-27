# Prompt caching: comparison with the Claude guidelines and improvement plan

Reference: <https://platform.claude.com/docs/en/build-with-claude/prompt-caching> and
<https://platform.claude.com/docs/en/build-with-claude/cache-diagnostics> (read 2026-10-24).

## Status and how to resume (2026-10-24)

**Done:** phases 0–3 (see [Implementation order](#implementation-order)). The proxy logs complete
token usage, writes one JSON line per chat request to `logs/requests.jsonl`, and can ask Anthropic for
cache diagnostics. 36 tests pass, clippy is clean, and it has been tested live against the Anthropic API
(`claude-haiku-5-5`, `claude-opus-5-5`) and a local Ollama (`gpt-oss:20b`).

**Next: phase 4, measure.** No code. Run the Agent Editor through the proxy for at least a day of
normal use, then analyze the request log and decide which of phases 5–7 to build.

### Run the proxy for phase 4

From the project directory, with `ANTHROPIC_API_KEY` exported in the shell:

```bash
cd /Users/olger/Development/dlectron/ms_foundry_llm_proxy
cargo build --release
./target/release/azure_ai_gateway \
  --backend anthropic \
  --anthropic-cache-diagnostics \
  --request-log summary \
  --port 8135 \
  --log-config log-config.yaml
```

- Point the Agent Editor at `http://<mac-ip>:8135/api/projects/<any>` (from a Windows VM, use the Mac's
  IP, not `localhost`).
- The same settings as environment variables: `AZURE_AI_GATEWAY_BACKEND=anthropic`,
  `ANTHROPIC_CACHE_DIAGNOSTICS=true`, `AZURE_AI_GATEWAY_REQUEST_LOG=summary`.
- `--request-log summary` is the default; it's spelled out above so the mode is visible. For a short
  session where the actual prompts matter (for example to see *what* changed in a `messages_changed`
  miss), use `--request-log full`. It writes prompts and answers to disk, with media replaced by type,
  size and hash.
- The run directory needs `log-config.yaml`, because it routes the records to `logs/requests.jsonl`. The
  main log stays in `logs/azure_ai_gateway.log`.
- Quick check before a long session: `python3 tools/live_cache_test.py
  http://127.0.0.1:8135/api/projects/x/chat/completions claude-haiku-5-5` (costs well under a cent).
  Expected: R2 and R4 read from the cache, and R3 reports `system_changed`.
- With Ollama instead (cache reads are reported there too): `--backend ollama --request-log summary`.

### Analyze after phase 4

The general `jq` examples (hit rate and cost per conversation, spend per day, gaps between requests,
write payback, largest writes) are in `CLAUDE.md` under "Request log". These answer the questions that
decide phases 5–7:

```bash
F=logs/requests.jsonl
# Why the cache missed, per reason
jq -r 'select(.diagnostics.cache_miss_reason) | .diagnostics.cache_miss_reason.type' $F | sort | uniq -c
# Effort levels in use (changes inside one conversation invalidate the cache)
jq -r 'select(.prompt) | [.fingerprint.conversation_key, (.prompt.effort // "default")] | @tsv' $F | sort -u
# Refusal fallbacks that answered with another model
jq -c 'select(.model_answered and .model != .model_answered) | {request_id, model, model_answered}' $F
# OpenAI cache hints sent by the client
jq -r 'select(.cache_hints) | .cache_hints[]' $F | sort | uniq -c
# Does a changed tool list or system prompt happen inside a conversation?
jq -s -c 'group_by(.fingerprint.conversation_key) | map(select(length > 1) | {conversation: .[0].fingerprint.conversation_key, tools: (map(.fingerprint.tools) | unique | length), system: (map(.fingerprint.system) | unique | length)}) | map(select(.tools > 1 or .system > 1))' $F
```

| If the data shows … | then … |
|---|---|
| Many gaps between 5 min and 1 h, misses with `diagnostics: null` after those gaps, large `cache_write` on the system prompt after a pause | **Phase 5**: `--anthropic-cache-ttl 1h` |
| `unavailable` misses, or several effort levels within one conversation | **Phase 6**: `--anthropic-effort-override` |
| `system_changed` inside a conversation that isn't a `COMPACTION MODE` request | **Phase 6**: mid-conversation system messages |
| `tools_changed` inside a conversation | Check `prompt.tools_before/after`. Consider `--tools-deny`, or the `inline-tools` beta (step 5) |
| `messages_changed` | Run a short session with `--request-log full` and compare the messages at `first_changed_message` (Mendix editing history, or our translation not being byte-stable) |
| `cache_hints` present | **Phase 7**: honour the client's cache hints |
| Nothing of the above, hit rate high | Stop; the remaining phases aren't worth building |

Fallback model switches are already visible through `model_answered` in the request log, so that part of
phase 6 only needs a log line if it turns out to matter.

### Where things are in the code

| What | Where |
|---|---|
| Cache breakpoints | `anthropic::mark_cache_breakpoints` (phase 5 adds the TTL in its inner `mark()`) |
| Translation options | `anthropic::RequestOptions` in `AnthropicClient.options` (phase 5: add `cache_ttl` here) |
| Usage counts and the usage log line | `anthropic::Usage` |
| Request records, conversation tracking, price table | `src/requestlog.rs` (`RequestRecord`, `RequestLog`, `PRICES`, `ResponseTap`) |
| Hook-in points per request | `proxy::anthropic_chat`, `proxy::ollama_chat`, the loop guard in `proxy::handle` |
| Diagnostics flag | `AnthropicClient.cache_diagnostics`; the request field is added in `proxy::anthropic_chat` |
| Logger config | `log-config.yaml` (appender `requests`, logger `request_log`) |
| Live check | `tools/live_cache_test.py` |

**Open points to keep in mind:**
- Prices in `requestlog::PRICES` are from 2026-10-24. Sonnet 5.5 cache reads are set to 0.05× ($0.10/MTok),
  from the live prompt-caching page; the Claude API skill's model table says $0.20. Check the pricing
  page when the cost numbers matter.
- The live test's two tools were below the 512-token minimum, so the tools breakpoint created no cache
  entry. With Mendix's real tool set the tools probably do get cached; phase 4 shows this (after a
  `system_changed`, `cache_read` should then still be > 0).
- A disconnected stream is only noticed on the next upstream chunk or at the end of the upstream stream,
  so `duration_ms` of an `incomplete` record can be longer than what the client waited.

## What the proxy did before this work

- `savings::tidy_tools` drops tools matching `--tools-deny` and sorts the rest by name, so the
  `tools` prefix is the same bytes on every call. The Mendix connector reorders tools on each call.
- `anthropic::mark_cache_breakpoints` sets up to 4 explicit `{"type":"ephemeral"}` (5 min)
  breakpoints: last tool, last system block, last block of the previous user turn, last block of the
  request. There are fewer when the request has no tools, no system prompt or only one user turn.
- `Usage` logs `input / cache_read / cache_write / output` and reports
  `prompt_tokens_details.cached_tokens` back.
- The JSON is serialized deterministically: `serde_json` runs without `preserve_order`, so object keys
  come out sorted (tool `input_schema`, `tool_use.input`).
- The loop guard and the `Done.` answer stop the client from resending the same prompt over and over.

## Comparison

| Guideline | Status | Notes |
|---|---|---|
| Prefix order tools → system → messages, stable content first | ✅ | Built in this order; tools sorted. |
| At most 4 breakpoints | ✅ | Up to 4 (`marks_cache_breakpoints` tests the full case with 4). |
| Breakpoint on the last block that stays the same across requests | ✅ | The last block becomes the read point for the next step of the tool loop. |
| 20-block lookback per breakpoint | ✅ mostly | The previous-user breakpoint covers a last turn that adds many blocks. Runs of `tool_use`/`tool_result` count as one position. |
| Tool schemas and `tool_use` input serialized deterministically | ✅ (test since phase 0) | Relies on `serde_json` without `preserve_order` (checked 2026-10-24: `cargo tree -e features -i serde_json` shows only `default`/`std`). No test, and if any dependency turns on that feature (Cargo feature unification), the order silently changes. |
| Keep `tool_choice`, images, thinking, `output_config.effort` stable | ⚠️ measurable | Effort comes from the caller's `reasoning_effort`, with `--anthropic-effort` as the default. A change mid-conversation can invalidate system and messages. Since phases 2–3, `prompt.effort`/`prompt.tool_choice` in the request log and `unavailable` from the diagnostics show it. |
| Model stays the same within a conversation | ⚠️ | `fallbacks: "default"` can rerun a request on another model; the cache is per model. That's acceptable. Since phase 2 it's visible as `model_answered` in the request log. |
| TTL that fits how far apart requests are | ⚠️ | Always 5 min. Old log, gaps between chat requests (Ollama and Anthropic together, so only indicative): 132 under 30 s, 43 between 30 s and 5 min, **8 between 5 min and 1 h**, 5 over 1 h. A person reading an answer and typing the next prompt often goes over 5 min. The TTL counts from the **start** of a request, so a long stream (the timeout is 600 s) uses up the window. |
| Read the `cache_creation` split (`ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`) | ✅ phase 1 | Was: only the totals were logged. |
| Cache diagnostics (`diagnostics.previous_message_id` → `cache_miss_reason`) | ✅ phase 3 | Was: not used; the only signal was `cache_read=0`, with no reason given. Now opt-in with `--anthropic-cache-diagnostics`. |
| Minimum cacheable length (512 tokens on Fable 5.1 / Opus 5.5 / Sonnet 5.5 / Haiku 5.5) | ✅ | The Maia system prompt is about 6 k chars, well above it. Shorter prompts aren't cached and no error is returned. |
| Mid-conversation system messages (`{"role":"system"}` in `messages`) instead of editing `system` | ⚠️ latent | Every `system`/`developer` message is moved into the top-level `system`. Mendix currently sends only one, at the start (183 of 183 in the log). A later system message would invalidate system and the whole conversation. |
| Thinking blocks | ✅ no action | Thinking is left out of the reply. The prefix cached at request N ends before that reply, so the next request still reads it. |
| Automatic caching (top-level `cache_control`) | — | Same as our "last block" breakpoint. It would only help with the walk-back to an eligible last block. Low value. |
| Pre-warming (`max_tokens: 0`), batches, parallel requests | n/a | The proxy only learns the prompt from the first request, and Mendix sends requests one at a time. |

The logs back up why this matters: the old log has 180 Anthropic calls with no usage logging, and they
ended in `You have reached your specified API usage limits` (2026-10-22). It also shows the Mendix
client sending **`COMPACTION MODE`** requests (15). Those have a different system prompt and probably
the same tools, which is exactly when the separate tools breakpoint pays off.

## Validation against the code (2026-10-24)

I checked every claim in this plan against `src/` (`cargo test`: 27 passed). The description of the
current behavior holds; the corrections are in the table above. These code facts change *how* steps
are built and are worked into the steps below:

1. **Hashes must be computed before the breakpoints are marked.** `mark_cache_breakpoints` writes
   `cache_control` into the blocks, and the previous-user breakpoint moves forward on every request.
   So the bytes of earlier messages differ between requests even when the content is the same: the
   first user message has a marker in request 2 and none in request 3. The API ignores this when it
   matches prefixes, but our own `conversation_key` and `message_hashes` would change on every request
   (steps 1b and 2). → Hash the translated request before marking, or with `cache_control` removed.
2. **Usage is logged inside pure translation functions.** `chat_completion` and `ChatStream` call
   `usage.log()` themselves, and `Usage` is private. `Usage::update` reads only top-level keys, so the
   nested `cache_creation` object needs its own code. → Make `Usage` public, return it from
   `chat_completion` (or have the caller log), and expose it from `ChatStream` (steps 1 and 1b).
3. **A stream can end without `message_stop`.** `ChatStream` is moved into the `map` closure of the
   response stream in `proxy::anthropic_chat`. When the client disconnects or the upstream connection
   breaks, `message_stop` never arrives and the usage line is never written. → Write the request record
   at `message_stop` and from `Drop` as a fallback, marked as `incomplete` (step 1b).
4. **Ollama responses aren't parsed.** `proxy::stream_response` returns `response.bytes_stream()`
   unchanged, and it's shared with `/v1/models` and embeddings. → Add a chat-only wrapper around the
   stream that keeps the tail of the stream and parses the final usage chunk, and reads the JSON body
   when not streaming. The bytes sent to the client stay the same. Mendix sends
   `stream_options.include_usage: true` (seen in the log), so the usage chunk is there (step 1b).
5. **The loop guard answers before routing.** It runs in `proxy::handle`, before dispatch and before
   the body is parsed. → Write its request record there, with only the path and the loop-guard flag (step 1b).
6. **`tidy_tools` only returns a bool**, and `anthropic_chat` ignores it. → Return the counts before
   and after `--tools-deny`, plus whether the order changed (step 1b).
7. **`messages_request` already has 4 parameters.** Adding the TTL (step 3) and the client's cache
   hints (step 7) → pass a `RequestOptions` struct instead, built from `AnthropicClient`. Diagnostics
   (step 2) need the shared conversation state, so add them in `anthropic_chat` after translation.
   `messages_request` stays a pure function.
8. **Tool-call arguments reach the client in two forms.** When streaming, they arrive in the model's
   original key order (raw `partial_json`). Without streaming, they come with sorted keys
   (`input.to_string()`). On the resend, both are parsed and serialized sorted. That turn came after the
   last breakpoint, so no cache read is lost, and from then on the bytes are stable. → The test in
   step 6 covers both paths.
9. **`push` merges consecutive messages with the same role.** A mid-conversation `system` message
   (step 5) has to be pushed as a separate message that isn't merged into a user turn.
10. **Logging config:** `log4rs::init_file` reads `log-config.yaml`. Only `azure_ai_gateway` is
    configured (debug, not additive). A request-log target that isn't configured falls back to the
    crate logger or root, and the JSON lines end up in the main log. If the config is missing, there's no
    request log at all. → Ship the logger in `log-config.yaml` and document it (step 1b).

## Cross-check with OpenAI and Flexera

Also checked against <https://developers.openai.com/api/docs/guides/prompt-caching> and
<https://www.flexera.com/blog/ai/prompt-caching-breakdown/> (July 2026), both read 2026-10-24. The
proxy speaks the OpenAI chat-completions protocol to the client, so the OpenAI side matters for what we
accept and report back. Anthropic's own docs stay the source for how we cache with Claude.

**Confirms the plan:**
- Both sources agree on the basics: exact prefix match, stable content first, history append-only,
  deterministic tool/JSON serialization, no timestamps or IDs in the prefix, no model switch
  mid-session. These are covered by the current code plus steps 4–6.
- OpenAI says that summarization/compaction resets reuse. That matches the `COMPACTION MODE` requests
  from Mendix. After compaction, a new conversation key starts, and only the tools prefix can still hit.
- Both recommend logging hit/miss fields on every call, a hit rate per group (user/day/conversation),
  input cost weighted by plain/cached/write tokens, and diffing prompt structure when the hit rate drops.
  These are covered by steps 1b and 2. The additions below fill the remaining gaps.
- Flexera: choose the TTL by the gaps between requests (1 h for gaps between 5 min and 1 h). This is step 3.

**Adds to the plan:**
- **OpenAI cache-write reporting:** OpenAI now reports writes as `cache_write_tokens`, split by
  lifetime, next to `cached_tokens`. The proxy reports only `cached_tokens` back → step 1.
- **OpenAI cache hints from the client:** an OpenAI client can send `prompt_cache_key`,
  `prompt_cache_retention` (`in_memory`/`24h`), `prompt_cache_options` (`ttl` `30m`/`12h`, `mode`) and
  `prompt_cache_breakpoint` markers on content blocks (GPT-5.6+). Today the Anthropic translation
  silently drops them, and for Ollama they pass through unchanged → new step 7.
- **Write payback and hit-rate drops (Flexera FinOps):** a write only pays off after a few reads, so
  measure that per prefix, alert on a sudden drop in hit rate, and track total spend per day, not only
  the per-token discount → step 1b.
- **Self-hosted (Ollama):** engine prefix caches are evicted on memory pressure (LRU), not by TTL. With
  several parallel slots (`OLLAMA_NUM_PARALLEL`), consecutive requests can land on a different slot and
  miss → noted under "Not in scope / Ollama".
- **Rate limits:** cache reads don't count toward Anthropic's input-tokens-per-minute limit, while on
  OpenAI they do. A plus for caching on Claude, but no action needed. The 2026-10-22 failure was a
  *spend* limit, which cache reads do lower.

**Where the sources disagree (Anthropic's docs win):**
- Flexera gives 0.1× for every Anthropic cache read, and a 512-token minimum only for Fable 5/Mythos 5.
  Anthropic's current page says reads cost 0.05× on Opus 5.5/Sonnet 5.5 and 0.025× on Fable 5.1, and the
  512 minimum also applies to Opus 5.5, Sonnet 5.5, Haiku 5.5 and Fable 5.1. The plan uses Anthropic's
  figures. So the price table in step 1b must be per model, configurable, and taken from Anthropic's
  pricing page, not from third-party summaries.
- OpenAI usage field names: the Responses API uses `usage.input_tokens_details.*`, while Chat
  Completions (what the proxy emits) uses `usage.prompt_tokens_details.cached_tokens`. The current code
  is correct. Check the exact Chat Completions name for cache writes before adding it (step 1).

## Plan

Ordered by value per effort. Steps 1, 1b and 2 come first, because steps 3–5 should only be done once
the data shows they're needed. Steps 1b and 2 share the conversation key and the per-message hashes, so
build them together.

### 1. Log usage completely (small)

- `anthropic.rs` `Usage`: also read `usage.cache_creation.ephemeral_5m_input_tokens` and
  `ephemeral_1h_input_tokens` (from `message_start` when streaming), and log them, e.g.
  `cache_write=… (5m=… 1h=…)`.
- Log a hit ratio, `cache_read / (input + cache_read + cache_write)`, and the model, so one log line is
  enough to judge a request.
- Log at debug level when `cache_read == 0 && cache_write == 0` (prompt below the minimum, or no
  breakpoint).
- In the OpenAI `usage` the proxy returns, also report cache writes next to
  `prompt_tokens_details.cached_tokens`, the way OpenAI does (`cache_write_tokens`, split by lifetime).
  First confirm the exact field names for Chat Completions in the OpenAI API reference. A client that
  tracks costs then sees the write premium too.
- Code: `Usage::update` reads only top-level keys, so read `cache_creation.*` separately. Make
  `Usage` public and stop logging it inside `chat_completion`/`ChatStream`, so step 1b can use the same
  values (validation 2).
- Tests: extend `translates_response` / `translates_stream_split_across_chunks` with a `cache_creation` object.

### 1b. Request log for later analysis (medium) — `--request-log` / `AZURE_AI_GATEWAY_REQUEST_LOG` = `off` | `summary` (default) | `full`

The text log can't be used to analyze caching after the fact: bodies are cut off at 4000 chars, usage
wasn't logged, and nothing ties the requests of one conversation together. This step adds a separate,
machine-readable log with **one JSON line per chat request**, for both backends, so later optimizations
(TTL choice, breakpoint placement, tool filtering, cost per conversation) can be based on data.

- **Where:** `logs/requests.jsonl`, through its own log4rs logger (target `request_log`, appender
  with pattern `{m}{n}`, rolling like the existing log). This keeps the main log readable, and log4rs
  takes care of rotation. Add the logger to `log-config.yaml` with `additive: false`. Without it, the
  lines end up in the main log (validation 10).
- **Module:** a new `src/requestlog.rs` with a `RequestRecord` struct (serde `Serialize`), filled in
  `proxy::anthropic_chat` / `proxy::ollama_chat` and written once the response, or the stream, has
  finished. Hook-in points from the code:
  - Anthropic streaming: write at `message_stop`, and from `Drop` on `ChatStream` (or a wrapper) when
    the stream breaks off, marked as `incomplete` (validation 3).
  - Ollama: a chat-only wrapper around `bytes_stream()` in `ollama_chat`. Leave `stream_response`
    alone for `/v1/models` and embeddings (validation 4).
  - Loop guard: a minimal record in `proxy::handle` (validation 5).
  - `tidy_tools` returns counts instead of a bool (validation 6).
  - Hashes are computed before `mark_cache_breakpoints` (validation 1).
- **Fields in `summary`** (no prompt content, so it's safe to keep for a long time):
  - `ts`, `request_id` (proxy-generated), `backend`, `route`/path, `deployment`, `model` requested
    and `model` answered (shows fallbacks), `stream`, `status`, `finish_reason`, `error_code`.
  - Timing: `duration_ms` total and `ttft_ms` (time to first byte/event). These matter for the TTL,
    because the cache lifetime counts from the start of the request.
  - Usage: `input`, `output`, `cache_read`, `cache_write_5m`, `cache_write_1h`, `hit_ratio`, and an
    estimated cost `cost_usd` from a price table per model (input, 5 m/1 h write, read, output).
  - Prompt shape: number of messages, tools (before/after `--tools-deny`), content blocks, images and
    documents, `tool_choice`, `effort`, `max_tokens`, the `anthropic-beta` headers sent, and where the
    breakpoints were placed (block indexes plus TTL).
  - Stability fingerprints: `conversation_key` (from step 2), and hashes of `tools`, `system` and each
    message (`message_hashes`). With these, a cache miss can be traced afterwards to the first changed
    part without storing the content. The fields `gap_since_previous_ms` (time since the previous
    request with the same key) and `diagnostics.cache_miss_reason` come from step 2.
  - Savings: whether `tidy_tools` reordered or dropped tools, whether the loop guard answered (a 429
    without a backend call), and whether `Done.` was returned for an empty turn.
  - Client cache hints: which OpenAI caching fields the client sent (`prompt_cache_key`,
    `prompt_cache_retention`, `prompt_cache_options`, the number of `prompt_cache_breakpoint` markers),
    so we can see whether Mendix uses them before building step 7.
- **Hit-rate drop alert:** a `warn` in the main log when a request in a conversation that already had
  cache reads comes back with `cache_read == 0`. Include the gap since the previous request and the
  first changed part (from the hashes), so a regression shows up without having to analyze anything.
- **Price table:** per model, with input, 5 m write, 1 h write, read and output prices, taken from
  Anthropic's pricing page (not third-party summaries; see the cross-check) and kept in one constant
  that's easy to update. Unknown model → `cost_usd: null` instead of a guess.
- **Extra fields in `full`** (opt-in, for a limited analysis session): the incoming OpenAI body, the
  translated Messages body that was sent, and the response (for streams, the reassembled message
  rather than the raw SSE). Keys are redacted the same way as in the debug log. Base64 data in
  images/PDFs is replaced by `{"media_type", "bytes", "sha256"}` so the files stay small and file
  contents don't end up in the log. Warn at startup that prompts and answers are written to disk.
- **Ollama:** the response is streamed through byte for byte. Usage (`prompt_tokens`,
  `completion_tokens`, from the last chunk when `stream_options.include_usage` is set, or the body
  without streaming) is read from a copy of the stream and doesn't change the bytes sent on.
  *Correction after the live test (2026-10-24):* Ollama does report its KV-cache reuse as
  `prompt_tokens_details.cached_tokens`, so `cache_read` and `hit_ratio` are filled for Ollama as well.
  Only the cache writes and the cost stay empty.
- **Analysis:** add a few `jq` examples in `CLAUDE.md`:
  - hit rate = cached / total input tokens, per conversation and per day;
  - cost per conversation and **total spend per day** (Flexera: watch the invoice, not only the discount);
  - **write payback**: per `system`/`tools` hash, the tokens written compared with the tokens read
    afterwards (a write at 1.25×/2× only pays off after a few reads, at 0.05× on Opus 5.5);
  - the distribution of gaps between requests compared with 5 m/1 h (the input for step 3);
  - the requests with the highest `cache_write`. Optionally later a small
  script under `tools/` that summarizes a `requests.jsonl`.
- Tests: `RequestRecord` serialization (`summary` contains no content), base64 replacement in `full`,
  usage extraction from an Ollama SSE tail, cost calculation with the price table.

### 2. Optional cache diagnostics (medium) — `--anthropic-cache-diagnostics` / `ANTHROPIC_CACHE_DIAGNOSTICS`

The API compares a request with the previous one and reports the first divergence (`model_changed`,
`system_changed`, `tools_changed`, `messages_changed`, `unavailable`, …). Diagnostics is GA, so no beta
header is needed. The proxy is stateless, so it has to remember the previous response `id` per conversation:

- Conversation key = hash of (model, first non-system message), from the OpenAI request after
  `tidy_tools`. *As built in phase 3:* tools and system prompt were dropped from the key, so that a
  changed system prompt or tool list stays in the same conversation and can be reported as
  `system_changed`/`tools_changed`. The last answer's id is kept per key in `RequestLog`, with a 1 h expiry.
- Request: add `"diagnostics": {"previous_message_id": <id or null>}` in `proxy::anthropic_chat`
  after translation, so `messages_request` stays pure (validation 7). Test the field once by hand
  against the API: if the API rejects it, every chat breaks, which is another reason to keep it opt-in.
- Response: read `id` and `diagnostics` (non-streaming from the body, streaming from `message_start` in
  `ChatStream::event`). Store the `id` under the key and log
  `cache_miss_reason.type` + `cache_missed_input_tokens` at info level.
- For a `messages_changed` miss, also log *which* message index differs, by comparing hashes of the
  messages with the previous request (keep a `Vec<u64>` of message hashes per key). This shows whether
  Mendix edits its history, or whether a resend of our own translation isn't byte-identical (for
  example `tool_call.arguments` that didn't parse and became `{}`).
- Off by default: it's a debugging tool, and fingerprints are stored on the Anthropic side for a short time.
- Tests: key stability (an appended message keeps the key), the request contains `diagnostics`, the
  stream parser takes `id`/`diagnostics` from `message_start`.

### 3. Configurable TTL (small) — `--anthropic-cache-ttl` / `ANTHROPIC_CACHE_TTL` = `5m` (default) | `1h`

- With `1h`, the breakpoints on **tools and system** get `{"type":"ephemeral","ttl":"1h"}`, and the two
  breakpoints in `messages` stay 5 min. This follows the rule that 1-hour entries must come before
  5-minute ones.
- Costs (Opus 5.5): a 1 h write is 2× the input price instead of 1.25×, reads are 0.05×. Tools + system
  are written once per hour instead of after every pause of more than 5 min. The conversation tail stays
  cheap. Decide on the default based on step 1: if `cache_write` on tools+system shows up after every
  pause, `1h` pays for itself after the first long pause.
- Optionally later `1h-all` (messages too), for long human think time. Measure first.
- Code: the inner `mark()` hardcodes `{"type":"ephemeral"}`, so give it the TTL per section. The TTL
  reaches `messages_request` through a new `RequestOptions` struct, together with `max_tokens`,
  `fallbacks` and `effort` (validation 7).
- Tests: `marks_cache_breakpoints` per TTL; no 1 h breakpoint after a 5 m one.

### 4. Keep prompt-affecting parameters stable (small)

- If step 2 shows `unavailable` misses, check which parameter changes: `tool_choice`,
  `output_config.effort`, the `anthropic-beta` set (the fallback beta depends on the model) or
  `output_config.format`.
- Possible fix: `--anthropic-effort-override`, which always applies the configured effort and ignores
  the caller's `reasoning_effort`. Only build it if the log shows it's needed (the log has only one
  `reasoning_effort`, on a `claude-haiku-5-5` call with `max_tokens: 300`, probably a title/summary
  request in a different conversation).
- Log when a fallback reran the request on another model (`model` in the response ≠ `model` in the
  request). The response model is already read: `message["model"]`, and `ChatStream.model` from
  `message_start`. Check once that a fallback actually reports the other model there.

### 5. Mid-conversation system messages (small, only if needed)

- In `messages_request`: only `system`/`developer` messages **before** the first user/assistant
  message go into the top-level `system`. Later ones become `{"role":"system", …}` in `messages` for
  the supported models (Fable 5.1, Opus 5.5, Opus 5, Sonnet 5.5, Haiku 5.5; not Sonnet 5), and are
  still hoisted for other models. Maintain this as a constant list, like `FALLBACK_MODELS`. Push the
  system message directly, not through `push`, because `push` merges consecutive same-role messages
  (validation 9).
- Only needed once the Mendix client sends system messages later in the conversation (the log shows
  none yet). Step 2 would show this as `system_changed`.
- If `tools_changed` turns out to be caused by tools *being added* mid-conversation, consider the
  `inline-tools-2026-09-15` beta (`tool_addition` in a mid-conversation system message). Low priority.

### 6. Lock in deterministic serialization (very small)

- Test: two OpenAI requests with the same tools but a different key order in `parameters` and in
  `tool_calls[].function.arguments` produce byte-identical Messages JSON. Include arguments in the
  model's original key order (how the streaming path passes them on) as well as sorted (the
  non-streaming path) (validation 8).
- Comment at `messages_request` or in `Cargo.toml` that `serde_json/preserve_order` must not be turned
  on, because that would make key order depend on the input.

### 7. Honour OpenAI cache hints from the client (small, only if step 1b shows Mendix sends them)

- `prompt_cache_key` → use it as the `conversation_key` for the request log and diagnostics instead of
  the hash (more reliable once a client sends it). Don't forward it to Anthropic, which doesn't know the field.
- `prompt_cache_retention: "24h"` or `prompt_cache_options.ttl: "12h"` → the 1 h TTL from step 3 for
  this request. Anthropic's longest TTL is 1 h. `in_memory`/`30m` → 5 m.
- `prompt_cache_breakpoint` markers on content blocks → `cache_control` on the matching Anthropic
  blocks, within the limit of 4 (the client's markers first, our defaults fill the rest).
  `prompt_cache_options.mode: "explicit"` → only the client's markers.
- Ollama: leave the body unchanged (Ollama ignores the fields). Only log them.
- Tests: mapping of each field, never more than 4 breakpoints, 1 h before 5 m.

### 8. Documentation

- Done for phases 0–3: `CLAUDE.md` describes `--request-log`, `--anthropic-cache-diagnostics`, the
  usage log line, the request log fields, the `jq` examples, `requestlog.rs` and the logger. Still to
  do with phase 5: `--anthropic-cache-ttl`.

## Implementation order

Each phase builds, passes its tests and can ship on its own. Every phase ends with `cargo fmt`,
`cargo clippy`, `cargo test`, the matching `CLAUDE.md` update and the matching verification item.

| Phase | Contents | Plan steps | Size |
|---|---|---|---|
| 0 ✅ (2026-10-24) | Refactor, no behavior change: `Usage` public, and `chat_completion` returns it so the caller logs it (`ChatStream` still logs at `message_stop`, because only the stream knows when it ends; phase 2 hooks the record in there); `RequestOptions` struct in `AnthropicClient.options` for `messages_request`; `tidy_tools` returns `ToolsTidy { before, after, reordered }`; test `serializes_deterministically` + `preserve_order` comment in `Cargo.toml`. 28 tests pass, clippy clean. | 6, preparation for 1/1b/3/7 | ½ day with phase 1 |
| 1 ✅ (2026-10-24) | Complete usage log: `cache_creation` split, hit ratio, model, debug line without caching, cache writes in the OpenAI usage (`prompt_tokens_details.cache_write_tokens`, the Chat Completions name; it has no split by lifetime). Tested against a fake Anthropic server: `Anthropic usage: model=claude-opus-5-5 input=12 cache_read=1800 cache_write=248 (5m=148 1h=100) output=5 hit=87%`. | 1 | |
| 2 ✅ (2026-10-24) | Done, with these differences from step 1b: **fingerprints come from the OpenAI request after `tidy_tools`**, not from the translated Anthropic request. That avoids the moving breakpoints (validation 1), and the hashes are the same for both backends; whether our own translation is unstable is left to the diagnostics (phase 3). Hashes use FNV-1a (stable across runs, no new dependency) instead of sha256. Missing values (such as `cost_usd` for unknown models) are left out instead of written as `null`. `response` is the assistant message as the caller received it. A broken-off stream is written as `incomplete` once the proxy notices (on the next chunk or at the end of the upstream stream). The price table has the Sonnet 5.5 cache-read multiplier at 0.05× ($0.10), from the live prompt-caching page; the skill's model table says $0.20. 36 tests pass, clippy clean, tested end to end with a fake Anthropic and Ollama server (with and without streaming, disconnect, loop guard, `full` redaction, hit-rate warning). | 1b | 1–2 days |
| 3 ✅ (2026-10-24) | Done. The **conversation key is now model + first non-system message** (no longer tools and system prompt), because otherwise a request with a changed system prompt or tool list starts a new conversation and diagnostics could never report `system_changed`/`tools_changed`. Tested live against the Anthropic API (`claude-haiku-5-5` and `claude-opus-5-5`, with the fallback beta): the `diagnostics` field is accepted without a beta header (the Claude API skill still describes the `cache-diagnosis-2026-04-07` beta; the live docs say it's GA); a streamed second turn with the tools in a different order read 99.5% from the cache with `diagnostics: null`, which confirms `tidy_tools` against the real API; a changed system prompt gave `system_changed` (~3,035 tokens missed), plus the hit-rate warning with `first changed message: 0`. **Finding:** after a system change, nothing at all was read from the cache, because the two tools are below the 512-token minimum, so the tools breakpoint doesn't create an entry. With Mendix's real tool set that's probably different; check in phase 4. **Finding (Ollama, live with `gpt-oss:20b`):** Ollama reports KV-cache reuse as `cached_tokens` (79 of 98 on the second turn), so `cache_read`/`hit_ratio` are filled for Ollama too. | 2 | ½ day |
| 4 ⏭ next | Measure for at least a day: run the proxy as in [Run the proxy for phase 4](#run-the-proxy-for-phase-4), then use the decision table under [Analyze after phase 4](#analyze-after-phase-4). No code. **Phases 5–7 only if the data shows they're needed.** | — | 1+ day of use |
| 5 | `--anthropic-cache-ttl 5m\|1h` (tools/system 1 h, messages 5 m) | 3 | small |
| 6 | Effort override, logging model switches by fallbacks, mid-conversation system messages | 4, 5 | small |
| 7 | OpenAI cache hints from the client | 7 | small |

## Not in scope

- Automatic caching instead of explicit breakpoints: no measurable gain. Our breakpoints already do
  what automatic caching does, plus a separate tools breakpoint and a system breakpoint.
- Pre-warming: the prompt isn't known before the first request.
- Ollama: the KV cache already benefits from the sorted tools. A possible separate step is a
  `keep_alive` option so the model (and its cache) stays loaded after 5 min of inactivity. First check
  whether Ollama's `/v1/chat/completions` honors `keep_alive`. Ollama's prefix cache is evicted on
  memory pressure, not by TTL. With `OLLAMA_NUM_PARALLEL` > 1, consecutive requests can land on a
  different slot and miss. The `ttft_ms` in the request log (step 1b) shows this.

## Verification

1. `cargo test` (27 passed before this work; 36 after phase 3), `cargo clippy`.
2. Run the Agent Editor with a tool loop of at least 5 steps against `claude-opus-5-5` and check the log:
   from the second step on, `cache_read` covers almost the whole prompt, and `cache_write` is only the
   new tail.
3. Wait 6 minutes and send another prompt: with `--anthropic-cache-ttl 1h`, `cache_read` must still
   cover tools + system.
4. With `--anthropic-cache-diagnostics`: no `cache_miss_reason` within a single conversation (verified
   live with `tools/live_cache_test.py`: R2/R4 `null`, R3 `system_changed`). A `COMPACTION MODE`
   request has a different first message, so it starts a new conversation key and is sent with
   `previous_message_id: null`. It's expected to still read the tools from the cache (`cache_read` > 0)
   when Mendix's tools are above the 512-token minimum.
5. `logs/requests.jsonl`: one line per chat request (both backends, streaming and non-streaming;
   verified live). In `summary` mode there's no prompt text in the file. Over the requests of one tool
   loop, `fingerprint.conversation_key` stays the same and `fingerprint.messages` only grows at the end
   (`first_changed_message` absent).
